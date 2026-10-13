#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""Rehearse release-only paths and verify their exact hosted receipts.

The Windows leg uses released baseline binaries with candidate proof commands.
This is tooling compatibility evidence. RC Build and preflight still qualify
the candidate binaries themselves. No command here tags or publishes a release.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import zipfile

SCHEMA = 'kin.release-rehearsal.v2'
BASELINE = 'v0.8.6'
MODES = ('compatibility-baseline', 'candidate-full')
SHA = re.compile(r'^[0-9a-f]{40}$')
HASH = re.compile(r'^[0-9a-f]{64}$')
INPUTS = (
    '.github/workflows/install-proof.yml',
    '.github/workflows/release-rehearsal.yml',
    '.github/workflows/release-rehearsal-windows.yml',
    'scripts/generate-release-rehearsal.py',
    'scripts/release-rehearsal.py',
    'scripts/release-rehearsal-compat.py',
    'scripts/install.ps1',
    'scripts/test-windows-install-mirror.ps1',
    'packages/kin/package.json',
    'Cargo.lock',
    'scripts/release-proof/bin/kin-release-preflight',
    'scripts/release-proof/bin/kin-acceptance-suite',
    'scripts/acceptance/magic_repro.py',
    'scripts/acceptance/brownfield_repro.py',
    'scripts/acceptance/trace_pages.py',
)


def run(*args: str, cwd: Path | None = None, env: dict | None = None) -> str:
    result = subprocess.run(args, cwd=cwd, env=env, check=True,
                            stdout=subprocess.PIPE, text=True)
    return result.stdout.strip()


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def write(path: Path, data: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + '\n')


def identity(source: Path, candidate: str, checked_out: bool = False) -> dict:
    if not SHA.fullmatch(candidate):
        raise ValueError('rehearsal requires an exact candidate SHA')
    prefix = run('git', 'rev-parse', '--show-prefix', cwd=source)
    root = Path(run('git', 'rev-parse', '--show-toplevel', cwd=source))
    tree_ref = f'{candidate}:{prefix.rstrip("/")}' if prefix else f'{candidate}^{{tree}}'
    tree = run('git', '--no-replace-objects', 'rev-parse', tree_ref, cwd=root)
    if checked_out and run('git', 'rev-parse', 'HEAD', cwd=root) != candidate:
        raise ValueError('checked-out source differs from the exact rehearsal candidate')
    inputs = {}
    for name in INPUTS:
        data = subprocess.check_output(['git', '--no-replace-objects', 'show', f'{candidate}:{prefix}{name}'], cwd=root)
        inputs[name] = digest(data)
        if checked_out and (source / name).read_bytes() != data:
            raise ValueError(f'rehearsal source {name} differs from committed candidate bytes')
    return {'candidate': candidate, 'source_tree': tree, 'inputs_sha256': inputs}


def bind(args) -> None:
    identity(args.source, args.candidate, checked_out=True)
    if args.candidate != os.environ.get('GITHUB_SHA', args.candidate):
        raise ValueError('rehearsal workflow did not resolve from the candidate commit')
    if args.mode == 'compatibility-baseline':
        if args.release_tag != BASELINE:
            raise ValueError(f'rehearsal baseline must be the reviewed stable {BASELINE}')
        release = json.loads(run('gh', 'api', f'repos/kinlab-ai/kin/releases/tags/{BASELINE}'))
        if release.get('tag_name') != BASELINE or release.get('draft') or release.get('prerelease'):
            raise ValueError('rehearsal baseline is not the reviewed published stable release')
    run(sys.executable, str(args.source / 'scripts/generate-release-rehearsal.py'), '--check')
    print(f'Rehearsing candidate source {args.candidate}, mode {args.mode}')


