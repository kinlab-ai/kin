# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Checks on how the archives a release ships are built, and by whom.

rc-build.yml's build job is the only place a release archive is compiled, and
release.yml compiles nothing: it ships the RC Build bytes the preflight judged.
These pin the archive build profile, what the archives may contain, that
release.yml builds nothing, and the runner pools a release-critical workflow
may use, including the one paid pool RC Build's build job alone is admitted to.
"""

from __future__ import annotations

import hashlib
import re
from pathlib import Path

from .test_helpers import (
    EXPECTED_DYNAMIC_JOB_CONTEXT_SHA256,
    INSTALL_PROOF,
    RELEASE,
    RELEASE_RECOVERY,
    RELEASE_TAG,
    ROOT,
    WORKFLOWS,
    active_lines,
    dynamic_job_context_source,
    expect_assertion,
    job_top_level_mapping_fields,
    optional_job_display_name,
    replace_exactly_once,
    require,
    workflow_job_blocks,
)


RC_BUILD = WORKFLOWS / "rc-build.yml"

# A release tag freezes the workflow that its run will execute. Keep every
# runner selected by the tag mint, tagged release, recovery and public install
# proof, and by the pretag build apart from the one exception below, on the
# standard hosted pool, so a paid or custom pool exhausting its budget cannot
# strand an immutable release. The tagged release needs no other pool anyway:
# it builds nothing, so its macOS legs only sign and notarize, which takes
# about a minute on a standard runner. A deliberate new label has to change
# this reviewed allowlist, which is the explicit gate for expanding runner
# authority.
RELEASE_CRITICAL_STANDARD_RUNNERS = frozenset(
    {
        "ubuntu-latest",
        "ubuntu-24.04-arm",
        "macos-latest",
        "macos-15-intel",
        "windows-latest",
    }
)
# The one exception, and why it is safe. RC Build's build job runs before any
# tag exists, so a paid pool that stops assigning runners there delays a
# candidate and cannot strand a release: release.yml builds nothing that such a
# pool could hold up. Its two macOS rows build on the Depot Apple Silicon
# label, where each build step takes 12 to 21 minutes against 77 to 79 on the
# standard runners, and those bytes are the macOS bytes that ship once
# release.yml signs them. Its Windows row builds on the 16-core Depot Windows
# label, where the build step took 18.6 minutes against 39 to 49 on the
# standard 4-vCPU runner (18.3 on 32 cores, so 16 is the size), and those are
# the Windows bytes that ship unchanged. The labels are admitted for that one
# job and nowhere else: RC Build's image, capability and attest jobs, the tag
# mint, the tagged release, recovery and the public install proof stay on the
# standard pool.
PRETAG_PAID_RUNNERS = {
    (RC_BUILD, "build"): frozenset({"depot-macos-26", "depot-windows-2022-16"}),
}
RELEASE_CRITICAL_MATRIX_JOBS = frozenset(
    {
        (RC_BUILD, "build"),
        (RC_BUILD, "capability"),
        (RELEASE, "ship_rc_archives"),
        (RELEASE, "sign_macos"),
        (INSTALL_PROOF, "install-proof"),
    }
)
RELEASE_CRITICAL_REUSABLE_JOBS = {
    (RELEASE, "install_proof"): "./.github/workflows/install-proof.yml",
}


def assert_release_critical_runner_policy(
    workflow_sources: dict[Path, str],
    *,
    expected_matrix_hashes: dict[tuple[str, str], str] = (
        EXPECTED_DYNAMIC_JOB_CONTEXT_SHA256
    ),
) -> None:
    """Keep every immutable-release runner on the reviewed standard pool."""

    critical = (RC_BUILD, RELEASE_TAG, RELEASE, RELEASE_RECOVERY, INSTALL_PROOF)
    seen_matrix_jobs: set[tuple[Path, str]] = set()
    seen_reusable_jobs: set[tuple[Path, str]] = set()
    for workflow in critical:
        source = workflow_sources.get(workflow)
        if source is None:
            raise AssertionError(
                f"release-critical runner policy cannot read {workflow.name}"
            )
        for job_id, job in workflow_job_blocks(source).items():
            fields = job_top_level_mapping_fields(job)
            runs_on = [value.strip() for key, value in fields if key == "runs-on"]
            uses = [value.strip() for key, value in fields if key == "uses"]
            if runs_on and uses:
                raise AssertionError(
                    "release-critical runner policy found both runs-on and uses at "
                    f"{workflow.name}:{job_id}"
                )
            if uses:
                expected_callee = RELEASE_CRITICAL_REUSABLE_JOBS.get(
                    (workflow, job_id)
                )
                if uses != [expected_callee]:
                    raise AssertionError(
                        "release-critical runner policy rejects unreviewed reusable "
                        f"callee {uses} at {workflow.name}:{job_id}"
                    )
                seen_reusable_jobs.add((workflow, job_id))
                continue
            if not runs_on:
                raise AssertionError(
                    "release-critical runner policy found a job with neither "
                    f"runs-on nor a reusable workflow: {workflow.name}:{job_id}"
                )
            if len(runs_on) != 1:
                raise AssertionError(
                    "release-critical runner policy requires exactly one runs-on "
                    f"scalar for {workflow.name}:{job_id}; found {runs_on}"
                )
            runner = runs_on[0]
            if runner == "${{ matrix.os }}":
                matrix_key = (workflow, job_id)
                if matrix_key not in RELEASE_CRITICAL_MATRIX_JOBS:
                    raise AssertionError(
                        "release-critical runner policy rejects an unreviewed runner "
                        f"matrix at {workflow.name}:{job_id}"
                    )
                path = workflow.relative_to(ROOT).as_posix()
                context_key = (path, job_id)
                display_name = optional_job_display_name(job)
                expected_hash = expected_matrix_hashes.get(context_key)
                if display_name is None or expected_hash is None:
                    raise AssertionError(
                        "release-critical runner policy has no exact matrix contract "
                        f"for {workflow.name}:{job_id}"
                    )
                matrix_source = dynamic_job_context_source(job)
                actual_hash = hashlib.sha256(
                    f"{display_name}\n{matrix_source}".encode()
                ).hexdigest()
                if actual_hash != expected_hash:
                    raise AssertionError(
                        "release-critical runner policy requires the exact canonical "
                        f"matrix for {workflow.name}:{job_id}"
                    )
                matrix_runners = re.findall(
                    r"(?m)^\s+- os:\s*([^\s#]+)\s*$", matrix_source
                ) + re.findall(r'"os":"([^"]+)"', matrix_source)
                if not matrix_runners:
                    raise AssertionError(
                        "release-critical runner policy cannot resolve the runner "
                        f"matrix at {workflow.name}:{job_id}"
                    )
                admitted = RELEASE_CRITICAL_STANDARD_RUNNERS | PRETAG_PAID_RUNNERS.get(
                    matrix_key, frozenset()
                )
                rejected = [
                    label for label in matrix_runners if label not in admitted
                ]
                if rejected:
                    raise AssertionError(
                        "release-critical runner policy rejects paid, custom, or "
                        f"unreviewed matrix labels {rejected} at "
                        f"{workflow.name}:{job_id}"
                    )
                seen_matrix_jobs.add(matrix_key)
                continue
            if runner not in RELEASE_CRITICAL_STANDARD_RUNNERS:
                raise AssertionError(
                    "release-critical runner policy rejects paid, custom, or "
                    f"unreviewed label `{runner}` at {workflow.name}:{job_id}"
                )

    if seen_matrix_jobs != RELEASE_CRITICAL_MATRIX_JOBS:
        raise AssertionError(
            "release-critical runner policy matrix membership drifted: "
            f"expected {sorted((path.name, job) for path, job in RELEASE_CRITICAL_MATRIX_JOBS)}, "
            f"found {sorted((path.name, job) for path, job in seen_matrix_jobs)}"
        )
    if seen_reusable_jobs != set(RELEASE_CRITICAL_REUSABLE_JOBS):
        raise AssertionError(
            "release-critical runner policy reusable membership drifted: "
            f"expected {sorted((path.name, job) for path, job in RELEASE_CRITICAL_REUSABLE_JOBS)}, "
            f"found {sorted((path.name, job) for path, job in seen_reusable_jobs)}"
        )


def assert_retired_vfs_runtime_is_absent_from_new_archives(
    ci: str,
    release: str,
    rc_build: str,
    install_proof: str,
    archive_shape: str,
    archive_docs: str,
    workspace_manifest: str,
    cli_manifest: str,
) -> None:
    """Keep new archives free of the retired external VFS runtime.

    `kin-vfs-core` remains a local projection-engine crate used by the CLI. The
    external `kin-vfs` executable and preload shim are a different, deprecated
    delivery surface. Future release and RC archives must never build, copy, or
    attest that external pair, while install proof keeps a conditional reader
    for already-published archives that carry both files and their provenance.
    """

    for workflow, label in ((release, "release.yml"), (rc_build, "rc-build.yml")):
        for forbidden in (
            "repository: kinlab-ai/kin-vfs",
            "kin-vfs/target/",
            "VFS_TARGET",
            "SHIM_NAME",
            "vfs_target:",
            "shim_name:",
        ):
            if forbidden in workflow:
                raise AssertionError(
                    f"{label} must not build or package retired VFS runtime inputs: {forbidden}"
                )
        policies = (
            (
                "node scripts/write-release-archive-docs.mjs",
                "for required in kin kin-daemon README.md INSTALL.md checksums-sha256.txt; do",
                'require("./scripts/release-archive-shape.cjs")',
                "classifyReleaseArchiveRoot(artifact, { target })",
            )
            if label == "rc-build.yml"
            # release.yml packages only the signed macOS archives and writes
            # its manifests with the shared generator below.
            else (
                "node scripts/write-release-archive-docs.mjs",
                "for required in kin kin-daemon README.md INSTALL.md checksums-sha256.txt; do",
                'require("./scripts/release-archive-shape.cjs")',
                "node scripts/write-release-artifact-provenance.cjs",
            )
        )
        for policy in policies:
            require(workflow, policy, f"{label} new archive contract")
    generator = (ROOT / "scripts" / "write-release-artifact-provenance.cjs").read_text(
        encoding="utf-8"
    )
    for policy in (
        'require("./release-archive-shape.cjs")',
        "classifyReleaseArchiveRoot(args.contentRoot, { target })",
    ):
        require(generator, policy, "release manifest generator archive contract")
    if "kin-vfs" in generator or "VFS_TARGET" in generator:
        raise AssertionError(
            "the release manifest generator must not admit retired VFS runtime inputs"
        )

    for forbidden in ("kin-vfs", "libkin_vfs_shim"):
        if forbidden in archive_shape:
            raise AssertionError(
                "release archive shape must not allow the retired VFS runtime "
                f"artifact {forbidden}"
            )
    for policy in (
        'darwin: Object.freeze(["kin", "kin-daemon", ...DOC_FILES]),',
        'linux: Object.freeze(["kin", "kin-daemon", ...DOC_FILES]),',
        'windows: Object.freeze(["kin.exe", "kin-daemon.exe", ...DOC_FILES]),',
    ):
        require(archive_shape, policy, "release archive root payload")

    for policy in (
        "const retiredProjectionFiles = [vfs, shim].filter(hasRegularFile);",
        "release archive carries retired VFS runtime artifact(s):",
    ):
        require(archive_docs, policy, "release archive documentation generator")

    for policy in (
        '"legacy-vfs-mode.txt"',
        'fail(!names.has("kin-vfs"), "new archive unexpectedly carries kin-vfs");',
        "manifest.kin_vfs === undefined && platform.kin_vfs === undefined",
        "const legacyVfsSource = manifest.kin_vfs !== undefined || platform.kin_vfs !== undefined;",
        'fail(legacyVfs || process.env.RUNNER_OS === "Windows",',
    ):
        require(install_proof, policy, "legacy-aware VFS proof boundary")
    for forbidden in ("expected_vfs_commit:", "REVIEWED_VFS_COMMIT"):
        if forbidden in install_proof:
            raise AssertionError(
                "install proof must derive legacy VFS verification from the archived "
                f"provenance rather than a current release caller input: {forbidden}"
            )

    for forbidden in (
        "scripts/kin-vfs-pins.mjs",
        "scripts/kin-vfs-pins.test.mjs",
        "expected_vfs_commit: """,
    ):
        if forbidden in ci:
            raise AssertionError(
                "CI must not retain a current-release compatibility gate for retired "
                f"external VFS packaging: {forbidden}"
            )

    for policy in (
        '"crates/kin-vfs-core",',
        'kin-vfs-core = { path = "crates/kin-vfs-core",',
    ):
        require(workspace_manifest, policy, "local kin-vfs-core workspace engine")
    require(
        cli_manifest,
        "kin-vfs-core = { workspace = true }",
        "CLI projection-engine dependency",
    )


