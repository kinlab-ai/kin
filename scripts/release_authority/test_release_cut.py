# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Checks that pin .github/workflows/release-cut.yml."""

from __future__ import annotations

import os
import re
import subprocess
import sys
import tempfile
import textwrap
from pathlib import Path

from .test_helpers import (
    DEFAULT_WORKFLOW_TOKEN,
    RELEASE_APP_TOKEN,
    ROOT,
    STEP_ENV_TOKEN_BINDING,
    expect_assertion,
    require,
    workflow_job_blocks,
    workflow_step_source,
)


PERMISSION_SCOPE_LINE = re.compile(r"(?m)^\s+(permission-[a-z-]+):")
# kin-release-bot's installation does not carry Actions today
# (verified live via `gh api orgs/firelock-ai/installations`: administration:
# read, contents:write, issues:write, metadata:read, pull_requests:write, no
# actions key), so the dispatch token below must degrade to the workflow
# token whenever the narrower mint failed, never dispatch unconditionally.
RC_BUILD_DISPATCH_TOKEN_STEP_ANCHOR = (
    "      - name: Mint repository-scoped dispatch token"
)
RC_BUILD_DISPATCH_STEP_ANCHOR = "      - name: Dispatch the candidate archive build"
RC_BUILD_DISPATCH_TOKEN_EXPR = (
    "${{ steps.dispatch-token.outcome == 'success' && "
    "steps.dispatch-token.outputs.token || github.token }}"
)
RELEASE_TAG_NUDGE_STEP_ANCHOR = "      - name: Nudge release-tag to evaluate now"


def assert_native_startup_release_proof(release_cut: str) -> None:
    block = release_cut.split("  preflight:\n", 1)[1].split("\n  publish:", 1)[0]
    proof_name = "      - name: Prove startup recovery across native daemon restarts\n"
    upload_name = "      - name: Upload this leg's record\n"
    if proof_name not in block or block.index(proof_name) > block.index(upload_name):
        raise AssertionError("native startup proof must precede the successful leg upload")
    proof = block.split(proof_name, 1)[1].split("\n      - name:", 1)[0]
    required = [
        "if: matrix.artifact == 'kin-macos-aarch64'",
        "set -euo pipefail",
        'tar -xzf "$KIN_RC_ARCHIVE"',
        'daemon="$RUNNER_TEMP/startup-archive/kin-macos-aarch64/kin-daemon"',
        'env -i PATH="$PATH" HOME="$RUNNER_TEMP/startup-home" TMPDIR="$RUNNER_TEMP/startup-home"',
        "python3 scripts/release-proof/startup-recovery/run.py",
        '--kin "$kin" --daemon "$daemon" --output "$RUNNER_TEMP/startup-recovery"',
    ]
    active = {line.strip() for line in proof.splitlines()}
    if required[0] not in active or any(line not in proof for line in required) or "continue-on-error" in proof or "||" in proof or "set +e" in proof:
        raise AssertionError("native startup proof must fail closed on the verified macOS archive")
    evidence = block.split("      - name: Preserve native startup recovery evidence\n", 1)[1].split("\n      - name:", 1)[0]
    if "if: always() && matrix.artifact == 'kin-macos-aarch64'" not in evidence:
        raise AssertionError("native startup evidence must survive a failed probe")
    for suffix in ("/*/probe-result.json", "/*/daemon.log", "/*-init.log"):
        if "${{ runner.temp }}/startup-recovery" + suffix not in evidence:
            raise AssertionError("native startup evidence must retain logs without fixture controls")
    upload = block.split(upload_name, 1)[1]
    if "if: success()" not in upload:
        raise AssertionError("native startup failure must prevent the leg upload")


