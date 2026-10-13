# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Checks that pin .github/workflows/release.yml, the tagged release."""

from __future__ import annotations

import os
import re
import subprocess
import tempfile

from .test_helpers import (
    ROOT,
    expect_assertion,
    require,
    workflow_job_blocks,
    workflow_step_shell,
)



def check_release_trigger_and_completion_seal(release: str) -> None:
    if "KIN_CI_BOT_TOKEN" in release or "bump_homebrew:" in release:
        raise AssertionError(
            "release.yml must not use a long-lived PAT or wait on cross-repo "
            "follow-ups before the completed-release callback can run"
        )
    if "workflow_dispatch:" in release:
        raise AssertionError(
            "release.yml must not expose branch-selectable workflow_dispatch"
        )
    require(release, 'tags:\n      - "v*.*.*"', "release trigger")
    require(release, "  publish_daemon_image:", "release daemon image job")
    require(release, "  attest_daemon_image:", "release daemon attestation job")
    for policy in (
        "  seal_release_completion:",
        "name: Seal completed stable release",
        "needs.promote_ghcr_latest.result == 'success'",
        "needs.publish_boundary_contracts.result == 'success'",
        "release-promotion.json.sha256",
        "release_workflow_run_id",
        "completed_capstones",
        "Attest terminal completion marker",
        "actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6",
        "Refuse mismatched existing completion assets",
        "Verify public terminal completion proof",
    ):
        require(release, policy, "terminal stable-release completion seal")