NATIVE_PROVENANCE = ROOT / "scripts" / "verify-native-release-provenance.mjs"
NATIVE_PROVENANCE_CALL = "node scripts/verify-native-release-provenance.mjs"


def assert_default_core_archive_profile(rc_build: str, provenance_script: str) -> None:
    """The one compile of a release archive uses Cargo defaults, and the shared
    provenance check RC Build runs on it attests the default feature profile."""

    build = workflow_job_blocks(rc_build)["build"]
    for line in active_lines(build):
        if "build --locked --release --target" in line and "-p kin-cli -p kin-daemon" in line:
            if "--features" in line or "--all-features" in line or "--no-default-features" in line:
                raise AssertionError("core archive builds must use Cargo defaults")
    if not any(NATIVE_PROVENANCE_CALL in line for line in active_lines(build)):
        raise AssertionError(
            "RC Build must run the shared native provenance check on the archives it builds"
        )
    for required in (
        'REQUIRED_FEATURES = Object.freeze(["vector", "external-embeddings"])',
        'FORBIDDEN_FEATURES = Object.freeze(["embeddings", "metal"])',
        'meta.embeddings?.local_inference_enabled !== false',
        'meta.embeddings?.external_embeddings_enabled !== true',
        'meta.embeddings?.provider !== "none"',
        "meta.kin_dirty !== false",
        "meta.dependency_provenance !== lockSha",
    ):
        if required not in provenance_script:
            raise AssertionError("default archive feature evidence is missing " + required)


