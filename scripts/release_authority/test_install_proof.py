# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Checks that pin .github/workflows/install-proof.yml.

Includes the install-proof canary and the preflight validator the proof
runs.
"""

from __future__ import annotations

import copy
import difflib
import hashlib
import json
import os
import re
import subprocess
import tempfile
import textwrap
from pathlib import Path

from .test_helpers import (
    MCP_TOOLS_DOC,
    NPM_CANONICAL_README,
    QUICKSTART_DOC,
    RELEASE_SUITE_EVENT_FILTER,
    ROOT,
    active_lines,
    classifier_active_job_source,
    dynamic_job_context_source,
    expect_assertion,
    install_proof_step,
    job_top_level_mapping_fields,
    replace_exactly_once,
    require,
    workflow_job_blocks,
    workflow_step_shell,
)


PORTED_PROOF_FLOW = (
    ROOT
    / "scripts"
    / "release-proof"
    / "bin"
    / "kin-release-preflight.d"
    / "proof-flow.sh"
)


def node_heredoc_body(step: str, label: str) -> str:
    """Extract one literal Node heredoc exactly as the Actions runner executes it."""

    start_marker = "          node <<'NODE'\n"
    end_marker = "          NODE\n"
    if step.count(start_marker) != 1:
        raise AssertionError(f"{label} must contain exactly one literal Node validator")
    start = step.index(start_marker) + len(start_marker)
    if step.count(end_marker, start) != 1:
        raise AssertionError(
            f"{label} must terminate exactly one literal Node validator"
        )
    body = step[start : step.index(end_marker, start)]
    yaml_indent = "          "
    lines: list[str] = []
    for line in body.splitlines():
        if line and not line.startswith(yaml_indent):
            raise AssertionError(
                f"{label} Node validator escaped the run-block indentation: {line!r}"
            )
        lines.append(line[len(yaml_indent) :] if line else "")
    return "\n".join(lines) + "\n"


def write_node_validator_fixture_files(
    root: Path,
    files: dict[str, object],
    containment_root: Path,
    label: str,
) -> None:
    """Write one isolated validator fixture without allowing path escape."""

    for relative_path, value in files.items():
        target = (root / relative_path).resolve()
        try:
            target.relative_to(containment_root)
        except ValueError as error:
            raise AssertionError(
                f"{label} fixture path escapes its isolated root: {relative_path!r}"
            ) from error
        target.parent.mkdir(parents=True, exist_ok=True)
        content = value if isinstance(value, str) else json.dumps(value)
        content = content.replace(
            "__VALIDATOR_HOME__", str((containment_root / "home").resolve())
        ).replace(
            "__VALIDATOR_PROOF__", str((containment_root / "proof").resolve())
        )
        target.write_text(content, encoding="utf-8")


def run_node_validator_fixture(
    step: str,
    label: str,
    proof_files: dict[str, object],
    home_files: dict[str, object] | None = None,
    extra_env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    """Run the exact inline validator against one deterministic proof fixture."""

    script = node_heredoc_body(step, label)
    with tempfile.TemporaryDirectory(prefix="kin-proof-validator-fixture-") as temporary:
        temporary_root = Path(temporary).resolve()
        proof_dir = temporary_root / "proof"
        home_dir = temporary_root / "home"
        proof_dir.mkdir()
        home_dir.mkdir()
        write_node_validator_fixture_files(
            proof_dir, proof_files, temporary_root, label
        )
        write_node_validator_fixture_files(
            home_dir, home_files or {}, temporary_root, label
        )
        environment = {
            **os.environ,
            "HOME": str(home_dir),
            "USERPROFILE": str(home_dir),
            **(extra_env or {}),
        }
        try:
            return subprocess.run(
                ["node", "-"],
                input=script,
                cwd=proof_dir,
                env=environment,
                text=True,
                capture_output=True,
                timeout=10,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            raise AssertionError(
                f"{label} could not execute its deterministic fixture: {error}"
            ) from error


def assert_node_validator_accepts_fixture(
    step: str,
    label: str,
    proof_files: dict[str, object],
    home_files: dict[str, object] | None = None,
    extra_env: dict[str, str] | None = None,
) -> None:
    """Require the exact validator to accept a complete known-good fixture."""

    result = run_node_validator_fixture(
        step, label, proof_files, home_files, extra_env
    )
    if result.returncode != 0:
        raise AssertionError(
            f"{label} rejected its known-good fixture with exit {result.returncode}: "
            f"{result.stderr.strip()}"
        )


def assert_node_validator_rejects_fixture(
    step: str,
    label: str,
    proof_files: dict[str, object],
    home_files: dict[str, object] | None = None,
    extra_env: dict[str, str] | None = None,
) -> None:
    """Require the exact validator to reject one behaviorally invalid fixture."""

    result = run_node_validator_fixture(
        step, label, proof_files, home_files, extra_env
    )
    if result.returncode == 0:
        raise AssertionError(f"{label} accepted a behaviorally invalid proof fixture")


VALIDATOR_FIXTURE_COMMIT = "a" * 40
VALIDATOR_FIXTURE_LOCK = "b" * 64
VALIDATOR_HOME = "__VALIDATOR_HOME__"
VALIDATOR_PROOF = "__VALIDATOR_PROOF__"


# The profiles setup assigns, as the install proof's validators expect them:
# Claude Code keeps agent-default, and every client that resends each tool with
# every request gets the routed profile.
DEFAULT_PROFILE = "agent-default"
ROUTED_PROFILE = "agent-routed"


def validator_mcp_entry(
    executable: str,
    *,
    profile: str,
    repo: str | None = None,
    cwd: str | None = None,
    extra_env: dict[str, str] | None = None,
) -> dict[str, object]:
    args = ["mcp", "start"]
    if repo is not None:
        args.extend(["--repo", repo])
    entry: dict[str, object] = {
        "command": executable,
        "args": args,
        "env": {
            **(extra_env or {}),
            "KIN_MCP_TOOL_PROFILE": profile,
        },
    }
    if cwd is not None:
        entry["cwd"] = cwd
    return entry


def validator_mcp_config(entry: dict[str, object]) -> dict[str, object]:
    return {"mcpServers": {"kin": entry}}


def swapped_agent_profile(fixture: dict[str, object], path: str) -> str:
    """Return the agent profile a fixture's MCP entry does not carry.

    A `full` drift fails a validator that accepts any agent profile in every
    entry just as it fails a correct one. Swapping the entry to the other agent
    profile is what shows the validator holds each client to its own.
    """

    env = fixture[path]["mcpServers"]["kin"]["env"]  # type: ignore[index]
    profile = env["KIN_MCP_TOOL_PROFILE"]  # type: ignore[index]
    return ROUTED_PROFILE if profile == DEFAULT_PROFILE else DEFAULT_PROFILE


def validator_windows_mcp_home_fixture() -> dict[str, object]:
    """Return every repo-free Windows JSON MCP config."""

    executable = f"{VALIDATOR_HOME}/.kin/bin/kin.exe"
    claude = validator_mcp_config(
        validator_mcp_entry(executable, profile=DEFAULT_PROFILE)
    )
    routed = validator_mcp_config(
        validator_mcp_entry(executable, profile=ROUTED_PROFILE)
    )
    return {
        ".claude.json": claude,
        ".cursor/mcp.json": copy.deepcopy(routed),
        ".gemini/settings.json": copy.deepcopy(routed),
        ".codeium/windsurf/mcp_config.json": copy.deepcopy(routed),
    }


def validator_unix_mcp_home_fixture() -> dict[str, object]:
    """Return every main-HOME Unix JSON MCP config."""

    executable = f"{VALIDATOR_HOME}/.kin/bin/kin"
    claude = validator_mcp_config(
        validator_mcp_entry(executable, profile=DEFAULT_PROFILE)
    )
    ordinary = validator_mcp_config(
        validator_mcp_entry(executable, profile=ROUTED_PROFILE)
    )
    repository = validator_mcp_config(
        validator_mcp_entry(
            executable,
            profile=ROUTED_PROFILE,
            repo=VALIDATOR_PROOF,
            cwd=VALIDATOR_PROOF,
        )
    )
    legacy = copy.deepcopy(repository)
    legacy["userPolicy"] = "preserve"
    legacy["mcpServers"]["kin"]["env"]["USER_POLICY"] = "preserve"
    return {
        ".claude.json": claude,
        ".cursor/mcp.json": copy.deepcopy(ordinary),
        ".gemini/settings.json": copy.deepcopy(ordinary),
        ".codeium/windsurf/mcp_config.json": copy.deepcopy(ordinary),
        ".gemini/config/mcp_config.json": copy.deepcopy(repository),
        ".gemini/antigravity-ide/mcp_config.json": legacy,
    }


HEALTH_JOIN_BEGIN = "// --- BEGIN HEALTH JOIN ---"
HEALTH_JOIN_END = "// --- END HEALTH JOIN ---"
# Every file carrying a copy of the health join. install-proof.yml and
# rc-build.yml run with no checkout and cannot import the module, so they paste
# it; `assert_health_join_copies_agree` requires the copies equal.
HEALTH_JOIN_HOMES = (
    "scripts/verify-capability-proof.mjs",
    ".github/workflows/install-proof.yml",
    ".github/workflows/rc-build.yml",
)


def health_join_verdict(statuses: dict[str, str]) -> str:
    """The product's roll-up rule, in Python, for building honest fixtures.

    A fourth copy, and deliberately so: a fixture generator that took the
    aggregate as an argument is what let this harness carry `healthy=True`
    beside `embedding_model: pending` and pass while the real Windows leg threw
    on that exact pair. `assert_health_join_copies_agree` pins the
    three JavaScript copies to each other; this one is pinned by the fixtures it
    builds having to survive the extracted validators.
    """

    if any(
        status in {"missing", "misconfigured"}
        or (check_id == "semantic_query_readiness" and status == "stale")
        for check_id, status in statuses.items()
    ):
        return "failing"
    if any(
        status not in {"healthy", "unsupported"} for status in statuses.values()
    ):
        return "needs_attention"
    return "ready"


def validator_health_report(
    statuses: dict[str, str],
    *,
    healthy: bool | None = None,
    verdict: str | None = None,
) -> dict[str, object]:
    """A health report whose aggregate is derived from its own checks.

    `healthy` and `verdict` stay overridable because several probes below have
    to build a report that disagrees with itself. Nothing else may pass them.
    """

    joined = health_join_verdict(statuses)
    return {
        "healthy": joined == "ready" if healthy is None else healthy,
        "verdict": joined if verdict is None else verdict,
        "checks": [
            {"id": check_id, "status": status}
            for check_id, status in statuses.items()
        ],
    }


WINDOWS_REQUIRED_VALIDATOR_CHECKS = {
    "kin_binary": "healthy",
    "kin_daemon_binary": "healthy",
    "shell_path": "healthy",
    "setup_ledger": "healthy",
    # The leg configures a host Git identity before setup, as a developer's
    # machine has; a fresh runner without one reads `missing` here, which is
    # what stopped v0.8.5.
    "commit_author": "healthy",
    "registry_authority": "unsupported",
    "vfs_projection": "unsupported",
    "semantic_query_readiness": "unsupported",
    "daemon_running": "unsupported",
    "repo_init": "unsupported",
    # Nothing recorded and nothing in force on a fresh repo-free install, so
    # there is no configured projection for the row to report on. Required
    # rather than tolerated: `misconfigured` here is what failed the real leg
    # on every release from v0.5.44 through v0.5.47.
    "projection_mode": "unsupported",
    "mcp_client_claude": "healthy",
    "mcp_client_cursor": "healthy",
    "mcp_client_gemini": "healthy",
    "mcp_client_windsurf": "healthy",
}
# The repo-free report carries checks the required map does not name, and
# omitting them here is how this harness kept passing while the real Windows
# leg threw on one of them. `retrieval_profile` is the case that did it: a
# fresh install has selected no profile and fetched no reranker model, which is
# a first-run state rather than a degraded serving configuration, and the leg's
# tolerance accepts it only as `unsupported`.
WINDOWS_VALIDATOR_CHECKS = {
    **WINDOWS_REQUIRED_VALIDATOR_CHECKS,
    "retrieval_profile": "unsupported",
    # The two first-run statuses the leg tolerates beyond healthy and
    # not-applicable. A public runner has never fetched the embedding model, so
    # a correct install reports `pending` there; the runner has 4 GiB and Kin
    # has no host-memory probe for target_os windows, so `memory_floor` reports
    # `degraded`. The step names each check with the status it may hold rather
    # than accepting those statuses generally. Carried in the fixture for the
    # reason `retrieval_profile` is: a fixture that omits a check the real
    # report carries cannot fail the way the real leg does.
    #
    # `memory_floor` is the proof of that sentence. It was absent here, the real
    # v0.6.1 Windows report carried it as `degraded`, and this harness passed
    # while that leg threw.
    "embedding_model": "pending",
    "memory_floor": "degraded",
}


def windows_node_validator_fixture() -> tuple[
    dict[str, object], dict[str, object], dict[str, str]
]:
    """Build a complete valid repository-free Windows proof fixture."""

    report = validator_health_report(WINDOWS_VALIDATOR_CHECKS)
    return (
        {
            "expected-commit.txt": VALIDATOR_FIXTURE_COMMIT,
            "expected-lock-sha.txt": VALIDATOR_FIXTURE_LOCK,
            "installed-kin-command.txt": f"{VALIDATOR_HOME}/.kin/bin/kin.exe",
            "kin-windows-bench-meta.json": {
                "kin_commit": VALIDATOR_FIXTURE_COMMIT,
                "kin_dirty": False,
                "kin_source_known": True,
                "dependency_provenance": VALIDATOR_FIXTURE_LOCK,
                "embeddings": {
                    "vector_enabled": True,
                    "embeddings_enabled": True,
                    "local_inference_enabled": False,
                    "external_embeddings_enabled": True,
                    "provider": "none",
                    "metal_enabled": False,
                },
            },
            "kin-windows-registry-authority.json": {
                "checks": [{"state": "unsupported"}]
            },
            "kin-windows-health.json": copy.deepcopy(report),
            "kin-windows-doctor.json": copy.deepcopy(report),
        },
        validator_windows_mcp_home_fixture(),
        {"RUNNER_OS": "Windows"},
    )


UNIX_REQUIRED_VALIDATOR_CHECKS = {
    "kin_binary": "healthy",
    "kin_daemon_binary": "healthy",
    "daemon_running": "healthy",
    "repo_init": "healthy",
    "shell_path": "healthy",
    "setup_ledger": "healthy",
    "registry_authority": "healthy",
    "vfs_projection": "unsupported",
    "mcp_client_claude": "healthy",
    "mcp_client_cursor": "healthy",
    "mcp_client_codex": "healthy",
    "mcp_client_gemini": "healthy",
    "mcp_client_windsurf": "healthy",
    "mcp_client_antigravity": "healthy",
    "mcp_client_antigravity_workspace": "healthy",
}
UNIX_VALIDATOR_CHECKS = {
    **UNIX_REQUIRED_VALIDATOR_CHECKS,
    # The archive is engine-free with no provider selected, so nothing will
    # embed and the honest readiness is unsupported, before and after the
    # refused `kin embed`.
    "semantic_query_readiness": "unsupported",
    # A tolerated first-run row, as the real runner reports with no language
    # server on it. It holds the honest aggregate false, so an overclaimed
    # `healthy` or `ready` stays something this fixture can be caught making.
    "reference_edge_coverage": "pending",
    # Carried for the same reason the Windows fixture carries it: the real
    # report has it, the pre-embed tolerance reads every check rather than only
    # the named ones, and a fixture that omits it cannot fail the way the real
    # leg does. A fresh install has selected no profile and fetched no reranker
    # model, which is a first-run state and reports unsupported.
    "retrieval_profile": "unsupported",
}


# What the engine-free archive's `kin embed` writes to stderr when no external
# provider is selected.
VALIDATOR_EMBED_REFUSAL = (
    "Error: kin embed cannot index anything here: this Kin build has no local "
    "embedding model, and no external embedding provider is selected for this "
    "repository. Search by meaning stays off, and locate, search, refs and "
    "context answer from the graph without it; docs/model-slots.md describes "
    "selecting an external embedding provider, or a build with the `embeddings` "
    "feature embeds locally\n"
)


def unix_node_validator_fixture() -> tuple[
    dict[str, object], dict[str, object], dict[str, str]
]:
    """Build a complete valid Unix release-byte proof fixture."""

    pre_embed_report = validator_health_report(UNIX_VALIDATOR_CHECKS)
    # The post-embed capture names the rows a completed embed leaves behind on
    # this runner, so the fixture carries them: `unsupported` must not move the
    # aggregate, and `memory_floor` degraded must not fail the leg. Carrying
    # both here is what makes those tested claims instead of assumed ones.
    embedded_report = validator_health_report(
        {
            "semantic_query_readiness": "unsupported",
            "retrieval_profile": "unsupported",
            "memory_floor": "degraded",
        }
    )
    fallback_report = validator_health_report({"mcp_client_claude": "healthy"})
    executable = f"{VALIDATOR_HOME}/.kin/bin/kin"
    claude_fallback_config = validator_mcp_config(
        validator_mcp_entry(executable, profile=DEFAULT_PROFILE)
    )
    repository_config = validator_mcp_config(
        validator_mcp_entry(
            executable,
            profile=ROUTED_PROFILE,
            repo=VALIDATOR_PROOF,
            cwd=VALIDATOR_PROOF,
        )
    )
    codex_config = validator_mcp_config(
        validator_mcp_entry(executable, profile=ROUTED_PROFILE, repo=VALIDATOR_PROOF)
    )
    return (
        {
            "../expected-commit.txt": VALIDATOR_FIXTURE_COMMIT,
            "../expected-lock-sha.txt": VALIDATOR_FIXTURE_LOCK,
            "../legacy-vfs-mode.txt": "false",
            "../installed-kin-command.txt": executable,
            "kin-status.json": {
                "schema": "kin.status.v3",
                "embedding_coverage": {
                    "state": "unobserved",
                    "reason": "no_running_daemon",
                },
            },
            "kin-build-meta.json": {
                "schema": "kin.bench-meta.v2",
                "kin_commit": VALIDATOR_FIXTURE_COMMIT,
                "kin_dirty": False,
                "kin_source_known": True,
                "dependency_provenance": VALIDATOR_FIXTURE_LOCK,
                "embeddings": {
                    "vector_enabled": True,
                    "embeddings_enabled": True,
                    "local_inference_enabled": False,
                    "external_embeddings_enabled": True,
                    "provider": "none",
                    "metal_enabled": False,
                },
            },
            "kin-daemon-health.json": {
                "build": {
                    "sha": VALIDATOR_FIXTURE_COMMIT,
                    "dirty": False,
                    "source_known": True,
                    "dependency_provenance": VALIDATOR_FIXTURE_LOCK,
                }
            },
            "kin-health.json": copy.deepcopy(pre_embed_report),
            "kin-doctor.json": copy.deepcopy(pre_embed_report),
            "kin-claude-fallback-health.json": copy.deepcopy(fallback_report),
            "kin-claude-fallback-doctor.json": copy.deepcopy(fallback_report),
            "kin-claude-fallback-config.json": copy.deepcopy(claude_fallback_config),
            "kin-codex-config.json": copy.deepcopy(codex_config),
            ".agents/mcp_config.json": copy.deepcopy(repository_config),
            "kin-search.json": [
                {"name": "hello", "file": "probe.py"}
            ],
            "kin-locate.json": {"files": [{"path": "probe.py"}]},
            "kin-embed-refusal.txt": VALIDATOR_EMBED_REFUSAL,
            "kin-embedded-status.json": {
                "schema": "kin.status.v3",
                "embedding_coverage": {
                    "state": "unobserved",
                    "reason": "no_vector_index_attached",
                },
            },
            "kin-embedded-health.json": copy.deepcopy(embedded_report),
            "kin-embedded-doctor.json": copy.deepcopy(embedded_report),
            "kin-semantic-search.json": [
                {"name": "hello", "file": "probe.py"}
            ],
            "kin-semantic-locate.json": {
                "semantic_coverage": {
                    "supported": True,
                    "complete": True,
                    "embedding_state": "not_used",
                },
                "files": [{"path": "probe.py"}],
            },
        },
        validator_unix_mcp_home_fixture(),
        {"RUNNER_OS": "Linux"},
    )


def fixture_with_json_value(
    fixture: dict[str, object],
    path: str,
    keys: tuple[str | int, ...],
    value: object,
) -> dict[str, object]:
    """Deep-copy a fixture and replace one nested JSON value."""

    mutated = copy.deepcopy(fixture)
    cursor: object = mutated[path]
    for key in keys[:-1]:
        cursor = cursor[key]  # type: ignore[index]
    cursor[keys[-1]] = value  # type: ignore[index]
    return mutated



def fixture_with_derived_aggregate(
    fixture: dict[str, object], path: str
) -> dict[str, object]:
    """Re-derive one report's aggregate from the checks it now carries.

    Every arm that mutates a check and still expects the fixture accepted has to
    call this. Setting `healthy` by hand beside a mutated check is how a probe
    builds a report the product could not emit, and a validator that rejects it
    is right for the wrong reason.
    """

    mutated = copy.deepcopy(fixture)
    report = mutated[path]
    statuses = {
        check["id"]: check["status"]  # type: ignore[index]
        for check in report["checks"]  # type: ignore[index]
    }
    verdict = health_join_verdict(statuses)
    report["healthy"] = verdict == "ready"  # type: ignore[index]
    report["verdict"] = verdict  # type: ignore[index]
    return mutated


def fixture_without_file(
    fixture: dict[str, object], path: str
) -> dict[str, object]:
    mutated = copy.deepcopy(fixture)
    del mutated[path]
    return mutated


def fixture_without_json_key(
    fixture: dict[str, object],
    path: str,
    keys: tuple[str | int, ...],
) -> dict[str, object]:
    """Deep-copy a fixture and remove one nested JSON key."""

    mutated = copy.deepcopy(fixture)
    cursor: object = mutated[path]
    for key in keys[:-1]:
        cursor = cursor[key]  # type: ignore[index]
    del cursor[keys[-1]]  # type: ignore[index]
    return mutated


def fixture_with_check_status(
    fixture: dict[str, object],
    path: str,
    check_id: str,
    status: str,
) -> dict[str, object]:
    """Deep-copy a health fixture and change exactly one named check."""

    mutated = copy.deepcopy(fixture)
    report = mutated[path]
    checks = report["checks"]  # type: ignore[index]
    matches = [check for check in checks if check["id"] == check_id]
    if len(matches) != 1:
        raise AssertionError(
            f"validator fixture must contain one {check_id!r} check in {path}: "
            f"{matches}"
        )
    matches[0]["status"] = status
    return mutated


def fixture_with_extra_check(
    fixture: dict[str, object],
    path: str,
    check_id: str,
    status: str,
) -> dict[str, object]:
    """Deep-copy a health fixture and append one unexpected check."""

    mutated = copy.deepcopy(fixture)
    report = mutated[path]
    report["checks"].append(  # type: ignore[index,union-attr]
        {"id": check_id, "status": status}
    )
    return mutated


def fixture_with_duplicate_check(
    fixture: dict[str, object],
    path: str,
    check_id: str,
    status: str,
) -> dict[str, object]:
    """Insert a contradictory check before the authoritative fixture entry."""

    mutated = copy.deepcopy(fixture)
    report = mutated[path]
    checks = report["checks"]  # type: ignore[index]
    indexes = [index for index, check in enumerate(checks) if check["id"] == check_id]
    if len(indexes) != 1:
        raise AssertionError(
            f"validator fixture must contain one {check_id!r} check in {path}: "
            f"{indexes}"
        )
    checks.insert(indexes[0], {"id": check_id, "status": status})
    return mutated


def wrong_required_check_status(expected: str) -> str:
    """Choose a mismatch that preserves generic readiness/failure semantics."""

    if expected == "healthy":
        return "unsupported"
    if expected == "unsupported":
        return "healthy"
    if expected == "missing":
        return "misconfigured"
    raise AssertionError(f"no isolated required-check mutation for {expected!r}")


def assert_windows_node_validator_behavior(step: str) -> None:
    """Behaviorally pin every substantive Windows validator obligation."""

    proof, home, environment = windows_node_validator_fixture()
    label = "repo-free Windows install proof"
    assert_node_validator_accepts_fixture(step, label, proof, home, environment)

    def reject(
        case: str,
        invalid_proof: dict[str, object] | None = None,
        invalid_home: dict[str, object] | None = None,
    ) -> None:
        assert_node_validator_rejects_fixture(
            step,
            f"{label} ({case})",
            invalid_proof if invalid_proof is not None else proof,
            invalid_home if invalid_home is not None else home,
            environment,
        )

    # Every proof and home input is independently load-bearing on the valid path.
    for path in proof:
        reject(f"missing {path}", fixture_without_file(proof, path))
    for path in home:
        reject(f"missing home/{path}", invalid_home=fixture_without_file(home, path))

    for case, path, keys, value in (
        (
            "installed commit mismatch",
            "kin-windows-bench-meta.json",
            ("kin_commit",),
            "c" * 40,
        ),
        (
            "dirty installed build",
            "kin-windows-bench-meta.json",
            ("kin_dirty",),
            True,
        ),
        (
            "unknown installed source",
            "kin-windows-bench-meta.json",
            ("kin_source_known",),
            False,
        ),
        (
            "lock provenance mismatch",
            "kin-windows-bench-meta.json",
            ("dependency_provenance",),
            "d" * 64,
        ),
        (
            "vector feature disabled",
            "kin-windows-bench-meta.json",
            ("embeddings", "vector_enabled"),
            False,
        ),
        (
            "embedding feature disabled",
            "kin-windows-bench-meta.json",
            ("embeddings", "embeddings_enabled"),
            False,
        ),
        (
            "local engine unexpectedly enabled",
            "kin-windows-bench-meta.json",
            ("embeddings", "local_inference_enabled"),
            True,
        ),
        (
            "external support absent",
            "kin-windows-bench-meta.json",
            ("embeddings", "external_embeddings_enabled"),
            False,
        ),
        (
            "model work selected by default",
            "kin-windows-bench-meta.json",
            ("embeddings", "provider"),
            "external",
        ),
        # Metal stays a negative control in the same direction: it is the
        # macOS-only compiled backend, so a Windows archive claiming it is
        # still the defect this rejects.
        (
            "Metal feature enabled",
            "kin-windows-bench-meta.json",
            ("embeddings", "metal_enabled"),
            True,
        ),
        (
            "registry authority falsely healthy",
            "kin-windows-registry-authority.json",
            ("checks", 0, "state"),
            "healthy",
        ),
    ):
        reject(case, fixture_with_json_value(proof, path, keys, value))

    reject(
        "registry authority reports more than one capability",
        fixture_with_json_value(
            proof,
            "kin-windows-registry-authority.json",
            ("checks",),
            [{"state": "unsupported"}, {"state": "unsupported"}],
        ),
    )

    # Every named repo-free posture is checked independently in both reports.
    # The wrong value deliberately preserves the generic aggregate and hard-
    # failure predicates, leaving only the required-map comparison able to
    # reject it.
    for report_path in ("kin-windows-health.json", "kin-windows-doctor.json"):
        for check_id, expected in WINDOWS_REQUIRED_VALIDATOR_CHECKS.items():
            wrong = wrong_required_check_status(expected)
            reject(
                f"{report_path} required {check_id}={wrong}",
                fixture_with_check_status(proof, report_path, check_id, wrong),
            )
        # A retrieval profile that is genuinely degraded, meaning levers off on
        # a configuration this install chose rather than levers it has never
        # had, is not tolerated by the repo-free posture. This is the arm that
        # keeps the accepted `unsupported` above from being a blanket pass on
        # anything this check reports.
        reject(
            f"{report_path} degraded retrieval profile",
            fixture_with_check_status(proof, report_path, "retrieval_profile", "stale"),
        )
        # The first-run pending tolerance is one check and one status, not a
        # standing pass for `pending`. These two arms are what say so: the
        # named check may not drift to another non-healthy status, and no other
        # check may borrow the tolerance by going pending itself. Both preserve
        # the aggregate and hard-failure predicates, so only the tolerance
        # sweep can reject them.
        reject(
            f"{report_path} embedding model drifts off pending",
            fixture_with_check_status(proof, report_path, "embedding_model", "stale"),
        )
        reject(
            f"{report_path} a second check borrows the pending tolerance",
            fixture_with_check_status(
                proof, report_path, "retrieval_profile", "pending"
            ),
        )
        reject(
            f"{report_path} contradictory duplicate check",
            fixture_with_duplicate_check(
                proof, report_path, "kin_binary", "unsupported"
            ),
        )
        # The overclaim itself, in the shape it shipped. A fresh repo-free
        # install carries a pending row and a degraded row, so its honest
        # aggregate is `healthy: false` with `verdict: needs_attention`. The
        # v0.6.1 Windows binary emitted `true` over exactly those rows; the
        # first arm is that report and it must be refused. The second is the
        # aggregate agreeing while the verdict does not, which no build should
        # be able to emit and which the validator must catch on its own rather
        # than by inference from the boolean.
        reject(
            f"{report_path} inconsistent healthy aggregate",
            fixture_with_json_value(proof, report_path, ("healthy",), True),
        )
        reject(
            f"{report_path} inconsistent verdict",
            fixture_with_json_value(proof, report_path, ("verdict",), "ready"),
        )
        reject(
            f"{report_path} verdict absent, as older report bytes emit",
            fixture_without_json_key(proof, report_path, ("verdict",)),
        )
        reject(
            f"{report_path} unexpected hard failure",
            fixture_with_extra_check(proof, report_path, "unexpected", "missing"),
        )
        for repo_bound_id in (
            "mcp_client_codex",
            "mcp_client_antigravity",
            "mcp_client_antigravity_workspace",
        ):
            reject(
                f"{report_path} repo-bound {repo_bound_id} writer appears",
                fixture_with_extra_check(
                    proof, report_path, repo_bound_id, "healthy"
                ),
            )

    for config_path in home:
        reject(
            f"{config_path} MCP command missing",
            invalid_home=fixture_without_json_key(
                home, config_path, ("mcpServers", "kin", "command")
            ),
        )
        reject(
            f"{config_path} MCP command drift",
            invalid_home=fixture_with_json_value(
                home,
                config_path,
                ("mcpServers", "kin", "command"),
                "/wrong/kin",
            ),
        )
        # Named as its own class rather than left to the drift case
        # above. `kin setup` recorded the MCP command by joining onto whatever
        # `KIN_HOME` held, and MSYS bash hands it a forward-slashed `$HOME`, so
        # a Windows install wrote `C:/Users/u/.kin\bin\kin.exe` against an
        # installed launcher of `C:\Users\u\.kin\bin\kin.exe`. Windows opens
        # both, so only a byte comparison against the installed launcher can
        # tell them apart, and this arm is what keeps that able to fail.
        reject(
            f"{config_path} MCP command mixes path separators",
            invalid_home=fixture_with_json_value(
                home,
                config_path,
                ("mcpServers", "kin", "command"),
                f"{VALIDATOR_HOME}/.kin\\bin\\kin.exe",
            ),
        )
        reject(
            f"{config_path} MCP args drift",
            invalid_home=fixture_with_json_value(
                home,
                config_path,
                ("mcpServers", "kin", "args"),
                ["mcp", "start", "--repo", "."],
            ),
        )
        reject(
            f"{config_path} MCP profile drift",
            invalid_home=fixture_with_json_value(
                home,
                config_path,
                ("mcpServers", "kin", "env", "KIN_MCP_TOOL_PROFILE"),
                "full",
            ),
        )
        reject(
            f"{config_path} MCP profile swapped for the other agent profile",
            invalid_home=fixture_with_json_value(
                home,
                config_path,
                ("mcpServers", "kin", "env", "KIN_MCP_TOOL_PROFILE"),
                swapped_agent_profile(home, config_path),
            ),
        )

    for forbidden_path in (
        ".gemini/config/mcp_config.json",
        ".gemini/antigravity-ide/mcp_config.json",
    ):
        unexpected_home = copy.deepcopy(home)
        unexpected_home[forbidden_path] = validator_mcp_config(
            validator_mcp_entry(
                f"{VALIDATOR_HOME}/.kin/bin/kin.exe", profile=ROUTED_PROFILE
            )
        )
        reject(
            f"repo-free Windows wrote {forbidden_path}",
            invalid_home=unexpected_home,
        )


def assert_unix_node_validator_behavior(step: str) -> None:
    """Behaviorally pin every substantive Unix validator obligation."""

    proof, home, environment = unix_node_validator_fixture()
    label = "released-byte Unix install proof"
    assert_node_validator_accepts_fixture(step, label, proof, home, environment)

    # A new archive carries no external VFS pair, so its report must mark the
    # projection unsupported. Already-published archives that carry the complete
    # pair still set this mode from their attested inventory and must require the
    # historical healthy proof. These two fixtures make both arms executable.
    legacy_proof = copy.deepcopy(proof)
    legacy_proof["../legacy-vfs-mode.txt"] = "true"
    for report_path in ("kin-health.json", "kin-doctor.json"):
        legacy_proof = fixture_with_check_status(
            legacy_proof, report_path, "vfs_projection", "healthy"
        )
        legacy_proof = fixture_with_derived_aggregate(legacy_proof, report_path)
    assert_node_validator_accepts_fixture(
        step, f"{label} (legacy VFS archive)", legacy_proof, home, environment
    )

    def reject(
        case: str,
        invalid_proof: dict[str, object] | None = None,
        invalid_home: dict[str, object] | None = None,
    ) -> None:
        assert_node_validator_rejects_fixture(
            step,
            f"{label} ({case})",
            invalid_proof if invalid_proof is not None else proof,
            invalid_home if invalid_home is not None else home,
            environment,
        )

    # Removing each input independently proves the complete success path reads it.
    for path in proof:
        reject(f"missing {path}", fixture_without_file(proof, path))
    for path in home:
        reject(f"missing home/{path}", invalid_home=fixture_without_file(home, path))

    malformed_commit = copy.deepcopy(proof)
    malformed_commit["../expected-commit.txt"] = "bad"
    malformed_commit = fixture_with_json_value(
        malformed_commit, "kin-build-meta.json", ("kin_commit",), "bad"
    )
    malformed_commit = fixture_with_json_value(
        malformed_commit, "kin-daemon-health.json", ("build", "sha"), "bad"
    )
    reject("matching but malformed build commits", malformed_commit)

    malformed_lock = copy.deepcopy(proof)
    malformed_lock["../expected-lock-sha.txt"] = "bad"
    malformed_lock = fixture_with_json_value(
        malformed_lock, "kin-build-meta.json", ("dependency_provenance",), "bad"
    )
    malformed_lock = fixture_with_json_value(
        malformed_lock,
        "kin-daemon-health.json",
        ("build", "dependency_provenance"),
        "bad",
    )
    reject("matching but malformed lock provenance", malformed_lock)

    for case, path, keys, value in (
        (
            "CLI schema drift",
            "kin-build-meta.json",
            ("schema",),
            "kin.bench-meta.v1",
        ),
        (
            "CLI commit mismatch",
            "kin-build-meta.json",
            ("kin_commit",),
            "c" * 40,
        ),
        (
            "CLI dirty build",
            "kin-build-meta.json",
            ("kin_dirty",),
            True,
        ),
        (
            "CLI unknown source",
            "kin-build-meta.json",
            ("kin_source_known",),
            False,
        ),
        (
            "CLI lock mismatch",
            "kin-build-meta.json",
            ("dependency_provenance",),
            "d" * 64,
        ),
        (
            "daemon commit mismatch",
            "kin-daemon-health.json",
            ("build", "sha"),
            "c" * 40,
        ),
        (
            "daemon dirty build",
            "kin-daemon-health.json",
            ("build", "dirty"),
            True,
        ),
        (
            "daemon unknown source",
            "kin-daemon-health.json",
            ("build", "source_known"),
            False,
        ),
        (
            "daemon lock mismatch",
            "kin-daemon-health.json",
            ("build", "dependency_provenance"),
            "d" * 64,
        ),
        (
            "status schema drift",
            "kin-status.json",
            ("schema",),
            "kin.status.v2",
        ),
        (
            "vector feature absent",
            "kin-build-meta.json",
            ("embeddings", "vector_enabled"),
            False,
        ),
        (
            "embedding feature absent",
            "kin-build-meta.json",
            ("embeddings", "embeddings_enabled"),
            False,
        ),
        (
            "local engine unexpectedly enabled",
            "kin-build-meta.json",
            ("embeddings", "local_inference_enabled"),
            True,
        ),
        (
            "external support absent",
            "kin-build-meta.json",
            ("embeddings", "external_embeddings_enabled"),
            False,
        ),
        (
            "model work selected by default",
            "kin-build-meta.json",
            ("embeddings", "provider"),
            "external",
        ),
        (
            "lexical search misses entity",
            "kin-search.json",
            (0, "name"),
            "other",
        ),
        (
            "locate misses artifact",
            "kin-locate.json",
            ("files", 0, "path"),
            "other.py",
        ),
        (
            "embedded status schema drift",
            "kin-embedded-status.json",
            ("schema",),
            "kin.status.v2",
        ),
        (
            "an archive that cannot embed reports indexed vectors",
            "kin-embedded-status.json",
            ("embedding_coverage",),
            {
                "state": "observed",
                "source": "live_query_graph",
                "indexed": 2,
                "pending": 0,
                "total": 2,
            },
        ),
        (
            "embedded coverage unobserved without a reason",
            "kin-embedded-status.json",
            ("embedding_coverage",),
            {"state": "unobserved"},
        ),
        (
            "semantic search misses entity",
            "kin-semantic-search.json",
            (0, "name"),
            "other",
        ),
        (
            "locate claims it used embeddings",
            "kin-semantic-locate.json",
            ("semantic_coverage", "embedding_state"),
            "used",
        ),
        (
            "semantic locate misses artifact",
            "kin-semantic-locate.json",
            ("files", 0, "path"),
            "other.py",
        ),
    ):
        reject(case, fixture_with_json_value(proof, path, keys, value))

    # The refusal has to be the one that names what is missing. A server error,
    # or a refusal that names nothing, sends the reader nowhere.
    for case, refusal in (
        ("kin embed answered with a server error", "Error: daemon embed failed: HTTP 500\n"),
        (
            "kin embed refused without naming the missing model",
            "Error: kin embed cannot index anything here\n",
        ),
    ):
        invalid_refusal = copy.deepcopy(proof)
        invalid_refusal["kin-embed-refusal.txt"] = refusal
        reject(case, invalid_refusal)

    reject(
        "pre-embed coverage missing",
        fixture_without_json_key(
            proof, "kin-status.json", ("embedding_coverage",)
        ),
    )
    reject(
        "pre-embed coverage malformed",
        fixture_with_json_value(
            proof, "kin-status.json", ("embedding_coverage",), []
        ),
    )
    reject(
        "pre-embed coverage has unknown state",
        fixture_with_json_value(
            proof,
            "kin-status.json",
            ("embedding_coverage", "state"),
            "unknown",
        ),
    )
    reject(
        "unobserved coverage has no reason",
        fixture_with_json_value(
            proof,
            "kin-status.json",
            ("embedding_coverage", "reason"),
            "",
        ),
    )
    reject(
        "unobserved coverage leaks counts",
        fixture_with_json_value(
            proof,
            "kin-status.json",
            ("embedding_coverage", "indexed"),
            0,
        ),
    )
    reject(
        "unobserved coverage leaks source",
        fixture_with_json_value(
            proof,
            "kin-status.json",
            ("embedding_coverage", "source"),
            "live_query_graph",
        ),
    )
    observed_pre_embed = fixture_with_json_value(
        proof,
        "kin-status.json",
        ("embedding_coverage",),
        {
            "state": "observed",
            "source": "live_query_graph",
            "indexed": 0,
            "pending": 2,
            "total": 2,
        },
    )
    assert_node_validator_accepts_fixture(
        step,
        f"{label} (observed pre-embed coverage)",
        observed_pre_embed,
        home,
        environment,
    )
    for case, keys, value in (
        ("observed coverage wrong source", ("source",), "snapshot"),
        ("observed coverage negative count", ("indexed",), -1),
        ("observed coverage carries reason", ("reason",), "stale"),
        ("observed coverage indexed exceeds total", ("indexed",), 3),
        ("observed coverage pending undercounts gap", ("pending",), 1),
    ):
        reject(
            case,
            fixture_with_json_value(
                observed_pre_embed,
                "kin-status.json",
                ("embedding_coverage", *keys),
                value,
            ),
        )

    for report_path in ("kin-health.json", "kin-doctor.json"):
        for check_id, expected in UNIX_REQUIRED_VALIDATOR_CHECKS.items():
            wrong = wrong_required_check_status(expected)
            reject(
                f"{report_path} required {check_id}={wrong}",
                fixture_with_check_status(proof, report_path, check_id, wrong),
            )
        # The pre-embed tolerance accepts an aggregate held false by pending
        # embeddings only while every OTHER check is healthy or unsupported, so
        # a genuinely degraded retrieval profile still closes it. That is what
        # refused v0.5.7 on this leg, and it is why the accepted `unsupported`
        # above is a real answer rather than a hole in the fixture.
        reject(
            f"{report_path} degraded retrieval profile",
            fixture_with_check_status(proof, report_path, "retrieval_profile", "stale"),
        )
        reject(
            f"{report_path} contradictory duplicate check",
            fixture_with_duplicate_check(
                proof, report_path, "kin_binary", "unsupported"
            ),
        )
        reject(
            f"{report_path} inconsistent healthy aggregate",
            fixture_with_json_value(proof, report_path, ("healthy",), True),
        )

        reject(
            f"{report_path} inconsistent verdict",
            fixture_with_json_value(proof, report_path, ("verdict",), "ready"),
        )
        reject(
            f"{report_path} verdict absent, as older report bytes emit",
            fixture_without_json_key(proof, report_path, ("verdict",)),
        )
        # A row that needs attention and is not one this posture named. The
        # aggregate is re-derived, so the fixture is a report the product could
        # actually emit and only the tolerance sweep can refuse it.
        reject(
            f"{report_path} an unnamed row needs attention",
            fixture_with_derived_aggregate(
                fixture_with_extra_check(
                    proof, report_path, "kinlab_connect", "degraded"
                ),
                report_path,
            ),
        )

        # Nothing embeds on the engine-free archive, so readiness that claims
        # a filled index, or a first pass under way, is a promise the host
        # never keeps.
        for claimed in ("healthy", "pending"):
            reject(
                f"{report_path} semantic readiness {claimed} on an archive that cannot embed",
                fixture_with_derived_aggregate(
                    fixture_with_check_status(
                        proof, report_path, "semantic_query_readiness", claimed
                    ),
                    report_path,
                ),
            )

    for report_path in ("kin-embedded-health.json", "kin-embedded-doctor.json"):
        # The post-embed capture carries `memory_floor: degraded`, so its honest
        # aggregate is already false. The overclaim is the true one.
        reject(
            f"{report_path} inconsistent healthy aggregate",
            fixture_with_json_value(proof, report_path, ("healthy",), True),
        )
        reject(
            f"{report_path} verdict absent, as older report bytes emit",
            fixture_without_json_key(proof, report_path, ("verdict",)),
        )
        reject(
            f"{report_path} an unnamed row needs attention after the embed",
            fixture_with_derived_aggregate(
                fixture_with_extra_check(
                    proof, report_path, "kinlab_connect", "pending"
                ),
                report_path,
            ),
        )
        reject(
            f"{report_path} contradictory duplicate check",
            fixture_with_duplicate_check(
                proof, report_path, "semantic_query_readiness", "healthy"
            ),
        )
        for claimed in ("healthy", "pending"):
            reject(
                f"{report_path} semantic readiness {claimed} after a refused embed",
                fixture_with_check_status(
                    proof, report_path, "semantic_query_readiness", claimed
                ),
            )

    for report_path in (
        "kin-claude-fallback-health.json",
        "kin-claude-fallback-doctor.json",
    ):
        reject(
            f"{report_path} Claude fallback not healthy",
            fixture_with_check_status(
                proof, report_path, "mcp_client_claude", "unsupported"
            ),
        )
        reject(
            f"{report_path} contradictory duplicate check",
            fixture_with_duplicate_check(
                proof, report_path, "mcp_client_claude", "misconfigured"
            ),
        )
        reject(
            f"{report_path} inconsistent healthy aggregate",
            fixture_with_json_value(proof, report_path, ("healthy",), False),
        )
        reject(
            f"{report_path} leaks a non-Claude global client",
            fixture_with_extra_check(
                proof, report_path, "mcp_client_cursor", "healthy"
            ),
        )

    for config_path in home:
        reject(
            f"{config_path} MCP command missing",
            invalid_home=fixture_without_json_key(
                home, config_path, ("mcpServers", "kin", "command")
            ),
        )
        reject(
            f"{config_path} MCP command drift",
            invalid_home=fixture_with_json_value(
                home,
                config_path,
                ("mcpServers", "kin", "command"),
                "/wrong/kin",
            ),
        )
        reject(
            f"{config_path} MCP args drift",
            invalid_home=fixture_with_json_value(
                home,
                config_path,
                ("mcpServers", "kin", "args"),
                ["mcp", "start", "--repo", "."],
            ),
        )
        reject(
            f"{config_path} MCP profile drift",
            invalid_home=fixture_with_json_value(
                home,
                config_path,
                ("mcpServers", "kin", "env", "KIN_MCP_TOOL_PROFILE"),
                "full",
            ),
        )
        reject(
            f"{config_path} MCP profile swapped for the other agent profile",
            invalid_home=fixture_with_json_value(
                home,
                config_path,
                ("mcpServers", "kin", "env", "KIN_MCP_TOOL_PROFILE"),
                swapped_agent_profile(home, config_path),
            ),
        )
        entry = home[config_path]["mcpServers"]["kin"]  # type: ignore[index]
        if "cwd" in entry:
            reject(
                f"{config_path} MCP cwd drift",
                invalid_home=fixture_with_json_value(
                    home,
                    config_path,
                    ("mcpServers", "kin", "cwd"),
                    "/wrong/repo",
                ),
            )

    for config_path in (
        "kin-claude-fallback-config.json",
        "kin-codex-config.json",
        ".agents/mcp_config.json",
    ):
        reject(
            f"{config_path} MCP command missing",
            fixture_without_json_key(
                proof, config_path, ("mcpServers", "kin", "command")
            ),
        )
        reject(
            f"{config_path} MCP command drift",
            fixture_with_json_value(
                proof,
                config_path,
                ("mcpServers", "kin", "command"),
                "/wrong/kin",
            ),
        )
        reject(
            f"{config_path} MCP args drift",
            fixture_with_json_value(
                proof,
                config_path,
                ("mcpServers", "kin", "args"),
                ["mcp", "start", "--repo", "/wrong/repo"],
            ),
        )
        reject(
            f"{config_path} MCP profile drift",
            fixture_with_json_value(
                proof,
                config_path,
                ("mcpServers", "kin", "env", "KIN_MCP_TOOL_PROFILE"),
                "full",
            ),
        )
        reject(
            f"{config_path} MCP profile swapped for the other agent profile",
            fixture_with_json_value(
                proof,
                config_path,
                ("mcpServers", "kin", "env", "KIN_MCP_TOOL_PROFILE"),
                swapped_agent_profile(proof, config_path),
            ),
        )
        entry = proof[config_path]["mcpServers"]["kin"]  # type: ignore[index]
        if "cwd" in entry:
            reject(
                f"{config_path} MCP cwd drift",
                fixture_with_json_value(
                    proof,
                    config_path,
                    ("mcpServers", "kin", "cwd"),
                    "/wrong/repo",
                ),
            )

    reject(
        "Antigravity legacy top-level policy drift",
        invalid_home=fixture_with_json_value(
            home,
            ".gemini/antigravity-ide/mcp_config.json",
            ("userPolicy",),
            "lost",
        ),
    )
    reject(
        "Antigravity legacy entry policy drift",
        invalid_home=fixture_with_json_value(
            home,
            ".gemini/antigravity-ide/mcp_config.json",
            ("mcpServers", "kin", "env", "USER_POLICY"),
            "lost",
        ),
    )


def assert_node_validator_rejects_missing_proof(step: str, label: str) -> None:
    """Execute the shipped validator and require incomplete proof trees to fail.

    Token checks explain which contract drifted, but cannot prove those tokens
    remain reachable. Running the extracted program against a deliberately
    absent evidence tree catches whole-validator no-ops. Running it again after
    seeding only the first expected-commit input gets beyond that first read and
    catches a false branch or early exit around every substantive validation.
    Both controls fail for runtime behavior rather than one enumerated syntax.
    """

    script = node_heredoc_body(step, label)
    try:
        syntax = subprocess.run(
            ["node", "--check", "-"],
            input=script,
            text=True,
            capture_output=True,
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise AssertionError(f"{label} could not parse under Node: {error}") from error
    if syntax.returncode != 0:
        raise AssertionError(
            f"{label} is not valid Node source: {syntax.stderr.strip()}"
        )
    expected_commit_reads = re.findall(
        r'const expectedCommit = fs\.readFileSync\("([^"]+)", "utf8"\)\.trim\(\);',
        script,
    )
    if len(expected_commit_reads) != 1:
        raise AssertionError(
            f"{label} must read exactly one expected-commit proof input: "
            f"{expected_commit_reads}"
        )

    with tempfile.TemporaryDirectory(prefix="kin-proof-validator-") as temporary:
        temporary_root = Path(temporary).resolve()
        proof_dir = temporary_root / "proof"
        proof_dir.mkdir()

        def require_runtime_rejection(scenario: str) -> None:
            try:
                result = subprocess.run(
                    ["node", "-"],
                    input=script,
                    cwd=proof_dir,
                    text=True,
                    capture_output=True,
                    timeout=10,
                    check=False,
                )
            except (OSError, subprocess.TimeoutExpired) as error:
                raise AssertionError(
                    f"{label} could not execute under Node for {scenario}: {error}"
                ) from error
            if result.returncode == 0:
                raise AssertionError(
                    f"{label} accepted {scenario}; the validator is not "
                    "runtime-falsifiable"
                )

        require_runtime_rejection("an empty proof tree")

        expected_commit_path = (proof_dir / expected_commit_reads[0]).resolve()
        try:
            expected_commit_path.relative_to(temporary_root)
        except ValueError as error:
            raise AssertionError(
                f"{label} expected-commit input escapes its isolated proof tree: "
                f"{expected_commit_reads[0]!r}"
            ) from error
        expected_commit_path.parent.mkdir(parents=True, exist_ok=True)
        expected_commit_path.write_text("0" * 40 + "\n", encoding="utf-8")
        require_runtime_rejection("an expected-commit-only proof tree")


PROOF_CAPTURE_DIRECTORY = 'captures="$RUNNER_TEMP/kin-proof-captures"'
PROOF_CAPTURE_DIRECTORY_CREATE = 'mkdir -p "$captures"'


def assert_install_proof_init_log_authority(first_run: str) -> None:
    """Keep the install proof's own report files out of the worktree kin init admits.

    kin init proves the Git worktree exactly, repeats that proof immediately
    before publication, and repeats it again afterwards, refusing when the two
    differ. A report file written into that worktree therefore breaks admission
    twice over: it is an untracked non-ignored path, and an ignored one would
    still change size between the repeats. The same worktree stays under a
    watcher that admits new non-ignored files for the rest of the job, so the
    constraint outlives init and every later capture is held to it by
    `assert_install_proof_captures_stay_out_of_the_admitted_tree`.

    Pinning the capture lines is not enough on its own, because the property is
    about the whole span between entering that worktree and admitting it. Two
    mutations leave every pinned line in place and still break admission: an
    unrelated write into the worktree before init, and a second init that an
    exact-prefix count cannot see. So the span is closed-form and an `init` this
    function does not recognize is refused by name.
    """

    active = active_lines(first_run)
    admission = 'kin init > "$captures/kin-init.txt" 2>&1 || init_status=$?'
    bootstrap = (
        "git init -q && git config user.email ci@firelock.ai && git config user.name ci"
    )
    entry = "mkdir -p kin-install-proof && cd kin-install-proof"
    propagate = 'if [ "$init_status" -ne 0 ]; then exit "$init_status"; fi'
    # An `init` token anywhere in an active line, not a prefix of one:
    # `(cd sub && kin init)`, `eval kin init`, and `"$KIN" init` all admit a
    # worktree and none of them starts with `kin init`.
    init_token = re.compile(r"(?<![\w./-])init(?![\w-])")
    admissions = [
        line for line in active if init_token.search(line) and line != bootstrap
    ]
    if len(admissions) != 1:
        raise AssertionError(
            "install proof must invoke kin init exactly once in the first-run "
            f"step: {admissions}"
        )
    if admissions[0] != admission:
        raise AssertionError(
            "kin init must write its log outside the worktree it admits rather "
            f"than through a relative redirect or pipe: {admissions[0]}"
        )
    for policy in (
        entry,
        PROOF_CAPTURE_DIRECTORY,
        PROOF_CAPTURE_DIRECTORY_CREATE,
        propagate,
    ):
        if policy not in active:
            raise AssertionError(
                f"install-proof init log capture lost an active line: {policy}"
            )
    # kin's own exit status is propagated after admission. Reordering the
    # propagation above init leaves every pinned line present and reports the
    # status of whatever ran before it instead.
    order = [active.index(step) for step in (admission, propagate)]
    if order != sorted(order):
        raise AssertionError(
            "install-proof init log capture must admit, then propagate kin's "
            f"own exit status; found line order {order}"
        )
    # Nothing may write into the worktree between entering it and admitting it.
    # A line-presence check cannot see an inserted write, and an inserted write
    # is byte-for-byte the class that broke the v0.4.5 release proof.
    prelude = active[active.index(entry) + 1 : order[0]]
    committed_prelude = [
        bootstrap,
        "printf 'def hello():\\n    return 42\\n' > probe.py",
        'git add -A && git commit -qm "probe"',
        PROOF_CAPTURE_DIRECTORY,
        PROOF_CAPTURE_DIRECTORY_CREATE,
        "init_status=0",
    ]
    if prelude != committed_prelude:
        raise AssertionError(
            "only the committed Git bootstrap may run between entering the "
            f"worktree kin init admits and admitting it; found {prelude}"
        )


def assert_install_proof_captures_stay_out_of_the_admitted_tree(
    proof_steps: dict[str, str], restore: str, restore_position: tuple[int, int, int]
) -> None:
    """Keep every proof report out of the admitted tree until nothing measures it.

    The worktree the proof admits stays under a watcher for the rest of the job,
    so a report written into it becomes semantic corpus and the store the
    assertions read grows while they read it. That is not a hypothesis: a leg
    read complete coverage in health and 13 of 18 in locate one second later,
    and it fenced a release.

    A report is therefore captured under RUNNER_TEMP and restored afterwards. A
    presence check on the restore step cannot see the mutation that matters,
    which is one capture reverting to a relative path, so every report-shaped
    redirect, tee, and Node write in these steps is examined instead and a
    relative target is refused by name. The restore itself has to run after the
    last step that reads the store, before the validator that reads the files,
    and on failure as well, or a leg that dies early hands over no evidence at
    all.
    """

    # A bare filename with a report extension and no directory part: exactly
    # the shape a write into the admitted tree takes, and one no capture under
    # `"$captures/..."` can wear, so a match is the defect rather than a
    # candidate for it.
    in_tree_report = re.compile(r"[\w.\-]+\.(?:json|jsonl|txt|log)")
    writes = (
        re.compile(r"(?:^|\s)\d?>>?\s*(\S+)"),
        re.compile(r"\|\s*tee\s+(\S+)"),
        re.compile(r"(?:write|append)FileSync\(\s*\"([^\"]+)\""),
    )
    for step_name, step_source in proof_steps.items():
        for line in active_lines(step_source):
            for pattern in writes:
                for raw in pattern.findall(line):
                    if in_tree_report.fullmatch(raw.strip('"')):
                        raise AssertionError(
                            f"{step_name} writes a proof report into the "
                            f"admitted tree rather than the capture "
                            f"directory: {line}"
                        )

    restore_active = active_lines(restore)
    for policy in (
        PROOF_CAPTURE_DIRECTORY,
        'destination="$PWD/kin-install-proof"',
        'cp "$captures/$capture" "$destination/$capture"',
        'done < <(ls -1 "$captures")',
    ):
        if policy not in restore_active:
            raise AssertionError(
                f"install-proof capture restore lost an active line: {policy}"
            )
    if "if: always()" not in restore_active:
        raise AssertionError(
            "install-proof capture restore must run on failure too, or a leg "
            "that dies early uploads no captured evidence"
        )
    last_store_read, restore_start, validation_start = restore_position
    if not last_store_read < restore_start < validation_start:
        raise AssertionError(
            "install-proof captures must be restored after the last step that "
            "reads the store and before the step that reads the files; found "
            f"positions {restore_position}"
        )


def assert_install_proof_embedding_settles_before_measurement(embedding: str) -> None:
    """Require a bounded, counter-driven settle between the embed and its captures.

    An embedding pass reports on the work it took, and content that belongs in
    the corpus is admitted asynchronously around it, so coverage read straight
    afterwards can be a sample of a store still taking work on. Waiting is
    therefore mandatory, but only in the form that can fail: a sleep asserts a
    duration nobody measured, while a poll asserts the counters themselves and
    says which one never settled when its budget runs out.
    """

    active = active_lines(embedding)
    embed = 'kin embed --max-seconds 300 --json \\'
    settle = 'PROOF_CAPTURES="$captures" node <<\'NODE\''
    capture = (
        'kin status --json --wait-quiesce 60 | tee "$captures/kin-embedded-status.json"'
    )
    for policy in (embed, settle, capture):
        if policy not in active:
            raise AssertionError(
                f"install-proof embedding settle lost an active line: {policy}"
            )
    settle_body = "\n".join(active)
    for policy in (
        '> "$captures/kin-embed.json" 2> "$captures/kin-embed-refusal.txt"',
        'if [ "$embed_rc" -eq 0 ]; then',
        '*"kin embed cannot index anything here"*"no local embedding model"*) ;;',
        "if (current === previous) {",
        "process.exit(1);",
    ):
        if policy not in settle_body:
            raise AssertionError(
                "install-proof embedding settle must poll the counters to "
                f"quiescence and fail on expiry: {policy}"
            )
    order = [active.index(step) for step in (embed, settle, capture)]
    if order != sorted(order):
        raise AssertionError(
            "install-proof must embed, then settle, then capture the status it "
            f"asserts on; found line order {order}"
        )
    for line in active:
        if re.fullmatch(r"sleep [\d.]+", line):
            raise AssertionError(
                "install-proof must settle on the counters rather than on a "
                f"duration nobody measured: {line}"
            )


INSTALL_PROOF_MATRIX_PATTERN = re.compile(
    r"^include: \$\{\{ fromJSON\(inputs\.local_artifact "
    r"&& '(?P<pull_request>\[.*?\])' \|\| '(?P<release>\[.*?\])'\) \}\}$"
)


def install_proof_matrix_rows(install_job: str) -> tuple[
    list[dict[str, object]], list[dict[str, object]]
]:
    """Decode both reviewed install-proof matrices from their one expression.

    The release matrix is the default and the pull-request matrix is the
    override, and the job carries exactly one line that can produce either.
    Decoding it here is what lets the posture assertions below judge rows
    rather than YAML text, so neither list can gain a platform, lose one, or
    grow a second tolerated leg without this failing. A matrix built any other
    way (a second expression, a literal include, a computed list) is refused
    outright rather than read past.
    """

    strategy_lines = active_lines(dynamic_job_context_source(install_job))
    includes = [line for line in strategy_lines if line.startswith("include:")]
    if len(includes) != 1:
        raise AssertionError(
            "install-proof must declare exactly one matrix include line; "
            f"found {includes}"
        )
    match = INSTALL_PROOF_MATRIX_PATTERN.fullmatch(includes[0])
    if match is None:
        raise AssertionError(
            "install-proof matrix must be the reviewed release-or-pull-request "
            f"expression; found `{includes[0]}`"
        )
    try:
        release = json.loads(match.group("release"))
        pull_request = json.loads(match.group("pull_request"))
    except json.JSONDecodeError as error:
        raise AssertionError(
            f"install-proof matrix does not decode as JSON: {error}"
        ) from error
    return release, pull_request


INSTALL_PROOF_PULL_REQUEST_ARTIFACT = "install-proof-pr-binaries"
INSTALL_PROOF_PULL_REQUEST_JOBS = (
    "install-proof-pr-build",
    "install-proof-pr",
    "install-proof-pr-gate",
)


# The only event filters the pull-request install proof jobs may carry, matched
# on the whole stripped line. A set rather than a substring test, because
# `event_name != 'pull_request'` is a substring of every filter that also
# excludes something else, and the thing this assertion exists to catch is a
# heavy leg quietly leaving the merge queue. `install-proof-pr-gate` carries no
# job-level filter at all: it always reports, and decides inside its own step.
INSTALL_PROOF_ADMITTED_EVENT_FILTERS = frozenset(
    {
        "&& github.event_name != 'pull_request'",
        f"&& ({RELEASE_SUITE_EVENT_FILTER})",
    }
)
# The gate is exempt from even that. It always reports, under one stable name,
# and decides inside its own step, so any job-level event filter here publishes
# no check rather than a skipped one.
INSTALL_PROOF_PULL_REQUEST_GATE = "install-proof-pr-gate"


def assert_install_proof_runs_on_pull_requests(ci: str, install_proof: str) -> None:
    """Require the release install proof to run before a tag exists.

    v0.5.38 was versioned, built, published, and only then refused, on an
    assertion no pull-request check ran: `kin locate` had to report complete
    semantic coverage over a freshly installed probe repository, and the only
    place that ran was the public install proof, which the release workflow
    calls after publishing. The remedy is not a second copy of that assertion,
    which would drift from the one the release enforces. It is the same
    reusable proof, called from CI against binaries the pull request built.

    So this pins the three properties that make that true and keeps each one
    falsifiable. The proof must reach the reviewed reusable workflow rather
    than a lookalike; the binaries it installs must be the ones the pull
    request compiled and packaged in the release archive layout; and none of
    the three jobs may exclude the MERGE QUEUE, which is what the release mint
    reads. The semantic-coverage assertion itself is pinned last: the step
    producing its capture stays on the ordinary Unix path, so a run reaches it
    rather than skipping it as an optional extra.

    The pull-request exclusion used to be barred too, and the admission core admits
    exactly one form of it and no other. The reason it was barred has not
    changed and is not weakened: a heavy leg quietly moved back to main is how
    this class of break returns. What changed is where "before a tag" is paid.
    The proof measured 9.4 minutes on a pull request against a ten-minute
    open-to-merge bar, and it now runs in the merge queue and on every commit
    that reaches main, both of which are before any tag. `release-tag.yml`
    still refuses to mint a sha whose required checks are not green.

    The admitted form is the exact expression below and nothing else, so an
    event filter that excluded the merge queue, or excluded a push, or was
    written differently enough to mean something else, still fails here.
    """

    jobs = workflow_job_blocks(ci)
    missing = [job for job in INSTALL_PROOF_PULL_REQUEST_JOBS if job not in jobs]
    if missing:
        raise AssertionError(
            "the install proof must run at pull-request time; CI lost "
            f"{missing}"
        )
    build, call, gate = (jobs[job] for job in INSTALL_PROOF_PULL_REQUEST_JOBS)

    call_lines = active_lines(call)
    for policy in (
        "uses: ./.github/workflows/install-proof.yml",
        f"local_artifact: {INSTALL_PROOF_PULL_REQUEST_ARTIFACT}",
    ):
        if policy not in call_lines:
            raise AssertionError(
                "the pull-request install proof must call the reviewed reusable "
                f"proof with the built artifact; missing `{policy}`"
            )
    build_lines = active_lines(build)
    if any("--features" in line or "--all-features" in line for line in build_lines):
        raise AssertionError("the install proof core build must use Cargo defaults")
    for policy in (
        f"name: {INSTALL_PROOF_PULL_REQUEST_ARTIFACT}",
        "--target x86_64-unknown-linux-musl -p kin-cli -p kin-daemon",
        'tar czf "$stage/$artifact.tar.gz" -C "$stage" "$artifact"',
        "assertReleaseArchiveMemberPaths(listing, {",
        'cp scripts/install.sh "$stage/install.sh"',
    ):
        if policy not in build_lines:
            raise AssertionError(
                "the pull-request install proof must install release-layout "
                f"binaries built by this pull request; missing `{policy}`"
            )

    for job_id, job in zip(INSTALL_PROOF_PULL_REQUEST_JOBS, (build, call, gate)):
        for line in active_lines(job):
            clause = line.strip()
            is_condition = clause.startswith(("&&", "||", "if:"))
            if not is_condition or "event_name" not in clause:
                continue
            # A folded `if:` puts the closing `}}` on whichever clause happens
            # to be last, so one reviewed filter reads two ways depending on
            # clause order. Normalise that and nothing else.
            clause = clause.removesuffix("}}").strip()
            if job_id == INSTALL_PROOF_PULL_REQUEST_GATE:
                raise AssertionError(
                    f"{job_id} must not exclude an event. It is the job that "
                    "always reports, under one stable name, and is the context "
                    "to require; a filter here publishes no check at all rather "
                    f"than a skipped one: {line}"
                )
            if clause not in INSTALL_PROOF_ADMITTED_EVENT_FILTERS:
                raise AssertionError(
                    f"{job_id} carries an event filter that is not the one "
                    "reviewed for the admission core. The install proof runs in the "
                    "merge queue and on every main commit, and a filter written "
                    f"any other way silently returns it to the tag: {line}"
                )
            if "merge_group" in line or "push" in line:
                raise AssertionError(
                    f"{job_id} must run inside the merge queue and on main: "
                    "the release mint keys off the queue-proven sha and "
                    f"refuses a build that skipped this proof: {line}"
                )

    gate_lines = active_lines(gate)
    for policy in (
        'if [ "$BUILD_RESULT" != "success" ]; then',
        'if [ "$PROOF_RESULT" != "success" ]; then',
        "exit 1",
    ):
        if policy not in gate_lines:
            raise AssertionError(
                "the pull-request install proof gate must fail closed on a "
                f"proof that did not pass; missing `{policy}`"
            )

    embedding = install_proof_step(
        install_proof, "Unix embedding and semantic retrieval proof"
    )
    embedding_lines = active_lines(embedding)
    conditioned = [line for line in embedding_lines if line.startswith("if:")]
    if conditioned != ["if: runner.os != 'Windows'"]:
        raise AssertionError(
            "the semantic retrieval proof admits exactly the Unix condition, so "
            "a pull-request run reaches the assertion that refused v0.5.38; "
            f"found {conditioned}"
        )
    if (
        'kin locate hello --json --explain --max-files 5 | tee "$captures/kin-semantic-locate.json"'
        not in embedding_lines
    ):
        raise AssertionError(
            "the semantic retrieval proof lost the locate capture the "
            "capability validator reads"
        )


def assert_install_proof_gate_grades_the_merge_queue(ci: str) -> None:
    """Exercise the actual wrapper with real failed/skipped dependency outcomes.

    Every pull request, export pull requests included, passes at once: the
    proof runs on the merge queue's commit, which is what lands, and on main.
    There the gate admits only a built and proven archive.
    """
    gate = workflow_job_blocks(ci)[INSTALL_PROOF_PULL_REQUEST_GATE]
    shell = workflow_step_shell(gate, "Require the pull request install proof")
    fixtures = (
        ("ordinary PR", "pull_request", "fix/example", "false", "skipped", "skipped", 0),
        ("export PR", "pull_request", "feature/public-export-kin-fixture", "false", "skipped", "skipped", 0),
        ("queue build failed", "merge_group", "", "false", "failure", "skipped", 1),
        ("queue proof failed", "merge_group", "", "false", "success", "failure", 1),
        ("queue proof cancelled", "merge_group", "", "false", "success", "cancelled", 1),
        ("queue proof skipped", "merge_group", "", "false", "success", "skipped", 1),
        ("queue passed", "merge_group", "", "false", "success", "success", 0),
        ("main proof failed", "push", "", "false", "success", "failure", 1),
        ("main build skipped", "push", "", "false", "skipped", "skipped", 1),
    )
    for label, event, head, docs, build, proof, expected in fixtures:
        result = subprocess.run(
            ["bash", "-euo", "pipefail", "-c", shell],
            env={
                "PATH": os.defpath,
                "EVENT_NAME": event,
                "HEAD_REF": head,
                "DOCS_ONLY": docs,
                "BUILD_RESULT": build,
                "PROOF_RESULT": proof,
            },
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        if result.returncode != expected:
            raise AssertionError(
                f"install proof gate {label}: expected exit {expected}, "
                f"got {result.returncode}: {result.stdout}{result.stderr}"
            )


INSTALL_PROOF_CLI_RELEASE_EVENT_FILTER = (
    "${{ github.event_name != 'release' || inputs.release_tag || "
    "startsWith(github.event.release.tag_name, 'v') }}"
)


def assert_install_proof_cli_release_events_only(install_proof: str) -> None:
    job = workflow_job_blocks(install_proof)["install-proof"]
    guards = [value.strip() for key, value in job_top_level_mapping_fields(job) if key == "if"]
    if len(guards) != 1 or not guards[0].startswith("${{") or not guards[0].endswith("}}"):
        raise AssertionError("install-proof must carry one job-level release event guard before its matrix")
    expression = guards[0][3:-2].strip()
    cases = [
        ("CLI stable", "release", "v0.8.6", False, "", True),
        ("CLI stable published as prerelease", "release", "v0.8.7", True, "", True),
        ("CLI prerelease", "release", "v0.8.7-rc.1", True, "", True),
        ("retired archive policy remains inside proof", "release", "v0.7.19", False, "", True),
        ("Mac alpha", "release", "mac-v0.1.1-alpha.1", True, "", False),
        ("Mac stable", "release", "mac-v1.0.0", False, "", False),
        ("Mac update feed", "release", "mac-update-feed", True, "", False),
        ("empty release tag", "release", "", False, "", False),
        ("workflow dispatch latest", "workflow_dispatch", "", False, "", True),
        ("workflow dispatch exact", "workflow_dispatch", "", False, "v0.8.6", True),
        ("weekly latest", "schedule", "", False, "", True),
        ("called release push", "push", "", False, "v0.8.6", True),
        ("called merge group", "merge_group", "", False, "v0.8.6", True),
        ("called pull request", "pull_request", "", False, "v0.8.6", True),
        ("explicit call from another release event", "release", "mac-v0.1.1-alpha.1", True, "v0.8.6", True),
    ]
    payload = [{"name": name, "github": {"event_name": event, "event": {"release": {
        "tag_name": tag, "prerelease": prerelease}}}, "inputs": {"release_tag": selected},
        "expected": expected} for name, event, tag, prerelease, selected, expected in cases]
    # Run the actual job expression. startsWith follows Actions' case-insensitive
    # string comparison; caller data stays separate from the expression source.
    script = """const cases = JSON.parse(require('fs').readFileSync(0, 'utf8'));