def stage_candidate(args) -> None:
    identity(args.source, args.candidate, checked_out=True)
    archive = args.directory / 'kin-windows-x86_64.zip'
    have = digest(archive.read_bytes())
    want = Path(str(archive) + '.sha256').read_text().split()[0]
    if have != want:
        raise ValueError('candidate Windows archive disagrees with its actual RC sidecar')
    manifest_path = args.directory / 'kin-windows-x86_64.provenance.json'
    manifest = json.loads(manifest_path.read_text())
    version = json.loads((args.source / 'packages/kin/package.json').read_text())['version']
    lock = digest((args.source / 'Cargo.lock').read_bytes())
    if (manifest.get('schema_version') != 2 or 'release_tag' in manifest
            or manifest.get('artifact') != 'kin-windows-x86_64'
            or manifest.get('kin', {}).get('commit') != args.candidate
            or manifest['kin'].get('cargo_lock_sha256') != lock
            or manifest['kin'].get('embedded_dependency_provenance') != lock
            or manifest.get('release_candidate', {}).get('version', '').lstrip('v') != version
            or manifest.get('archive', {}).get('name') != archive.name
            or manifest['archive'].get('sha256') != have
            or str(manifest.get('workflow', {}).get('run_id')) != os.environ['GITHUB_RUN_ID']
            or manifest['workflow'].get('repository') != os.environ['GITHUB_REPOSITORY']):
        raise ValueError('Windows RC archive provenance is not the exact candidate source and calling run')
    for name, value in (('version', version), ('commit', args.candidate), ('lock-sha', lock)):
        (args.directory / f'expected-{name}.txt').write_text(value + '\n')
    shutil.copyfile(args.source / 'scripts/install.ps1', args.directory / 'install.ps1')
    write(args.directory / 'candidate-identity.json', {
        'commit': args.candidate, 'version': version, 'lock_sha256': lock,
        'archive_sha256': have, 'provenance_sha256': digest(manifest_path.read_bytes()),
        'components': {entry['name']: entry['sha256'] for entry in manifest['archive_contents']
                       if entry['name'] in ('kin.exe', 'kin-daemon.exe')},
    })


def imports(args) -> None:
    source = identity(args.source, args.candidate, checked_out=True)
    root = args.output.parent / 'imports'
    root.mkdir(parents=True, exist_ok=False)
    repository = Path(run('git', 'rev-parse', '--show-toplevel', cwd=args.source))
    snapshot = root / 'source'
    run('git', 'clone', '--quiet', '--shared', '--no-checkout', str(repository), str(snapshot))
    # Keep the candidate's actual subtree objects. A local tree-view commit
    # lets the preflight resolver read public-shaped paths from either layout.
    # It is recorded as a view, never as the source candidate or binary stamp.
    author = dict(os.environ, GIT_AUTHOR_NAME='Release rehearsal',
                  GIT_AUTHOR_EMAIL='release-rehearsal@kinlab.dev',
                  GIT_COMMITTER_NAME='Release rehearsal',
                  GIT_COMMITTER_EMAIL='release-rehearsal@kinlab.dev')
    view = run('git', 'commit-tree', source['source_tree'], '-m',
               'Create release tooling tree view\n\nSigned-off-by: Release rehearsal <release-rehearsal@kinlab.dev>',
               cwd=snapshot, env=author)
    run('git', 'update-ref', 'refs/heads/rehearsal', view, cwd=snapshot)
    run('git', 'symbolic-ref', 'HEAD', 'refs/heads/rehearsal', cwd=snapshot)
    run('git', 'update-ref', 'refs/remotes/origin/main', view, cwd=snapshot)
    run('git', 'checkout', view, '--', 'scripts/acceptance',
        'scripts/release-proof/bin/kin-acceptance-suite', cwd=snapshot)
    resolver = snapshot / 'scripts/release-proof/bin/kin-acceptance-suite'
    clean = {'PATH': os.environ['PATH'], 'HOME': str(root), 'LANG': 'C.UTF-8'}
    staged = {}
    for suite in ('magic', 'brownfield'):
        destination = root / suite / f'{suite}_repro.py'
        resolution = root / suite / 'resolution.json'
        run(sys.executable, str(resolver), suite, '--kin-repo', str(snapshot), '--ref', view,
            '--dest', str(destination), '--no-fetch', '--json', str(resolution), env=clean)
        # Run outside the original checkout with no PYTHONPATH. Importing the
        # staged entrypoint exercises the very sibling-copy path preflight uses.
        help_text = run(sys.executable, str(destination), '--help', cwd=root, env=clean)
        if 'usage:' not in help_text.lower():
            raise ValueError(f'staged {suite} suite did not reach its actual argument parser')
        siblings = destination.parent / 'trace_pages.py'
        if siblings.read_bytes() != (args.source / 'scripts/acceptance/trace_pages.py').read_bytes():
            raise ValueError(f'staged {suite} imported a stale acceptance sibling')
        staged[suite] = {'entry_sha256': digest(destination.read_bytes()),
                         'sibling_sha256': digest(siblings.read_bytes()),
                         'help_sha256': digest(help_text.encode()),
                         'resolution': json.loads(resolution.read_text())}
    help_text = run('bash', str(args.source / 'scripts/release-proof/bin/kin-release-preflight'),
                    '--help', cwd=root, env=clean)
    if 'usage:' not in help_text:
        raise ValueError('preflight driver did not reach its argument parser')
    write(args.output, {'status': 'PASS', 'candidate': args.candidate,
                        'tree_view_commit': view, 'staged': staged})


