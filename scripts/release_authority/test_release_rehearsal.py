# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""Execute the actual pre-token rehearsal gate against poisoned verifier fixtures."""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import textwrap

from .test_helpers import ROOT, workflow_step_source, expect_assertion
from release_rehearsal_contract import CONTEXT_GUARD, check_context_checkout

ANCHOR = "      - name: Require the exact candidate's hosted release rehearsal"
CHANNELS = ('GITHUB_ENV', 'GITHUB_PATH', 'GITHUB_STATE', 'GITHUB_OUTPUT')
FAKE = '''import os, pathlib, subprocess, sys
mode = os.environ.get('POISON', '')
if mode.startswith('GITHUB_'):
    pathlib.Path(os.environ[mode]).write_text('PATH=/elsewhere\\n')
elif mode == 'checkout':
    pathlib.Path('tracked.txt').write_text('changed')
elif mode == 'head':
    subprocess.run(['git','-c','user.name=t','-c','user.email=t@example.com',
        '-c','commit.gpgsign=false','commit','--allow-empty','-s','-m','fixture history'], check=True)
sys.exit(0)
'''


def execute(source: str, *, poison: str = '', receipt: str = '', real_policy: bool = False):
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        repo = root / 'repo'
        repo.mkdir()
        spec = importlib.util.spec_from_file_location('receipt_policy', ROOT / 'scripts/release-rehearsal.py')
        policy = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(policy)
        for name in policy.INPUTS:
            path = repo / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('fixture\n')
        verifier = repo / 'scripts/release-rehearsal.py'
        verifier.write_text((ROOT / 'scripts/release-rehearsal.py').read_text() if real_policy else FAKE)
        (repo / 'tracked.txt').write_text('unchanged')
        subprocess.run(['git','init','-q','-b','main',str(repo)], check=True)
        def commit(message):
            subprocess.run(['git','add','.'], cwd=repo, check=True)
            subprocess.run(['git','-c','user.name=t','-c','user.email=t@example.com',
                '-c','commit.gpgsign=false','commit','-q','-s','-m',message], cwd=repo, check=True)
            return subprocess.check_output(['git','rev-parse','HEAD'], cwd=repo, text=True).strip()
        authority = commit('protected policy fixture')
        candidate_mark = root / 'candidate-ran'
        verifier.write_text(f'import pathlib\npathlib.Path({str(candidate_mark)!r}).write_text("ran")\n')
        candidate = commit('permissive candidate fixture')
        # A candidate cannot redirect the pinned policy through mutable refs or
        # replace refs. Both are deliberately hostile in this fixture.
        subprocess.run(['git','update-ref','refs/remotes/origin/main',candidate], cwd=repo, check=True)
        subprocess.run(['git','replace',authority,candidate], cwd=repo, check=True)
        (root / 'rc-locator-result.txt').write_text('run_id=123\n')
        (root / 'rehearsal').mkdir()
        if receipt:
            (root / 'rehearsal/rehearsal.json').write_text(receipt)
        bin_dir = root / 'bin'
        bin_dir.mkdir()
        gh = bin_dir / 'gh'
        gh.write_text('#!/bin/sh\nexit 0\n')
        gh.chmod(0o755)
        # Safe-path isolation must reject a module planted beside the extracted
        # policy, which would otherwise turn a missing receipt into exit 0.
        (root / 'json.py').write_text('raise SystemExit(0)\n')
        native = {}
        for name in CHANNELS:
            path = root / name
            path.write_text('')
            native[name] = str(path)
        mark = root / 'token-would-mint'
        env = {**os.environ, **native, 'AUTHORITY_MAIN': authority, 'SHA': candidate,
               'REPO': 'owner/product', 'RUNNER_TEMP': str(root), 'GITHUB_WORKSPACE': str(repo),
               'PATH': str(bin_dir) + os.pathsep + os.environ['PATH'], 'POISON': poison,
               'TOKEN_MARK': str(mark)}
        result = subprocess.run(['bash','-c',source + '\nprintf minted > "$TOKEN_MARK"\n'],
                                cwd=repo, env=env, capture_output=True, text=True, timeout=30)
        if any(Path(path).read_bytes() for path in native.values()):
            raise AssertionError('the verifier fixture escaped into native runner channels')
        return result.returncode, result.stdout + result.stderr, mark.exists(), candidate_mark.exists()