const guard = new Function('github', 'inputs', 'startsWith', 'return Boolean(' + process.argv[1] + ')');
const startsWith = (value, prefix) => String(value ?? '').toLowerCase().startsWith(prefix.toLowerCase());
for (const c of cases) {
  const actual = guard(c.github, c.inputs, startsWith);
  if (actual !== c.expected) throw new Error('install-proof misroutes ' + c.name);
}
"""
    result = subprocess.run(["node", "-e", script, expression], input=json.dumps(payload),
        text=True, capture_output=True, timeout=10)
    if result.returncode:
        raise AssertionError(result.stdout + result.stderr)


def assert_install_proof_every_leg_gates_the_release(install_proof: str) -> None:
    """Require every install-proof matrix row to gate the release.

    The windows-latest row carried ``experimental: true`` and the job carried
    the matching ``continue-on-error`` guard, so the one platform most likely
    to break shipped without a verdict. Two defects held that waiver open and
    both are fixed on released bytes: kin#811's inherited-pipe read of
    ``.kin/daemon.port``, then a projection chooser that named
    ``projection_mode=misconfigured`` on a host where nothing was recorded,
    which killed every release from v0.5.44 through v0.5.47. v0.5.49's run
    concluded ``Public Install Proof / windows-latest`` SUCCESS against its
    own published archives, which is the condition the waiver itself named
    for its removal, so the waiver is spent.

    Tolerance is therefore rejected in every spelling it could return in: a
    job-level ``continue-on-error`` of any value, a step-level one, which
    would spare every leg rather than one, and an ``experimental`` key on any
    row of either decoded matrix, since that key is the half of the waiver a
    reader would restore first. The reviewed platform set stays pinned here
    too, so a row cannot be dropped in the same edit that would have
    tolerated it.
    """

    jobs = workflow_job_blocks(install_proof)
    install_job = jobs.get("install-proof")
    if install_job is None:
        raise AssertionError(
            "install-proof gating posture lost the install-proof job"
        )
    job_fields = job_top_level_mapping_fields(install_job)
    tolerance = [
        value.strip() for key, value in job_fields if key == "continue-on-error"
    ]
    if tolerance:
        raise AssertionError(
            "install-proof admits no job-level continue-on-error; every "
            f"matrix row gates the release; found {tolerance}"
        )
    tolerance_mentions = [
        line for line in active_lines(install_job) if "continue-on-error" in line
    ]
    if tolerance_mentions:
        raise AssertionError(
            "install-proof admits no continue-on-error at all; a step-level "
            "tolerance would spare every leg, not one; found "
            f"{tolerance_mentions}"
        )
    release_rows, pull_request_rows = install_proof_matrix_rows(install_job)
    for label, rows in (("release", release_rows), ("pull request", pull_request_rows)):
        tolerated = [row.get("os") for row in rows if "experimental" in row]
        if tolerated:
            raise AssertionError(
                f"install-proof {label} matrix rows admit no experimental "
                f"key; the Windows waiver is spent; found it under {tolerated}"
            )
    release_platforms = [row.get("os") for row in release_rows]
    expected_platforms = [
        "ubuntu-latest",
        "ubuntu-24.04-arm",
        "macos-latest",
        "macos-15-intel",
        "windows-latest",
    ]
    if release_platforms != expected_platforms:
        raise AssertionError(
            "install-proof release matrix must gate exactly "
            f"{expected_platforms}; found {release_platforms}"
        )


# The one admitted scoping for the N-1 update proof, spelled exactly so a
# condition written any other way fails rather than passing as "some filter".
UPDATE_PROOF_ADMITTED_EVENT_FILTER = (
    "${{ github.event_name == 'schedule' "
    "|| github.event_name == 'workflow_dispatch' }}"
)


def assert_update_proof_stays_off_the_release_path(install_proof: str) -> None:
    """Keep the N-1 update proof on schedule and dispatch, never on a release.

    The job installs the previously published release and requires the real
    updater to move it to Latest. On the release path that assertion cannot
    hold, and is not supposed to. release.yml creates every tag
    ``prerelease: true, make_latest: false`` and promotes it to Latest only
    after this workflow succeeds, while ``kin update`` on the default stable
    channel resolves ``/releases/latest``, which never returns a pre-release.
    The tag under test is therefore not Latest while the job runs, the updater
    correctly reports the installation already current at N-1, the final
    assertion exits 1, and every job needing ``install_proof`` to succeed skips
    with it: npm publish, the boundary contracts, the version-tag image, and
    the promotion that would have made the tag Latest. The job would fail every
    stable cut while proving nothing about the updater.

    So the scoping is the property, pinned as the exact expression. A filter
    written any other way, including one that also admitted ``push``, returns
    the job to the release path, and a called workflow reads the CALLING run's
    ``github`` context, so ``push`` is exactly what the release arrives as.

    Two smaller properties of the same job ride here, because each is the
    difference between a true report and a plausible one. The release listing
    captures gh's exit code on its own line: ``mapfile -t tags < <(gh ...)``
    runs gh in a subshell whose status neither ``set -e`` nor ``pipefail``
    observes, so a 403 would produce an empty array and report an absent
    previous release for what is really an API failure. And the installer step
    runs under ``pipefail``, because ``sh`` reading a truncated download exits
    0 and a dead endpoint would surface a step later as a missing binary.
    """

    jobs = workflow_job_blocks(install_proof)
    update_proof = jobs.get("update-proof")
    if update_proof is None:
        raise AssertionError(
            "install-proof must keep the update-proof job; it is the only "
            "witness that the real updater moves a real installation"
        )
    conditions = [
        value.strip()
        for key, value in job_top_level_mapping_fields(update_proof)
        if key == "if"
    ]
    if conditions != [UPDATE_PROOF_ADMITTED_EVENT_FILTER]:
        raise AssertionError(
            "the update proof runs on the weekly schedule and on a manual "
            "dispatch and on nothing else. On the release path the tag under "
            "test is still a pre-release, so the updater refuses to move and "
            "the job fails every cut; admitted "
            f"{[UPDATE_PROOF_ADMITTED_EVENT_FILTER]}, found {conditions}"
        )

    resolve = install_proof_step(
        install_proof, "Resolve Latest and the release published before it"
    )
    resolve_lines = active_lines(resolve)
    hidden_listing = [
        line for line in resolve_lines if "mapfile" in line and "gh release list" in line
    ]
    if hidden_listing:
        raise AssertionError(
            "the release listing must not run gh inside a process "
            "substitution: neither `set -e` nor `pipefail` sees its status, so "
            "an API failure becomes an empty array and reads as an absent "
            f"previous release; found {hidden_listing}"
        )
    rc_capture = "--json tagName --jq '.[].tagName')\" || rc=$?"
    rc_guard = 'if [ "$rc" -ne 0 ]; then'
    for policy in (rc_capture, rc_guard):
        if policy not in resolve_lines:
            raise AssertionError(
                "the release listing must capture gh's exit code on its own "
                f"line and refuse on it; missing `{policy}`"
            )
    gap_guard = [line for line in resolve_lines if "no N-1 to prove against" in line]
    if not gap_guard:
        raise AssertionError(
            "the release listing must still report an absent previous release; "
            "an API guard that swallowed that case would trade one wrong cause "
            "for another"
        )
    if resolve_lines.index(rc_guard) > resolve_lines.index(gap_guard[0]):
        raise AssertionError(
            "the API-failure refusal must come before the absent-release "
            "refusal, or a failed listing still reports the wrong cause"
        )

    install = install_proof_step(
        install_proof,
        "Install the PREVIOUS release through the real public installer",
    )
    install_lines = active_lines(install)
    piped_install = [
        line
        for line in install_lines
        if line.startswith("curl ") and line.endswith("| \\")
    ]
    if not piped_install:
        raise AssertionError(
            "the update proof must install through the real public installer "
            "piped into `sh`; without that pipe the pipefail requirement below "
            "guards nothing"
        )
    if "set -euo pipefail" not in install_lines:
        raise AssertionError(
            "the update proof's installer step runs under pipefail: `sh` "
            "reading a truncated download exits 0, so a dead endpoint would "
            "surface a step later as a missing `kin` and read as a broken "
            f"binary; found {[line for line in install_lines if line.startswith('set ')]}"
        )


# The preflight driver refuses to grade a candidate whose install-proof.yml is
# not the one its ported validator was written against, and it builds that
# comparison out of these two lines. They are pinned so this test reads the
# same number the driver reads. A test that recomputed the pin its own way
# could agree with itself while the driver went on refusing.
PORTED_PIN_EXTRACTOR = (
    r"""PORTED_SHA="$(sed -n 's/^  sha256: "\([0-9a-f]\{64\}\)",$/\1/p' """
    r'''"$HELPERS/validate.mjs" | head -1)"'''
)
PORTED_PIN_COMPARISON = 'if [ "$WORKFLOW_SHA" != "$PORTED_SHA" ]; then'
# That extractor as a Python pattern: any line of validate.mjs, not only the
# one inside PORTED_FROM, and `head -1` keeps whichever comes first.
PORTED_PIN_LINE = re.compile(r'(?m)^  sha256: "([0-9a-f]{64})",$')
PORTED_FROM_BLOCK = re.compile(r"(?ms)^export const PORTED_FROM = \{.*?^\};$")