def environment(args) -> None:
    identity(args.source, args.candidate, checked_out=True)
    channels = {key: Path(os.environ[key]) for key in ('GITHUB_ENV', 'GITHUB_PATH')}
    before = {key: path.read_bytes() if path.exists() else b'' for key, path in channels.items()}
    # Inherit the real runner file commands. A poison fake escaping its
    # sandbox now fails against an actual Actions path before another step runs.
    print(run(sys.executable, str(args.source / 'scripts/test-release-workflow-authority.py'),
              cwd=args.source))
    for key, path in channels.items():
        if (path.read_bytes() if path.exists() else b'') != before[key]:
            raise ValueError(f'release authority changed the live {key} file')
    probe = args.output.parent / 'handoff-bin'
    probe.mkdir(parents=True, exist_ok=True)
    command = probe / 'rehearsal-path-probe'
    command.write_text(f'#!/bin/sh\nprintf "%s\\n" "{args.candidate}"\n')
    command.chmod(0o755)
    with channels['GITHUB_ENV'].open('a') as out:
        out.write(f'REHEARSAL_HANDOFF={args.candidate}\n')
    with channels['GITHUB_PATH'].open('a') as out:
        out.write(str(probe) + '\n')
    write(args.output, {'status': 'PASS', 'candidate': args.candidate,
                       'live_file_commands': sorted(channels),
                       'unchanged_sha256': {key: digest(data) for key, data in before.items()}})


def record(args) -> None:
    source = identity(args.source, args.candidate, checked_out=True)
    common = dict(source, schema=SCHEMA, status='PASS', leg=args.leg,
                  run_id=int(os.environ['GITHUB_RUN_ID']),
                  repository=os.environ['GITHUB_REPOSITORY'], mode=args.mode,
                  baseline_tag=args.release_tag if args.mode == 'compatibility-baseline' else None,
                  event=os.environ['GITHUB_EVENT_NAME'], ref=os.environ['GITHUB_REF'])
    if args.leg == 'tooling':
        directory = args.output.parent
        for name in ('imports', 'environment'):
            evidence = json.loads((directory / f'{name}.json').read_text())
            if evidence.get('status') != 'PASS' or evidence.get('candidate') != args.candidate:
                raise ValueError(f'{name} evidence does not pass for the exact candidate')
            common[name] = evidence
        if os.environ.get('REHEARSAL_HANDOFF') != args.candidate:
            raise ValueError('the native runner did not carry the actual GITHUB_ENV handoff')
        if run('rehearsal-path-probe') != args.candidate:
            raise ValueError('the native runner did not carry the actual GITHUB_PATH handoff')
        common['native_handoff'] = 'PASS'
    elif args.mode == 'compatibility-baseline':
        compatibility = json.loads((args.output.parent / 'compatibility/result.json').read_text())
        if (compatibility.get('schema') != 'kin.release-rehearsal-compat.v1'
                or compatibility.get('status') != 'PASS'
                or compatibility.get('coverage') != 'compatibility-baseline'
                or compatibility.get('baseline', {}).get('tag') != BASELINE):
            raise ValueError('Windows compatibility baseline did not pass its actual stable setup proof')
        common['compatibility'] = compatibility
        common['baseline'] = compatibility['baseline']
    else:
        version = Path('expected-version.txt').read_text().strip()
        commit = Path('expected-commit.txt').read_text().strip()
        staged = Path(os.environ['RUNNER_TEMP']) / 'kin-pull-request-release'
        binary = json.loads((staged / 'candidate-identity.json').read_text())
        meta = json.loads(Path('kin-windows-bench-meta.json').read_text())
        if (commit != args.candidate or binary['commit'] != args.candidate
                or version != binary['version'] or meta.get('kin_commit') != args.candidate
                or meta.get('kin_dirty') is not False or meta.get('kin_source_known') is not True
                or meta.get('dependency_provenance') != binary['lock_sha256']
                or Path('expected-lock-sha.txt').read_text().strip() != binary['lock_sha256']):
            raise ValueError('full Windows proof did not execute the exact candidate archive build')
        for name in ('kin.exe', 'kin-daemon.exe'):
            installed = Path.home() / '.kin/bin' / name
            if digest(installed.read_bytes()) != binary['components'].get(name):
                raise ValueError(f'installed {name} differs from the actual candidate archive inventory')
        common['candidate_binary'] = binary
        common['full_install_proof'] = 'PASS'
    write(args.output, common)