# Anything on an active line of release.yml that compiles. `docker buildx
# imagetools` reads a registry and is not a build.
RELEASE_COMPILE_PATTERNS = (
    (r"(^|[\s;&|(])cargo\s", "runs cargo"),
    (r"(^|[\s;&|(])cross\s+build", "runs cross"),
    (r"(^|[\s;&|(])rustup\s", "runs rustup"),
    (r"docker\s+(buildx\s+)?build(\s|$)", "runs a Docker build"),
    (r"docker/build-push-action@", "uses docker/build-push-action"),
    (r"\./\.github/actions/rust-toolchain", "installs the Rust toolchain"),
)


def assert_release_builds_nothing(release: str) -> None:
    """release.yml compiles no archive and builds no image.

    It ships the archives and the daemon image RC Build built for the tagged
    commit, the bytes the preflight judged, which is why it needs no fast or
    paid pool and why no compile failure can strand a frozen tag.
    """

    for line in active_lines(release):
        for pattern, what in RELEASE_COMPILE_PATTERNS:
            if re.search(pattern, line):
                raise AssertionError(
                    f"release.yml {what}, but it must ship RC Build's bytes and build nothing: {line.strip()}"
                )
    for job, expected in (("ship_rc_archives", 1), ("sign_macos", 2)):
        body = workflow_job_blocks(release)[job]
        runs = [
            line
            for line in active_lines(body)
            if NATIVE_PROVENANCE_CALL in line or "--require-execution" in line
        ]
        if len(runs) != 2 * expected:
            raise AssertionError(
                f"release.yml's {job} must execute what it ships with the shared "
                f"provenance check and --require-execution, {expected} time(s)"
            )