def assert_rc_build_dispatch_degrades_to_todays_token(release_cut: str) -> None:
    """RC Build's dispatch may use the App only when the App can actually send it.

    kin-release-bot's installation does not carry the Actions permission today
    (installation 148381548: administration:read, contents:write,
    issues:write, metadata:read, pull_requests:write; verified live via
    `gh api orgs/firelock-ai/installations`), so a token minted against it
    with permission-actions: write fails until a person grants that
    permission. The mint step must tolerate that failure without reddening
    the job, and the dispatch step must fall back to github.token, exactly
    what ran before this change, whenever the mint did not succeed: landing
    this ahead of the grant must change nothing observable, and the cascade
    turns on the moment the grant lands with no further edit here. The mint
    must also ask for nothing beyond actions: a second permission on this
    token duplicates scope the contents:write token beside it (for the branch
    move) already carries, on a token the dispatch never needs to use it
    through.
    """

    mint_step = workflow_step_source(
        "release cut", release_cut, RC_BUILD_DISPATCH_TOKEN_STEP_ANCHOR
    )
    require(mint_step, "continue-on-error: true", "RC Build dispatch token mint")
    require(mint_step, "permission-actions: write", "RC Build dispatch token mint")
    scopes = {match.group(1) for match in PERMISSION_SCOPE_LINE.finditer(mint_step)}
    if scopes != {"permission-actions"}:
        raise AssertionError(
            "the RC Build dispatch token must be scoped to actions only, and "
            f"is scoped to {sorted(scopes) or '<nothing>'}; a second "
            "permission on this token over-grants a credential the dispatch "
            "never needs"
        )
    dispatch_step = workflow_step_source(
        "release cut", release_cut, RC_BUILD_DISPATCH_STEP_ANCHOR
    )
    require(dispatch_step, "gh workflow run rc-build.yml", "RC Build dispatch")
    bindings = [
        match.group("token")
        for match in STEP_ENV_TOKEN_BINDING.finditer(dispatch_step)
    ]
    if bindings != [RC_BUILD_DISPATCH_TOKEN_EXPR]:
        raise AssertionError(
            "the RC Build dispatch step must bind GH_TOKEN to "
            f"{RC_BUILD_DISPATCH_TOKEN_EXPR} exactly once, falling back to "
            f"{DEFAULT_WORKFLOW_TOKEN} whenever the App mint above did not "
            f"succeed, and binds {bindings or '<nothing>'}"
        )


def assert_release_tag_nudge_uses_app_identity(release_cut: str) -> None:
    """The nudge to release-tag.yml must carry an actor that workflow accepts.

    release-tag.yml's own trigger guard allowlists troyjr4103 and
    kin-release-bot[bot] and refuses every other actor, including
    github-actions[bot], the identity a github.token-authenticated dispatch
    would carry. Sending a repository_dispatch also needs contents:write,
    which this job's own default token does not hold (permissions:
    contents: read at the top of this file). The publish job already mints a
    kin-release-bot token scoped to contents:write for kin-evidence-publish;
    this reuses that exact token rather than minting a second one or reaching
    for github.token.

    The nudge must also be best-effort. It has two fallbacks that need no
    code here to work at all, release-tag.yml's own 15-minute schedule and
    CI's workflow_run trigger, so a transient API failure on this one call
    must not redden the publish job of a cut that already durably wrote the
    record this step only announces.
    """

    step = workflow_step_source(
        "release cut", release_cut, RELEASE_TAG_NUDGE_STEP_ANCHOR
    )
    require(step, "release_tag_evaluate", "release-tag nudge")
    require(step, "repos/${GITHUB_REPOSITORY}/dispatches", "release-tag nudge")
    require(step, "continue-on-error: true", "release-tag nudge")
    bindings = [
        match.group("token") for match in STEP_ENV_TOKEN_BINDING.finditer(step)
    ]
    if bindings != [RELEASE_APP_TOKEN]:
        raise AssertionError(
            "the release-tag nudge must bind GH_TOKEN to the minted App "
            f"installation token {RELEASE_APP_TOKEN} exactly once, because "
            "release-tag.yml's own actor allowlist refuses the identity a "
            f"{DEFAULT_WORKFLOW_TOKEN} dispatch would carry, and binds "
            f"{bindings or '<nothing>'}"
        )



