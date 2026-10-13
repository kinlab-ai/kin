# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Shared paths, parsing helpers and assertion primitives.

Every module in this package imports from here, and nothing here imports
from a workflow module.
"""

from __future__ import annotations

import hashlib
import os
import re
import subprocess
import tempfile
import textwrap
from collections.abc import Callable
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent.parent
WORKFLOWS = ROOT / ".github" / "workflows"
README = ROOT / "README.md"
RELEASE = WORKFLOWS / "release.yml"
RELEASE_CUT = WORKFLOWS / "release-cut.yml"
RELEASE_RECOVERY = WORKFLOWS / "release-recovery.yml"
RELEASE_TAG = WORKFLOWS / "release-tag.yml"

RELEASE_TRAIN = WORKFLOWS / "release-train.yml"
SAST = WORKFLOWS / "sast.yml"
ADVISORY_SWEEP = WORKFLOWS / "advisory-sweep.yml"
ADVISORY_SWEEP_SCRIPT = ROOT / "scripts" / "advisory-sweep.mjs"
HOLD_ALARM = ROOT / "scripts" / "release-hold-alarm.mjs"
HOLD_ALARM_POLICY = "scripts/release-hold-alarm.mjs"
PROOF_GATE = ROOT / "scripts" / "check-release-proof-artifacts.mjs"

INSTALL_PROOF_CANARY = WORKFLOWS / "install-proof-canary.yml"

CAPABILITY_CONTRACT = ROOT / "scripts" / "verify-capability-proof.mjs"
CAPABILITY_CONTRACT_POLICY = "scripts/verify-capability-proof.mjs"
RELEASE_BOT_DOC = ROOT / "docs" / "release-bot.md"
INSTALL_PROOF = WORKFLOWS / "install-proof.yml"
PORTED_VALIDATOR = (
    ROOT
    / "scripts"
    / "release-proof"
    / "bin"
    / "kin-release-preflight.d"
    / "validate.mjs"
)
PREFLIGHT_DRIVER = ROOT / "scripts" / "release-proof" / "bin" / "kin-release-preflight"

QUICKSTART_DOC = ROOT / "docs" / "quickstart.md"
README_REFERENCE_DOC = ROOT / "docs" / "readme-reference.md"
MCP_TOOLS_DOC = ROOT / "docs" / "mcp-tools.md"

NPM_CANONICAL_README = ROOT / "packages" / "kin" / "README.md"

INSTALLER_CALLBACK = WORKFLOWS / "publish-release-installers.yml"
UPDATE_TRUST = ROOT / "docs" / "security" / "signing-and-update-trust.md"

INSTALL_SH = ROOT / "scripts" / "install.sh"
INSTALL_PS1 = ROOT / "scripts" / "install.ps1"

RELEASE_VERSION_FALSIFIER = ROOT / "scripts" / "falsify-release-version-guards.py"

TRUSTED_POLICY_PREFIX = "refs/remotes/origin/main:"
TAG_LISTING_FORMAT = (
    "--format='%(refname:strip=2) "
    "%(if)%(*objectname)%(then)%(*objectname)%(else)%(objectname)%(end)'"
)

RELEASE_APP_TOKEN = "${{ steps.app-token.outputs.token }}"
DEFAULT_WORKFLOW_TOKEN = "${{ github.token }}"
STEP_ENV_TOKEN_BINDING = re.compile(r"(?m)^\s+GH_TOKEN:\s*(?P<token>\S.*?)\s*$")

HEALTH = ROOT / "crates" / "kin-cli" / "src" / "commands" / "health.rs"
SETUP = ROOT / "crates" / "kin-cli" / "src" / "commands" / "setup.rs"
DOCKERFILE = ROOT / "Dockerfile"
DEV_IMAGE_PUBLISHER = ROOT / "scripts" / "publish-dev-container.sh"
DEV_IMAGE_TOOL = ROOT / "scripts" / "kin-dev-image"

RUST_CACHE_ACTION = "Swatinem/rust-cache@c19371144df3bb44fab255c43d04cbc2ab54d1c4"

MAIN_ONLY_CACHE_SAVE_VALUE = "${{ github.ref == 'refs/heads/main' }}"
# A job that reads the cache entry another job writes, and must never write it
# itself, saves on no ref at all. That is strictly narrower than the main-only
# scalar and never broader, so it cannot become the way an untrusted ref
# poisons a cache. These two values are the whole allowlist and anything else
# is still rejected. Nothing here has to re-prove that a saver still exists:
# the falsification below pins the check job's own main-only save by count.
RESTORE_ONLY_CACHE_SAVE_VALUE = "false"
CACHE_SAVE_VALUES = frozenset(
    {MAIN_ONLY_CACHE_SAVE_VALUE, RESTORE_ONLY_CACHE_SAVE_VALUE}
)
RUST_CACHE_REFERENCE = re.compile(r"Swatinem/rust-cache@", re.IGNORECASE)
CANONICAL_STEP_FIELDS = frozenset(
    {
        "continue-on-error",
        "env",
        "id",
        "if",
        "name",
        "run",
        "shell",
        "timeout-minutes",
        "uses",
        "with",
        "working-directory",
    }
)

REQUIRED_RELEASE_CHECKS = (
    "Check & Test (ubuntu-latest)",
    "Check & Test (macos-latest)",
    "Falsify guards",
    "Feature permutation tests (ubuntu-latest)",
    "Feature permutation tests (macos-latest)",
    "DCO Sign-off",
    "cargo-deny",
    "gitleaks (full history)",
    "Windows installer + vector release build",
)

CI_JOB_DISPLAY_NAMES = {
    "dco": "DCO Sign-off",
    "release-version": "Release version gate",
    "npm-launchers": "npm launcher tests",
    "windows-authority-tests": "Windows authority tests",
    "windows-authority-cli-tests": "Windows authority CLI tests",
    "windows-authority-runtime-tests": "Windows authority runtime tests",
    "windows-installer": "Windows installer + vector release build",
    "changes": "Classify diff scope",
    # The admission core. Both run on every event and between them
    # they are the whole of what blocks a pull request. `fast-gate-tests`
    # publishes a shard name no ruleset requires; `fast-gate-tests-aggregate`
    # publishes the required name and is green only when every shard succeeded.
    "fast-gate-lint": "Fast gate lint and policy",
    "fast-gate-tests": "Fast gate test shard",
    "fast-gate-tests-aggregate": "Fast gate build and tests",
    # The pull request's only Windows evidence. All four native Windows jobs
    # carry `github.event_name != 'pull_request'`, so on 2026-09-02 a helper
    # that was dead code under `cfg(not(unix))` read green through its whole
    # pull request and turned every Windows job on main red for an hour. This
    # cross-compiles the shipped package set for x86_64-pc-windows-gnu, which
    # reproduces that class because `cfg(unix)` is false for both Windows
    # targets and msvc cannot cross-compile off Windows. It publishes a name no
    # ruleset requires and blocks through the aggregate's `needs:` below.
    "windows-cross-check": "Windows cross-check",
    # The served MCP surface, graded on the pull request that moves it. Five
    # assertions read that surface and all five run only on main's push, so a
    # profile change that moved a served name and trimmed three schema knobs and
    # a description sentence was `skipped` across 44 of 44 check runs on its own
    # pull request and red on main for four landings. This job publishes no
    # required context and claims none: `bin/kin-lane merge land` refuses on any
    # check that is not green by name, and ruleset 19746451 is what would make it
    # additionally block a merge through GitHub's own UI.
    "mcp-surface-contract": "MCP surface contract",
    # Every Kin library tested on its own minimal features and packaged, on
    # every event. A workspace unifies features, so a library can be healthy
    # here and broken for the consumer who depends on it alone; feature workers
    # run the same minimal-feature legs on macOS on main and export pull requests,
    # which is a landing too late. Like `mcp-surface-contract` it publishes no
    # required context and claims none: `bin/kin-lane merge land` refuses on any
    # check that is not green by name, and a ruleset is what would make it
    # additionally block a merge through GitHub's own UI.
    "library-packages": "Library packages",
    # The inert pull-request producer of the two expanded `Check & Test` names.
    # It covered documentation-only diffs alone until the admission core took the ubuntu
    # and macOS suites off the pull-request path; it now covers every pull
    # request, which is why the id no longer says docs.
    "check-pr-fast-path": "Check & Test",
    # The ubuntu half of Check & Test, split so its 14.98-minute test step can
    # run as two nextest partitions. The shard job keeps the id `check`, which
    # bin/kin-precheck and rust-cache both key on, and publishes a name of its
    # own that is required by nothing; `check-aggregate` publishes
    # `Check & Test` with a one-value matrix, which expands to the
    # release-required `Check & Test (ubuntu-latest)` and is what the ruleset
    # names.
    "check": "Check & Test ubuntu shard",
    # Every source-reading gate, clippy, both musl target checks and the
    # doctests, once, beside the partitions instead of in front of one. It
    # publishes a name no ruleset requires; `check-aggregate` requires it.
    "check-lint": "Check & Test ubuntu lint",
    "check-aggregate": "Check & Test",
    # The macOS half of Check & Test, split so its 14.2-minute test step can run
    # as two nextest partitions. The shard job publishes a name of its own and
    # is required by nothing; the aggregate publishes `Check & Test` with a
    # one-value matrix, which expands to the release-required
    # `Check & Test (macos-latest)` and is what the ruleset names.
    "check-macos": "Check & Test macOS shard",
    # Clippy over macOS-gated code, the boundary lint and the doctests, beside
    # the five partitions. Required by `check-macos-aggregate`, by nothing else.
    "check-macos-lint": "Check & Test macOS lint",
    "check-macos-aggregate": "Check & Test",
    # Falsification and feature permutations run beside Check & Test. Feature
    # workers publish their OS/group outcomes; only the PR stub and two exact
    # per-platform aggregates own the release-required feature context names.
    "falsify-guards": "Falsify guards",
    "feature-tests-pr-fast-path": "Feature permutation tests",
    "feature-tests": "Feature permutation tests",
    "feature-tests-macos-aggregate": "Feature permutation tests",
    "feature-tests-ubuntu-shards": "Feature permutation shard",
    "feature-tests-macos-shards": "Feature permutation shard",
    "install-proof-pr-build": "Install Proof (PR) Build",
    "install-proof-pr": "Install Proof (PR)",
    "install-proof-pr-gate": "Install Proof (PR) Gate",
}
EXPECTED_WORKFLOW_JOB_DISPLAY_NAMES: dict[str, dict[str, str | None]] = {
    # The per-pull-request product acceptance job. It publishes no
    # required context and claims none; it is registered here because this
    # census is what would otherwise let a new workflow's job NAME appear
    # unreviewed. The job id and display name are stable on purpose: renaming
    # either is what ejects unrelated queue entries.
    # The red-Acceptance alarm. It triggers only on `workflow_run` once
    # Acceptance concludes, so it runs on no pull-request or merge-group event and
    # can never claim a required context. Its only write is one tracking issue.
    # The pull request's producer of `Release preflight`, which the merge queue
    # requires. On the queue's commit Release Cut posts it as a commit status,
    # so this workflow runs on pull_request and nothing else. The tag mint
    # never reads the context.
    ".github/workflows/release-preflight-stand-in.yml": {
        "release-preflight": "Release preflight",
    },
    ".github/workflows/acceptance-red-alarm.yml": {
        "alarm": "Report a red Acceptance on main",
    },
    ".github/workflows/acceptance.yml": {
        # One release build, five suite jobs that grade it on runners of their
        # own, and the gate. Only the gate carries the name the landing tool
        # requires to pass; the others publish names nothing requires.
        "acceptance-build": "Acceptance release build",
        "acceptance-brownfield": "Acceptance brownfield suite",
        "acceptance-core-suites": "Acceptance magic and memory suites",
        "acceptance-init-memory": "Acceptance init-memory suite",
        "acceptance-lifecycle-suites": "Acceptance lifecycle suites",
        "acceptance-surface-suites": "Acceptance read-surface suites",
        "acceptance": "Product Acceptance",
    },
    # The scheduled advisory sweep. It publishes no required context and runs on
    # no pull-request or merge-group event, so it can never claim one; it is
    # registered here because this census is what would otherwise let a new
    # job's NAME appear unreviewed.
    ".github/workflows/advisory-sweep.yml": {
        "sweep": "Sweep advisories",
    },
    # The typed-dispatch proof that the alarm credential can open and close an
    # issue on kin-infra. It publishes no required context and runs on no
    # pull-request or merge-group event; registered so its job NAME cannot
    # appear unreviewed.
    ".github/workflows/alarm-dry-run.yml": {
        "dry-run": "Open and close one alarm on kin-infra",
    },
    ".github/workflows/approve-to-merge.yml": {
        "gate": None,
    },
    ".github/workflows/base-image-pins.yml": {
        "verify-pins": "Verify base image pins",
    },
    ".github/workflows/ci.yml": CI_JOB_DISPLAY_NAMES,
    ".github/workflows/claude.yml": {
        "preflight": "Resolve responder credential",
        "respond": "Respond to the mention",
    },
    ".github/workflows/daemon-smoke.yml": {
        "daemon-smoke": "Linux Daemon Smoke + MCP Headless",
    },
    ".github/workflows/dco.yml": {
        # This pull_request_target check is deliberately distinct from the
        # release-required CI job. GitHub check names are case-sensitive:
        # lowercase "sign-off" is extra PR evidence, never release evidence.
        "dco": "DCO sign-off",
    },
    ".github/workflows/docker.yml": {
        "build-image": "Build and publish dev daemon image",
    },
    ".github/workflows/fuzz.yml": {
        "fuzz": "cargo-fuzz (${{ matrix.target }})",
    },
    ".github/workflows/install-proof.yml": {
        "install-proof": "${{ matrix.os }}",
        # Proves the real update path moves a real installation from
        # the previously published release to Latest. It produces no required
        # check context, and unlike every other leg of this workflow it does
        # not gate a release: its witnesses are the weekly schedule and a
        # manual dispatch. A release tag is still a pre-release while this
        # workflow runs, so the stable channel would correctly refuse to move
        # and the job would fail every cut.
        # `assert_update_proof_stays_off_the_release_path` holds that scoping.
        "update-proof": "Update proof (N-1 to Latest)",
    },
    # The dependency receiver validates the exact payload, prepares and compiles
    # candidate registry bytes without a write credential, then admits only the
    # hash-bound manifest delta on a fresh App-authenticated runner. It publishes
    # no pull-request or merge-group context.
    ".github/workflows/kin-registry-release.yml": {
        "validate-dispatch": "Validate Kin registry release",
        "disarm-wave": "Disarm inherited Kin registry landing state",
        "prepare-wave": "Prepare and compile Kin registry pins",
        "mutate-wave": "Update the fixed Kin registry PR",
    },
    # This workflow_run consumer persists the exact App attestation only after
    # the receiver attempt is terminal-successful. It produces no required
    # context; the required CI gate reads and revalidates its server evidence.
    ".github/workflows/kin-registry-release-attest.yml": {
        "attest-completed-receiver": "Attest completed Kin registry receiver",
    },
    # The wave's own landing. The judge reads every check-run on the wave head
    # by name and the land job squash-merges through the release App only when
    # that full set has concluded green. Both run on workflow_run and schedule,
    # so neither publishes a pull-request or merge-group context.
    ".github/workflows/kin-registry-wave-land.yml": {
        "judge-wave": "Judge the open Kin registry wave",
        "land-wave": "Land the green Kin registry wave",
    },
    ".github/workflows/link-check.yml": {
        "link-check": "Check public documentation links",
    },
    # Manual fix-forward publisher for a release whose registry entry failed;
    # workflow_dispatch only, so it never produces a pull request check.
    ".github/workflows/mcp-registry-republish.yml": {
        "republish_mcp_registry": "Publish to the MCP Registry",
    },
    # Reports an ejection on the pull request the merge queue dropped. It runs
    # on workflow_run after the fact, produces no check on a pull request, and
    # so is never a required-context producer.
    ".github/workflows/merge-queue-ejection-notice.yml": {
        "notice": None,
    },
    ".github/workflows/notify-approver.yml": {
        "notify": None,
    },
    ".github/workflows/pr-text-hygiene.yml": {
        "pr_text": "PR text hygiene",
    },
    ".github/workflows/publish-release-installers.yml": {
        "dispatch": None,
    },
    # The only producer of release bytes: the archives and the daemon image,
    # built on the merge queue for main or by Release Cut's dispatch. Its build
    # jobs hold no credential and it publishes nothing; its names are not among
    # the release-required contexts, so a merge-group run of it claims none of
    # them. It is registered here because this census is what would otherwise
    # let a new job's NAME appear unreviewed.
    ".github/workflows/rc-build.yml": {
        "build": "RC Build (${{ matrix.artifact }})",
        "image": "RC Daemon Image",
        "capability": "RC Capability (${{ matrix.os }})",
        "attest": "RC Attest",
    },
    ".github/workflows/registry-index-migrate.yml": {
        "migrate": None,
    },
    # The automatic candidate selection and the hosted half of its proof. Every
    # trigger resolves from protected main and none of its jobs runs on a
    # pull_request or merge_group event, so none can claim a required context;
    # it is registered here because this census is what would otherwise let a
    # new workflow's job NAME appear unreviewed.
    ".github/workflows/release-cut.yml": {
        "select": "Select the release candidate",
        "arm": "Arm the release candidate",
        # A merge group's RC Build cannot be dispatched again, so a runner
        # timeout there is retried by re-running its failed jobs in place.
        "retry": "Re-run an RC Build its runner timeout killed",
        "preflight": "Preflight ${{ matrix.artifact }}",
        "publish": "Publish the candidate's preflight record",
        # Posts the commit status the merge queue waits on, so a queued
        # release lands only once its preflight record is filed.
        "report-to-queue": "Tell the merge queue whether this group's release is proven",
        # The stranger runs on a runner the fleet owns, gated by the
        # KIN_STRANGER_RUNNER variable, because three concurrent arms need 15
        # CPUs and 36 GB and a job whose labels match no online runner queues
        # rather than skipping. It reads the driver's API key and nothing else;
        # the record it produces is filed by publish-stranger, so the release
        # App never reaches the machine executing candidate bytes.
        "stranger": "Run the stranger on the candidate archive",
        "publish-stranger": "Publish the candidate's stranger record",
        "stranger-standby": "Report the stranger a runner cannot take",
        "report": "Report the cut's decision",
    },
    # The far end of the proof contract. A tag mints on the machine preflight
    # alone and the release is published AND promoted; this sweep is what
    # finishes the record afterwards, taking the pending notice back out once a
    # stranger record lands, promoting the older shape that was held as a
    # prerelease, and alarming while neither has happened. None of its jobs
    # runs on a pull_request or
    # merge_group event, so none can claim a required context; it is registered
    # here because this census is what would otherwise let a new workflow's job
    # NAME appear unreviewed.
    ".github/workflows/release-promote.yml": {
        "promote": "Promote proven releases to Latest",
    },
    ".github/workflows/release-recovery.yml": {
        "reconcile": "Reconcile failed release",
    },
    ".github/workflows/install-proof-canary.yml": {
        "capability-canary": "Capability Contract Canary",
    },
    ".github/workflows/release-tag.yml": {
        "mint-release-tag": "Mint release tag",
    },
    ".github/workflows/release-train.yml": {
        "reconcile": "Reconcile release PR",
        "hold-alarm": "Report a held rail",
    },
    ".github/workflows/release.yml": {
        "duplicate_guard": "Refuse a duplicate release run",
        "config": "Resolve release config",
        "locate_rc_build": "Locate the RC Build for this tag",
        "publish_daemon_image": "Publish the RC Build daemon image",
        "attest_daemon_image": "Attest immutable daemon image",
        "ship_rc_archives": "Ship RC archive (${{ matrix.artifact }})",
        "sign_macos": "Sign and notarize (${{ matrix.artifact }})",
        "notarize_linux": "Notarize macOS binaries (Linux / rcodesign)",
        "publish": "Publish Release",
        "install_proof": "Public Install Proof",
        "npm_publish_preflight": "Preflight Both npm Packages",
        "publish_npm_compatibility": (
            "Publish npm Compatibility Wrapper (@kinlab/kin-mcp)"
        ),
        "publish_npm_canonical": "Publish npm Canonical Package (@kinlab/kin)",
        "verify_npm_published": (
            "Verify Published npm Provenance (${{ matrix.package }})"
        ),
        "smoke_npm_published": (
            "Anonymous Published npm Smoke (${{ matrix.package }})"
        ),
        "publish_mcp_registry": "Publish to the MCP Registry",
        "finalize_release": "Promote Proven Release",
        "promote_ghcr_latest": "Promote stable ghcr latest",
        "publish_boundary_contracts": "Post-release boundary contracts publish",
        "version_tag_image": "Version-tag ghcr image",
        "seal_release_completion": "Seal completed stable release",
    },
    ".github/workflows/sast.yml": {
        "changes": "Classify diff scope",
        "cargo-deny": "cargo-deny",
    },
    ".github/workflows/secret-scan.yml": {
        "gitleaks": "gitleaks (full history)",
    },
    # workflow_dispatch only, so as written it publishes no check run on a pull
    # request or merge group. Registered here because this census is what would
    # otherwise let a new job's NAME appear unreviewed.
    #
    # It does not follow that the dispatch-only property is protected: this
    # census reads job names and never `on:` triggers, so adding
    # `pull_request:` to that workflow leaves the suite green. Verified by
    # injecting the trigger and re-running.
    ".github/workflows/windows-vector-proof.yml": {
        "windows-vector-proof": "Windows semantic vector search proof",
    },
}
EXPECTED_DYNAMIC_JOB_CONTEXT_SHA256 = {
    (
        ".github/workflows/fuzz.yml",
        "fuzz",
    ): "aa645addde738ea8f1fac9c70071ac0f603fd0030602f15d013535abf9a32443",
    (
        ".github/workflows/install-proof.yml",
        "install-proof",
    ): "3ab38cec398001873f4a29bb17ce14264b6eb54dbbe2b865ff109d1d4a74767d",
    (
        ".github/workflows/rc-build.yml",
        "build",
    ): "d548aec1dfb72f778e575467b70033f3c9d80688181ec23bb91adbbaf9694c1c",
    (
        ".github/workflows/rc-build.yml",
        "capability",
    ): "dfd5ec09bf9ff8cd1ad4bcba0801b372f9e9f32f54f165f5fc4c57046e6dd893",
    (
        ".github/workflows/release-cut.yml",
        "preflight",
    ): "f86f3c6f46bcacba012e93cd28dab5aaa2d711a66078007e4a4fbeb21e6629e7",
    (
        ".github/workflows/release.yml",
        "ship_rc_archives",
    ): "fdf251213c7c66aacbb6144d8b1e92b18e7638f3a1b872c4d8d266fbe9a87867",
    (
        ".github/workflows/release.yml",
        "sign_macos",
    ): "d6016492fc501dada9e7d63ea118160e68e341bece02a6e630b31518e09b9ca5",
    (
        ".github/workflows/release.yml",
        "verify_npm_published",
    ): "be398c786bc9f4a31e8a4196076f707a93b33b70c0ec9b4a0ea5e87f3e84c314",
    (
        ".github/workflows/release.yml",
        "smoke_npm_published",
    ): "495829aee07c97dfd59924fcac7ff3ddb57be574ffd0bf741adc93a37425b492",
}
REQUIRED_CHECK_JOB_PRODUCERS = {
    # Three producers, one per required expansion plus the documentation-only
    # stub. `check-aggregate` carries ubuntu-latest and is green only when every
    # `check` shard succeeded, `check-macos-aggregate` carries macos-latest on
    # the same terms, and `check-docs-only` publishes both names on a
    # documentation-only diff. `check` and `check-macos` publish names of their
    # own that no ruleset requires, which is what keeps a shard from being a
    # second check run under a required name. The real jobs and the stub stay
    # mutually exclusive by condition, so no two of them ever publish the same
    # expanded name on one event.
    "Check & Test": {
        (".github/workflows/ci.yml", "check-pr-fast-path"),
        (".github/workflows/ci.yml", "check-aggregate"),
        (".github/workflows/ci.yml", "check-macos-aggregate"),
    },
    "Falsify guards": {
        (".github/workflows/ci.yml", "falsify-guards"),
    },
    # The admission core. Neither is required by a ruleset at the
    # moment this lands; the captain adds them, and the shape of that change is
    # in the report. They are registered here for the reason `Install Proof (PR)
    # Gate` is: this map is what stops a second job appearing under a name a
    # ruleset is about to block on, and a name is easiest to steal in the window
    # before anything requires it.
    "Fast gate lint and policy": {
        (".github/workflows/ci.yml", "fast-gate-lint"),
    },
    "Fast gate build and tests": {
        (".github/workflows/ci.yml", "fast-gate-tests-aggregate"),
    },
    "Fast gate test shard": {
        (".github/workflows/ci.yml", "fast-gate-tests"),
    },
    # One inert PR producer plus one fail-closed aggregate for each OS. Worker
    # names never claim either required expanded feature context.
    "Feature permutation tests": {
        (".github/workflows/ci.yml", "feature-tests-pr-fast-path"),
        (".github/workflows/ci.yml", "feature-tests"),
        (".github/workflows/ci.yml", "feature-tests-macos-aggregate"),
    },
    "DCO Sign-off": {
        (".github/workflows/ci.yml", "dco"),
    },
    "cargo-deny": {
        (".github/workflows/sast.yml", "cargo-deny"),
    },
    "gitleaks (full history)": {
        (".github/workflows/secret-scan.yml", "gitleaks"),
    },
    "Windows installer + vector release build": {
        (".github/workflows/ci.yml", "windows-installer"),
    },
    # The pull-request install proof's required context is this gate rather
    # than the reusable call's per-leg checks. A reusable-workflow call
    # publishes `<caller> / <leg>`, and a caller that skips publishes nothing
    # under that name at all, so requiring a leg would hang every
    # documentation-only pull request on a check that will never report.
    "Install Proof (PR) Gate": {
        (".github/workflows/ci.yml", "install-proof-pr-gate"),
    },
}

RELEASE_GATE_FIXTURE_SHA = "1" * 40


def require(content: str, needle: str, context: str) -> None:
    if needle not in content:
        raise AssertionError(f"{context} is missing required policy: {needle}")


def expect_assertion(
    label: str,
    expected_error: str,
    check: Callable[[], None],
) -> None:
    try:
        check()
    except AssertionError as error:
        if expected_error not in str(error):
            raise AssertionError(
                f"falsification failed for the wrong reason: {label}: {error}"
            ) from error
        return
    raise AssertionError(f"falsification did not fail: {label}")


def workflow_paths(directory: Path = WORKFLOWS) -> list[Path]:
    """Return every workflow filename GitHub Actions recognizes."""

    return sorted(
        path
        for path in directory.iterdir()
        if path.is_file() and path.suffix in {".yml", ".yaml"}
    )


def classifier_active_job_source(classifier: str) -> str:
    """Return the complete active YAML contract for the classifier job."""

    source = textwrap.dedent(classifier).strip()
    active_lines = [
        line.rstrip()
        for line in source.splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    return "\n".join(active_lines)


# Block comment delimiters, longest opener first so `<!--` is tried before any
# prefix of it could match.
BLOCK_COMMENTS = (("<!--", "-->"), ("<#", "#>"), ("/*", "*/"))


def strip_block_comments(lines: list[str]) -> list[str]:
    """Drop block comments written as block comments, and nothing else.

    A block comment is recognized only where one is actually written: the
    opener begins a line, and the closer ends that line or a later one.

    This used to be three DOTALL regexes over the whole text, which cannot tell
    a comment from a pair of shell globs. In ci.yml the `docs/*)` pattern in a
    `case` arm opened one and `hashFiles('**/Cargo.lock')` closed it, and 46,138
    characters, 48% of the file, vanished from every assertion built on the
    result; release-train.yml lost 24% the same way. No workflow contains a real
    block comment at all, so that stripping has only ever produced silent false
    negatives, and an assertion that cannot see the line it names passes exactly
    like one that checked it.
    """

    kept: list[str] = []
    closer: str | None = None
    for line in lines:
        stripped = line.strip()
        if closer is not None:
            if stripped.endswith(closer):
                closer = None
            continue
        consumed = False
        for opener, end in BLOCK_COMMENTS:
            if not stripped.startswith(opener):
                continue
            # A complete one-line comment closes itself; anything else opens a
            # span that the next line ending in the closer will end.
            if not (
                stripped.endswith(end) and len(stripped) >= len(opener) + len(end)
            ):
                closer = end
            consumed = True
            break
        # Blank lines pass through. `active_lines` drops them, and counting them
        # here would make every file look like it had lost a comment.
        if not consumed:
            kept.append(line)
    return kept


def active_lines(source: str) -> list[str]:
    """Return a block's non-blank, non-comment lines, stripped of indentation.

    Shell/YAML line comments, JavaScript line/block comments, PowerShell block
    comments, and Markdown/HTML comments all count. Install-proof steps embed
    JavaScript in `node <<'NODE'` heredocs and the Windows installer is
    PowerShell, so stripping only `#`/`//` still lets an entire validator or
    user-facing warning become a valid no-op block while satisfying a guard.
    """

    return [
        line.strip()
        for line in strip_block_comments(source.splitlines())
        if line.strip()
        and not line.strip().startswith("#")
        and not line.strip().startswith("//")
    ]


def install_proof_step(install_proof: str, name: str) -> str:
    """Return one install-proof step, from its name to the next step's name."""

    anchor = f"      - name: {name}\n"
    if install_proof.count(anchor) != 1:
        raise AssertionError(f"install proof must declare one step named {name}")
    start = install_proof.index(anchor)
    return install_proof[
        start : install_proof.index("\n      - name: ", start + len(anchor))
    ]