MACOS_ARM64_PER_BINARY_LINK = (
    'cargo build --locked --release --target "$TARGET" -p kin-cli -p kin-daemon '
    '--bin "$bin" || { rc=$?; break; }'
)
COMBINED_CORE_ARCHIVE_BUILD = (
    'cargo build --locked --release --target "$TARGET" -p kin-cli -p kin-daemon || rc=$?'
)


def assert_macos_arm64_links_one_at_a_time(workflow: str) -> None:
    """The macOS arm64 leg links kin and kin-daemon in separate invocations.

    Both 0.8.2 release-candidate attempts reached the 90-minute bound on that
    leg with both fat-LTO links resident at once on a 7 GB runner. Each
    per-binary invocation keeps the combined -p pair, so Cargo resolves features
    exactly as the combined build does, and the combined invocation still runs
    afterwards, so the shipped bytes are the combined build's.
    """
    lines = active_lines(workflow_job_blocks(workflow)["build"])
    try:
        guard = lines.index('if [ "$TARGET" = aarch64-apple-darwin ]; then')
    except ValueError:
        raise AssertionError(
            "the macOS arm64 leg must link its binaries one at a time"
        ) from None
    if lines[guard + 1 : guard + 3] != [
        "for bin in kin kin-daemon; do",
        'echo "linking $bin on its own"',
    ] or lines[guard + 3] != MACOS_ARM64_PER_BINARY_LINK:
        raise AssertionError(
            "the macOS arm64 leg must link kin then kin-daemon, each with the "
            "combined -p kin-cli -p kin-daemon pair"
        )
    if COMBINED_CORE_ARCHIVE_BUILD not in lines[guard + 4 :]:
        raise AssertionError(
            "the combined core archive build must still run after the macOS "
            "arm64 per-binary links"
        )