def assert_release_cut_arms_at_landing(release_cut: str) -> None:
    """The cut wakes on the landing push, on main alone, and can read pull requests.

    An export squash that inherits its pull request's grade can arm the RC
    Build the moment it lands, instead of after main's own CI re-grades the
    same tree, but only if the push wakes the cut. The trigger must stay on
    main: a push run resolves this workflow from the branch pushed, and this
    workflow's arm and publish jobs reach the release App's key. The select
    job reads which pull request a commit was squashed from, which needs
    pull-requests: read and nothing that writes.
    """

    header = release_cut.split("\njobs:", 1)[0]
    on_block = header.split("\non:\n", 1)[1] if "\non:\n" in header else ""
    trigger = re.search(r"(?m)^  push:\n((?:    .*\n)*)", on_block)
    if trigger is None:
        raise AssertionError("release-cut.yml must wake on a push to main so a landing can arm at once")
    if trigger.group(1) != "    branches: [main]\n":
        raise AssertionError(
            "release-cut.yml's push trigger must name main and nothing else, since a push "
            f"run resolves this workflow from the branch pushed: {trigger.group(1)!r}"
        )
    select = workflow_job_blocks(release_cut)["select"]
    permissions = re.search(r"(?m)^    permissions:\n((?:      .*\n)+)", select)
    granted = permissions.group(1) if permissions else ""
    if "      pull-requests: read\n" not in granted:
        raise AssertionError(
            "the select job must hold pull-requests: read to bind a main commit to "
            "the export pull request it inherits from"
        )
    if "write" in granted:
        raise AssertionError(f"the select job must stay read-only: {granted!r}")
    if not re.search(r"(?m)^      github\.event_name != 'workflow_run' \|\|$", select):
        raise AssertionError(
            "the select job's condition must let a non-workflow_run occasion through, "
            "or the landing push starts nothing"
        )


QUEUE_REPORT_STEP_ANCHOR = "      - name: Post the Release preflight status on the group commit"
QUEUE_SHA = "5" * 40


def queue_report_source(release_cut: str) -> str:
    """The report's script, which is the last thing in its job."""

    job = workflow_job_blocks(release_cut)["report-to-queue"]
    if QUEUE_REPORT_STEP_ANCHOR not in job:
        raise AssertionError("the queue report job no longer posts the Release preflight status")
    step = job[job.index(QUEUE_REPORT_STEP_ANCHOR):]
    marker = "        run: |\n"
    if marker not in step:
        raise AssertionError("the queue report step no longer runs a script")
    script = step[step.index(marker) + len(marker):]
    return textwrap.dedent("\n".join(line for line in script.splitlines() if not line.startswith("  #")))


def execute_queue_report(release_cut: str, *, state: str, publish: str, sha: str = QUEUE_SHA) -> tuple[int, str, str]:
    """Run the report exactly as the workflow does, with gh recording what it would post."""

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        posted = root / "posted"
        gh = root / "gh"
        gh.write_text(f'#!/usr/bin/env bash\nprintf "%s\\n" "$@" >> "{posted}"\n', encoding="utf-8")
        gh.chmod(0o755)
        environment = {
            **os.environ,
            "PATH": f"{root}:{os.environ['PATH']}",
            "QUEUE_SHA": sha,
            "QUEUE_STATE": state,
            "REASON": "merge group gh-readonly-queue/main/pr-1-x stages v0.8.4; " + "x" * 200,
            "PUBLISH": publish,
            "GITHUB_REPOSITORY": "kinlab-ai/kin",
            "GITHUB_SERVER_URL": "https://github.com",
            "GITHUB_RUN_ID": "1",
        }
        result = subprocess.run(
            ["bash", "-c", queue_report_source(release_cut)],
            env=environment,
            capture_output=True,
            text=True,
            timeout=30,
        )
        return result.returncode, result.stdout + result.stderr, posted.read_text() if posted.exists() else ""