def replace_exactly_once(
    source: str, original: str, replacement: str, label: str
) -> str:
    """Build one source mutation without letting a stale probe become a no-op."""

    matches = source.count(original)
    if matches != 1:
        raise AssertionError(
            f"{label} mutation expected one source anchor, found {matches}: "
            f"{original!r}"
        )
    return source.replace(original, replacement, 1)


# The release suites skip every pull request, export pull requests included,
# and run on the merge queue's commit and on the push to main. The queue builds
# the exact commit that lands, so grading it there refuses a red change before
# main moves, and a pull request only pays for the light admission set.
RELEASE_SUITE_EVENT_FILTER = "github.event_name != 'pull_request'"


def workflow_job_blocks(workflow: str) -> dict[str, str]:
    """Return every job block from a workflow, preserving declaration order."""

    marker = "jobs:\n"
    if workflow.count(marker) != 1:
        raise AssertionError("workflow must contain exactly one jobs mapping")
    jobs = workflow.split(marker, 1)[1]
    top_level_job_lines = [
        line
        for line in jobs.splitlines()
        if line.startswith("  ")
        and not line.startswith(("   ", "\t"))
        and line.strip()
        and not line.lstrip().startswith("#")
    ]
    for line in top_level_job_lines:
        if re.fullmatch(r"  [A-Za-z0-9_-]+:[ \t]*", line) is None:
            raise AssertionError(
                "workflow job ids must use one canonical unquoted scalar line: "
                f"{line.strip()}"
            )
    matches = list(
        re.finditer(r"^  (?P<job>[A-Za-z0-9_-]+):[ \t]*$", jobs, re.MULTILINE)
    )
    if len(matches) != len(top_level_job_lines):
        raise AssertionError("workflow job census did not account for every job key")
    blocks: dict[str, str] = {}
    for index, match in enumerate(matches):
        job = match.group("job")
        if job in blocks:
            raise AssertionError(f"workflow declares duplicate job id: {job}")
        end = matches[index + 1].start() if index + 1 < len(matches) else len(jobs)
        blocks[job] = jobs[match.start() : end].rstrip()
    return blocks