def assert_boundary(release_tag: str) -> None:
    step = workflow_step_source('release-tag.yml', release_tag, ANCHOR)
    source = textwrap.dedent(step.split('        run: |\n', 1)[1])
    code, log, minted, candidate_ran = execute(source)
    if code or not minted or candidate_ran:
        raise AssertionError(f'protected rehearsal policy was not the sole verifier: {code} {log}')
    for poison in (*CHANNELS, 'checkout', 'head'):
        code, log, minted, _ = execute(source, poison=poison)
        expected = 'runner state' if poison in CHANNELS else 'admitted checkout'
        if code == 0 or minted or expected not in log:
            raise AssertionError(f'rehearsal policy {poison} poison reached the token mint: {code} {log}')
    for label, record in (('missing', ''), ('stale', json.dumps({
            'schema':'kin.release-rehearsal.v1','status':'PASS','candidate':'0'*40}))):
        code, log, minted, candidate_ran = execute(source, real_policy=True, receipt=record)
        if code == 0 or minted or candidate_ran or 'protected release rehearsal policy refused' not in log:
            raise AssertionError(f'candidate lowered the protected {label} receipt bar: {code} {log}')


def check_protected_rehearsal_boundary(release_tag: str) -> None:
    assert_boundary(release_tag)
    for before, after, error in (
        ('git --no-replace-objects show "${AUTHORITY_MAIN}:scripts/release-rehearsal.py"',
         'git show "HEAD:scripts/release-rehearsal.py"', 'sole verifier'),
        ('python3 -I "$rehearsal_policy"', 'python3 "$rehearsal_policy"', 'candidate lowered'),
    ):
        expect_assertion('rehearsal policy authority or isolation removed',
                         error,
                         lambda before=before,after=after: assert_boundary(release_tag.replace(before, after, 1)))


def assert_context_guards(workflow: str) -> None:
    guards = workflow.split(CONTEXT_GUARD)[1:]
    if not guards:
        raise AssertionError('rehearsal has no executable pre-checkout guard')
    for block in guards:
        step = block.split('\n      - name:', 1)[0]
        source = textwrap.dedent(step.split('        run: |\n', 1)[1])
        for candidate, context, allowed in (
            ('a' * 40, 'a' * 40, True),
            ('b' * 40, 'a' * 40, False),
            ('', '', False),
            ('not-a-commit', 'not-a-commit', False),
            ('a' * 40 + '; echo injected', 'a' * 40, False),
        ):
            with tempfile.TemporaryDirectory() as directory:
                marker = Path(directory) / 'candidate-would-run'
                result = subprocess.run(['bash', '-c', source + '\nprintf ran > "$CANDIDATE_MARK"\n'],
                    env={**os.environ, 'CANDIDATE': candidate, 'CONTEXT_SHA': context,
                         'CANDIDATE_MARK': str(marker)}, capture_output=True, text=True, timeout=10)
                if allowed:
                    if result.returncode or not marker.exists():
                        raise AssertionError('actual workflow context candidate was refused')
                elif result.returncode == 0 or marker.exists() or 'actual workflow context SHA' not in result.stdout:
                    raise AssertionError('mismatched workflow context reached candidate execution')


def check_rehearsal_context_boundary() -> None:
    for name in ('release-rehearsal.yml', 'release-rehearsal-windows.yml'):
        workflow = (ROOT / '.github/workflows' / name).read_text()
        check_context_checkout(workflow)
        assert_context_guards(workflow)
        expect_assertion('arbitrary input checkout restored', 'checkout must use',
            lambda: check_context_checkout(workflow.replace('ref: ${{ github.sha }}',
                'ref: ${{ inputs.candidate_sha }}', 1)))
        expect_assertion('guard context bound to arbitrary input', 'pre-checkout context guard lost',
            lambda: check_context_checkout(workflow.replace('CONTEXT_SHA: ${{ github.sha }}',
                'CONTEXT_SHA: ${{ inputs.candidate_sha }}', 1)))
        guard_start = workflow.index(CONTEXT_GUARD)
        guard_end = workflow.index('\n      - name:', guard_start + len(CONTEXT_GUARD)) + 1
        guard = workflow[guard_start:guard_end]
        without_guard = workflow[:guard_start] + workflow[guard_end:]
        checkout = without_guard.index('      - name: Checkout the exact rehearsal source\n')
        after_checkout = without_guard.index('\n      - name:', checkout + 1) + 1
        late_guard = without_guard[:after_checkout] + guard + without_guard[after_checkout:]
        expect_assertion('context guard moved after checkout', 'precede checkout',
            lambda: check_context_checkout(late_guard))
        expect_assertion('equality guard removed', 'reached candidate execution',
            lambda: assert_context_guards(workflow.replace('"$CANDIDATE" != "$CONTEXT_SHA"',
                '"$CANDIDATE" != "$CANDIDATE"', 1)))