def check_release_image_and_publish_jobs(
    ci_workflow: str, release: str, update_trust: str
) -> tuple[str, str]:
    image_start = release.index("  publish_daemon_image:")
    image_end = release.index("\n  attest_daemon_image:", image_start)
    image_job = release[image_start:image_end]
    for policy in (
        "needs: [config, locate_rc_build]",
        "environment: release",
        "packages: write",
        "actions: read",
        "ghcr.io/kinlab-ai/kin",
        "name: kin-daemon-image",
        "run-id: ${{ needs.locate_rc_build.outputs.run_id }}",
        "scripts/locate-rc-build.mjs verify-artifact",
        "--artifact kin-daemon-image",
        "scripts/locate-rc-build.mjs verify-attestation",
        'jq -er .manifest_digest "$RUNNER_TEMP/rc/kin-daemon-image.json"',
        'skopeo copy --preserve-digests "oci-archive:${archive}" "docker://${source_image}"',
        'if [ "$digest" != "$EXPECTED_DIGEST" ]',
        "refusing to overwrite it",
        "verify-container-build-info.sh",
    ):
        require(image_job, policy, "release daemon image push job")

    for forbidden in (
        "id-token: write",
        "attestations: write",
        "actions/attest@",
        "docker buildx build",
        "docker push",
        "--cache-from",
    ):
        if forbidden in image_job:
            raise AssertionError(
                f"release daemon image push job builds or attests: {forbidden}"
            )

    attestation_start = release.index("  attest_daemon_image:")
    attestation_end = release.index("\n  ship_rc_archives:", attestation_start)
    attestation_job = release[attestation_start:attestation_end]
    for policy in (
        "needs: [config, publish_daemon_image]",
        "always()",
        "needs.config.result == 'success'",
        "needs.publish_daemon_image.result == 'success'",
        "environment: release",
        "packages: write",
        "id-token: write",
        "attestations: write",
        "EXPECTED_COMMIT: ${{ needs.publish_daemon_image.outputs.commit }}",
        "EXPECTED_DIGEST: ${{ needs.publish_daemon_image.outputs.digest }}",
        'git rev-parse "${GITHUB_REF_NAME}^{commit}"',
        'reference="${IMAGE}:${COMMIT}"',
        "docker buildx imagetools inspect",
        "^sha256:[0-9a-f]{64}$",
        'if [ "$digest" != "$EXPECTED_DIGEST" ]',
        '"${IMAGE}@${digest}" "$COMMIT"',
        "Attest immutable daemon image",
        "actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6",
        "subject-name: ghcr.io/kinlab-ai/kin",
        "subject-digest: ${{ steps.subject.outputs.digest }}",
        "push-to-registry: true",
        "create-storage-record: false",
        "Verify immutable daemon image attestation",
        '"oci://ghcr.io/kinlab-ai/kin@${DIGEST}"',
        "--bundle-from-oci",
        "--predicate-type https://slsa.dev/provenance/v1",
        '--signer-workflow "$GITHUB_REPOSITORY/.github/workflows/release.yml"',
        '--signer-digest "$COMMIT"',
        '--source-digest "$COMMIT"',
        '--source-ref "$GITHUB_REF"',
        "--deny-self-hosted-runners",
    ):
        require(attestation_job, policy, "release daemon image attestation job")

    if "docker buildx build" in attestation_job or re.search(
        r"(?m)^\s*docker\s+build\s", attestation_job
    ):
        raise AssertionError(
            "release daemon attestation job must not rebuild the image"
        )
    if "docker buildx build" in release:
        raise AssertionError(
            "release.yml must push RC Build's daemon image, never build one"
        )

    finalize_start = release.index("  finalize_release:")
    latest_promotion_start = release.index("  promote_ghcr_latest:")
    boundary_publish_start = release.index("  publish_boundary_contracts:")
    if not finalize_start < latest_promotion_start < boundary_publish_start:
        raise AssertionError(
            "GHCR latest promotion must be a separate job after release finalization"
        )
    latest_promotion_job = release[latest_promotion_start:boundary_publish_start]
    for policy in (
        "needs: [config, finalize_release, version_tag_image, publish_daemon_image, attest_daemon_image]",
        "always()",
        "needs.config.result == 'success'",
        "needs.finalize_release.result == 'success'",
        "needs.version_tag_image.result == 'success'",
        "needs.publish_daemon_image.result == 'success'",
        "needs.attest_daemon_image.result == 'success'",
        "needs.config.outputs.release_channel == 'latest'",
        "startsWith(github.ref, 'refs/tags/v')",
        "!contains(github.ref_name, '-')",
        "environment: release",
        "contents: read",
        "packages: write",
        "attestations: read",
        "persist-credentials: false",
        "fetch-depth: 0",
        "docker/login-action@dbcb813823bdd20940b903addbd779551569679f",
        "docker/setup-buildx-action@bb05f3f5519dd87d3ba754cc423b652a5edd6d2c",
        "EXPECTED_COMMIT: ${{ needs.publish_daemon_image.outputs.commit }}",
        "EXPECTED_SOURCE_DIGEST: ${{ needs.publish_daemon_image.outputs.digest }}",
        "is not an exact stable release tag",
        'if [ "$GITHUB_REF" != "refs/tags/${GITHUB_REF_NAME}" ]',
        'channel="$(node scripts/release-order.mjs channel "$VERSION")"',
        'git rev-parse "${GITHUB_REF_NAME}^{commit}"',
        'if [ "$COMMIT" != "$EXPECTED_COMMIT" ]',
        'SRC="${IMAGE}:${COMMIT}"',
        'VERSION_TAG="${IMAGE}:${VERSION}"',
        'V_VERSION_TAG="${IMAGE}:${GITHUB_REF_NAME}"',
        'LATEST="${IMAGE}:latest"',
        "inspect_digest()",
        "resolve_required_digest()",
        "wait_for_expected_digest()",
        "for attempt in $(seq 1 12)",
        "matching_reads=$((matching_reads + 1))",
        'if [ "$matching_reads" -eq 2 ]',
        "did not converge to $expected",
        "verify_stable_release_authority()",
        '"repos/${GITHUB_REPOSITORY}/releases/latest"',
        '"repos/${GITHUB_REPOSITORY}/releases/tags/${GITHUB_REF_NAME}"',
        "GitHub Latest moved to",
        "verify_immutable_authority()",
        "scripts/verify-container-build-info.sh",
        "verify_attestation()",
        '"oci://${IMAGE}@${digest}"',
        "--bundle-from-oci",
        "--predicate-type https://slsa.dev/provenance/v1",
        '--signer-workflow "$GITHUB_REPOSITORY/.github/workflows/release.yml"',
        '--signer-digest "$COMMIT"',
        '--source-digest "$COMMIT"',
        '--source-ref "$GITHUB_REF"',
        "--deny-self-hosted-runners",
        "initial_latest_state=missing",
        "mutation_required=true",
        'if [ "$initial_latest_digest" = "$EXPECTED_SOURCE_DIGEST" ]',
        "GHCR offers no atomic",
        'verify_immutable_authority "pre-write"',
        "prewrite_latest_state=missing",
        'if [ "$prewrite_latest_digest" = "$EXPECTED_SOURCE_DIGEST" ]',
        'action="Verified concurrent"',
        "changed from ${initial_latest_state}:${initial_latest_digest:-<missing>}",
        'prewrite_source_digest="$(resolve_required_digest "$SRC" "pre-write source recheck")"',
        "final_prewrite_latest_state=missing",
        'if [ "$final_prewrite_latest_digest" = "$EXPECTED_SOURCE_DIGEST" ]',
        "during final admission",
        "docker buildx imagetools create",
        "--prefer-index=false",
        '--tag "$LATEST"',
        '"${IMAGE}@${EXPECTED_SOURCE_DIGEST}"',
        'verify_immutable_authority "post-write"',
        'actual_latest_digest="$(wait_for_expected_digest',
        '"$LATEST" "$EXPECTED_SOURCE_DIGEST" "post-write latest")"',
        'scripts/verify-container-build-info.sh "$LATEST" "$COMMIT"',
        'verify_attestation "$actual_latest_digest"',
        '"$LATEST" "$EXPECTED_SOURCE_DIGEST" "final latest readback"',
    ):
        require(latest_promotion_job, policy, "stable GHCR latest promotion")

    if latest_promotion_job.count("docker buildx imagetools create") != 1:
        raise AssertionError(
            "stable GHCR latest promotion must contain exactly one registry mutation"
        )
    for forbidden in (
        "contents: write",
        "id-token: write",
        "attestations: write",
        "artifact-metadata: write",
        "actions/attest@",
        "docker buildx build",
        "docker push",
        "gh release edit",
        "continue-on-error",
        "!failure()",
        "!cancelled()",
        "|| true",
        "WIF_PROVIDER",
        "WIF_SERVICE_ACCOUNT",
        "us-central1-docker.pkg.dev",
    ):
        if forbidden in latest_promotion_job:
            raise AssertionError(
                "stable GHCR latest promotion contains forbidden authority or "
                f"fail-open behavior: {forbidden}"
            )

    publish_start = release.index("  publish:")
    publish_end = release.index("\n  install_proof:", publish_start)
    publish_job = release[publish_start:publish_end]
    for policy in (
        "needs: [config, locate_rc_build, ship_rc_archives, sign_macos, notarize_linux, attest_daemon_image]",
        "always()",
        "needs.config.result == 'success'",
        "needs.locate_rc_build.result == 'success'",
        "needs.ship_rc_archives.result == 'success'",
        "needs.sign_macos.result == 'success'",
        "needs.attest_daemon_image.result == 'success'",
        "needs.config.outputs.notarize_on_linux == 'true'",
        "needs.notarize_linux.result == 'success'",
        "needs.config.outputs.notarize_on_linux == 'false'",
        "needs.notarize_linux.result == 'skipped'",
    ):
        require(publish_job, policy, "first GitHub Release write admission")
    for forbidden in ("!failure()", "!cancelled()"):
        if forbidden in publish_job:
            raise AssertionError(
                "first GitHub Release write must use exact direct-needs results, "
                f"not permissive aggregate state: {forbidden}"
            )
    require(
        release,
        "needs: [publish, install_proof, publish_daemon_image, attest_daemon_image]",
        "version image daemon attestation gate",
    )
    require(
        release,
        "EXPECTED_SOURCE_DIGEST: ${{ needs.publish_daemon_image.outputs.digest }}",
        "release daemon image promotion",
    )

    # The jobs that ship archives no longer compile them. They read the tracked
    # Cargo.lock the provenance check hashes, so Windows conversion is still
    # turned off before checkout and the lockfile still checked before any byte
    # is judged.
    ship_job = workflow_job_blocks(release)["ship_rc_archives"]
    sign_job = workflow_job_blocks(release)["sign_macos"]
    windows_bytes = ship_job.index("      - name: Preserve tracked bytes on Windows")
    checkout = ship_job.index("      - name: Checkout\n")
    lock_assertion = ship_job.index("      - name: Verify canonical release input bytes")
    first_judgement = ship_job.index("      - name: Verify native release build provenance")
    if not windows_bytes < checkout < lock_assertion < first_judgement:
        raise AssertionError(
            "release shipping must disable Windows conversion before checkout and verify "
            "the tracked Kin lockfile before it judges a binary against it"
        )
    for job in (ship_job, sign_job):
        if "kin-vfs" in job:
            raise AssertionError(
                "release shipping must not reintroduce retired external VFS inputs"
            )
        for policy in (
            '["cat-file", "blob", "HEAD:Cargo.lock"]',
            "if (!working.equals(tracked))",
            "refusing release provenance",
            "node scripts/write-release-artifact-provenance.cjs",
            '--rc-manifest "$RUNNER_TEMP/rc/$ARTIFACT.provenance.json"',
            '--rc-run-id "$RC_RUN_ID"',
            '--tag "$GITHUB_REF_NAME"',
        ):
            require(job, policy, "cross-platform release lockfile and manifest authority")
    require(ship_job, "if: runner.os == 'Windows'", "cross-platform release lockfile authority")
    require(ship_job, "git config --global core.autocrlf false", "cross-platform release lockfile authority")
    build_job = ship_job + sign_job

    generator = (ROOT / "scripts" / "write-release-artifact-provenance.cjs").read_text(
        encoding="utf-8"
    )
    for policy in (
        'require("./read-update-build-identity.cjs")',
        "record.build_identity = readUpdateBuildIdentity(file)",
        "schema_version: 2",
        "static build identity does not match the tagged release source",
        "CLI and daemon graph snapshot identities disagree",
        "inventories other component bytes",
        "records another static build identity for",
        "if (!signedTarget) {",
    ):
        require(generator, policy, "static release build identity generation")

    # The archive that carries the release and the manifest that describes it are
    # produced by one job and judged by another. Both must decide what a release
    # archive is allowed to contain from the same rules, or a shape one side
    # emits is a shape the other refuses at publish time.
    for policy in (
        'require("./release-archive-shape.cjs")',
        "classifyReleaseArchiveRoot(args.contentRoot, { target })",
    ):
        require(generator, policy, "sanctioned release archive shape at packaging")
    for policy in (
        'require("./scripts/release-archive-shape.cjs")',
        "assertReleaseArchiveMemberPaths(",
        "classifyReleaseArchiveRoot(contentRoot, {",
    ):
        require(publish_job, policy, "sanctioned release archive shape at publish")
    # The inventory is what an installed updater judges the release by, with its
    # own frozen component list, and it refuses any other name. Every manifest
    # from v0.5.45 to v0.6.3 named the three documentation members, and v0.6.2
    # refused v0.6.3 on checksums-sha256.txt, the first of them. The classifier
    # now keeps documentation out of `files`, and the publish job must both read
    # the doc list from the shared module and refuse a manifest that names one by
    # that defect's name rather than as a generic inventory mismatch.
    for policy in (
        "DOC_FILES,",
        "names documentation the installed updater refuses",
        "does not carry every documentation member",
    ):
        require(publish_job, policy, "documentation kept out of the provenance inventory")
    for forbidden in (
        "entries.every((entry) => entry.isFile())",
        ".filter((entry) => entry.isFile())",
    ):
        if forbidden in publish_job or forbidden in build_job:
            raise AssertionError(
                "release archive shape must be judged by the shared classifier, not by "
                f"an inline entry-type filter: {forbidden}"
            )
    member_path_check = publish_job.index("assertReleaseArchiveMemberPaths(")
    archive_extraction = publish_job.index("fs.mkdtempSync(path.join(os.tmpdir()")
    if member_path_check >= archive_extraction:
        raise AssertionError(
            "release archive member paths must be judged before extraction, because a "
            "member that escapes the extraction root has already been written by then"
        )
    if 'tar tzf "${ARTIFACT}.tar.gz" | grep' in build_job:
        raise AssertionError(
            "release packaging must read the archive listing from a file: piping it into "
            "grep lets an early grep exit fail the pipeline under pipefail, which is "
            "indistinguishable from the member being absent"
        )
    require(
        ci_workflow,
        "./scripts/release-archive-shape.test.cjs",
        "release archive shape regression",
    )

    # The release build decides which files may sit at an archive root; the
    # updater decides which names it is willing to stage. `kin update` aborts a
    # whole staging on the first name it does not manage, so a name sanctioned
    # here that the updater has never heard of publishes cleanly and then stops
    # every update on that platform until a later release replaces the archive.
    # Pin the two sets against each other instead of trusting them to be edited
    # together. Only one direction is required: the updater deliberately manages
    # names the release no longer ships, so that it can delete a stale copy.
    archive_shape = (ROOT / "scripts" / "release-archive-shape.cjs").read_text(
        encoding="utf-8"
    )
    for policy in (
        "holds unexpected file",
        "declares unexpected file",
        "has no component list for target",
    ):
        require(archive_shape, policy, "sanctioned release archive root file names")
    updater = (ROOT / "crates" / "kin-cli" / "src" / "commands" / "update.rs").read_text(
        encoding="utf-8"
    )

    def between(content: str, start: str, end: str, label: str) -> str:
        try:
            begin = content.index(start) + len(start)
            return content[begin : content.index(end, begin)]
        except ValueError as error:
            raise AssertionError(
                f"cannot read {label}: anchor moved, so this check would compare "
                "two empty sets and pass without judging anything"
            ) from error

    for family, spec, cli in (
        ("darwin", "MACOS_COMPONENTS", "kin"),
        ("linux", "LINUX_COMPONENTS", "kin"),
        ("windows", "WINDOWS_COMPONENTS", "kin.exe"),
    ):
        sanctioned = set(
            re.findall(
                r'"([^"]+)"',
                between(
                    archive_shape,
                    f"  {family}: Object.freeze([",
                    "]),",
                    f"the {family} release archive root file names",
                ),
            )
        )
        managed = set(
            re.findall(
                r'name: "([^"]+)"',
                between(
                    updater,
                    f"const {spec}: &[ComponentSpec] = &[",
                    "\n];",
                    f"the updater's {spec} list",
                ),
            )
        )
        # Both extractions have to have found the CLI, or an anchor drifted and
        # the subset below is comparing nothing against nothing.
        for names, source in ((sanctioned, family), (managed, spec)):
            if cli not in names:
                raise AssertionError(
                    f"{source} component names do not include {cli}, so the release "
                    "archive and updater component sets were not actually read"
                )
        unmanaged = sorted(sanctioned - managed)
        if unmanaged:
            raise AssertionError(
                f"release archive root files {unmanaged} are sanctioned for {family} "
                f"but are not in the updater's {spec}, so an archive carrying them "
                "would publish and then abort every update on that platform"
            )
    archive_attestation_start = publish_job.index(
        "      - name: Attest final release archives and provenance"
    )
    release_creation_start = publish_job.index(
        "      - name: Create GitHub Release", archive_attestation_start
    )
    if archive_attestation_start >= release_creation_start:
        raise AssertionError(
            "final release archives must be attested before the first GitHub Release write"
        )
    archive_attestation_step = publish_job[
        archive_attestation_start:release_creation_start
    ]
    for policy in (
        "actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6",
        "subject-path: |",
        "kin-linux-x86_64.tar.gz",
        "kin-linux-aarch64.tar.gz",
        "kin-macos-x86_64.tar.gz",
        "kin-macos-aarch64.tar.gz",
        "kin-windows-x86_64.zip",
        "kin-windows-x86_64.tar.gz",
        "release-provenance.json",
        "Fail closed",
    ):
        require(
            archive_attestation_step,
            policy,
            "final platform-archive attestation authority",
        )
    for policy in (
        "--expect-version",
        "--expect-sha",
        "--expect-archive-sha256",
        "gh attestation verify",
        "kinlab-ai/kin/.github/workflows/release.yml",
        "sourceRepositoryDigest",
        "opens the install lock or performs local mutation",
        "selection and drift evidence only",
        "does not authenticate archive bytes",
    ):
        require(update_trust, policy, "pinned updater external byte-authority contract")
    parser = (ROOT / "scripts" / "read-update-build-identity.cjs").read_text(
        encoding="utf-8"
    )
    for policy in (
        "MAX_COMPONENT_BYTES = 256 * 1024 * 1024",
        "bytes.length > MAX_COMPONENT_BYTES",
        "expected exactly one",
        "static build identity end marker is invalid",
        "graph snapshot version must be nonzero",
    ):
        require(parser, policy, "bounded static build identity parser")

    aggregate_start = publish_job.index(
        "      - name: Aggregate per-artifact checksums"
    )
    aggregate_end = publish_job.index(
        "      - name: Verify complete release asset inventory", aggregate_start
    )
    aggregate_step = publish_job[aggregate_start:aggregate_end]
    for policy in (
        "set -euo pipefail",
        "tr -d '\\r' < \"$f\" >> checksums-sha256.txt",
        "grep -q $'\\r' checksums-sha256.txt",
        "aggregate checksum file still contains carriage returns",
        # A published sidecar with a CR cannot be verified by sha256sum -c, so
        # the publisher refuses it rather than cleaning up the aggregate alone.
        "if LC_ALL=C grep -q $'\\r' \"$f\"; then",
        "contains a carriage return, so sha256sum -c cannot verify it",
    ):
        require(aggregate_step, policy, "cross-platform checksum aggregate")
    inventory_start = aggregate_end
    inventory_end = publish_job.index("\n      - ", inventory_start + 1)
    inventory_step = publish_job[inventory_start:inventory_end]
    for policy in (
        'sha256sum -c "$asset.sha256"',
        "printf '%s  %s\\n' \"$(sha256sum \"$asset\" | cut -d' ' -f1)\" \"$asset\"",
        '| cmp -s - "$asset.sha256"',
    ):
        require(inventory_step, policy, "LF checksum sidecar inventory")
    if 'cat "$f" >> checksums-sha256.txt' in aggregate_step:
        raise AssertionError(
            "release checksum aggregation must not preserve Windows CRLF bytes"
        )
    return attestation_job, image_job