def workflow_step_shell(job: str, name: str) -> str:
    """Return one named step's active shell body, dedented and comment-free.

    Unlike workflow_step_source this resolves a step that is the last one in
    its job, which is where the cargo-deny gate sits.
    """

    anchor = f"      - name: {name}\n"
    if job.count(anchor) != 1:
        raise AssertionError(f"job must declare exactly one step named {name}")
    lines = job[job.index(anchor) :].splitlines()
    try:
        run_block = lines.index("        run: |")
    except ValueError as error:
        raise AssertionError(
            f"step {name} must carry one literal shell run block"
        ) from error
    body: list[str] = []
    for line in lines[run_block + 1 :]:
        if not line.strip():
            continue
        if len(line) - len(line.lstrip()) <= 8:
            break
        body.append(line.rstrip())
    source = textwrap.dedent("\n".join(body))
    return "\n".join(
        line for line in source.splitlines() if not line.lstrip().startswith("#")
    )


def job_top_level_mapping_fields(job: str) -> list[tuple[str, str]]:
    """Return canonical job-level fields, rejecting YAML key aliases."""

    active_lines = classifier_active_job_source(job).splitlines()
    child_indents = [
        len(line) - len(line.lstrip()) for line in active_lines[1:] if line.strip()
    ]
    if not child_indents:
        return []
    if min(child_indents) != 2:
        raise AssertionError(
            "workflow job top-level fields must use canonical two-space "
            "child indentation"
        )

    fields: list[tuple[str, str]] = []
    for line in active_lines[1:]:
        indent = len(line) - len(line.lstrip())
        if indent != 2:
            continue
        match = re.fullmatch(
            r"  (?P<key>[A-Za-z0-9_-]+):(?:[ \t]*(?P<value>.*))?",
            line,
        )
        if match is None:
            raise AssertionError(
                "workflow job top-level keys must use canonical unquoted "
                f"`key:` syntax: {line.strip()}"
            )
        fields.append((match.group("key"), match.group("value") or ""))
    return fields


