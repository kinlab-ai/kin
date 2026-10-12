# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Checks that pin .github/workflows/release-train.yml.

The release pull request, its merge policy and the release hold marker.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import tempfile
import textwrap
from pathlib import Path

from .test_helpers import (
    DEFAULT_WORKFLOW_TOKEN,
    RELEASE_APP_TOKEN,
    ROOT,
    STEP_ENV_TOKEN_BINDING,
    TAG_LISTING_FORMAT,
    TRUSTED_POLICY_PREFIX,
    expect_assertion,
    job_step_active_lines,
    require,
    workflow_job_blocks,
    workflow_step_source,
)


PREPARE_RELEASE = ROOT / "scripts" / "prepare-release.mjs"
# A repo-relative file path as the release generator writes it.
GENERATED_PATH_LITERAL = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._/-]*\.[A-Za-z0-9]+$")

RELEASE_TRAIN_BODY = ROOT / "scripts" / "release-train-body.mjs"
RELEASE_TRAIN_BODY_POLICY = "scripts/release-train-body.mjs"
RELEASE_TRAIN_BODY_BEGIN = "<!-- kin-release-train:begin -->"
RELEASE_TRAIN_BODY_END = "<!-- kin-release-train:end -->"

RELEASE_PR_STEP_ANCHOR = "      - name: Open the protected release PR"


def release_train_merge_policy_source(release_train: str) -> str:
    """Extract the exact merge-policy gate executed by the release train."""

    step_anchor = "      - name: Resolve releasable drift and SemVer intent"
    if step_anchor not in release_train:
        raise AssertionError(
            "release train no longer carries the step that owns the "
            f"merge-policy gate: {step_anchor.strip()}"
        )
    step_start = release_train.index(step_anchor)
    if "\n      - name:" not in release_train[step_start + 1 :]:
        raise AssertionError(
            "release train merge-policy step is no longer followed by another "
            "step, so its boundary cannot be resolved"
        )
    step_end = release_train.index("\n      - name:", step_start + 1)
    step = release_train[step_start:step_end]
    open_anchor = '          if [ -z "$MERGE_POLICY_TOKEN" ]; then'
    if open_anchor not in step:
        raise AssertionError(
            "release train merge-policy gate no longer opens with its "
            "installation-token guard, so the executed gate cannot be extracted"
        )
    source_start = step.index(open_anchor)
    close_anchor = "\n          git fetch origin"
    if close_anchor not in step[source_start:]:
        raise AssertionError(
            "release train merge-policy gate no longer ends before the origin "
            "fetch, so the executed gate cannot be extracted"
        )
    source_end = step.index(close_anchor, source_start)
    return textwrap.dedent(step[source_start:source_end])


def execute_merge_policy_gate(
    source: str,
    repository: dict[str, object] | None,
    *,
    token: str = "fixture-installation-token",
    raw: str | None = None,
) -> subprocess.CompletedProcess[str]:
    """Execute the real merge-policy gate against a fixture API response."""

    if (repository is None) == (raw is None):
        raise AssertionError(
            "merge-policy fixture needs exactly one of a repository object or "
            "a raw response body"
        )
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        gate = root / "merge-policy-gate.sh"
        gate.write_text(source, encoding="utf-8")
        fixture = root / "repository.json"
        fixture.write_text(
            json.dumps(repository) if raw is None else raw, encoding="utf-8"
        )
        binaries = root / "bin"
        binaries.mkdir()
        shim = binaries / "gh"
        shim.write_text(
            "#!/usr/bin/env bash\n"
            "set -euo pipefail\n"
            'if [ "${GH_TOKEN:-}" != "$MERGE_POLICY_EXPECTED_TOKEN" ]; then\n'
            '  echo "policy read presented the wrong token" >&2\n'
            "  exit 77\n"
            "fi\n"
            'cat "$MERGE_POLICY_FIXTURE"\n',
            encoding="utf-8",
        )
        shim.chmod(0o755)
        environment = dict(os.environ)
        environment.pop("GH_TOKEN", None)
        environment["PATH"] = f"{binaries}{os.pathsep}{environment['PATH']}"
        environment["MERGE_POLICY_FIXTURE"] = str(fixture)
        environment["MERGE_POLICY_TOKEN"] = token
        environment["MERGE_POLICY_EXPECTED_TOKEN"] = token
        environment["REPO"] = "kinlab-ai/kin"
        return subprocess.run(
            ["bash", "-euo", "pipefail", str(gate)],
            capture_output=True,
            text=True,
            env=environment,
            check=False,
        )