def assert_the_queue_is_told_only_what_was_proven(release_cut: str) -> None:
    """The merge queue waits on "Release preflight"; it must only ever read the truth.

    A group whose record was filed is a success, a group whose preflight did
    not file one is a failure, a group that stages no release is let through,
    and a group with a build still to wait for is told nothing at all.
    """

    for state, publish, expected in (
        ("proof", "success", "state=success"),
        ("proof", "failure", "state=failure"),
        ("proof", "skipped", "state=failure"),
        ("success", "skipped", "state=success"),
        ("failure", "skipped", "state=failure"),
    ):
        code, log, posted = execute_queue_report(release_cut, state=state, publish=publish)
        if code != 0 or expected not in posted.split("\n") or "context=Release preflight" not in posted.split("\n"):
            raise AssertionError(
                f"the queue report for {state} with publish {publish} must post {expected}: rc={code} {log} {posted}"
            )
        description = next(line for line in posted.split("\n") if line.startswith("description="))
        if len(description) - len("description=") > 140:
            raise AssertionError(f"a commit status description is capped at 140 characters: {description}")
        if f"repos/kinlab-ai/kin/statuses/{QUEUE_SHA}" not in posted:
            raise AssertionError(f"the queue report must post on the group commit: {posted}")
    code, log, posted = execute_queue_report(release_cut, state="", publish="skipped")
    if code != 0 or posted:
        raise AssertionError(f"a group still being built must be told nothing: rc={code} {log} {posted}")
    code, _log, posted = execute_queue_report(release_cut, state="success", publish="skipped", sha="main")
    if code == 0 or posted:
        raise AssertionError("the queue report must refuse anything but an exact group commit")
    report = workflow_job_blocks(release_cut)["report-to-queue"]
    permissions = re.search(r"(?m)^    permissions:\n((?:      .*\n)+)", report)
    if permissions is None or permissions.group(1) != "      statuses: write\n":
        raise AssertionError("the queue report may hold statuses: write and nothing else")


def assert_a_queue_timeout_is_retried_in_place(release_cut: str) -> None:
    retry = workflow_job_blocks(release_cut)["retry"]
    for needle, why in (
        ("    if: needs.select.outputs.decision == 'retry'", "run only on the retry decision"),
        ('/actions/runs/${RC_RUN}/rerun-failed-jobs', "re-run the failed jobs of that very run"),
        ('if [ "$path" != .github/workflows/rc-build.yml ] || [ "$head" != "$CANDIDATE" ] || [ "$status" != completed ]; then',
         "re-run only a finished RC Build of the candidate"),
        ("permission-actions: write", "mint the App's actions-only token"),
        ("${{ steps.dispatch-token.outcome == 'success' && steps.dispatch-token.outputs.token || github.token }}",
         "fall back to the workflow token"),
    ):
        if needle not in retry:
            raise AssertionError(f"the retry job must {why}: {needle}")


def assert_a_record_is_filed_only_for_main_or_a_live_group(release_cut: str) -> None:
    publish = workflow_step_source(
        "release cut", workflow_job_blocks(release_cut)["publish"], "      - name: Re-bind the publish to protected main history"
    )
    if "python3 scripts/select-release-candidate.py bind-candidate" not in publish:
        raise AssertionError("the publish job must bind its candidate to protected main or a live merge group")
    if '--policy-sha "$CANDIDATE"' in publish:
        raise AssertionError(
            "the publish job binds its candidate with bind-candidate, which proves protected main "
            "history or a live merge group; requiring main alone refuses every queued release"
        )


def assert_the_record_names_the_run_it_judged(release_cut: str) -> None:
    """preflight.json names its RC Build run, which the release locator then ships from.

    A commit the merge queue and a fallback dispatch both built has two runs a
    release could take bytes from. The record names the one every leg judged,
    which is the selected run the preflight matrix downloaded from.
    """

    merge = workflow_step_source(
        "release cut",
        workflow_job_blocks(release_cut)["publish"],
        "      - name: Merge the leg records into the candidate's preflight record",
    )
    for needle, why in (
        ("RC_RUN: ${{ needs.select.outputs.rc_run }}", "take the run the preflight legs downloaded from"),
        ('--rc-run "$RC_RUN"', "record that run as rc_build.run_id"),
    ):
        if needle not in merge:
            raise AssertionError(f"the publish job must {why}: {needle}")
    preflight = workflow_job_blocks(release_cut)["preflight"]
    if 'gh run download "$RC_RUN"' not in preflight or "RC_RUN: ${{ needs.select.outputs.rc_run }}" not in preflight:
        raise AssertionError("the preflight legs must download from the same selected run the record names")