def assert_ported_validator_pin_tracks_install_proof(
    install_proof: bytes, validator: str, preflight: str
) -> None:
    """Fail the pull request that moves install-proof.yml and not the ported pin.

    ``scripts/release-proof/bin/kin-release-preflight.d/validate.mjs`` is a hand
    port of install-proof.yml's "Validate installed capability proof" step, and
    it records the sha256 of the workflow it was ported from. The preflight
    driver compares that pin against the workflow at the ref under test and
    exits 65, REFUSED, when the two differ, because a PASS from a stale port is
    a PASS against assertions the release gate no longer runs.

    That refusal is correct and it arrives far too late. It arrives inside
    Release Cut, on every leg at once, against a candidate already selected,
    hours after the pull request that moved the workflow merged green. kin#1652
    appended the update-proof job on 2026-09-09 and all three legs of the
    v0.7.9 cut refused the next morning, holding a release. It was the fourth
    drift of this pin, and none of the four was visible to the pull request
    that caused it. So the driver's own comparison runs here too, where ci.yml
    runs this file on every pull request, and the author who moves either file
    restamps the pin in the change that needs it.

    Nothing here judges whether the port is still CORRECT, only whether it
    still claims to mirror the workflow in this tree. A resync is still a human
    reading the workflow step assertion by assertion.
    """

    block = PORTED_FROM_BLOCK.search(validator)
    if block is None:
        raise AssertionError(
            "validate.mjs must export a PORTED_FROM block naming the "
            "install-proof.yml it was ported from; with no pin the preflight "
            "has nothing to compare and refuses every leg of the cut"
        )
    pins = list(PORTED_PIN_LINE.finditer(validator))
    inside = [
        pin for pin in pins if block.start() <= pin.start() and pin.end() <= block.end()
    ]
    if len(pins) != 1 or len(inside) != 1:
        raise AssertionError(
            "the driver keeps the FIRST line of validate.mjs its sed "
            "expression matches, so exactly one line may match and it must be "
            "the one inside PORTED_FROM; a second one earlier in the file "
            "would be read in its place and this test would still agree with "
            f"the pin nobody uses. Found {len(pins)} matching, {len(inside)} "
            "of them in the block"
        )
    recorded = inside[0].group(1)
    actual = hashlib.sha256(install_proof).hexdigest()
    if recorded != actual:
        raise AssertionError(
            "the ported capability validator records install-proof.yml at "
            f"{recorded}, but the workflow in this tree is {actual}. Read the "
            '"Validate installed capability proof" step at both shas, port '
            "every changed assertion into validate.mjs, and move "
            "PORTED_FROM.sha256 to the new sha. When nothing the validator "
            "asserts moved, say which hunks you compared and move the pin "
            "anyway: leaving it behind refuses the next release cut rather "
            "than this pull request"
        )
    for policy in (PORTED_PIN_EXTRACTOR, PORTED_PIN_COMPARISON):
        if policy not in preflight:
            raise AssertionError(
                "the preflight driver must still read the pin and compare it "
                "to the workflow the way this test mirrors, or the two agree "
                f"only by luck; missing `{policy}`"
            )


