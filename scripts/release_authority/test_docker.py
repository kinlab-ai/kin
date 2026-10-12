# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Checks that pin docker.yml, the Dockerfile base images, base-image-pins.yml
and the dev image publisher.
"""

from __future__ import annotations

import re

from .test_helpers import (
    ROOT,
    WORKFLOWS,
    expect_assertion,
    job_step_active_lines,
    require,
    workflow_job_blocks,
)


BASE_IMAGE_REGISTRY = "docker.io"
BASE_IMAGE_MIRROR = 'mirrors = ["mirror.gcr.io"]'
BASE_IMAGE_MIRROR_INPUT = "buildkitd-config-inline:"
BASE_IMAGE_PIN = re.compile(r"(?m)^FROM\s+(?P<reference>\S+)")
SETUP_BUILDX_ACTION = "docker/setup-buildx-action@"
BASE_IMAGE_PINS = WORKFLOWS / "base-image-pins.yml"


def assert_container_base_image_authority(
    dockerfile: str,
    docker_workflow: str,
    rc_build: str,
) -> None:
    """Keep one registry from being able to refuse a release on its own.

    `Build and publish dev daemon image` publishes a check-run against every commit
    that reaches main, and release-tag.yml's second sweep refuses a release
    commit carrying any non-green check-run whether or not a ruleset requires
    it. Minutes of registry unavailability therefore refuse a release
    permanently, which has already cost a cut. Two properties remove that: the
    build resolves base images through a mirror before the canonical registry,
    and every base image is pinned by digest so the two doors cannot serve
    different bytes.

    The released image is built by rc-build.yml's image job, before any tag
    exists; release.yml pushes those bytes and builds nothing.
    """

    references = [
        match.group("reference") for match in BASE_IMAGE_PIN.finditer(dockerfile)
    ]
    if not references:
        raise AssertionError(
            "the release container Dockerfile must declare at least one base image"
        )
    for reference in references:
        if re.search(r"@sha256:[0-9a-f]{64}$", reference) is None:
            raise AssertionError(
                "release container base images must be pinned by digest, so a "
                "mirror cannot serve different bytes than the canonical "
                f"registry: {reference}"
            )
        if not reference.startswith(f"{BASE_IMAGE_REGISTRY}/"):
            raise AssertionError(
                "release container base images must name their registry, so the "
                f"reviewed mirror rewrite applies to them: {reference}"
            )

    for label, source, job in (
        ("docker.yml", docker_workflow, "build-image"),
        ("rc-build.yml", rc_build, "image"),
    ):
        block = workflow_job_blocks(source).get(job)
        if block is None:
            raise AssertionError(
                f"{label} no longer declares the {job} job that builds the Dockerfile"
            )
        mirror_input = setup_buildx_mirror_input(block, f"{label}:{job}")
        for needle in (f'[registry."{BASE_IMAGE_REGISTRY}"]', BASE_IMAGE_MIRROR):
            require(mirror_input, needle, f"{label}:{job} base image mirror")


def assert_pin_prover_cannot_refuse_a_release(pins_workflow: str) -> None:
    """Keep the scheduled pin prover unable to red the sha it runs against.

    A `schedule` run's check-runs land on the default branch HEAD, and the
    release sweep treats anything but success, skipped or neutral as a terminal
    refusal. This job therefore must not conclude on what it finds. That held by
    accident once and then stopped holding, because a `run:` step is executed by
    `bash -e {0}` and the step's own `set -o pipefail` is what armed errexit
    against the prover's non-zero status. The remedy read correctly and did
    nothing for a full review round, so the properties it depends on are pinned
    here rather than left to the next reader to re-derive.
    """

    verify = job_step_active_lines(pins_workflow, "verify-pins", "id: verify")
    shell = next(
        (line for line in verify if line.strip().startswith("shell:")),
        None,
    )
    if shell is None:
        raise AssertionError(
            "the pin prover step must declare its shell explicitly; the default "
            "carries -e, which kills the step on the prover status it exists to "
            "capture"
        )
    if re.search(r"(?<![\w-])-\w*e", shell.split("shell:", 1)[1]) is not None:
        raise AssertionError(
            f"the pin prover step's shell must not carry errexit: {shell.strip()}"
        )
    for policy in ("set +e", "exit 0"):
        require(
            "\n".join(verify),
            policy,
            "pin prover step that must not conclude on what it finds",
        )

    checkout = job_step_active_lines(pins_workflow, "verify-pins", "actions/checkout@")
    require(
        "\n".join(checkout),
        "continue-on-error: true",
        "pin prover checkout, whose failure would fail the job regardless of "
        "the guard below it",
    )


def assert_dev_image_publish_policy(
    workflow: str,
    publisher: str,
    local_tool: str,
) -> None:
    """Keep the free hosted builder narrow, exact, and replayable."""

    job = workflow_job_blocks(workflow).get("build-image")
    if job is None:
        raise AssertionError("dev image workflow no longer declares build-image")

    required_workflow = (
        "run-name: ${{ github.event_name == 'workflow_dispatch' && format('Dev image {0} {1}', github.event.inputs.mode || 'canary', github.event.inputs.commit || github.sha) || format('Dev image main {0}', github.sha) }}",
        "workflow_dispatch:",
        "description: Full 40-hex commit reachable from main",
        "description: Canary proves GitHub upload; publish writes the supported dev image",
        "type: choice",
        "- canary",
        "- publish",
        "group: ${{ github.workflow }}-${{ github.ref }}-${{ github.event_name == 'workflow_dispatch' && (github.event.inputs.commit || github.sha) || github.sha }}",
        "runs-on: ubuntu-latest",
        "id-token: write",
        "persist-credentials: false",
        "fetch-depth: 0",
        "REQUESTED_COMMIT: ${{ github.event.inputs.commit || '' }}",
        "REQUESTED_MODE: ${{ github.event.inputs.mode || '' }}",
        "EVENT_NAME: ${{ github.event_name }}",
        "^[0-9a-f]{40}$",
        'git merge-base --is-ancestor "$commit" origin/main',
        'install -m 0755 "scripts/${helper}" "$RUNNER_TEMP/${helper}"',
        'install -m 0644 docker-compose.yml "$RUNNER_TEMP/docker-compose.yml"',
        'echo "mode=${mode}" >> "$GITHUB_OUTPUT"',
        'echo "publish_tag=gha-canary-${commit}" >> "$GITHUB_OUTPUT"',
        'echo "publish_tag=${commit}" >> "$GITHUB_OUTPUT"',
        "publish-dev-container.sh",
        "verify-container-build-info.sh",
        "test-entrypoint-smoke.sh",
        "test-entrypoint-workspace-mount.sh",
        "test-compose-published-port.sh",
        "test-gcs-emulator-endpoint.sh",
        "docker/build-push-action@53b7df96c91f9c12dcc8a07bcb9ccacbed38856a",
        "push: false",
        "load: true",
        "KIN_BUILD_GIT_SHA=${{ steps.source.outputs.commit }}",
        "KIN_BUILD_DIRTY=false",
        "KIN_BUILD_BRANCH=${{ steps.source.outputs.branch }}",
        "KIN_LTO=fat",
        "cache-from: type=gha",
        "cache-to: type=gha,mode=max",
        "EXPECTED_SHA: ${{ steps.source.outputs.commit }}",
        'bash "$RUNNER_TEMP/verify-container-build-info.sh" kin:ci "$EXPECTED_SHA"',
        'bash "$RUNNER_TEMP/test-entrypoint-smoke.sh" kin:ci',
        'bash "$RUNNER_TEMP/test-entrypoint-workspace-mount.sh" kin:ci',
        'KIN_COMPOSE_FILE: ${{ runner.temp }}/docker-compose.yml',
        'bash "$RUNNER_TEMP/test-compose-published-port.sh" kin:ci',
        'bash "$RUNNER_TEMP/test-gcs-emulator-endpoint.sh" kin:ci',
        "GCP_WORKLOAD_IDENTITY_PROVIDER",
        "GCP_BUILD_SERVICE_ACCOUNT",
        "GCP_ARTIFACT_REGISTRY",
        "projects/642331057243/locations/global/workloadIdentityPools/github-actions/providers/kin-build-oidc",
        "kin-github-builder@kin-ecosystem.iam.gserviceaccount.com",
        "us-central1-docker.pkg.dev/kin-ecosystem/kin-dev",
        "id: publisher-config",
        "development image OIDC is not activated; build and smoke passed, publish skipped",
        'echo "ready=false" >> "$GITHUB_OUTPUT"',
        'echo "ready=true" >> "$GITHUB_OUTPUT"',
        "if: steps.publisher-config.outputs.ready == 'true'",
        "google-github-actions/auth@7c6bc770dae815cd3e89ee6cdf493a5fab2cc093",
        "token_format: access_token",
        "create_credentials_file: false",
        "docker/login-action@dbcb813823bdd20940b903addbd779551569679f",
        "username: oauth2accesstoken",
        "password: ${{ steps.gcp-auth.outputs.access_token }}",
        "KIN_DEV_IMAGE_VERIFIER: ${{ runner.temp }}/verify-container-build-info.sh",
        "id: publish",
        "DIGEST: ${{ steps.publish.outputs.digest }}",
        "ALIASES_PROMOTED: ${{ steps.publish.outputs.aliases_promoted }}",
        "REFERENCE: ${{ steps.publish.outputs.reference }}",
        "PUBLICATION: ${{ steps.publish.outputs.publication }}",
        "READBACK: ${{ steps.publish.outputs.readback }}",
        "EMBEDDED_SHA: ${{ steps.publish.outputs.embedded_sha }}",
        'if [ "$ALIASES_PROMOTED" != false ]',
        "GitHub publisher must never write mutable aliases",
        "## Development image proof",
        "KIN_DEV_IMAGE_PROOF mode=${MODE} source=${COMMIT} reference=${REFERENCE} digest=${DIGEST} publication=${PUBLICATION} aliases_promoted=${ALIASES_PROMOTED} readback=${READBACK} embedded_sha=${EMBEDDED_SHA} transport=github-hosted",
        "standard GitHub-hosted runner with GCP OIDC",
        'kin:ci "${REGISTRY}/kin-daemon" "$COMMIT" "$PUBLISH_TAG"',
    )
    for policy in required_workflow:
        require(workflow, policy, "GitHub-hosted development image publisher")

    expected_job_if = (
        "if: ${{ (github.event_name == 'push' || "
        "github.event_name == 'workflow_dispatch') && "
        "github.ref == 'refs/heads/main' }}"
    )
    active_job_conditions = [
        line.strip()
        for line in job.splitlines()
        if re.match(r"^    if:\s+", line)
        and not line.lstrip().startswith("#")
    ]
    if active_job_conditions != [expected_job_if]:
        raise AssertionError(
            "development image OIDC requires exactly one active job-level "
            "push/workflow_dispatch allowlist on refs/heads/main"
        )

    # The GCP exchange and mutation must remain after every image smoke, and the
    # exact source resolver must remain before the one build. This is an order
    # contract, not merely a census of strings comments could satisfy.
    ordered_markers = (
        "      - name: Resolve exact main source",
        "      - name: Build image",
        "      - name: Verify container build provenance",
        "      - name: Entrypoint smoke",
        "      - name: Entrypoint contract with a volume mounted inside .kin",
        "      - name: Published port reachable from outside the container namespace",
        "      - name: GCS emulator endpoint acceptance",
        "      - name: Validate development registry identity",
        "      - name: Authenticate development image writer",
        "      - name: Log in to Artifact Registry",
        "      - name: Publish immutable development image",
        "      - name: Record development image proof",
    )
    positions = [job.index(marker) for marker in ordered_markers]
    if positions != sorted(positions):
        raise AssertionError(
            "development image auth or publish moved ahead of build provenance smoke"
        )

    for forbidden in (
        "packages: write",
        "contents: write",
        "secrets.",
        "gcloud ",
        "gcloud-builds",
        "cloudbuild.yaml",
        "ghcr.io",
        "github.run_id",
        "staging-latest",
        "imagetools create",
    ):
        if forbidden in job:
            raise AssertionError(
                f"development image publisher contains forbidden authority: {forbidden}"
            )
    if job.count("id-token: write") != 1:
        raise AssertionError(
            "development image publisher must grant exactly one job-scoped id-token writer"
        )
    if job.count("if: steps.publisher-config.outputs.ready == 'true'") != 4:
        raise AssertionError(
            "auth, registry login, publish, and proof must all skip before OIDC activation"
        )
    if re.search(r"(?m)^\s+run:\s+bash scripts/", job):
        raise AssertionError(
            "historical checkout shell must not execute on the OIDC-capable host"
        )

    required_publisher = (
        "<40-hex-commit> <publish-tag>",
        "KIN_DEV_IMAGE_VERIFIER",
        "^[0-9a-f]{40}$",
        'canary_tag="gha-canary-${commit}"',
        'if [ "$publish_tag" != "$commit" ] && [ "$publish_tag" != "$canary_tag" ]',
        'source_ref="${registry_image}:${publish_tag}"',
        'if source_digest="$(inspect_digest "$source_ref")"',
        '"${registry_image}@${source_digest}" "$commit"',
        'bash "$verifier"',
        'if [ "$inspect_status" -ne 44 ]',
        'docker image tag "$local_image" "$source_ref"',
        'docker push "$source_ref"',
        "^sha256:[0-9a-f]{64}$",
        'references=("$source_ref")',
        'for reference in "${references[@]}"',
        'if [ "$actual_digest" != "$source_digest" ]',
        'echo "digest=$source_digest"',
        'echo "aliases_promoted=false"',
        'echo "reference=$source_ref"',
        'echo "publication=$publication"',
        'publication="verified_existing"',
        'publication="published"',
        'echo "readback=true"',
        'echo "embedded_sha=true"',
    )
    for policy in required_publisher:
        require(publisher, policy, "immutable development image helper")

    existing_start = publisher.index(
        'if source_digest="$(inspect_digest "$source_ref")"'
    )
    first_push = publisher.index('docker push "$source_ref"', existing_start)
    first_verify = publisher.index(
        'bash "$verifier"', existing_start
    )
    readback = publisher.index(
        'for reference in "${references[@]}"', first_push
    )
    served_verify = publisher.rindex('bash "$verifier"')
    proof_outputs = publisher.index('echo "readback=true"', served_verify)
    if not (
        existing_start
        < first_verify
        < first_push
        < readback
        < served_verify
        < proof_outputs
    ):
        raise AssertionError(
            "existing commit provenance must pass before a replay is accepted, "
            "and the immutable registry reference plus served digest must be "
            "verified before proof outputs are recorded"
        )
    for forbidden in (
        "docker build ",
        "docker buildx build",
        "docker buildx imagetools create",
        "gcloud ",
        ":latest",
        ":main",
        "staging-latest",
        "promote_aliases",
        "branch_ref",
    ):
        if forbidden in publisher:
            raise AssertionError(
                f"immutable development image helper contains forbidden behavior: {forbidden}"
            )

    required_tool = (
        "scripts/kin-dev-image local [image-tag]",
        "scripts/kin-dev-image canary [40-hex-commit]",
        "scripts/kin-dev-image hosted [40-hex-commit]",
        'if [ "$command" = canary ]',
        "mode=canary",
        "mode=publish",
        "KIN_LTO=thin",
        "KIN_BUILD_DIRTY=false",
        "full lowercase 40-hex commit",
        'remote get-url origin',
        "firelock-ai/kin.git",
        'merge-base --is-ancestor "$resolved" origin/main',
        'workflow run docker.yml',
        '--repo "$UPSTREAM_REPOSITORY"',
        "--ref main",
        '-f "commit=${resolved}"',
        '-f "mode=${mode}"',
        "--event workflow_dispatch",
        'expected_title="Dev image ${mode} ${resolved}"',
        'if ! grep -Fxq "$candidate_id" "$prior_runs"',
        'run watch "$run_id"',
        "--exit-status",
        'run view "$run_id"',
        'grep -F "KIN_DEV_IMAGE_PROOF mode=${mode} source=${resolved} "',
        "us-central1-docker.pkg.dev/kin-ecosystem/kin-dev/kin-daemon:gha-canary-${resolved}",
        "publication=published aliases_promoted=false readback=true embedded_sha=true transport=github-hosted",
        "publication=(published|verified_existing) aliases_promoted=false readback=true embedded_sha=true transport=github-hosted",
        'echo "KIN_DEV_IMAGE_RUN id=$run_id url=$run_url"',
        'echo "KIN_DEV_IMAGE_PROOF $proof"',
    )
    for policy in required_tool:
        require(local_tool, policy, "local-first development image tool")


def comment_out_mirror_input(workflow: str) -> str:
    """Comment out the buildx mirror input while leaving its text in the file."""

    lines = workflow.splitlines(keepends=True)
    mutated: list[str] = []
    commenting = False
    input_indent = 0
    for line in lines:
        stripped = line.strip()
        indent = len(line) - len(line.lstrip())
        if stripped.startswith(BASE_IMAGE_MIRROR_INPUT):
            commenting = True
            input_indent = indent
            mutated.append(f"{' ' * indent}# {line.lstrip()}")
            continue
        if commenting:
            if not stripped or indent > input_indent:
                mutated.append(f"{' ' * indent}# {line.lstrip()}" if stripped else line)
                continue
            commenting = False
        mutated.append(line)
    return "".join(mutated)


def setup_buildx_mirror_input(job_block: str, label: str) -> str:
    """Return the active `buildkitd-config-inline` the buildx action receives.

    Asserting the mirror against the job body as raw text accepts a config that
    was commented out and left behind, which is the shape a debugging session
    produces and the one shape a delete-the-string falsification cannot catch.
    Binding the assertion to the input of the step whose `uses:` is the buildx
    action, with comment lines dropped, means only a configuration the action
    is actually handed can satisfy it.
    """

    lines = job_block.splitlines()
    uses = [
        index
        for index, line in enumerate(lines)
        if SETUP_BUILDX_ACTION in line and not line.lstrip().startswith("#")
    ]
    if len(uses) != 1:
        raise AssertionError(
            f"{label} base image mirror expects exactly one buildx setup step, "
            f"found {len(uses)}"
        )

    start = uses[0]
    while start >= 0 and re.match(r"^\s*-\s", lines[start]) is None:
        start -= 1
    if start < 0:
        raise AssertionError(
            f"{label} base image mirror could not find the buildx step's start"
        )
    step_indent = len(lines[start]) - len(lines[start].lstrip())

    active: list[str] = []
    for line in lines[start + 1 :]:
        if not line.strip():
            continue
        indent = len(line) - len(line.lstrip())
        if indent <= step_indent:
            break
        if line.lstrip().startswith("#"):
            continue
        active.append(line)

    input_index = next(
        (
            index
            for index, line in enumerate(active)
            if line.strip().startswith(BASE_IMAGE_MIRROR_INPUT)
        ),
        None,
    )
    if input_index is None:
        raise AssertionError(
            f"{label} base image mirror is missing required policy: "
            f"{BASE_IMAGE_MIRROR_INPUT}"
        )

    input_indent = len(active[input_index]) - len(active[input_index].lstrip())
    body: list[str] = []
    for line in active[input_index + 1 :]:
        if len(line) - len(line.lstrip()) <= input_indent:
            break
        body.append(line)
    return "\n".join(body)



def check_dev_image_publish_policy(
    ci_workflow: str,
    dev_image_publisher: str,
    dev_image_tool: str,
    docker_workflow: str,
) -> None:
    assert_dev_image_publish_policy(
        docker_workflow,
        dev_image_publisher,
        dev_image_tool,
    )
    for invocation in (
        "bash ./scripts/test-publish-dev-container.sh",
        "bash ./scripts/kin-dev-image.test.sh",
        "bash -n ./scripts/publish-dev-container.sh",
        "bash -n ./scripts/kin-dev-image",
    ):
        require(ci_workflow, invocation, "development image guard reachability")
    expect_assertion(
        "same-commit manual publishers stop sharing one serial group",
        "missing required policy: group:",
        lambda: assert_dev_image_publish_policy(
            docker_workflow.replace(
                "(github.event.inputs.commit || github.sha)",
                "github.run_id",
                1,
            ),
            dev_image_publisher,
            dev_image_tool,
        ),
    )
    expect_assertion(
        "dev image OIDC starts admitting an unreviewed future event",
        "requires exactly one active job-level",
        lambda: assert_dev_image_publish_policy(
            docker_workflow.replace(
                "if: ${{ (github.event_name == 'push' || github.event_name == 'workflow_dispatch') && github.ref == 'refs/heads/main' }}",
                "if: ${{ github.event_name != 'pull_request' && github.ref == 'refs/heads/main' }}",
                1,
            ),
            dev_image_publisher,
            dev_image_tool,
        ),
    )
    safe_dev_image_job_if = (
        "if: ${{ (github.event_name == 'push' || "
        "github.event_name == 'workflow_dispatch') && "
        "github.ref == 'refs/heads/main' }}"
    )
    unsafe_dev_image_job_if = (
        "if: ${{ github.event_name != 'pull_request' && "
        "github.ref == 'refs/heads/main' }}"
    )
    expect_assertion(
        "a commented safe allowlist compensates for an unsafe active job field",
        "requires exactly one active job-level",
        lambda: assert_dev_image_publish_policy(
            docker_workflow.replace(
                f"    {safe_dev_image_job_if}",
                f"    {unsafe_dev_image_job_if}\n    # {safe_dev_image_job_if}",
                1,
            ),
            dev_image_publisher,
            dev_image_tool,
        ),
    )
    expect_assertion(
        "manual canary reuses the legacy full-SHA tag",
        "missing required policy: echo \"publish_tag=gha-canary-",
        lambda: assert_dev_image_publish_policy(
            docker_workflow.replace(
                'echo "publish_tag=gha-canary-${commit}"',
                'echo "publish_tag=${commit}"',
                1,
            ),
            dev_image_publisher,
            dev_image_tool,
        ),
    )
    expect_assertion(
        "historical compose data reaches the OIDC-capable runner",
        "missing required policy: KIN_COMPOSE_FILE:",
        lambda: assert_dev_image_publish_policy(
            docker_workflow.replace(
                "KIN_COMPOSE_FILE: ${{ runner.temp }}/docker-compose.yml",
                "KIN_COMPOSE_FILE: ${{ github.workspace }}/docker-compose.yml",
                1,
            ),
            dev_image_publisher,
            dev_image_tool,
        ),
    )
    expect_assertion(
        "registry login runs while the development OIDC contract is inactive",
        "auth, registry login, publish, and proof must all skip",
        lambda: assert_dev_image_publish_policy(
            docker_workflow.replace(
                "        if: steps.publisher-config.outputs.ready == 'true'\n",
                "",
                1,
            ),
            dev_image_publisher,
            dev_image_tool,
        ),
    )
    expect_assertion(
        "historical checkout shell executes on the OIDC-capable runner",
        "historical checkout shell must not execute",
        lambda: assert_dev_image_publish_policy(
            docker_workflow.replace(
                "      - name: Authenticate development image writer\n",
                "      - name: Execute historical helper\n"
                "        run: bash scripts/verify-container-build-info.sh kin:ci "
                '"$GITHUB_SHA"\n\n'
                "      - name: Authenticate development image writer\n",
                1,
            ),
            dev_image_publisher,
            dev_image_tool,
        ),
    )
    expect_assertion(
        "manual image publish accepts a commit outside main",
        "missing required policy: git merge-base --is-ancestor",
        lambda: assert_dev_image_publish_policy(
            docker_workflow.replace(
                'git merge-base --is-ancestor "$commit" origin/main',
                'git cat-file -e "$commit"',
                1,
            ),
            dev_image_publisher,
            dev_image_tool,
        ),
    )
    existing_verify_start = dev_image_publisher.index(
        '  bash "$verifier"'
    )
    existing_verify_end = dev_image_publisher.index(
        '  publication="verified_existing"', existing_verify_start
    )
    publisher_without_existing_verify = (
        dev_image_publisher[:existing_verify_start]
        + dev_image_publisher[existing_verify_end:]
    )
    expect_assertion(
        "an existing commit tag is accepted before its provenance is checked",
        "existing commit provenance must pass before a replay is accepted",
        lambda: assert_dev_image_publish_policy(
            docker_workflow,
            publisher_without_existing_verify,
            dev_image_tool,
        ),
    )
    expect_assertion(
        "the immutable publisher reintroduces a staging alias",
        "contains forbidden behavior: docker buildx imagetools create",
        lambda: assert_dev_image_publish_policy(
            docker_workflow,
            dev_image_publisher
            + "\ndocker buildx imagetools create --tag image:staging-latest image@sha256:bad\n",
            dev_image_tool,
        ),
    )
    expect_assertion(
        "local helper dispatches branch-controlled workflow code",
        "missing required policy: --ref main",
        lambda: assert_dev_image_publish_policy(
            docker_workflow,
            dev_image_publisher,
            dev_image_tool.replace("--ref main", '--ref "$branch"', 1),
        ),
    )



def check_base_image_pins_and_container_bases(
    docker_workflow: str, dockerfile: str, rc_build: str
) -> None:
    base_image_pins = BASE_IMAGE_PINS.read_text(encoding="utf-8")
    assert_pin_prover_cannot_refuse_a_release(base_image_pins)
    expect_assertion(
        "the pin prover step loses its explicit errexit-free shell",
        "must declare its shell explicitly",
        lambda: assert_pin_prover_cannot_refuse_a_release(
            re.sub(r"(?m)^\s+shell: bash --noprofile --norc \{0\}\n", "", base_image_pins)
        ),
    )
    expect_assertion(
        "the pin prover step's shell is given errexit back",
        "must not carry errexit",
        lambda: assert_pin_prover_cannot_refuse_a_release(
            base_image_pins.replace(
                "shell: bash --noprofile --norc {0}",
                "shell: bash --noprofile --norc -e {0}",
            )
        ),
    )
    expect_assertion(
        "the pin prover step stops disarming errexit in its own body",
        "missing required policy: set +e",
        lambda: assert_pin_prover_cannot_refuse_a_release(
            base_image_pins.replace("          set +e\n", "")
        ),
    )
    expect_assertion(
        "the pin prover step stops forcing a green conclusion",
        "missing required policy: exit 0",
        lambda: assert_pin_prover_cannot_refuse_a_release(
            base_image_pins.replace("          exit 0\n", "")
        ),
    )
    expect_assertion(
        "a failed checkout is allowed to fail the pin prover job",
        "missing required policy: continue-on-error: true",
        lambda: assert_pin_prover_cannot_refuse_a_release(
            base_image_pins.replace(
                "      - name: Checkout\n        continue-on-error: true\n",
                "      - name: Checkout\n",
            )
        ),
    )

    assert_container_base_image_authority(dockerfile, docker_workflow, rc_build)
    expect_assertion(
        "a floating base image tag reaches the release container",
        "must be pinned by digest",
        lambda: assert_container_base_image_authority(
            re.sub(r"@sha256:[0-9a-f]{64}", "", dockerfile, count=1),
            docker_workflow,
            rc_build,
        ),
    )
    expect_assertion(
        "a base image with no registry escapes the reviewed mirror rewrite",
        "must name their registry",
        lambda: assert_container_base_image_authority(
            dockerfile.replace(f"FROM {BASE_IMAGE_REGISTRY}/", "FROM ", 1),
            docker_workflow,
            rc_build,
        ),
    )
    expect_assertion(
        "the pull-request image build loses its second registry",
        "docker.yml:build-image base image mirror",
        lambda: assert_container_base_image_authority(
            dockerfile,
            docker_workflow.replace(BASE_IMAGE_MIRROR, ""),
            rc_build,
        ),
    )
    expect_assertion(
        "the released image build loses its second registry",
        "rc-build.yml:image base image mirror",
        lambda: assert_container_base_image_authority(
            dockerfile,
            docker_workflow,
            rc_build.replace(BASE_IMAGE_MIRROR, ""),
        ),
    )
    # Deleting the string is the one mutation shape a raw-text assertion always
    # catches, so it proves the least. These comment the config out instead and
    # leave the strings in the file, which is what a debugging session actually
    # leaves behind, and which a raw-text assertion passes.
    for label, mutated_docker, mutated_rc_build in (
        (
            "the pull-request image build's mirror is commented out",
            comment_out_mirror_input(docker_workflow),
            rc_build,
        ),
        (
            "the released image build's mirror is commented out",
            docker_workflow,
            comment_out_mirror_input(rc_build),
        ),
    ):
        expect_assertion(
            label,
            f"base image mirror is missing required policy: {BASE_IMAGE_MIRROR_INPUT}",
            lambda docker=mutated_docker, source=mutated_rc_build: (
                assert_container_base_image_authority(dockerfile, docker, source)
            ),
        )



def check_docker_workflow_pins(docker_workflow: str) -> None:
    for policy in (
        "actions/checkout@9c091bb21b7c1c1d1991bb908d89e4e9dddfe3e0",
        "docker/setup-buildx-action@bb05f3f5519dd87d3ba754cc423b652a5edd6d2c",
        "docker/build-push-action@53b7df96c91f9c12dcc8a07bcb9ccacbed38856a",
        'bash "$RUNNER_TEMP/test-entrypoint-smoke.sh" kin:ci',
        "cache-from: type=gha",
        "cache-to: type=gha,mode=max",
    ):
        require(docker_workflow, policy, "Docker CI authority and smoke gate")
    # The entrypoint smoke moved out of docker.yml so the released image's
    # build in rc-build.yml runs the same one. Its bounded-poll fix moved with it.
    smoke = (ROOT / "scripts" / "test-entrypoint-smoke.sh").read_text(encoding="utf-8")
    for policy in (
        "Keep polling the bounded log authority even",
        "set -euo pipefail",
        "daemon API server listening",
        "FATAL: workspace '/workspace' is not writable",
    ):
        require(smoke, policy, "shared entrypoint smoke")