def check_core_archive_build_profile(release: str, rc_build: str) -> None:
    script = NATIVE_PROVENANCE.read_text(encoding="utf-8")
    assert_default_core_archive_profile(rc_build, script)
    expect_assertion("local engine smuggled into a default archive", "must use Cargo defaults", lambda: assert_default_core_archive_profile(rc_build.replace('-p kin-cli -p kin-daemon || rc=$?', '-p kin-cli -p kin-daemon --features kin-cli/embeddings || rc=$?', 1), script))
    expect_assertion("engine metadata assertion removed", "feature evidence is missing", lambda: assert_default_core_archive_profile(rc_build, script.replace('meta.embeddings?.local_inference_enabled !== false', 'false', 1)))
    expect_assertion("RC Build stops running the shared provenance check", "shared native provenance check", lambda: assert_default_core_archive_profile(rc_build.replace(NATIVE_PROVENANCE_CALL, "node scripts/other.mjs", 1), script))
    assert_macos_arm64_links_one_at_a_time(rc_build)
    expect_assertion("macOS arm64 links both binaries at once", "one at a time", lambda: assert_macos_arm64_links_one_at_a_time(rc_build.replace('if [ "$TARGET" = aarch64-apple-darwin ]; then', 'if false; then', 1)))
    expect_assertion("macOS arm64 per-binary link drops the combined package pair", "combined -p kin-cli -p kin-daemon pair", lambda: assert_macos_arm64_links_one_at_a_time(rc_build.replace('-p kin-cli -p kin-daemon --bin "$bin"', '-p kin-cli --bin "$bin"', 1)))
    expect_assertion("macOS arm64 skips the combined build", "must still run", lambda: assert_macos_arm64_links_one_at_a_time(rc_build.replace("          if [ \"$rc\" -eq 0 ]; then\n            cargo build --locked --release --target \"$TARGET\" -p kin-cli -p kin-daemon || rc=$?\n          fi\n", "", 1)))

    assert_release_builds_nothing(release)
    for label, mutation, expected in (
        ("release.yml compiles an archive again", lambda text: text.replace("      - name: Download the RC archive\n", "      - name: Rebuild\n        run: cargo build --locked --release -p kin-cli -p kin-daemon\n\n      - name: Download the RC archive\n", 1), "runs cargo"),
        ("release.yml builds the daemon image again", lambda text: text.replace("      - name: Download the RC Build image\n", "      - name: Rebuild\n        run: docker buildx build --load .\n\n      - name: Download the RC Build image\n", 1), "runs a Docker build"),
        ("release.yml installs a Rust toolchain", lambda text: text.replace("      - name: Download the RC archive\n", "      - name: Install Rust stable\n        uses: ./.github/actions/rust-toolchain\n\n      - name: Download the RC archive\n", 1), "installs the Rust toolchain"),
        ("release.yml ships an archive it never executed", lambda text: text.replace("            --require-execution\n", "\n", 1), "must execute what it ships"),
    ):
        mutated = mutation(release)
        if mutated == release:
            raise AssertionError(f"falsifier anchor moved: {label}")
        expect_assertion(label, expected, lambda mutated=mutated: assert_release_builds_nothing(mutated))
    # Prose about a compile is not a compile.
    assert_release_builds_nothing(release.replace("jobs:\n", "jobs:\n  # cargo build used to run in this workflow.\n", 1))