def optional_job_display_name(job: str) -> str | None:
    """Return a job's exact top-level display name, if it declares one."""

    names = [
        value.strip()
        for key, value in job_top_level_mapping_fields(job)
        if key == "name"
    ]
    if len(names) > 1:
        raise AssertionError(
            "workflow jobs may carry at most one one-line display name"
        )
    if names and (not names[0] or names[0] in {"|", "|-", ">", ">-"}):
        raise AssertionError(
            "workflow job display names must be non-empty one-line scalars"
        )
    return names[0] if names else None


def dynamic_job_context_source(job: str) -> str:
    """Return the name-bearing matrix contract for a dynamic job display name."""

    active_lines = classifier_active_job_source(job).splitlines()
    try:
        strategy_start = active_lines.index("  strategy:")
    except ValueError as error:
        raise AssertionError(
            "a matrix-derived workflow job name requires an explicit strategy"
        ) from error
    strategy_end = len(active_lines)
    for index in range(strategy_start + 1, len(active_lines)):
        line = active_lines[index]
        if len(line) - len(line.lstrip()) <= 2:
            strategy_end = index
            break
    return "\n".join(active_lines[strategy_start:strategy_end])


def assert_workflow_job_census(workflows: dict[Path, str]) -> None:
    """Pin every workflow job so no second required-context producer can appear."""

    actual: dict[str, dict[str, str | None]] = {}
    dynamic_contexts: dict[tuple[str, str], str] = {}
    for workflow, content in sorted(workflows.items()):
        path = workflow.relative_to(ROOT).as_posix()
        actual[path] = {}
        for job_id, block in workflow_job_blocks(content).items():
            display_name = optional_job_display_name(block)
            actual[path][job_id] = display_name
            if display_name is None or "${{" not in display_name:
                continue
            if "${{ matrix." not in display_name:
                raise AssertionError(
                    "dynamic workflow job display names must derive only from "
                    f"their reviewed matrix contract: {path}:{job_id}"
                )
            authority = (
                f"{display_name}\n{dynamic_job_context_source(block)}"
            ).encode()
            dynamic_contexts[(path, job_id)] = hashlib.sha256(authority).hexdigest()

    if actual != EXPECTED_WORKFLOW_JOB_DISPLAY_NAMES:
        raise AssertionError(
            "workflow-wide required-check producer census requires the exact "
            "reviewed workflow paths, job ids, and display names"
        )
    if dynamic_contexts != EXPECTED_DYNAMIC_JOB_CONTEXT_SHA256:
        raise AssertionError(
            "workflow-wide required-check producer census requires the exact "
            "reviewed matrix expansions for dynamic job display names"
        )

    actual_producers = {name: set() for name in REQUIRED_CHECK_JOB_PRODUCERS}
    reserved_expanded_names = set(REQUIRED_RELEASE_CHECKS) - set(
        REQUIRED_CHECK_JOB_PRODUCERS
    )
    for path, jobs in actual.items():
        for job_id, display_name in jobs.items():
            if display_name in actual_producers:
                actual_producers[display_name].add((path, job_id))
            if display_name in reserved_expanded_names:
                raise AssertionError(
                    "workflow job directly claims a release-required expanded "
                    f"context outside its reviewed producer: {path}:{job_id}: "
                    f"{display_name}"
                )

    if actual_producers != REQUIRED_CHECK_JOB_PRODUCERS:
        raise AssertionError(
            "workflow-wide release-required check producers do not match the "
            "reviewed workflow/job authority map"
        )


