# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""Pin the pre-tag rehearsal to real steps, native runners and exact receipts."""
from pathlib import Path
import importlib.util

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('rehearsal_generator', ROOT / 'scripts/generate-release-rehearsal.py')
generator = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(generator)


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
    for contract in ('needs: [windows, tooling]', 'runs-on: ubuntu-latest',
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
            resolve = workflow.split('      - name: Resolve exact coherent release commit\n', 1)[1].split('\n      - name:', 1)[0]
            if resolve.index('echo "authority_main=$authority_main" >> "$GITHUB_OUTPUT"') > resolve.index('git checkout --detach "$sha"'):
                raise AssertionError('tag rehearsal policy authority must be pinned before candidate checkout')
        else:
            for contract in ('test "$(git rev-parse HEAD)" = "$GITHUB_SHA"',
                             'git --no-replace-objects show "${GITHUB_SHA}:scripts/release-rehearsal.py"'):
                if contract not in step:
                    raise AssertionError(f'cut rehearsal must use queued protected workflow policy: {contract}')