def assert_mcp_smoke_follows_the_launched_profile(source: str, context: str) -> None:
    """Hold the MCP smoke's tools/list check to the call the smoke makes.

    The smoke launches the entry setup wrote, and that entry's profile decides
    what the server lists: one `kin` tool on a routed profile, `locate` by name
    otherwise. The call already followed the profile when the listing check
    still looked for the named tool by substring, which a routed listing never
    contains, so every leg would have failed on the release's own bytes. The
    workflow step and its preflight port both carry this smoke.
    """

    active = "\n".join(active_lines(source))
    if 'includes("locate")' in active or 'includes("semantic_search")' in active:
        raise AssertionError(
            f"{context} must check tools/list for the tool the launched entry's "
            "profile serves, not for one tool by name"
        )
    for policy in (
        "const launchProfile = entry.env?.KIN_MCP_TOOL_PROFILE;",
        "const launchCall = searchCall(launchProfile);",
        'profile.startsWith("agent-routed")',
        '{ name: "kin", arguments: { command: "locate", args: searchArgs } }',
        '{ name: "locate", arguments: searchArgs }',
        "listedTools.some((tool) => tool?.name === launchCall.name)",
        "`MCP tools/list omitted ${launchCall.name}, the tool the launched ` +",
        "params: launchCall,",
    ):
        require(active, policy, context)


