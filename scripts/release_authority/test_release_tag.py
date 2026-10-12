# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Checks that pin .github/workflows/release-tag.yml.

The tag mint, its required-check gate, tag readback and abandoned-tag
admission.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import tempfile
import textwrap
from collections.abc import Callable
from pathlib import Path

from .test_helpers import (
    RELEASE_GATE_FIXTURE_SHA,
    RELEASE_TAG,
    REQUIRED_RELEASE_CHECKS,
    ROOT,
    TAG_LISTING_FORMAT,
    TRUSTED_POLICY_PREFIX,
    active_lines,
    execute_tag_readback,
    expect_assertion,
    job_step_active_lines,
    replace_exactly_once,
    require,
    tag_readback_source,
    workflow_job_blocks,
    workflow_step_source,
)


PROOF_GATE_POLICY = "scripts/check-release-proof-artifacts.mjs"

ABANDONED_TAGS = ROOT / "scripts" / "abandoned-release-tags.json"
TAG_SELECTOR = ROOT / "scripts" / "select-admissible-release-tag.py"
ABANDONED_TAGS_POLICY = "scripts/abandoned-release-tags.json"
TAG_SELECTOR_POLICY = "scripts/select-admissible-release-tag.py"

EXPECTED_SELECTOR_INVOCATIONS = {
    "release-tag": ('"$abandoned"', '"$candidate_tags"', '"$TAG"', '"$admissible"'),
    "release-train": ('"$abandoned"', '"$candidate_tags"', '""', '"$admissible"'),
    # Recovery asks the same question about one tag, so its candidate listing is
    # the single pair the failed release ran rather than the whole tag listing,
    # and its mint intent is empty for a blunter reason than the train's: naming
    # the tag under reconcile as intent makes the selector refuse exactly the
    # recorded tag recovery is asking about, so recovery would read a waived tag
    # as unwaived and re-arm the alert the record exists to stand down.
    "release-recovery": ('"$RECORD"', '"$candidate"', '""', '"$admissible"'),
}

# Durable workflow IDs are GitHub's repository-scoped identity, while `path`
# makes that identity reviewable in source. These values are also exercised
# against the current REST response shape by the positive fixture below.
#
# The third element is the event the producer publishes under INSIDE the merge
# queue. It is merge_group for every producer now: secret-scan.yml used to
# carry only `push: branches: ["**"]` and publish `gitleaks (full history)` from
# a push to the queue ref, which is why the mint admits a tier by the ref the
# queue built rather than by the event name, and still does.
REQUIRED_RELEASE_CHECK_PROVENANCE = {
    "Check & Test (ubuntu-latest)": (
        245_803_170,
        ".github/workflows/ci.yml",
        "merge_group",
    ),
    "Check & Test (macos-latest)": (
        245_803_170,
        ".github/workflows/ci.yml",
        "merge_group",
    ),
    "Falsify guards": (245_803_170, ".github/workflows/ci.yml", "merge_group"),
    "Feature permutation tests (ubuntu-latest)": (
        245_803_170,
        ".github/workflows/ci.yml",
        "merge_group",
    ),
    "Feature permutation tests (macos-latest)": (
        245_803_170,
        ".github/workflows/ci.yml",
        "merge_group",
    ),
    "DCO Sign-off": (245_803_170, ".github/workflows/ci.yml", "merge_group"),
    "cargo-deny": (251_549_972, ".github/workflows/sast.yml", "merge_group"),
    "gitleaks (full history)": (
        293_452_372,
        ".github/workflows/secret-scan.yml",
        "merge_group",
    ),
    "Windows installer + vector release build": (
        245_803_170,
        ".github/workflows/ci.yml",
        "merge_group",
    ),
}
# Ruleset 19746451 "Require status checks on main" gates the merge queue and is
# live GitHub configuration no test here can read. The mint carries a reviewed
# mirror of it; this is the part of that mirror the mint does not itself veto
# on, which it still requires an admitted build to have published.
RULESET_ONLY_RELEASE_CHECKS = {
    "PR text hygiene": (328_945_626, ".github/workflows/pr-text-hygiene.yml"),
}
GITHUB_ACTIONS_APP_ID = 15_368
RELEASE_TAG_WORKFLOW_ID = 318_521_292
# The Acceptance job's own context, a release veto the mint judges apart from
# REQUIRED_CHECKS (which mirrors the main ruleset) in every evidence tier.
ACCEPTANCE_CONTEXT = "Product Acceptance"
ACCEPTANCE_PATH = ".github/workflows/acceptance.yml"
ACCEPTANCE_WORKFLOW_ID = 339_594_603

# The landing commit's first parent, and the base sha the queue ref embeds.
# The mint requires these to be the same value, which is the assertion that
# binds "the tree the queue proved" to "the commit being released".
RELEASE_GATE_PARENT_SHA = "2" * 40
RELEASE_GATE_QUEUE_REF = f"gh-readonly-queue/main/pr-958-{RELEASE_GATE_PARENT_SHA}"
RELEASE_GATE_CURRENT_RUN_ID = 9000


def workflow_active_text(block: str) -> str:
    """Comment-stripped workflow text, safe for a job whose steps run shell.

    `active_lines` is the general helper, and it also strips C-style block
    comments so a JavaScript heredoc cannot hide a no-op validator. A shell
    glob opens one of those: `refs/tags/*` starts a match that runs to the next
    `*/`, which on release-train.yml is `${policy##*/` eleven thousand
    characters later, swallowing most of the reconcile job. A guard reading a
    workflow through that rule is structurally unable to see anything in the
    swallowed range, and an absence check reading it would report absence for a
    step that is right there. YAML and shell both comment with `#` alone, so
    that is all this strips.
    """

    return "\n".join(
        line.strip()
        for line in block.splitlines()
        if line.strip() and not line.strip().startswith("#")
    )


def swap_release_tag_step_order(release_tag: str) -> str:
    """Move the release proof gate after the tag write it exists to gate.

    Both step names survive the swap, so the count check still passes and the
    ordering check is the only thing that can catch it. That is the point: a
    gate that runs after the ref is written reads exactly like a gate.
    """

    gate = "      - name: Require proof-loop artifacts for the release candidate\n"
    write = "      - name: Create release tag ref\n"
    placeholder = "      - name: kin-order-swap-placeholder\n"
    swapped = replace_exactly_once(
        release_tag, gate, placeholder, "proof gate order"
    )
    swapped = replace_exactly_once(swapped, write, gate, "proof gate order")
    return replace_exactly_once(swapped, placeholder, write, "proof gate order")


def swap_train_undraft_after_arm(release_train: str) -> str:
    """Arm the bump pull request before un-drafting it, instead of after.

    Both commands survive, so the presence checks still pass and the ordering
    check is the only thing that can catch it. Ordering is the whole point
    here: `gh pr merge --auto` registered against a draft pull request is not
    taken by the merge queue, so un-drafting afterwards leaves auto-merge armed
    on a pull request nothing will merge.
    """

    lines = release_train.splitlines(keepends=True)

    def find(predicate, start, label):
        for index in range(start, len(lines)):
            if predicate(lines[index]):
                return index
        raise AssertionError(f"release train un-draft order mutation found no {label}")

    undraft_start = find(
        lambda line: ".isDraft" in line and line.lstrip().startswith("if ["),
        0,
        "draft-state branch",
    )
    undraft_end = find(
        lambda line: line.strip() == "fi", undraft_start, "end of the draft-state branch"
    ) + 1
    arm_start = find(
        lambda line: 'gh pr merge "$PR"' in line, undraft_end, "arm command"
    )
    arm_end = find(
        lambda line: "--match-head-commit" in line, arm_start, "end of the arm command"
    ) + 1
    return "".join(
        lines[:undraft_start]
        + lines[undraft_end:arm_end]
        + lines[undraft_start:undraft_end]
        + lines[arm_end:]
    )


def release_check_gate_source(release_tag: str) -> str:
    """Extract the exact Python gate executed by the release-tag workflow."""

    step_start = release_tag.index("      - name: Verify required checks are green")
    step_end = release_tag.index("\n      - name:", step_start + 1)
    step = release_tag[step_start:step_end]
    marker = "          python3 - <<'PY'\n"
    source_start = step.index(marker) + len(marker)
    source_end = step.index("\n          PY", source_start)
    return textwrap.dedent(step[source_start:source_end])


def execute_release_check_gate(
    source: str,
    conclusions: dict[str, str],
    *,
    mutate_fixture: Callable[
        [
            list[dict[str, object]],
            list[dict[str, object]],
            dict[str, object],
        ],
        None,
    ]
    | None = None,
    parent_sha: str = RELEASE_GATE_PARENT_SHA,
    inherited: dict[str, object] | None = None,
    annotations: list[dict[str, object]] | None = None,
) -> subprocess.CompletedProcess[str]:
    """Execute the real gate against a current-API-shaped provenance fixture.

    The default fixture is a merge-queue landing as the API actually reports
    one: every required context published once, by the queue's build of the
    exact sha that landed. That is the mint's authority, so the falsifications
    below mutate the evidence a release is really minted from rather than a
    second copy of it. Fixtures that need the landing push add it explicitly.

    `inherited` is the record the "Resolve evidence inherited from the export
    pull request" step writes, and `annotations` the lines the gate's shell
    writes for cancelled required check-runs; both are absent unless given, as
    they are on a run where nothing was inherited or cancelled.
    """

    workflow_specs = {
        ".github/workflows/ci.yml": {
            "id": 1001,
            "workflow_id": 245_803_170,
            "path": ".github/workflows/ci.yml",
            "event": "merge_group",
            "head_branch": RELEASE_GATE_QUEUE_REF,
            "head_sha": RELEASE_GATE_FIXTURE_SHA,
            "status": "completed",
            "conclusion": "success",
            "check_suite_id": 101,
        },
        ".github/workflows/sast.yml": {
            "id": 1002,
            "workflow_id": 251_549_972,
            "path": ".github/workflows/sast.yml",
            "event": "merge_group",
            "head_branch": RELEASE_GATE_QUEUE_REF,
            "head_sha": RELEASE_GATE_FIXTURE_SHA,
            "status": "completed",
            "conclusion": "success",
            "check_suite_id": 102,
        },
        # secret-scan.yml runs on merge_group, and no longer on pushes to other
        # branches, so the queue ref carries no push copy of gitleaks.
        ".github/workflows/secret-scan.yml": {
            "id": 1003,
            "workflow_id": 293_452_372,
            "path": ".github/workflows/secret-scan.yml",
            "event": "merge_group",
            "head_branch": RELEASE_GATE_QUEUE_REF,
            "head_sha": RELEASE_GATE_FIXTURE_SHA,
            "status": "completed",
            "conclusion": "success",
            "check_suite_id": 103,
        },
        ".github/workflows/pr-text-hygiene.yml": {
            "id": 1004,
            "workflow_id": 328_945_626,
            "path": ".github/workflows/pr-text-hygiene.yml",
            "event": "merge_group",
            "head_branch": RELEASE_GATE_QUEUE_REF,
            "head_sha": RELEASE_GATE_FIXTURE_SHA,
            "status": "completed",
            "conclusion": "success",
            "check_suite_id": 107,
        },
        ACCEPTANCE_PATH: {
            "id": 1005,
            "workflow_id": ACCEPTANCE_WORKFLOW_ID,
            "path": ACCEPTANCE_PATH,
            "event": "merge_group",
            "head_branch": RELEASE_GATE_QUEUE_REF,
            "head_sha": RELEASE_GATE_FIXTURE_SHA,
            "status": "completed",
            "conclusion": "success",
            "check_suite_id": 108,
        },
    }
    workflow_runs = list(workflow_specs.values())
    current_run: dict[str, object] = {
        "id": RELEASE_GATE_CURRENT_RUN_ID,
        "workflow_id": RELEASE_TAG_WORKFLOW_ID,
        "path": ".github/workflows/release-tag.yml",
        "event": "workflow_run",
        "head_branch": "main",
        "head_sha": RELEASE_GATE_FIXTURE_SHA,
        "status": "in_progress",
        "conclusion": None,
        "check_suite_id": 104,
    }
    workflow_runs.append(current_run.copy())
    check_runs = []
    fixture_contexts = list(REQUIRED_RELEASE_CHECKS) + sorted(
        RULESET_ONLY_RELEASE_CHECKS
    ) + [ACCEPTANCE_CONTEXT]
    for index, name in enumerate(fixture_contexts, start=1):
        if name in RULESET_ONLY_RELEASE_CHECKS:
            workflow_path = RULESET_ONLY_RELEASE_CHECKS[name][1]
        elif name == ACCEPTANCE_CONTEXT:
            workflow_path = ACCEPTANCE_PATH
        else:
            _, workflow_path, _ = REQUIRED_RELEASE_CHECK_PROVENANCE[name]
        check_runs.append(
            {
                "name": name,
                "status": "completed",
                "conclusion": conclusions.get(name, "success"),
                "id": index,
                "app_id": GITHUB_ACTIONS_APP_ID,
                "app_slug": "github-actions",
                "check_suite_id": workflow_specs[workflow_path]["check_suite_id"],
                "head_sha": RELEASE_GATE_FIXTURE_SHA,
            }
        )
    check_runs.append(
        {
            "name": "Mint release tag",
            "status": "in_progress",
            "conclusion": None,
            "id": len(check_runs) + 1,
            "app_id": GITHUB_ACTIONS_APP_ID,
            "app_slug": "github-actions",
            "check_suite_id": current_run["check_suite_id"],
            "head_sha": RELEASE_GATE_FIXTURE_SHA,
        }
    )
    if mutate_fixture is not None:
        mutate_fixture(check_runs, workflow_runs, current_run)

    with tempfile.TemporaryDirectory() as directory:
        fixture = Path(directory)
        (fixture / "check_runs.ndjson").write_text(
            "".join(f"{json.dumps(run)}\n" for run in check_runs),
            encoding="utf-8",
        )
        (fixture / "workflow_runs.ndjson").write_text(
            "".join(f"{json.dumps(run)}\n" for run in workflow_runs),
            encoding="utf-8",
        )
        (fixture / "current_run.json").write_text(
            json.dumps(current_run),
            encoding="utf-8",
        )
        if annotations is not None:
            (fixture / "check_run_annotations.ndjson").write_text(
                "".join(f"{json.dumps(record)}\n" for record in annotations),
                encoding="utf-8",
            )
        environment = os.environ.copy()
        environment.pop("INHERITED_EVIDENCE", None)
        if inherited is not None:
            record = fixture / "inherited-evidence.json"
            record.write_text(json.dumps(inherited), encoding="utf-8")
            environment["INHERITED_EVIDENCE"] = str(record)
        environment.update(
            {
                "CURRENT_RUN_ID": str(RELEASE_GATE_CURRENT_RUN_ID),
                "CURRENT_RUN_EVENT": str(current_run["event"]),
                "REQUIRED_CHECKS": "\n".join(REQUIRED_RELEASE_CHECKS),
                "RULESET_REQUIRED_CHECKS": "\n".join(
                    list(REQUIRED_RELEASE_CHECKS) + sorted(RULESET_ONLY_RELEASE_CHECKS)
                ),
                "SHA": RELEASE_GATE_FIXTURE_SHA,
                "RELEASE_PARENT_SHA": parent_sha,
                "TRIGGER_SHA": RELEASE_GATE_FIXTURE_SHA,
                "GITHUB_ACTIONS_APP_ID": str(GITHUB_ACTIONS_APP_ID),
                "GITHUB_ACTIONS_APP_SLUG": "github-actions",
            }
        )
        return subprocess.run(
            [sys.executable, "-c", source],
            cwd=fixture,
            env=environment,
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )


def assert_release_gate_fixture_rejected(
    source: str,
    label: str,
    expected_error: str,
    mutate_fixture: Callable[
        [
            list[dict[str, object]],
            list[dict[str, object]],
            dict[str, object],
        ],
        None,
    ],
    *,
    inherited: dict[str, object] | None = None,
    annotations: list[dict[str, object]] | None = None,
) -> None:
    """Require a provenance/collision fixture to fail for the expected reason."""

    result = execute_release_check_gate(
        source,
        {},
        mutate_fixture=mutate_fixture,
        inherited=inherited,
        annotations=annotations,
    )
    output = result.stdout + result.stderr
    if result.returncode == 0:
        raise AssertionError(f"release gate falsification was admitted: {label}")
    if expected_error not in output:
        raise AssertionError(
            f"release gate falsification failed for the wrong reason: {label}: {output}"
        )


def assert_release_check_rejected(
    source: str,
    check_name: str,
    conclusion: str,
) -> None:
    result = execute_release_check_gate(source, {check_name: conclusion})
    output = result.stdout + result.stderr
    expected = f"required check not green: {check_name} (conclusion={conclusion})"
    if result.returncode == 0:
        raise AssertionError(
            f"release check falsification admitted {check_name}={conclusion}"
        )
    if expected not in output:
        raise AssertionError(
            "release check falsification failed without the expected refusal: "
            f"{check_name}={conclusion}: {output}"
        )


def assert_release_check_accepted(
    source: str,
    check_name: str,
    conclusion: str,
) -> None:
    result = execute_release_check_gate(source, {check_name: conclusion})
    if result.returncode != 0:
        raise AssertionError(
            "release check falsification rejected an allowed conclusion: "
            f"{check_name}={conclusion}: {result.stdout}{result.stderr}"
        )