def assert_merge_policy_gate(release_train: str) -> None:
    """Keep an unreadable merge policy distinct from a violated one."""

    gate = release_train_merge_policy_source(release_train)
    enforced: dict[str, object] = {
        "allow_squash_merge": True,
        "allow_merge_commit": False,
        "allow_rebase_merge": False,
        "squash_merge_commit_title": "PR_TITLE",
        "squash_merge_commit_message": "PR_BODY",
    }
    accepted = execute_merge_policy_gate(gate, enforced)
    if accepted.returncode != 0:
        raise AssertionError(
            "release train merge-policy gate refused the enforced squash "
            f"policy: {accepted.stdout}{accepted.stderr}"
        )

    # Both refusals below read the emptiness of a jq result, and jq emits
    # nothing at all for a body that is not a JSON object, so a gate that
    # judged only those results would pass having read no policy whatsoever.
    for label, body in (
        ("an empty", ""),
        ("a whitespace-only", "   \n"),
        ("a JSON array", "[]"),
        ("a bare JSON string", '"not-an-object"'),
    ):
        unread = execute_merge_policy_gate(gate, None, raw=body)
        if unread.returncode == 0:
            raise AssertionError(
                f"release train merge-policy gate accepted {label} API "
                "response, so it passed without reading any policy"
            )
        if "was not a JSON object" not in unread.stdout:
            raise AssertionError(
                f"release train merge-policy gate must refuse {label} API "
                f"response by naming the response shape: "
                f"{unread.stdout}{unread.stderr}"
            )
        if "immutable PR-body release intent" in unread.stdout:
            raise AssertionError(
                "release train merge-policy gate blamed the repository "
                f"settings for {label} response it could not read"
            )

    # A response carrying none of the policy fields is exactly what a token
    # without push-level repository access receives. It cannot disprove the
    # policy, and calling it a violation sends recovery after a correct
    # repository instead of after the token that could not read it.
    unreadable = execute_merge_policy_gate(
        gate,
        {"full_name": "kinlab-ai/kin", "permissions": {"push": False}},
    )
    if unreadable.returncode == 0:
        raise AssertionError(
            "release train merge-policy gate accepted a response that carried "
            "no merge policy at all"
        )
    if "absent from the API response" not in unreadable.stdout:
        raise AssertionError(
            "release train merge-policy gate must refuse an unreadable policy "
            f"as absent fields: {unreadable.stdout}{unreadable.stderr}"
        )
    if "immutable PR-body release intent" in unreadable.stdout:
        raise AssertionError(
            "release train merge-policy gate blamed the repository settings "
            "for a policy it never read"
        )

    for field, drift in (
        ("allow_squash_merge", False),
        ("allow_merge_commit", True),
        ("allow_rebase_merge", True),
        ("squash_merge_commit_title", "COMMIT_OR_PR_TITLE"),
        ("squash_merge_commit_message", "COMMIT_MESSAGES"),
    ):
        drifted = dict(enforced)
        drifted[field] = drift
        refused = execute_merge_policy_gate(gate, drifted)
        if refused.returncode == 0:
            raise AssertionError(
                f"release train merge-policy gate accepted drifted {field}"
            )
        if "immutable PR-body release intent" not in refused.stdout:
            raise AssertionError(
                f"release train merge-policy gate must refuse drifted {field} "
                f"as a policy violation: {refused.stdout}{refused.stderr}"
            )
        if field not in refused.stdout:
            raise AssertionError(
                "release train merge-policy gate must name the offending "
                f"setting {field}: {refused.stdout}"
            )
        if "absent from the API response" in refused.stdout:
            raise AssertionError(
                f"release train merge-policy gate reported present {field} "
                "as absent"
            )

    unscoped = execute_merge_policy_gate(gate, enforced, token="")
    if unscoped.returncode == 0:
        raise AssertionError(
            "release train merge-policy gate read the merge policy without "
            "the release App installation token"
        )


