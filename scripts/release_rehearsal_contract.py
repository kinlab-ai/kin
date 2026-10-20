# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""Pin the pre-tag rehearsal to real steps, native runners and exact receipts."""
from pathlib import Path
import importlib.util
import re

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('rehearsal_generator', ROOT / 'scripts/generate-release-rehearsal.py')
generator = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(generator)

CONTEXT_GUARD = '      - name: Require the workflow context candidate before checkout\n'


def check_context_checkout(workflow: str) -> None:
    for job in re.split(r'(?m)^  [A-Za-z0-9_-]+:\n', workflow):
        checkouts = job.count('uses: actions/checkout@')
        if not checkouts:
            continue
        if job.count('ref: ${{ github.sha }}') != checkouts or 'ref: ${{ inputs.' in job:
            raise AssertionError('rehearsal checkout must use the actual workflow context SHA')
        if '    steps:\n' + CONTEXT_GUARD not in job:
            raise AssertionError('rehearsal context guard must precede checkout and candidate execution')
        guard = job.split(CONTEXT_GUARD, 1)[1].split('\n      - name:', 1)[0]
        for contract in ('shell: bash', 'CANDIDATE: ${{ inputs.candidate_sha }}',
                         'CONTEXT_SHA: ${{ github.sha }}', 'set -euo pipefail',
                         'if [[ ! "$CONTEXT_SHA" =~ ^[0-9a-f]{40}$ || "$CANDIDATE" != "$CONTEXT_SHA" ]]; then',
                         'exit 1'):
            if contract not in guard:
                raise AssertionError(f'rehearsal pre-checkout context guard lost {contract}')