def workflow_structural_lines(content: str) -> list[tuple[int, int, str]]:
    """Return active YAML lines while excluding literal and folded bodies."""

    structural: list[tuple[int, int, str]] = []
    block_scalar_indent: int | None = None
    block_scalar = re.compile(
        r"^(?:-\s+)?[A-Za-z0-9_-]+:\s*[|>]"
        r"(?:[+-]?[1-9]?|[1-9][+-]?)?\s*(?:#.*)?$"
    )
    for line_number, line in enumerate(content.splitlines(), start=1):
        if block_scalar_indent is not None:
            if not line.strip():
                continue
            indent = len(line) - len(line.lstrip())
            if indent > block_scalar_indent:
                continue
            block_scalar_indent = None

        if not line.strip() or line.lstrip().startswith("#"):
            continue
        indentation = line[: len(line) - len(line.lstrip())]
        if "\t" in indentation:
            raise AssertionError(
                f"workflow YAML indentation must use spaces: line {line_number}"
            )
        stripped = line.lstrip()
        indent = len(indentation)
        structural.append((line_number, indent, stripped))
        if block_scalar.fullmatch(stripped):
            block_scalar_indent = indent
    return structural


def canonical_step_field(
    workflow: Path,
    line_number: int,
    source: str,
) -> tuple[str, str]:
    """Parse one canonical block-style workflow-step field."""

    match = re.fullmatch(
        r"(?P<key>[A-Za-z][A-Za-z0-9-]*):(?P<value>.*)",
        source,
    )
    if match is None:
        raise AssertionError(
            f"{workflow.name}:{line_number} workflow step fields must use "
            "canonical unquoted `key:` syntax in a canonical block mapping"
        )
    key = match.group("key")
    if key not in CANONICAL_STEP_FIELDS:
        raise AssertionError(
            f"{workflow.name}:{line_number} workflow step field is not in the "
            f"canonical Actions grammar: {key}"
        )
    return key, match.group("value").strip()