def assert_release_gate_admits(
    source: str,
    label: str,
    expected: tuple[str, ...],
    mutate_fixture: Callable[
        [
            list[dict[str, object]],
            list[dict[str, object]],
            dict[str, object],
        ],
        None,
    ],
    *,
    inherited: dict[str, object] | None = None,
    annotations: list[dict[str, object]] | None = None,
    absent: tuple[str, ...] = (),
) -> None:
    """Require a fixture to be admitted, and to announce what it admitted over.

    A downgrade that is not announced is worse than the refusal it replaces, so
    admission and announcement are asserted together and never separately.
    `absent` names what the admission must not claim.
    """

    result = execute_release_check_gate(
        source,
        {},
        mutate_fixture=mutate_fixture,
        inherited=inherited,
        annotations=annotations,
    )
    output = result.stdout + result.stderr
    if result.returncode != 0:
        raise AssertionError(
            f"release gate refused an admissible fixture: {label}: {output}"
        )
    for needle in expected:
        if needle not in output:
            raise AssertionError(
                f"release gate admitted {label} silently, expected {needle!r}: "
                f"{output}"
            )
    for needle in absent:
        if needle in output:
            raise AssertionError(
                f"release gate admitted {label} claiming {needle!r}: {output}"
            )


def run_tag_selector(
    manifest: str,
    candidates: str,
    minting_tag: str = "",
) -> tuple[subprocess.CompletedProcess[str], str]:
    """Execute the shipped admission selector against fixture inputs."""

    with tempfile.TemporaryDirectory() as directory:
        fixture = Path(directory)
        manifest_path = fixture / "abandoned-release-tags.json"
        manifest_path.write_text(manifest, encoding="utf-8")
        candidates_path = fixture / "release-tags"
        candidates_path.write_text(candidates, encoding="utf-8")
        selected_path = fixture / "highest-admissible-tag"
        completed = subprocess.run(
            [
                sys.executable,
                str(TAG_SELECTOR),
                str(manifest_path),
                str(candidates_path),
                minting_tag,
                str(selected_path),
            ],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        selected = ""
        if selected_path.exists():
            selected = selected_path.read_text(encoding="utf-8")
    return completed, selected


def assert_admissible_tag(
    label: str,
    manifest: str,
    candidates: str,
    expected: str,
    *,
    minting_tag: str = "",
) -> None:
    """Require the selector to name exactly the tag the rail must wait on."""

    completed, selected = run_tag_selector(manifest, candidates, minting_tag)
    if completed.returncode != 0:
        raise AssertionError(
            f"release-lane admission refused {label}: "
            f"{completed.stdout}{completed.stderr}"
        )
    if selected != expected:
        raise AssertionError(
            f"release-lane admission selected {selected or '<none>'} for "
            f"{label}, expected {expected or '<none>'}"
        )


def assert_tag_selection_refused(
    label: str,
    manifest: str,
    candidates: str,
    expected_error: str,
    *,
    minting_tag: str = "",
) -> None:
    """Require a loud, named refusal rather than a silently degraded skip."""

    completed, selected = run_tag_selector(manifest, candidates, minting_tag)
    output = completed.stdout + completed.stderr
    if completed.returncode == 0:
        raise AssertionError(f"release-lane admission accepted {label}: {output}")
    if expected_error not in output:
        raise AssertionError(
            f"release-lane admission refused {label} for the wrong reason: {output}"
        )
    if selected:
        raise AssertionError(
            f"release-lane admission named {selected} while refusing {label}"
        )


def abandonment_manifest(*records: dict[str, str]) -> str:
    """Serialize an abandonment record the way the tracked file carries it."""

    return json.dumps({"schema_version": 1, "abandoned": list(records)})


def assert_trusted_abandonment_reads(workflow: str, content: str) -> None:
    """Both admission policies must come from protected main, nowhere else."""

    for policy in (ABANDONED_TAGS_POLICY, TAG_SELECTOR_POLICY):
        index = content.find(policy)
        if index < 0:
            raise AssertionError(f"{workflow} never reads {policy}")
        while index >= 0:
            prefix = content[max(0, index - len(TRUSTED_POLICY_PREFIX)) : index]
            if prefix != TRUSTED_POLICY_PREFIX:
                raise AssertionError(
                    f"{workflow} reads {policy} from something other than "
                    "protected main. The release commit under admission can "
                    "predate the abandonment it must honour, so reading either "
                    "policy from the checkout re-deadlocks the lane it unblocks"
                )
            index = content.find(policy, index + 1)


def assert_admission_step_order(
    workflow: str,
    content: str,
    step_anchor: str,
    comparison: str,
) -> None:
    """The waiver must be read, then applied, before the lane predicate runs."""

    step = workflow_step_source(workflow, content, step_anchor)
    manifest_read = step.index(f"{TRUSTED_POLICY_PREFIX}{ABANDONED_TAGS_POLICY}")
    selector_read = step.index(f"{TRUSTED_POLICY_PREFIX}{TAG_SELECTOR_POLICY}")
    selection = step.index('python3 "$selector"')
    adoption = step.index('highest_tag="$(cat "$admissible")"')
    predicate = step.index(comparison)
    if not manifest_read < selector_read < selection < adoption < predicate:
        raise AssertionError(
            f"{workflow} must read the reviewed abandonment record and its "
            "selector from protected main, resolve the highest admissible tag, "
            "and only then admit against finalized GitHub Latest"
        )
    if "git tag --list" in step:
        raise AssertionError(
            f"{workflow} still ranks release tags without consulting the "
            "reviewed abandonment record"
        )


def selector_invocation(workflow: str, content: str) -> tuple[str, ...]:
    """Return the exact argument list a workflow hands the admission selector.

    Every invocation is read, not the first. A second one is what would defeat
    this pin: it can re-run the selector with any argument at all and overwrite
    the file the workflow then adopts as the highest admissible tag, so pinning
    only the first would leave the guard describing bytes that no longer decide
    the outcome.
    """

    anchor = 'python3 "$selector"'
    invocations: list[tuple[str, ...]] = []
    for match in re.finditer(re.escape(anchor), content):
        tail = content[match.end() :]
        if not tail.startswith(" \\\n"):
            raise AssertionError(
                f"{workflow} invokes the admission selector without a "
                "reviewable multi-line argument list"
            )
        arguments: list[str] = []
        for line in tail[len(" \\\n") :].splitlines():
            argument = line.strip()
            if argument.endswith("\\"):
                arguments.append(argument[:-1].strip())
                continue
            arguments.append(argument)
            break
        invocations.append(tuple(arguments))
    if len(invocations) != 1:
        raise AssertionError(
            f"{workflow} must invoke the admission selector exactly once, so "
            "one reviewed argument list decides the highest admissible tag: "
            f"found {len(invocations)}"
        )
    return invocations[0]


def assert_tag_readback_retries(release_tag: str) -> None:
    """A minted tag must never be reported failed because a read was early."""

    source = tag_readback_source(release_tag)
    good = (0, RELEASE_GATE_FIXTURE_SHA)
    missing = (404, "")

    immediate = execute_tag_readback(source, [good])
    if immediate.returncode != 0:
        raise AssertionError(
            f"post-mint readback failed on a readable ref: "
            f"{immediate.stdout}{immediate.stderr}"
        )

    # The observed defect: the ref exists, the first reads 404, and the mint
    # concluded failure on a release it had already created.
    delayed = execute_tag_readback(source, [missing, missing, good])
    if delayed.returncode != 0:
        raise AssertionError(
            "post-mint readback reported a successful mint as failed because "
            f"the ref was not visible yet: {delayed.stdout}{delayed.stderr}"
        )
    if "retrying" not in delayed.stdout:
        raise AssertionError(
            "post-mint readback must say it is retrying, so a slow read is "
            f"legible in the log: {delayed.stdout}"
        )

    absent = execute_tag_readback(source, [missing])
    if absent.returncode == 0:
        raise AssertionError("post-mint readback accepted a ref that never appeared")
    if "never read back" not in absent.stdout:
        raise AssertionError(
            "post-mint readback must distinguish exhausted reads from a "
            f"mismatch: {absent.stdout}{absent.stderr}"
        )
    # The reason the API gave has to survive into the refusal. A persistent
    # auth or rate-limit failure reads identically to a missing ref from here,
    # and reporting it as absence sends an operator after a tag that exists.
    if "last reason:" not in absent.stdout or "Not Found" not in absent.stdout:
        raise AssertionError(
            "post-mint readback must report why the read failed rather than "
            f"asserting the tag is gone: {absent.stdout}"
        )

    # A ref that reads back pointing elsewhere is terminal on the first read.
    # Retrying it would be waiting for someone else's tag to change.
    mismatch = execute_tag_readback(source, [(0, "0" * 40)])
    if mismatch.returncode == 0:
        raise AssertionError("post-mint readback accepted a ref at the wrong commit")
    if "points at" not in mismatch.stdout:
        raise AssertionError(
            f"post-mint readback must name the wrong commit: {mismatch.stdout}"
        )
    if "retrying" in mismatch.stdout:
        raise AssertionError(
            "post-mint readback retried a ref that resolved to another commit, "
            "which cannot become correct by waiting"
        )


def assert_ruleset_mirror_stays_a_superset(release_tag: str) -> None:
    """The reviewed mirror must never shrink below what the mint vetoes on.

    `missing_from_ruleset` runs unconditionally in the mint and refuses when the
    mint requires a context the reviewed mirror does not gate. `REQUIRED_CHECKS`
    is the veto set and `RULESET_REQUIRED_CHECKS` is the mirror of live ruleset
    19746451, so the mirror has to stay a superset of the veto set forever, and
    the day it stops being one EVERY release refuses.

    That is not a theoretical edit. The admission core thins the live ruleset to a short
    admission list, which makes "sync the mirror to the ruleset" the obvious
    next tidy-up and makes it fatal. Nothing caught it before this: the mint's
    own refusal is driven here only against a synthetic environment, so the
    file's actual mirror content was unread, and a shrunken mirror landed green
    and failed at the next mint with no pull request to blame.

    The mirror may hold MORE than the veto set. That is its purpose: a context a
    ruleset gates and the mint does not veto on still has to be published by an
    admitted build, or the queue waits on a name nobody produces.
    """

    def block(key: str) -> tuple[str, ...]:
        found = re.search(
            rf"\n          {key}: \|\n((?:            \S.*\n)+)", release_tag
        )
        if found is None:
            raise AssertionError(
                f"release-tag.yml no longer declares {key} as a block scalar"
            )
        return tuple(
            line.strip() for line in found.group(1).splitlines() if line.strip()
        )

    veto = block("REQUIRED_CHECKS")
    mirror = block("RULESET_REQUIRED_CHECKS")
    dropped = sorted(set(veto) - set(mirror))
    if dropped:
        raise AssertionError(
            "the reviewed ruleset mirror must stay a superset of the checks the "
            f"mint vetoes on; it no longer gates {dropped}, which makes "
            "missing_from_ruleset non-empty and refuses EVERY release"
        )


def assert_required_check_set_is_single_sourced(release_tag: str) -> None:
    """The env list and the provenance table must name the same contexts.

    Dropping a context from the env alone leaves the table intact, so every
    literal needle still matches and the suite stays green while the mint has
    quietly stopped requiring a release-critical check. The workflow's own
    order check fails that closed at runtime, but only after the drift has
    landed and started burning mint attempts, which is a log to read rather
    than a red PR.
    """

    workflow = ".github/workflows/release-tag.yml"
    step = workflow_step_source(
        workflow,
        release_tag,
        "      - name: Verify required checks are green",
    )
    block = re.search(
        r"\n          REQUIRED_CHECKS: \|\n((?:            \S.*\n)+)",
        step,
    )
    if block is None:
        raise AssertionError(
            f"{workflow} no longer declares REQUIRED_CHECKS as a block scalar"
        )
    env_names = tuple(
        line.strip() for line in block.group(1).splitlines() if line.strip()
    )
    table = re.search(
        r"\nexpected_provenance = \{\n(.*?)\n\}\n",
        release_check_gate_source(release_tag),
        re.DOTALL,
    )
    if table is None:
        raise AssertionError(
            f"{workflow} no longer declares an expected_provenance table"
        )
    table_names = tuple(
        re.findall(r'^    "(.+?)": \(', table.group(1), re.MULTILINE)
    )
    if env_names != REQUIRED_RELEASE_CHECKS:
        raise AssertionError(
            "the release gate's REQUIRED_CHECKS env no longer matches the "
            f"reviewed required set: {env_names}"
        )
    if table_names != REQUIRED_RELEASE_CHECKS:
        raise AssertionError(
            "the release gate's expected_provenance table no longer matches "
            f"the reviewed required set: {table_names}"
        )


def decline_escalation_source(release_tag: str) -> str:
    """Extract the persistent-decline shell exactly as the mint runs it."""

    step = workflow_step_source(
        ".github/workflows/release-tag.yml",
        release_tag,
        "      - name: Escalate a persistent mint decline",
    )
    marker = "        run: |\n"
    if marker not in step:
        raise AssertionError("persistent-decline step no longer has a shell body")
    return textwrap.dedent(step[step.index(marker) + len(marker) :])


def execute_decline_escalation(
    source: str,
    prior_runs: list[tuple[int, str, str | None]],
) -> subprocess.CompletedProcess[str]:
    """Run the decline counter against production-shaped workflow/job payloads.

    Each tuple is `(run id, Mint release tag job conclusion, escalation-step
    conclusion)`. A `None` marker models a whole job skipped by the workflow's
    job-level `workflow_run` guard; that is the high-volume production shape
    that must be ignored rather than mistaken for a successful mint.
    """

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        script = root / "decline.sh"
        script.write_text(source, encoding="utf-8")
        responses = root / "responses"
        responses.mkdir()
        list_path = responses / (
            "repos_kinlab-ai_kin_actions_workflows_release-tag.yml_runs"
        )
        list_path.write_text(
            json.dumps({"workflow_runs": [{"id": run_id} for run_id, _, _ in prior_runs]}),
            encoding="utf-8",
        )
        for run_id, job_conclusion, marker in prior_runs:
            steps = []
            if marker is not None:
                steps.append(
                    {
                        "name": "Escalate a persistent mint decline",
                        "conclusion": marker,
                    }
                )
            payload = {
                "jobs": [
                    {
                        "name": "Mint release tag",
                        "conclusion": job_conclusion,
                        "steps": steps,
                    }
                ]
            }
            (responses / f"repos_kinlab-ai_kin_actions_runs_{run_id}_jobs").write_text(
                json.dumps(payload),
                encoding="utf-8",
            )

        binaries = root / "bin"
        binaries.mkdir()
        (binaries / "gh").write_text(
            textwrap.dedent(
                """\
                #!/usr/bin/env bash
                set -uo pipefail
                [ "$1" = api ] || exit 2
                shift
                path=""; query=""
                while [ $# -gt 0 ]; do
                  case "$1" in
                    -q|--jq) shift; query="$1" ;;
                    -*) ;;
                    *) [ -n "$path" ] || path="$1" ;;
                  esac
                  shift
                done
                path="${path%%\\?*}"
                file="$FIXTURE/responses/$(printf '%s' "$path" | tr '/' '_')"
                if [ ! -f "$file" ]; then
                  echo "gh: Not Found (HTTP 404)" >&2
                  exit 1
                fi
                if [ -n "$query" ]; then
                  jq -r "$query" < "$file"
                else
                  cat "$file"
                fi
                """
            ),
            encoding="utf-8",
        )
        (binaries / "gh").chmod(0o755)
        environment = dict(os.environ)
        environment["PATH"] = f"{binaries}{os.pathsep}{environment['PATH']}"
        environment.update(
            {
                "FIXTURE": str(root),
                "GH_TOKEN": "fixture",
                "REPO": "kinlab-ai/kin",
                "CURRENT_RUN_ID": "99999",
                "DECLINE_REASON": "unrecovered-latest",
                "DECLINE_LIMIT": "4",
                "DECLINE_SCAN_LIMIT": "100",
                "GITHUB_STEP_SUMMARY": str(root / "summary.md"),
            }
        )
        return subprocess.run(
            ["bash", str(script)],
            capture_output=True,
            text=True,
            env=environment,
            timeout=30,
            check=False,
        )


def assert_soft_decline_is_legible(release_tag: str) -> None:
    """A decline exits the job green, so it has to be loud, named, and counted.

    The failure this pins is not a wrong answer but an invisible one: the mint
    declining to mint concluded success and read on the board exactly like a
    mint that happened, so a blocked release stayed hidden until somebody read
    the step conclusions by hand.
    """

    workflow = ".github/workflows/release-tag.yml"
    admission = workflow_step_source(
        workflow,
        release_tag,
        "      - name: Admit the serialized release lane",
    )
    for needle in (
        "decline() { # <reason> <sentence>",
        "printf '::warning::release mint declined (%s): %s\\n' \"$1\" \"$2\"",
        '} >> "$GITHUB_STEP_SUMMARY"',
        'echo "decline_reason=$1"',
    ):
        require(admission, needle, f"{workflow} soft-decline legibility")

    # Every automatic decline goes through the helper. A bare `ready=false`
    # beside a `::notice::` is the exact shape that made a blocked release
    # indistinguishable from a healthy one, so exactly one write may exist and
    # it is the helper's.
    ready_false_writes = admission.count('echo "ready=false"')
    if ready_false_writes != 1:
        raise AssertionError(
            f"{workflow} must write ready=false only through the decline helper, "
            f"so every decline is announced and named: found {ready_false_writes}"
        )
    declines = re.findall(r"^\s+decline [a-z][a-z-]* ", admission, re.MULTILINE)
    if len(declines) != 3:
        raise AssertionError(
            f"{workflow} has three automatic soft-decline branches, each of "
            f"which must announce itself: found {len(declines)}"
        )

    escalation_name = "Escalate a persistent mint decline"
    escalation = workflow_step_source(
        workflow,
        release_tag,
        f"      - name: {escalation_name}",
    )
    if "steps.prior.outputs.ready == 'false'" not in escalation:
        raise AssertionError(
            f"{workflow} must escalate only on the decline path, so an ordinary "
            "mint is never held against a decline budget"
        )
    # The counter's only decline marker is this step's own conclusion on prior
    # runs, so the name it selects on and the name it runs under have to be the
    # same string. Renaming the step without renaming the query would silently
    # reset the count to one on every run and never escalate again.
    if f'select(.name == "{escalation_name}")' not in escalation:
        raise AssertionError(
            f"{workflow} escalation must count prior runs by its own step name, "
            f"which is the only durable record of a decline: {escalation_name}"
        )
    limit = re.search(r'DECLINE_LIMIT: "(\d+)"', escalation)
    if limit is None or int(limit.group(1)) < 2:
        raise AssertionError(
            f"{workflow} needs a decline budget above one, because a single "
            "decline while a release is in flight is correct behaviour"
        )

    source = decline_escalation_source(release_tag)
    # `workflow_run` creates skipped controller jobs for unrelated CI
    # completions. They interleave with real scheduled declines in production
    # and must not reset the streak. The previous implementation stopped at the
    # first such run, so this fixture makes the fourth real decline fail only
    # when the skipped jobs are actively ignored.
    interleaved = execute_decline_escalation(
        source,
        [
            (910, "skipped", None),
            (909, "success", "success"),
            (908, "skipped", None),
            (907, "success", "success"),
            (906, "skipped", None),
            (905, "failure", "failure"),
        ],
    )
    if interleaved.returncode == 0 or "declined 4 times in a row" not in (
        interleaved.stdout + interleaved.stderr
    ):
        raise AssertionError(
            "whole-job workflow_run skips reset the persistent-decline counter: "
            f"{interleaved.stdout}{interleaved.stderr}"
        )

    # A mint job that really ran and skipped the escalation step is a genuine
    # non-decline (mint/no-op/refusal elsewhere), so it must reset the streak
    # even when older declines exist behind it.
    reset = execute_decline_escalation(
        source,
        [
            (920, "success", "skipped"),
            (919, "success", "success"),
            (918, "failure", "failure"),
            (917, "success", "success"),
        ],
    )
    if reset.returncode != 0 or "1 in a row" not in reset.stdout:
        raise AssertionError(
            "a completed non-decline mint no longer resets decline escalation: "
            f"{reset.stdout}{reset.stderr}"
        )


def assert_selector_arguments(
    release_tag: str,
    release_train: str,
    release_recovery: str,
) -> None:
    """Pin which tag each workflow declares it is about to mint."""

    actual = {
        "release-tag": selector_invocation("release-tag", release_tag),
        "release-train": selector_invocation("release-train", release_train),
        "release-recovery": selector_invocation(
            "release-recovery", release_recovery
        ),
    }
    if actual != EXPECTED_SELECTOR_INVOCATIONS:
        raise AssertionError(
            "each release workflow must hand the admission selector its own "
            "reviewed arguments. The mint-intent argument names the tag that "
            "workflow is about to create, and only the mint creates one. The "
            "train resolves drift from a base tag it never mints, and recovery "
            "reconciles a tag it never mints either, so naming that tag as "
            "mint intent refuses exactly when a record covers it, "
            f"which is every abandonment: expected={EXPECTED_SELECTOR_INVOCATIONS} "
            f"actual={actual}"
        )


def assert_mint_trigger_survives_advisory_flakes(release_tag: str) -> None:
    """The event-driven mint must evaluate on any completed main-push CI run.

    ci.yml carries jobs beyond the release-critical ones, so its aggregate
    conclusion goes red whenever a non-required advisory leg flakes, which is
    most main pushes. Gating the workflow_run trigger on that aggregate made
    the event path hostage to a flake and left the schedule as the only
    automatic mint, which GitHub then failed to deliver for roughly four hours.
    The mint's own "Verify required checks are green" step is the release
    authority: it binds each reviewed required context to its exact producing
    workflow and explicitly refuses to let that producer's aggregate conclusion
    veto through it. A trigger that re-imposes the aggregate contradicts the
    gate it precedes and can only ever subtract mint occasions.
    """

    block = workflow_job_blocks(release_tag).get("mint-release-tag")
    if block is None:
        raise AssertionError("release-tag no longer declares the mint job")
    guard: list[str] = []
    inside = False
    for line in block.splitlines():
        stripped = line.strip()
        if not inside:
            if stripped.startswith("if:"):
                inside = True
                guard.append(stripped)
            continue
        if stripped.startswith("runs-on:"):
            break
        if stripped.startswith("#"):
            continue
        guard.append(stripped)
    active = "\n".join(guard)
    if not inside:
        raise AssertionError("the release mint no longer guards its triggers")
    if "github.event.workflow_run.conclusion" in active:
        raise AssertionError(
            "the release mint's workflow_run trigger must not consult the CI "
            "run's aggregate conclusion: a single non-required advisory flake "
            "reds that aggregate and would silence the event-driven mint, "
            "leaving only the schedule GitHub is free not to deliver"
        )
    for clause in (
        "github.event.workflow_run.event == 'push'",
        "github.event.workflow_run.head_branch == 'main'",
        # The queue's own CI run is the occasion that removes the wait: it
        # concludes when the landing does, roughly half an hour before the
        # landing push's run concludes, and the mint now keys off exactly the
        # sha that build proved. Losing this clause does not break a release,
        # which is what makes it worth pinning: it silently restores the
        # half-hour of pure waiting the rekey exists to delete.
        "github.event.workflow_run.event == 'merge_group'",
        "startsWith(github.event.workflow_run.head_branch, 'gh-readonly-queue/main/')",
    ):
        if clause not in active:
            raise AssertionError(
                "the release mint's workflow_run trigger must pin both reviewed "
                f"occasions it evaluates: {clause}"
            )
    if "gh-readonly-queue/'" in active or "'gh-readonly-queue/')" in active:
        raise AssertionError(
            "the release mint's queue trigger must pin the merge-queue ref for "
            "main, not any merge-queue ref in the repository"
        )

    # A widened trigger must not widen what the mint may release. The event's
    # own head sha is the only stale-capable input into the candidate: a rerun
    # of an older CI run carries the sha main held then, so resolving from it
    # selects the version of that moment rather than the version main carries
    # now, which is a class the scheduled arm structurally cannot reach. Both
    # automatic arms therefore read one freshly fetched protected main, and the
    # trigger stays a decision about when to look.
    resolve = workflow_step_source(
        "release-tag",
        release_tag,
        "      - name: Resolve exact coherent release commit\n",
    )
    if "workflow_run.head_sha" in resolve:
        raise AssertionError(
            "the release mint must not resolve its release candidate from the "
            "triggering CI run's head sha, because a rerun of an older run "
            "carries a stale one and the trigger decides only when to look"
        )
    if 'sha="$(git rev-parse refs/remotes/origin/main)"' not in resolve:
        raise AssertionError(
            "the release mint's automatic path must resolve its candidate from "
            "the freshly fetched protected main it just verified"
        )


def assert_recovery_record_authority(release_recovery: str) -> None:
    """Recovery must decide abandonment through the rail's own selector.

    Two readers of one record is how an alarm gets disarmed in the state that
    needs it most. A reader of its own accepts entries the selector refuses, so
    recovery would stand down for a record too malformed to waive the mint: the
    rail hard stuck, and the check-run that would say so green. Recovery
    therefore asks the selector the same question the mint asks, and this pins
    that it keeps asking rather than reading the record itself.

    Recovery is deliberately absent from the protected-main read above. The mint
    and the train run on a release commit that predates the abandonment it has
    to honour, so they must reach past their checkout; recovery checks out
    GITHUB_SHA, which is the default-branch commit itself, and honours no
    abandonment from a tree whose HEAD is not that commit.
    """

    step = workflow_step_source(
        "release-recovery",
        release_recovery,
        "      - name: Reconcile against the tracked abandonment record\n",
    )
    for policy in (TAG_SELECTOR_POLICY, 'python3 "$selector"'):
        require(step, policy, "record-aware release recovery")
    for reader in ("jq ", "python3 -c"):
        if reader in step:
            raise AssertionError(
                "release recovery must decide abandonment through "
                f"{TAG_SELECTOR_POLICY} and not read the record a second way, "
                "because a second reader accepts what the rail refuses and "
                f"quiets the alarm the refusal exists to raise: {reader.strip()}"
            )
    # The selector answers by ranking, and it drops a candidate that is not a
    # vX.Y.Z release tag for the same reason it drops an abandoned one. An empty
    # ranking therefore means "waived" only while the tag handed to it is known
    # to be a release tag, which resolve establishes before it declares the
    # reconcile needed. The order is what makes it true at the ranking.
    resolve = workflow_step_source(
        "release-recovery",
        release_recovery,
        "      - name: Resolve exact retry candidate\n",
    )
    shape = resolve.index(r'[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]')
    declared = resolve.index('echo "needed=true"')
    if shape > declared:
        raise AssertionError(
            "release recovery must prove its retry candidate is a vX.Y.Z "
            "release tag before it declares a reconcile needed, because the "
            "admission selector drops anything else from its ranking and "
            "recovery reads an empty ranking as a reviewed waiver"
        )


def assert_release_proof_key_authority(
    release_train: str,
    release_tag: str,
    release: str,
    proof_gate: str,
) -> None:
    """Pin the release evidence to a commit nothing rewrites, and to the tag.

    This guard did not exist while the thing it guards was load-bearing. The
    earlier proof gate lived in release-train.yml keyed on the head of
    automation/release-next, the train mints a new head on every cycle that
    finds drift, and nothing in this file noticed either fact. Removing that
    step outright left the whole suite green, which is how a release chain
    acquires a rule that only a runbook sentence enforces.

    What is pinned now is the arrangement that replaced it. The candidate is a
    main commit, so nothing can rewrite it out from under its records; the mint
    refuses to write the tag ref without them; the tag names the exact sha the
    gate verified; and the train keys nothing on a branch head any more, which
    is the property that lets main keep moving through a proof window.
    """

    mint = workflow_job_blocks(release_tag).get("mint-release-tag")
    if mint is None:
        raise AssertionError("release-tag.yml no longer declares the mint job")

    gate_anchor = "- name: Require proof-loop artifacts for the release candidate"
    tag_write_anchor = "- name: Create release tag ref"
    for anchor in (gate_anchor, tag_write_anchor):
        if mint.count(anchor) != 1:
            raise AssertionError(
                f"the mint must declare exactly one {anchor!r} step, so the "
                "release proof gate has one reviewed place to live"
            )
    if mint.index(gate_anchor) > mint.index(tag_write_anchor):
        raise AssertionError(
            "the release proof gate must run before the tag ref is written; a "
            "tag run resolves its workflows from the tag and can never be "
            "repaired, so a gate after the write refuses nothing"
        )

    gate_step = "\n".join(
        job_step_active_lines(release_tag, "mint-release-tag", gate_anchor)
    )
    for needle, duty in (
        (
            f'"refs/remotes/origin/main:{PROOF_GATE_POLICY}"',
            "read the gate from protected main rather than from the commit "
            "under judgement",
        ),
        (
            'CANDIDATE_SHA="$SHA"',
            "ask about the exact sha it is going to tag",
        ),
        (
            'if [ "$proven" != "$SHA" ]',
            "refuse when the sha the gate verified is not the sha it would tag",
        ),
        (
            'git diff --name-only "$proven" "$SHA"',
            "state that the tree it tags is the tree that was proven",
        ),
    ):
        if needle not in gate_step:
            raise AssertionError(
                f"the release proof gate must {duty}: {needle}"
            )

    resolve = "\n".join(
        job_step_active_lines(
            release_tag, "mint-release-tag", "name: Resolve exact coherent release"
        )
    )
    for needle, duty in (
        (
            "git/trees/release-evidence?recursive=1",
            "select the candidate from the records the proof loop published",
        ),
        (
            '.truncated // false',
            "refuse a truncated listing rather than shipping the older "
            "candidate it can still see",
        ),
        (
            'if ! listing="$(gh api',
            "test the status of the listing read, because a transport "
            "failure, a GitHub error object and an empty evidence branch "
            "otherwise print the same green no-op and only one of them means "
            "there is nothing to release",
        ),
        (
            '2>"$read_error"',
            "keep the error text from the listing read, because a refusal "
            "that cannot say what went wrong is one an operator has to "
            "reproduce before they can act on it",
        ),
        (
            'echo "needed=false"',
            "stand down as a no-op when no candidate is proven, rather than "
            "declining or tagging one that is not",
        ),
    ):
        if needle not in resolve:
            raise AssertionError(
                f"the mint's candidate selection must {duty}: {needle}"
            )

    reconcile = workflow_job_blocks(release_train).get("reconcile")
    if reconcile is None:
        raise AssertionError("release-train.yml no longer declares the reconcile job")

    if PROOF_GATE_POLICY in workflow_active_text(reconcile):
        raise AssertionError(
            "the release train must key nothing on the version bump branch's "
            "head; that key is what forced the fleet to freeze main for the "
            "length of every proof window, and the refusal now lives on the "
            "tag the mint writes"
        )

    # The removed proof step was the only code that ever drafted or un-drafted
    # the bump pull request, and it drafted it while it held. The arm step now
    # runs on drift alone, so without this a draft left behind by that step
    # arms auto-merge on a pull request the merge queue never takes: the bump
    # never lands, no candidate is ever provable, and the plan step's all-clear
    # marker reports a healthy rail every fifteen minutes while nothing moves.
    arm = "\n".join(
        job_step_active_lines(
            release_train, "reconcile", "name: Arm protected auto-merge"
        )
    )
    for needle, duty in (
        (
            'gh pr ready "$PR"',
            "un-draft the bump pull request before it arms auto-merge, "
            "because the merge queue never takes a draft and nothing else "
            "here un-drafts one any more",
        ),
        (
            "--json headRefOid,isDraft",
            "read the draft state in the same call that reads the head, so "
            "the check costs no extra round trip and cannot go stale between "
            "two of them",
        ),
    ):
        if needle not in arm:
            raise AssertionError(
                f"the release train's arm step must {duty}: {needle}"
            )
    if arm.index('gh pr ready "$PR"') > arm.index('gh pr merge "$PR"'):
        raise AssertionError(
            "the release train must un-draft the bump pull request BEFORE it "
            "arms auto-merge; arming first is what leaves a draft armed and "
            "unmergeable while the rail reports itself clear"
        )

    promote = "\n".join(
        job_step_active_lines(
            release, "finalize_release", "name: Require proof-loop artifacts"
        )
    )
    for needle, duty in (
        (
            "CANDIDATE_SHA: ${{ github.sha }}",
            "ask about the tagged commit directly, because the mint now tags "
            "the candidate itself",
        ),
        (
            "RESOLVE_FROM_COMMIT: ${{ github.sha }}",
            "keep the bridge, which is what makes a tag with no direct record "
            "refuse by naming where it came from rather than only naming the "
            "file that was missing",
        ),
        (
            "process.stdout.write(result.sha)",
            "read back the sha the gate verified rather than only its exit "
            "status, because the exit status alone cannot say which commit "
            "the evidence was about",
        ),
        (
            'if [ "$proven" != "$CANDIDATE_SHA" ]',
            "refuse when the sha the gate verified is not the sha it "
            "promotes; the mint makes that comparison before it writes the "
            "ref, and without it here a bridged answer flips a tag to Latest "
            "on evidence about a different commit and a different tree",
        ),
    ):
        if needle not in promote:
            raise AssertionError(
                f"the promote gate must {duty}: {needle}"
            )

    gate_source = "\n".join(active_lines(proof_gate))

    # The bridge is the one path in the gate that can answer about a sha nobody
    # named, so it is bounded to the branch that is the only place a record was
    # ever published. The bound is read out of the release train rather than
    # written down twice: renaming the bump branch there must fail here until
    # the gate follows, because a bridge pointed at a branch that no longer
    # exists resolves the head of whatever feature pull request produced the
    # tagged commit, and that head never carried a record.
    bump_branch = re.search(
        r"^BRANCH: (\S+)$", workflow_active_text(reconcile), re.MULTILINE
    )
    if bump_branch is None:
        raise AssertionError(
            "release-train.yml no longer names the bump branch it writes, so "
            "the proof gate's bridge cannot be bounded to it"
        )
    if f"export const BUMP_BRANCH = '{bump_branch.group(1)}';" not in gate_source:
        raise AssertionError(
            "the proof gate's bridge must be bounded to the same bump branch "
            f"the release train writes ({bump_branch.group(1)})"
        )
    if (
        "produced.filter((pull) => pull?.head?.ref === BUMP_BRANCH)"
        not in gate_source
    ):
        raise AssertionError(
            "the proof gate must apply the bump-branch bound when it bridges, "
            "or a tag whose records were removed resolves the head of the "
            "feature pull request that produced it and asks about a build "
            "nobody proved"
        )

    if "absent.evidenceAbsent = true" not in gate_source:
        raise AssertionError(
            "the proof gate must mark an absent record as absent, so a caller "
            "deciding to bridge is not matching on an error message"
        )
    if "if (!error.evidenceAbsent || !resolveFromCommit)" not in gate_source:
        raise AssertionError(
            "the proof gate must bridge only when the direct record is ABSENT; "
            "widening the search on an unreadable record is a check reporting "
            "success for the wrong reason"
        )


def assert_abandoned_tag_admission(
    release_tag: str,
    release_train: str,
    release_recovery: str,
) -> None:
    """Only a reviewed record may waive release-lane serialization."""

    assert_selector_arguments(release_tag, release_train, release_recovery)
    assert_recovery_record_authority(release_recovery)

    for workflow, content, step_anchor, comparison in (
        (
            "release-tag",
            release_tag,
            "      - name: Admit the serialized release lane\n",
            '[ "$highest_tag" != "$latest_tag" ]',
        ),
        (
            "release-train",
            release_train,
            "      - name: Resolve releasable drift and SemVer intent\n",
            '[ "$highest_tag" != "$latest_tag" ]',
        ),
    ):
        assert_trusted_abandonment_reads(workflow, content)
        assert_admission_step_order(workflow, content, step_anchor, comparison)

    unbuildable = "1" * 40
    superseding = "2" * 40
    stable = "3" * 40
    older = "4" * 40
    unversioned = "5" * 40
    # A non-release tag ahead of every version tag, because the version filter
    # has to survive whatever else the repository has tagged.
    candidates = (
        f"nightly {unversioned}\n"
        f"v0.4.3 {unbuildable}\n"
        f"v0.3.6 {stable}\n"
        f"v0.3.5 {older}\n"
    )
    record = {
        "tag": "v0.4.3",
        "sha": unbuildable,
        "reason": "the tagged lockfile pins a dependency that cannot build",
        "superseded_by": "v0.4.4",
        "failed_release_run_id": "30627672394",
    }

    # The invariant this whole gate exists for: a tag that is only failing so
    # far is not skippable, so it still holds its successor. Nothing but a
    # reviewed record changes that, and a record that does not describe the tag
    # in the repository is not one.
    assert_admissible_tag(
        "an unlisted tag whose release keeps failing",
        abandonment_manifest(),
        candidates,
        "v0.4.3",
    )
    assert_admissible_tag(
        "an abandonment naming a different tag",
        abandonment_manifest({**record, "tag": "v0.3.5", "sha": older}),
        candidates,
        "v0.4.3",
    )
    assert_admissible_tag(
        "an abandonment of a tag that no longer exists",
        abandonment_manifest({**record, "tag": "v0.9.9"}),
        candidates,
        "v0.4.3",
    )
    assert_tag_selection_refused(
        "an abandonment whose tag has since moved",
        abandonment_manifest({**record, "sha": superseding}),
        candidates,
        f"abandonment of v0.4.3 waives {superseding} but the tag now names",
    )

    # With the record present the unbuildable tag stops holding the lane, and
    # ranking otherwise stays exactly what git already decided.
    assert_admissible_tag(
        "a reviewed abandonment",
        abandonment_manifest(record),
        candidates,
        "v0.3.6",
    )
    assert_admissible_tag(
        "consecutive reviewed abandonments",
        abandonment_manifest(
            record,
            {**record, "tag": "v0.3.6", "sha": stable, "superseded_by": "v0.4.4"},
        ),
        candidates,
        "v0.3.5",
    )
    assert_admissible_tag(
        "an abandonment of every version tag",
        abandonment_manifest(
            record,
            {**record, "tag": "v0.3.6", "sha": stable, "superseded_by": "v0.4.4"},
            {**record, "tag": "v0.3.5", "sha": older, "superseded_by": "v0.4.4"},
        ),
        candidates,
        "",
    )
    # The mint-intent contract, in the state that actually occurs. A record is
    # written the moment a release is stuck, and main's version still equals the
    # stuck tag then, because the mint only ever creates `v$(workspace version)`
    # and the bump that moves main past it is the thing being unblocked. So the
    # tag under a fresh record IS the tag both workflows are holding. Naming it
    # as mint intent must refuse, and resolving drift without naming it must
    # walk past it. Getting this backwards turns the record's own scenario into
    # a permanently failing scheduled workflow.
    assert_tag_selection_refused(
        "minting a tag that is itself recorded as abandoned",
        abandonment_manifest(record),
        candidates,
        "v0.4.3 is recorded as an abandoned release tag and must not be released",
        minting_tag="v0.4.3",
    )
    assert_admissible_tag(
        "resolving drift in the same state, declaring no mint intent",
        abandonment_manifest(record),
        candidates,
        "v0.3.6",
        minting_tag="",
    )

    for label, manifest, expected_error in (
        ("a manifest that is not JSON", "{not json", "is not valid JSON"),
        ("a manifest that is a JSON array", "[]", "must be a JSON object"),
        (
            "a manifest declaring another schema",
            json.dumps({"schema_version": 2, "abandoned": []}),
            "must declare schema_version 1",
        ),
        (
            "a manifest carrying an unreviewed key",
            json.dumps({"schema_version": 1, "abandoned": [], "waive_all": True}),
            "unreviewed keys: waive_all",
        ),
        (
            "a manifest with no abandonment array",
            json.dumps({"schema_version": 1}),
            "must carry an 'abandoned' array",
        ),
        (
            "a manifest whose abandonments are an object",
            json.dumps({"schema_version": 1, "abandoned": {}}),
            "must carry an 'abandoned' array",
        ),
        (
            "an abandonment that is a bare tag string",
            json.dumps({"schema_version": 1, "abandoned": ["v0.4.3"]}),
            "every abandonment entry must be a JSON object",
        ),
        (
            "an abandonment carrying an unreviewed field",
            abandonment_manifest({**record, "force": "yes"}),
            "unreviewed fields: force",
        ),
        (
            "an abandonment with no reason",
            abandonment_manifest(
                {key: value for key, value in record.items() if key != "reason"}
            ),
            "missing required evidence: reason",
        ),
        (
            "an abandonment whose reason is blank",
            abandonment_manifest({**record, "reason": "   "}),
            "missing required evidence: reason",
        ),
        (
            "an abandonment with no superseding tag",
            abandonment_manifest(
                {
                    key: value
                    for key, value in record.items()
                    if key != "superseded_by"
                }
            ),
            "missing required evidence: superseded_by",
        ),
        (
            "an abandonment that is not a release tag",
            abandonment_manifest({**record, "tag": "0.4.3"}),
            "abandoned tag '0.4.3' is not a vX.Y.Z release tag",
        ),
        (
            "an abandonment with an abbreviated commit",
            abandonment_manifest({**record, "sha": unbuildable[:12]}),
            "must record the exact 40-hex commit it waives",
        ),
        (
            "an abandonment superseded by a non-release tag",
            abandonment_manifest({**record, "superseded_by": "main"}),
            "must name a vX.Y.Z superseding tag",
        ),
        (
            "an abandonment superseded by itself",
            abandonment_manifest({**record, "superseded_by": record["tag"]}),
            "cannot name itself as its successor",
        ),
        (
            "an abandonment citing no failed run",
            abandonment_manifest({**record, "failed_release_run_id": "0"}),
            "must cite the Release run id that failed",
        ),
        (
            "an abandonment recorded twice",
            abandonment_manifest(record, record),
            "v0.4.3 is recorded as abandoned more than once",
        ),
    ):
        assert_tag_selection_refused(label, manifest, candidates, expected_error)

    assert_tag_selection_refused(
        "a tag listing with no commit",
        abandonment_manifest(record),
        "v0.4.3\n",
        "release tag listing carries an unreadable record",
    )
    assert_tag_selection_refused(
        "a tag listed against two commits",
        abandonment_manifest(record),
        f"v0.3.6 {stable}\nv0.3.6 {older}\n",
        "release tag v0.3.6 was listed against two different commits",
    )

    # The tracked record itself is release authority, so it is exercised rather
    # than trusted: every entry it carries must survive the same validation and
    # actually stop holding the lane.
    shipped = ABANDONED_TAGS.read_text(encoding="utf-8")
    entries = json.loads(shipped)["abandoned"]
    if not entries:
        raise AssertionError(
            "the tracked abandonment record must stay a reviewed ledger of "
            "every tag the release rail has given up on"
        )
    floor = "v0.0.1"
    shipped_candidates = "".join(
        f"{entry['tag']} {entry['sha']}\n" for entry in entries
    )
    assert_admissible_tag(
        "the tracked abandonment record",
        shipped,
        f"{shipped_candidates}{floor} {'9' * 40}\n",
        floor,
    )



# ── inherited export evidence and the timed-out retry ─────────────────────
#
# A main commit that is an export pull request's squash, with the head's tree
# on the head's base, may be graded on the head's runs while main's own ci.yml
# push build has no verdict. The step "Resolve evidence inherited from the
# export pull request" asks scripts/select-release-candidate.py whether the
# commit qualifies; the gate then judges the head's required contexts as a
# third tier. These fixtures are shaped on PR 1848 and the squash 08d48c27 it
# landed as: on the head, secret-scan.yml publishes `gitleaks (full history)`
# from the branch push and a skipped twin from its pull_request run.

INHERIT_STEP_ANCHOR = "      - name: Resolve evidence inherited from the export pull request"
GATE_STEP_ANCHOR = "      - name: Verify required checks are green"
EXPORT_HEAD_SHA = "4" * 40
EXPORT_HEAD_REF = f"feature/public-export-kin-{EXPORT_HEAD_SHA}"
INHERITED_RECORD: dict[str, object] = {
    "sha": RELEASE_GATE_FIXTURE_SHA,
    "inherited": True,
    "reasons": [],
    "pull": 1848,
    "head_sha": EXPORT_HEAD_SHA,
    "head_ref": EXPORT_HEAD_REF,
    "base_sha": RELEASE_GATE_PARENT_SHA,
    "tree": "5" * 40,
    "runs": {"CI": 5_010, "Acceptance": 5_050},
    "retried": [],
}
REAL_RELEASE_CANDIDATE = json.loads(
    (ROOT / "scripts" / "fixtures" / "release-candidate" / "pr1848-08d48c27.json").read_text(
        encoding="utf-8"
    )
)
CI_PATH = ".github/workflows/ci.yml"
SAST_PATH = ".github/workflows/sast.yml"
SECRET_SCAN_PATH = ".github/workflows/secret-scan.yml"
MAIN_SUITES = {CI_PATH: 301, SAST_PATH: 302, SECRET_SCAN_PATH: 303, ACCEPTANCE_PATH: 304}
HEAD_SUITES = {CI_PATH: 501, SAST_PATH: 502, SECRET_SCAN_PATH: 503, ACCEPTANCE_PATH: 505}
WORKFLOW_IDS = {
    CI_PATH: 245_803_170,
    SAST_PATH: 251_549_972,
    SECRET_SCAN_PATH: 293_452_372,
    ".github/workflows/acceptance.yml": 339_594_603,
    ".github/workflows/pr-text-hygiene.yml": 328_945_626,
}


def real_cancel_message(kind: str) -> str:
    """GitHub's own annotation on the macOS arm64 leg of a real RC Build run."""

    builds = REAL_RELEASE_CANDIDATE["rc_builds"]
    build = builds["timed_out"][0] if kind == "timed_out" else builds[kind]
    job = next(job for job in build["jobs"] if job["name"] == "RC Build (kin-macos-aarch64)")
    return job["annotations"][0]["message"]


def gate_check(
    name: str,
    suite: int,
    head_sha: str,
    check_id: int,
    *,
    status: str = "completed",
    conclusion: str | None = "success",
) -> dict[str, object]:
    return {
        "name": name,
        "status": status,
        "conclusion": conclusion,
        "id": check_id,
        "app_id": GITHUB_ACTIONS_APP_ID,
        "app_slug": "github-actions",
        "check_suite_id": suite,
        "head_sha": head_sha,
    }


def gate_build(
    run_id: int,
    path: str,
    event: str,
    branch: str,
    head_sha: str,
    suite: int,
    *,
    status: str = "completed",
    conclusion: str | None = "success",
) -> dict[str, object]:
    return {
        "id": run_id,
        "workflow_id": WORKFLOW_IDS[path],
        "path": path,
        "event": event,
        "head_branch": branch,
        "head_sha": head_sha,
        "status": status,
        "conclusion": conclusion,
        "check_suite_id": suite,
    }


def inherited_landing(
    *,
    main_ci: tuple[str, str | None] = ("in_progress", None),
) -> Callable[[list[dict[str, object]], list[dict[str, object]], dict[str, object]], None]:
    """No queue build; main's own push still grading; the export head complete."""

    def mutate(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        queue_suites = {101, 102, 103, 107, 108}
        check_runs[:] = [run for run in check_runs if run["check_suite_id"] not in queue_suites]
        workflow_runs[:] = [run for run in workflow_runs if run.get("check_suite_id") not in queue_suites]
        status, conclusion = main_ci
        workflow_runs.extend(
            [
                gate_build(3_010, CI_PATH, "push", "main", RELEASE_GATE_FIXTURE_SHA, 301,
                           status=status, conclusion=conclusion),
                gate_build(3_020, SAST_PATH, "push", "main", RELEASE_GATE_FIXTURE_SHA, 302),
                gate_build(3_030, SECRET_SCAN_PATH, "push", "main", RELEASE_GATE_FIXTURE_SHA, 303),
                gate_build(3_040, ACCEPTANCE_PATH, "push", "main", RELEASE_GATE_FIXTURE_SHA, 304,
                           status=status, conclusion=conclusion),
                gate_build(5_010, CI_PATH, "pull_request", EXPORT_HEAD_REF, EXPORT_HEAD_SHA, 501),
                gate_build(5_020, SAST_PATH, "pull_request", EXPORT_HEAD_REF, EXPORT_HEAD_SHA, 502),
                gate_build(5_030, SECRET_SCAN_PATH, "push", EXPORT_HEAD_REF, EXPORT_HEAD_SHA, 503),
                gate_build(5_040, SECRET_SCAN_PATH, "pull_request", EXPORT_HEAD_REF, EXPORT_HEAD_SHA, 504,
                           conclusion="skipped"),
                gate_build(5_050, ".github/workflows/acceptance.yml", "pull_request", EXPORT_HEAD_REF,
                           EXPORT_HEAD_SHA, 505),
                gate_build(5_060, ".github/workflows/pr-text-hygiene.yml", "pull_request", EXPORT_HEAD_REF,
                           EXPORT_HEAD_SHA, 506),
            ]
        )
        next_id = 60_000
        for name in (*REQUIRED_RELEASE_CHECKS, ACCEPTANCE_CONTEXT):
            if name == ACCEPTANCE_CONTEXT:
                path = ACCEPTANCE_PATH
            else:
                _, path, _ = REQUIRED_RELEASE_CHECK_PROVENANCE[name]
            next_id += 2
            check_runs.append(gate_check(name, HEAD_SUITES[path], EXPORT_HEAD_SHA, next_id))
            main_status = status if path in (CI_PATH, ACCEPTANCE_PATH) else "completed"
            main_conclusion = None
            if main_status == "completed":
                main_conclusion = "skipped" if name == "DCO Sign-off" else "success"
            check_runs.append(
                gate_check(name, MAIN_SUITES[path], RELEASE_GATE_FIXTURE_SHA, next_id + 1,
                           status=main_status, conclusion=main_conclusion)
            )
        check_runs.append(
            gate_check("gitleaks (full history)", 504, EXPORT_HEAD_SHA, 61_000, conclusion="skipped")
        )
        check_runs.append(gate_check("PR text hygiene", 506, EXPORT_HEAD_SHA, 61_001))

    return mutate


def then(
    first: Callable[[list[dict[str, object]], list[dict[str, object]], dict[str, object]], None],
    second: Callable[[list[dict[str, object]], list[dict[str, object]]], None],
) -> Callable[[list[dict[str, object]], list[dict[str, object]], dict[str, object]], None]:
    def mutate(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        current_run: dict[str, object],
    ) -> None:
        first(check_runs, workflow_runs, current_run)
        second(check_runs, workflow_runs)

    return mutate


def one_check(check_runs: list[dict[str, object]], name: str, head_sha: str) -> dict[str, object]:
    matches = [
        run
        for run in check_runs
        if run["name"] == name and run["head_sha"] == head_sha and run["conclusion"] != "skipped"
    ]
    if len(matches) != 1:
        raise AssertionError(f"inheritance fixture carries {len(matches)} {name} at {head_sha}")
    return matches[0]


def check_inherited_export_tier(release_gate: str) -> None:
    assert_release_gate_admits(
        release_gate,
        "a landing whose export head carries every required context while main's CI runs",
        (
            "admitting on evidence inherited from export pull request #1848",
            f"so every required context was judged on head {EXPORT_HEAD_SHA} ({EXPORT_HEAD_REF})",
            f"on the export_pr evidence for {RELEASE_GATE_FIXTURE_SHA}",
            f"(inherited from export pull request #1848 head {EXPORT_HEAD_SHA})",
            "not waiting for the other admitted build of a required context: "
            "Falsify guards (status=in_progress",
        ),
        inherited_landing(),
        inherited=INHERITED_RECORD,
    )

    # The control: the same landing with nothing inherited waits for main.
    waiting = execute_release_check_gate(
        release_gate, {}, mutate_fixture=inherited_landing()
    )
    if waiting.returncode != 2 or "required check not completed: Falsify guards" not in (
        waiting.stdout + waiting.stderr
    ):
        raise AssertionError(
            "with nothing inherited, a landing whose CI is still running must be "
            f"held for retry: rc={waiting.returncode} {waiting.stdout}{waiting.stderr}"
        )

    assert_release_gate_admits(
        release_gate,
        "main's own CI concluded, so it decides again",
        ("admitting on landing-push evidence: no merge-group build exists at",),
        inherited_landing(main_ci=("completed", "success")),
        inherited=INHERITED_RECORD,
        absent=("inherited from export pull request",),
    )

    assert_release_gate_admits(
        release_gate,
        "a queue build outranks an inherited head",
        (f"on the merge_group evidence for {RELEASE_GATE_FIXTURE_SHA}",),
        lambda _check_runs, _workflow_runs, _current_run: None,
        inherited=INHERITED_RECORD,
        absent=("inherited from export pull request",),
    )

    def head_falsify_guards_red(check_runs, _workflow_runs):
        one_check(check_runs, "Falsify guards", EXPORT_HEAD_SHA)["conclusion"] = "failure"

    assert_release_gate_fixture_rejected(
        release_gate,
        "the inherited head failed a required context",
        "required check not green: Falsify guards (conclusion=failure)",
        then(inherited_landing(), head_falsify_guards_red),
        inherited=INHERITED_RECORD,
    )

    def main_cargo_deny_red(check_runs, _workflow_runs):
        one_check(check_runs, "cargo-deny", RELEASE_GATE_FIXTURE_SHA)["conclusion"] = "failure"

    assert_release_gate_fixture_rejected(
        release_gate,
        "main's own push failed a context the inherited head passed",
        "corroborating required check not green: cargo-deny (conclusion=failure",
        then(inherited_landing(), main_cargo_deny_red),
        inherited=INHERITED_RECORD,
    )

    def head_duplicate(check_runs, _workflow_runs):
        twin = dict(one_check(check_runs, "cargo-deny", EXPORT_HEAD_SHA))
        twin["id"] = 62_000
        check_runs.append(twin)

    assert_release_gate_fixture_rejected(
        release_gate,
        "the inherited head carries a required context twice",
        "ambiguous required check: cargo-deny (2 check-runs under one provenance)",
        then(inherited_landing(), head_duplicate),
        inherited=INHERITED_RECORD,
    )

    def head_wrong_app(check_runs, _workflow_runs):
        one_check(check_runs, "DCO Sign-off", EXPORT_HEAD_SHA)["app_slug"] = "unreviewed-checks-app"

    assert_release_gate_fixture_rejected(
        release_gate,
        "another App claims a required name on the inherited head",
        "required check name claimed by another app: DCO Sign-off",
        then(inherited_landing(), head_wrong_app),
        inherited=INHERITED_RECORD,
    )

    def skipped_writer_from_another_workflow(check_runs, workflow_runs):
        workflow_runs.append(
            {
                "id": 5_070,
                "workflow_id": 999,
                "path": ".github/workflows/required-context-spoof.yaml",
                "event": "pull_request",
                "head_branch": EXPORT_HEAD_REF,
                "head_sha": EXPORT_HEAD_SHA,
                "status": "completed",
                "conclusion": "skipped",
                "check_suite_id": 507,
            }
        )
        check_runs.append(gate_check("Falsify guards", 507, EXPORT_HEAD_SHA, 62_001, conclusion="skipped"))

    assert_release_gate_fixture_rejected(
        release_gate,
        "a skipped run of another workflow claims a required name on the inherited head",
        "required check workflow provenance mismatch: Falsify guards",
        then(inherited_landing(), skipped_writer_from_another_workflow),
        inherited=INHERITED_RECORD,
    )

    def gitleaks_under_the_wrong_event(_check_runs, workflow_runs):
        for run in workflow_runs:
            if run["check_suite_id"] == 503:
                run["event"] = "pull_request"

    assert_release_gate_fixture_rejected(
        release_gate,
        "secret-scan.yml's head build is read under an event it does not publish there",
        "required check workflow provenance mismatch: gitleaks (full history)",
        then(inherited_landing(), gitleaks_under_the_wrong_event),
        inherited=INHERITED_RECORD,
    )

    for label, change in (
        ("an inheritance record for a branch that is not an export", {"head_ref": "chore/lane-x"}),
        ("an inheritance record about another commit", {"sha": "6" * 40}),
        ("an inheritance record with no exact head", {"head_sha": "HEAD"}),
    ):
        assert_release_gate_fixture_rejected(
            release_gate,
            label,
            "the inherited export evidence does not describe",
            inherited_landing(),
            inherited={**INHERITED_RECORD, **change},
        )


def check_timed_out_retry(release_gate: str) -> None:
    """The one admitted retry may follow a job that hit its own timeout.

    GitHub concludes such a job `cancelled`. The annotation texts below are the
    ones it wrote on real RC Build jobs: the timeout on 08d48c27's macOS arm64
    leg, a superseding dispatch, and a manual cancel.
    """

    def retry_after(message: str | list[str] | None, completed_at: str) -> tuple[
        Callable[[list[dict[str, object]], list[dict[str, object]], dict[str, object]], None],
        list[dict[str, object]],
    ]:
        def mutate(check_runs, _workflow_runs, _current_run):
            original = next(run for run in check_runs if run["name"] == "cargo-deny")
            attempt = dict(original)
            original.update(
                {
                    "id": 20_100,
                    "conclusion": "cancelled",
                    "started_at": "2026-09-28T08:50:00Z",
                    "completed_at": completed_at,
                }
            )
            attempt.update(
                {
                    "id": 20_101,
                    "conclusion": "success",
                    "started_at": "2026-09-28T10:25:00Z",
                    "completed_at": "2026-09-28T10:45:00Z",
                }
            )
            check_runs.append(attempt)

        messages = [] if message is None else [message] if isinstance(message, str) else message
        annotations = [
            {"annotation_level": "failure", "message": text}
            for text in [*messages, "The operation was canceled."]
        ] if messages else []
        return mutate, [{"id": 20_100, "annotations": annotations}]

    mutate, annotations = retry_after(real_cancel_message("timed_out"), "2026-09-28T10:20:20Z")
    assert_release_gate_admits(
        release_gate,
        "a required job that hit its own timeout, retried once to success",
        ("required check passed on its one admitted retry: cargo-deny",),
        mutate,
        annotations=annotations,
    )
    for label, message, completed_at in (
        ("a superseding cancel", real_cancel_message("concurrency_cancelled"), "2026-09-28T10:20:20Z"),
        ("a manual cancel", real_cancel_message("manually_cancelled"), "2026-09-28T10:20:20Z"),
        ("a timeout text on a job that stopped early", real_cancel_message("timed_out"), "2026-09-28T09:10:00Z"),
        (
            "a timeout text beside a manual cancel",
            [real_cancel_message("manually_cancelled"), real_cancel_message("timed_out")],
            "2026-09-28T10:20:20Z",
        ),
        ("a cancel with no annotation at all", None, "2026-09-28T10:20:20Z"),
    ):
        mutate, annotations = retry_after(message, completed_at)
        assert_release_gate_fixture_rejected(
            release_gate,
            f"{label}, retried to success, is admitted as a timeout",
            "ambiguous required check: cargo-deny (2 check-runs under one provenance)",
            mutate,
            annotations=annotations,
        )


def assert_inheritance_is_resolved_by_trusted_policy(release_tag: str) -> None:
    """The mint binds a commit to a pull request with protected main's selector.

    The binding decides which build's evidence releases a commit, so it runs
    from the copy of scripts/select-release-candidate.py on freshly fetched
    main, in isolated mode, never from the checkout under judgment; it runs
    before the gate it feeds; and a read that fails falls back to grading
    main's own push runs rather than failing the mint.
    """

    workflow = ".github/workflows/release-tag.yml"
    step = workflow_active_text(workflow_step_source(workflow, release_tag, INHERIT_STEP_ANCHOR))
    for needle, why in (
        ("id: inherit", "carry the id the gate reads its outputs by"),
        ('git show "refs/remotes/origin/main:scripts/select-release-candidate.py"',
         "copy the selector from protected main"),
        ('python3 -I "$policy" inheritance', "run that copy in isolated mode"),
        ('--sha "$SHA"', "ask about the commit being minted"),
        ('fallback "the inheritance read failed"', "fall back when the read fails"),
    ):
        if needle not in step:
            raise AssertionError(f"the inheritance step must {why}: {needle}")
    if "scripts/select-release-candidate.py inheritance" in step:
        raise AssertionError(
            "the inheritance step must run protected main's selector, never the "
            "checkout under judgment's"
        )
    for line in step.splitlines():
        if line.startswith("fallback()"):
            break
    else:
        raise AssertionError("the inheritance step must define its fallback")
    if release_tag.index(INHERIT_STEP_ANCHOR) > release_tag.index(GATE_STEP_ANCHOR):
        raise AssertionError("the inheritance step must resolve before the gate that reads it")
    gate = workflow_step_source(workflow, release_tag, GATE_STEP_ANCHOR)
    for needle in (
        "INHERITED_EVIDENCE: ${{ steps.inherit.outputs.record }}",
        "EXPORT_HEAD_SHA: ${{ steps.inherit.outputs.inherited == 'true' && "
        "steps.inherit.outputs.head_sha || '' }}",
        'gh api "repos/$REPO/commits/$EXPORT_HEAD_SHA/check-runs?per_page=100&filter=all"',
        'gh api "repos/$REPO/check-runs/$check_run_id/annotations?per_page=100"',
    ):
        require(gate, needle, "release gate inheritance wiring")


def check_inheritance_step(release_tag: str) -> None:
    assert_inheritance_is_resolved_by_trusted_policy(release_tag)
    step = workflow_step_source(".github/workflows/release-tag.yml", release_tag, INHERIT_STEP_ANCHOR)
    expect_assertion(
        "the mint runs the selector from the checkout under judgment",
        "run that copy in isolated mode",
        lambda: assert_inheritance_is_resolved_by_trusted_policy(
            release_tag.replace(
                'python3 -I "$policy" inheritance',
                "python3 -I scripts/select-release-candidate.py inheritance",
                1,
            )
        ),
    )
    expect_assertion(
        "a failed inheritance read reddens the mint",
        "fall back when the read fails",
        lambda: assert_inheritance_is_resolved_by_trusted_policy(
            release_tag.replace('fallback "the inheritance read failed"', "exit 1", 1)
        ),
    )
    moved = release_tag.replace(step, "", 1)
    after_gate = "      - name: Verify tag does not already exist"
    expect_assertion(
        "the inheritance resolves after the gate that reads it",
        "must resolve before the gate",
        lambda: assert_inheritance_is_resolved_by_trusted_policy(
            moved.replace(after_gate, step + after_gate, 1)
        ),
    )
    expect_assertion(
        "the gate stops fetching the inherited head",
        "is missing required policy",
        lambda: assert_inheritance_is_resolved_by_trusted_policy(
            release_tag.replace(
                'gh api "repos/$REPO/commits/$EXPORT_HEAD_SHA/check-runs?per_page=100&filter=all"',
                'gh api "repos/$REPO/commits/$SHA/check-runs?per_page=100&filter=all"',
                1,
            )
        ),
    )



# ── the merge-queue landing: mint on the push, refuse a retired version ────


ADMISSION_START = '          version_policy="$RUNNER_TEMP/select-release-candidate.py"\n'
ADMISSION_END = '          if [ "$automatic" = true ]; then\n            tag="$resolved_tag"\n'


def version_admission_source(release_tag: str) -> str:
    """The resolve step's retired-version admission, exactly as the mint runs it."""

    resolve = workflow_step_source(
        ".github/workflows/release-tag.yml",
        release_tag,
        "      - name: Resolve exact coherent release commit",
    )
    if ADMISSION_START not in resolve or ADMISSION_END not in resolve:
        raise AssertionError("the mint must admit the version before it records the tag it mints")
    start = resolve.index(ADMISSION_START)
    if start > resolve.index(ADMISSION_END) or start < resolve.index('"$node_bin" "$policy"'):
        raise AssertionError(
            "the mint must admit the version after the release intent resolves it and "
            "before it records the tag it mints"
        )
    return textwrap.dedent(resolve[start : resolve.index(ADMISSION_END)])


def execute_version_admission(source: str, tag: str, automatic: bool) -> tuple[int, str, str]:
    """Run the admission against protected main's copy of the real selector."""

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        main = root / "main"
        (main / "scripts").mkdir(parents=True)
        (main / "scripts" / "select-release-candidate.py").write_bytes(
            (ROOT / "scripts" / "select-release-candidate.py").read_bytes()
        )
        identity = ["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"]
        subprocess.run(["git", "init", "--quiet", "-b", "main", str(main)], check=True)
        subprocess.run(["git", "-C", str(main), "add", "scripts"], check=True)
        subprocess.run(["git", "-C", str(main), *identity, "commit", "--quiet", "-s", "-m", "policy"], check=True)
        authority = subprocess.run(
            ["git", "-C", str(main), "rev-parse", "HEAD"], check=True, capture_output=True, text=True
        ).stdout.strip()
        output = root / "output"
        output.write_text("", encoding="utf-8")
        script = (
            "set -euo pipefail\n"
            f"authority_main={authority}\n"
            f"resolved_tag={tag}\n"
            f"automatic={'true' if automatic else 'false'}\n"
            + source
            + "echo ADMITTED\n"
        )
        environment = {**os.environ, "RUNNER_TEMP": str(root), "GITHUB_OUTPUT": str(output)}
        result = subprocess.run(
            ["bash", "-c", script], cwd=main, env=environment, capture_output=True, text=True, timeout=30
        )
        return result.returncode, result.stdout + result.stderr, output.read_text(encoding="utf-8")


def assert_retired_versions_are_never_minted(release_tag: str) -> None:
    source = version_admission_source(release_tag)
    for line in (
        'git show "${authority_main}:scripts/select-release-candidate.py" > "$version_policy"',
        'python3 -I "$version_policy" admit-version --version "$resolved_tag")"',
    ):
        if line not in source:
            raise AssertionError(f"the mint must ask protected main's selector: {line}")
    code, log, output = execute_version_admission(source, "v0.8.2", automatic=True)
    if code != 0 or "needed=false" not in output or "ADMITTED" in log:
        raise AssertionError(f"an automatic mint of retired v0.8.2 was not a no-op: rc={code} {log} {output}")
    code, log, _ = execute_version_admission(source, "v0.8.3", automatic=False)
    if code == 0 or "may not be released" not in log:
        raise AssertionError(f"break glass minted retired v0.8.3: rc={code} {log}")
    code, log, output = execute_version_admission(source, "v0.8.4", automatic=True)
    if code != 0 or "ADMITTED" not in log or output:
        raise AssertionError(f"the mint refused releasable v0.8.4: rc={code} {log} {output}")


def assert_the_mint_runs_on_the_landing_push(release_tag: str) -> None:
    header = release_tag.split("\njobs:", 1)[0]
    trigger = re.search(r"(?m)^  push:\n((?:    .*\n)*)", header)
    if trigger is None or trigger.group(1) != "    branches: [main]\n":
        raise AssertionError("release-tag.yml must mint on a push to main, and on main alone")
    guard = workflow_step_source(
        ".github/workflows/release-tag.yml", release_tag, "      - name: Guard trigger authority"
    )
    if 'if [ "$EVENT_NAME" = push ] && [ "$REF" != refs/heads/main ]; then' not in guard:
        raise AssertionError("the mint's trigger guard must refuse a push to anything but main")
    require(release_tag, "            push|workflow_run|schedule)\n", "the mint resolves a push like its other automatic arms")
    require(
        release_check_gate_source(release_tag),
        'release_tag_events = {"push", "schedule", "workflow_run", "repository_dispatch"}',
        "the mint's own run may be a push run",
    )


def check_queue_landing_mint(release_tag: str) -> None:
    assert_the_mint_runs_on_the_landing_push(release_tag)
    for label, before, after, error in (
        ("the mint stops waking on the landing push", "  push:\n    branches: [main]\n  schedule:",
         "  schedule:", "must mint on a push to main"),
        ("the mint's push trigger widens to every branch", "  push:\n    branches: [main]\n  schedule:",
         '  push:\n    branches: ["**"]\n  schedule:', "must mint on a push to main"),
        ("the guard lets a push to another branch through",
         'if [ "$EVENT_NAME" = push ] && [ "$REF" != refs/heads/main ]; then', "if false; then",
         "must refuse a push to anything but main"),
    ):
        expect_assertion(
            label,
            error,
            lambda before=before, after=after: assert_the_mint_runs_on_the_landing_push(
                release_tag.replace(before, after, 1)
            ),
        )
    assert_retired_versions_are_never_minted(release_tag)
    expect_assertion(
        "the mint stops asking whether the version is retired",
        "must admit the version before it records the tag",
        lambda: assert_retired_versions_are_never_minted(
            release_tag.replace(ADMISSION_START, "", 1)
        ),
    )
    expect_assertion(
        "the mint asks the checkout under judgment instead of protected main",
        "must ask protected main's selector",
        lambda: assert_retired_versions_are_never_minted(
            release_tag.replace(
                'git show "${authority_main}:scripts/select-release-candidate.py" > "$version_policy"',
                'cp scripts/select-release-candidate.py "$version_policy"',
                1,
            )
        ),
    )


def check_acceptance_veto(release_gate: str) -> None:
    """Acceptance is a release veto in every tier, read by the mint itself."""

    assert_release_gate_fixture_rejected(
        release_gate,
        "the queue build's Acceptance failed",
        "required check not green: Product Acceptance (conclusion=failure)",
        lambda check_runs, _workflow_runs, _current_run: next(
            run for run in check_runs if run["name"] == ACCEPTANCE_CONTEXT
        ).update({"conclusion": "failure"}),
    )
    absent = execute_release_check_gate(
        release_gate,
        {},
        mutate_fixture=lambda check_runs, _workflow_runs, _current_run: check_runs.__setitem__(
            slice(None), [run for run in check_runs if run["name"] != ACCEPTANCE_CONTEXT]
        ),
    )
    if absent.returncode != 2 or "missing required check: Product Acceptance" not in absent.stdout + absent.stderr:
        raise AssertionError(
            f"a landing with no Acceptance must be held: rc={absent.returncode} {absent.stdout}{absent.stderr}"
        )

    def head_acceptance_skipped_kept(check_runs, workflow_runs):
        for run in check_runs:
            if run["name"] == ACCEPTANCE_CONTEXT and run["head_sha"] == EXPORT_HEAD_SHA:
                run["conclusion"] = "skipped"

    assert_release_gate_fixture_rejected(
        release_gate,
        "an export head whose Acceptance job was skipped by its if:",
        "required check not green: Product Acceptance (conclusion=skipped)",
        then(inherited_landing(), head_acceptance_skipped_kept),
        inherited=INHERITED_RECORD,
    )

    def started_by_the_landing_push(_check_runs, workflow_runs, current_run):
        current_run["event"] = "push"
        for run in workflow_runs:
            if run.get("id") == current_run["id"]:
                run["event"] = "push"

    assert_release_gate_admits(
        release_gate,
        "a mint started by the push that lands the queue's commit",
        (f"on the merge_group evidence for {RELEASE_GATE_FIXTURE_SHA}",),
        started_by_the_landing_push,
    )


# ── the RC Build locator, asked before any tag is written ─────────────────

LOCATOR_STEP_ANCHOR = "      - name: Locate the RC Build the release will ship"
FAKE_LOCATOR = """import { appendFileSync, writeFileSync } from 'node:fs';
const args = process.argv.slice(2);
writeFileSync(process.env.FAKE_MARK, args.join(' '));
if (process.env.FAKE_POISON) appendFileSync(process.env.GITHUB_ENV, 'PATH=/elsewhere\\n');
appendFileSync(args[args.indexOf('--summary') + 1], '### RC Build located\\n');
process.exit(Number(process.env.FAKE_LOCATE || 0));
"""


def locator_step_source(release_tag: str) -> str:
    step = workflow_step_source(".github/workflows/release-tag.yml", release_tag, LOCATOR_STEP_ANCHOR)
    marker = "        run: |\n"
    if marker not in step:
        raise AssertionError("the locator step no longer runs a script")
    return textwrap.dedent(step[step.index(marker) + len(marker):])


def execute_locator_step(
    source: str, *, locate: int = 0, poison: bool = False, locates: bool = True, detached: bool = False
) -> tuple[int, str, bool]:
    """Run the mint's locator step against a commit carrying a fake locator."""

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        repo = root / "repo"
        (repo / "scripts").mkdir(parents=True)
        (repo / ".github" / "workflows").mkdir(parents=True)
        (repo / "scripts" / "locate-rc-build.mjs").write_text(FAKE_LOCATOR, encoding="utf-8")
        (repo / ".github" / "workflows" / "release.yml").write_text(
            "run: node scripts/locate-rc-build.mjs select\n" if locates else "run: cargo build\n",
            encoding="utf-8",
        )
        identity = ["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"]
        subprocess.run(["git", "init", "--quiet", "-b", "main", str(repo)], check=True)
        subprocess.run(["git", "-C", str(repo), "add", "."], check=True)
        subprocess.run(["git", "-C", str(repo), *identity, "commit", "--quiet", "-s", "-m", "candidate"], check=True)
        sha = subprocess.run(
            ["git", "-C", str(repo), "rev-parse", "HEAD"], check=True, capture_output=True, text=True
        ).stdout.strip()
        # The gate step writes its listings into the workspace before this runs.
        (repo / "check_runs.ndjson").write_text("{}\n", encoding="utf-8")
        mark = root / "located"
        summary = root / "summary"
        environment = {
            **os.environ,
            "SHA": "0" * 40 if detached else sha,
            "TAG": "v0.8.4",
            "REPO": "kinlab-ai/kin",
            "RUNNER_TEMP": str(root),
            "GITHUB_STEP_SUMMARY": str(summary),
            "FAKE_MARK": str(mark),
            "FAKE_LOCATE": str(locate),
        }
        if poison:
            environment["FAKE_POISON"] = "1"
        result = subprocess.run(
            ["bash", "-c", source], cwd=repo, env=environment, capture_output=True, text=True, timeout=30
        )
        asked = mark.read_text() if mark.exists() else ""
        if asked and asked != f"select --repo kinlab-ai/kin --sha {sha} --version 0.8.4 --summary {root}/rc-locator-runner-state/summary.md":
            raise AssertionError(f"the mint asked the locator something else: {asked}")
        return result.returncode, result.stdout + result.stderr, bool(asked)


def assert_no_tag_is_written_that_release_yml_would_refuse(release_tag: str) -> None:
    order = [
        release_tag.index("      - name: Require proof-loop artifacts for the release candidate"),
        release_tag.index(LOCATOR_STEP_ANCHOR) if LOCATOR_STEP_ANCHOR in release_tag else -1,
        release_tag.index("      - name: Mint kin-release-bot installation token"),
        release_tag.index("      - name: Create release tag ref"),
    ]
    if order[1] < 0 or order != sorted(order):
        raise AssertionError(
            "the mint must ask the RC Build locator after the proof gate and before it mints a token or writes a tag"
        )
    source = locator_step_source(release_tag)
    code, log, asked = execute_locator_step(source)
    if code != 0 or not asked:
        raise AssertionError(f"a candidate release.yml can ship from was refused: rc={code} {log}")
    code, log, asked = execute_locator_step(source, locate=1)
    if code == 0 or "release.yml would refuse v0.8.4" not in log:
        raise AssertionError(f"the mint went on to tag a candidate the locator refuses: rc={code} {log}")
    code, log, _ = execute_locator_step(source, poison=True)
    if code == 0 or "attempted to change runner state" not in log:
        raise AssertionError(f"the locator reached the runner state of the privileged steps: rc={code} {log}")
    code, log, asked = execute_locator_step(source, locates=False)
    if code != 0 or asked or "locates no RC Build bytes" not in log:
        raise AssertionError(f"a release.yml that builds its own bytes must need no locator: rc={code} {log}")
    code, log, asked = execute_locator_step(source, detached=True)
    if code == 0 or asked:
        raise AssertionError(f"the locator was asked about a checkout other than the commit being tagged: {log}")


def check_rc_build_locator_precondition(release_tag: str) -> None:
    assert_no_tag_is_written_that_release_yml_would_refuse(release_tag)
    step = workflow_step_source(".github/workflows/release-tag.yml", release_tag, LOCATOR_STEP_ANCHOR)
    write = workflow_step_source(".github/workflows/release-tag.yml", release_tag, "      - name: Create release tag ref")
    expect_assertion(
        "the locator is asked only after the tag is written",
        "before it mints a token or writes a tag",
        lambda: assert_no_tag_is_written_that_release_yml_would_refuse(
            release_tag.replace(step, "", 1).replace(write, write + "\n" + step, 1)
        ),
    )

    def mutate(before: str, after: str) -> str:
        if release_tag.count(before) != 1:
            raise AssertionError(f"locator falsification needle is not unique: {before!r}")
        return release_tag.replace(before, after, 1)

    expect_assertion(
        "a locator refusal no longer stops the mint",
        "went on to tag a candidate the locator refuses",
        lambda: assert_no_tag_is_written_that_release_yml_would_refuse(
            mutate('          if [ "$located" -ne 0 ]; then\n', '          if false; then\n')
        ),
    )
    expect_assertion(
        "the locator runs with the real runner state files",
        "reached the runner state",
        lambda: assert_no_tag_is_written_that_release_yml_would_refuse(
            mutate(
                'GITHUB_ENV="$inert/env" GITHUB_PATH="$inert/path" GITHUB_STATE="$inert/state" GITHUB_OUTPUT="$inert/output" \\\n',
                "",
            )
        ),
    )


def check_release_tag_authority(release_tag: str) -> None:
    for policy in (
        'workflows: ["CI"]',
        "types: [completed]",
        'cron: "11,26,41,56 * * * *"',
        "actions: read",
        "attestations: read",
        "contents: read",
        "checks: read",
        "github.event.workflow_run.event == 'push'",
        "github.event.workflow_run.head_branch == 'main'",
        "repository_dispatch:",
        # Two typed dispatches, pinned as one literal so adding a third is a
        # reviewed edit here rather than a quiet trigger widening. release_tag
        # is break glass and names an exact commit; release_tag_evaluate runs
        # the scheduled algorithm on demand and names nothing. Both face the
        # same actor allowlist and main-only ref below.
        "types: [release_tag, release_tag_evaluate]",
        "github.event.action",
        "github.event.repository.default_branch",
        "github.event.client_payload.tag",
        "github.event.client_payload.sha",
        "EVENT_SHA: ${{ github.sha }}",
        'EVENT_NAME" != repository_dispatch',
        # The dispatch action allowlist. It was a single `!=` test while there
        # was one type; with two it is a case whose only admitting arm is this
        # literal, so an unlisted action still falls to the refusing arm.
        "release_tag|release_tag_evaluate) ;;",
        "unsupported repository dispatch action",
        # Evaluate runs the automatic selection, so it must never accept a
        # caller-named commit. Ignoring one would tag a different commit than
        # the caller believed they had chosen.
        'EVENT_ACTION" = release_tag_evaluate',
        "release_tag_evaluate runs the automatic selection and takes no tag or sha",
        'DEFAULT_BRANCH" != main',
        'REF" != "refs/heads/$DEFAULT_BRANCH"',
        "environment: release-tag",
        'git show "${authority_main}:scripts/release-intent.mjs" > "$policy"',
        '"$node_bin" "$policy"',
        "git merge-base --is-ancestor",
        "break-glass release sha",
        "actions/workflows/release.yml/runs?per_page=100",
        '.status == "requested"',
        ".status == \"queued\"",
        ".conclusion == \"success\"",
        "release-provenance.json.sha256",
        "release-promotion.json.sha256",
        "gh attestation verify release-provenance.json",
        "gh attestation verify release-promotion.json",
        "release_workflow_run_id",
        "runInvocationURI",
        "runDetails.metadata.invocationId",
        "ltrimstr($run_uri)",
        "verifiedTimestamps",
        "registry.npmjs.org/@kinlab%2F",
        "ghcr.io/v2/kinlab-ai/kin/manifests",
        "oci://ghcr.io/kinlab-ai/kin@${ghcr_latest}",
        "--bundle-from-oci",
        'latest_tag" != v0.3.6',
        "markerless logless fallback is retired",
        "matching_count",
        "highest_tag",
        "refs/remotes/origin/main:scripts/abandoned-release-tags.json",
        "refs/remotes/origin/main:scripts/select-admissible-release-tag.py",
        "git for-each-ref",
        TAG_LISTING_FORMAT,
        'python3 "$selector"',
        'highest_tag="$(cat "$admissible")"',
        "REQUIRED_CHECKS:",
        'GITHUB_ACTIONS_APP_ID: "15368"',
        "GITHUB_ACTIONS_APP_SLUG: github-actions",
        "app_id: (.app.id // 0)",
        'actual_app = (run.get("app_id"), run.get("app_slug"))',
        "if actual_app != expected_app:",
        "check-runs?per_page=100&filter=all",
        "head_branch: .head_branch",
        'release_branch = "main"',
        "expected_provenance = {",
        "tier = evidence_tier(workflow)",
        ") != expected_producer:",
        "if tier == authority_tier:",
        "ambiguous required check",
        # The rekey onto the merge-group-proven sha. Each of these is one half
        # of a proof that cannot be dropped on its own: the queue anchor names
        # the ref, the parent read from checked-out history is what the ref's
        # embedded base has to equal, and the tier decision is what keeps a
        # duplicated context from reading as ambiguous authority.
        'RELEASE_PARENT_SHA="$(git rev-parse "${SHA}^")"',
        "queue_anchor_workflow = (245803170",
        # The third tier: an export head bound by the step above, the
        # authority only while main's own CI push build has no verdict.
        'else "export_pr" if export_head is not None',
        "if inherited is not None and queue_ref is None and not main_ci_verdict:",
        "pull-requests: read",
        "ambiguous merge-group evidence",
        "merge-group evidence is not on a merge-queue ref",
        "merge-group evidence sha mismatch",
        "merge-group evidence proved a different tree",
        "corroborating required check not green",
        "RULESET_REQUIRED_CHECKS:",
        "ruleset-required context has no admitted build at",
        "mint requires a context the reviewed main ruleset does not",
        'allowed_conclusions = (\n                  {"success", "skipped"}',
        "release_tag_suite_ids",
        "did not settle within 30 minutes",
        "actions/create-github-app-token@bcd2ba49218906704ab6c1aa796996da409d3eb1",
        "repositories: kin",
        "break-glass release sha $SHA is no longer current origin/main HEAD",
        'ref="refs/tags/$TAG"',
    ):
        require(release_tag, policy, "automatic App-mediated release tag admission")
    for forbidden in (
        "workflow_dispatch:",
        "${{ inputs.",
        "contents: write",
        "packages: write",
        "id-token: write",
        "KIN_CI_BOT_TOKEN",
    ):
        if forbidden in release_tag:
            raise AssertionError(
                f"release-tag workflow contains forbidden standing authority: {forbidden}"
            )

    resolve_start = release_tag.index(
        "      - name: Resolve exact coherent release commit\n"
    )
    resolve_end = release_tag.index(
        "      - name: Re-verify checked-out SHA is on reviewed origin/main history\n"
    )
    resolve_step = release_tag[resolve_start:resolve_end]
    authority_main = resolve_step.index(
        'authority_main="$(git rev-parse refs/remotes/origin/main)"'
    )
    automatic_authority = resolve_step.index(
        'git merge-base --is-ancestor "$sha" "$authority_main"'
    )
    break_glass_authority = resolve_step.index(
        'elif [ "$sha" != "$authority_main" ]; then'
    )
    trusted_node = resolve_step.index('node_bin="$(command -v node)"')
    trusted_policy = resolve_step.index(
        'git show "${authority_main}:scripts/release-intent.mjs" > "$policy"'
    )
    target_checkout = resolve_step.index('git checkout --detach "$sha"')
    intent_execution = resolve_step.index('"$node_bin" "$policy"')
    runner_state_isolation = tuple(
        resolve_step.index(marker)
        for marker in (
            'GITHUB_ENV="$intent_env"',
            'GITHUB_PATH="$intent_path"',
            'GITHUB_STATE="$intent_state"',
        )
    )
    if not (
        authority_main
        < automatic_authority
        < break_glass_authority
        < trusted_node
        < trusted_policy
        < target_checkout
        < min(runner_state_isolation)
        <= max(runner_state_isolation)
        < intent_execution
    ):
        raise AssertionError(
            "release-tag must validate the selected SHA against freshly fetched "
            "main, copy trusted current-main policy, and isolate runner state "
            "before checkout or policy execution"
        )
    if "node scripts/release-intent.mjs" in resolve_step:
        raise AssertionError(
            "release-tag must not execute release policy from the "
            "payload-selected checkout"
        )
    check_inheritance_step(release_tag)
    check_queue_landing_mint(release_tag)
    check_rc_build_locator_precondition(release_tag)



def check_abandoned_tag_admission(
    release_recovery: str, release_tag: str, release_train: str
) -> None:
    assert_abandoned_tag_admission(release_tag, release_train, release_recovery)



def check_release_proof_key_authority(
    proof_gate: str, release: str, release_tag: str, release_train: str
) -> None:
    # Where the release evidence is keyed decides whether main has to hold
    # still. Keyed to a branch head the train rewrites, it did; keyed to a main
    # commit and checked against the tag, it does not. Nothing pinned that
    # before, so the whole arrangement is pinned here together with the refusal
    # it carries.
    assert_release_proof_key_authority(
        release_train, release_tag, release, proof_gate
    )
    expect_assertion(
        "the mint stops comparing the proven sha to the sha it tags",
        "refuse when the sha the gate verified",
        lambda: assert_release_proof_key_authority(
            release_train,
            replace_exactly_once(
                release_tag,
                'if [ "$proven" != "$SHA" ]; then',
                'if [ "$proven" = "never" ]; then',
                "proof gate identity",
            ),
            release,
            proof_gate,
        ),
    )
    expect_assertion(
        "the mint stops stating that the tagged tree is the proven tree",
        "state that the tree it tags is the tree that was proven",
        lambda: assert_release_proof_key_authority(
            release_train,
            replace_exactly_once(
                release_tag,
                'delta="$(git diff --name-only "$proven" "$SHA")"',
                'delta=""',
                "proof gate tree identity",
            ),
            release,
            proof_gate,
        ),
    )
    expect_assertion(
        "the proof gate runs after the tag ref it was supposed to gate",
        "must run before the tag ref is written",
        lambda: assert_release_proof_key_authority(
            release_train,
            swap_release_tag_step_order(release_tag),
            release,
            proof_gate,
        ),
    )
    expect_assertion(
        "the mint takes a truncated evidence listing at face value",
        "refuse a truncated listing",
        lambda: assert_release_proof_key_authority(
            release_train,
            replace_exactly_once(
                release_tag,
                "jq -r '.truncated // false' <<< \"$listing\"",
                "jq -r 'false' <<< \"$listing\"",
                "truncated listing refusal",
            ),
            release,
            proof_gate,
        ),
    )
    expect_assertion(
        "the train keys a release on the version bump branch's head again",
        "must key nothing on the version bump branch's head",
        lambda: assert_release_proof_key_authority(
            replace_exactly_once(
                release_train,
                "      - name: Arm protected auto-merge\n",
                "      - name: Require proof-loop artifacts for the candidate\n"
                "        run: |\n"
                "          git show "
                f'"refs/remotes/origin/main:{PROOF_GATE_POLICY}" > gate\n'
                "\n"
                "      - name: Arm protected auto-merge\n",
                "release train proof key",
            ),
            release_tag,
            release,
            proof_gate,
        ),
    )
    # The same regression, planted where the general comment stripper cannot
    # see it. `refs/tags/*` opens a C-style block comment for `active_lines`,
    # which then swallows the next eleven thousand characters of the reconcile
    # job, and this anchor sits inside that range. Reading the job through that
    # rule reports absence for a step that is right there, so the guard uses a
    # shell-safe reader and this proves it.
    expect_assertion(
        "the train keys a release on the bump branch head inside the "
        "block-comment blind spot",
        "must key nothing on the version bump branch's head",
        lambda: assert_release_proof_key_authority(
            replace_exactly_once(
                release_train,
                '          test -s "$abandoned"\n',
                '          test -s "$abandoned"\n'
                f'          git show "refs/remotes/origin/main:{PROOF_GATE_POLICY}" > gate\n',
                "release train proof key, blind spot",
            ),
            release_tag,
            release,
            proof_gate,
        ),
    )
    expect_assertion(
        "the promote gate stops asking about the tagged commit itself",
        "ask about the tagged commit directly",
        lambda: assert_release_proof_key_authority(
            release_train,
            release_tag,
            replace_exactly_once(
                release,
                "          CANDIDATE_SHA: ${{ github.sha }}\n",
                "",
                "promote gate direct key",
            ),
            proof_gate,
        ),
    )
    expect_assertion(
        "the promote gate stops bridging a tag with no direct record",
        "keep the bridge, which is what makes a tag with no direct record",
        lambda: assert_release_proof_key_authority(
            release_train,
            release_tag,
            replace_exactly_once(
                release,
                "          RESOLVE_FROM_COMMIT: ${{ github.sha }}\n",
                "",
                "promote gate bridge",
            ),
            proof_gate,
        ),
    )
    expect_assertion(
        "the proof gate's bridge stops being bounded to the bump branch",
        "must apply the bump-branch bound when it bridges",
        lambda: assert_release_proof_key_authority(
            release_train,
            release_tag,
            release,
            replace_exactly_once(
                proof_gate,
                "produced.filter((pull) => pull?.head?.ref === BUMP_BRANCH)",
                "produced.filter((pull) => pull !== null)",
                "proof gate bump-branch bound",
            ),
        ),
    )
    expect_assertion(
        "the bump branch is renamed in the train and not in the gate",
        "must be bounded to the same bump branch",
        lambda: assert_release_proof_key_authority(
            replace_exactly_once(
                release_train,
                "          BRANCH: automation/release-next\n",
                "          BRANCH: automation/release-later\n",
                "bump branch rename",
            ),
            release_tag,
            release,
            proof_gate,
        ),
    )
    expect_assertion(
        "the proof gate bridges on a record it could not read",
        "must bridge only when the direct record is ABSENT",
        lambda: assert_release_proof_key_authority(
            release_train,
            release_tag,
            release,
            replace_exactly_once(
                proof_gate,
                "if (!error.evidenceAbsent || !resolveFromCommit) {",
                "if (!resolveFromCommit) {",
                "proof gate absence-only bridge",
            ),
        ),
    )
    expect_assertion(
        "the mint reads the evidence listing without testing whether it read one",
        "test the status of the listing read",
        lambda: assert_release_proof_key_authority(
            release_train,
            replace_exactly_once(
                release_tag,
                'if ! listing="$(gh api',
                'if listing="$(gh api',
                "listing read status",
            ),
            release,
            proof_gate,
        ),
    )
    expect_assertion(
        "the mint throws away why the evidence listing could not be read",
        "keep the error text from the listing read",
        lambda: assert_release_proof_key_authority(
            release_train,
            replace_exactly_once(
                release_tag,
                '"repos/${REPO}/git/trees/release-evidence?recursive=1" \\\n'
                '              2>"$read_error")"; then',
                '"repos/${REPO}/git/trees/release-evidence?recursive=1" \\\n'
                '              2>/dev/null)"; then',
                "listing read diagnostics",
            ),
            release,
            proof_gate,
        ),
    )
    expect_assertion(
        "the train arms auto-merge without un-drafting the bump pull request",
        "un-draft the bump pull request before it arms auto-merge",
        lambda: assert_release_proof_key_authority(
            replace_exactly_once(
                release_train,
                '            gh pr ready "$PR" --repo "$GITHUB_REPOSITORY"\n',
                "",
                "release train un-draft",
            ),
            release_tag,
            release,
            proof_gate,
        ),
    )
    # The same regression as an ordering change rather than a deletion. Both
    # commands survive, so only the ordering check can catch it, and arming
    # first is not a smaller mistake: auto-merge registered against a draft is
    # what leaves the rail reporting itself clear while nothing lands.
    expect_assertion(
        "the train un-drafts the bump pull request after it has already armed",
        "must un-draft the bump pull request BEFORE it",
        lambda: assert_release_proof_key_authority(
            swap_train_undraft_after_arm(release_train),
            release_tag,
            release,
            proof_gate,
        ),
    )
    expect_assertion(
        "the promote gate stops reading back the sha the gate verified",
        "read back the sha the gate verified",
        lambda: assert_release_proof_key_authority(
            release_train,
            release_tag,
            replace_exactly_once(
                release,
                "process.stdout.write(result.sha);",
                'process.stdout.write("verified");',
                "promote gate sha readback",
            ),
            proof_gate,
        ),
    )
    expect_assertion(
        "the promote gate stops comparing the proven sha to the one it promotes",
        "not the sha it promotes",
        lambda: assert_release_proof_key_authority(
            release_train,
            release_tag,
            replace_exactly_once(
                release,
                'if [ "$proven" != "$CANDIDATE_SHA" ]; then',
                'if [ "$proven" = "never" ]; then',
                "promote gate identity",
            ),
            proof_gate,
        ),
    )



def check_release_tag_required_check_gate() -> None:
    release_tag = RELEASE_TAG.read_text(encoding="utf-8")
    release_tag_header = release_tag.split("\njobs:", 1)[0]
    require(
        release_tag_header,
        "permissions:\n  contents: read\n  checks: read\n  actions: read",
        "release-tag read-only check and workflow provenance access",
    )
    for forbidden_permission in (
        "contents: write",
        "checks: write",
        "actions: write",
    ):
        if forbidden_permission in release_tag_header:
            raise AssertionError(
                "release-tag workflow token must remain read-only: "
                f"{forbidden_permission}"
            )
    for policy in (
        "REQUIRED_CHECKS: |",
        "Check & Test (ubuntu-latest)",
        "Check & Test (macos-latest)",
        "DCO Sign-off",
        "cargo-deny",
        "gitleaks (full history)",
        "Windows installer + vector release build",
        "missing required check: {name}",
        "required check not green: {name}",
        "ambiguous required check: {name}",
        "required check workflow provenance mismatch: {name}",
        "actions: read",
        "filter=all",
        'app_slug: (.app.slug // "")',
        "head_branch: .head_branch",
        "check_suite_id: .check_suite.id",
        "workflow_id: .workflow_id",
        "head_sha: .head_sha",
        "workflow_runs.ndjson",
        "current_run.json",
    ):
        require(release_tag, policy, "release tag required-check gate")
    for policy in (
        'skippable_required = {"DCO Sign-off"}',
        'non_failing_optional = {"success", "skipped", "neutral"}',
        "if name in skippable_required",
        'else {"success"}',
        'run["conclusion"] not in allowed_conclusions',
    ):
        require(release_tag, policy, "per-check release conclusion policy")

    release_gate = release_check_gate_source(release_tag)
    positive_fixture = execute_release_check_gate(release_gate, {})
    if positive_fixture.returncode != 0:
        raise AssertionError(
            "current release check provenance fixture was rejected: "
            f"{positive_fixture.stdout}{positive_fixture.stderr}"
        )
    for accepted_conclusion in ("success", "skipped"):
        assert_release_check_accepted(
            release_gate,
            "DCO Sign-off",
            accepted_conclusion,
        )
    for check_name in REQUIRED_RELEASE_CHECKS:
        if check_name == "DCO Sign-off":
            continue
        for rejected_conclusion in ("skipped", "neutral"):
            assert_release_check_rejected(
                release_gate,
                check_name,
                rejected_conclusion,
            )
    assert_release_check_rejected(
        release_gate,
        "DCO Sign-off",
        "neutral",
    )

    def required_check_fixture(
        check_runs: list[dict[str, object]],
        name: str,
    ) -> dict[str, object]:
        matches = [run for run in check_runs if run["name"] == name]
        if len(matches) != 1:
            raise AssertionError(
                f"release gate fixture requires one {name} check, got {len(matches)}"
            )
        return matches[0]

    def workflow_fixture(
        workflow_runs: list[dict[str, object]],
        suite_id: object,
    ) -> dict[str, object]:
        matches = [run for run in workflow_runs if run["check_suite_id"] == suite_id]
        if len(matches) != 1:
            raise AssertionError(
                "release gate fixture requires one workflow for suite "
                f"{suite_id}, got {len(matches)}"
            )
        return matches[0]

    def add_prior_release_tag_refusal(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        workflow_runs.append(
            {
                "id": 8_000,
                "workflow_id": RELEASE_TAG_WORKFLOW_ID,
                "path": ".github/workflows/release-tag.yml",
                "event": "repository_dispatch",
                "head_branch": "main",
                "head_sha": RELEASE_GATE_FIXTURE_SHA,
                "status": "completed",
                "conclusion": "failure",
                "check_suite_id": 105,
            }
        )
        check_runs.append(
            {
                "name": "Mint release tag",
                "status": "completed",
                "conclusion": "failure",
                "id": 8_001,
                "app_id": GITHUB_ACTIONS_APP_ID,
                "app_slug": "github-actions",
                "check_suite_id": 105,
                "head_sha": RELEASE_GATE_FIXTURE_SHA,
            }
        )

    retry_fixture = execute_release_check_gate(
        release_gate,
        {},
        mutate_fixture=add_prior_release_tag_refusal,
    )
    if retry_fixture.returncode != 0:
        raise AssertionError(
            "prior refusal from the exact release-tag workflow blocked a retry: "
            f"{retry_fixture.stdout}{retry_fixture.stderr}"
        )

    def add_external_mint_failure(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        workflow_runs.append(
            {
                "id": 8_100,
                "workflow_id": 999,
                "path": ".github/workflows/mint-name-spoof.yml",
                "event": "push",
                "head_branch": "main",
                "head_sha": RELEASE_GATE_FIXTURE_SHA,
                "status": "completed",
                "conclusion": "failure",
                "check_suite_id": 106,
            }
        )
        check_runs.append(
            {
                "name": "Mint release tag",
                "status": "completed",
                "conclusion": "failure",
                "id": 8_101,
                "app_id": GITHUB_ACTIONS_APP_ID,
                "app_slug": "github-actions",
                "check_suite_id": 106,
                "head_sha": RELEASE_GATE_FIXTURE_SHA,
            }
        )

    assert_release_gate_fixture_rejected(
        release_gate,
        "same-name tag check from another workflow is not self-excluded",
        "release-tag job name claimed by another producer: Mint release tag",
        add_external_mint_failure,
    )

    def add_external_mint_success(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        current_run: dict[str, object],
    ) -> None:
        """A green namesake is the same authority conflict as a red one."""

        add_external_mint_failure(check_runs, workflow_runs, current_run)
        for run in check_runs:
            if run["name"] == "Mint release tag" and run["id"] == 8_101:
                run["conclusion"] = "success"

    assert_release_gate_fixture_rejected(
        release_gate,
        "green same-name tag check from another workflow is admitted",
        "release-tag job name claimed by another producer: Mint release tag",
        add_external_mint_success,
    )

    def add_higher_id_success_collision(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        real = required_check_fixture(
            check_runs,
            "Check & Test (ubuntu-latest)",
        )
        real["conclusion"] = "failure"
        spoof = real.copy()
        spoof.update(
            {
                "id": 10_000,
                "conclusion": "success",
                "check_suite_id": 999,
            }
        )
        check_runs.append(spoof)
        workflow_runs.append(
            {
                "id": 9_999,
                "workflow_id": 999,
                "path": ".github/workflows/required-context-spoof.yaml",
                "event": "push",
                "head_branch": "main",
                "head_sha": RELEASE_GATE_FIXTURE_SHA,
                "status": "completed",
                "conclusion": "success",
                "check_suite_id": 999,
            }
        )

    assert_release_gate_fixture_rejected(
        release_gate,
        "higher-ID same-name success masks a failed required producer",
        (
            "required check workflow provenance mismatch: "
            "Check & Test (ubuntu-latest)"
        ),
        add_higher_id_success_collision,
    )

    def add_duplicate_success(
        check_runs: list[dict[str, object]],
        _workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        duplicate = required_check_fixture(check_runs, "cargo-deny").copy()
        duplicate["id"] = 10_001
        check_runs.append(duplicate)

    assert_release_gate_fixture_rejected(
        release_gate,
        "two successful check-runs claim one required context",
        "ambiguous required check: cargo-deny (2 check-runs under one provenance)",
        add_duplicate_success,
    )

    def retry(first: str, second: str, second_start: str) -> Callable[
        [list[dict[str, object]], list[dict[str, object]], dict[str, object]], None
    ]:
        def mutate(
            check_runs: list[dict[str, object]],
            _workflow_runs: list[dict[str, object]],
            _current_run: dict[str, object],
        ) -> None:
            original = required_check_fixture(check_runs, "cargo-deny")
            attempt = original.copy()
            original.update(
                {
                    "conclusion": first,
                    "started_at": "2026-09-28T10:00:00Z",
                    "completed_at": "2026-09-28T10:20:00Z",
                }
            )
            attempt.update(
                {
                    "id": 10_002,
                    "conclusion": second,
                    "started_at": second_start,
                    "completed_at": "2026-09-28T10:45:00Z",
                }
            )
            check_runs.append(attempt)

        return mutate

    assert_release_gate_admits(
        release_gate,
        "one failed attempt of a required job retried to success in its own run",
        ("required check passed on its one admitted retry: cargo-deny",),
        retry("failure", "success", "2026-09-28T10:25:00Z"),
    )
    assert_release_gate_fixture_rejected(
        release_gate,
        "a passing attempt followed by a failing one is no retry",
        "ambiguous required check: cargo-deny (2 check-runs under one provenance)",
        retry("success", "failure", "2026-09-28T10:25:00Z"),
    )
    assert_release_gate_fixture_rejected(
        release_gate,
        "a retry that started before the failure finished is no retry",
        "ambiguous required check: cargo-deny (2 check-runs under one provenance)",
        retry("failure", "success", "2026-09-28T10:10:00Z"),
    )

    def add_landing_push_producer(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        """The landing push republishes every context the queue already proved.

        Both builds carry the same sha because the queue's speculative commit
        IS the squash that lands. Crediting both would make every context
        ambiguous, so the queue build is the authority and this one is
        corroboration: judged when it has concluded, never waited for.
        """

        for path, suite in (
            (".github/workflows/ci.yml", 301),
            (".github/workflows/sast.yml", 302),
            (".github/workflows/secret-scan.yml", 303),
            (ACCEPTANCE_PATH, 304),
        ):
            source = workflow_fixture(
                workflow_runs,
                next(
                    run["check_suite_id"]
                    for run in workflow_runs
                    if run.get("path") == path
                ),
            ).copy()
            source.update(
                {
                    "id": suite * 10,
                    "event": "push",
                    "head_branch": "main",
                    "check_suite_id": suite,
                }
            )
            workflow_runs.append(source)
        for name in (*REQUIRED_RELEASE_CHECKS, ACCEPTANCE_CONTEXT):
            if name == ACCEPTANCE_CONTEXT:
                workflow_path = ACCEPTANCE_PATH
            else:
                _, workflow_path, _ = REQUIRED_RELEASE_CHECK_PROVENANCE[name]
            push_copy = required_check_fixture(check_runs, name).copy()
            push_copy.update(
                {
                    "id": 20_000 + len(check_runs),
                    "check_suite_id": {
                        ".github/workflows/ci.yml": 301,
                        ".github/workflows/sast.yml": 302,
                        ".github/workflows/secret-scan.yml": 303,
                        ACCEPTANCE_PATH: 304,
                    }[workflow_path],
                }
            )
            check_runs.append(push_copy)

    merge_queue_fixture = execute_release_check_gate(
        release_gate,
        {},
        mutate_fixture=add_landing_push_producer,
    )
    if merge_queue_fixture.returncode != 0:
        raise AssertionError(
            "a merge-queue landing's duplicate contexts blocked the release: "
            f"{merge_queue_fixture.stdout}{merge_queue_fixture.stderr}"
        )
    if "corroborated by the other admitted build" not in (
        merge_queue_fixture.stdout + merge_queue_fixture.stderr
    ):
        raise AssertionError(
            "the landing push's verdict on an already-proven tree was neither "
            "judged nor reported: "
            f"{merge_queue_fixture.stdout}{merge_queue_fixture.stderr}"
        )

    def red_landing_push_corroboration(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        current_run: dict[str, object],
    ) -> None:
        """The other build of this exact tree failed the same required context."""

        add_landing_push_producer(check_runs, workflow_runs, current_run)
        for run in check_runs:
            if run["name"] == "cargo-deny" and run["check_suite_id"] == 302:
                run["conclusion"] = "failure"

    assert_release_gate_fixture_rejected(
        release_gate,
        "the landing push failed a context the queue passed",
        "corroborating required check not green: cargo-deny (conclusion=failure",
        red_landing_push_corroboration,
    )

    def unfinished_landing_push_corroboration(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        current_run: dict[str, object],
    ) -> None:
        """The usual case: the landing push is still running when the mint fires."""

        add_landing_push_producer(check_runs, workflow_runs, current_run)
        workflow_fixture(workflow_runs, 301).update(
            {"status": "in_progress", "conclusion": None}
        )
        for run in check_runs:
            if run["check_suite_id"] == 301:
                run.update({"status": "in_progress", "conclusion": None})

    assert_release_gate_admits(
        release_gate,
        "the landing push is still running when the queue's proof is complete",
        (
            "not waiting for the other admitted build of a required context: "
            "Check & Test (ubuntu-latest) (status=in_progress",
        ),
        unfinished_landing_push_corroboration,
    )

    def skipped_queue_leg(
        check_runs: list[dict[str, object]],
        _workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        """The queue build skipped a leg the mint now keys off.

        This is the pre-rekey shape of the merge_group build, and it is the
        defect the rekey would reintroduce if the Windows leg were admitted as
        proof while the queue kept skipping it.
        """

        required_check_fixture(
            check_runs,
            "Windows installer + vector release build",
        )["conclusion"] = "skipped"

    assert_release_gate_fixture_rejected(
        release_gate,
        "the queue's skipped Windows leg is admitted as release evidence",
        (
            "required check not green: Windows installer + vector release build "
            "(conclusion=skipped)"
        ),
        skipped_queue_leg,
    )

    def absent_queue_leg(
        check_runs: list[dict[str, object]],
        _workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        """The queue build published no such context at all."""

        check_runs[:] = [
            run
            for run in check_runs
            if run["name"] != "Windows installer + vector release build"
        ]

    absent_leg_fixture = execute_release_check_gate(
        release_gate,
        {},
        mutate_fixture=absent_queue_leg,
    )
    if absent_leg_fixture.returncode != 2:
        raise AssertionError(
            "a required context the admitted build never published no longer "
            "holds the mint for retry: "
            f"rc={absent_leg_fixture.returncode} "
            f"{absent_leg_fixture.stdout}{absent_leg_fixture.stderr}"
        )
    if "missing required check: Windows installer + vector release build" not in (
        absent_leg_fixture.stdout + absent_leg_fixture.stderr
    ):
        raise AssertionError(
            "absent queue evidence was held without naming the missing "
            f"producer: {absent_leg_fixture.stdout}{absent_leg_fixture.stderr}"
        )

    def queue_ref_names_another_base(
        _check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        """The queue built a tree whose parent is not this release's parent."""

        workflow_fixture(workflow_runs, 101)["head_branch"] = (
            "gh-readonly-queue/main/pr-958-" + "9" * 40
        )

    assert_release_gate_fixture_rejected(
        release_gate,
        "the queue build credited to this release was built on another base",
        "merge-group evidence proved a different tree",
        queue_ref_names_another_base,
    )

    # The other half of that equality, mutated on the git side. The parent is
    # read from the protected history this job checked out rather than from the
    # endpoint being judged, so this proves the assertion consults it at all
    # instead of comparing the API's answer with itself.
    wrong_parent_fixture = execute_release_check_gate(
        release_gate,
        {},
        parent_sha="9" * 40,
    )
    if wrong_parent_fixture.returncode == 0:
        raise AssertionError(
            "a queue build was credited to a release whose first parent is not "
            "the base that build was made on"
        )
    if "merge-group evidence proved a different tree" not in (
        wrong_parent_fixture.stdout + wrong_parent_fixture.stderr
    ):
        raise AssertionError(
            "the sha-identity refusal fired for the wrong reason: "
            f"{wrong_parent_fixture.stdout}{wrong_parent_fixture.stderr}"
        )

    def queue_build_proved_another_sha(
        _check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        workflow_fixture(workflow_runs, 101)["head_sha"] = "7" * 40

    assert_release_gate_fixture_rejected(
        release_gate,
        "the queue build credited to this release proved another sha",
        "merge-group evidence sha mismatch",
        queue_build_proved_another_sha,
    )

    def queue_ref_is_an_ordinary_branch(
        _check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        """A branch that merely looks like queue evidence cannot nominate itself."""

        workflow_fixture(workflow_runs, 101)["head_branch"] = "attacker/queue-lookalike"

    assert_release_gate_fixture_rejected(
        release_gate,
        "an ordinary branch claims to be the queue build",
        "merge-group evidence is not on a merge-queue ref",
        queue_ref_is_an_ordinary_branch,
    )

    def two_uncancelled_queue_builds(
        _check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        rebuild = workflow_fixture(workflow_runs, 101).copy()
        rebuild.update({"id": 1_101, "check_suite_id": 401})
        workflow_runs.append(rebuild)

    assert_release_gate_fixture_rejected(
        release_gate,
        "two uncancelled queue builds of one sha are collapsed by recency",
        "ambiguous merge-group evidence",
        two_uncancelled_queue_builds,
    )

    def superseded_queue_build(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        """A speculative group the queue cancelled and rebatched gated nothing."""

        cancelled = workflow_fixture(workflow_runs, 101).copy()
        cancelled.update(
            {"id": 1_102, "check_suite_id": 402, "conclusion": "cancelled"}
        )
        workflow_runs.append(cancelled)
        check_runs.append(
            {
                "name": "Check & Test (ubuntu-latest)",
                "status": "completed",
                "conclusion": "cancelled",
                "id": 40_201,
                "app_id": GITHUB_ACTIONS_APP_ID,
                "app_slug": "github-actions",
                "check_suite_id": 402,
                "head_sha": RELEASE_GATE_FIXTURE_SHA,
            }
        )

    superseded_fixture = execute_release_check_gate(
        release_gate,
        {},
        mutate_fixture=superseded_queue_build,
    )
    if superseded_fixture.returncode != 0:
        raise AssertionError(
            "a cancelled superseded queue group was read as a competing "
            f"authority: {superseded_fixture.stdout}{superseded_fixture.stderr}"
        )

    def no_queue_build_at_all(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        current_run: dict[str, object],
    ) -> None:
        """A commit that reached main without a queue build is still releasable.

        The landing push is the fallback authority, which is exactly the
        evidence the mint used before it was rekeyed. Removing that path would
        make one queue outage an unreleasable repository.
        """

        add_landing_push_producer(check_runs, workflow_runs, current_run)
        queue_suites = {101, 102, 103, 107, 108}
        check_runs[:] = [
            run for run in check_runs if run["check_suite_id"] not in queue_suites
        ]
        workflow_runs[:] = [
            run
            for run in workflow_runs
            if run.get("check_suite_id") not in queue_suites
        ]

    assert_release_gate_admits(
        release_gate,
        "a landing with no queue build at all",
        (
            "admitting on landing-push evidence: no merge-group build exists at",
        ),
        no_queue_build_at_all,
    )

    def ruleset_context_nothing_publishes(
        check_runs: list[dict[str, object]],
        _workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        """Ruleset 19746451 still gates a context no producer publishes.

        Nothing in this repository can read that ruleset, and the queue's own
        symptom is a wait with every visible check green, so the mint reading
        its reviewed mirror against the sha is the only place this becomes a
        stated failure.
        """

        check_runs[:] = [
            run for run in check_runs if run["name"] != "PR text hygiene"
        ]

    assert_release_gate_fixture_rejected(
        release_gate,
        "a ruleset-gated context that no admitted build published",
        "ruleset-required context has no admitted build at this sha: PR text hygiene",
        ruleset_context_nothing_publishes,
    )

    def ruleset_context_is_red(
        check_runs: list[dict[str, object]],
        _workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        required_check_fixture(check_runs, "PR text hygiene")["conclusion"] = "failure"

    assert_release_gate_fixture_rejected(
        release_gate,
        "a ruleset-gated context that the admitted build failed",
        "ruleset-required context not green: PR text hygiene (conclusion=failure)",
        ruleset_context_is_red,
    )

    def non_required_check(
        name: str,
        suite_id: int,
        check_id: int,
        *,
        status: str = "completed",
        conclusion: str | None = "failure",
    ) -> dict[str, object]:
        return {
            "name": name,
            "status": status,
            "conclusion": conclusion,
            "id": check_id,
            "app_id": GITHUB_ACTIONS_APP_ID,
            "app_slug": "github-actions",
            "check_suite_id": suite_id,
            "head_sha": RELEASE_GATE_FIXTURE_SHA,
        }

    def add_red_advisory_in_the_queue_build(
        check_runs: list[dict[str, object]],
        _workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        """An advisory job reds inside the build the mint now keys off.

        Before the rekey this was discounted, because a merge_group job the
        queue never waited for concluded after the merge having gated nothing.
        The queue's build is now this release's evidence, so a red in it is
        the same class of fact as a red in the landing push: admitted over,
        and announced. The disclosure got wider here rather than darker, which
        is the property the rekey had to preserve.
        """

        check_runs.append(
            non_required_check("Windows authority tests", 101, 30_001)
        )

    assert_release_gate_admits(
        release_gate,
        "an advisory job reds inside the admitted queue build",
        (
            "admitting over a red check no required context covers: "
            "Windows authority tests (conclusion=failure",
        ),
        add_red_advisory_in_the_queue_build,
    )

    def add_red_queue_only_job(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        """A red job from another queue entry's build of this same sha."""

        workflow_runs.append(
            {
                "id": 30_100,
                "workflow_id": 400_200,
                "path": ".github/workflows/queue-only.yml",
                "event": "merge_group",
                "head_branch": "gh-readonly-queue/main/pr-2-" + "0" * 40,
                "head_sha": RELEASE_GATE_FIXTURE_SHA,
                "status": "completed",
                "conclusion": "failure",
                "check_suite_id": 302,
            }
        )
        check_runs.append(non_required_check("Queue-only smoke", 302, 30_101))

    assert_release_gate_admits(
        release_gate,
        "a red job from a build neither tier admits",
        (
            "discounting a red check that is not this release's evidence: "
            "Queue-only smoke",
            "it has no admitted build at this sha",
        ),
        add_red_queue_only_job,
    )

    def add_red_non_required_push_job(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        """A registry blip reds a landing-push job no ruleset requires."""

        workflow_runs.append(
            {
                "id": 31_000,
                "workflow_id": 400_300,
                "path": ".github/workflows/docker.yml",
                "event": "push",
                "head_branch": "main",
                "head_sha": RELEASE_GATE_FIXTURE_SHA,
                "status": "completed",
                "conclusion": "failure",
                "check_suite_id": 303,
            }
        )
        check_runs.append(
            non_required_check("Build and publish dev daemon image", 303, 31_001)
        )

    assert_release_gate_admits(
        release_gate,
        "a red landing-push check that no required context covers",
        (
            "admitting over a red check no required context covers: "
            "Build and publish dev daemon image (conclusion=failure",
        ),
        add_red_non_required_push_job,
    )

    def red_non_required_job_inside_required_producer(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        """A red advisory job turns its whole required producer's run red."""

        ci_suite = required_check_fixture(
            check_runs,
            "Check & Test (ubuntu-latest)",
        )["check_suite_id"]
        workflow_fixture(workflow_runs, ci_suite)["conclusion"] = "failure"
        check_runs.append(non_required_check("Code Coverage", ci_suite, 32_001))

    assert_release_gate_admits(
        release_gate,
        "a red advisory job inside the workflow that produces required contexts",
        (
            "admitting over a producing run that concluded failure: "
            ".github/workflows/ci.yml run 1001",
            "admitting over a red check no required context covers: "
            "Code Coverage (conclusion=failure",
        ),
        red_non_required_job_inside_required_producer,
    )

    def unfinished_non_required_check(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        """An advisory job still running when every required context is green."""

        ci_suite = required_check_fixture(
            check_runs,
            "Check & Test (ubuntu-latest)",
        )["check_suite_id"]
        # This coupling is mandatory in the real Actions API: while any sibling
        # job is in progress, its producer workflow cannot be completed/success.
        # Leaving the fixture completed creates an impossible state and lets the
        # gate keep waiting on the aggregate without the test noticing.
        workflow_fixture(workflow_runs, ci_suite).update(
            {"status": "in_progress", "conclusion": None}
        )
        check_runs.append(
            non_required_check(
                "npm launcher tests",
                ci_suite,
                33_001,
                status="in_progress",
                conclusion=None,
            )
        )

    assert_release_gate_admits(
        release_gate,
        "an unfinished advisory check no required context covers",
        (
            "admitting over a producing run that is still in_progress: "
            ".github/workflows/ci.yml run 1001",
            "not waiting for a check no required context covers: "
            "npm launcher tests (status=in_progress)",
        ),
        unfinished_non_required_check,
    )

    def unfinished_required_check(
        check_runs: list[dict[str, object]],
        _workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        required_check_fixture(check_runs, "cargo-deny").update(
            {"status": "in_progress", "conclusion": None}
        )

    unfinished_required_fixture = execute_release_check_gate(
        release_gate,
        {},
        mutate_fixture=unfinished_required_check,
    )
    if unfinished_required_fixture.returncode != 2:
        raise AssertionError(
            "an unfinished required context no longer holds the mint for retry: "
            f"rc={unfinished_required_fixture.returncode} "
            f"{unfinished_required_fixture.stdout}"
            f"{unfinished_required_fixture.stderr}"
        )

    def change_required_workflow_path(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        check = required_check_fixture(check_runs, "cargo-deny")
        workflow = workflow_fixture(workflow_runs, check["check_suite_id"])
        workflow["path"] = ".github/workflows/required-context-spoof.yaml"

    assert_release_gate_fixture_rejected(
        release_gate,
        "required check comes from the wrong workflow path",
        "required check workflow provenance mismatch: cargo-deny",
        change_required_workflow_path,
    )

    def change_required_workflow_id(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        check = required_check_fixture(check_runs, "cargo-deny")
        workflow = workflow_fixture(workflow_runs, check["check_suite_id"])
        workflow["workflow_id"] = 999

    assert_release_gate_fixture_rejected(
        release_gate,
        "required check comes from the wrong workflow identity",
        "required check workflow provenance mismatch: cargo-deny",
        change_required_workflow_id,
    )

    def change_required_workflow_event(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        check = required_check_fixture(
            check_runs,
            "gitleaks (full history)",
        )
        workflow = workflow_fixture(workflow_runs, check["check_suite_id"])
        workflow["event"] = "workflow_run"

    assert_release_gate_fixture_rejected(
        release_gate,
        "required check comes from the wrong workflow event",
        "required check workflow provenance mismatch: gitleaks (full history)",
        change_required_workflow_event,
    )

    def change_required_check_head(
        check_runs: list[dict[str, object]],
        _workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        check = required_check_fixture(
            check_runs,
            "Windows installer + vector release build",
        )
        check["head_sha"] = "2" * 40

    assert_release_gate_fixture_rejected(
        release_gate,
        "required check is attached to the wrong head sha",
        (
            "required check has wrong head sha: "
            "Windows installer + vector release build"
        ),
        change_required_check_head,
    )

    def change_required_workflow_head(
        check_runs: list[dict[str, object]],
        workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        # Deliberately not a ci.yml context. ci.yml's run is the queue anchor,
        # and moving its head sha is refused earlier and more specifically by
        # the sha-identity assertion, which `queue_build_proved_another_sha`
        # covers. This case is the per-context provenance path, which every
        # non-anchor producer still has to be judged against.
        check = required_check_fixture(check_runs, "cargo-deny")
        workflow = workflow_fixture(workflow_runs, check["check_suite_id"])
        workflow["head_sha"] = "3" * 40

    assert_release_gate_fixture_rejected(
        release_gate,
        "required workflow run is attached to the wrong head sha",
        "required check workflow provenance mismatch: cargo-deny",
        change_required_workflow_head,
    )

    def change_required_check_app(
        check_runs: list[dict[str, object]],
        _workflow_runs: list[dict[str, object]],
        _current_run: dict[str, object],
    ) -> None:
        check = required_check_fixture(check_runs, "DCO Sign-off")
        check["app_slug"] = "unreviewed-checks-app"

    assert_release_gate_fixture_rejected(
        release_gate,
        "required check comes from a different checks app",
        "required check name claimed by another app: DCO Sign-off",
        change_required_check_app,
    )

    check_inherited_export_tier(release_gate)
    check_timed_out_retry(release_gate)
    check_acceptance_veto(release_gate)



def check_tag_readback_and_ruleset_mirror(release_tag: str) -> None:
    assert_tag_readback_retries(release_tag)
    assert_required_check_set_is_single_sourced(release_tag)
    assert_ruleset_mirror_stays_a_superset(release_tag)
    for label, old_name, new_name in (
        (
            "the mirror is synced down to a thinned live ruleset",
            "            Falsify guards\n            Feature permutation tests (ubuntu-latest)\n",
            "",
        ),
        (
            "one name is dropped from the mirror alone",
            "            Windows installer + vector release build\n            PR text hygiene\n",
            "            PR text hygiene\n",
        ),
    ):
        mirror_start = release_tag.index("          RULESET_REQUIRED_CHECKS: |")
        head, mirror = release_tag[:mirror_start], release_tag[mirror_start:]
        if mirror.count(old_name) != 1:
            raise AssertionError(
                f"ruleset mirror falsification could not identify {label}"
            )
        expect_assertion(
            label,
            "must stay a superset",
            lambda mutant=head + mirror.replace(old_name, new_name, 1): (
                assert_ruleset_mirror_stays_a_superset(mutant)
            ),
        )



def check_soft_decline_is_legible(release_tag: str) -> None:
    assert_soft_decline_is_legible(release_tag)



def check_mint_trigger_survives_advisory_flakes(release_tag: str) -> None:
    assert_mint_trigger_survives_advisory_flakes(release_tag)