def assert_install_proof_first_run_never_pipes_the_daemon_spawner(
    first_run: str,
) -> None:
    """Keep every daemon-spawning command in the first-run step unpiped.

    A pipeline outlives the command that wrote to it whenever a child
    inherits the descriptor. On Windows that child holds the write end until
    it idles out, so the reader blocks while the step looks like it is
    working. v0.5.49's Windows leg spent 3672 s of a 3684 s step in exactly
    two such waits, 1805 s and 1868 s, which is the 1800 s daemon idle window
    plus slack; both were a ``kin setup --intent agent`` piped into ``tee``,
    and that step was 62 of the 63.5 minutes between the published tag and
    npm.

    `kin status` and the graph queries already carry that rule in prose. This
    is the executable half: the first-run step captures by redirect and
    prints with `cat`, never by pipe, and a setup that fails still reaches
    the job log rather than aborting the step before the `cat`.
    """

    # The pipe scan runs first, so a capture that reverts to `tee` is named as
    # the pipe it is rather than as a missing redirect line.
    piped = [line for line in active_lines(first_run) if "| tee" in line]
    if piped:
        raise AssertionError(
            "the first-run install proof captures by redirect, never by pipe: "
            "a child that inherits the pipe holds it for its whole idle "
            f"window and the step waits on it; found {piped}"
        )
    for policy in (
        'kin setup --no-interactive --intent agent --shell "$PROOF_SHELL" \\',
        '> "$captures/kin-setup.txt" 2>&1 || setup_status=$?',
        'cat "$captures/kin-setup.txt"',
        'if [ "$setup_status" -ne 0 ]; then exit "$setup_status"; fi',
        '> "$captures/kin-claude-fallback-setup.txt" 2>&1 || fallback_setup_status=$?',
        'cat "$captures/kin-claude-fallback-setup.txt"',
        'if [ "$fallback_setup_status" -ne 0 ]; then exit "$fallback_setup_status"; fi',
    ):
        require(first_run, policy, "unpiped first-run setup capture")


def assert_install_proof_repo_steps_cover_windows(install_proof: str) -> None:
    """Require native Windows released bytes to prove repo, daemon, and MCP.

    This is deliberately a positive execution proof. Merely rejecting one
    spelling of ``runner.os != 'Windows'`` lets an equivalent Linux-only
    expression, a job-level guard, a matrix exclusion, or a fixed non-Windows
    runner silently remove the Windows leg while the policy test stays green.
    Only the reviewed CLI release namespace guard may scope the job. The three
    release-critical steps remain unconditional inside one canonical matrix
    job whose runner is the matrix OS and whose include list contains an
    unexcluded ``windows-latest`` row.
    """

    jobs = workflow_job_blocks(install_proof)
    install_job = jobs.get("install-proof")
    if install_job is None:
        raise AssertionError("native Windows release proof lost the install-proof job")

    job_fields = job_top_level_mapping_fields(install_job)
    conditions = [value.strip() for key, value in job_fields if key == "if"]
    if conditions != [INSTALL_PROOF_CLI_RELEASE_EVENT_FILTER]:
        raise AssertionError(
            "native Windows release proof lost repository coverage: install-proof "
            "admits only its reviewed CLI release event job-level condition"
        )
    runs_on = [value.strip() for key, value in job_fields if key == "runs-on"]
    if runs_on != ["${{ matrix.os }}"]:
        raise AssertionError(
            "native Windows release proof must run on the reviewed OS matrix; "
            f"found runs-on={runs_on}"
        )

    strategy_lines = active_lines(dynamic_job_context_source(install_job))
    if any(
        line == "exclude:" or line.startswith("exclude:")
        for line in strategy_lines
    ):
        raise AssertionError(
            "native Windows release proof matrix must not exclude a reviewed OS row"
        )
    release_rows, _ = install_proof_matrix_rows(install_job)
    windows_rows = [row for row in release_rows if row.get("os") == "windows-latest"]
    if len(windows_rows) != 1:
        raise AssertionError(
            "native Windows release proof matrix must contain exactly one "
            f"windows-latest row; found {len(windows_rows)}"
        )

    for step in (
        "First-run repository, daemon, and setup proof",
        "Graph query and MCP tool-call proof",
        "Validate installed capability proof",
    ):
        step_source = textwrap.dedent(
            install_proof_step(install_proof, step)
        ).strip()
        top_level_fields: list[str] = []
        for line in classifier_active_job_source(step_source).splitlines()[1:]:
            indent = len(line) - len(line.lstrip())
            if indent != 2:
                continue
            match = re.fullmatch(r"  (?P<key>[A-Za-z0-9_-]+):(?:[ \t].*)?", line)
            if match is None:
                raise AssertionError(
                    "install-proof step fields must use canonical unquoted `key:` "
                    f"syntax: {line.strip()}"
                )
            top_level_fields.append(match.group("key"))
        if "if" in top_level_fields:
            raise AssertionError(
                f"native Windows release proof lost repository coverage: {step} "
                "must run unconditionally for every reviewed matrix row"
            )


def assert_install_proof_repo_free_windows_proof(repo_free: str) -> None:
    """Keep the pre-repository Windows setup/provenance checkpoint falsifiable.

    The later first-run steps prove the admitted repo, daemon, all five client
    writers, and MCP. This earlier checkpoint separately proves the CLI's
    public provenance, unsupported capability posture, and the four client
    writers that do not require an exact repository binding.
    """

    active = "\n".join(active_lines(repo_free))
    for policy in (
        "kin bench-meta --json > kin-windows-bench-meta.json",
        "kin registry authority --json > kin-windows-registry-authority.json",
        "if kin registry authority --fix > kin-windows-registry-fix.txt 2>&1; then",
        'kin setup --no-interactive --intent agent --shell "$PROOF_SHELL"',
        "kin setup status --json | tee kin-windows-health.json",
        "kin doctor --json | tee kin-windows-doctor.json",
        "for agent in claude cursor codex gemini windsurf agy; do",
        'fs.readFileSync("expected-commit.txt", "utf8").trim()',
        'fs.readFileSync("expected-lock-sha.txt", "utf8").trim()',
        'fs.readFileSync("installed-kin-command.txt", "utf8").trim()',
        "installedKin !== expectedInstalledKin",
        "meta.kin_commit !== expectedCommit",
        "meta.kin_dirty !== false",
        "meta.kin_source_known !== true",
        "meta.dependency_provenance !== expectedLock",
        "meta.embeddings?.vector_enabled !== true",
        "meta.embeddings?.embeddings_enabled !== true",
        "meta.embeddings?.local_inference_enabled !== false",
        "meta.embeddings?.external_embeddings_enabled !== true",
        'meta.embeddings?.provider !== "none"',
        "meta.embeddings?.metal_enabled !== false",
        'authority.checks[0]?.state !== "unsupported"',
        '["repo_init", "unsupported"]',
        '["daemon_running", "unsupported"]',
        '["registry_authority", "unsupported"]',
        '["vfs_projection", "unsupported"]',
        '["semantic_query_readiness", "unsupported"]',
        '["shell_path", "healthy"]',
        '["setup_ledger", "healthy"]',
        '["commit_author", "healthy"]',
        "git config --global user.email ci@firelock.ai",
        "git config --global user.name ci",
        '["mcp_client_claude", "healthy"]',
        '["mcp_client_cursor", "healthy"]',
        '"mcp_client_codex", "mcp_client_antigravity", "mcp_client_antigravity_workspace"',
        '["mcp_client_gemini", "healthy"]',
        '["mcp_client_windsurf", "healthy"]',
        "entry.command !== installedKin",
        'path.join(home, ".gemini", "config", "mcp_config.json")',
    ):
        require(active, policy, "repo-free Windows install proof")


def assert_install_proof_status_contract(
    first_run: str, graph_query: str, embedding: str, validation: str
) -> None:
    """Pin install proof to fields the released binaries actually emit.

    `kin status --json` is the repository status report, not the daemon command
    envelope. Build provenance therefore comes from the CLI's bench metadata
    and the daemon's public health response, while embedding progress comes
    from the required `kin.status.v3` coverage sum type. Keeping these sources
    separate prevents a plausible-looking proof from reading fields that no
    shipped command produces.
    """

    first_run_active = "\n".join(active_lines(first_run))
    embedding_active = "\n".join(active_lines(embedding))
    validation_active = "\n".join(active_lines(validation))

    require(
        first_run_active,
        'kin bench-meta --json > "$captures/kin-build-meta.json"',
        "installed CLI provenance capture",
    )
    require(
        first_run_active,
        "printf '%s\\n' \"$fake_agent_bin\" >> \"$GITHUB_PATH\"",
        "cross-step agent-client proof PATH",
    )
    for policy in (
        "for agent in claude cursor codex gemini windsurf agy; do",
        'antigravity_legacy="$HOME/.gemini/antigravity-ide/mcp_config.json"',
        'command: "/stale/kin"',
        'CODEX_CONFIG="$HOME/.codex/config.toml" PROOF_CAPTURES="$captures" python3',
        'tomllib.load(handle)["mcp_servers"]["kin"]',
        'claude_fallback_home="$RUNNER_TEMP/kin-proof-claude-fallback-home"',
        'printf \'{}\\n\' > "$claude_fallback_home/.claude/config.json"',
        'if [ -e "$claude_fallback_home/.claude.json" ]; then',
        "kin-claude-fallback-health.json",
        "kin-claude-fallback-doctor.json",
    ):
        require(first_run_active, policy, "complete MCP writer state/path matrix")

    graph_active_lines = active_lines(graph_query)
    graph_active = "\n".join(graph_active_lines)
    # A query that starts the daemon must be redirected, never piped. Windows
    # hands a spawned daemon every handle its caller was given, so a piped query
    # leaves the reader on the far side waiting for the daemon rather than for
    # the CLI. The step then resumes only once the daemon has idled out and
    # retired the endpoint the next line reads, so it reports a missing
    # `.kin/daemon.port`. That reads as a daemon which never published one, and
    # for two releases it was recorded as exactly that.
    for line in graph_active_lines:
        if line.startswith(("kin search ", "kin locate ")) and "|" in line:
            raise AssertionError(
                "install proof must redirect a daemon-starting query rather than "
                "pipe it: a daemon that inherits the pipe holds it until idle "
                "shutdown and retires the endpoint this step reads before the "
                f"read happens: {line}"
            )

    for policy in (
        'kin search hello --json > "$captures/kin-search.json"',
        'kin locate hello --json --explain --max-files 5 > "$captures/kin-locate.json"',
        'kin store path > "$captures/kin-store-path.txt"',
        "store_dir=.kin",
        "daemon_port=\"$(tr -d '[:space:]' < \"$store_dir/daemon.port\")\"",
        'DAEMON_PORT="$daemon_port" node',
        "http://127.0.0.1:${process.env.DAEMON_PORT}/health",
        "kin-daemon-health.json",
        'kin setup status --json | tee "$captures/kin-health.json"',
        'kin doctor --json | tee "$captures/kin-doctor.json"',
        'path.join(process.cwd(), ".agents", "mcp_config.json")',
        "spawn(entry.command, entry.args",
        'const stripVerbatim = (p) => (typeof p === "string" && p.startsWith("\\\\\\\\?\\\\") ? p.slice(4) : p);',
        "cwd: stripVerbatim(entry.cwd),",
        "env: { ...proofEnv, ...(entry.env ?? {}) }",
    ):
        require(graph_active, policy, "installed daemon startup and health capture")

    daemon_start = 'kin search hello --json > "$captures/kin-search.json"'
    endpoint_capture = "daemon_port=\"$(tr -d '[:space:]' < \"$store_dir/daemon.port\")\""
    setup_health = 'kin setup status --json | tee "$captures/kin-health.json"'
    doctor_health = 'kin doctor --json | tee "$captures/kin-doctor.json"'
    daemon_start_index = graph_active_lines.index(daemon_start)
    if any(
        daemon_start_index >= graph_active_lines.index(capture)
        for capture in (endpoint_capture, setup_health, doctor_health)
    ):
        raise AssertionError(
            "install proof must start the daemon through a graph query before "
            "reading its endpoint or capturing setup health"
        )

    for stale_capture in (setup_health, doctor_health):
        if stale_capture in "\n".join(active_lines(first_run)):
            raise AssertionError(
                "install proof must capture setup health after the daemon-starting "
                f"graph query, not in the first-run step: {stale_capture}"
            )

    for stale in (
        "status.build",
        "status.semantic_coverage",
        "embeddedStatus.semantic_coverage",
    ):
        if stale in validation_active:
            raise AssertionError(
                "install proof reads a field the released status report does not "
                f"emit: {stale}"
            )

    for policy in (
        'const cliMeta = JSON.parse(fs.readFileSync("kin-build-meta.json", "utf8"))',
        'const daemonHealth = JSON.parse(fs.readFileSync("kin-daemon-health.json", "utf8"))',
        "sha: cliMeta.kin_commit",
        "sha: daemonHealth.build?.sha",
        'status.schema !== "kin.status.v3"',
        "validateEmbeddingCoverage(status.embedding_coverage",
        "cliMeta.embeddings?.vector_enabled !== true",
        "cliMeta.embeddings?.embeddings_enabled !== true",
        "cliMeta.embeddings?.local_inference_enabled !== false",
        "cliMeta.embeddings?.external_embeddings_enabled !== true",
        'cliMeta.embeddings?.provider !== "none"',
        'embeddedStatus.schema !== "kin.status.v3"',
        "validateEmbeddingCoverage(embeddedStatus.embedding_coverage",
        'embeddedCoverage.state === "observed" && embeddedCoverage.indexed !== 0',
        'fs.readFileSync("kin-embed-refusal.txt", "utf8")',
        'const allowedSemanticReadiness = ["unsupported"];',
        'fs.readFileSync("../installed-kin-command.txt", "utf8").trim()',
        "installedKin !== expectedInstalledKin",
        '["mcp_client_antigravity", "healthy"]',
        '["mcp_client_antigravity_workspace", "healthy"]',
        '"kin-claude-fallback-config.json"',
        '"kin-codex-config.json"',
        'path.join(repoRoot, ".agents", "mcp_config.json")',
        "entry.command !== installedKin",
        'legacy.userPolicy !== "preserve"',
    ):
        require(
            validation_active,
            policy,
            "released-byte status and build proof contract",
        )

    require(
        embedding_active,
        'kin status --json | tee "$captures/kin-embedded-status.json"',
        "post-embedding repository status capture",
    )