def validate_receipt(receipt: dict, source: dict, run_id: int, repository: str,
                     mode: str = 'candidate-full') -> None:
    if receipt.get('schema') != SCHEMA or receipt.get('status') != 'PASS':
        raise ValueError('missing passing release rehearsal receipt')
    if any(receipt.get(key) for key in ('mock', 'fake', 'stand_in')):
        raise ValueError('release rehearsal must carry actual hosted evidence')
    for key in ('candidate', 'source_tree', 'inputs_sha256'):
        if receipt.get(key) != source[key]:
            raise ValueError(f'release rehearsal {key} is stale or names different candidate bytes')
    if receipt.get('run_id') != run_id or receipt.get('repository') != repository:
        raise ValueError('release rehearsal belongs to another hosted run or repository')
    if receipt.get('mode') != mode:
        raise ValueError(f'release rehearsal must prove {mode}, not {receipt.get("mode", "missing")}')
    if receipt.get('baseline_tag') != (BASELINE if mode == 'compatibility-baseline' else None):
        raise ValueError('release rehearsal baseline identity disagrees with its coverage mode')
    legs = receipt.get('legs', {})
    if set(legs) != {'windows', 'tooling'}:
        raise ValueError('release rehearsal must contain both actual passing legs')
    for name, leg in legs.items():
        for key in ('schema', 'status', 'candidate', 'source_tree', 'inputs_sha256',
                    'run_id', 'repository', 'baseline_tag', 'event', 'ref', 'mode'):
            if leg.get(key) != receipt.get(key):
                raise ValueError(f'{name} rehearsal leg disagrees on {key}')
        if leg.get('leg') != name or leg.get('mock') or leg.get('fake') or leg.get('stand_in'):
            raise ValueError(f'{name} rehearsal leg is not real hosted evidence')
    windows = legs['windows']
    if mode == 'candidate-full':
        binary = windows.get('candidate_binary', {})
        if binary.get('commit') != source['candidate'] or windows.get('full_install_proof') != 'PASS':
            raise ValueError('Windows rehearsal has no full exact-candidate binary proof')
        if binary.get('lock_sha256') != source['inputs_sha256'].get('Cargo.lock'):
            raise ValueError('Windows candidate binary lock differs from the exact source Cargo.lock')
        for key in ('archive_sha256', 'lock_sha256', 'provenance_sha256'):
            if not HASH.fullmatch(binary.get(key, '')):
                raise ValueError(f'Windows candidate proof has no {key}')
        if not isinstance(binary.get('version'), str) or not binary['version']:
            raise ValueError('Windows candidate proof has no binary version')
        if set(binary.get('components', {})) != {'kin.exe', 'kin-daemon.exe'} or any(
                not HASH.fullmatch(value) for value in binary['components'].values()):
            raise ValueError('Windows candidate proof has no actual CLI/daemon component identity')
    else:
        baseline = windows.get('baseline', {})
        compatibility = windows.get('compatibility', {})
        if (baseline.get('tag') != BASELINE or not SHA.fullmatch(baseline.get('commit', ''))
                or not HASH.fullmatch(baseline.get('lock_sha256', ''))
                or compatibility.get('coverage') != mode or compatibility.get('status') != 'PASS'
                or compatibility.get('baseline') != baseline):
            raise ValueError('Windows rehearsal has no actual reviewed compatibility-baseline proof')
    tooling = legs['tooling']
    if tooling.get('native_handoff') != 'PASS':
        raise ValueError('release rehearsal did not verify native file-command handoff')
    for name in ('imports', 'environment'):
        evidence = tooling.get(name, {})
        if evidence.get('status') != 'PASS' or evidence.get('candidate') != source['candidate']:
            raise ValueError(f'release rehearsal has no passing {name} evidence')