def check_queue_window_cut(release_cut: str) -> None:
    assert_the_queue_is_told_only_what_was_proven(release_cut)

    def mutate(before: str, after: str) -> str:
        if release_cut.count(before) != 1:
            raise AssertionError(f"queue report falsification needle is not unique: {before!r}")
        return release_cut.replace(before, after, 1)

    expect_assertion(
        "the queue hears success whatever the publish concluded",
        "must post state=failure",
        lambda: assert_the_queue_is_told_only_what_was_proven(
            mutate('if [ "$PUBLISH" = success ]; then', "if true; then")
        ),
    )
    expect_assertion(
        "the queue hears a verdict while the build is still running",
        "must be told nothing",
        lambda: assert_the_queue_is_told_only_what_was_proven(
            mutate("            success|failure) ;;\n", "            *) ;;\n")
        ),
    )
    expect_assertion(
        "the queue report gains a write beyond statuses",
        "statuses: write and nothing else",
        lambda: assert_the_queue_is_told_only_what_was_proven(
            release_cut.replace("    permissions:\n      statuses: write\n", "    permissions:\n      statuses: write\n      contents: write\n", 1)
        ),
    )
    assert_a_queue_timeout_is_retried_in_place(release_cut)
    expect_assertion(
        "the retry re-runs whatever run it is handed",
        "re-run only a finished RC Build of the candidate",
        lambda: assert_a_queue_timeout_is_retried_in_place(
            release_cut.replace('|| [ "$head" != "$CANDIDATE" ] ', "", 1)
        ),
    )
    assert_the_record_names_the_run_it_judged(release_cut)
    expect_assertion(
        "the record stops naming the run its legs judged",
        "record that run as rc_build.run_id",
        lambda: assert_the_record_names_the_run_it_judged(
            release_cut.replace('            --rc-run "$RC_RUN" \\\n', "", 1)
        ),
    )
    assert_a_record_is_filed_only_for_main_or_a_live_group(release_cut)
    expect_assertion(
        "the publish job binds a queued candidate to main alone again",
        "must bind its candidate",
        lambda: assert_a_record_is_filed_only_for_main_or_a_live_group(
            release_cut.replace("python3 scripts/select-release-candidate.py bind-candidate", "python3 scripts/verify-protected-main-history.py", 1)
        ),
    )