def rebind_release_pr_token(release_train: str, token: str) -> str:
    """Rewrite only the release-PR opener's own GH_TOKEN binding."""

    step = workflow_step_source(
        "release train", release_train, RELEASE_PR_STEP_ANCHOR
    )
    return release_train.replace(
        step,
        STEP_ENV_TOKEN_BINDING.sub(
            lambda _: f"          GH_TOKEN: {token}", step, count=1
        ),
        1,
    )


def assert_release_pr_author_identity(release_train: str) -> None:
    """The release pull request must be opened by the App, not by Actions.

    GitHub refuses `createPullRequest` from an Actions token wherever the
    organization withholds pull-request creation from workflows, so a train
    reaching this step on GITHUB_TOKEN dies holding a prepared release branch
    and no pull request to merge it. The App identity is load bearing a second
    time: only an App-authored pull request emits the events that start its own
    protected checks, which is what the activation fallback below it exists to
    supply when nothing else does. The binding is therefore behavior under
    test, not an incidental credential choice, and the alternative repair is
    the repository-wide toggle that would let any workflow open pull requests.
    """

    step = workflow_step_source(
        "release train", release_train, RELEASE_PR_STEP_ANCHOR
    )
    require(step, "gh pr create", "protected release PR opener")
    bindings = [
        match.group("token") for match in STEP_ENV_TOKEN_BINDING.finditer(step)
    ]
    if bindings != [RELEASE_APP_TOKEN]:
        raise AssertionError(
            "the step that opens the protected release PR must bind GH_TOKEN "
            f"to the minted App installation token {RELEASE_APP_TOKEN} exactly "
            f"once, and binds {bindings or '<nothing>'}. GitHub Actions is not "
            "permitted to create pull requests, so an Actions token here "
            "leaves every prepared release branch unopened"
        )
    if "GH_TOKEN=" in step:
        raise AssertionError(
            "the step that opens the protected release PR must not override "
            "GH_TOKEN inline, because a per-command token re-authors the pull "
            "request as an identity the step's environment no longer describes"
        )


def assert_release_pr_body_preserves_operator_text(
    release_train: str, merge_script: str
) -> None:
    """The train may own a delimited region of the release body, nothing more.

    This repository squashes with the pull-request body as the commit message,
    and the merge queue mints that message when the entry is admitted, so the
    release body is the release's permanent commit message rather than a
    description of it. The train used to overwrite the whole body with its own
    generic line on every reconcile cycle, which erased the disclosures the
    release doctrine requires an operator to add, and a cycle landing between
    that edit and queue admission would have shipped a release carrying none of
    them. A captain-side polling guard covered the window once; a guard that
    lives in a session can always be retired, so the preservation lives here.

    Reading the merge from protected main is load bearing for the same reason
    the proof gate's read is: a release branch that could carry its own body
    merge could decide what its own commit message says.
    """

    step = workflow_step_source(
        "release train", release_train, RELEASE_PR_STEP_ANCHOR
    )
    if '--body "' in step:
        raise AssertionError(
            "the release PR body must never be written from an inline literal: "
            "that overwrite is what discarded the operator-authored release "
            "disclosures the squash message has to carry"
        )
    for policy in (
        f"{TRUSTED_POLICY_PREFIX}{RELEASE_TRAIN_BODY_POLICY}",
        '--body-file "$initial_body"',
        '--body-file "$next_body"',
        '--json body',
    ):
        require(step, policy, "operator-preserving release PR body")
    index = step.find(RELEASE_TRAIN_BODY_POLICY)
    while index >= 0:
        prefix = step[max(0, index - len(TRUSTED_POLICY_PREFIX)) : index]
        if prefix != TRUSTED_POLICY_PREFIX:
            raise AssertionError(
                "the release PR body merge must be read from protected main. A "
                "branch that carries its own body merge decides what its own "
                "squash message says"
            )
        index = step.find(RELEASE_TRAIN_BODY_POLICY, index + 1)
    for marker in (RELEASE_TRAIN_BODY_BEGIN, RELEASE_TRAIN_BODY_END):
        require(merge_script, marker, "release PR body merge")