def check_npm_release_path_and_action_pins(
    attestation_job: str, ci_workflow: str, image_job: str, release: str
) -> None:
    for obsolete in (
        ROOT / "scripts" / "promote-npm-release.sh",
        ROOT / "scripts" / "test-promote-npm-release.py",
        ROOT / "scripts" / "stage-npm-release.sh",
        ROOT / "scripts" / "wait-npm-approval.sh",
        ROOT / "scripts" / "test-npm-staged-release.py",
    ):
        if obsolete.exists():
            raise AssertionError(
                f"obsolete npm release helper still exists: {obsolete}"
            )

    preflight_start = release.index("  npm_publish_preflight:")
    preflight_end = release.index("\n  publish_npm_compatibility:", preflight_start)
    npm_preflight = release[preflight_start:preflight_end]
    # Read-only, so it runs beside the release; both publishers still wait for
    # it and for the GitHub Release and the public install proof below.
    for policy in (
        "needs: config",
        "npm test --prefix ./packages/kin",
        "npm test --prefix ./packages/kin-mcp",
        "for package_dir in ./packages/kin ./packages/kin-mcp",
        "bash scripts/publish-npm-release.sh --preflight",
        '"$package_dir" "$GITHUB_REF" "$(git rev-parse HEAD)"',
        "npm@11.15.0",
    ):
        require(npm_preflight, policy, "two-package npm preflight")
    for forbidden in ("environment: release", "id-token: write", "npm publish"):
        if forbidden in npm_preflight:
            raise AssertionError(
                f"npm preflight has mutation authority before both packages pass: {forbidden}"
            )

    for job, next_job, package_dir in (
        ("publish_npm_compatibility", "publish_npm_canonical", "./packages/kin-mcp"),
        ("publish_npm_canonical", "verify_npm_published", "./packages/kin"),
    ):
        start = release.index(f"  {job}:")
        end = release.index(f"\n  {next_job}:", start + 3)
        publish_job = release[start:end]
        for policy in (
            "needs: [publish, install_proof, npm_publish_preflight]",
            "needs.publish.result == 'success'",
            "needs.install_proof.result == 'success'",
            "needs.npm_publish_preflight.result == 'success'",
            "environment: release",
            "id-token: write",
            "npm@11.15.0",
            "scripts/publish-npm-release.sh",
            package_dir,
        ):
            require(publish_job, policy, f"{job} trusted publishing")

    install_proof_start = release.index("  install_proof:")
    install_proof_end = release.index("\n  npm_publish_preflight:", install_proof_start)
    install_proof_job = release[install_proof_start:install_proof_end]
    for policy in (
        "needs: publish",
        "always()",
        "needs.publish.result == 'success'",
        "uses: ./.github/workflows/install-proof.yml",
    ):
        require(install_proof_job, policy, "mandatory public install proof")

    exact_result_gates = {
        "verify_npm_published": (
            "needs.publish_npm_canonical.result == 'success'",
            "needs.publish_npm_compatibility.result == 'success'",
            "needs.version_tag_image.result == 'success'",
        ),
        "smoke_npm_published": ("needs.verify_npm_published.result == 'success'",),
        "finalize_release": (
            "needs.publish.result == 'success'",
            "needs.install_proof.result == 'success'",
            "needs.version_tag_image.result == 'success'",
            "needs.smoke_npm_published.result == 'success'",
        ),
        "promote_ghcr_latest": (
            "needs.config.result == 'success'",
            "needs.finalize_release.result == 'success'",
            "needs.version_tag_image.result == 'success'",
            "needs.publish_daemon_image.result == 'success'",
            "needs.attest_daemon_image.result == 'success'",
        ),
        "publish_boundary_contracts": (
            "needs.publish.result == 'success'",
            "needs.install_proof.result == 'success'",
            "needs.finalize_release.result == 'success'",
        ),
        "version_tag_image": (
            "needs.publish.result == 'success'",
            "needs.install_proof.result == 'success'",
            "needs.publish_daemon_image.result == 'success'",
            "needs.attest_daemon_image.result == 'success'",
        ),
    }
    for job, policies in exact_result_gates.items():
        start = release.index(f"  {job}:")
        match = re.search(r"(?m)^  [a-zA-Z0-9_]+:\s*$", release[start + 3 :])
        end = start + 3 + match.start() if match else len(release)
        job_body = release[start:end]
        require(job_body, "always()", f"{job} exact result admission")
        for policy in policies:
            require(job_body, policy, f"{job} exact result admission")

    for policy in (
        "needs: [publish_npm_canonical, publish_npm_compatibility, version_tag_image]",
        "scripts/verify-npm-release.sh",
        "needs: verify_npm_published",
        '"${PACKAGE}@${VERSION}"',
        "needs: [publish, install_proof, version_tag_image, smoke_npm_published]",
        "GitHub Latest remains blocked",
        "published automatically through protected npm OIDC",
        "anonymous byte, provenance, and install proof",
    ):
        require(release, policy, "two-package automatic npm release gate")

    publisher = (ROOT / "scripts" / "publish-npm-release.sh").read_text(
        encoding="utf-8"
    )
    for policy in (
        'npm publish "$tarball" --access public --tag "$channel"',
        "--provenance",
        "exact bytes, final channel, and provenance verified before skipping publication",
        "short-lived OIDC credential only",
        "anonymous public authority",
        "--preflight] <package-dir>",
        'bash "$verify_script"',
        'node "$release_order_script" npm-channel "$package" "$channel"',
        "require_exact_channel",
        "assert-not-rollback",
        "integrity=$integrity",
        "immutable version cannot be rolled back",
        "rerun this same release",
    ):
        require(publisher, policy, "OIDC npm publisher")

    verifier = (ROOT / "scripts" / "verify-npm-release.sh").read_text(encoding="utf-8")
    for policy in (
        'if remote_integrity="$(printf \'%s\\n\' "$remote_dist" | node -e',
        'typeof dist.integrity !== "string"',
        "complete dist metadata with integrity",
    ):
        require(verifier, policy, "retryable npm dist metadata verifier")
    require(
        ci_workflow,
        "python3 ./scripts/test-verify-npm-release.py",
        "partial npm metadata failure-injection regression",
    )
    for helper_name, helper in (
        ("OIDC npm publisher", publisher),
        ("anonymous npm provenance verifier", verifier),
    ):
        require(
            helper,
            "env -u NODE_AUTH_TOKEN -u NPM_TOKEN",
            f"{helper_name} credential scrubbing",
        )

    public_npm_path = "\n".join((release, publisher, verifier))
    if re.search(r"npm stage(?:\s|$)", public_npm_path):
        raise AssertionError(
            "automatic npm release path must not retain staged publishing authority"
        )
    if re.search(r"(?m)^\s*(?:run:\s*)?npm publish(?:\s|$)", release):
        raise AssertionError(
            "release workflow must delegate npm mutation to the audited helper"
        )

    for live_contract in (
        "Both public npm packages trust only kinlab-ai/kin + release.yml + environment",
        "allow `npm publish` through short-lived OIDC, and disallow",
        "publish_npm_canonical",
        "publish_npm_compatibility",
    ):
        require(release, live_contract, "live automatic npm trust contract")

    for forbidden in (
        "secrets.NPM_TOKEN",
        "release-candidate-",
        "npm dist-tag",
        "promote_npm",
        "promote-npm-release.sh",
        "stage_npm_",
        "wait_for_npm_approval",
    ):
        if forbidden in release or forbidden in publisher:
            raise AssertionError(
                f"automatic npm release path contains retired authority: {forbidden}"
            )
    for forbidden in (
        "workflow_dispatch",
        "branches: [main]",
        "WIF_PROVIDER",
        "WIF_SERVICE_ACCOUNT",
        "us-central1-docker.pkg.dev",
        ":latest",
    ):
        if forbidden in image_job or forbidden in attestation_job:
            raise AssertionError(
                f"release daemon image path contains forbidden authority: {forbidden}"
            )

    unpinned: list[str] = []
    for line_number, line in enumerate(release.splitlines(), start=1):
        match = re.match(r"\s*(?:-\s*)?uses:\s*([^\s]+)", line)
        if match is None:
            continue
        action = match.group(1)
        if action.startswith("./"):
            continue
        _, separator, ref = action.rpartition("@")
        if not separator or re.fullmatch(r"[0-9a-f]{40}", ref) is None:
            unpinned.append(f"line {line_number}: {action}")
    if unpinned:
        raise AssertionError(
            "release.yml has moving third-party action refs: " + ", ".join(unpinned)
        )