def canonical_workflow_steps(
    workflow: Path,
    content: str,
) -> list[
    tuple[
        dict[str, tuple[int, int, str]],
        list[tuple[int, int, str]],
    ]
]:
    """Inventory steps under a fail-closed, actionlint-compatible block grammar."""

    structural = workflow_structural_lines(content)
    steps: list[
        tuple[
            dict[str, tuple[int, int, str]],
            list[tuple[int, int, str]],
        ]
    ] = []

    for steps_index, (steps_line, steps_indent, stripped) in enumerate(structural):
        steps_match = re.fullmatch(r"steps:(?P<value>.*)", stripped)
        if steps_match is None:
            continue
        steps_value = steps_match.group("value").strip()
        if steps_value and not steps_value.startswith("#"):
            raise AssertionError(
                f"{workflow.name}:{steps_line} workflow steps must use a "
                "canonical block sequence"
            )
        end = len(structural)
        for index in range(steps_index + 1, len(structural)):
            if structural[index][1] <= steps_indent:
                end = index
                break
        children = structural[steps_index + 1 : end]
        if not children:
            raise AssertionError(
                f"{workflow.name} steps mapping must contain canonical step entries"
            )

        list_indent = steps_indent + 2
        field_indent = list_indent + 2
        fields: dict[str, tuple[int, int, str]] | None = None
        step_lines: list[tuple[int, int, str]] = []

        def finish_step() -> None:
            nonlocal fields, step_lines
            if fields is None:
                return
            execution_fields = {"run", "uses"}.intersection(fields)
            if len(execution_fields) != 1:
                raise AssertionError(
                    f"{workflow.name}:{step_lines[0][0]} canonical workflow "
                    "steps must contain exactly one run or uses field"
                )
            steps.append((fields, step_lines))
            fields = None
            step_lines = []

        for line_number, indent, child in children:
            if indent == list_indent:
                finish_step()
                if not child.startswith("- "):
                    raise AssertionError(
                        f"{workflow.name}:{line_number} workflow steps must use "
                        "a canonical block mapping for every sequence item"
                    )
                fields = {}
                step_lines = [(line_number, indent, child)]
                key, value = canonical_step_field(
                    workflow,
                    line_number,
                    child[2:],
                )
                fields[key] = (line_number, field_indent, value)
                continue

            if fields is None:
                raise AssertionError(
                    f"{workflow.name}:{line_number} workflow step entries must "
                    "start at canonical two-space child indentation"
                )
            step_lines.append((line_number, indent, child))
            if indent == field_indent:
                key, value = canonical_step_field(workflow, line_number, child)
                if key in fields:
                    raise AssertionError(
                        f"{workflow.name}:{line_number} workflow step contains "
                        f"a duplicate {key} field"
                    )
                fields[key] = (line_number, field_indent, value)
            elif indent < field_indent:
                raise AssertionError(
                    f"{workflow.name}:{line_number} workflow step fields must "
                    "use canonical two-space child indentation"
                )
        finish_step()
    return steps