def release_branch_allowlist(release_train: str) -> str:
    """Return the regex the train uses to admit its own generated branch."""

    match = re.search(r"^\s*allowed='(?P<pattern>[^']+)'", release_train, re.M)
    if match is None:
        raise AssertionError(
            "release train no longer carries the generated-path allowlist that "
            "admits its own release branch"
        )
    return match.group("pattern")


def prepared_release_paths(generator: str) -> set[str]:
    """Return every repo path the release generator writes."""

    bindings = dict(re.findall(r"const\s+(\w+)\s*=\s*'([^']+)'\s*;", generator))
    paths: set[str] = set()
    # Array literals of paths, which is how the npm manifests are carried.
    for block in re.findall(r"const\s+\w+\s*=\s*\[([^\]]*)\]\s*;", generator):
        for literal in re.findall(r"'([^']+)'", block):
            if GENERATED_PATH_LITERAL.match(literal):
                paths.add(literal)
    # Direct writes, whether the destination is a literal or a bound constant.
    for argument in re.findall(r"fs\.writeFile\(\s*([^,\s]+)\s*,", generator):
        candidate = argument[1:-1] if argument.startswith("'") else bindings.get(argument)
        if candidate and GENERATED_PATH_LITERAL.match(candidate):
            paths.add(candidate)
    return paths


def assert_release_branch_allowlist_covers_generator(release_train: str) -> None:
    """The train must admit every path its own generator writes.

    These two drifted apart once already: the generator learned to bump a
    second lockfile and the allowlist did not, so the train refused the branch
    it had just written and no release could be prepared at all. Neither side
    is authority over the other, so the gate is that they agree.
    """

    allowed = re.compile(release_branch_allowlist(release_train))
    generated = prepared_release_paths(
        PREPARE_RELEASE.read_text(encoding="utf-8")
    )
    if not generated:
        raise AssertionError(
            "no generated release path could be resolved from "
            "prepare-release.mjs, so this cross-check proves nothing"
        )
    refused = sorted(path for path in generated if not allowed.match(path))
    if refused:
        raise AssertionError(
            "the release branch guard refuses paths the release generator "
            "writes, so the train cannot open the bump it just prepared: "
            f"{', '.join(refused)}"
        )