def check_release_critical_runner_pools(workflow_sources: dict[Path, str]) -> None:
    assert_release_critical_runner_policy(workflow_sources)

    for label, workflow, original, mutation in (
        (
            "RC build returns to a paid runner",
            RC_BUILD,
            "          - os: ubuntu-latest\n            target: x86_64-unknown-linux-musl\n",
            "          - os: macos-15-large\n            target: x86_64-unknown-linux-musl\n",
        ),
        (
            "tag mint moves onto a custom runner",
            RELEASE_TAG,
            "    runs-on: ubuntu-latest\n",
            "    runs-on: kin-16core\n",
        ),
        (
            "tag mint moves onto the pretag Depot runner",
            RELEASE_TAG,
            "    runs-on: ubuntu-latest\n",
            "    runs-on: depot-macos-26\n",
        ),
        (
            "tagged release returns to a paid runner",
            RELEASE,
            "          - os: macos-15-intel\n",
            "          - os: macos-15-large\n",
        ),
        (
            "release recovery hides a runner behind a variable",
            RELEASE_RECOVERY,
            "    runs-on: ubuntu-latest\n",
            "    runs-on: ${{ vars.KIN_RELEASE_RUNNER || 'ubuntu-latest' }}\n",
        ),
        (
            "public install proof returns to a paid runner",
            INSTALL_PROOF,
            '{"os":"macos-15-intel","setup-shell":"zsh"}',
            '{"os":"macos-15-large","setup-shell":"zsh"}',
        ),
    ):
        runner_spoof = dict(workflow_sources)
        runner_spoof[workflow] = replace_exactly_once(
            runner_spoof[workflow], original, mutation, label
        )
        expect_assertion(
            label,
            "release-critical runner policy",
            lambda runner_spoof=runner_spoof: (
                assert_release_critical_runner_policy(runner_spoof)
            ),
        )

    # The canonical matrix hash and the standard-runner allowlist are separate
    # controls. Re-pin each paid matrix mutation to its new exact hash before
    # checking it, so these falsifiers reach the allowlist instead of stopping
    # at hash drift. Deleting the label-rejection branch must therefore make
    # this suite red even when a future reviewer deliberately updates a matrix
    # contract alongside its workflow.
    for label, workflow, job_id, original, mutation in (
        (
            "a repinned RC build matrix still rejects a paid runner",
            RC_BUILD,
            "build",
            "          - os: ubuntu-latest\n",
            "          - os: macos-15-large\n",
        ),
        (
            "a repinned RC capability matrix still rejects a paid runner",
            RC_BUILD,
            "capability",
            "          - os: macos-latest\n",
            "          - os: macos-15-large\n",
        ),
        (
            "a repinned tagged-release matrix still rejects a paid runner",
            RELEASE,
            "sign_macos",
            "          - os: macos-15-intel\n",
            "          - os: macos-15-large\n",
        ),
        (
            "a repinned tagged-release shipping matrix still rejects a paid runner",
            RELEASE,
            "ship_rc_archives",
            "          - os: windows-latest\n",
            "          - os: windows-latest-8-cores\n",
        ),
        (
            "a repinned install-proof matrix still rejects a paid runner",
            INSTALL_PROOF,
            "install-proof",
            '{"os":"macos-15-intel","setup-shell":"zsh"}',
            '{"os":"macos-15-large","setup-shell":"zsh"}',
        ),
        # The pretag Depot label is admitted for RC Build's build job alone.
        # Each of these moves it somewhere a tag depends on, or widens the
        # exception to a second paid label, and each must still be refused
        # after its matrix contract is deliberately repinned.
        (
            "a repinned tagged-release matrix still rejects the pretag Depot runner",
            RELEASE,
            "sign_macos",
            "          - os: macos-latest\n",
            "          - os: depot-macos-26\n",
        ),
        (
            "a repinned RC capability matrix still rejects the pretag Depot runner",
            RC_BUILD,
            "capability",
            "          - os: macos-15-intel\n",
            "          - os: depot-macos-26\n",
        ),
        (
            "a repinned install-proof matrix still rejects the pretag Depot runner",
            INSTALL_PROOF,
            "install-proof",
            '{"os":"macos-15-intel","setup-shell":"zsh"}',
            '{"os":"depot-macos-26","setup-shell":"zsh"}',
        ),
        (
            "a repinned RC build matrix still rejects a second Depot label",
            RC_BUILD,
            "build",
            "          - os: depot-macos-26\n            target: aarch64-apple-darwin\n",
            "          - os: depot-macos-15\n            target: aarch64-apple-darwin\n",
        ),
        (
            "a repinned RC capability matrix still rejects the pretag Depot Windows runner",
            RC_BUILD,
            "capability",
            "          - os: windows-latest\n",
            "          - os: depot-windows-2022-16\n",
        ),
        (
            "a repinned tagged-release shipping matrix still rejects the pretag Depot Windows runner",
            RELEASE,
            "ship_rc_archives",
            "          - os: windows-latest\n",
            "          - os: depot-windows-2022-16\n",
        ),
        (
            "a repinned install-proof matrix still rejects the pretag Depot Windows runner",
            INSTALL_PROOF,
            "install-proof",
            '{"os":"windows-latest","setup-shell":"powershell"}',
            '{"os":"depot-windows-2022-16","setup-shell":"powershell"}',
        ),
        (
            "a repinned RC build matrix still rejects a second Depot Windows size",
            RC_BUILD,
            "build",
            "          - os: depot-windows-2022-16\n",
            "          - os: depot-windows-2022-32\n",
        ),
    ):
        repinned_spoof = dict(workflow_sources)
        original_job = workflow_job_blocks(repinned_spoof[workflow])[job_id]
        mutated_job = replace_exactly_once(
            original_job, original, mutation, label
        )
        repinned_spoof[workflow] = replace_exactly_once(
            repinned_spoof[workflow], original_job, mutated_job, label
        )
        display_name = optional_job_display_name(mutated_job)
        if display_name is None:
            raise AssertionError(
                f"runner policy repinned falsification lost job name for {label}"
            )
        repinned_hashes = dict(EXPECTED_DYNAMIC_JOB_CONTEXT_SHA256)
        context_key = (workflow.relative_to(ROOT).as_posix(), job_id)
        repinned_hashes[context_key] = hashlib.sha256(
            f"{display_name}\n{dynamic_job_context_source(mutated_job)}".encode()
        ).hexdigest()
        expect_assertion(
            label,
            "unreviewed matrix labels",
            lambda repinned_spoof=repinned_spoof, repinned_hashes=repinned_hashes: (
                assert_release_critical_runner_policy(
                    repinned_spoof,
                    expected_matrix_hashes=repinned_hashes,
                )
            ),
        )

    release_build = workflow_job_blocks(workflow_sources[RELEASE])["sign_macos"]
    for label, original, mutation in (
        (
            "an inline YAML row adds a paid runner",
            "          - os: macos-latest\n",
            "          - { os: macos-15-large, target: x86_64-apple-darwin, "
            "artifact: paid-inline }\n"
            "          - os: macos-latest\n",
        ),
        (
            "a separate matrix axis adds a paid runner",
            "      matrix:\n        include:\n",
            "      matrix:\n        os: [macos-15-large]\n        include:\n",
        ),
    ):
        matrix_spoof = dict(workflow_sources)
        mutated_build = replace_exactly_once(
            release_build, original, mutation, label
        )
        matrix_spoof[RELEASE] = replace_exactly_once(
            matrix_spoof[RELEASE], release_build, mutated_build, label
        )
        expect_assertion(
            label,
            "exact canonical matrix",
            lambda matrix_spoof=matrix_spoof: (
                assert_release_critical_runner_policy(matrix_spoof)
            ),
        )

    reusable_spoof = dict(workflow_sources)
    reusable_spoof[RELEASE] = replace_exactly_once(
        reusable_spoof[RELEASE],
        "jobs:\n",
        "jobs:\n  paid-hop:\n"
        "    uses: kinlab-ai/kin-actions/.github/workflows/paid-release.yml@"
        "0123456789abcdef0123456789abcdef01234567\n",
        "an unknown reusable workflow can hide a paid runner",
    )
    expect_assertion(
        "an unknown reusable workflow can hide a paid runner",
        "rejects unreviewed reusable callee",
        lambda: assert_release_critical_runner_policy(reusable_spoof),
    )

    comment_control = dict(workflow_sources)
    commented_build = replace_exactly_once(
        release_build,
        "        include:\n",
        '        # diagnostic fixture only: {"os":"macos-15-large"}\n'
        "        include:\n",
        "runner policy comment control",
    )
    comment_control[RELEASE] = replace_exactly_once(
        comment_control[RELEASE],
        release_build,
        commented_build,
        "runner policy comment control",
    )
    assert_release_critical_runner_policy(comment_control)