def yaml_uses_scalar(workflow: Path, line_number: int, value: str) -> str:
    """Decode the direct scalar forms permitted for every action reference."""

    double_quoted = re.fullmatch(r'"([^"\r\n]*)"\s*(?:#.*)?', value)
    if double_quoted is not None:
        action = double_quoted.group(1)
        if "\\" in action:
            raise AssertionError(
                f"{workflow.name}:{line_number} workflow uses scalar must not "
                "contain YAML escape sequences"
            )
        return action

    single_quoted = re.fullmatch(r"'([^'\r\n]*)'\s*(?:#.*)?", value)
    if single_quoted is not None:
        return single_quoted.group(1)

    plain = re.fullmatch(r"""([^#\s'"\\]+)\s*(?:#.*)?""", value)
    if plain is None or plain.group(1).startswith(("*", "&", "!", "{", "[", "|", ">")):
        raise AssertionError(
            f"{workflow.name}:{line_number} workflow uses must be one direct "
            "action scalar without aliases, anchors, tags, or flow values"
        )
    return plain.group(1)


def canonical_job_action_uses(workflow: Path, content: str) -> list[str]:
    """Inventory reusable-workflow references alongside action-step references."""

    actions: list[str] = []
    for block in workflow_job_blocks(content).values():
        for key, value in job_top_level_mapping_fields(block):
            if key == "uses":
                actions.append(yaml_uses_scalar(workflow, 0, value))
    return actions


def canonical_child_mapping(
    workflow: Path,
    step_lines: list[tuple[int, int, str]],
    parent_line: int,
    parent_indent: int,
) -> dict[str, tuple[int, str]]:
    """Parse a step field's canonical two-space-indented child mapping."""

    fields: dict[str, tuple[int, str]] = {}
    expected_indent = parent_indent + 2
    for line_number, indent, source in step_lines:
        if line_number <= parent_line:
            continue
        if indent <= parent_indent:
            break
        if indent != expected_indent:
            raise AssertionError(
                f"{workflow.name}:{line_number} workflow with inputs must use "
                "canonical two-space child indentation"
            )
        match = re.fullmatch(
            r"(?P<key>[A-Za-z][A-Za-z0-9_-]*):(?P<value>.*)",
            source,
        )
        if match is None:
            raise AssertionError(
                f"{workflow.name}:{line_number} workflow with inputs must use "
                "canonical unquoted `key:` syntax"
            )
        key = match.group("key")
        if key in fields:
            raise AssertionError(
                f"{workflow.name}:{line_number} workflow with mapping contains "
                f"a duplicate {key} input"
            )
        fields[key] = (line_number, match.group("value").strip())
    return fields