def collect(args) -> None:
    source = identity(args.source, args.candidate, checked_out=True)
    legs = {name: json.loads((args.directory / f'{name}.json').read_text())
            for name in ('windows', 'tooling')}
    receipt = dict(source, schema=SCHEMA, status='PASS', mode=args.mode,
                   baseline_tag=BASELINE if args.mode == 'compatibility-baseline' else None,
                   run_id=int(os.environ['GITHUB_RUN_ID']),
                   repository=os.environ['GITHUB_REPOSITORY'], legs=legs,
                   event=os.environ['GITHUB_EVENT_NAME'], ref=os.environ['GITHUB_REF'])
    validate_receipt(receipt, source, receipt['run_id'], receipt['repository'], args.mode)
    write(args.directory / 'rehearsal.json', receipt)


def verify(args) -> None:
    source = identity(args.source, args.candidate)
    receipt = json.loads(args.receipt.read_text())
    mode = 'candidate-full' if args.workflow == '.github/workflows/rc-build.yml' else 'compatibility-baseline'
    validate_receipt(receipt, source, args.run_id, args.repository, mode)
    hosted = json.loads(run('gh', 'api', f'repos/{args.repository}/actions/runs/{args.run_id}'))
    if (hosted.get('status') != 'completed' or hosted.get('conclusion') != 'success'
            or hosted.get('path') != args.workflow or hosted.get('event') != receipt.get('event')):
        raise ValueError('rehearsal receipt is not from the successful expected hosted workflow')
    if args.workflow == '.github/workflows/ci.yml' and hosted.get('event') == 'pull_request':
        # A PR workflow executes the synthetic merge, while Actions reports the
        # source branch head. Prove both parents and the actual GitHub merge ref.
        match = re.fullmatch(r'refs/pull/([1-9][0-9]*)/merge', receipt.get('ref', ''))
        if not match:
            raise ValueError('PR rehearsal has no exact native merge ref')
        pulls = [pull for pull in hosted.get('pull_requests', [])
                 if pull.get('number') == int(match[1])]
        if len(pulls) != 1 or pulls[0]['head']['sha'] != hosted.get('head_sha'):
            raise ValueError('PR rehearsal head is not the head this Actions run grades')
        parent1 = run('git', 'rev-parse', f'{args.candidate}^1', cwd=args.source)
        parent2 = run('git', 'rev-parse', f'{args.candidate}^2', cwd=args.source)
        # Actions' pull_requests[].base.sha can retain the PR's earlier base
        # even when GITHUB_SHA ran a newer merge. The immutable commit object
        # and service-owned merge ref are the authority for the actual parents.
        commit = json.loads(run('gh', 'api',
            f'repos/{args.repository}/git/commits/{args.candidate}'))
        parents = [parent['sha'] for parent in commit.get('parents', [])]
        if (commit.get('sha') != args.candidate or parents != [parent1, parent2]
                or parent2 != hosted['head_sha'] or pulls[0]['base']['ref'] != 'main'):
            raise ValueError('PR rehearsal merge parents do not match its actual GitHub commit and run head')
        merge = json.loads(run('gh', 'api',
            f'repos/{args.repository}/git/ref/pull/{match[1]}/merge'))
        if merge.get('object', {}).get('sha') != args.candidate:
            raise ValueError('PR rehearsal receipt is stale relative to its actual GitHub merge ref')
    elif hosted.get('head_sha') != args.candidate:
        raise ValueError('hosted rehearsal run grades a different candidate SHA')
    pages = json.loads(run('gh', 'api', '--paginate', '--slurp',
        f'repos/{args.repository}/actions/runs/{args.run_id}/jobs?per_page=100'))
    jobs = [job for page in pages for job in page['jobs']]
    prefix = 'RC rehearsal' if args.workflow == '.github/workflows/rc-build.yml' else 'Release rehearsal'
    for suffix in (f'{prefix} / windows / windows-latest',
                   f'{prefix} / Rehearse release tooling',
                   f'{prefix} / Collect release rehearsal'):
        matches = [job for job in jobs if job.get('name') == suffix]
        if len(matches) != 1 or matches[0].get('conclusion') != 'success':
            raise ValueError(f'RC Build carries no unique successful {suffix} job')
    if mode == 'candidate-full':
        # A receipt's digest is a claim until it is compared with the actual
        # selected RC archive. Download existing outputs, never build here.
        with tempfile.TemporaryDirectory(prefix='rehearsal-archive-',
                dir=os.environ.get('RUNNER_TEMP', str(args.receipt.parent))) as directory:
            root = Path(directory)
            run('gh', 'run', 'download', str(args.run_id), '--repo', args.repository,
                '--name', 'kin-windows-x86_64', '--dir', str(root))
            archive = root / 'kin-windows-x86_64.zip'
            manifest_path = root / 'kin-windows-x86_64.provenance.json'
            manifest = json.loads(manifest_path.read_text())
            binary = receipt['legs']['windows']['candidate_binary']
            have = digest(archive.read_bytes())
            if (have != binary['archive_sha256']
                    or Path(str(archive) + '.sha256').read_text().split()[0] != have
                    or digest(manifest_path.read_bytes()) != binary['provenance_sha256']
                    or manifest.get('kin', {}).get('commit') != args.candidate
                    or manifest['kin'].get('cargo_lock_sha256') != binary['lock_sha256']
                    or manifest.get('release_candidate', {}).get('version', '').lstrip('v') != binary['version']
                    or 'release_tag' in manifest
                    or str(manifest.get('workflow', {}).get('run_id')) != str(args.run_id)
                    or manifest['workflow'].get('repository') != args.repository):
                raise ValueError('full Windows rehearsal receipt differs from actual selected RC archive provenance')
            inventory = {entry['name']: entry['sha256'] for entry in manifest['archive_contents']
                         if entry['name'] in ('kin.exe', 'kin-daemon.exe')}
            if inventory != binary['components']:
                raise ValueError('full Windows rehearsal component identity differs from RC inventory')
            with zipfile.ZipFile(archive) as zipped:
                for name, expected in inventory.items():
                    if digest(zipped.read(name)) != expected:
                        raise ValueError(f'actual RC zip component {name} differs from its inventory')
    print(f'Hosted release rehearsal passed for {args.candidate}, {args.workflow} run {args.run_id}, mode {mode}')


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    for name in ('bind', 'imports', 'environment', 'record', 'collect', 'verify', 'stage-candidate'):
        sub = commands.add_parser(name)
        sub.add_argument('--source', type=Path, required=True)
        sub.add_argument('--candidate', required=True)
        if name in ('bind', 'record'):
            sub.add_argument('--release-tag', default=BASELINE)
        if name in ('bind', 'record', 'collect'):
            sub.add_argument('--mode', choices=MODES, default='compatibility-baseline')
        if name in ('imports', 'environment', 'record'):
            sub.add_argument('--output', type=Path, required=True)
        if name == 'record':
            sub.add_argument('--leg', choices=('windows', 'tooling'), required=True)
        if name in ('collect', 'stage-candidate'):
            sub.add_argument('--directory', type=Path, required=True)
        if name == 'verify':
            sub.add_argument('--receipt', type=Path, required=True)
            sub.add_argument('--run-id', type=int, required=True)
            sub.add_argument('--repository', required=True)
            sub.add_argument('--workflow', choices=('.github/workflows/rc-build.yml',
                                                   '.github/workflows/ci.yml'),
                             default='.github/workflows/rc-build.yml')
    args = parser.parse_args()
    args.source = args.source.resolve()
    if hasattr(args, 'output'):
        args.output = args.output.resolve()
    if hasattr(args, 'directory'):
        args.directory = args.directory.resolve()
    globals()[args.command.replace('-', '_')](args)


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f'release rehearsal refused: {error}', file=sys.stderr)
        sys.exit(1)