def health_join_copies() -> list[tuple[str, str]]:
    """Every pasted copy of the health join, as (home, dedented text).

    Extracted from the start of the BEGIN line so the first line carries the
    same indentation as the rest, which is what makes `textwrap.dedent` able to
    remove it. Slicing from the marker itself leaves line one at column zero,
    dedent finds a common prefix of "" and every copy compares unequal for a
    reason that has nothing to do with the rule.
    """

    found: list[tuple[str, str]] = []
    for home in HEALTH_JOIN_HOMES:
        text = (ROOT / home).read_text(encoding="utf-8")
        cursor = 0
        while True:
            begin = text.find(HEALTH_JOIN_BEGIN, cursor)
            if begin < 0:
                break
            line_start = text.rfind("\n", 0, begin) + 1
            end = text.index(HEALTH_JOIN_END, begin)
            end = text.index("\n", end) + 1
            found.append((home, textwrap.dedent(text[line_start:end])))
            cursor = end
    return found


def assert_health_join_copies_agree() -> None:
    """One roll-up rule, however many files have to carry the letters.

    install-proof.yml and rc-build.yml run with no checkout by design,
    so they cannot import `scripts/verify-capability-proof.mjs` and each pastes
    the rule instead. Four copies of a rule drift, and this set drifts in the
    worst direction: every copy kept agreeing with every other while all four
    disagreed with the product, so a fresh Windows install emitted
    `"healthy": true` over a pending and a degraded row and the release's own
    proof threw at tag time, where no fix on main can reach the tag.
    """

    copies = health_join_copies()
    expected = 4
    if len(copies) != expected:
        raise AssertionError(
            f"the health join must appear exactly {expected} times across "
            f"{', '.join(HEALTH_JOIN_HOMES)}; found {len(copies)}: "
            f"{[home for home, _ in copies]}"
        )
    # The module's copy is the reference. It is the one with a test suite.
    reference_home, reference = copies[0]
    if reference_home != HEALTH_JOIN_HOMES[0]:
        raise AssertionError(
            f"the first copy must come from {HEALTH_JOIN_HOMES[0]}, which is the "
            f"one with unit tests; found {reference_home}"
        )
    for home, block in copies[1:]:
        if block != reference:
            diff = "\n".join(
                difflib.unified_diff(
                    reference.splitlines(),
                    block.splitlines(),
                    fromfile=reference_home,
                    tofile=home,
                    lineterm="",
                )
            )
            raise AssertionError(
                f"{home} carries a health join that differs from "
                f"{reference_home}; one rule, one text:\n{diff}"
            )
    # The extractor itself must be able to come up empty, or "every copy agrees"
    # is a sentence about a list nothing ever put anything into.
    if HEALTH_JOIN_BEGIN.replace("HEALTH JOIN", "HEALTH JOIN THAT IS NOT THERE") in reference:
        raise AssertionError("the control marker must not appear in the real block")
    for home in HEALTH_JOIN_HOMES:
        text = (ROOT / home).read_text(encoding="utf-8")
        if "// --- BEGIN HEALTH JOIN THAT IS NOT THERE ---" in text:
            raise AssertionError(
                f"{home} carries the fabricated control marker, so the extractor "
                "cannot be trusted to have found the real ones"
            )
    # Every copy must carry both halves of the rule. A block that lost
    # `healthNeedsAttention` would still be identical in four places.
    for clause in (
        'check.status !== "healthy" && check.status !== "unsupported"',
        'check.status === "missing"',
        'check.id === "semantic_query_readiness" && check.status === "stale"',
        'return checks.some(healthNeedsAttention) ? "needs_attention" : "ready";',
    ):
        if clause not in reference:
            raise AssertionError(
                f"the health join must carry the clause {clause!r}; four identical "
                "copies of the wrong rule is the failure this pin exists to catch"
            )


def assert_capability_canary_contract(
    install_proof: str,
    canary: str,
    capability_script: str,
) -> None:
    """Bind the branch canary to the same capability contract the proof enforces.

    The install proof cannot read a tracked script. It runs anonymously with no
    checkout, which is what makes it worth trusting and also why the contract
    cannot simply be shared by import. Two copies of a contract drift, and this
    pair drifts in the worst direction: the canary keeps passing while the proof
    it was built to predict starts failing at tag time, where no fix on main can
    reach the tag. So the copies are compared here instead.
    """

    blocks = re.findall(
        r"const required = new Map\(\[\n(.*?)\n\s*\]\);", install_proof, re.S
    )
    if len(blocks) != 2:
        raise AssertionError(
            "the install proof is expected to carry exactly two required-check "
            f"tables, the Windows repo-free one and the capability one; found {len(blocks)}"
        )
    # The second table is the capability validator's, which is the one the canary
    # mirrors. The first belongs to the Windows repository-free proof, which
    # asserts a different surface on a leg the canary does not run.
    proof_ids = set(re.findall(r'\["([a-z_0-9]+)",', blocks[1]))
    # The proof asserts readiness in its own arm rather than in the table, so it
    # is required by the proof without appearing in it.
    proof_ids.add("semantic_query_readiness")

    listed = re.search(
        r"export const REQUIRED_CHECK_IDS = \[(.*?)\];", capability_script, re.S
    )
    if listed is None:
        raise AssertionError(
            "the capability contract must export the check ids it requires"
        )
    canary_ids = set(re.findall(r'"([a-z_0-9]+)"', listed.group(1)))

    if canary_ids != proof_ids:
        missing = sorted(proof_ids - canary_ids)
        extra = sorted(canary_ids - proof_ids)
        raise AssertionError(
            "the canary's capability contract must require exactly the health "
            "checks the install proof requires; "
            f"absent from the canary: {missing or 'none'}; "
            f"required by the canary but not the proof: {extra or 'none'}"
        )

    # The rule the proof applies to every health report it reads lives in one
    # delimited block, pasted into the workflows that cannot import it, and
    # `assert_health_join_copies_agree` requires every copy identical. A text
    # probe for one clause was what stood here, and a clause probe cannot see the
    # half of the rule that was wrong: the copies all agreed on `stale` while
    # none of them counted `pending` or `degraded`, which is what fenced v0.6.1.
    assert_health_join_copies_agree()

    # Everything below is judged on active lines only, in both directions.
    #
    # A comment must not be able to satisfy a requirement, which is the trap this
    # suite already guards elsewhere: this file explains its own rules at length,
    # so a whole-file scan would find `--require-observed` in the paragraph
    # explaining it and pass a canary that no longer passes the flag. A comment
    # must not be able to trip a prohibition either, for the same reason in
    # reverse: the header explains why the proof installs from the public
    # endpoint and why the canary must not, and a naive scan refuses the very
    # sentence documenting the rule.
    active_canary = "\n".join(
        line for line in canary.splitlines() if not line.lstrip().startswith("#")
    )

    # The two triggers the ticket asked for, and the reason for each. A canary
    # with only a path filter misses a break arriving through a path nobody
    # anticipated; one with only a schedule finds it a night late.
    if "schedule:" not in active_canary:
        raise AssertionError("the canary must run nightly against main")
    if "pull_request:" not in active_canary or "paths:" not in active_canary:
        raise AssertionError(
            "the canary must run path-filtered on the proof's known inputs"
        )
    for required_path in (
        '".github/workflows/install-proof.yml"',
        '"crates/kin-cli/src/commands/health.rs"',
    ):
        if required_path not in active_canary:
            raise AssertionError(
                "the canary's path filter must cover the proof itself and the "
                f"readiness classifier that fenced v0.5.18; absent: {required_path}"
            )

    if "scripts/verify-capability-proof.mjs" not in active_canary:
        raise AssertionError(
            "the canary must actually run the shared capability contract"
        )
    if "--require-observed" not in active_canary:
        raise AssertionError(
            "the canary must assert against an observed coverage at least once; "
            "a run that only ever saw unobservable coverage judged nothing, and a "
            "check that cannot fail is not evidence"
        )

    # A canary publishes nothing and promotes nothing. The release path stays the
    # release path, and a branch binary never touches it.
    for forbidden in (
        "contents: write",
        "id-token: write",
        "packages: write",
        "gh release",
        "npm publish",
        "get.kinlab.dev",
        "curl -fsSL https://get",
    ):
        if forbidden in active_canary:
            raise AssertionError(
                f"the canary must neither publish nor promote anything: {forbidden}"
            )



def check_capability_canary_contract(
    capability_contract: str, install_proof: str, install_proof_canary: str
) -> None:
    # Recovery is outside the mint's reviewed veto set, but it still must not
    # spend runners or raise an advisory alarm for a release the reviewed record
    # retired. Parse active step fields so a comment repeating the condition
    # cannot satisfy the guard while the YAML `if` omits it.
    # The proof is the best gate Kin has and it ran only once a tag existed, so
    # a change that broke it was discovered one release late and could not be
    # repaired in place. The canary moves the portable half of that contract onto
    # main. What binds the two is here, because two copies of one contract drift
    # in the direction that matters: the canary stays green while the proof it
    # predicts starts failing at a tag.
    assert_capability_canary_contract(
        install_proof, install_proof_canary, capability_contract
    )
    expect_assertion(
        "the proof requires a check the canary does not",
        "must require exactly the health checks",
        lambda: assert_capability_canary_contract(
            install_proof,
            install_proof_canary,
            capability_contract.replace('  "setup_ledger",\n', "", 1),
        ),
    )
    expect_assertion(
        "the canary drops the classifier that fenced v0.5.18 from its path filter",
        "path filter must cover",
        lambda: assert_capability_canary_contract(
            install_proof,
            install_proof_canary.replace(
                '      - "crates/kin-cli/src/commands/health.rs"\n', "", 2
            ),
            capability_contract,
        ),
    )
    expect_assertion(
        "the canary can pass without ever observing coverage",
        "check that cannot fail is not evidence",
        lambda: assert_capability_canary_contract(
            install_proof,
            install_proof_canary.replace("--require-observed \\\n", ""),
            capability_contract,
        ),
    )
    expect_assertion(
        "the canary grows the authority to publish",
        "must neither publish nor promote",
        lambda: assert_capability_canary_contract(
            install_proof,
            install_proof_canary.replace(
                "permissions:\n  contents: read\n",
                "permissions:\n  contents: write\n",
                1,
            ),
            capability_contract,
        ),
    )
    expect_assertion(
        "the canary stops running the shared contract at all",
        "must actually run the shared capability contract",
        lambda: assert_capability_canary_contract(
            install_proof,
            install_proof_canary.replace("scripts/verify-capability-proof.mjs", "true"),
            capability_contract,
        ),
    )
    expect_assertion(
        "the canary loses its nightly run and only fires on anticipated paths",
        "must run nightly against main",
        lambda: assert_capability_canary_contract(
            install_proof,
            install_proof_canary.replace("  schedule:\n", "  # schedule:\n", 1),
            capability_contract,
        ),
    )



def check_first_run_captures_and_embedding(
    install_proof: str
) -> tuple[str, str, str, str]:
    first_run_start = install_proof.index(
        "      - name: First-run repository, daemon, and setup proof"
    )
    # The guarded span ends at the next step rather than three steps later, so
    # the failure message naming the first-run step describes the region it
    # actually covers and a legitimate later `kin init` is not misdiagnosed.
    graph_query_start = install_proof.index(
        "      - name: Graph query and MCP tool-call proof",
        first_run_start,
    )
    embedding_start = install_proof.index(
        "      - name: Unix embedding and semantic retrieval proof",
        graph_query_start,
    )
    restore_start = install_proof.index(
        "      - name: Restore captured proof reports into the proof repository",
        embedding_start,
    )
    validation_start = install_proof.index(
        "      - name: Validate installed capability proof",
        restore_start,
    )
    preserve_start = install_proof.index(
        "      - name: Preserve proof reports",
        validation_start,
    )
    first_run = install_proof[first_run_start:graph_query_start]
    graph_query = install_proof[graph_query_start:embedding_start]
    embedding = install_proof[embedding_start:restore_start]
    restore = install_proof[restore_start:validation_start]
    validation = install_proof[validation_start:preserve_start]
    for policy in (
        'case "$PROOF_SHELL" in',
        "export SHELL=/bin/bash",
        "export SHELL=/bin/zsh",
        'printf \'SHELL=%s\\n\' "$SHELL" >> "$GITHUB_ENV"',
    ):
        require(first_run, policy, "cross-step install-proof shell pin")
    assert_install_proof_init_log_authority(first_run)
    assert_install_proof_first_run_never_pipes_the_daemon_spawner(first_run)
    for label, original, mutation, expected in (
        (
            "the first setup goes back through a pipe",
            '            > "$captures/kin-setup.txt" 2>&1 || setup_status=$?',
            '            2>&1 | tee "$captures/kin-setup.txt"',
            "never by pipe",
        ),
        (
            "the fallback setup goes back through a pipe",
            '            > "$captures/kin-claude-fallback-setup.txt" 2>&1 || fallback_setup_status=$?',
            '            2>&1 | tee "$captures/kin-claude-fallback-setup.txt"',
            "never by pipe",
        ),
        (
            "a failing setup stops reaching the job log",
            '          cat "$captures/kin-setup.txt"\n',
            "",
            "unpiped first-run setup capture",
        ),
        (
            "a refused setup stops failing the install proof",
            '          if [ "$setup_status" -ne 0 ]; then exit "$setup_status"; fi\n',
            "",
            "unpiped first-run setup capture",
        ),
        (
            "a refused fallback setup stops failing the install proof",
            '          if [ "$fallback_setup_status" -ne 0 ]; then exit "$fallback_setup_status"; fi\n',
            "",
            "unpiped first-run setup capture",
        ),
    ):
        if original not in first_run:
            raise AssertionError(
                "first-run unpiped-setup falsification lost fixture for "
                f"{label}: {original!r}"
            )
        expect_assertion(
            label,
            expected,
            lambda mutated=first_run.replace(original, mutation, 1): (
                assert_install_proof_first_run_never_pipes_the_daemon_spawner(mutated)
            ),
        )
    expect_assertion(
        "kin init writes its log into the worktree it admits",
        "outside the worktree it admits",
        lambda: assert_install_proof_init_log_authority(
            first_run.replace(
                'kin init > "$captures/kin-init.txt" 2>&1 || init_status=$?',
                "kin init 2>&1 | tee kin-init.txt",
                1,
            )
        ),
    )
    expect_assertion(
        "a second kin init escapes the admitted-worktree contract",
        "exactly once",
        lambda: assert_install_proof_init_log_authority(
            f"{first_run}\n          kin init\n"
        ),
    )
    for label, active_line in (
        (
            "the capture directory stops being rooted outside the worktree",
            'captures="$RUNNER_TEMP/kin-proof-captures"',
        ),
        (
            "a refused init stops failing the install proof",
            'if [ "$init_status" -ne 0 ]; then exit "$init_status"; fi',
        ),
    ):
        expect_assertion(
            label,
            "install-proof init log capture",
            lambda mutated=first_run.replace(
                active_line, f"# {active_line}", 1
            ): assert_install_proof_init_log_authority(mutated),
        )
    for label, wrapped_init in (
        ("a wrapped second kin init", "(cd sub && kin init)"),
        ("an indirect second admission", '"$KIN" init'),
        ("an evaluated second admission", "eval kin init"),
    ):
        expect_assertion(
            f"{wrapped_init} escapes the exactly-once count",
            "exactly once",
            lambda mutated=f"{first_run}\n          {wrapped_init}\n": (
                assert_install_proof_init_log_authority(mutated)
            ),
        )
    expect_assertion(
        "an unrelated write into the admitted worktree escapes the guard",
        "only the committed Git bootstrap may run",
        lambda: assert_install_proof_init_log_authority(
            first_run.replace(
                '          captures="$RUNNER_TEMP/kin-proof-captures"\n',
                "          echo scratch > proof-note.txt\n"
                '          captures="$RUNNER_TEMP/kin-proof-captures"\n',
                1,
            )
        ),
    )
    expect_assertion(
        "kin's own init status is propagated before init has run",
        "must admit, then propagate",
        lambda: assert_install_proof_init_log_authority(
            first_run.replace(
                '          kin init > "$captures/kin-init.txt" 2>&1 || init_status=$?\n'
                '          cat "$captures/kin-init.txt"\n'
                '          if [ "$init_status" -ne 0 ]; then exit "$init_status"; fi\n',
                '          if [ "$init_status" -ne 0 ]; then exit "$init_status"; fi\n'
                '          kin init > "$captures/kin-init.txt" 2>&1 || init_status=$?\n'
                '          cat "$captures/kin-init.txt"\n',
                1,
            )
        ),
    )

    # The capture contract the init log was the first case of, now covering
    # every report the proof writes while the watcher is admitting.
    proof_steps = {
        "the first-run proof": first_run,
        "the graph query, MCP, and VFS proof": graph_query,
        "the embedding proof": embedding,
    }
    restore_position = (embedding_start, restore_start, validation_start)
    assert_install_proof_captures_stay_out_of_the_admitted_tree(
        proof_steps, restore, restore_position
    )
    # The MCP smoke lists and calls the tool the launched entry's profile
    # serves, in the workflow and in the release preflight's port of it alike.
    proof_flow = PORTED_PROOF_FLOW.read_text(encoding="utf-8")
    for smoke_context, smoke_source in (
        ("install-proof MCP smoke", graph_query),
        ("release preflight MCP smoke", proof_flow),
    ):
        assert_mcp_smoke_follows_the_launched_profile(smoke_source, smoke_context)
        expect_assertion(
            f"the {smoke_context} looks for locate whatever the profile",
            "not for one tool by name",
            lambda mutated=replace_exactly_once(
                smoke_source,
                "listedTools.some((tool) => tool?.name === launchCall.name)",
                'JSON.stringify(listedTools).includes("locate")',
                f"{smoke_context} listing check",
            ), context=smoke_context: assert_mcp_smoke_follows_the_launched_profile(
                mutated, context
            ),
        )
        expect_assertion(
            f"the {smoke_context} calls locate whatever the profile",
            "is missing required policy: const launchCall = searchCall(launchProfile);",
            lambda mutated=replace_exactly_once(
                smoke_source,
                "const launchCall = searchCall(launchProfile);",
                'const launchCall = { name: "locate", arguments: searchArgs };',
                f"{smoke_context} call shape",
            ), context=smoke_context: assert_mcp_smoke_follows_the_launched_profile(
                mutated, context
            ),
        )
    for label, step_name, original, mutation in (
        (
            "a first-run capture reverts to a relative redirect",
            "the first-run proof",
            'kin status --json > "$captures/kin-status.json" 2> "$captures/kin-status-json.stderr.txt"',
            'kin status --json > kin-status.json 2> "$captures/kin-status-json.stderr.txt"',
        ),
        (
            "a graph-query capture reverts to a relative tee",
            "the graph query, MCP, and VFS proof",
            'kin doctor --json | tee "$captures/kin-doctor.json"',
            "kin doctor --json | tee kin-doctor.json",
        ),
        (
            "a VFS capture reverts to a relative stderr redirect",
            "the graph query, MCP, and VFS proof",
            '2> "$captures/vfs-graph-read.stderr.txt"',
            "2> vfs-graph-read.stderr.txt",
        ),
        (
            "an embedding capture reverts to a relative tee",
            "the embedding proof",
            'kin locate hello --json --explain --max-files 5 | tee "$captures/kin-semantic-locate.json"',
            "kin locate hello --json --explain --max-files 5 | tee kin-semantic-locate.json",
        ),
        (
            "an MCP Node capture reverts to a relative write",
            "the graph query, MCP, and VFS proof",
            'fs.writeFileSync(path.join(captures, "kin-mcp-out.jsonl"), stdout);',
            'fs.writeFileSync("kin-mcp-out.jsonl", stdout);',
        ),
    ):
        expect_assertion(
            label,
            "writes a proof report into the admitted tree",
            lambda mutated_steps={
                **proof_steps,
                step_name: proof_steps[step_name].replace(original, mutation, 1),
            }: assert_install_proof_captures_stay_out_of_the_admitted_tree(
                mutated_steps, restore, restore_position
            ),
        )
    for label, active_line in (
        (
            "the captures never reach the proof reports",
            'cp "$captures/$capture" "$destination/$capture"',
        ),
        (
            "the restore stops listing the capture directory",
            'done < <(ls -1 "$captures")',
        ),
    ):
        expect_assertion(
            label,
            "install-proof capture restore",
            lambda mutated=restore.replace(
                active_line, f"# {active_line}", 1
            ): assert_install_proof_captures_stay_out_of_the_admitted_tree(
                proof_steps, mutated, restore_position
            ),
        )
    expect_assertion(
        "a failed leg stops handing over what it captured",
        "must run on failure too",
        lambda: assert_install_proof_captures_stay_out_of_the_admitted_tree(
            proof_steps, restore.replace("if: always()", "if: success()", 1), restore_position
        ),
    )
    expect_assertion(
        "the captures are restored before the assertions that read the store",
        "after the last step that reads the store",
        lambda: assert_install_proof_captures_stay_out_of_the_admitted_tree(
            proof_steps, restore, (embedding_start, embedding_start - 1, validation_start)
        ),
    )

    assert_install_proof_embedding_settles_before_measurement(embedding)
    for label, expected, original, mutation in (
        (
            "an embed pass that ran stops failing the engine-free leg",
            "poll the counters to quiescence",
            'if [ "$embed_rc" -eq 0 ]; then',
            "if false; then",
        ),
        (
            "the refusal stops having to name the missing model",
            "poll the counters to quiescence",
            '*"kin embed cannot index anything here"*"no local embedding model"*) ;;',
            "*) ;;",
        ),
        (
            "the settle stops requiring two agreeing reads",
            "poll the counters to quiescence",
            "if (current === previous) {",
            "if (true) {",
        ),
        (
            "an expired settle stops failing the leg",
            "poll the counters to quiescence",
            "process.exit(1);",
            "process.exitCode = 0;",
        ),
        (
            "the store is measured before it has settled",
            "embed, then settle, then capture",
            'PROOF_CAPTURES="$captures" node <<\'NODE\'',
            "kin status --json --wait-quiesce 60 | tee \"$captures/kin-embedded-status.json\"\n"
            "          PROOF_CAPTURES=\"$captures\" node <<'NODE'",
        ),
    ):
        expect_assertion(
            label,
            expected,
            lambda mutated=embedding.replace(
                original, mutation, 1
            ): assert_install_proof_embedding_settles_before_measurement(mutated),
        )
    expect_assertion(
        "the settle becomes a duration nobody measured",
        "rather than on a duration nobody measured",
        lambda: assert_install_proof_embedding_settles_before_measurement(
            embedding.replace(
                '          PROOF_CAPTURES="$captures" node',
                "          sleep 30\n"
                '          PROOF_CAPTURES="$captures" node',
                1,
            )
        ),
    )
    return embedding, first_run, graph_query, validation