# The fixture suites for the scripts release.yml and rc-build.yml run to move
# bytes from RC Build to a release. ci.yml's release policy step runs this
# authority suite, so running them here is what makes a pull request go red on
# them; ci.yml's own node --test list may name them as well.
RC_BUILD_CONTRACT_SUITES = (
    "scripts/locate-rc-build.test.mjs",
    "scripts/verify-native-release-provenance.test.mjs",
    "scripts/compare-signed-macos-archive.test.mjs",
    "scripts/write-release-artifact-provenance.test.cjs",
    "scripts/rc-daemon-image.test.mjs",
    "scripts/check-rc-build-drift.test.mjs",
)


def assert_release_consumes_rc_build(release: str, locator: str) -> None:
    """release.yml takes every byte from the one RC Build run it located.

    The bytes the preflight judged are the bytes that ship: the Linux and
    Windows archives byte for byte, the macOS archives signed and otherwise
    identical, and the daemon image with its digest preserved. Every consumer
    proves its own bytes against that run before using them.
    """

    jobs = workflow_job_blocks(release)
    locate = jobs.get("locate_rc_build")
    if locate is None:
        raise AssertionError("release.yml no longer locates the RC Build it ships")
    for policy in (
        "needs: duplicate_guard",
        "if: needs.duplicate_guard.outputs.duplicate != 'true'",
        "actions: read",
        'git rev-parse "${GITHUB_REF_NAME}^{commit}"',
        "node scripts/locate-rc-build.mjs select",
        '--sha "$COMMIT"',
        '--github-output "$GITHUB_OUTPUT"',
    ):
        require(locate, policy, "RC Build location")
    for forbidden in ("environment:", "secrets.", "id-token: write", "download-artifact"):
        if forbidden in locate:
            raise AssertionError(f"locating the RC Build needs no {forbidden}")

    located_run = "run-id: ${{ needs.locate_rc_build.outputs.run_id }}"
    for job_id in ("ship_rc_archives", "sign_macos", "publish_daemon_image"):
        job = jobs[job_id]
        for policy in (
            "needs: [config, locate_rc_build]",
            "actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c",
            located_run,
            "github-token: ${{ github.token }}",
            "node scripts/locate-rc-build.mjs verify-artifact",
            '--run-id "$RC_RUN_ID"',
            '--preflight "$RC_PREFLIGHT"',
            "node scripts/locate-rc-build.mjs verify-attestation",
            '--source-ref "refs/heads/${RC_HEAD_BRANCH}"',
            'test "$(git rev-parse HEAD)" = "$RELEASE_COMMIT"',
        ):
            require(job, policy, f"{job_id} RC Build consumption")
        download = job.index("actions/download-artifact@")
        verified = job.index("node scripts/locate-rc-build.mjs verify-artifact")
        attested = job.index("node scripts/locate-rc-build.mjs verify-attestation")
        if not download < verified < attested:
            raise AssertionError(
                f"{job_id} must verify the downloaded bytes and their attestation before using them"
            )

    ship = jobs["ship_rc_archives"]
    for policy in (
        'cmp "$RUNNER_TEMP/rc/$name" "$RUNNER_TEMP/ship/$name"',
        "--require-execution",
    ):
        require(ship, policy, "unchanged Linux and Windows archives")
    for forbidden in ("environment:", "secrets.", "tar czf", "Compress-Archive"):
        if forbidden in ship:
            raise AssertionError(f"shipping an unsigned RC archive must not involve {forbidden}")

    sign = jobs["sign_macos"]
    require(sign, "environment: release", "macOS signing credentials stay in the release environment")
    order = [
        "      - name: Verify the archive is the one the preflight judged and RC Build attested",
        "      - name: Verify native release build provenance of the RC binaries",
        "      - name: Sign macOS binaries",
        "      - name: Notarize macOS binaries (notarytool)",
        "      - name: Package (Unix)",
        "      - name: Prove the signed archive differs from the RC archive only by its signatures",
        "      - name: Verify native release build provenance of the signed binaries",
        "      - name: Generate artifact provenance manifest",
        "      - name: Upload artifact",
    ]
    positions = [sign.index(marker) for marker in order]
    if positions != sorted(positions):
        raise AssertionError(
            "macOS signing must verify the RC bytes, sign, notarize, package, prove the "
            "result differs only by signature and execute it, in that order"
        )
    require(sign, "node scripts/compare-signed-macos-archive.mjs", "signature-only difference proof")

    publish = jobs["publish"]
    binding = publish.index("      - name: Verify the shipped archives against the RC Build")
    attest = publish.index("      - name: Attest final release archives and provenance")
    created = publish.index("      - name: Create GitHub Release")
    if not binding < attest < created:
        raise AssertionError(
            "the shipped archives must be bound to the RC Build before they are attested or published"
        )
    for policy in (
        "pattern: kin-*",
        'rc.workflow === ".github/workflows/rc-build.yml"',
        "String(rc.run_id) === process.env.RC_RUN_ID",
        "shipped.sha256 === built.sha256",
        "const judgedName = `${artifact}.tar.gz`;",
        "[rc.archive, ...(rc.additional_archives ?? [])].find(",
        "fail(judged?.sha256 === preflight[artifact],",
        "rc_build: {",
    ):
        require(publish, policy, "publish binds every archive to the located RC Build")
    # A preflight leg judges <artifact>.tar.gz. The primary archive is that
    # tarball everywhere but Windows, whose primary archive is the zip, so a
    # publish job comparing the primary record with the preflight digest
    # refuses the first release that carries a Windows leg, while locate,
    # which reads the tarball, admits it.
    if "rc.archive.sha256 === preflight[artifact]" in publish:
        raise AssertionError(
            "publish compares the preflight digest with the primary archive, which is "
            "not the archive a Windows preflight leg judged"
        )

    for policy in (
        'export const RC_BUILD_WORKFLOW = ".github/workflows/rc-build.yml";',
        "export const MERGE_QUEUE_BRANCH = /^gh-readonly-queue\\/main\\/pr-[1-9][0-9]*-[0-9a-f]{40}$/;",
        'export const RELEASE_AUTOMATION_ACTORS = Object.freeze(["github-actions[bot]", "kin-release-bot[bot]"]);',
        "judgePreflight(record, sha);",
        "export const preflightArchive = (artifact) => `${artifact}.tar.gz`;",
        "const judgedArchive = preflightArchive(entry.artifact);",
        '"--signer-workflow", `${repository}/${RC_BUILD_WORKFLOW}`',
        '"--signer-digest", sha',
        '"--source-digest", sha',
        '"--deny-self-hosted-runners"',
        "signedBySelectedRun",
        "is ambiguous",
        'entry.run.event === "merge_group"',
        "refusing a partial reading",
    ):
        require(locator, policy, "RC Build locator bindings")