def assert_rust_cache_steps(workflows: dict[Path, str]) -> None:
    rust_cache_uses = 0
    for workflow, content in sorted(workflows.items()):
        for fields, step_lines in canonical_workflow_steps(workflow, content):
            uses_field = fields.get("uses")
            if uses_field is None:
                continue
            uses_line, _, uses_value = uses_field
            action = yaml_uses_scalar(workflow, uses_line, uses_value)
            if RUST_CACHE_REFERENCE.search(action) is None:
                continue
            rust_cache_uses += 1
            if action != RUST_CACHE_ACTION:
                raise AssertionError(
                    f"{workflow.name} uses rust-cache at an unpinned ref"
                )

            with_field = fields.get("with")
            if with_field is None:
                raise AssertionError(
                    f"{workflow.name} rust-cache step must contain one canonical "
                    "with mapping and one main-only save-if"
                )
            with_line, with_indent, with_value = with_field
            if with_value and not with_value.startswith("#"):
                raise AssertionError(
                    f"{workflow.name} rust-cache with field must be a canonical "
                    "block mapping"
                )
            inputs = canonical_child_mapping(
                workflow,
                step_lines,
                with_line,
                with_indent,
            )
            save = inputs.get("save-if")
            if save is None:
                raise AssertionError(
                    f"{workflow.name} rust-cache step must contain one canonical "
                    "with mapping and one main-only save-if"
                )
            _, save_value = save
            save_value = save_value.split(" #", 1)[0].rstrip()
            if save_value not in CACHE_SAVE_VALUES:
                raise AssertionError(
                    f"{workflow.name} rust-cache save-if must be the exact "
                    "main-only scalar or the exact restore-only scalar"
                )

        for action in canonical_job_action_uses(workflow, content):
            if RUST_CACHE_REFERENCE.search(action) is not None:
                raise AssertionError(
                    f"{workflow.name} rust-cache use must be a canonical "
                    "workflow action step, not a reusable-workflow job"
                )

    if rust_cache_uses == 0:
        raise AssertionError("no pinned rust-cache steps found")


def workflow_step_source(workflow: str, content: str, step_anchor: str) -> str:
    """Extract one named workflow step so its internals can be judged."""

    if step_anchor not in content:
        raise AssertionError(
            f"{workflow} no longer carries the step this gate is anchored to: "
            f"{step_anchor.strip()}"
        )
    start = content.index(step_anchor)
    if "\n      - name:" not in content[start + 1 :]:
        raise AssertionError(
            f"{workflow} step {step_anchor.strip()} is no longer followed by "
            "another step, so its boundary cannot be resolved"
        )
    return content[start : content.index("\n      - name:", start + 1)]


def tag_readback_source(release_tag: str) -> str:
    """Extract the post-mint ref readback exactly as the workflow runs it."""

    anchor = "      - name: Verify tag ref and summarize\n"
    if anchor not in release_tag:
        raise AssertionError(
            "release-tag no longer carries the post-mint ref readback step"
        )
    start = release_tag.index(anchor)
    end = release_tag.index("\n      - name:", start + 1) if (
        "\n      - name:" in release_tag[start + 1 :]
    ) else len(release_tag)
    step = release_tag[start:end]
    marker = "        run: |\n"
    return textwrap.dedent(step[step.index(marker) + len(marker) :])


def execute_tag_readback(
    source: str,
    responses: list[tuple[int, str]],
    *,
    stranger_state: str = "pending",
    stranger_arms: str = "none",
    stranger_driver: str = "unrecorded",
    summary: list[str] | None = None,
) -> subprocess.CompletedProcess[str]:
    """Run the readback against a scripted sequence of API answers.

    The first-contact values are the ones the step's own `env:` block binds
    from the proof step's outputs, so the fixture has to bind them too: the
    step runs under `set -u` and an unbound one would die here for a reason
    production never has. `summary`, when given, collects the step summary the
    run wrote, which is where the mint records what its tag may claim.
    """

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        script = root / "readback.sh"
        script.write_text(source, encoding="utf-8")
        (root / "responses").write_text(
            "".join(f"{code} {body}\n" for code, body in responses),
            encoding="utf-8",
        )
        (root / "attempts").write_text("0", encoding="utf-8")
        binaries = root / "bin"
        binaries.mkdir()
        (binaries / "gh").write_text(
            "#!/usr/bin/env bash\n"
            'attempt="$(cat "$FIXTURE/attempts")"\n'
            'attempt=$((attempt + 1))\n'
            'printf %s "$attempt" > "$FIXTURE/attempts"\n'
            'line="$(sed -n "${attempt}p" "$FIXTURE/responses")"\n'
            '[ -n "$line" ] || line="$(tail -n 1 "$FIXTURE/responses")"\n'
            'code="${line%% *}"; body="${line#* }"\n'
            '[ "$code" = 0 ] || { echo "gh: Not Found (HTTP 404)" >&2; exit 1; }\n'
            'printf "%s\\n" "$body"\n',
            encoding="utf-8",
        )
        (binaries / "gh").chmod(0o755)
        # The exhaustion path sleeps ~30s in production; the fixture proves the
        # control flow, not the wall clock.
        (binaries / "sleep").write_text("#!/usr/bin/env bash\nexit 0\n", encoding="utf-8")
        (binaries / "sleep").chmod(0o755)
        environment = dict(os.environ)
        environment["PATH"] = f"{binaries}{os.pathsep}{environment['PATH']}"
        environment.update(
            {
                "FIXTURE": str(root),
                "GH_TOKEN": "fixture",
                "REPO": "kinlab-ai/kin",
                "ACTOR": "kin-release-bot[bot]",
                "TAG": "v9.9.9",
                "SHA": RELEASE_GATE_FIXTURE_SHA,
                "GITHUB_STEP_SUMMARY": str(root / "summary.md"),
                "RUNNER_TEMP": str(root),
            }
        )
        environment.update(
            {
                "STRANGER_STATE": stranger_state,
                "STRANGER_ARMS": stranger_arms,
                "STRANGER_DRIVER": stranger_driver,
            }
        )
        completed = subprocess.run(
            ["bash", str(script)],
            capture_output=True,
            text=True,
            env=environment,
            timeout=30,
            check=False,
        )
        if summary is not None:
            written = root / "summary.md"
            summary.append(
                written.read_text(encoding="utf-8") if written.exists() else ""
            )
        return completed


def job_step_active_lines(workflow: str, job: str, marker: str) -> list[str]:
    """Return the active lines of the step in `job` that contains `marker`."""

    block = workflow_job_blocks(workflow).get(job)
    if block is None:
        raise AssertionError(f"workflow no longer declares the {job} job")
    lines = block.splitlines()
    hits = [
        index
        for index, line in enumerate(lines)
        if marker in line and not line.lstrip().startswith("#")
    ]
    if len(hits) != 1:
        raise AssertionError(
            f"{job} must contain exactly one step matching {marker!r}, "
            f"found {len(hits)}"
        )
    start = hits[0]
    while start >= 0 and re.match(r"^\s*-\s", lines[start]) is None:
        start -= 1
    if start < 0:
        raise AssertionError(f"{job} step matching {marker!r} has no step start")
    step_indent = len(lines[start]) - len(lines[start].lstrip())
    active = [lines[start]]
    for line in lines[start + 1 :]:
        if not line.strip():
            continue
        if len(line) - len(line.lstrip()) <= step_indent:
            break
        if line.lstrip().startswith("#"):
            continue
        active.append(line)
    return active