def assert_release_hold_marker_contract(
    release_train: str,
    hold_alarm: str,
) -> None:
    """Pin the hold marker to a producer and its one alarm title.

    A marker nobody reads and an alarm keyed to a title that moves are the two
    ways this reporting path fails back into silence, and both of them look
    exactly like a working rail from the run history. The producer and the
    deterministic consumer spell the same title and the same schema, so this
    is what stops the two drifting apart.
    """

    plan = "\n".join(
        job_step_active_lines(
            release_train, "reconcile", "name: Resolve releasable drift"
        )
    )
    stand_downs = plan.count('echo "needed=false"')
    holds = plan.count("write_marker held ")
    if stand_downs == 0:
        raise AssertionError(
            "the release train must still have a stand-down path to report"
        )
    if holds != stand_downs:
        raise AssertionError(
            "every release-train stand-down must publish a hold marker; "
            f"{stand_downs} stand-down(s) publish {holds} marker(s)"
        )
    if "write_marker clear " not in plan:
        raise AssertionError(
            "the release train must publish an all-clear marker when it "
            "proceeds, because only an all-clear can close the alarm"
        )

    schema = '"kin.release-hold.v1"'
    if schema not in plan:
        raise AssertionError(
            "the release train must stamp the reviewed hold-marker schema"
        )
    if f"MARKER_SCHEMA = {schema}" not in hold_alarm:
        raise AssertionError(
            "the hold-alarm reader must accept exactly the schema the release "
            "train stamps, or every marker reads as unreadable"
        )

    artifact = "name: release-hold-marker"
    upload = "\n".join(
        job_step_active_lines(
            release_train, "reconcile", "name: Publish this cycle's hold marker"
        )
    )
    if artifact not in upload:
        raise AssertionError(
            "the release train must upload the hold marker under the reviewed "
            "artifact name"
        )
    if "if: always()" not in upload:
        raise AssertionError(
            "the hold marker must be uploaded on every path, including the "
            "stand-downs that are the whole reason it exists"
        )
    gather = "\n".join(
        job_step_active_lines(
            release_train, "hold-alarm", "name: Gather this cycle"
        )
    )
    # Compared as a whole token, not as a substring. `release-hold-marker-v2`
    # contains `release-hold-marker`, so a prefix test would pass a rename that
    # points the reader at an artifact the train never uploads, and the alarm
    # would go quiet exactly the way it did before any of this existed.
    downloaded = re.findall(r"--name (\S+)", gather)
    if downloaded != ["release-hold-marker"]:
        raise AssertionError(
            "the alarm must download exactly the artifact the train uploads, "
            f"not {downloaded}"
        )

    title = "Release rail is held with releasable drift"
    for source, surface in (
        (hold_alarm, "the hold-alarm reader"),
        (release_train, "the release train's alarm job"),
    ):
        if source.count(title) < 1:
            raise AssertionError(
                f"{surface} must spell the one reviewed alarm title exactly, "
                "or a second issue is opened every time the wording drifts"
            )
    if re.search(r"v\d+\.\d+\.\d+", title) or any(char.isdigit() for char in title):
        raise AssertionError(
            "the alarm title must carry no tag and no count, because a title "
            "that moves with the rail opens a new issue on every move"
        )

    decide = "\n".join(
        job_step_active_lines(
            release_train, "hold-alarm", "name: Decide whether the rail"
        )
    )
    # An issue on its own leaves the run history reading all-green, and the
    # all-green run history is the surface that lied. Both loud verdicts have to
    # end the job nonzero, and neither quiet verdict may.
    for verdict in ("open)", "update)"):
        segment = decide.split(verdict, 1)
        if len(segment) != 2:
            raise AssertionError(
                f"the alarm must still handle the {verdict[:-1]} verdict"
            )
        tail = segment[1].split(";;", 1)[0]
        if "exit 1" not in tail:
            raise AssertionError(
                f"an alarm that {verdict[:-1]}s an issue must also fail its run; "
                "a green run beside an open alarm is the silence this replaces"
            )
    for verdict in ("quiet)", "close)"):
        segment = decide.split(verdict, 1)
        if len(segment) != 2:
            raise AssertionError(
                f"the alarm must still handle the {verdict[:-1]} verdict"
            )
        tail = segment[1].split(";;", 1)[0]
        if "exit 1" in tail:
            raise AssertionError(
                f"a {verdict[:-1]} verdict must not fail the run; a rail that "
                "is merely idle would then cry wolf every cycle"
            )

    # The alarm must stand down when the reconcile job was skipped, and this is
    # load-bearing rather than tidiness. Almost every train run is a skipped
    # workflow_run tick. A job that ran on those would contribute a cycle with no
    # marker, and an unreadable cycle breaks the streak, so the count could never
    # reach the threshold. It would also conclude those runs success rather than
    # skipped, which is exactly how the gather step tells a non-cycle from a
    # cycle. The alarm would read as working and be unable to fire.
    alarm_job = workflow_job_blocks(release_train).get("hold-alarm")
    if alarm_job is None:
        raise AssertionError("the release train must declare its hold-alarm job")
    alarm_condition = "\n".join(
        line for line in alarm_job.splitlines() if not line.lstrip().startswith("#")
    )
    if "needs.reconcile.result != 'skipped'" not in alarm_condition:
        raise AssertionError(
            "the hold alarm must stand down on a skipped reconcile; counting "
            "skipped ticks as cycles breaks the streak it needs to reach and "
            "erases the skipped conclusion the gather step reads"
        )

    threshold = re.search(r"DEFAULT_THRESHOLD = (\d+)", hold_alarm)
    if threshold is None:
        raise AssertionError("the hold-alarm reader must declare its threshold")
    if f'THRESHOLD: "{threshold.group(1)}"' not in release_train:
        raise AssertionError(
            "the release train must pass the same consecutive-cycle threshold "
            "the reader defaults to, so one number describes the alarm"
        )