def move_step_before(job: str, step: str, step_end: str, anchor: str) -> str:
    """Move one step, up to the next one, to just before `anchor`."""

    start = job.index(step)
    end = job.index(step_end, start)
    moved = job[start:end]
    without = job[:start] + job[end:]
    at = without.index(anchor)
    return without[:at] + moved + without[at:]


def check_release_consumes_rc_build(release: str) -> None:
    locator = (ROOT / "scripts" / "locate-rc-build.mjs").read_text(encoding="utf-8")
    assert_release_consumes_rc_build(release, locator)
    jobs = workflow_job_blocks(release)
    for label, mutate, expected in (
        (
            "a shipping job downloads from this run instead of the RC Build",
            lambda text: text.replace(jobs["sign_macos"], jobs["sign_macos"].replace(
                "          run-id: ${{ needs.locate_rc_build.outputs.run_id }}\n", "", 1)),
            "sign_macos RC Build consumption",
        ),
        (
            "the image push skips the RC Build attestation",
            lambda text: text.replace(jobs["publish_daemon_image"], jobs["publish_daemon_image"].replace(
                "node scripts/locate-rc-build.mjs verify-attestation", "true", 1)),
            "publish_daemon_image RC Build consumption",
        ),
        (
            "an unsigned archive is repacked before it ships",
            lambda text: text.replace(jobs["ship_rc_archives"], jobs["ship_rc_archives"].replace(
                "          mkdir -p \"$RUNNER_TEMP/ship\"\n", "          mkdir -p \"$RUNNER_TEMP/ship\"\n          tar czf repacked.tar.gz .\n", 1)),
            "must not involve tar czf",
        ),
        (
            "locating the RC Build gains the release environment",
            lambda text: text.replace(jobs["locate_rc_build"], jobs["locate_rc_build"].replace(
                "    timeout-minutes: 10\n", "    timeout-minutes: 10\n    environment: release\n", 1)),
            "needs no environment:",
        ),
        (
            "the signature-only proof runs before the archive is packaged",
            lambda text: text.replace(jobs["sign_macos"], move_step_before(
                jobs["sign_macos"],
                "      - name: Prove the signed archive differs from the RC archive only by its signatures\n",
                "      - name: Verify native release build provenance of the signed binaries\n",
                "      - name: Package (Unix)\n",
            )),
            "in that order",
        ),
        (
            "the publish job stops binding archives to the RC Build",
            lambda text: text.replace(jobs["publish"], jobs["publish"].replace(
                "String(rc.run_id) === process.env.RC_RUN_ID", "true", 1)),
            "publish binds every archive to the located RC Build",
        ),
        (
            "the publish job judges the preflight digest against the primary archive again",
            lambda text: text.replace(jobs["publish"], jobs["publish"].replace(
                "            if (artifact in preflight) {\n",
                "            if (artifact in preflight) {\n"
                "              fail(rc.archive.sha256 === preflight[artifact], `${artifact} primary`);\n",
                1,
            )),
            "not the archive a Windows preflight leg judged",
        ),
    ):
        mutated = mutate(release)
        if mutated == release:
            raise AssertionError(f"falsifier anchor moved: {label}")
        expect_assertion(
            label,
            expected,
            lambda mutated=mutated: assert_release_consumes_rc_build(mutated, locator),
        )
    for label, needle in (
        ("the locator admits a run from another workflow", '"--signer-workflow", `${repository}/${RC_BUILD_WORKFLOW}`'),
        ("the locator stops binding attestations to the selected run", "signedBySelectedRun"),
        ("the locator judges the primary archive instead of the preflight's", "const judgedArchive = preflightArchive(entry.artifact);"),
    ):
        expect_assertion(
            label,
            "RC Build locator bindings",
            lambda needle=needle: assert_release_consumes_rc_build(release, locator.replace(needle, "")),
        )

    completed = subprocess.run(
        ["node", "--test", *RC_BUILD_CONTRACT_SUITES],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    if completed.returncode != 0:
        raise AssertionError(
            "the RC Build to release contract suites failed:\n"
            + completed.stdout[-4000:]
            + completed.stderr[-2000:]
        )


NOTARIZE_BACKEND_STEP = "Resolve notarize backend"


def run_notarize_backend(config: str, value: str) -> tuple[int, str, str]:
    """Run config's backend step as written, with NOTARIZE_ON_LINUX set to `value`."""

    shell = workflow_step_shell(config, NOTARIZE_BACKEND_STEP)
    with tempfile.TemporaryDirectory(prefix="kin-notarize-backend-") as scratch:
        output = os.path.join(scratch, "github-output")
        open(output, "w", encoding="utf-8").close()
        completed = subprocess.run(
            ["bash", "-c", shell],
            env={"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "REPO_VAR": value, "GITHUB_OUTPUT": output},
            capture_output=True,
            text=True,
            check=False,
        )
        with open(output, encoding="utf-8") as handle:
            written = handle.read()
    return completed.returncode, written, completed.stdout + completed.stderr


def assert_linux_notarization_refused(release: str) -> None:
    """config refuses NOTARIZE_ON_LINUX until the Linux path staples and repacks.

    sign_macos packs the macOS archive before notarize_linux runs, and
    notarize_linux only submits to Apple Notary: it never staples
    KinNotifier.app or repacks the archive, so the bundle would ship without
    the ticket the notarytool path staples into it. Lift this pin in the change
    that makes notarize_linux staple the bundle, then repack the archive and
    regenerate its checksum and provenance manifest, not before.
    """

    config = workflow_job_blocks(release)["config"]
    require(
        config,
        'REPO_VAR: ${{ vars.NOTARIZE_ON_LINUX }}',
        "config reads the Linux notarization switch",
    )
    code, written, said = run_notarize_backend(config, "true")
    if code == 0 or "notarize_on_linux=true" in written:
        raise AssertionError(
            "config must refuse NOTARIZE_ON_LINUX=true: notarize_linux neither staples "
            f"KinNotifier.app nor repacks the archive (exit {code}, wrote {written!r})"
        )
    if "::error::NOTARIZE_ON_LINUX is true" not in said:
        raise AssertionError(
            "config must refuse NOTARIZE_ON_LINUX=true with an error that names the variable"
        )
    for value in ("", "false", "TRUE"):
        code, written, said = run_notarize_backend(config, value)
        if code != 0 or written != "notarize_on_linux=false\n":
            raise AssertionError(
                f"config must select notarytool when NOTARIZE_ON_LINUX is {value!r} "
                f"(exit {code}, wrote {written!r}): {said}"
            )


def check_linux_notarization_refused(release: str) -> None:
    assert_linux_notarization_refused(release)
    config = workflow_job_blocks(release)["config"]
    refusal = (
        '            echo "::error::NOTARIZE_ON_LINUX is true, but the Linux notarization path'
    )
    start = config.index(refusal)
    end = config.index("            exit 1\n", start) + len("            exit 1\n")
    for label, replacement, expected in (
        (
            "config selects the Linux backend again",
            '            echo "notarize_on_linux=true" >> "$GITHUB_OUTPUT"\n'
            "            exit 0\n",
            "config must refuse NOTARIZE_ON_LINUX=true",
        ),
        (
            "config only warns about the Linux backend",
            '            echo "::warning::NOTARIZE_ON_LINUX is true"\n',
            "config must refuse NOTARIZE_ON_LINUX=true",
        ),
        (
            "config refuses without saying why",
            "            exit 1\n",
            "an error that names the variable",
        ),
    ):
        mutated_config = config[:start] + replacement + config[end:]
        mutated = release.replace(config, mutated_config, 1)
        if mutated == release:
            raise AssertionError(f"falsifier anchor moved: {label}")
        expect_assertion(
            label,
            expected,
            lambda mutated=mutated: assert_linux_notarization_refused(mutated),
        )