def check_install_proof_legs_and_pull_request_gate(
    ci_workflow: str,
    install_proof: str,
    install_proof_bytes: bytes,
    ported_validator: str,
    preflight_driver: str,
) -> None:
    assert_install_proof_cli_release_events_only(install_proof)
    guard = "    if: " + INSTALL_PROOF_CLI_RELEASE_EVENT_FILTER + "\n"
    for label, replacement, error in (
        ("release guard removed", "", "one job-level release event guard"),
        ("all published releases admitted", "    if: ${{ true }}\n", "misroutes Mac alpha"),
        ("CLI prerelease publications excluded", "    if: ${{ github.event_name != 'release' || inputs.release_tag || (startsWith(github.event.release.tag_name, 'v') && !github.event.release.prerelease) }}\n", "misroutes CLI stable published as prerelease"),
        ("non-release callers excluded", "    if: ${{ startsWith(github.event.release.tag_name, 'v') }}\n", "misroutes workflow dispatch latest"),
        ("explicit reusable call loses its input", "    if: ${{ github.event_name != 'release' || startsWith(github.event.release.tag_name, 'v') }}\n", "misroutes explicit call from another release event"),
    ):
        expect_assertion(label, error,
            lambda replacement=replacement: assert_install_proof_cli_release_events_only(
                replace_exactly_once(install_proof, guard, replacement, label)))
    assert_install_proof_repo_steps_cover_windows(install_proof)
    for repo_step in (
        "First-run repository, daemon, and setup proof",
        "Graph query and MCP tool-call proof",
        "Validate installed capability proof",
    ):
        expect_assertion(
            f"the native Windows release proof loses {repo_step}",
            "lost repository coverage",
            lambda mutated=install_proof.replace(
                f"      - name: {repo_step}\n",
                f"      - name: {repo_step}\n        if: runner.os != 'Windows'\n",
                1,
            ): assert_install_proof_repo_steps_cover_windows(mutated),
        )
        expect_assertion(
            f"an alternate Linux-only expression removes {repo_step} from Windows",
            "lost repository coverage",
            lambda mutated=install_proof.replace(
                f"      - name: {repo_step}\n",
                f"      - name: {repo_step}\n        if: runner.os == 'Linux'\n",
                1,
            ): assert_install_proof_repo_steps_cover_windows(mutated),
        )

    for label, original, mutation, expected in (
        (
            "a job-level condition removes the Windows install proof",
            "    name: ${{ matrix.os }}\n",
            "    name: ${{ matrix.os }}\n    if: runner.os != 'Windows'\n",
            "job-level condition",
        ),
        (
            "the install proof stops using its OS matrix runner",
            "    runs-on: ${{ matrix.os }}",
            "    runs-on: ubuntu-latest",
            "reviewed OS matrix",
        ),
        (
            "the Windows matrix row disappears",
            ',{"os":"windows-latest","setup-shell":"powershell"}',
            "",
            "windows-latest row",
        ),
        (
            "a matrix exclusion removes Windows after retaining its include row",
            "        include: ${{ fromJSON(",
            "        exclude:\n          - os: windows-latest\n"
            "        include: ${{ fromJSON(",
            "must not exclude",
        ),
    ):
        if original not in install_proof:
            raise AssertionError(
                f"Windows install-proof falsification lost fixture for {label}: {original}"
            )
        expect_assertion(
            label,
            expected,
            lambda mutated=install_proof.replace(original, mutation, 1): (
                assert_install_proof_repo_steps_cover_windows(mutated)
            ),
        )

    assert_install_proof_every_leg_gates_the_release(install_proof)
    for label, original, mutation, expected in (
        (
            "the Windows waiver returns as a matrix flag",
            '{"os":"windows-latest","setup-shell":"powershell"}',
            '{"os":"windows-latest","setup-shell":"powershell","experimental":true}',
            "admit no experimental",
        ),
        (
            "the waiver returns on a Unix row instead",
            '{"os":"ubuntu-latest","setup-shell":"bash"}',
            '{"os":"ubuntu-latest","setup-shell":"bash","experimental":true}',
            "admit no experimental",
        ),
        (
            "the install proof tolerates every leg unconditionally",
            "    strategy:\n",
            "    continue-on-error: true\n    strategy:\n",
            "no job-level continue-on-error",
        ),
        (
            "the matrix-guard spelling of the waiver returns",
            "    strategy:\n",
            "    continue-on-error: ${{ matrix.experimental == true }}\n    strategy:\n",
            "no job-level continue-on-error",
        ),
        (
            "a step-level tolerance spares every leg below the job header",
            "      - name: Public install (Unix)\n",
            "      - name: Public install (Unix)\n"
            "        continue-on-error: true\n",
            "no continue-on-error at all",
        ),
        (
            "a quoted-key tolerance hides on a step",
            "      - name: Public install (Unix)\n",
            "      - name: Public install (Unix)\n"
            "        \"continue-on-error\": true\n",
            "no continue-on-error at all",
        ),
        (
            "a reviewed platform is dropped from the gating matrix",
            ',{"os":"macos-15-intel","setup-shell":"zsh"}',
            "",
            "must gate exactly",
        ),
    ):
        if original not in install_proof:
            raise AssertionError(
                "install-proof gating posture falsification lost fixture for "
                f"{label}: {original!r}"
            )
        expect_assertion(
            label,
            expected,
            lambda mutated=install_proof.replace(original, mutation, 1): (
                assert_install_proof_every_leg_gates_the_release(mutated)
            ),
        )

    assert_update_proof_stays_off_the_release_path(install_proof)
    update_proof_job = workflow_job_blocks(install_proof)["update-proof"]
    for label, original, mutation, expected in (
        (
            "the update proof loses its scoping and returns to every cut",
            "    if: ${{ github.event_name == 'schedule' "
            "|| github.event_name == 'workflow_dispatch' }}\n",
            "",
            "and on nothing else",
        ),
        (
            "the release path is admitted beside the two witnesses",
            "|| github.event_name == 'workflow_dispatch' }}\n",
            "|| github.event_name == 'workflow_dispatch' "
            "|| github.event_name == 'push' }}\n",
            "and on nothing else",
        ),
        (
            "the scoping is spelled as an exclusion instead",
            "    if: ${{ github.event_name == 'schedule' "
            "|| github.event_name == 'workflow_dispatch' }}\n",
            "    if: ${{ github.event_name != 'pull_request' }}\n",
            "and on nothing else",
        ),
        (
            "the whole update proof is dropped rather than scoped",
            update_proof_job,
            "",
            "must keep the update-proof job",
        ),
        (
            "the release listing goes back inside a process substitution",
            '          rc=0\n          listing="$(gh release list',
            "          mapfile -t tags < <(gh release list",
            "must not run gh inside a process",
        ),
        (
            "the API-failure refusal stops refusing",
            '          if [ "$rc" -ne 0 ]; then\n',
            "          if false; then\n",
            "capture gh's exit code on its own line",
        ),
        (
            "the API guard swallows the absent-release report",
            '            echo "::error title=no N-1 to prove against::'
            "latest='$latest' previous='$previous'\"\n",
            "",
            "must still report an absent previous release",
        ),
        (
            "the API refusal is moved below the absent-release refusal",
            "          rc=0\n",
            '          echo "::error title=no N-1 to prove against::moved"\n'
            "          rc=0\n",
            "must come before the absent-release refusal",
        ),
        (
            "the installer step loses pipefail and swallows a dead endpoint",
            "          set -euo pipefail\n"
            "          # Same installer and the same env contract as the proof above.\n",
            "          set -eu\n"
            "          # Same installer and the same env contract as the proof above.\n",
            "runs under pipefail",
        ),
        (
            "the installer stops piping the real download into sh",
            "          curl -fsSL https://get.kinlab.dev/install | \\\n"
            '            KIN_NO_SETUP=1 KIN_VERSION="${PREVIOUS#v}" sh\n',
            '          KIN_NO_SETUP=1 KIN_VERSION="${PREVIOUS#v}" sh ./install.sh\n',
            "piped into `sh`",
        ),
    ):
        if original not in install_proof:
            raise AssertionError(
                "update-proof scoping falsification lost fixture for "
                f"{label}: {original!r}"
            )
        expect_assertion(
            label,
            expected,
            lambda mutated=install_proof.replace(original, mutation, 1): (
                assert_update_proof_stays_off_the_release_path(mutated)
            ),
        )

    assert_ported_validator_pin_tracks_install_proof(
        install_proof_bytes, ported_validator, preflight_driver
    )
    ported_pin = PORTED_PIN_LINE.search(ported_validator).group(1)
    ported_block = PORTED_FROM_BLOCK.search(ported_validator).group(0)
    decoy_pin = "0" * 64
    for fixture, source, name in (
        (ported_pin, ported_validator, "the PORTED_FROM pin"),
        (ported_block, ported_validator, "the PORTED_FROM block"),
        (PORTED_PIN_EXTRACTOR, preflight_driver, "the driver's pin extractor"),
        (PORTED_PIN_COMPARISON, preflight_driver, "the driver's pin comparison"),
    ):
        if fixture not in source:
            raise AssertionError(
                f"ported-pin falsification lost fixture for {name}: {fixture!r}"
            )
    for label, install_bytes, validator_text, preflight_text, expected in (
        (
            "install-proof.yml moves and the ported pin stays behind",
            install_proof_bytes + b"\n# a step the ported validator never saw\n",
            ported_validator,
            preflight_driver,
            "but the workflow in this tree is",
        ),
        (
            "the pin is restamped to a sha that is not the workflow's",
            install_proof_bytes,
            ported_validator.replace(ported_pin, "a" * 64, 1),
            preflight_driver,
            "but the workflow in this tree is",
        ),
        (
            "the PORTED_FROM export is dropped and nothing is pinned at all",
            install_proof_bytes,
            ported_validator.replace(ported_block, "", 1),
            preflight_driver,
            "must export a PORTED_FROM block",
        ),
        (
            "a second pin line appears above the block, where the driver's "
            "first-match extractor reads it instead",
            install_proof_bytes,
            ported_validator.replace(
                ported_block, f'  sha256: "{decoy_pin}",\n{ported_block}', 1
            ),
            preflight_driver,
            "exactly one line may match",
        ),
        (
            "the driver stops extracting the pin the way this test mirrors",
            install_proof_bytes,
            ported_validator,
            preflight_driver.replace(PORTED_PIN_EXTRACTOR, 'PORTED_SHA=""', 1),
            "must still read the pin",
        ),
        (
            "the driver stops comparing the pin against the workflow",
            install_proof_bytes,
            ported_validator,
            preflight_driver.replace(PORTED_PIN_COMPARISON, "if false; then", 1),
            "must still read the pin",
        ),
    ):
        expect_assertion(
            label,
            expected,
            lambda i=install_bytes, v=validator_text, p=preflight_text: (
                assert_ported_validator_pin_tracks_install_proof(i, v, p)
            ),
        )

    assert_install_proof_runs_on_pull_requests(ci_workflow, install_proof)
    assert_install_proof_gate_grades_the_merge_queue(ci_workflow)
    pull_request_gate = workflow_job_blocks(ci_workflow)["install-proof-pr-gate"]
    for label, source, original, mutation, expected in (
        (
            "the pull-request install proof gate is deleted outright",
            ci_workflow,
            pull_request_gate,
            "",
            "CI lost",
        ),
        (
            "the pull-request proof calls a lookalike workflow",
            ci_workflow,
            "uses: ./.github/workflows/install-proof.yml",
            "uses: ./.github/workflows/daemon-smoke.yml",
            "reviewed reusable",
        ),
        (
            "the pull-request proof installs a released archive instead",
            ci_workflow,
            f"      local_artifact: {INSTALL_PROOF_PULL_REQUEST_ARTIFACT}\n",
            '      local_artifact: ""\n',
            "reviewed reusable",
        ),
        (
            "the built binaries are uploaded under another name",
            ci_workflow,
            f"          name: {INSTALL_PROOF_PULL_REQUEST_ARTIFACT}\n",
            "          name: install-proof-pr-other\n",
            "built by this pull request",
        ),
        (
            "the proof stops building the release target",
            ci_workflow,
            "--target x86_64-unknown-linux-musl -p kin-cli -p kin-daemon",
            "-p kin-cli -p kin-daemon",
            "built by this pull request",
        ),
        (
            "the archive is no longer judged by the release shape contract",
            ci_workflow,
            "            assertReleaseArchiveMemberPaths(listing, {\n",
            "            void listing && ((_) => {})({\n",
            "built by this pull request",
        ),
        (
            "the proof is moved off the pull request the way a slow leg was",
            ci_workflow,
            "  install-proof-pr-gate:\n    name: Install Proof (PR) Gate\n",
            "  install-proof-pr-gate:\n    name: Install Proof (PR) Gate\n"
            "    if: ${{ github.event_name != 'pull_request' }}\n",
            "must not exclude an event",
        ),
        (
            "the gate stops failing on a proof that did not pass",
            ci_workflow,
            '          if [ "$PROOF_RESULT" != "success" ]; then\n',
            '          if false; then\n',
            "fail closed",
        ),
        (
            "the semantic retrieval proof becomes release-only",
            install_proof,
            "      - name: Unix embedding and semantic retrieval proof\n"
            "        if: runner.os != 'Windows'\n",
            "      - name: Unix embedding and semantic retrieval proof\n"
            "        if: runner.os != 'Windows' && inputs.local_artifact == ''\n",
            "exactly the Unix condition",
        ),
        (
            "the locate capture the validator reads disappears",
            install_proof,
            '          kin locate hello --json --explain --max-files 5 '
            '| tee "$captures/kin-semantic-locate.json"\n',
            "",
            "lost the locate capture",
        ),
    ):
        if original not in source:
            raise AssertionError(
                "pull-request install proof falsification lost fixture for "
                f"{label}: {original!r}"
            )
        mutated_ci = ci_workflow
        mutated_proof = install_proof
        if source is ci_workflow:
            mutated_ci = ci_workflow.replace(original, mutation, 1)
        else:
            mutated_proof = install_proof.replace(original, mutation, 1)
        expect_assertion(
            label,
            expected,
            lambda ci=mutated_ci, proof=mutated_proof: (
                assert_install_proof_runs_on_pull_requests(ci, proof)
            ),
        )