def check_retired_vfs_runtime_absent_from_archives(
    ci_workflow: str,
    install_proof: str,
    release: str,
    workflow_sources: dict[Path, str],
) -> None:
    rc_build_workflow = workflow_sources[RC_BUILD]
    archive_shape = (ROOT / "scripts" / "release-archive-shape.cjs").read_text(
        encoding="utf-8"
    )
    archive_docs = (ROOT / "scripts" / "write-release-archive-docs.mjs").read_text(
        encoding="utf-8"
    )
    workspace_manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    cli_manifest = (ROOT / "crates" / "kin-cli" / "Cargo.toml").read_text(
        encoding="utf-8"
    )
    assert_retired_vfs_runtime_is_absent_from_new_archives(
        ci_workflow,
        release,
        rc_build_workflow,
        install_proof,
        archive_shape,
        archive_docs,
        workspace_manifest,
        cli_manifest,
    )
    for label, expected, inputs in (
        (
            "release packaging restores an external kin-vfs checkout",
            "must not build or package retired VFS runtime inputs",
            {"release": release + "\nrepository: kinlab-ai/kin-vfs\n"},
        ),
        (
            "RC packaging restores a VFS target input",
            "must not build or package retired VFS runtime inputs",
            {"rc_build": rc_build_workflow + "\nVFS_TARGET=legacy\n"},
        ),
        (
            "a new archive stops rejecting kin-vfs in its attested inventory",
            "legacy-aware VFS proof boundary",
            {
                "install_proof": install_proof.replace(
                    'fail(!names.has("kin-vfs"), "new archive unexpectedly carries kin-vfs");',
                    'fail(true, "new archive unexpectedly carries kin-vfs");',
                    1,
                )
            },
        ),
        (
            "legacy source provenance can omit a VFS pair on Unix",
            "legacy-aware VFS proof boundary",
            {
                "install_proof": install_proof.replace(
                    'fail(legacyVfs || process.env.RUNNER_OS === "Windows",',
                    'fail(legacyVfs || process.env.RUNNER_OS === "Linux",',
                    1,
                )
            },
        ),
        (
            "the archive shape readmits a VFS executable",
            "must not allow the retired VFS runtime artifact",
            {
                "archive_shape": archive_shape.replace(
                    'darwin: Object.freeze(["kin", "kin-daemon", ...DOC_FILES]),',
                    'darwin: Object.freeze(["kin", "kin-daemon", "kin-vfs", ...DOC_FILES]),',
                    1,
                )
            },
        ),
        (
            "the CLI drops its local projection engine",
            "CLI projection-engine dependency",
            {
                "cli_manifest": cli_manifest.replace(
                    "kin-vfs-core = { workspace = true }",
                    "# kin-vfs-core removed",
                    1,
                )
            },
        ),
    ):
        values = {
            "ci": ci_workflow,
            "release": release,
            "rc_build": rc_build_workflow,
            "install_proof": install_proof,
            "archive_shape": archive_shape,
            "archive_docs": archive_docs,
            "workspace_manifest": workspace_manifest,
            "cli_manifest": cli_manifest,
        }
        values.update(inputs)
        expect_assertion(
            label,
            expected,
            lambda values=values: assert_retired_vfs_runtime_is_absent_from_new_archives(
                values["ci"],
                values["release"],
                values["rc_build"],
                values["install_proof"],
                values["archive_shape"],
                values["archive_docs"],
                values["workspace_manifest"],
                values["cli_manifest"],
            ),
        )
