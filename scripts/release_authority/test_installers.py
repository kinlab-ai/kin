# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Checks that pin the install scripts, publish-release-installers.yml and the
installer asset guards.
"""

from __future__ import annotations

import re

from .test_helpers import ROOT, expect_assertion, require


INSTALLER_ASSET_GUARD = ROOT / "scripts" / "verify-installer-release-assets.py"
INSTALLER_ASSET_GUARD_POLICY = "scripts/verify-installer-release-assets.py"
INSTALLER_ASSET_FALSIFIER = ROOT / "scripts" / "falsify-installer-release-assets.py"
INSTALLER_ASSET_FALSIFIER_POLICY = "scripts/falsify-installer-release-assets.py"
INSTALLER_BINARY_GUARD = ROOT / "scripts" / "verify-installer-archive-binaries.py"
INSTALLER_BINARY_GUARD_POLICY = "scripts/verify-installer-archive-binaries.py"
INSTALLER_BINARY_FALSIFIER = ROOT / "scripts" / "falsify-installer-archive-binaries.py"
INSTALLER_BINARY_FALSIFIER_POLICY = "scripts/falsify-installer-archive-binaries.py"


def assert_installer_asset_guard_wired(ci: str, release: str) -> None:
    """Keep the check that every installer asset name is one the release ships.

    Each install surface builds a release asset name out of the platform it
    detects, and the release publishes a fixed list of names. Nothing compared
    the two lists, so a disagreement rode every release to date: the POSIX
    installer asks `windows` for a `.tar.gz`, mapping the MSYS, MINGW, and
    CYGWIN shells onto it, while the Windows leg published only a `.zip`. The
    documented curl command 404'd there and every check stayed green.

    The guard runs on pull requests against the workflow's own asset lists and
    again inside the release against the bytes staged for upload. The falsifier
    runs beside it because a guard nobody has watched fail is not evidence that
    it can, and this defect is exactly what a check that cannot fail looks like.
    """

    for path, policy in (
        (INSTALLER_ASSET_GUARD, INSTALLER_ASSET_GUARD_POLICY),
        (INSTALLER_ASSET_FALSIFIER, INSTALLER_ASSET_FALSIFIER_POLICY),
    ):
        if not path.is_file():
            raise AssertionError(
                f"{policy} is missing; an installer could ask a release for an "
                "asset it does not publish and nothing would notice"
            )

    # Match whole invocations rather than searching for the path, which a
    # commented-out line would still satisfy.
    ci_lines = {line.strip() for line in ci.splitlines()}
    missing = sorted(
        command
        for command in (
            f"python3 ./{INSTALLER_ASSET_GUARD_POLICY}",
            f"python3 ./{INSTALLER_ASSET_FALSIFIER_POLICY}",
        )
        if command not in ci_lines
    )
    if missing:
        raise AssertionError(
            "ci.yml must run " + " and ".join(missing) + "; without both, an "
            "installer asset name can stop being published and no pull request "
            "would go red"
        )

    release_command = f"run: python3 ./{INSTALLER_ASSET_GUARD_POLICY} --assets-dir ."
    if release_command not in {line.strip() for line in release.splitlines()}:
        raise AssertionError(
            f"release.yml must run {INSTALLER_ASSET_GUARD_POLICY} against the "
            "staged assets; the workflow's intent is not the same evidence as "
            "the bytes about to be uploaded"
        )


def assert_installer_archive_binary_guard_wired(ci: str) -> None:
    """Keep the check that the installer names binaries the archive carries.

    Resolving the right archive name is only half an install. Once the download
    lands, the installer names the binaries inside it, and a Windows archive
    carries `.exe`-suffixed names. The installer named the bare form on every
    platform, so its mandatory-daemon assertion failed against an archive that
    was present, complete, and checksum-verified, aborting after the user had
    already waited for the download.

    This guard reads source rather than staged bytes, so it belongs on pull
    requests alone. The falsifier runs beside it because a guard nobody has
    watched fail is not evidence that it can.
    """

    for path, policy in (
        (INSTALLER_BINARY_GUARD, INSTALLER_BINARY_GUARD_POLICY),
        (INSTALLER_BINARY_FALSIFIER, INSTALLER_BINARY_FALSIFIER_POLICY),
    ):
        if not path.is_file():
            raise AssertionError(
                f"{policy} is missing; the installer could name a binary the "
                "release archive does not carry and nothing would notice"
            )

    # Match whole invocations rather than searching for the path, which a
    # commented-out line would still satisfy.
    ci_lines = {line.strip() for line in ci.splitlines()}
    missing = sorted(
        command
        for command in (
            f"python3 ./{INSTALLER_BINARY_GUARD_POLICY}",
            f"python3 ./{INSTALLER_BINARY_FALSIFIER_POLICY}",
        )
        if command not in ci_lines
    )
    if missing:
        raise AssertionError(
            "ci.yml must run " + " and ".join(missing) + "; without both, the "
            "installer can abort on a complete archive and no pull request "
            "would go red"
        )



def check_installer_registry_authority(install_ps1: str, install_sh: str) -> None:
    require(
        install_sh,
        '"$EXTRACT_DIR/kin$BIN_EXT" registry authority --initialize',
        "content-free Unix installer registry-authority initialization",
    )
    require(
        install_sh,
        "No installed binary or registry authority file was replaced.",
        "fail-closed Unix installer registry-authority preflight",
    )
    require(
        install_ps1,
        '@("registry", "authority", "--json")',
        "honest Windows registry-authority capability report",
    )
    if "KIN_REGISTRY_REPAIR" in install_ps1:
        raise AssertionError(
            "Windows installer must not imply Unix mode repair support"
        )



def check_installer_callback(installer_callback: str, update_trust: str) -> None:
    for policy in (
        'workflows: ["Release"]',
        "types: [completed]",
        "actions: read",
        "contents: read",
        "vars.RELEASE_FOLLOWUP_READY == 'true'",
        "github.event.workflow_run.status == 'completed'",
        "github.event.workflow_run.conclusion == 'success'",
        "github.event.workflow_run.event == 'push'",
        "startsWith(github.event.workflow_run.head_branch, 'v')",
        "!contains(github.event.workflow_run.head_branch, '-')",
        "environment: release-followups",
        "timeout-minutes: 30",
        "SOURCE_RUN_ID: ${{ github.event.workflow_run.id }}",
        "KIN_TAG: ${{ github.event.workflow_run.head_branch }}",
        "KIN_SHA: ${{ github.event.workflow_run.head_sha }}",
        "Require current callback authority without blocking unrelated main progress",
        '"repos/${GITHUB_REPOSITORY}/compare/${GITHUB_SHA}...${current}"',
        '[ "$ancestry" = ahead ] || [ "$ancestry" = identical ]',
        "authority_paths=(",
        ".github/workflows/publish-release-installers.yml",
        "scripts/verify-homebrew-formula.sh",
        "scripts/validate-homebrew-formula.py",
        "scripts/verify_installer_parity.py",
        '"repos/${GITHUB_REPOSITORY}/contents/${path}?ref=${GITHUB_SHA}"',
        '"repos/${GITHUB_REPOSITORY}/contents/${path}?ref=${current}"',
        "callback authority changed on main after this event",
        'gh api "repos/${GITHUB_REPOSITORY}/actions/runs/${SOURCE_RUN_ID}"',
        'assert_field "source run status" "$(jq -r .status <<< "$run")" completed',
        'assert_field "source run conclusion" "$(jq -r .conclusion <<< "$run")" success',
        'assert_field "source run workflow" "$(jq -r .path <<< "$run")" .github/workflows/release.yml',
        'assert_field "release tag target sha" "$peeled" "$KIN_SHA"',
        'gh api "repos/${GITHUB_REPOSITORY}/releases/tags/${KIN_TAG}"',
        'gh api "repos/${GITHUB_REPOSITORY}/releases/latest"',
        "ref: ${{ env.KIN_SHA }}",
        "actions/create-github-app-token@bcd2ba49218906704ab6c1aa796996da409d3eb1",
        "secrets.KIN_RELEASE_APP_ID",
        "secrets.KIN_RELEASE_APP_PRIVATE_KEY",
        "repositories: |",
        "homebrew-kin",
        "kin-infra",
        "permission-actions: read",
        "permission-contents: write",
        'event_type:"kin-release"',
        "https://api.github.com/repos/kinlab-ai/homebrew-kin/dispatches",
        "Update formula ${KIN_TAG} from Kin run ${SOURCE_RUN_ID}",
        "actions/workflows/update-formula.yml/runs?event=repository_dispatch",
        'bash scripts/verify-homebrew-formula.sh "$KIN_TAG"',
        'event_type:"publish-install"',
        "schema_version:1",
        "release_workflow_run_id:$run_id",
        "install_sha256:$install_sha256",
        "install_ps1_sha256:$install_ps1_sha256",
        "Publish installers ${KIN_TAG} from Kin run ${SOURCE_RUN_ID}",
        "actions/workflows/publish-install.yml/runs?event=repository_dispatch",
        "the workflow may still be disabled",
        'python3 scripts/verify_installer_parity.py "$KIN_TAG" --expected-sha "$KIN_SHA" --surface pages',
        # The image-promotion dispatch, third beside the two above and under the
        # same token, whose repositories list already carries kin-infra.
        #
        # The payload is pinned to the TAG ALONE on purpose. kin-infra's receiver
        # resolves the commit, the release run, the GHCR digest and both main
        # heads itself and refuses a tag that is not exact GitHub Latest, which is
        # the rule publish-install.yml states for this pair: payload fields are
        # never trusted without independent API verification. Sending more would
        # be sending a caller-supplied identity for a production image, so the
        # exact payload text is pinned rather than only the event type.
        'event_type:"promote-release-images"',
        "client_payload:{schema_version:1,kin_tag:$tag}",
        "image promotion repository dispatch returned HTTP $code",
    ):
        require(installer_callback, policy, "completed-release follow-up callback")

    if '[ "$current" = "$GITHUB_SHA" ]' in installer_callback:
        raise AssertionError(
            "the callback must tolerate unrelated forward progress on main while "
            "pinning its runtime authority blobs"
        )

    for policy in (
        "`kinlab-ai/homebrew-kin` and `kinlab-ai/kin-infra`",
        "`KIN_RELEASE_APP_ID`",
        "`KIN_RELEASE_APP_PRIVATE_KEY`",
        "`RELEASE_FOLLOWUP_READY`",
        "Contents write permission",
        "Actions read",
    ):
        require(update_trust, policy, "release follow-up trust documentation")

    callback_admission_start = installer_callback.index("    if: >-")
    callback_admission_end = installer_callback.index(
        "    runs-on:", callback_admission_start
    )
    callback_admission = installer_callback[
        callback_admission_start:callback_admission_end
    ]
    require(
        callback_admission,
        "!contains(github.event.workflow_run.head_branch, '-')",
        "stable-only installer callback admission",
    )

    for forbidden in (
        "workflow_dispatch:",
        "push:",
        "pull_request:",
        "gcloud",
        "WIF_INSTALLER_PROVIDER",
        "WIF_INSTALLER_SERVICE_ACCOUNT",
    ):
        if forbidden in installer_callback:
            raise AssertionError(
                f"completed-release installer callback contains forbidden authority: {forbidden}"
            )
    for permission in ("contents", "id-token", "packages", "attestations"):
        if re.search(
            rf"(?m)^\s+{re.escape(permission)}:\s*write\s*$", installer_callback
        ):
            raise AssertionError(
                "completed-release installer callback grants its workflow token "
                f"forbidden {permission}:write authority"
            )



def check_installer_guards_wired(ci_workflow: str, release: str) -> None:
    assert_installer_asset_guard_wired(ci_workflow, release)
    expect_assertion(
        "ci.yml drops the guard that installer asset names are published",
        "ci.yml must run",
        lambda: assert_installer_asset_guard_wired(
            ci_workflow.replace(INSTALLER_ASSET_GUARD_POLICY, "scripts/absent.py"),
            release,
        ),
    )
    expect_assertion(
        "ci.yml keeps the guard but drops its falsification",
        "ci.yml must run",
        lambda: assert_installer_asset_guard_wired(
            ci_workflow.replace(INSTALLER_ASSET_FALSIFIER_POLICY, "scripts/absent.py"),
            release,
        ),
    )
    expect_assertion(
        "the release stops checking installer asset names against its own assets",
        "must run",
        lambda: assert_installer_asset_guard_wired(
            ci_workflow,
            release.replace(f"./{INSTALLER_ASSET_GUARD_POLICY} --assets-dir .", "true"),
        ),
    )
    assert_installer_archive_binary_guard_wired(ci_workflow)
    expect_assertion(
        "ci.yml drops the guard that installer binary names match the archive",
        "ci.yml must run",
        lambda: assert_installer_archive_binary_guard_wired(
            ci_workflow.replace(INSTALLER_BINARY_GUARD_POLICY, "scripts/absent.py")
        ),
    )
    expect_assertion(
        "ci.yml keeps the binary-name guard but drops its falsification",
        "ci.yml must run",
        lambda: assert_installer_archive_binary_guard_wired(
            ci_workflow.replace(INSTALLER_BINARY_FALSIFIER_POLICY, "scripts/absent.py")
        ),
    )