def check_preflight_validators_and_status_contract(
    embedding: str,
    first_run: str,
    graph_query: str,
    install_proof: str,
    validation: str,
) -> None:
    repo_free = install_proof_step(
        install_proof, "Windows repo-free provenance and setup proof"
    )
    assert_install_proof_repo_free_windows_proof(repo_free)
    assert_node_validator_rejects_missing_proof(
        repo_free, "repo-free Windows install proof"
    )
    assert_windows_node_validator_behavior(repo_free)
    selective_windows_required_bypass = replace_exactly_once(
        repo_free,
        "          if (actual !== expected) {\n",
        '          if (id !== "kin_binary" && actual !== expected) {\n',
        "Windows selective required-check bypass",
    )
    expect_assertion(
        "the Windows validator selectively skips the kin_binary requirement",
        "accepted a behaviorally invalid proof fixture",
        lambda: assert_windows_node_validator_behavior(
            selective_windows_required_bypass
        ),
    )
    windows_mcp_only_claude = replace_exactly_once(
        repo_free,
        "          for (const [configPath, profile] of jsonConfigs) {\n",
        "          for (const [configPath, profile] of jsonConfigs.slice(0, 1)) {\n",
        "Windows selective MCP-config bypass",
    )
    expect_assertion(
        "the Windows validator checks MCP values only in Claude's config",
        "accepted a behaviorally invalid proof fixture",
        lambda: assert_windows_node_validator_behavior(windows_mcp_only_claude),
    )
    windows_any_agent_profile = replace_exactly_once(
        repo_free,
        "              entry.env?.KIN_MCP_TOOL_PROFILE !== profile\n",
        '              !String(entry.env?.KIN_MCP_TOOL_PROFILE).startsWith("agent-")\n',
        "Windows per-client MCP profile",
    )
    expect_assertion(
        "the Windows validator accepts either agent profile in every config",
        "MCP profile swapped for the other agent profile",
        lambda: assert_windows_node_validator_behavior(windows_any_agent_profile),
    )
    blocked_repo_free = repo_free.replace(
        '          const fs = require("fs");\n',
        '          /*\n          const fs = require("fs");\n',
        1,
    ).replace('          NODE\n', '          */\n          NODE\n', 1)
    expect_assertion(
        "a JavaScript block comment disables the entire Windows validator",
        "repo-free Windows install proof",
        lambda: assert_install_proof_repo_free_windows_proof(blocked_repo_free),
    )
    false_branch_repo_free = repo_free.replace(
        '          const fs = require("fs");\n',
        '          if (false) {\n          const fs = require("fs");\n',
        1,
    ).replace('          NODE\n', '          }\n          NODE\n', 1)
    expect_assertion(
        "a false branch disables the entire Windows validator",
        "validator is not runtime-falsifiable",
        lambda: assert_node_validator_rejects_missing_proof(
            false_branch_repo_free, "repo-free Windows install proof"
        ),
    )
    partial_false_branch_repo_free = repo_free.replace(
        '          const expectedCommit = fs.readFileSync("expected-commit.txt", "utf8").trim();\n',
        '          const expectedCommit = fs.readFileSync("expected-commit.txt", "utf8").trim();\n'
        '          if (false) {\n',
        1,
    ).replace('          NODE\n', '          }\n          NODE\n', 1)
    expect_assertion(
        "a false branch after expected-commit disables the substantive Windows validator",
        "accepted an expected-commit-only proof tree",
        lambda: assert_node_validator_rejects_missing_proof(
            partial_false_branch_repo_free, "repo-free Windows install proof"
        ),
    )
    for label, original, mutation in (
        (
            "the Windows leg stops binding installed provenance to the release tag",
            "meta.kin_commit !== expectedCommit",
            'meta.kin_commit !== "0000000000000000000000000000000000000000"',
        ),
        (
            "the Windows leg stops proving the release Cargo.lock provenance",
            "meta.dependency_provenance !== expectedLock",
            'meta.dependency_provenance !== ""',
        ),
        (
            "the Windows leg stops proving its vector feature contract",
            "meta.embeddings?.vector_enabled !== true",
            "meta.embeddings?.vector_enabled !== false",
        ),
        (
            "the Windows registry-authority repair negative control disappears",
            "if kin registry authority --fix > kin-windows-registry-fix.txt 2>&1; then",
            "if false; then",
        ),
        (
            "the repo-free posture stops requiring the no-repository answer",
            '["repo_init", "unsupported"]',
            '["repo_init", "healthy"]',
        ),
        (
            "the repo-free posture stops proving the agent-client MCP writers",
            '["mcp_client_windsurf", "healthy"]',
            "",
        ),
        (
            "a repo-free posture pin is commented out in the heredoc it lives in",
            '["repo_init", "unsupported"]',
            '// ["repo_init", "unsupported"]',
        ),
    ):
        expect_assertion(
            label,
            "repo-free Windows install proof",
            lambda mutated=repo_free.replace(
                original, mutation, 1
            ): assert_install_proof_repo_free_windows_proof(mutated),
        )
    assert_install_proof_status_contract(first_run, graph_query, embedding, validation)
    assert_node_validator_rejects_missing_proof(
        validation, "released-byte Unix install proof"
    )
    assert_unix_node_validator_behavior(validation)
    selective_unix_required_bypass = replace_exactly_once(
        validation,
        "          if (actual !== expected) {\n",
        '          if (id !== "kin_binary" && actual !== expected) {\n',
        "Unix selective required-check bypass",
    )
    expect_assertion(
        "the Unix validator selectively skips the kin_binary requirement",
        "accepted a behaviorally invalid proof fixture",
        lambda: assert_unix_node_validator_behavior(selective_unix_required_bypass),
    )
    unix_mcp_only_claude = replace_exactly_once(
        validation,
        "          for (const expected of mcpConfigs) {\n",
        "          for (const expected of mcpConfigs.slice(0, 1)) {\n",
        "Unix selective MCP-config bypass",
    )
    expect_assertion(
        "the Unix validator checks MCP values only in Claude's config",
        "accepted a behaviorally invalid proof fixture",
        lambda: assert_unix_node_validator_behavior(unix_mcp_only_claude),
    )
    unix_any_agent_profile = replace_exactly_once(
        validation,
        "              entry.env?.KIN_MCP_TOOL_PROFILE !== expected.profile ||\n",
        '              !String(entry.env?.KIN_MCP_TOOL_PROFILE).startsWith("agent-") ||\n',
        "Unix per-client MCP profile",
    )
    expect_assertion(
        "the Unix validator accepts either agent profile in every config",
        "MCP profile swapped for the other agent profile",
        lambda: assert_unix_node_validator_behavior(unix_any_agent_profile),
    )
    unix_full_sha_bypass = replace_exactly_once(
        validation,
        '          if (!fullSha.test(build.sha ?? "")) {\n',
        '          if (false && !fullSha.test(build.sha ?? "")) {\n',
        "Unix full-commit-SHA bypass",
    )
    expect_assertion(
        "the Unix validator disables the full commit SHA comparison",
        "accepted a behaviorally invalid proof fixture",
        lambda: assert_unix_node_validator_behavior(unix_full_sha_bypass),
    )
    unix_lock_sha_bypass = replace_exactly_once(
        validation,
        '          if (!lockSha.test(build.dependencyProvenance ?? "")) {\n',
        '          if (false && !lockSha.test(build.dependencyProvenance ?? "")) {\n',
        "Unix lock-SHA bypass",
    )
    expect_assertion(
        "the Unix validator disables the lock SHA comparison",
        "accepted a behaviorally invalid proof fixture",
        lambda: assert_unix_node_validator_behavior(unix_lock_sha_bypass),
    )
    blocked_validation = validation.replace(
        '          const fs = require("fs");\n',
        '          /*\n          const fs = require("fs");\n',
        1,
    ).replace('          NODE\n', '          */\n          NODE\n', 1)
    expect_assertion(
        "a JavaScript block comment disables the entire Unix validator",
        "released-byte status and build proof contract",
        lambda: assert_install_proof_status_contract(
            first_run,
            graph_query,
            embedding,
            blocked_validation,
        ),
    )
    false_branch_validation = validation.replace(
        '          const fs = require("fs");\n',
        '          if (false) {\n          const fs = require("fs");\n',
        1,
    ).replace('          NODE\n', '          }\n          NODE\n', 1)
    expect_assertion(
        "a false branch disables the entire Unix validator",
        "validator is not runtime-falsifiable",
        lambda: assert_node_validator_rejects_missing_proof(
            false_branch_validation, "released-byte Unix install proof"
        ),
    )
    partial_false_branch_validation = validation.replace(
        '          const expectedCommit = fs.readFileSync("../expected-commit.txt", "utf8").trim();\n',
        '          const expectedCommit = fs.readFileSync("../expected-commit.txt", "utf8").trim();\n'
        '          if (false) {\n',
        1,
    ).replace('          NODE\n', '          }\n          NODE\n', 1)
    expect_assertion(
        "a false branch after expected-commit disables the substantive Unix validator",
        "accepted an expected-commit-only proof tree",
        lambda: assert_node_validator_rejects_missing_proof(
            partial_false_branch_validation, "released-byte Unix install proof"
        ),
    )
    expect_assertion(
        "install proof stops capturing CLI build metadata",
        "installed CLI provenance capture",
        lambda: assert_install_proof_status_contract(
            first_run.replace(
                'kin bench-meta --json > "$captures/kin-build-meta.json"',
                'kin --version > "$captures/kin-build-meta.json"',
                1,
            ),
            graph_query,
            embedding,
            validation,
        ),
    )
    expect_assertion(
        "install proof reads daemon endpoint before a daemon-starting query",
        "must start the daemon through a graph query",
        lambda: assert_install_proof_status_contract(
            first_run,
            graph_query.replace(
                'kin search hello --json > "$captures/kin-search.json"',
                "# graph query moved below daemon provenance capture",
                1,
            ).replace(
                'cat "$captures/kin-daemon-health.json"',
                'cat "$captures/kin-daemon-health.json"\n          kin search hello --json > "$captures/kin-search.json"',
                1,
            ),
            embedding,
            validation,
        ),
    )
    expect_assertion(
        "a commented-out daemon-starting query cannot satisfy the proof contract",
        "installed daemon startup and health capture",
        lambda: assert_install_proof_status_contract(
            first_run,
            graph_query.replace(
                'kin search hello --json > "$captures/kin-search.json"',
                '# kin search hello --json > "$captures/kin-search.json"',
                1,
            ),
            embedding,
            validation,
        ),
    )
    expect_assertion(
        "a daemon-starting query is piped again",
        "must redirect a daemon-starting query rather than pipe it",
        lambda: assert_install_proof_status_contract(
            first_run,
            graph_query.replace(
                'kin search hello --json > "$captures/kin-search.json"\n'
                '          cat "$captures/kin-search.json"',
                'kin search hello --json | tee "$captures/kin-search.json"',
                1,
            ),
            embedding,
            validation,
        ),
    )
    expect_assertion(
        "the second daemon-starting query is piped again",
        "must redirect a daemon-starting query rather than pipe it",
        lambda: assert_install_proof_status_contract(
            first_run,
            graph_query.replace(
                "kin locate hello --json --explain --max-files 5 > "
                '"$captures/kin-locate.json"\n'
                '          cat "$captures/kin-locate.json"',
                "kin locate hello --json --explain --max-files 5 | tee "
                '"$captures/kin-locate.json"',
                1,
            ),
            embedding,
            validation,
        ),
    )
    expect_assertion(
        "setup health is captured before the daemon-starting query",
        "must start the daemon through a graph query",
        lambda: assert_install_proof_status_contract(
            first_run,
            graph_query.replace(
                'kin setup status --json | tee "$captures/kin-health.json"',
                "# setup health moved above daemon startup",
                1,
            )
            .replace(
                'kin doctor --json | tee "$captures/kin-doctor.json"',
                "# doctor health moved above daemon startup",
                1,
            )
            .replace(
                'kin search hello --json > "$captures/kin-search.json"',
                'kin setup status --json | tee "$captures/kin-health.json"\n'
                '          kin doctor --json | tee "$captures/kin-doctor.json"\n'
                '          kin search hello --json > "$captures/kin-search.json"',
                1,
            ),
            embedding,
            validation,
        ),
    )
    expect_assertion(
        "install proof reads build provenance from repository status again",
        "does not emit: status.build",
        lambda: assert_install_proof_status_contract(
            first_run,
            graph_query,
            embedding,
            validation.replace("const builds = new Map([", "const stale = status.build;\n          const builds = new Map([", 1),
        ),
    )
    expect_assertion(
        "install proof accepts the pre-coverage status schema",
        "released-byte status and build proof contract",
        lambda: assert_install_proof_status_contract(
            first_run,
            graph_query,
            embedding,
            validation.replace("kin.status.v3", "kin.status.v2"),
        ),
    )
    expect_assertion(
        "install proof reads locate coverage from repository status again",
        "does not emit: status.semantic_coverage",
        lambda: assert_install_proof_status_contract(
            first_run,
            graph_query,
            embedding,
            validation.replace(
                "status.embedding_coverage",
                "status.semantic_coverage",
                1,
            ),
        ),
    )
    for policy in (
        "PROOF_SHELL: ${{ matrix.setup-shell }}",
        'case "$PROOF_SHELL" in',
        "unset PSModulePath PSVersionTable",
        "export SHELL=/bin/bash",
        "export SHELL=/bin/zsh",
    ):
        require(embedding, policy, "embedded-health shell reset")



def check_installed_mcp_launchers_and_proof_reports(
    health: str,
    install_proof: str,
    mcp_tools: str,
    npm_canonical_readme: str,
    quickstart: str,
    setup: str,
) -> None:
    # A health failure has to name the rows it failed on. It used to name every
    # row that was not `healthy`, which on a fresh Windows install is 21 rows of
    # which 19 are `unsupported` and irrelevant, so the message buried its own
    # cause. These needles are the replacement: the rows that were NOT tolerated
    # first, then the verdict, then the full attention list.
    for policy in (
        "rows needing attention that a fresh repo-free install does not expect:",
        "rows needing attention that a fresh install does not expect:",
        "verdict=${report.verdict}",
        "every row needing attention: ${attentionRows(report)}",
    ):
        require(install_proof, policy, "actionable install-proof health failure")
    require(
        install_proof,
        "manifest.schema_version === 2",
        "aggregate release-provenance schema accepted by install proof",
    )
    require(
        install_proof,
        '["mcp_client_codex", "healthy"]',
        "repo-bound Codex MCP install proof",
    )
    for policy in (
        'fs.writeFileSync("installed-kin-command.txt", `${installedKin}\\n`)',
        "execFileSync(installedKin, [\"--version\"]",
        "for agent in claude cursor codex gemini windsurf agy; do",
        "spawn(entry.command, entry.args",
    ):
        require(install_proof, policy, "installed MCP executable and writer proof")
    for policy in (
        "evaluate_mcp_client(&client.path, client.id)",
        "McpLauncherTopology::Native",
        "McpLauncherTopology::CanonicalNpm",
        "CANONICAL_NPM_MCP_COMMAND",
        "CANONICAL_NPM_MCP_PACKAGE",
        "mcp_argument_vector_matches(entry, client_id, topology)",
        '"codex" | "grok" | "antigravity" | "antigravity_workspace"',
        "configured_mcp_launcher()",
    ):
        require(health, policy, "product-owned exact MCP entry health validation")
    require(
        health,
        "evaluate_codex_binding(&client.path)",
        "product-owned Codex MCP binding validation",
    )
    require(
        health,
        "super::setup::codex_entry_has_exact_repo_binding(&content, expected_repo)",
        "shared TOML parser for Codex MCP binding validation",
    )
    for policy in (
        "CANONICAL_NPM_MCP_PACKAGE",
        'args[0].as_str() == Some("-y")',
        'args[2].as_str() == Some("mcp")',
        'args[4].as_str() == Some("--repo")',
    ):
        require(setup, policy, "canonical npm Codex repository binding")

    ordinary_repair_start = setup.index(
        "pub(crate) fn remerge_existing_mcp_configs_detailed()"
    )
    ordinary_repair_end = setup.index(
        "#[cfg(all(test, unix))]", ordinary_repair_start
    )
    ordinary_repair = setup[ordinary_repair_start:ordinary_repair_end]
    require(
        ordinary_repair,
        "configured_mcp_launcher",
        "doctor repair uses the configured installation launcher",
    )
    updater_repair_start = setup.index(
        "pub(crate) fn remerge_mcp_targets_exact_with_topology_and_finalizer"
    )
    updater_repair_end = setup.index(
        "fn validate_mcp_repair_precondition", updater_repair_start
    )
    updater_repair = setup[updater_repair_start:updater_repair_end]
    require(
        updater_repair,
        "let command = managed_mcp_launcher()?;",
        "updater repair stays pinned to the managed launcher",
    )
    if "configured_mcp_launcher" in updater_repair:
        raise AssertionError(
            "updater MCP repair must not accept the ordinary configured launcher resolver"
        )

    for path, source in (
        (QUICKSTART_DOC, quickstart),
        (MCP_TOOLS_DOC, mcp_tools),
        (NPM_CANONICAL_README, npm_canonical_readme),
    ):
        if '"command": "kin"' in "\n".join(active_lines(source)):
            raise AssertionError(
                f"{path.relative_to(ROOT)} must not document a PATH-dependent bare Kin MCP launcher"
            )
    for source, label in (
        (quickstart, "quickstart canonical npm MCP launcher"),
        (npm_canonical_readme, "canonical npm package MCP launcher"),
    ):
        require(source, '"command": "npx"', label)
        require(
            source,
            '"args": ["-y", "@kinlab/kin", "mcp", "start"]',
            label,
        )
    for source, label in (
        (quickstart, "quickstart native MCP launcher"),
        (mcp_tools, "MCP tools native launcher"),
    ):
        require(source, '"command": "/absolute/path/to/kin"', label)
    if "JSON.parse(codexArgsMatch[1])" in install_proof:
        raise AssertionError(
            "install proof must not parse TOML as JSON; the product health check owns Codex binding validation"
        )

    for policy in (
        "Graph-backed VFS projection proof",
        'probe_bin="$RUNNER_TEMP/vfs-open-probe"',
        "fstat(STDOUT_FILENO, &stdout_stat)",
        "chmod 000 probe.py",
        "KIN_VFS_STRICT=1 kin-vfs exec --workspace .",
        'cmp -s "$captures/vfs-expected.txt" "$captures/vfs-graph-read.txt"',
        "installed VFS did not return the exact graph-owned probe.py bytes",
        "release-provenance-attestation.json",
        "installed-vfs-provenance.json",
        "installed ${component.name} differs from the attested public archive",
        '--signer-digest "$expected_commit"',
        'if [ "$negative_status" -ne 4 ]',
        "negative control did not fail with the expected raw-disk permission error",
        "installed kin-vfs socket remained after shutdown",
        'process_state="$(ps -o stat= -p "$vfs_pid" 2>/dev/null || true)"',
        "process_state=\"$(printf '%s' \"$process_state\" | tr -d '[:space:]')\"",
        "trap 'on_vfs_signal 130' INT",
        "trap 'on_vfs_signal 143' TERM",
    ):
        require(install_proof, policy, "public VFS and installed-artifact proof")
    cleanup_start = install_proof.index("          cleanup_vfs() {")
    signal_start = install_proof.index("          on_vfs_signal() {", cleanup_start)
    cleanup_vfs = install_proof[cleanup_start:signal_start]
    require(cleanup_vfs, 'vfs_pid=""', "idempotent public VFS cleanup")
    if 'ps -o stat= -p "$vfs_pid" 2>/dev/null | tr' in cleanup_vfs:
        raise AssertionError(
            "public VFS cleanup must treat a disappeared daemon PID as the "
            "expected successful-stop state under set -euo pipefail"
        )
    require(
        install_proof,
        "          trap - EXIT\n          cleanup_vfs\n          trap - INT TERM",
        "non-reentrant public VFS normal cleanup",
    )
    if "kin-vfs-open-probe" in install_proof:
        raise AssertionError(
            "public VFS probe must not use a Kin-family basename; the shipped "
            "shim intentionally bypasses basenames beginning with 'kin-'"
        )
    proof_upload = install_proof[
        install_proof.index("- name: Preserve proof reports") :
    ]
    for report in (
        "release-provenance.json",
        "release-provenance.json.sha256",
        "release-provenance-attestation.json",
        "installed-vfs-provenance.json",
        # The Windows leg's whole evidence trail. Its admission logs are the
        # only record of what each refusal actually said, and the v0.4.5
        # incident was diagnosed from exactly this kind of uploaded log.
        "kin-windows-bench-meta.json",
        "kin-windows-registry-authority.json",
        "kin-windows-registry-fix.txt",
        "kin-windows-setup.txt",
        "kin-windows-health.json",
        "kin-windows-doctor.json",
        "kin-windows-admission/*.txt",
    ):
        require(proof_upload, report, "preserved installed-artifact proof report")