def check_release_train_authority(release_train: str) -> None:
    for policy in (
        'name: Release Train',
        'workflows: ["CI"]',
        'cron: "7,22,37,52 * * * *"',
        "types: [release-reconcile]",
        "contents: read",
        "issues: write",
        "pull-requests: write",
        "github.event.workflow_run.event == 'push'",
        "github.event.workflow_run.head_branch == 'main'",
        "github.event.workflow_run.conclusion == 'success'",
        # The train reconciles off the queue's CI completion as well, for the
        # same reason the mint does: it is the occasion that arrives when the
        # landing does rather than half an hour later.
        "github.event.workflow_run.event == 'merge_group'",
        "startsWith(github.event.workflow_run.head_branch, 'gh-readonly-queue/main/')",
        "environment: release-tag",
        "actions/create-github-app-token@bcd2ba49218906704ab6c1aa796996da409d3eb1",
        "repositories: kin",
        "BRANCH: automation/release-next",
        'git show "refs/remotes/origin/main:${policy}"',
        'node "$policy_dir/prepare-release.mjs"',
        'node "$policy_dir/check-release-version.mjs"',
        'node "$policy_dir/release-intent.mjs"',
        "release branch contains non-generated paths",
        "git restore --source refs/remotes/origin/main",
        '.headRepositoryOwner.login == "kinlab-ai"',
        '.headRepository.nameWithOwner == "kinlab-ai/kin"',
        "highest_tag",
        "refs/remotes/origin/main:scripts/abandoned-release-tags.json",
        "refs/remotes/origin/main:scripts/select-admissible-release-tag.py",
        "git for-each-ref",
        TAG_LISTING_FORMAT,
        'python3 "$selector"',
        'highest_tag="$(cat "$admissible")"',
        "git merge --signoff --no-edit -X ours refs/remotes/origin/main",
        'gh pr merge "$PR"',
        "GH_TOKEN: ${{ steps.app-token.outputs.token }}",
        "--match-head-commit",
        "--auto",
        "--squash",
        "git commit --allow-empty --signoff",
        "Activate protected checks for the automated release PR",
        'node "$intent_policy"',
        'git show "refs/remotes/origin/main:scripts/resolve-release-intent.mjs"',
        '--base-ref "$tag"',
        "--head-ref refs/remotes/origin/main",
        "unsupported immutable release intent",
        '.squash_merge_commit_title == "PR_TITLE"',
        '.squash_merge_commit_message == "PR_BODY"',
        "immutable PR-body release intent requires enforced PR_TITLE + PR_BODY "
        "squash-only merging",
        # GitHub withholds the merge-policy fields from a token without
        # push-level repository access, so the read must stay bound to the
        # repository-scoped App installation token rather than the read-scoped
        # GITHUB_TOKEN that the rest of the step uses.
        "MERGE_POLICY_TOKEN: ${{ steps.app-token.outputs.token }}",
        'GH_TOKEN="$MERGE_POLICY_TOKEN" gh api "repos/${REPO}"',
        "absent from the API response",
        "repository merge policy response was not a JSON object",
    ):
        require(release_train, policy, "coalescing protected release train")
    assert_merge_policy_gate(release_train)
    assert_release_pr_author_identity(release_train)
    # The reverts this pin has to survive, in the order they are reachable: the
    # binding falling back to the default token, a per-command token quietly
    # restoring it while the environment still reads as the App, and creation
    # moving out from under the step the pin is anchored to.
    expect_assertion(
        "the release PR opener falls back to the Actions token",
        f"must bind GH_TOKEN to the minted App installation token {RELEASE_APP_TOKEN}",
        lambda: assert_release_pr_author_identity(
            rebind_release_pr_token(release_train, DEFAULT_WORKFLOW_TOKEN)
        ),
    )
    expect_assertion(
        "the release PR opener overrides its token for the create call",
        "must not override GH_TOKEN inline",
        lambda: assert_release_pr_author_identity(
            release_train.replace(
                "            gh pr create \\",
                '            GH_TOKEN="$FALLBACK" gh pr create \\',
                1,
            )
        ),
    )
    expect_assertion(
        "pull-request creation leaves the step whose token the pin binds",
        "protected release PR opener is missing required policy: gh pr create",
        lambda: assert_release_pr_author_identity(
            release_train.replace("gh pr create \\", "gh pr view \\", 1)
        ),
    )
    release_train_body = RELEASE_TRAIN_BODY.read_text(encoding="utf-8")
    assert_release_pr_body_preserves_operator_text(release_train, release_train_body)
    # The reverts this preservation has to survive, in the order they are
    # reachable: the generic line going back to an inline overwrite, the merge
    # moving off protected main and onto the branch under judgement, and the
    # created body losing its file.
    expect_assertion(
        "the release PR body returns to an inline literal overwrite",
        "must never be written from an inline literal",
        lambda: assert_release_pr_body_preserves_operator_text(
            release_train.replace(
                '--body-file "$next_body"',
                '--body "Automated, coalescing Kin release PR."',
                1,
            ),
            release_train_body,
        ),
    )
    expect_assertion(
        "the release PR body merge stops being read from protected main",
        f"is missing required policy: {TRUSTED_POLICY_PREFIX}"
        f"{RELEASE_TRAIN_BODY_POLICY}",
        lambda: assert_release_pr_body_preserves_operator_text(
            release_train.replace(
                f"{TRUSTED_POLICY_PREFIX}{RELEASE_TRAIN_BODY_POLICY}",
                RELEASE_TRAIN_BODY_POLICY,
                1,
            ),
            release_train_body,
        ),
    )
    expect_assertion(
        "a second body merge runs from the branch beside the trusted read",
        "must be read from protected main",
        lambda: assert_release_pr_body_preserves_operator_text(
            release_train.replace(
                'node "$merge_body"',
                f"node {RELEASE_TRAIN_BODY_POLICY}",
                1,
            ),
            release_train_body,
        ),
    )
    expect_assertion(
        "the created release PR body stops coming from the merged file",
        'is missing required policy: --body-file "$initial_body"',
        lambda: assert_release_pr_body_preserves_operator_text(
            release_train.replace('--body-file "$initial_body"', "--body-file -", 1),
            release_train_body,
        ),
    )
    expect_assertion(
        "the body merge loses the markers that delimit what the train owns",
        f"is missing required policy: {RELEASE_TRAIN_BODY_BEGIN}",
        lambda: assert_release_pr_body_preserves_operator_text(
            release_train, release_train_body.replace(RELEASE_TRAIN_BODY_BEGIN, "")
        ),
    )
    assert_release_branch_allowlist_covers_generator(release_train)