def check_contract(root: Path = ROOT) -> None:
    workflows = root / '.github/workflows'
    windows = (workflows / 'release-rehearsal-windows.yml').read_text()
    installer = (root / 'scripts/install.ps1').read_text()
    checksum = installer.split('$ChecksumFile = Join-Path $TmpDir "$Archive.sha256"', 1)[1].split('$ChecksumContent =', 1)[0]
    if 'Invoke-ArchiveDownload -Uri $ChecksumUrl -OutFile $ChecksumFile -ArchiveName "$Archive.sha256"' not in checksum:
        raise AssertionError('Windows checksum download must support the same file mirror as its archive')
    if 'Prove the Windows file mirror checksum boundary' not in windows or 'scripts/test-windows-install-mirror.ps1"' not in windows:
        raise AssertionError('Windows compatibility rehearsal must execute the actual file mirror boundary proof')
    if windows != generator.render((workflows / 'install-proof.yml').read_text()):
        raise AssertionError('the Windows rehearsal must be generated from the actual install proof')
    rehearsal = (workflows / 'release-rehearsal.yml').read_text()
    check_context_checkout(rehearsal)
    check_context_checkout(windows)
    runner = "runs-on: ${{ vars.KIN_RC_USE_GITHUB_RUNNERS == 'true' && 'ubuntu-latest' || 'depot-ubuntu-24.04-4' }}"
    if rehearsal.count(runner) != 2 or rehearsal.count('runs-on:') != 2:
        raise AssertionError('both release rehearsal Linux legs must use the reviewed Depot runner and outage switch')
    windows_runner = "runs-on: ${{ vars.KIN_RC_USE_GITHUB_RUNNERS == 'true' && matrix.os || matrix.runner }}"
    if windows.count(windows_runner) != 1 or '"os":"windows-latest","runner":"depot-windows-2022-16"' not in windows:
        raise AssertionError('the Windows rehearsal must keep its windows-latest name and the reviewed Depot runner')
    marker = "KIN_DISPOSABLE_RUNNER: ${{ vars.KIN_RC_USE_GITHUB_RUNNERS != 'true' && 'depot' || '' }}"
    declared = [line for line in windows.splitlines()
                if 'KIN_DISPOSABLE_RUNNER' in line and not line.lstrip().startswith('#')]
    if declared != ['      ' + marker]:
        raise AssertionError('only the Depot branch of the Windows rehearsal runner may declare a disposable Depot runner')
    for contract in ('needs: [windows, tooling]',
                     'name: kin-release-rehearsal', 'if-no-files-found: error',
                     'scripts/release-rehearsal.py" environment',
                     'scripts/release-rehearsal.py" imports',
                     'scripts/release-rehearsal.py" collect'):
        if contract not in rehearsal:
            raise AssertionError(f'release rehearsal lost required contract {contract}')
    if 'continue-on-error:' in rehearsal or 'secrets:' in rehearsal or 'write' in rehearsal:
        raise AssertionError('release rehearsal must be read-only and fail closed')
    rc = (workflows / 'rc-build.yml').read_text().split('\n  rehearsal:\n', 1)[1]
    for contract in ('uses: ./.github/workflows/release-rehearsal.yml',
                     'candidate_sha: ${{ github.sha }}', 'proof_mode: candidate-full',
                     'local_artifact: kin-windows-x86_64', 'needs: build'):
        if contract not in rc:
            raise AssertionError(f'RC Build lost candidate-bound rehearsal {contract}')
    for name, privilege in (('release-cut.yml', 'Mint repository-scoped evidence token'),
                            ('release-tag.yml', 'Mint kin-release-bot installation token')):
        workflow = (workflows / name).read_text()
        anchor = "      - name: Require the exact candidate's hosted release rehearsal\n"
        if anchor not in workflow or workflow.index(anchor) > workflow.index('      - name: ' + privilege):
            raise AssertionError(f'{name} must require rehearsal before privileged release writes')
        step = workflow.split(anchor, 1)[1].split('\n      - name:', 1)[0]
        for contract in ('set -euo pipefail', '--name kin-release-rehearsal',
                         'python3 -I "$rehearsal_policy" verify', '--candidate', '--run-id', '--repository'):
            if contract not in step:
                raise AssertionError(f'{name} rehearsal gate lost {contract}')
        if 'continue-on-error:' in step or '|| true' in step:
            raise AssertionError(f'{name} cannot tolerate a missing or failed release rehearsal')
        if name == 'release-tag.yml':
            for contract in ('AUTHORITY_MAIN: ${{ steps.resolve.outputs.authority_main }}',
                             'git --no-replace-objects show "${AUTHORITY_MAIN}:scripts/release-rehearsal.py"',
                             'GITHUB_ENV="$rehearsal_inert/env"',
                             'GITHUB_PATH="$rehearsal_inert/path"',
                             'GITHUB_STATE="$rehearsal_inert/state"',
                             'GITHUB_OUTPUT="$rehearsal_inert/output"',
                             'if [ "$rehearsed" -ne 0 ]; then',
                             '"$(git status --porcelain --untracked-files=all)" != "$rehearsal_before"',
                             '"$(git rev-parse HEAD)" != "$SHA"'):
                if contract not in step:
                    raise AssertionError(f'tag rehearsal must preserve protected policy isolation: {contract}')
            # The run is the locator step's reported answer, never a file it may
            # not have written, and the requirement is not gated on that answer,
            # so a locator that skips or stops reporting is refused here.
            for contract in ('RC_LOCATED: ${{ steps.locate.outputs.located }}',
                             'RC_RUN: ${{ steps.locate.outputs.run_id }}',
                             'if [ "$RC_LOCATED" != true ]; then'):
                if contract not in step:
                    raise AssertionError(f'tag rehearsal must read the located RC Build run explicitly: {contract}')
            if 'rc-locator-result.txt' in step:
                raise AssertionError('tag rehearsal must not read a locator result file in place of its outputs')
            condition = step.split('        if: >-\n', 1)[1].split('        env:\n', 1)[0]
            if condition != ("          steps.resolve.outputs.needed == 'true' &&\n"
                             "          steps.prior.outputs.ready == 'true'\n"):
                raise AssertionError('tag rehearsal must run whenever the mint proceeds, not only when an RC Build was located')
            locate = workflow.split('      - name: Locate the RC Build the release will ship\n', 1)[1].split('\n      - name:', 1)[0]
            if not locate.startswith('        id: locate\n'):
                raise AssertionError('the RC Build locator step must be id locate so its outputs reach the rehearsal gate')
            resolve = workflow.split('      - name: Resolve exact coherent release commit\n', 1)[1].split('\n      - name:', 1)[0]
            if resolve.index('echo "authority_main=$authority_main" >> "$GITHUB_OUTPUT"') > resolve.index('git checkout --detach "$sha"'):
                raise AssertionError('tag rehearsal policy authority must be pinned before candidate checkout')
        else:
            for contract in ('test "$(git rev-parse HEAD)" = "$GITHUB_SHA"',
                             'git --no-replace-objects show "${GITHUB_SHA}:scripts/release-rehearsal.py"'):
                if contract not in step:
                    raise AssertionError(f'cut rehearsal must use queued protected workflow policy: {contract}')