def check_native_startup_and_dispatch_identity(release_cut: str) -> None:
    check_queue_window_cut(release_cut)
    assert_release_cut_arms_at_landing(release_cut)
    for label, before, after, error in (
        ("the cut stops waking on the landing push", "  push:\n    branches: [main]\n", "", "must wake on a push"),
        ("the push trigger widens to every branch", "  push:\n    branches: [main]\n",
         '  push:\n    branches: ["**"]\n', "must name main and nothing else"),
        ("the push trigger also takes tags", "  push:\n    branches: [main]\n",
         "  push:\n    branches: [main]\n    tags: [v*]\n", "must name main and nothing else"),
        ("the select job loses its pull request read", "      pull-requests: read\n", "", "pull-requests: read"),
        ("the select job gains a write", "      pull-requests: read\n", "      pull-requests: write\n",
         "pull-requests: read"),
    ):
        expect_assertion(
            label,
            error,
            lambda before=before, after=after: assert_release_cut_arms_at_landing(
                release_cut.replace(before, after, 1)
            ),
        )

    assert_native_startup_release_proof(release_cut)
    for label, before, after, error in [
        ("missing native startup proof", "Prove startup recovery across native daemon restarts", "Removed native startup proof", "must precede"),
        ("wrong native startup archive", "tar -xzf \"$KIN_RC_ARCHIVE\"", "tar -xzf unrelated.tar.gz", "must fail closed"),
        ("inherited native startup credentials", 'env -i PATH="$PATH" HOME="$RUNNER_TEMP/startup-home" TMPDIR="$RUNNER_TEMP/startup-home"', 'env PATH="$PATH"', "must fail closed"),
        ("ignored native startup failure", 'test -x "$daemon"', 'test -x "$daemon"\n          false || true', "must fail closed"),
        ("startup evidence lost on failure", "if: always() && matrix.artifact == 'kin-macos-aarch64'", "if: success() && matrix.artifact == 'kin-macos-aarch64'", "must survive"),
    ]:
        expect_assertion(label, error, lambda before=before, after=after: assert_native_startup_release_proof(release_cut.replace(before, after, 1)))

    # RC Build completing does not cascade into this workflow's own
    # workflow_run trigger when RC Build was dispatched on github.token, which
    # it always is: a run started that way completes without producing the
    # occasion anything is listening for. Both new hops below replace which
    # credential performs a dispatch, never what a dispatch is allowed to do.
    assert_rc_build_dispatch_degrades_to_todays_token(release_cut)
    dispatch_token_mint = workflow_step_source(
        "release cut", release_cut, RC_BUILD_DISPATCH_TOKEN_STEP_ANCHOR
    )
    expect_assertion(
        "dispatch token mint loses its continue-on-error",
        "is missing required policy: continue-on-error: true",
        lambda: assert_rc_build_dispatch_degrades_to_todays_token(
            release_cut.replace(
                dispatch_token_mint,
                dispatch_token_mint.replace(
                    "        continue-on-error: true\n", "", 1
                ),
                1,
            )
        ),
    )
    expect_assertion(
        "dispatch token mint gains a contents scope too",
        "must be scoped to actions only",
        lambda: assert_rc_build_dispatch_degrades_to_todays_token(
            release_cut.replace(
                dispatch_token_mint,
                dispatch_token_mint.replace(
                    "          permission-actions: write\n",
                    "          permission-actions: write\n          permission-contents: write\n",
                    1,
                ),
                1,
            )
        ),
    )
    rc_build_dispatch = workflow_step_source(
        "release cut", release_cut, RC_BUILD_DISPATCH_STEP_ANCHOR
    )
    expect_assertion(
        "dispatch step drops its fallback to the workflow token",
        "must bind GH_TOKEN to",
        lambda: assert_rc_build_dispatch_degrades_to_todays_token(
            release_cut.replace(
                rc_build_dispatch,
                STEP_ENV_TOKEN_BINDING.sub(
                    lambda _: "          GH_TOKEN: "
                    + RELEASE_APP_TOKEN.replace("app-token", "dispatch-token"),
                    rc_build_dispatch,
                    count=1,
                ),
                1,
            )
        ),
    )

    assert_release_tag_nudge_uses_app_identity(release_cut)
    release_tag_nudge = workflow_step_source(
        "release cut", release_cut, RELEASE_TAG_NUDGE_STEP_ANCHOR
    )
    expect_assertion(
        "release-tag nudge reverts to the workflow token",
        "must bind GH_TOKEN to",
        lambda: assert_release_tag_nudge_uses_app_identity(
            release_cut.replace(
                release_tag_nudge,
                STEP_ENV_TOKEN_BINDING.sub(
                    lambda _: f"          GH_TOKEN: {DEFAULT_WORKFLOW_TOKEN}",
                    release_tag_nudge,
                    count=1,
                ),
                1,
            )
        ),
    )
    expect_assertion(
        "release-tag nudge loses its continue-on-error",
        "is missing required policy: continue-on-error: true",
        lambda: assert_release_tag_nudge_uses_app_identity(
            release_cut.replace(
                release_tag_nudge,
                release_tag_nudge.replace(
                    "        continue-on-error: true\n", "", 1
                ),
                1,
            )
        ),
    )

    subprocess.run([sys.executable, str(ROOT / "scripts/release-proof/startup-recovery/run.py"), "--verify-only"], check=True)
    subprocess.run([sys.executable, str(ROOT / "scripts/test-startup-release-proof.py")], check=True)