def check_release_train_forbidden_authority(release_train: str) -> None:
    # The release bump must never be resolvable from anything a merged pull
    # request can still change.
    for forbidden in (
        "workflow_dispatch:",
        "contents: write",
        "packages: write",
        "id-token: write",
        "git push --force",
        "git push -f",
        "client_payload.bump",
        "OVERRIDE_BUMP",
        "/pulls\" \\",
        "raise_bump",
        "train_labels",
    ):
        if forbidden in release_train:
            raise AssertionError(
                f"release train contains forbidden authority or history rewrite: {forbidden}"
            )



def check_release_hold_marker_contract(hold_alarm: str, release_train: str) -> None:
    # A hold is a correct decision that used to reach nobody. The marker is what
    # makes it readable and the alarm job is what makes it heard, so the pair is
    # pinned together: a producer with no consumer and a consumer with no
    # producer both read exactly like a healthy rail.
    assert_release_hold_marker_contract(release_train, hold_alarm)
    expect_assertion(
        "a release-train stand-down publishes no hold marker",
        "must publish a hold marker",
        lambda: assert_release_hold_marker_contract(
            release_train.replace(
                '            write_marker held no_drift "main has no commits beyond ${tag}" \\\n'
                '              "" "$tag" 0\n',
                "",
                1,
            ),
            hold_alarm,
        ),
    )
    expect_assertion(
        "the alarm opens an issue and still concludes the run green",
        "must also fail its run",
        lambda: assert_release_hold_marker_contract(
            release_train.replace(
                '              echo "::error::Release rail requires attention (${reason}). '
                'Opened the tracking issue with the observed failure or hold and its recovery path."\n'
                "              exit 1\n",
                '              echo "::error::Release rail requires attention (${reason}). '
                'Opened the tracking issue with the observed failure or hold and its recovery path."\n',
                1,
            ),
            hold_alarm,
        ),
    )
    # The title-agreement loop lost its only falsification when the release
    # sentinel was retired: the deleted case mutated the sentinel's copy of the
    # title, and it was the one thing proving the loop could fire at all. An
    # assertion nothing falsifies is an assertion that can stop working without
    # anybody noticing, which is the defect this whole suite exists to catch.
    # Both surviving surfaces get a case, because a loop that fires for one and
    # silently skips the other reads identically from a green run.
    expect_assertion(
        "the hold-alarm reader stops naming the one alarm title it owns",
        "must spell the one reviewed alarm title",
        lambda: assert_release_hold_marker_contract(
            release_train,
            hold_alarm.replace(
                "Release rail is held with releasable drift",
                "Release rail is stuck",
            ),
        ),
    )
    expect_assertion(
        "the release train stops naming the one alarm title the reader owns",
        "must spell the one reviewed alarm title",
        lambda: assert_release_hold_marker_contract(
            release_train.replace(
                "Release rail is held with releasable drift",
                "Release rail is stuck",
            ),
            hold_alarm,
        ),
    )
    expect_assertion(
        "the reader accepts a schema the train never stamps",
        "must accept exactly the schema",
        lambda: assert_release_hold_marker_contract(
            release_train,
            hold_alarm.replace(
                'MARKER_SCHEMA = "kin.release-hold.v1"',
                'MARKER_SCHEMA = "kin.release-hold.v2"',
            ),
        ),
    )
    expect_assertion(
        "the train and the reader disagree about how many cycles it takes",
        "must pass the same consecutive-cycle threshold",
        lambda: assert_release_hold_marker_contract(
            release_train,
            hold_alarm.replace(
                "DEFAULT_THRESHOLD = 4",
                "DEFAULT_THRESHOLD = 6",
            ),
        ),
    )
    expect_assertion(
        "the alarm counts every skipped workflow_run tick as a cycle",
        "must stand down on a skipped reconcile",
        lambda: assert_release_hold_marker_contract(
            release_train.replace(
                "    if: always() && needs.reconcile.result != 'skipped'",
                "    if: always()",
                1,
            ),
            hold_alarm,
        ),
    )
    expect_assertion(
        "the train stops uploading the marker later cycles read",
        "must download exactly the artifact the train uploads",
        lambda: assert_release_hold_marker_contract(
            release_train.replace(
                "--name release-hold-marker \\",
                "--name release-hold-marker-v2 \\",
                1,
            ),
            hold_alarm,
        ),
    )
