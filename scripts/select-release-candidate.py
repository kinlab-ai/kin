#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Select the release candidate the proof loop proves next, with nobody at a keyboard.

release-tag.yml mints a tag for the newest reviewed main commit in the staged
version's range that carries both proof records, and it refuses a required
context that appears more than once at that sha as ambiguous authority. Until
now a captain chose the sha those records would be filed under, pushed the
candidate branch, dispatched rc-build.yml and ran the preflight by hand. On
2026-09-02 that procedure lost a day: a rerun of a flaky required job made the
first candidate unmintable, and the host carrying the proof ran out of memory.

This script is the selection half of the pipeline. `select` reads everything
the decision rests on and answers one of four things:

  stand-down  nothing to do now: the version is tagged, a mint-eligible sha
              is waiting on the mint, an rc-build is still running, or no
              complete green sha exists yet
  arm         move release/v<version>-candidate to the chosen sha and dispatch
              rc-build.yml there
  proof       an rc-build for the current candidate succeeded and still holds
              the candidate archives; preflight them and publish the record
  stranger    preflight.json is filed for the candidate, so the mint can tag it
              already, and stranger.env is what would let the release claim
              first-contact coverage; run the three-arm stranger on that archive
  refuse      no reviewed main commit carrying the version qualifies, named sha
              by sha with the context that disqualified it; a hold, reported as
              a warning with exit 0 (only an unreadable read exits non-zero)

The rule, in the order it is applied:

1. v<version> exists as a tag: the cut is done and the train owns the next
   version.
2. A sha in the range carries both records: the mint owns it, and its release
   may claim first-contact coverage.
3. A current candidate, the branch tip, which must lie in the range, is kept
   until it is proven or dead. With its preflight recorded the stranger is what
   is left, and it must run on the very bytes that preflight judged, so a
   candidate whose rc-build artifacts have expired is refused rather than
   rebuilt: a second build is not guaranteed to reproduce the archive sha256
   the published record already names. Alive with a usable rc-build it is
   proven; alive with none it is armed again, up to RC_BUILD_ATTEMPT_LIMIT
   attempts. Dead means a required
   context that is red or duplicated, a CI or Acceptance push run that did not
   conclude success, or exhausted rc-build attempts.
4. A sha in the range carries preflight.json alone: the mint owns it too. Since
   2026-09-04 the mint tags on the machine preflight alone, matching the
   preflight-only tier its gate runs, so a stranger that cannot run does not
   hold a release. The cut stands down rather than arming a newer sha it would
   only race. `stranger_state` on the decision says which of the two records
   backed it, and a release missing stranger.env is published and promoted like
   any other, carrying a pending notice that says nobody has measured what a
   first-time user meets.
5. Otherwise the newest main commit in the range whose CI and Acceptance push
   runs concluded success, and whose required contexts each appear exactly once
   under push provenance and concluded green, becomes the candidate. A sha still
   being graded is skipped rather than waited for: on a busy night the newest
   sha is always pending, and a selector that waits never converges. A sha that
   is red or ambiguous is named and skipped.

Through GitHub's merge queue the release candidate is the group commit itself,
the exact sha the queue fast-forwards main to. `judge_queue` decides for a group
that is still queued: when the queue's RC Build of it is usable and the group
stages a releasable, untagged version, its archives are preflighted and
preflight.json is filed for it before the queue lands it, and the decision says
what to tell the queue (QUEUE_STATUS_CONTEXT). `bind_merge_group` proves the
group first: a live gh-readonly-queue/main/ ref points at the sha, the base the
ref names is its only parent, and that base is protected main or another live
group. Once landed, a commit is graded on the queue's build of it
(`merge_group_grade`) while main's own push runs have no verdict, and the cut
preflights the queue's archives rather than building the commit again.

Main's own push runs keep guarding main. A green push grade is used as is, and
a red or conflicting one is never overridden by another build of the same tree.
A commit that never went through the queue is graded on its push runs exactly
as before. Inheriting an export pull request head's grade (`resolve_inheritance`)
is built and tested but off (EXPORT_INHERITANCE): pull request heads now carry
stand-in contexts. The decision names the build whose evidence it used.

Only the version main stages is acted on, and never a retired one
(RETIRED_VERSIONS): 0.8.2 and 0.8.3 were cancelled and must never be tagged.

An rc-build attempt the runner's own job timeout killed is a retry, not a spent
attempt: GitHub reports it as `cancelled`, the same word it uses for a person or
a superseding dispatch, so `rc_build_timeout` tells them apart by the job's
annotations, step and timing data. Timeouts have their own budget,
RC_BUILD_TIMEOUT_LIMIT, so a build that can never finish inside its bound does
not re-arm forever.

Every judgment is a pure function over one snapshot document, so the test feeds
it fixtures and asserts the decision with no GitHub in the loop.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any, Callable, Sequence


EXPECTED_REPOSITORY = "kinlab-ai/kin"
BASE_BRANCH = "main"
EVIDENCE_REF = "release-evidence"
PREFLIGHT_RECORD = "preflight.json"
STRANGER_RECORD = "stranger.env"
# The first-contact vocabulary check-release-proof-artifacts.mjs returns in
# `result.stranger.state`, and that release-tag.yml and release.yml both case
# on. It is deliberately the gate's words rather than new ones, so a decision
# this selector emits and a state a workflow read from the gate compare
# directly. STRANGER_PENDING shares its value with the grading verdict PENDING
# and means something else: PENDING is "this sha is still being graded",
# STRANGER_PENDING is "no stranger.env exists for this candidate yet".
STRANGER_COMPLETE = "complete"
STRANGER_PENDING = "pending"
CI_WORKFLOW = ".github/workflows/ci.yml"
CI_WORKFLOW_NAME = "CI"
ACCEPTANCE_WORKFLOW = ".github/workflows/acceptance.yml"
ACCEPTANCE_WORKFLOW_ID = 339594603
SAST_WORKFLOW = ".github/workflows/sast.yml"
SECRET_SCAN_WORKFLOW = ".github/workflows/secret-scan.yml"
RC_BUILD_WORKFLOW = ".github/workflows/rc-build.yml"
RC_BUILD_WORKFLOW_NAME = "RC Build"
PUSH_EVENT = "push"
PULL_REQUEST_EVENT = "pull_request"
# The branch prefix the exporter opens public pull requests from. ci.yml and
# acceptance.yml run the full release suites on a pull_request only for these
# heads, so no other pull request can carry the evidence a release inherits.
EXPORT_BRANCH_PREFIX = "feature/public-export-"
# The event each release producer publishes under on an export pull request's
# head. secret-scan.yml skips its pull_request run for a same-repository branch
# and publishes `gitleaks (full history)` from the branch push instead, so on
# PR 1848 that name appears twice at the head: once skipped under pull_request
# and once green under push. Only the listed event is evidence.
EXPORT_PULL_EVENTS = {
    CI_WORKFLOW: PULL_REQUEST_EVENT,
    ACCEPTANCE_WORKFLOW: PULL_REQUEST_EVENT,
    SAST_WORKFLOW: PULL_REQUEST_EVENT,
    SECRET_SCAN_WORKFLOW: PUSH_EVENT,
}
MERGE_GROUP_EVENT = "merge_group"
# The ref GitHub's merge queue builds a group on: the pull request it carries
# and the base it was built on, which is the group commit's only parent.
QUEUE_REF = re.compile(r"^gh-readonly-queue/main/pr-([0-9]+)-([0-9a-f]{40})$")
# The event each release producer publishes under inside the merge queue. All
# four run on merge_group; secret-scan.yml no longer runs on pushes to other
# branches, so the queue ref carries no push copy of `gitleaks (full history)`.
MERGE_GROUP_EVENTS = {
    CI_WORKFLOW: MERGE_GROUP_EVENT,
    ACCEPTANCE_WORKFLOW: MERGE_GROUP_EVENT,
    SAST_WORKFLOW: MERGE_GROUP_EVENT,
    SECRET_SCAN_WORKFLOW: MERGE_GROUP_EVENT,
}
# Whether a landing outside the merge queue may inherit its export pull
# request head's grade. Off: ci.yml now publishes the release context names on
# a pull_request from stand-in jobs, or from the real job skipped by its `if:`,
# so a green context on a pull request head no longer proves the suite ran, and
# nothing in a check-run tells a stand-in from the real job. The merge queue's
# build of the landing commit is the authority; a landing outside the queue is
# graded on main's own push runs, exactly as before inheritance existed. The
# binding and grading below are kept and tested. Turning this on again needs
# ci.yml to guarantee the real producers on export heads, the census to pin
# them, and EXPORT_PULL_EVENTS re-read from that shape (it records PR 1848's).
EXPORT_INHERITANCE = False
# The Acceptance job's own context, graded on an export head and on a merge
# group. On a main push the job always runs; elsewhere a run of acceptance.yml
# could report success with the graded job itself skipped. release-tag.yml
# vetoes on the same context in every tier.
ACCEPTANCE_CONTEXTS = (("Product Acceptance", ACCEPTANCE_WORKFLOW_ID, ACCEPTANCE_WORKFLOW),)
# Versions that were staged and then cancelled, and must never be tagged. Main
# staged 0.8.2 when 0.8.2 and 0.8.3 were cancelled on 2026-10-12, and a
# release/v0.8.2-candidate branch still points at it, so neither the cut nor the
# mint may act on either version even if main still stages one of them.
RETIRED_VERSIONS = {
    "0.8.2": "cancelled on 2026-10-12 and never published",
    "0.8.3": "cancelled on 2026-10-12 and never published",
}
# The commit status the cut posts on a merge group commit, so the merge queue
# can hold a release candidate until its preflight record is filed and the push
# that lands it can mint at once. It is posted on every group, release or not.
QUEUE_STATUS_CONTEXT = "Release preflight"
# The rc-build.yml jobs that build what a release ships: the build matrix,
# `RC Build (<artifact>)`, and the daemon image job, `RC Daemon Image`. Only one
# of these hitting its own `timeout-minutes` makes a cancelled attempt a retry.
RC_BUILD_JOB_PREFIX = "RC Build ("
RC_BUILD_JOB_PREFIXES = (RC_BUILD_JOB_PREFIX, "RC Daemon Image")
# The artifacts rc-build.yml's build matrix uploads, one archive per row, named
# by `matrix.artifact` alone. These are the bytes release-cut.yml's preflight
# job downloads by that same name, so a run missing one of them cannot feed the
# leg that judges it and is not usable evidence.
#
# These names were once written with a `kin-release-preflight-` prefix, which is
# what release-cut.yml's own preflight job calls the LEG RECORDS it uploads into
# its own run. No rc-build run has ever carried a name of that shape, so no
# rc-build could ever be usable, the decision never reached `proof`, and the
# preflight job that would have produced those records is gated on that very
# decision. The loop armed a fresh build every cycle and burned the candidate at
# RC_BUILD_ATTEMPT_LIMIT. A selector reads the run it is judging for what that
# run produces, never for what a later consumer of it produces.
RC_BUILD_ARTIFACTS = ("kin-linux-aarch64", "kin-linux-x86_64", "kin-macos-aarch64")
# Everything release.yml ships from an RC Build run: the five archives and the
# daemon image. scripts/locate-rc-build.mjs refuses a run missing any of them
# (RC_ARTIFACTS there), so a run is usable only when it still holds all six: a
# record filed for a run release.yml cannot ship from would strand the tag.
RC_RELEASE_ARTIFACTS = (
    "kin-linux-x86_64",
    "kin-linux-aarch64",
    "kin-macos-x86_64",
    "kin-macos-aarch64",
    "kin-windows-x86_64",
    "kin-daemon-image",
)
# One rebuild is a flake allowance. A second failure on the same sha is a
# verdict about the sha, and the next landing supplies a new one.
RC_BUILD_ATTEMPT_LIMIT = 2
# A runner timeout is not a verdict about the sha, so it spends none of the
# attempts above. On 2026-10-12 candidate 08d48c27 died after two macOS arm64
# legs hit their 90-minute bound, with every other leg green. Timeouts still
# have a budget of their own: a build that cannot finish inside its bound three
# times running is a fact about the build, and re-arming it forever is not.
RC_BUILD_TIMEOUT_LIMIT = 3
# The merge queue's bounds. A group has QUEUE_RESPONSE_MINUTES to report every
# required check (ruleset 20050419), and RC Build bounds a merge group's build
# legs at MERGE_GROUP_LEG_MINUTES, with a worst case of MERGE_GROUP_WORST_MINUTES
# across its jobs. A leg that times out has used its whole bound, so a re-run
# cannot also finish inside the window: a timeout in the queue fails the group
# at once and a fresh group rebuilds, while a landed commit's queue build is
# re-run in place under RC_BUILD_TIMEOUT_LIMIT.
QUEUE_RESPONSE_MINUTES = 90
MERGE_GROUP_LEG_MINUTES = 60
MERGE_GROUP_WORST_MINUTES = 75
# A step's timestamps come from the runner's clock and the job's from the
# service's. The killed step on 08d48c27 ended 15 s past the bound on both
# attempts, so a minute of skew is tolerated before the step evidence disagrees.
STEP_CLOCK_SKEW_SECONDS = 60
# The texts GitHub writes as a failure annotation on a cancelled job's check
# run. The timeout text changed shape at least once ("The job running on runner
# GitHub Actions 2 has exceeded the maximum execution time of 360 minutes."
# before, "The job has exceeded the maximum execution time of 1h30m0s" on
# 2026-10-12), and both are read. A text this list does not know reads as not
# a timeout, which is the behaviour from before timeouts were told apart.
TIMEOUT_ANNOTATION = re.compile(
    r"^The job (?:running on runner .+ )?has exceeded the maximum execution time of "
    r"(?P<bound>[0-9hms]+|[0-9]+ minutes?)\.?$"
)
OTHER_CANCEL_ANNOTATIONS = (
    re.compile(r"^The run was canceled by @"),
    re.compile(r"^Canceling since a higher priority waiting request for "),
    re.compile(r"^The strategy configuration was canceled because "),
)
RANGE_LIMIT = 1000
PAGE_SIZE = 100
LOWER_SHA = re.compile(r"^[0-9a-f]{40}$")
VERSION = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
CANDIDATE_BRANCH = re.compile(r"^release/v([0-9]+\.[0-9]+\.[0-9]+)-candidate$")
# Only the GitHub Actions App may write a required context name, pinned by
# numeric id as well as slug so a renamed or impersonating App cannot inherit
# the name. Mirrors release-tag.yml.
GITHUB_ACTIONS_APP = (15368, "github-actions")
# release-tag.yml's REQUIRED_CHECKS with the workflow provenance its mint binds
# each context to. scripts/test-select-release-candidate.py holds this table
# equal to the mint's own, so the selector cannot pick a sha the mint refuses
# for a context it never read.
REQUIRED_CONTEXTS = (
    ("Check & Test (ubuntu-latest)", 245803170, CI_WORKFLOW),
    ("Check & Test (macos-latest)", 245803170, CI_WORKFLOW),
    ("Falsify guards", 245803170, CI_WORKFLOW),
    ("Feature permutation tests (ubuntu-latest)", 245803170, CI_WORKFLOW),
    ("Feature permutation tests (macos-latest)", 245803170, CI_WORKFLOW),
    ("DCO Sign-off", 245803170, CI_WORKFLOW),
    ("cargo-deny", 251549972, ".github/workflows/sast.yml"),
    ("gitleaks (full history)", 293452372, ".github/workflows/secret-scan.yml"),
    ("Windows installer + vector release build", 245803170, CI_WORKFLOW),
)
SKIPPABLE_REQUIRED = frozenset({"DCO Sign-off"})
# The typed manual kick and who may send it, mirroring release-tag.yml's
# break-glass shape: a repository_dispatch always runs this workflow from the
# default branch, and the actor is pinned to the captain and the release App.
KICK_ACTION = "release_cut"
KICK_ACTORS = frozenset({"troyjr4103", "kin-release-bot[bot]"})
KICK_REASON_LIMIT = 200
TRIGGER_KICK = "kick"
TRIGGER_CI = "ci-completed"
TRIGGER_RC_BUILD = "rc-build-completed"
TRIGGER_PUSH = "main-push"
TRIGGER_MERGE_GROUP = "merge-group"
TRIGGER_SWEEP = "sweep"
EVIDENCE_PUSH = "push"
EVIDENCE_EXPORT = "export-pull-request"
EVIDENCE_MERGE_GROUP = "merge-group"
KIND_PUSH = "push"
KIND_EXPORT = "export"
KIND_MERGE_GROUP = "merge_group"

STAND_DOWN = "stand-down"
ARM = "arm"
PROOF = "proof"
# Re-run the failed jobs of an RC Build whose build job hit its own timeout,
# rather than dispatching a new build: a merge group's build cannot be
# dispatched again, and its re-run keeps the run, the sha and the artifacts of
# the legs that already passed.
RETRY = "retry"
STRANGER = "stranger"
REFUSE = "refuse"
GREEN = "green"
PENDING = "pending"
DEAD = "dead"
MOVE_CREATE = "create"
MOVE_FAST_FORWARD = "fast-forward"
MOVE_RESET = "reset"
MOVE_NONE = "none"
# The states the cut posts as QUEUE_STATUS_CONTEXT. `proof` is not a status:
# it means the decision is to preflight, and the workflow posts success or
# failure once the publish job has concluded. Empty means post nothing yet.
QUEUE_SUCCESS = "success"
QUEUE_FAILURE = "failure"
QUEUE_PROOF = "proof"
# Every key `_emit` writes to GITHUB_OUTPUT, in order. release-cut.yml's select
# job republishes these as job outputs, and scripts/test-select-release-candidate.py
# holds the two lists equal, so a value this selector decides cannot go missing
# between the step that decided it and the jobs that read it. `reason` is written
# separately because its newlines have to be flattened first.
OUTPUT_KEYS = (
    "decision",
    "version",
    "branch",
    "candidate",
    "rc_run",
    "move",
    "stranger_state",
    "queue_sha",
    "queue_state",
)


class SelectionError(RuntimeError):
    """The candidate could not be selected on a trustworthy reading."""


class NotFound(SelectionError):
    """GitHub answered 404: the one answer a caller may act on differently."""


Fetch = Callable[[str], Any]
Grader = Callable[[str], dict[str, Any]]


# ---------------------------------------------------------------------------
# Endpoints. Named here so the live gather and the test agree byte for byte.
# ---------------------------------------------------------------------------


def candidate_branch(version: str) -> str:
    return f"release/v{version}-candidate"


def version_refusal(version: str) -> str | None:
    """Why `version` may never be cut or minted, or None when it may be."""

    if not isinstance(version, str) or VERSION.fullmatch(version) is None:
        return f"{version!r} is not a semver-shaped version"
    retired = RETIRED_VERSIONS.get(version)
    if retired is not None:
        return f"v{version} is retired and will never be tagged ({retired})"
    return None


def tag_endpoint(repository: str, version: str) -> str:
    return f"repos/{repository}/git/ref/tags/v{version}"


def evidence_tree_endpoint(repository: str) -> str:
    return f"repos/{repository}/git/trees/{EVIDENCE_REF}?recursive=1"


def branch_endpoint(repository: str, branch: str) -> str:
    return f"repos/{repository}/git/ref/heads/{branch}"


def rc_build_runs_endpoint(repository: str, branch: str, page: int = 1) -> str:
    return (
        f"repos/{repository}/actions/workflows/rc-build.yml/runs"
        f"?branch={branch}&event=workflow_dispatch&per_page={PAGE_SIZE}&page={page}"
    )


def merge_group_rc_build_runs_endpoint(repository: str, sha: str, page: int = 1) -> str:
    return (
        f"repos/{repository}/actions/workflows/rc-build.yml/runs"
        f"?head_sha={sha}&event={MERGE_GROUP_EVENT}&per_page={PAGE_SIZE}&page={page}"
    )


def queue_refs_endpoint(repository: str) -> str:
    return f"repos/{repository}/git/matching-refs/heads/gh-readonly-queue/{BASE_BRANCH}/"


def run_artifacts_endpoint(repository: str, run_id: int, page: int = 1) -> str:
    return f"repos/{repository}/actions/runs/{run_id}/artifacts?per_page={PAGE_SIZE}&page={page}"


def check_runs_endpoint(repository: str, sha: str, page: int = 1) -> str:
    # `filter=all` is deliberate: a rerun that claims one required name is
    # ambiguous authority, not evidence to collapse by recency.
    return f"repos/{repository}/commits/{sha}/check-runs?per_page={PAGE_SIZE}&filter=all&page={page}"


def workflow_runs_endpoint(repository: str, sha: str, page: int = 1) -> str:
    return f"repos/{repository}/actions/runs?head_sha={sha}&per_page={PAGE_SIZE}&page={page}"


def run_jobs_endpoint(repository: str, run_id: int, page: int = 1) -> str:
    # The latest attempt's jobs, which are the ones the run's conclusion reports.
    return f"repos/{repository}/actions/runs/{run_id}/jobs?filter=latest&per_page={PAGE_SIZE}&page={page}"


def annotations_endpoint(repository: str, check_run_id: int) -> str:
    return f"repos/{repository}/check-runs/{check_run_id}/annotations?per_page={PAGE_SIZE}"


def commit_pulls_endpoint(repository: str, sha: str) -> str:
    return f"repos/{repository}/commits/{sha}/pulls?per_page={PAGE_SIZE}"


def pull_endpoint(repository: str, number: int) -> str:
    return f"repos/{repository}/pulls/{number}"


def pull_head_ref(number: int) -> str:
    """Where an inheritance read parks a pull request's head in the workspace."""

    return f"refs/kin-inherited/pull-{number}"


# ---------------------------------------------------------------------------
# Trigger validation, from GitHub-owned context only.
# ---------------------------------------------------------------------------


def _sha(value: Any) -> str | None:
    if isinstance(value, str) and LOWER_SHA.fullmatch(value):
        return value
    return None


def validate_trigger(
    *,
    event_name: str,
    event_action: str,
    actor: str,
    repository: str,
    default_branch: str,
    ref: str,
    workflow_sha: str,
    event: Any,
) -> dict[str, Any]:
    """Admit one of five trigger shapes, every one resolved from protected main.

    A push to main, a CI completion for a main push and an RC Build completion
    on a candidate branch are the occasions that move the cut along; the typed
    kick is the manual arm GitHub's cron cannot stall; the schedule is the
    fallback. The push is what lets a landing that inherits its export pull
    request's evidence arm at once instead of waiting out main's own CI. The
    event only decides when to look. Which sha is released is re-derived from
    scratch by `gather` and `judge`, never read off the event.
    """

    if repository != EXPECTED_REPOSITORY:
        raise SelectionError(f"repository must be {EXPECTED_REPOSITORY!r}, got {repository!r}")
    if default_branch != BASE_BRANCH:
        raise SelectionError(f"default branch must be {BASE_BRANCH!r}, got {default_branch!r}")
    if ref != f"refs/heads/{BASE_BRANCH}":
        raise SelectionError(f"workflow ref must be refs/heads/{BASE_BRANCH}, got {ref!r}")
    if _sha(workflow_sha) is None:
        raise SelectionError(f"workflow sha must be 40-character lowercase hex, got {workflow_sha!r}")
    if not isinstance(event, dict):
        raise SelectionError("event document must be an object")
    if event_name == "repository_dispatch":
        if event_action != KICK_ACTION:
            raise SelectionError(f"event action must be {KICK_ACTION!r}, got {event_action!r}")
        if event.get("action") != event_action:
            raise SelectionError("event document action differs from the trigger context")
        if actor not in KICK_ACTORS:
            raise SelectionError(f"actor {actor!r} may not kick the release cut")
        payload = event.get("client_payload")
        if payload is None:
            payload = {}
        if not isinstance(payload, dict) or set(payload) - {"reason"}:
            raise SelectionError("kick payload may carry a reason and nothing else")
        reason = payload.get("reason", "")
        if not isinstance(reason, str) or len(reason) > KICK_REASON_LIMIT:
            raise SelectionError("kick reason must be text of at most 200 characters")
        return {"trigger": TRIGGER_KICK, "actor": actor, "reason": reason}
    if event_name == "workflow_run":
        if event_action != "completed":
            raise SelectionError(f"workflow_run action must be 'completed', got {event_action!r}")
        run = event.get("workflow_run")
        if not isinstance(run, dict):
            raise SelectionError("workflow_run event omitted its run")
        head_repository = run.get("head_repository")
        if (
            not isinstance(head_repository, dict)
            or head_repository.get("full_name") != repository
        ):
            raise SelectionError("workflow_run head repository is not first-party")
        if run.get("status") != "completed":
            raise SelectionError("workflow_run is not completed")
        head_branch = run.get("head_branch")
        if run.get("event") == MERGE_GROUP_EVENT and run.get("path") in (CI_WORKFLOW, RC_BUILD_WORKFLOW):
            # The queue's build of the commit that will land: an RC Build there
            # is the occasion to preflight it inside the queue window, and a CI
            # completion is the occasion to tell the queue a group that stages
            # no release is not waiting on one.
            if not (isinstance(head_branch, str) and QUEUE_REF.fullmatch(head_branch)):
                raise SelectionError(
                    "a merge_group completion is an occasion only on a gh-readonly-queue/main/ "
                    f"ref, got {head_branch!r}"
                )
            if _sha(run.get("head_sha")) is None:
                raise SelectionError("a merge_group completion names no exact head sha")
            return {
                "trigger": TRIGGER_MERGE_GROUP,
                "workflow": run.get("path"),
                "conclusion": run.get("conclusion"),
                "head_sha": run.get("head_sha"),
                "head_branch": head_branch,
                "run_id": run.get("id"),
            }
        if run.get("path") == CI_WORKFLOW:
            if run.get("event") != "push" or head_branch != BASE_BRANCH:
                raise SelectionError(
                    f"a CI completion is an occasion only for a push to {BASE_BRANCH}, "
                    f"got {run.get('event')!r} on {head_branch!r}"
                )
            return {
                "trigger": TRIGGER_CI,
                "conclusion": run.get("conclusion"),
                "head_sha": run.get("head_sha"),
            }
        if run.get("path") == RC_BUILD_WORKFLOW:
            if run.get("event") != "workflow_dispatch" or not (
                isinstance(head_branch, str) and CANDIDATE_BRANCH.fullmatch(head_branch)
            ):
                raise SelectionError(
                    "an RC Build completion is an occasion only for a dispatch on a "
                    f"release/v<version>-candidate branch, got {run.get('event')!r} on "
                    f"{head_branch!r}"
                )
            return {
                "trigger": TRIGGER_RC_BUILD,
                "conclusion": run.get("conclusion"),
                "head_sha": run.get("head_sha"),
                "head_branch": head_branch,
                "run_id": run.get("id"),
            }
        raise SelectionError(
            f"workflow_run path {run.get('path')!r} is neither {CI_WORKFLOW} nor {RC_BUILD_WORKFLOW}"
        )
    if event_name == "push":
        # The ref check above already pins a push run to protected main, since
        # a push run's GITHUB_REF is the branch pushed. The event document has
        # to agree with that context rather than merely be present.
        if event_action != "":
            raise SelectionError(f"push event action must be empty, got {event_action!r}")
        if event.get("ref") != f"refs/heads/{BASE_BRANCH}":
            raise SelectionError(
                f"a push is an occasion only on refs/heads/{BASE_BRANCH}, got {event.get('ref')!r}"
            )
        pushed = event.get("repository")
        if not isinstance(pushed, dict) or pushed.get("full_name") != repository:
            raise SelectionError("push event repository is not first-party")
        if event.get("deleted") is True or _sha(event.get("after")) != workflow_sha:
            raise SelectionError("push event head differs from the workflow sha")
        return {"trigger": TRIGGER_PUSH, "head_sha": workflow_sha}
    if event_name == "schedule":
        if event_action != "":
            raise SelectionError(f"scheduled event action must be empty, got {event_action!r}")
        return {"trigger": TRIGGER_SWEEP}
    raise SelectionError(
        f"event name must be workflow_run, repository_dispatch, push or schedule, got {event_name!r}"
    )


# ---------------------------------------------------------------------------
# GitHub reads.
# ---------------------------------------------------------------------------


def gh_json(endpoint: str) -> Any:
    result = subprocess.run(
        ["gh", "api", endpoint],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or "no output"
        if "HTTP 404" in detail:
            raise NotFound(f"gh api {endpoint}: {detail}")
        raise SelectionError(f"gh api {endpoint} failed: {detail}")
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise SelectionError(f"GitHub returned malformed JSON for {endpoint}") from exc


def _count(value: Any, label: str) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise SelectionError(f"{label} is not a count: {value!r}")
    return value


def flatten_pages(pages: Any, key: str) -> list[dict[str, Any]]:
    """Flatten a paged listing and refuse one that does not add up.

    The endpoints page at 100 and fail toward green: a page that arrives short
    reads exactly like a sha with fewer checks. Every page carries the same
    `total_count`, so the listed length is asserted against it.
    """

    if not isinstance(pages, list) or not pages:
        raise SelectionError(f"{key} listing has no pages")
    items: list[dict[str, Any]] = []
    totals: set[int] = set()
    for page in pages:
        if not isinstance(page, dict):
            raise SelectionError(f"{key} page is not an object")
        totals.add(_count(page.get("total_count"), f"{key} total_count"))
        chunk = page.get(key)
        if not isinstance(chunk, list):
            raise SelectionError(f"{key} page carries no {key} list")
        for item in chunk:
            if not isinstance(item, dict):
                raise SelectionError(f"{key} entry is not an object")
            items.append(item)
    if len(totals) != 1:
        raise SelectionError(
            f"{key} listing changed under the read: total_count {sorted(totals)}"
        )
    total = totals.pop()
    if len(items) != total:
        raise SelectionError(f"{key} listing is incomplete: {len(items)} listed of {total}")
    return items


def read_listing(fetch: Fetch, endpoint_for_page: Callable[[int], str], key: str) -> list[dict[str, Any]]:
    first = fetch(endpoint_for_page(1))
    if not isinstance(first, dict):
        raise SelectionError(f"{key} listing is not an object")
    total = _count(first.get("total_count"), f"{key} total_count")
    pages = [first]
    for page in range(2, max(1, math.ceil(total / PAGE_SIZE)) + 1):
        pages.append(fetch(endpoint_for_page(page)))
    return flatten_pages(pages, key)


def read_tag_exists(fetch: Fetch, repository: str, version: str) -> bool:
    try:
        document = fetch(tag_endpoint(repository, version))
    except NotFound:
        return False
    ref_object = document.get("object") if isinstance(document, dict) else None
    if not isinstance(ref_object, dict) or _sha(ref_object.get("sha")) is None:
        raise SelectionError(f"tag v{version} read back without an exact object sha")
    return True


def read_evidence(fetch: Fetch, repository: str) -> dict[str, list[str]]:
    """Which candidates the proof loop recorded, the way the mint reads them.

    An unreadable listing is not an empty one, and a truncated listing could
    hide a newer proven candidate, so both refuse rather than answer.
    """

    listing = fetch(evidence_tree_endpoint(repository))
    if not isinstance(listing, dict) or not isinstance(listing.get("tree"), list):
        raise SelectionError(
            f"the {EVIDENCE_REF} listing is not a git tree object, so the proof "
            "loop's records cannot be read"
        )
    if listing.get("truncated") is True:
        raise SelectionError(
            f"the {EVIDENCE_REF} listing is truncated, so a newer proven candidate "
            "could be invisible"
        )
    pattern = re.compile(r"^evidence/([0-9a-f]{40})/(preflight\.json|stranger\.env)$")
    evidence: dict[str, list[str]] = {}
    for entry in listing["tree"]:
        if not isinstance(entry, dict) or entry.get("type") != "blob":
            continue
        match = pattern.fullmatch(str(entry.get("path", "")))
        if match is None:
            continue
        records = evidence.setdefault(match.group(1), [])
        if match.group(2) not in records:
            records.append(match.group(2))
    return evidence


def read_branch(fetch: Fetch, repository: str, branch: str) -> str | None:
    try:
        document = fetch(branch_endpoint(repository, branch))
    except NotFound:
        return None
    ref_object = document.get("object") if isinstance(document, dict) else None
    sha = _sha(ref_object.get("sha")) if isinstance(ref_object, dict) else None
    if sha is None:
        raise SelectionError(f"branch {branch} read back without an exact object sha")
    return sha


def read_rc_builds(fetch: Fetch, repository: str, branch: str) -> list[dict[str, Any]]:
    """Every rc-build dispatch on the candidate branch, newest first.

    A completed successful run also carries the names of its unexpired
    artifacts, which is how a run still holding the candidate archives is told
    apart from one whose archives have aged out of the artifact store. A
    cancelled run also carries its latest attempt's jobs, each cancelled job
    with the annotations GitHub wrote on it, which is how a runner timeout is
    told apart from a person or a superseding dispatch (`rc_build_timeout`).
    """

    runs = read_listing(
        fetch, lambda page: rc_build_runs_endpoint(repository, branch, page), "workflow_runs"
    )
    builds = [rc_build_record(fetch, repository, run) for run in runs]
    builds.sort(key=lambda build: (build["created_at"], build["id"]), reverse=True)
    return builds


def read_merge_group_rc_builds(fetch: Fetch, repository: str, sha: str) -> list[dict[str, Any]]:
    """Every RC Build the merge queue ran on group commit `sha`, newest first."""

    runs = read_listing(
        fetch, lambda page: merge_group_rc_build_runs_endpoint(repository, sha, page), "workflow_runs"
    )
    builds = [
        rc_build_record(fetch, repository, run)
        for run in runs
        if run.get("head_sha") == sha and run.get("event") == MERGE_GROUP_EVENT
    ]
    builds.sort(key=lambda build: (build["created_at"], build["id"]), reverse=True)
    return builds


def rc_build_record(fetch: Fetch, repository: str, run: dict[str, Any]) -> dict[str, Any]:
    run_id = _count(run.get("id"), "rc-build run id")
    record = {
        "id": run_id,
        "head_sha": run.get("head_sha"),
        "head_branch": run.get("head_branch"),
        "event": run.get("event"),
        "status": run.get("status"),
        "conclusion": run.get("conclusion"),
        "created_at": run.get("created_at") or "",
        "run_attempt": run.get("run_attempt") or 1,
        "artifacts": [],
    }
    if record["status"] == "completed" and record["conclusion"] == "success":
        artifacts = read_listing(
            fetch, lambda page: run_artifacts_endpoint(repository, run_id, page), "artifacts"
        )
        record["artifacts"] = sorted(
            str(artifact.get("name")) for artifact in artifacts if not artifact.get("expired")
        )
    if record["status"] == "completed" and record["conclusion"] == "cancelled":
        record["jobs"] = read_jobs(fetch, repository, run_id)
    return record


def read_annotations(fetch: Fetch, repository: str, check_run_id: Any) -> list[dict[str, Any]]:
    listing = fetch(annotations_endpoint(repository, _count(check_run_id, "check-run id")))
    if not isinstance(listing, list):
        raise SelectionError(f"annotations for check-run {check_run_id} are not a list")
    if len(listing) >= PAGE_SIZE:
        raise SelectionError(
            f"check-run {check_run_id} carries {len(listing)} or more annotations, "
            "so the one naming why it was cancelled could be on a page not read"
        )
    return [
        {"annotation_level": item.get("annotation_level"), "message": item.get("message")}
        for item in listing
        if isinstance(item, dict)
    ]


def read_jobs(fetch: Fetch, repository: str, run_id: int) -> list[dict[str, Any]]:
    """A run's latest-attempt jobs, a cancelled job with its steps and annotations."""

    jobs = []
    for job in read_listing(fetch, lambda page: run_jobs_endpoint(repository, run_id, page), "jobs"):
        record = {
            "id": job.get("id"),
            "name": job.get("name"),
            "status": job.get("status"),
            "conclusion": job.get("conclusion"),
            "started_at": job.get("started_at"),
            "completed_at": job.get("completed_at"),
            "steps": [],
            "annotations": [],
        }
        if record["conclusion"] == "cancelled":
            record["steps"] = [
                {
                    "name": step.get("name"),
                    "status": step.get("status"),
                    "conclusion": step.get("conclusion"),
                    "started_at": step.get("started_at"),
                    "completed_at": step.get("completed_at"),
                }
                for step in job.get("steps") or []
                if isinstance(step, dict)
            ]
            record["annotations"] = read_annotations(fetch, repository, record["id"])
        jobs.append(record)
    return jobs


def run_git(workspace: Path, *arguments: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(workspace), *arguments],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip() or "no output"
        raise SelectionError(f"git {' '.join(arguments)} failed: {detail}")
    return result.stdout.strip()


def workspace_version(cargo_toml: str) -> str:
    try:
        version = tomllib.loads(cargo_toml)["workspace"]["package"]["version"]
    except (tomllib.TOMLDecodeError, KeyError, TypeError) as exc:
        raise SelectionError(f"Cargo.toml carries no [workspace.package] version: {exc}") from exc
    if not isinstance(version, str) or VERSION.fullmatch(version) is None:
        raise SelectionError(f"workspace version {version!r} is not semver-shaped")
    return version


def version_range(workspace: Path, tip: str, version: str) -> list[str]:
    """The reviewed main commits carrying `version`, newest first.

    The floor is the squash of the version bump: the oldest first-parent commit
    whose Cargo.toml carries this version. Nothing before it can be this
    release, because nothing before it carries the version the tag names.
    """

    log = run_git(workspace, "log", "--first-parent", f"--max-count={RANGE_LIMIT}", "--format=%H", tip)
    shas = []
    for sha in log.splitlines():
        sha = sha.strip()
        if _sha(sha) is None:
            raise SelectionError(f"git log produced a non-sha line {sha!r}")
        if workspace_version(run_git(workspace, "show", f"{sha}:Cargo.toml")) != version:
            break
        shas.append(sha)
    if not shas:
        raise SelectionError(f"the tip {tip} does not carry version {version}")
    return shas


# ---------------------------------------------------------------------------
# Grading one sha. Pure over the two listings the mint reads.
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Provenance:
    """Which runs at which sha are a grade's evidence.

    The push grade reads main's own push runs at the commit, as it always has.
    An export grade reads the runs on an export pull request's head, and a
    merge-group grade the queue's build of the very commit that lands, each
    under the event every producer publishes there (EXPORT_PULL_EVENTS,
    MERGE_GROUP_EVENTS), and both additionally require the Acceptance job's own
    context. Every other rule is shared.
    """

    sha: str
    branch: str = BASE_BRANCH
    kind: str = KIND_PUSH

    @property
    def name(self) -> str:
        return {KIND_EXPORT: "export pull request", KIND_MERGE_GROUP: "merge group"}.get(self.kind, "push")

    def _events(self) -> dict[str, str]:
        return {KIND_EXPORT: EXPORT_PULL_EVENTS, KIND_MERGE_GROUP: MERGE_GROUP_EVENTS}.get(self.kind, {})

    def event_for(self, path: str) -> str:
        default = {KIND_EXPORT: PULL_REQUEST_EVENT, KIND_MERGE_GROUP: MERGE_GROUP_EVENT}.get(self.kind, PUSH_EVENT)
        return self._events().get(path, default)

    def produced(self, run: dict[str, Any], path: str) -> bool:
        """`run` is the build this provenance admits for workflow `path`."""

        return (
            run.get("path") == path
            and run.get("event") == self.event_for(path)
            and run.get("head_branch") == self.branch
            and run.get("head_sha") == self.sha
        )

    def on_ref(self, run: dict[str, Any]) -> bool:
        """`run` sits on the ref this grade is proved from, whatever produced it."""

        events = set(self._events().values()) or {PUSH_EVENT}
        return (
            run.get("event") in events
            and run.get("head_branch") == self.branch
            and run.get("head_sha") == self.sha
        )

    def contexts(self) -> tuple[tuple[str, int, str], ...]:
        return REQUIRED_CONTEXTS + (ACCEPTANCE_CONTEXTS if self.kind != KIND_PUSH else ())


# A required context that completed with one of these is not a failure. The
# mint judges the build it does not key on by the same set.
NON_FAILING = frozenset({"success", "skipped", "neutral"})


def grade_sha(
    sha: str,
    check_run_pages: Any,
    workflow_run_pages: Any,
    provenance: Provenance | None = None,
) -> dict[str, Any]:
    """Decide whether one sha is green, pending or dead for the mint.

    Green means the CI and Acceptance push runs on main concluded success and
    every required context appears exactly once under push provenance at this
    sha, by the GitHub Actions App, concluded success (DCO Sign-off may report
    skipped on a merged commit). Pending means a producing run has not finished.
    Everything else is dead, and a rerun is the reason that matters most: the
    mint refuses a required context that appears more than once as ambiguous
    authority, whatever the copies concluded.

    `provenance` grades an export pull request's head by the same rules instead
    (see Provenance). Besides the verdict the grade names what is `red`, any
    admitted attempt or producing run that completed without passing, and what
    `conflicts`, a second writer or App claiming a required name. Those two are
    what inheritance may never stand in front of.
    """

    if _sha(sha) is None:
        raise SelectionError(f"cannot grade {sha!r}: not a 40-character lowercase sha")
    provenance = provenance or Provenance(sha)
    if provenance.sha != sha:
        raise SelectionError(f"cannot grade {sha} on the provenance of {provenance.sha}")
    check_runs = flatten_pages(check_run_pages, "check_runs")
    workflow_runs = flatten_pages(workflow_run_pages, "workflow_runs")
    dead: list[str] = []
    pending: list[str] = []
    retries: list[str] = []
    red: list[str] = []
    conflicts: list[str] = []
    producing: dict[str, int] = {}

    by_suite: dict[Any, list[dict[str, Any]]] = {}
    for run in workflow_runs:
        by_suite.setdefault(run.get("check_suite_id"), []).append(run)

    def produced_runs(path: str) -> list[dict[str, Any]]:
        return [run for run in workflow_runs if provenance.produced(run, path)]

    for label, path in (("CI", CI_WORKFLOW), ("Acceptance", ACCEPTANCE_WORKFLOW)):
        event = provenance.event_for(path)
        runs = produced_runs(path)
        if not runs:
            pending.append(f"no {label} {event} run on {provenance.branch} at this sha yet")
        elif len(runs) > 1:
            reason = f"{len(runs)} {label} {event} runs at this sha: ambiguous authority"
            dead.append(reason)
            conflicts.append(reason)
        else:
            run = runs[0]
            producing[label] = run.get("id")
            if run.get("status") != "completed":
                pending.append(f"{label} {event} run {run.get('id')} is {run.get('status')}")
            elif run.get("conclusion") != "success":
                reason = f"{label} {event} run {run.get('id')} concluded {run.get('conclusion')}"
                dead.append(reason)
                if run.get("conclusion") not in NON_FAILING | {"cancelled"}:
                    red.append(reason)

    by_name: dict[Any, list[dict[str, Any]]] = {}
    for check in check_runs:
        by_name.setdefault(check.get("name"), []).append(check)

    for name, workflow_id, path in provenance.contexts():
        admitted = []
        for check in by_name.get(name, []):
            app = check.get("app") if isinstance(check.get("app"), dict) else {}
            if (app.get("id"), app.get("slug")) != GITHUB_ACTIONS_APP:
                reason = f"required check name claimed by another app: {name} ({app.get('slug')!r})"
                dead.append(reason)
                conflicts.append(reason)
                continue
            suite = check.get("check_suite") if isinstance(check.get("check_suite"), dict) else {}
            producers = by_suite.get(suite.get("id"), [])
            if len(producers) != 1:
                reason = (
                    f"required check has ambiguous workflow provenance: {name} "
                    f"(suite={suite.get('id')}, workflow-runs={len(producers)})"
                )
                dead.append(reason)
                conflicts.append(reason)
                continue
            producer = producers[0]
            live = producer.get("conclusion") != "cancelled"
            if (producer.get("workflow_id"), producer.get("path")) != (workflow_id, path):
                if provenance.on_ref(producer) and live:
                    # A second writer of a required context name on the ref this
                    # release is proved from is an authority conflict, never a
                    # fallback. Off that ref it is somebody else's build at the
                    # same sha and is discounted.
                    reason = (
                        f"required check workflow provenance mismatch: {name} "
                        f"produced by {producer.get('path')} run {producer.get('id')}"
                    )
                    dead.append(reason)
                    conflicts.append(reason)
                continue
            if provenance.produced(producer, path) and live:
                admitted.append(check)
        for check in admitted:
            if check.get("status") == "completed" and check.get("conclusion") not in NON_FAILING:
                red.append(
                    f"required check attempt not green under {provenance.name} provenance: "
                    f"{name} (conclusion={check.get('conclusion')})"
                )
        if not admitted:
            producers_running = any(
                run.get("status") != "completed" for run in produced_runs(path)
            )
            (pending if producers_running else dead).append(
                f"missing required check: {name} "
                f"({len(by_name.get(name, []))} same-named check-runs, none under "
                f"{provenance.name} provenance)"
            )
            continue
        if len(admitted) == 2:
            retried = settled_retry(admitted)
            if retried is not None:
                admitted = [retried]
                retries.append(name)
        if len(admitted) != 1:
            dead.append(
                f"ambiguous required check: {name} ({len(admitted)} check-runs under one "
                "provenance; only one retry of a failed or timed-out attempt is admitted)"
            )
            continue
        check = admitted[0]
        if check.get("status") != "completed":
            pending.append(f"required check not completed: {name} ({check.get('status')})")
            continue
        allowed = {"success", "skipped"} if name in SKIPPABLE_REQUIRED else {"success"}
        if check.get("conclusion") not in allowed:
            dead.append(f"required check not green: {name} (conclusion={check.get('conclusion')})")

    verdict = DEAD if dead else (PENDING if pending else GREEN)
    return {
        "sha": sha,
        "verdict": verdict,
        "dead": dead,
        "pending": pending,
        "retried": retries,
        "red": red,
        "conflicts": conflicts,
        "runs": producing,
    }


def settled_retry(checks: list[dict[str, Any]]) -> dict[str, Any] | None:
    """Return the passing attempt when two check-runs are one job retried once.

    A job rerun publishes a second check-run under the same name in the same
    check suite. Before this, any second copy made the sha unmintable, so one
    flaky attempt on the single public main commit of a release cost a whole
    export cycle. The retry is admitted only in its narrowest shape: both
    attempts belong to one workflow run, the first completed as a failure or as
    a job that hit its own timeout, and the second started after it and passed.
    Anything else, a third attempt, a pass followed by a fail, a first attempt
    a person or a superseding run cancelled, or two copies from different runs,
    stays ambiguous. release-tag.yml applies the same rule, and the retried
    contexts are reported so a flaky job stays visible.

    GitHub reports a job that reached its `timeout-minutes` as `cancelled`, not
    `timed_out`, so the timeout half of this rule read nothing until the first
    attempt's own annotations and timing were consulted (`hit_its_own_timeout`).
    """

    first, second = sorted(checks, key=lambda check: str(check.get("started_at") or ""))
    suite = lambda check: (check.get("check_suite") or {}).get("id")
    if suite(first) is None or suite(first) != suite(second):
        return None
    if first.get("status") != "completed":
        return None
    if first.get("conclusion") not in {"failure", "timed_out"} and not (
        first.get("conclusion") == "cancelled" and hit_its_own_timeout(first) is not None
    ):
        return None
    if second.get("status") != "completed" or second.get("conclusion") != "success":
        return None
    finished, restarted = first.get("completed_at"), second.get("started_at")
    if not finished or not restarted or str(restarted) < str(finished):
        return None
    return second


# ---------------------------------------------------------------------------
# Telling a runner timeout from every other cancel.
# ---------------------------------------------------------------------------


def _instant(value: Any) -> datetime | None:
    if not isinstance(value, str) or not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def _bound_seconds(text: str) -> int | None:
    minutes = re.fullmatch(r"([0-9]+) minutes?", text)
    if minutes:
        return int(minutes.group(1)) * 60
    parts = re.fullmatch(r"(?:([0-9]+)h)?(?:([0-9]+)m)?(?:([0-9]+)s)?", text)
    if parts is None or not any(parts.groups()):
        return None
    hours, mins, secs = (int(part or 0) for part in parts.groups())
    return hours * 3600 + mins * 60 + secs


def _duration(seconds: float) -> str:
    seconds = int(seconds)
    return f"{seconds // 3600}h{seconds % 3600 // 60}m{seconds % 60}s"


def hit_its_own_timeout(item: dict[str, Any]) -> str | None:
    """Say how a cancelled job or check-run hit its own timeout, or return None.

    GitHub concludes a job that reached `timeout-minutes` as `cancelled`, the
    same word it uses when a person cancels the run or a newer run in the same
    concurrency group supersedes it, so the conclusion alone tells them apart
    from nothing. Three readings do, and all three are required:

      * exactly one failure annotation is GitHub's timeout text, which names
        the bound, and none is GitHub's text for a manual, superseding or
        fail-fast cancel;
      * the job ran at least that long, by the service's own start and finish
        times, so a timeout text on a job cancelled early proves nothing;
      * when the steps are known (the jobs API carries them, the check-runs API
        does not), exactly one step was cancelled, and it ended at or after the
        bound, give or take the runner's clock.

    Each reading fails toward "not a timeout", which is how every cancel was
    counted before. Both attempts the cut spent on 08d48c27 on 2026-10-12 read
    as timeouts; the concurrency and manual cancels of RC Build in the same
    fixture do not.
    """

    if item.get("status") != "completed" or item.get("conclusion") != "cancelled":
        return None
    failures = [
        str(annotation.get("message") or "")
        for annotation in item.get("annotations") or []
        if isinstance(annotation, dict) and annotation.get("annotation_level") == "failure"
    ]
    if any(pattern.search(message) for message in failures for pattern in OTHER_CANCEL_ANNOTATIONS):
        return None
    bounds = [match.group("bound") for match in map(TIMEOUT_ANNOTATION.fullmatch, failures) if match]
    if len(bounds) != 1:
        return None
    bound = _bound_seconds(bounds[0])
    started, finished = _instant(item.get("started_at")), _instant(item.get("completed_at"))
    if bound is None or started is None or finished is None:
        return None
    elapsed = (finished - started).total_seconds()
    if elapsed < bound:
        return None
    steps = item.get("steps")
    if steps:
        cancelled = [step for step in steps if isinstance(step, dict) and step.get("conclusion") == "cancelled"]
        if len(cancelled) != 1:
            return None
        ended = _instant(cancelled[0].get("completed_at"))
        if ended is None or (ended - started).total_seconds() < bound - STEP_CLOCK_SKEW_SECONDS:
            return None
    return f"{item.get('name')} hit its {bounds[0]} bound after {_duration(elapsed)}"


def rc_build_timeout(build: dict[str, Any]) -> str | None:
    """Say how a cancelled rc-build attempt was a runner timeout, or return None.

    The narrow shape: the run concluded cancelled, at least one build matrix
    job hit its own timeout, and every other job passed or was skipped behind
    it. A job that failed is a verdict, and any cancel that is not a timeout is
    a person or a superseding dispatch, so either keeps the attempt spent.
    """

    if build.get("status") != "completed" or build.get("conclusion") != "cancelled":
        return None
    jobs = build.get("jobs")
    if not isinstance(jobs, list) or not jobs:
        return None
    timed_out = []
    for job in jobs:
        if not isinstance(job, dict):
            return None
        if job.get("conclusion") in {"success", "skipped"}:
            continue
        if not str(job.get("name") or "").startswith(RC_BUILD_JOB_PREFIXES):
            return None
        reason = hit_its_own_timeout(job)
        if reason is None:
            return None
        timed_out.append(reason)
    return "; ".join(timed_out) or None


# ---------------------------------------------------------------------------
# Evidence a squash inherits from the export pull request it landed from.
# ---------------------------------------------------------------------------


def bind_export_pull(
    sha: str, document: dict[str, Any], repository: str = EXPECTED_REPOSITORY
) -> tuple[dict[str, Any] | None, list[str]]:
    """Bind `sha` to the export pull request head whose grade it may inherit.

    Returns the binding, or None with every reason it does not hold. The
    commit's own tree and parents come from protected history, the head's tree
    and ancestry from git after fetching the head, and only the pull request's
    identity from the pulls API.
    """

    if not isinstance(document, dict) or document.get("sha") != sha:
        return None, [f"the inheritance read is not about {sha}"]
    associated = [
        pull.get("number")
        for pull in document.get("associated_pulls") or []
        if isinstance(pull, dict) and pull.get("merge_commit_sha") == sha
    ]
    if len(associated) > 1:
        return None, [f"{len(associated)} pull requests name {sha} as their merge commit"]
    # The association listing is index-backed and can still be empty seconds
    # after a merge, which is exactly when the landing push wakes the cut. The
    # squash's own subject, read from protected history, then locates the pull
    # request instead. Either way only the pull request itself, below, proves
    # it merged as this commit.
    number = associated[0] if associated else document.get("subject_pull")
    if not isinstance(number, int) or isinstance(number, bool):
        return None, [f"no pull request names {sha} as its merge commit"]
    pull = document.get("pull")
    if not isinstance(pull, dict) or pull.get("number") != number:
        return None, [f"pull request #{number} could not be read back"]
    head = pull.get("head") if isinstance(pull.get("head"), dict) else {}
    base = pull.get("base") if isinstance(pull.get("base"), dict) else {}
    head_sha, base_sha, head_ref = _sha(head.get("sha")), _sha(base.get("sha")), head.get("ref")
    reasons: list[str] = []
    if pull.get("merged") is not True or pull.get("merge_commit_sha") != sha:
        reasons.append(
            f"#{number} is not merged as {sha} (merged={pull.get('merged')!r}, "
            f"merge commit {pull.get('merge_commit_sha')})"
        )
    if (head.get("repo") or {}).get("full_name") != repository:
        reasons.append(f"#{number}'s head is not a branch of {repository}")
    if not isinstance(head_ref, str) or not head_ref.startswith(EXPORT_BRANCH_PREFIX):
        reasons.append(f"#{number}'s head {head_ref!r} is not an export branch")
    if base.get("ref") != BASE_BRANCH or (base.get("repo") or {}).get("full_name") != repository:
        reasons.append(f"#{number} does not target {repository} {BASE_BRANCH}")
    if head_sha is None or base_sha is None:
        reasons.append(f"#{number} names no exact head and base sha")
        return None, reasons
    commit = document.get("commit") if isinstance(document.get("commit"), dict) else {}
    parents = commit.get("parents")
    if not isinstance(parents, list) or len(parents) != 1:
        reasons.append(
            f"{sha} has {len(parents) if isinstance(parents, list) else 'no'} parents, "
            f"so it is not the squash of #{number}"
        )
    elif parents[0] != base_sha:
        reasons.append(f"{sha}'s parent {parents[0]} is not #{number}'s base {base_sha}")
    fetched = document.get("head") if isinstance(document.get("head"), dict) else {}
    tree = commit.get("tree")
    if fetched.get("sha") != head_sha:
        reasons.append(f"#{number}'s head read back as {fetched.get('sha')}, not {head_sha}")
    elif _sha(tree) is None or fetched.get("tree") != tree:
        reasons.append(f"{sha}'s tree {tree} is not #{number}'s head tree {fetched.get('tree')}")
    elif fetched.get("base_is_ancestor") is not True:
        # A pull_request run tests the head merged into main as main stood when
        # the run began. Main only moves forward and the base is main at the
        # merge, so when the base is the head's ancestor every such merge is the
        # head itself, and the runs tested exactly this tree.
        reasons.append(
            f"#{number}'s base {base_sha} is not an ancestor of its head {head_sha}, "
            "so its runs could have tested a merge with something else"
        )
    if reasons:
        return None, reasons
    return {
        "pull": number,
        "head_sha": head_sha,
        "head_ref": head_ref,
        "base_sha": base_sha,
        "tree": tree,
    }, []


def resolve_inheritance(
    sha: str, document: dict[str, Any] | None, repository: str = EXPECTED_REPOSITORY
) -> dict[str, Any]:
    """The full inheritance answer for `sha`: bound, and the head graded green.

    release-tag.yml runs this exact function through the `inheritance` command,
    so the mint and the cut bind a commit to a pull request by one rule.
    """

    if document is None:
        return {"sha": sha, "inherited": False, "reasons": ["no inheritance read"]}
    binding, reasons = bind_export_pull(sha, document, repository)
    if binding is None:
        return {"sha": sha, "inherited": False, "reasons": reasons}
    head = grade_sha(
        binding["head_sha"],
        document.get("check_runs"),
        document.get("workflow_runs"),
        provenance=Provenance(binding["head_sha"], binding["head_ref"], KIND_EXPORT),
    )
    if head["verdict"] != GREEN:
        return {
            "sha": sha,
            "inherited": False,
            "reasons": [
                f"export pull request #{binding['pull']} head {binding['head_sha']} is "
                f"{head['verdict']}: " + "; ".join(head["dead"] or head["pending"])
            ],
            **binding,
        }
    return {
        "sha": sha,
        "inherited": True,
        "reasons": [],
        **binding,
        "runs": head["runs"],
        "retried": head["retried"],
    }


def merge_group_grade(
    sha: str, check_run_pages: Any, workflow_run_pages: Any, parents: list[str]
) -> tuple[dict[str, Any] | None, str | None]:
    """Grade `sha` on the merge queue's build of it, or say why that is not evidence.

    With squash merging the queue's group commit IS the commit that lands, so
    its build proves this exact tree. That is asserted rather than assumed, the
    way release-tag.yml asserts it: the queue ref names the base the group was
    built on, and that base must be this commit's only parent, read from
    protected history rather than from the listing being judged. Returns
    (None, None) when the commit never went through the queue.
    """

    runs = flatten_pages(workflow_run_pages, "workflow_runs")
    anchors = [
        run
        for run in runs
        if run.get("path") == CI_WORKFLOW
        and run.get("event") == MERGE_GROUP_EVENT
        and run.get("head_sha") == sha
        and run.get("conclusion") != "cancelled"
    ]
    if not anchors:
        return None, None
    if len(anchors) > 1:
        return None, f"{len(anchors)} uncancelled merge_group CI runs at {sha}: ambiguous merge-group evidence"
    ref = anchors[0].get("head_branch")
    queued = QUEUE_REF.fullmatch(ref) if isinstance(ref, str) else None
    if queued is None:
        return None, f"merge_group CI run {anchors[0].get('id')} is not on a merge-queue ref of main: {ref!r}"
    if list(parents) != [queued.group(2)]:
        return None, (
            f"merge group {ref} was built on {queued.group(2)}, but {sha}'s parents are "
            f"{list(parents)}, so it proved a different tree"
        )
    grade = grade_sha(sha, check_run_pages, workflow_run_pages, Provenance(sha, ref, KIND_MERGE_GROUP))
    grade["queue"] = {"ref": ref, "pull": int(queued.group(1)), "base": queued.group(2)}
    return grade, None


def grade_candidate(
    sha: str,
    check_run_pages: Any,
    workflow_run_pages: Any,
    inheritance: dict[str, Any] | Callable[[], dict[str, Any] | None] | None = None,
    repository: str = EXPECTED_REPOSITORY,
    parents: list[str] | None = None,
    export_inheritance: bool | None = None,
) -> dict[str, Any]:
    """Grade `sha` on main's own push runs, its merge-queue build, or its export head.

    A green push grade is used as is, and a push grade carrying a red or a
    conflicting writer is never overridden: the same tree failing a full pass
    is a verdict, and a second writer of a required name is an authority
    conflict whichever build is read. Only while main's own grade is still
    pending, or ended without a verdict (a cancelled run, a context it never
    published, or no push run at all), may another build of the same tree stand
    in for it: first the merge queue's build of this very commit, which is what
    a queued landing carries, then the export pull request head it was squashed
    from, which is what a landing outside the queue carries. `parents` is this
    commit's parents from protected history, and `inheritance` the read
    `resolve_inheritance` consumes, or a callable that makes it, so the reads
    are spent only when the push grade leaves room for them.
    """

    push = grade_sha(sha, check_run_pages, workflow_run_pages)
    push["evidence"] = {"source": EVIDENCE_PUSH, "sha": sha}
    if not (EXPORT_INHERITANCE if export_inheritance is None else export_inheritance):
        inheritance = None
    if push["verdict"] == GREEN or (inheritance is None and parents is None):
        return push
    if push["red"] or push["conflicts"]:
        push["evidence"]["inheritance"] = (
            "not considered: main's own push runs on this tree already show "
            + "; ".join(push["red"] + push["conflicts"])
        )
        return push
    if parents is not None:
        queued, refusal = merge_group_grade(sha, check_run_pages, workflow_run_pages, parents)
        if queued is not None and queued["verdict"] == GREEN:
            queue = queued["queue"]
            return {
                "sha": sha,
                "verdict": GREEN,
                "dead": [],
                "pending": [],
                "retried": list(queued["retried"]),
                "red": [],
                "conflicts": [],
                "runs": dict(queued["runs"]),
                "evidence": {
                    "source": EVIDENCE_MERGE_GROUP,
                    "sha": sha,
                    "queue_ref": queue["ref"],
                    "pull": queue["pull"],
                    "base_sha": queue["base"],
                    "runs": dict(queued["runs"]),
                    "main_push": {
                        "verdict": push["verdict"],
                        "pending": push["pending"],
                        "dead": push["dead"],
                    },
                },
            }
        if queued is not None:
            refusal = f"merge group {queued['queue']['ref']} is {queued['verdict']}: " + "; ".join(
                queued["dead"] or queued["pending"]
            )
        if refusal:
            push["evidence"]["merge_group"] = "refused: " + refusal
    if inheritance is None:
        return push
    try:
        document = inheritance() if callable(inheritance) else inheritance
    except SelectionError as exc:
        # Unlike the push grade's own listings, an unreadable inheritance read
        # cannot fail toward green: not inheriting is the old behaviour. So it
        # is recorded and the push grade stands, rather than failing the cut.
        push["evidence"]["inheritance"] = f"unreadable, so not considered: {exc}"
        print(f"::warning title=Inherited evidence unreadable::{sha}: {exc}", file=sys.stderr)
        return push
    resolved = resolve_inheritance(sha, document, repository)
    if not resolved["inherited"]:
        push["evidence"]["inheritance"] = "refused: " + "; ".join(resolved["reasons"])
        return push
    return {
        "sha": sha,
        "verdict": GREEN,
        "dead": [],
        "pending": [],
        "retried": list(resolved["retried"]),
        "red": [],
        "conflicts": [],
        "runs": dict(resolved["runs"]),
        "evidence": {
            "source": EVIDENCE_EXPORT,
            "sha": sha,
            "pull": resolved["pull"],
            "head_sha": resolved["head_sha"],
            "head_ref": resolved["head_ref"],
            "base_sha": resolved["base_sha"],
            "tree": resolved["tree"],
            "runs": dict(resolved["runs"]),
            "main_push": {
                "verdict": push["verdict"],
                "pending": push["pending"],
                "dead": push["dead"],
            },
        },
    }


def read_pages(fetch: Fetch, endpoint_for_page: Callable[[int], str]) -> list[Any]:
    """Every page of a listing, raw, for `grade_sha` to flatten and count."""

    pages = [fetch(endpoint_for_page(1))]
    total = _count(pages[0].get("total_count"), "listing total_count") if isinstance(pages[0], dict) else 0
    for page in range(2, max(1, math.ceil(total / PAGE_SIZE)) + 1):
        pages.append(fetch(endpoint_for_page(page)))
    return pages


def attach_cancel_annotations(fetch: Fetch, repository: str, check_pages: list[Any]) -> None:
    """Give each cancelled required check-run the annotations GitHub wrote on it.

    `settled_retry` admits a cancelled first attempt only when it was a timeout,
    and only the annotations say so. On almost every sha nothing required was
    cancelled and this reads nothing.
    """

    names = {name for name, _id, _path in REQUIRED_CONTEXTS + ACCEPTANCE_CONTEXTS}
    for page in check_pages:
        for check in (page.get("check_runs") if isinstance(page, dict) else None) or []:
            if isinstance(check, dict) and check.get("conclusion") == "cancelled" and check.get("name") in names:
                check["annotations"] = read_annotations(fetch, repository, check.get("id"))


def git_succeeds(workspace: Path, *arguments: str) -> bool:
    """A git predicate: exit 0 is true, exit 1 is false, anything else refuses."""

    result = subprocess.run(
        ["git", "-C", str(workspace), *arguments],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.returncode in (0, 1):
        return result.returncode == 0
    detail = result.stderr.strip() or result.stdout.strip() or "no output"
    raise SelectionError(f"git {' '.join(arguments)} failed: {detail}")


def read_inheritance(fetch: Fetch, repository: str, sha: str, workspace: Path) -> dict[str, Any]:
    """Read what `bind_export_pull` judges, stopping where a binding cannot exist.

    The commit's own tree and parents come from the protected-main checkout.
    The head is fetched into the workspace under refs/kin-inherited/, so its
    tree and the base's ancestry come from git too, and the pulls API answers
    only which pull request this commit was merged from.
    """

    lines = run_git(workspace, "rev-list", "--parents", "-n", "1", sha).split()
    if not lines or lines[0] != sha:
        raise SelectionError(f"git could not read {sha}'s parents")
    document: dict[str, Any] = {
        "sha": sha,
        "commit": {"tree": run_git(workspace, "rev-parse", f"{sha}^{{tree}}"), "parents": lines[1:]},
    }
    subject = re.search(r"\(#([0-9]+)\)$", run_git(workspace, "log", "-1", "--format=%s", sha))
    if subject:
        document["subject_pull"] = int(subject.group(1))
    associated = fetch(commit_pulls_endpoint(repository, sha))
    if not isinstance(associated, list):
        raise SelectionError(f"the pull requests associated with {sha} are not a list")
    document["associated_pulls"] = [
        {"number": pull.get("number"), "merge_commit_sha": pull.get("merge_commit_sha")}
        for pull in associated
        if isinstance(pull, dict)
    ]
    matching = [pull["number"] for pull in document["associated_pulls"] if pull["merge_commit_sha"] == sha]
    if len(matching) > 1:
        return document
    number = matching[0] if matching else document.get("subject_pull")
    if not isinstance(number, int):
        return document
    pull = fetch(pull_endpoint(repository, number))
    if not isinstance(pull, dict):
        raise SelectionError(f"pull request #{number} read back as something other than an object")
    head = pull.get("head") if isinstance(pull.get("head"), dict) else {}
    base = pull.get("base") if isinstance(pull.get("base"), dict) else {}
    document["pull"] = {
        "number": pull.get("number"),
        "merged": pull.get("merged"),
        "merge_commit_sha": pull.get("merge_commit_sha"),
        "head": {
            "sha": head.get("sha"),
            "ref": head.get("ref"),
            "repo": {"full_name": (head.get("repo") or {}).get("full_name")},
        },
        "base": {
            "sha": base.get("sha"),
            "ref": base.get("ref"),
            "repo": {"full_name": (base.get("repo") or {}).get("full_name")},
        },
    }
    head_sha, base_sha = _sha(head.get("sha")), _sha(base.get("sha"))
    if head_sha is None or base_sha is None:
        return document
    parked = pull_head_ref(number)
    run_git(workspace, "fetch", "--no-tags", "--quiet", "origin", f"+refs/pull/{number}/head:{parked}")
    fetched = run_git(workspace, "rev-parse", f"{parked}^{{commit}}")
    document["head"] = {"sha": fetched}
    if fetched != head_sha:
        return document
    document["head"]["tree"] = run_git(workspace, "rev-parse", f"{head_sha}^{{tree}}")
    document["head"]["base_is_ancestor"] = git_succeeds(
        workspace, "merge-base", "--is-ancestor", base_sha, head_sha
    )
    check_pages = read_pages(fetch, lambda page: check_runs_endpoint(repository, head_sha, page))
    attach_cancel_annotations(fetch, repository, check_pages)
    document["check_runs"] = check_pages
    document["workflow_runs"] = read_pages(fetch, lambda page: workflow_runs_endpoint(repository, head_sha, page))
    return document


def commit_parents(workspace: Path, sha: str) -> list[str]:
    line = run_git(workspace, "rev-list", "--parents", "-n", "1", sha).split()
    if not line or line[0] != sha:
        raise SelectionError(f"git could not read {sha}'s parents")
    return line[1:]


def grade_live(fetch: Fetch, repository: str, sha: str, workspace: Path | None = None) -> dict[str, Any]:
    check_pages = read_pages(fetch, lambda page: check_runs_endpoint(repository, sha, page))
    attach_cancel_annotations(fetch, repository, check_pages)
    run_pages = read_pages(fetch, lambda page: workflow_runs_endpoint(repository, sha, page))
    inheritance = None
    parents = None
    if workspace is not None:
        inheritance = lambda: read_inheritance(fetch, repository, sha, workspace)
        parents = commit_parents(workspace, sha)
    return grade_candidate(sha, check_pages, run_pages, inheritance, repository, parents)


# ---------------------------------------------------------------------------
# The merge queue: a release candidate is the group commit that will land.
# ---------------------------------------------------------------------------


MG_NONE = "none"
MG_RUNNING = "running"
MG_USABLE = "usable"
MG_RETRY = "retry"
MG_FAILED = "failed"


def merge_group_build_state(builds: list[dict[str, Any]]) -> tuple[str, dict[str, Any] | None, str]:
    """What the queue's RC Build of one group commit offers, and why.

    There is one such run per group commit; a re-run keeps the run and counts
    up `run_attempt`. A latest attempt that hit the build job's own timeout is
    re-run, not given up on, until RC_BUILD_TIMEOUT_LIMIT attempts have timed
    out, the same budget a dispatched candidate build gets.
    """

    if not builds:
        return MG_NONE, None, "no RC Build has started for this commit's merge group"
    # A usable run wins wherever it sits: the record filed from it names it
    # (rc_build.run_id), which is the run release.yml then ships from, so a
    # second run of the same commit is no longer ambiguous for anyone.
    usable = [build for build in builds if _usable(build)]
    if usable:
        build = usable[0]
        return MG_USABLE, build, f"merge_group RC Build {build['id']} succeeded and holds all six release artifacts"
    running = [build for build in builds if build.get("status") != "completed"]
    if running:
        build = running[0]
        return MG_RUNNING, build, f"merge_group RC Build {build['id']} is {build.get('status')}"
    build = builds[0]
    timeout = rc_build_timeout(build)
    attempt = build.get("run_attempt") or 1
    if timeout is not None and attempt < RC_BUILD_TIMEOUT_LIMIT:
        return MG_RETRY, build, (
            f"merge_group RC Build {build['id']} attempt {attempt} was killed by its runner "
            f"timeout ({timeout}); re-running its failed jobs rather than spending the candidate"
        )
    if timeout is not None:
        return MG_FAILED, build, (
            f"merge_group RC Build {build['id']} hit its runner timeout on all {attempt} attempts ({timeout})"
        )
    return MG_FAILED, build, (
        f"merge_group RC Build {build['id']} concluded {build.get('conclusion')}"
        + ("" if build.get("conclusion") != "success" else " without all six release artifacts")
    )


def bind_merge_group(sha: str, read: dict[str, Any]) -> dict[str, Any]:
    """Prove `sha` is a merge group commit of main, or that it already landed.

    A gh-readonly-queue ref can only be written by GitHub's merge queue, so a
    live one pointing at `sha` is the queue's own word that this is the commit
    it will fast-forward main to. The ref names the base the group was built
    on, which must be the commit's only parent, read from git, and that base
    must be protected main or, for a group stacked behind another, the head of
    another live group. Nothing else is a release candidate, and the mint still
    tags only once the commit is on main.
    """

    if read.get("landed") is True:
        return {"bound": "main", "reason": f"{sha} has landed on {BASE_BRANCH}"}
    refs = [ref for ref in read.get("refs") or [] if isinstance(ref, dict)]
    live = [ref for ref in refs if ref.get("sha") == sha]
    if len(live) != 1:
        return {
            "bound": None,
            "reason": f"{len(live)} live merge-queue refs point at {sha}"
            + ("; its group left the queue without landing" if not live else ""),
        }
    name = str(live[0].get("ref") or "").removeprefix("refs/heads/")
    queued = QUEUE_REF.fullmatch(name)
    if queued is None:
        return {"bound": None, "reason": f"{name!r} is not a merge-queue ref of {BASE_BRANCH}"}
    base = queued.group(2)
    if list(read.get("parents") or []) != [base]:
        return {
            "bound": None,
            "reason": f"{sha}'s parents {read.get('parents')} are not the base {base} its queue ref names",
        }
    stacked = base in {ref.get("sha") for ref in refs if ref is not live[0]}
    if read.get("base_on_main") is not True and not stacked:
        return {
            "bound": None,
            "reason": f"the base {base} of {name} is neither protected {BASE_BRANCH} nor another live group",
        }
    return {"bound": "merge-group", "ref": name, "pull": int(queued.group(1)), "base": base, "stacked": stacked}


def judge_queue(snapshot: dict[str, Any]) -> Decision:
    """Decide what the cut does for a merge group commit still in the queue.

    A group that stages a release is preflighted the moment its RC Build is
    usable, so preflight.json is filed before the queue merges and the push
    that lands it can mint at once. Every decision also says what to tell the
    queue (QUEUE_STATUS_CONTEXT): success for a group that stages no release or
    already has its record, failure for one whose build cannot be proven, and
    nothing while there is still something to wait for.
    """

    sha = snapshot.get("sha")
    if _sha(sha) is None:
        raise SelectionError(f"queue snapshot sha {sha!r} is not a 40-character sha")
    binding = snapshot.get("binding") if isinstance(snapshot.get("binding"), dict) else {}
    version = snapshot.get("version") or ""
    branch = candidate_branch(version) if VERSION.fullmatch(str(version)) else ""
    details = {"binding": binding}

    def decide(decision: str, reason: str, state: str | None, build: dict[str, Any] | None = None) -> Decision:
        return Decision(
            decision,
            reason,
            version,
            branch,
            candidate=sha,
            rc_run=build["id"] if build else None,
            queue_sha=sha,
            queue_state=state,
            details=details,
        )

    if binding.get("bound") != "merge-group":
        return Decision(
            STAND_DOWN,
            f"{sha} is not a live merge group commit of {BASE_BRANCH}: {binding.get('reason')}",
            version,
            branch,
            details=details,
        )
    # The reason doubles as the status description, capped at 140 characters,
    # so it names the group by its pull request; the ref is in the details.
    group = f"the merge group of #{binding.get('pull')}"
    refusal = version_refusal(version)
    if refusal is not None:
        return decide(STAND_DOWN, f"{group} stages no release: {refusal}", QUEUE_SUCCESS)
    if snapshot.get("tag_exists") is True:
        return decide(STAND_DOWN, f"{group} stages v{version}, which is already tagged, so no release waits on it", QUEUE_SUCCESS)
    records = snapshot.get("evidence", {}).get(sha, []) if isinstance(snapshot.get("evidence"), dict) else []
    if PREFLIGHT_RECORD in records:
        return decide(STAND_DOWN, f"{PREFLIGHT_RECORD} is already filed for {sha}, the {group} commit", QUEUE_SUCCESS)
    state, build, why = merge_group_build_state(list(snapshot.get("rc_builds") or []))
    if state == MG_USABLE:
        return decide(
            PROOF,
            f"{group} stages v{version}; {why}, so its archives are preflighted now and "
            f"{PREFLIGHT_RECORD} is filed for {sha} before the queue lands it",
            QUEUE_PROOF,
            build,
        )
    if state == MG_RETRY:
        # Inside the queue a re-run cannot land in time: the leg already spent
        # its whole bound. Failing now frees the queue for a fresh group.
        return decide(
            REFUSE,
            f"{group} stages v{version}, but its RC Build {build['id']} was killed by a runner timeout, "
            f"and a re-run cannot finish inside the queue's {QUEUE_RESPONSE_MINUTES}-minute window; "
            "re-enqueue it for a fresh build",
            QUEUE_FAILURE,
            build,
        )
    if state in (MG_NONE, MG_RUNNING):
        return decide(STAND_DOWN, f"{group} stages v{version}; {why}", None, build)
    return decide(REFUSE, f"{group} stages v{version}, but {why}", QUEUE_FAILURE, build)


def read_queue_binding(fetch: Fetch, repository: str, sha: str, workspace: Path) -> dict[str, Any]:
    """Read what `bind_merge_group` judges, from the queue's refs and from git."""

    listing = fetch(queue_refs_endpoint(repository))
    if not isinstance(listing, list):
        raise SelectionError("the merge-queue ref listing is not a list")
    refs = [
        {"ref": item.get("ref"), "sha": (item.get("object") or {}).get("sha")}
        for item in listing
        if isinstance(item, dict)
    ]
    read: dict[str, Any] = {"sha": sha, "refs": refs, "landed": False}
    if git_succeeds(workspace, "cat-file", "-e", f"{sha}^{{commit}}"):
        read["landed"] = git_succeeds(
            workspace, "merge-base", "--is-ancestor", sha, f"refs/remotes/origin/{BASE_BRANCH}"
        )
    if read["landed"]:
        return read
    live = [ref for ref in refs if ref["sha"] == sha]
    if len(live) != 1 or not str(live[0]["ref"]).startswith("refs/heads/gh-readonly-queue/"):
        return read
    parked = "refs/kin-queue/" + str(live[0]["ref"]).removeprefix("refs/heads/gh-readonly-queue/")
    run_git(workspace, "fetch", "--no-tags", "--quiet", "origin", f"+{live[0]['ref']}:{parked}")
    if run_git(workspace, "rev-parse", f"{parked}^{{commit}}") != sha:
        return read
    read["parents"] = commit_parents(workspace, sha)
    if len(read["parents"]) == 1:
        read["base_on_main"] = git_succeeds(
            workspace, "merge-base", "--is-ancestor", read["parents"][0], f"refs/remotes/origin/{BASE_BRANCH}"
        )
    return read


def gather_queue(repository: str, workspace: Path, sha: str, fetch: Fetch | None = None) -> dict[str, Any]:
    read = gh_json if fetch is None else fetch
    binding = bind_merge_group(sha, read_queue_binding(read, repository, sha, workspace))
    snapshot: dict[str, Any] = {"mode": "merge-group", "repository": repository, "sha": sha, "binding": binding}
    if binding.get("bound") != "merge-group":
        return snapshot
    version = workspace_version(run_git(workspace, "show", f"{sha}:Cargo.toml"))
    snapshot["version"] = version
    if version_refusal(version) is not None:
        return snapshot
    snapshot["tag_exists"] = read_tag_exists(read, repository, version)
    if snapshot["tag_exists"]:
        return snapshot
    snapshot["evidence"] = read_evidence(read, repository)
    snapshot["rc_builds"] = read_merge_group_rc_builds(read, repository, sha)
    return snapshot


# ---------------------------------------------------------------------------
# The judgment.
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Decision:
    decision: str
    reason: str
    version: str
    branch: str
    candidate: str | None = None
    rc_run: int | None = None
    move: str | None = None
    # First-contact state for the candidate this decision names, in the proof
    # gate's own vocabulary, or None when the decision names no candidate. It
    # is a field rather than a `details` key because `details` is a free-form
    # bag that never reaches GITHUB_OUTPUT, and the whole point of this value
    # is that a workflow reads it back.
    stranger_state: str | None = None
    # The merge group commit this decision is about while it is still in the
    # queue, and what the cut tells the queue about it (QUEUE_STATUS_CONTEXT).
    queue_sha: str | None = None
    queue_state: str | None = None
    details: dict[str, Any] = field(default_factory=dict)

    def document(self) -> dict[str, Any]:
        return {
            "decision": self.decision,
            "reason": self.reason,
            "version": self.version,
            "branch": self.branch,
            "candidate": self.candidate,
            "rc_run": self.rc_run,
            "move": self.move,
            "stranger_state": self.stranger_state,
            "queue_sha": self.queue_sha,
            "queue_state": self.queue_state,
            "details": self.details,
        }


def stranger_command(version: str, sha: str, rc_run: int | None) -> str:
    """The local step the workflow prints while the stranger has no hosted host.

    An exported ANTHROPIC_API_KEY drives it on a key instead of the account
    login: bin/kin-stranger scrubs that variable only for a local endpoint.
    """

    run_id = f"rc{version.replace('.', '')}-{sha[:7]}"
    download = f"/tmp/kin-rc-{sha[:12]}"
    lines = [
        f"gh run download {rc_run if rc_run is not None else '<rc-build run id>'} --repo {EXPECTED_REPOSITORY} --dir {download}",
        f"bin/kin-stranger prepare --run {run_id} --arms green,brown,vcs",
        f"bin/kin-stranger run --run {run_id} --archive {download}/kin-linux-aarch64/kin-linux-aarch64.tar.gz --candidate-sha {sha}",
    ]
    return "\n".join(lines)


def _snapshot_field(snapshot: dict[str, Any], key: str, kind: type) -> Any:
    value = snapshot.get(key)
    if not isinstance(value, kind) or (kind is int and isinstance(value, bool)):
        raise SelectionError(f"snapshot {key} is not a {kind.__name__}")
    return value


def judge(
    snapshot: dict[str, Any],
    grade: Grader,
    queue_builds: Callable[[str], list[dict[str, Any]]] | None = None,
) -> Decision:
    """Decide what the cut does next from one snapshot and a grader.

    The grader is called lazily and only for the shas the rule reaches, newest
    first, so a range with a green newest sha costs one grading. `queue_builds`
    reads the RC Build the merge queue ran on a sha, and is consulted only for
    the sha the cut would otherwise arm: a commit that landed through the queue
    already has its archives, so the cut preflights those instead of building
    the same commit again.
    """

    if not isinstance(snapshot, dict):
        raise SelectionError("snapshot must be an object")
    version = _snapshot_field(snapshot, "version", str)
    if VERSION.fullmatch(version) is None:
        raise SelectionError(f"snapshot version {version!r} is not semver-shaped")
    shas = _snapshot_field(snapshot, "range", list)
    if not shas or any(_sha(sha) is None for sha in shas):
        raise SelectionError("snapshot range must be a non-empty list of 40-character shas")
    if len(set(shas)) != len(shas):
        raise SelectionError("snapshot range repeats a sha")
    tag_exists = _snapshot_field(snapshot, "tag_exists", bool)
    evidence = _snapshot_field(snapshot, "evidence", dict)
    candidate = snapshot.get("candidate")
    if candidate is not None and _sha(candidate) is None:
        raise SelectionError(f"snapshot candidate {candidate!r} is not a 40-character sha")
    rc_builds = _snapshot_field(snapshot, "rc_builds", list)
    branch = candidate_branch(version)
    tip = shas[0]

    retired = version_refusal(version)
    if retired is not None:
        # The cut acts only on a version main may release. A candidate branch
        # or evidence left over from a cancelled version is never read here,
        # because everything below is keyed by this version.
        return Decision(
            STAND_DOWN,
            f"main stages v{version}, and {retired}; nothing is armed, preflighted or minted "
            "until main stages a releasable version",
            version,
            branch,
            details={"tip": tip},
        )

    if tag_exists:
        return Decision(
            STAND_DOWN,
            f"v{version} already exists; the cut is done and the train owns the next version",
            version,
            branch,
            details={"tip": tip},
        )

    def records(sha: str) -> set[str]:
        value = evidence.get(sha, [])
        if not isinstance(value, list):
            raise SelectionError(f"evidence for {sha} is not a list")
        return set(value)

    proven = [sha for sha in shas if {PREFLIGHT_RECORD, STRANGER_RECORD} <= records(sha)]
    if proven:
        return Decision(
            STAND_DOWN,
            f"{proven[0]} carries both proof records and awaits the mint",
            version,
            branch,
            candidate=proven[0],
            stranger_state=STRANGER_COMPLETE,
            details={"proven": proven},
        )

    # Mint-eligible on the machine preflight alone. release-tag.yml lists any
    # sha carrying preflight.json and runs its proof gate with
    # KIN_RELEASE_REQUIRE=preflight, so a filed preflight is already a tag
    # whether or not a stranger ever runs. The cut still files the stranger
    # when it can, because that record is the only thing that tells a measured
    # release from an unmeasured one; what it no longer does is treat a missing
    # one as a reason to hold the version, or the release. Computed here and
    # consumed after the candidate block, so the
    # STRANGER decision below stays reachable and release-cut.yml's stranger
    # job keeps running.
    mintable = [sha for sha in shas if PREFLIGHT_RECORD in records(sha)]

    dead: dict[str, list[str]] = {}
    notes: list[str] = []
    if candidate is not None:
        if candidate not in shas:
            return Decision(
                REFUSE,
                f"{branch} points at {candidate}, which is not a reviewed {BASE_BRANCH} commit "
                f"carrying {version}; delete or move the branch by hand",
                version,
                branch,
                candidate=candidate,
            )
        if PREFLIGHT_RECORD in records(candidate):
            # The stranger has to run on the very bytes the published preflight
            # judged, because kin-evidence-publish refuses a stranger record
            # naming an archive no preflight leg for this candidate judged. The
            # rc-build's artifacts carry those bytes and they expire, so an
            # expired run is a refusal rather than a rebuild: a second build of
            # the same sha is not guaranteed to reproduce the archive sha256 the
            # record already names, and a candidate that can no longer be proven
            # needs a human rather than a retry.
            rc_run = _newest_usable_run(rc_builds, candidate)
            if rc_run is None:
                return Decision(
                    REFUSE,
                    f"preflight is recorded for {candidate} but no rc-build still holds the "
                    "archives it judged, so the stranger cannot run on the bytes the record "
                    f"names; first contact can never be recorded for {candidate}, and the "
                    "mint may still tag it with the stranger pending",
                    version,
                    branch,
                    candidate=candidate,
                    stranger_state=STRANGER_PENDING,
                )
            return Decision(
                STRANGER,
                f"preflight is recorded for {candidate}, which the mint can already tag and "
                "promote; the stranger record is the only thing that will tell that release "
                "from a measured one, so run it while the archives it must judge still exist",
                version,
                branch,
                candidate=candidate,
                rc_run=rc_run,
                stranger_state=STRANGER_PENDING,
                details={
                    "stranger_command": stranger_command(version, candidate, rc_run),
                    "mintable": mintable,
                },
            )
        grade_of = grade(candidate)
        if grade_of.get("verdict") == PENDING:
            return Decision(
                STAND_DOWN,
                f"candidate {candidate} is being graded again: {'; '.join(grade_of.get('pending', []))}",
                version,
                branch,
                candidate=candidate,
            )
        if grade_of.get("verdict") == DEAD:
            dead[candidate] = list(grade_of.get("dead", []))
            notes.append(f"current candidate {candidate} died: {'; '.join(dead[candidate])}")
        else:
            if queue_builds is not None:
                # The queue is the producer and a dispatch the fallback: never
                # build a commit again while the queue's build of it is usable,
                # running or being re-run.
                queued = _queued_build_decision(candidate, version, branch, grade_of, queue_builds(candidate), notes)
                if queued is not None:
                    return queued
            builds = [build for build in rc_builds if build.get("head_sha") == candidate]
            active = [build for build in builds if build.get("status") != "completed"]
            if active:
                return Decision(
                    STAND_DOWN,
                    f"rc-build {active[0]['id']} is {active[0].get('status')} for {candidate}",
                    version,
                    branch,
                    candidate=candidate,
                    rc_run=active[0]["id"],
                )
            usable = [build for build in builds if _usable(build)]
            if usable:
                return Decision(
                    PROOF,
                    f"rc-build {usable[0]['id']} succeeded for {candidate} and still holds its archives",
                    version,
                    branch,
                    candidate=candidate,
                    rc_run=usable[0]["id"],
                    details={
                        "stranger_command": stranger_command(version, candidate, usable[0]["id"]),
                        "evidence": grade_of.get("evidence"),
                    },
                )
            spent, timeouts = rc_build_attempts(builds)
            if len(spent) >= RC_BUILD_ATTEMPT_LIMIT:
                dead[candidate] = [f"rc-build attempts exhausted: {', '.join(spent + timeouts)}"]
                notes.append(f"current candidate {candidate} died: {dead[candidate][0]}")
            elif len(timeouts) >= RC_BUILD_TIMEOUT_LIMIT:
                dead[candidate] = [
                    f"rc-build hit its runner timeout {len(timeouts)} times: {', '.join(timeouts + spent)}"
                ]
                notes.append(f"current candidate {candidate} died: {dead[candidate][0]}")
            else:
                prior = []
                if spent:
                    prior.append(f"prior attempts: {', '.join(spent)}")
                if timeouts:
                    prior.append(f"runner timeouts retried rather than spent: {', '.join(timeouts)}")
                return Decision(
                    ARM,
                    f"{candidate} is green with no usable rc-build yet"
                    + (f" ({'; '.join(prior)})" if prior else "")
                    + _evidence_clause(grade_of),
                    version,
                    branch,
                    candidate=candidate,
                    move=MOVE_NONE,
                    details={"attempts": spent, "timeouts": timeouts, "evidence": grade_of.get("evidence")},
                )

    if mintable:
        # Nothing left for the cut to do on this version. The candidate block
        # above already returned for a mintable current candidate, so reaching
        # here means the mint owns a sha the cut has no further work on, and
        # arming a newer one would only race the tag.
        return Decision(
            STAND_DOWN,
            f"{mintable[0]} carries {PREFLIGHT_RECORD} and awaits the mint, which tags on the "
            "machine preflight alone; first contact is still unrecorded, so the release will "
            "ship and be promoted carrying a notice that names that gap",
            version,
            branch,
            candidate=mintable[0],
            stranger_state=STRANGER_PENDING,
            details={"mintable": mintable, "notes": notes},
        )

    pending: dict[str, list[str]] = {}
    disqualified: dict[str, list[str]] = {}
    for sha in shas:
        if sha in dead:
            disqualified[sha] = dead[sha]
            continue
        grade_of = grade(sha)
        verdict = grade_of.get("verdict")
        if verdict == GREEN and queue_builds is not None and candidate != sha:
            queued = _queued_build_decision(sha, version, branch, grade_of, queue_builds(sha), notes)
            if queued is not None:
                return queued
        if verdict == GREEN:
            if candidate is None:
                move = MOVE_CREATE
            elif candidate == sha:
                move = MOVE_NONE
            elif shas.index(sha) < shas.index(candidate):
                move = MOVE_FAST_FORWARD
            else:
                move = MOVE_RESET
            inherited = (grade_of.get("evidence") or {}).get("source") in (EVIDENCE_EXPORT, EVIDENCE_MERGE_GROUP)
            return Decision(
                ARM,
                (
                    f"{sha} is the newest reviewed {BASE_BRANCH} commit carrying {version}"
                    + _evidence_clause(grade_of)
                    if inherited
                    else f"{sha} is the newest reviewed {BASE_BRANCH} commit carrying {version} whose push "
                    "runs concluded success with every required context present exactly once "
                    "or retried once after a failure"
                ),
                version,
                branch,
                candidate=sha,
                move=move,
                details={
                    "notes": notes,
                    "retried": list(grade_of.get("retried", [])),
                    "skipped_pending": pending,
                    "disqualified": disqualified,
                    "previous_candidate": candidate,
                    "evidence": grade_of.get("evidence"),
                },
            )
        if verdict == PENDING:
            pending[sha] = list(grade_of.get("pending", []))
            continue
        if verdict != DEAD:
            raise SelectionError(f"grader answered {verdict!r} for {sha}")
        disqualified[sha] = list(grade_of.get("dead", []))

    if pending:
        return Decision(
            STAND_DOWN,
            f"no complete green sha carries {version} yet; {len(pending)} still being graded",
            version,
            branch,
            details={"notes": notes, "pending": pending, "disqualified": disqualified},
        )
    newest = shas[0]
    return Decision(
        REFUSE,
        f"no reviewed {BASE_BRANCH} commit carrying {version} qualifies; newest {newest}: "
        + "; ".join(disqualified.get(newest, ["not graded"])),
        version,
        branch,
        details={"notes": notes, "disqualified": disqualified},
    )


def _queued_build_decision(
    sha: str,
    version: str,
    branch: str,
    grade_of: dict[str, Any],
    builds: list[dict[str, Any]],
    notes: list[str],
) -> Decision | None:
    """Preflight, wait for or retry the merge queue's RC Build of a landed sha.

    Returns None when the queue built nothing usable for it, and the cut then
    arms its own build on the candidate branch exactly as before, which is also
    the path for a landing that never went through the queue.
    """

    state, build, why = merge_group_build_state(builds)
    details = {"evidence": grade_of.get("evidence"), "rc_source": EVIDENCE_MERGE_GROUP, "notes": notes}
    if state == MG_USABLE:
        return Decision(
            PROOF,
            f"{sha} landed through the merge queue and {why}; preflighting those archives "
            "rather than building the same commit again" + _evidence_clause(grade_of),
            version,
            branch,
            candidate=sha,
            rc_run=build["id"],
            details=details,
        )
    if state == MG_RUNNING:
        return Decision(STAND_DOWN, f"{sha} landed through the merge queue and {why}", version, branch,
                        candidate=sha, rc_run=build["id"], details=details)
    if state == MG_RETRY:
        return Decision(RETRY, f"{sha} landed through the merge queue; {why}", version, branch,
                        candidate=sha, rc_run=build["id"], details=details)
    if state == MG_FAILED:
        notes.append(f"{why}; arming a build of {sha} on {branch} instead")
    return None


def _evidence_clause(grade_of: dict[str, Any]) -> str:
    """The sentence a decision carries when its grade was inherited, else nothing."""

    evidence = grade_of.get("evidence") or {}
    runs = evidence.get("runs") or {}
    main_push = evidence.get("main_push") or {}
    waiting = "; ".join((main_push.get("pending") or []) + (main_push.get("dead") or [])) or "not graded"
    if evidence.get("source") == EVIDENCE_MERGE_GROUP:
        return (
            f", graded on the merge queue's build of this very commit on {evidence.get('queue_ref')} "
            f"(pull request #{evidence.get('pull')}, built on its parent {evidence.get('base_sha')}): "
            f"CI run {runs.get('CI')} and Acceptance run {runs.get('Acceptance')} concluded success "
            "with every required context present exactly once or retried once after a failure. "
            f"Main's own push runs are not on this path ({waiting})"
        )
    if evidence.get("source") != EVIDENCE_EXPORT:
        return ""
    return (
        f", and it inherits CI and Acceptance evidence from export pull request "
        f"#{evidence.get('pull')}: it is that pull request's squash onto its base "
        f"{evidence.get('base_sha')}, its tree {evidence.get('tree')} is the head "
        f"{evidence.get('head_sha')}'s tree, and on that head CI run {runs.get('CI')} and "
        f"Acceptance run {runs.get('Acceptance')} concluded success with every required "
        "context present exactly once or retried once after a failure. Main's own push "
        f"runs keep guarding main and are not on this path ({waiting})"
    )


def rc_build_attempts(builds: list[dict[str, Any]]) -> tuple[list[str], list[str]]:
    """Split a candidate's finished rc-builds into spent attempts and timeouts.

    A spent attempt is any finished build that left no usable archives and was
    not a runner timeout: a failure, a person's cancel, a superseding dispatch,
    or a success whose archives expired. A timeout is retried without spending
    one (`rc_build_timeout`), within RC_BUILD_TIMEOUT_LIMIT.
    """

    spent: list[str] = []
    timeouts: list[str] = []
    for build in builds:
        reason = rc_build_timeout(build)
        if reason is not None:
            timeouts.append(f"{build['id']} (cancelled: {reason})")
        else:
            spent.append(f"{build['id']} ({build.get('conclusion') or 'no conclusion'})")
    return spent, timeouts


def _usable(build: dict[str, Any]) -> bool:
    """A completed rc-build still holding every candidate archive it built.

    `artifacts` is filtered to unexpired names when the run is read, so a run
    whose archives aged out reads as missing them and is refused rather than
    reused: the stranger has to run on the very bytes a published preflight
    record names, and a rebuild is not guaranteed to reproduce that sha256.
    """

    if build.get("status") != "completed" or build.get("conclusion") != "success":
        return False
    names = set(build.get("artifacts") or [])
    return all(artifact in names for artifact in RC_RELEASE_ARTIFACTS)


def _newest_usable_run(rc_builds: list[dict[str, Any]], sha: str) -> int | None:
    for build in rc_builds:
        if build.get("head_sha") == sha and _usable(build):
            return build["id"]
    return None


# ---------------------------------------------------------------------------
# Live gather.
# ---------------------------------------------------------------------------


def gather(repository: str, workspace: Path, fetch: Fetch | None = None) -> dict[str, Any]:
    read = gh_json if fetch is None else fetch
    tip = run_git(workspace, "rev-parse", f"refs/remotes/origin/{BASE_BRANCH}")
    if _sha(tip) is None:
        raise SelectionError(f"origin/{BASE_BRANCH} did not resolve to an exact sha")
    version = workspace_version(run_git(workspace, "show", f"{tip}:Cargo.toml"))
    shas = version_range(workspace, tip, version)
    branch = candidate_branch(version)
    return {
        "repository": repository,
        "version": version,
        "range": shas,
        "tag_exists": read_tag_exists(read, repository, version),
        "evidence": read_evidence(read, repository),
        "candidate": read_branch(read, repository, branch),
        "rc_builds": read_rc_builds(read, repository, branch),
    }


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)

    trigger = commands.add_parser("validate-trigger", help="admit the trigger from GitHub-owned context")
    trigger.add_argument("--event-file", required=True)
    trigger.add_argument("--event-name", required=True)
    trigger.add_argument("--event-action", default="")
    trigger.add_argument("--actor", required=True)
    trigger.add_argument("--repository", required=True)
    trigger.add_argument("--default-branch", required=True)
    trigger.add_argument("--ref", required=True)
    trigger.add_argument("--workflow-sha", required=True)

    select = commands.add_parser("select", help="decide what the cut does next")
    select.add_argument("--repository", default=EXPECTED_REPOSITORY)
    select.add_argument("--workspace", help="the protected-main checkout the range is read from")
    select.add_argument(
        "--snapshot",
        help="judge this snapshot document instead of reading GitHub; it carries the grades",
    )
    select.add_argument(
        "--queue-sha",
        help="the merge group commit a merge_group completion names; verified, never trusted",
    )

    bind = commands.add_parser(
        "bind-candidate",
        help="prove a candidate is protected main history or a live merge group commit of main",
    )
    bind.add_argument("--repository", default=EXPECTED_REPOSITORY)
    bind.add_argument("--sha", required=True)
    bind.add_argument("--workspace", required=True)

    admit = commands.add_parser("admit-version", help="refuse a version that may never be released")
    admit.add_argument("--repository", default=EXPECTED_REPOSITORY)
    admit.add_argument("--version", required=True)

    inherit = commands.add_parser(
        "inheritance",
        help="say whether a main commit inherits its export pull request's evidence",
    )
    inherit.add_argument("--repository", default=EXPECTED_REPOSITORY)
    inherit.add_argument("--sha", required=True)
    inherit.add_argument("--workspace", help="the protected-main checkout the commit is read from")
    inherit.add_argument(
        "--document",
        help="judge this inheritance read instead of reading GitHub and git",
    )
    return parser.parse_args(argv)


def _emit(decision: Decision) -> int:
    document = decision.document()
    print(json.dumps(document, sort_keys=True))
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as handle:
            for key in OUTPUT_KEYS:
                value = document.get(key)
                handle.write(f"{key}={'' if value is None else value}\n")
            handle.write(f"reason={decision.reason.replace(chr(10), ' ')}\n")
    if decision.decision == REFUSE:
        # A refusal is a release hold, not a broken run: no candidate qualifies
        # yet, and the next cycle re-judges. It is surfaced as a warning and in
        # the published decision artifact; a hold that persists is what
        # release-hold-alarm.mjs exists to raise. Failing the run here turned
        # every scheduled cycle into a failure notification while the hold lasted.
        print(f"::warning title=Release cut refused::{decision.reason}", file=sys.stderr)
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv if argv is not None else sys.argv[1:])
    try:
        if args.command == "validate-trigger":
            with open(args.event_file, encoding="utf-8") as handle:
                event = json.load(handle)
            verdict = validate_trigger(
                event_name=args.event_name,
                event_action=args.event_action,
                actor=args.actor,
                repository=args.repository,
                default_branch=args.default_branch,
                ref=args.ref,
                workflow_sha=args.workflow_sha,
                event=event,
            )
            print(json.dumps(verdict, sort_keys=True))
            return 0
        if args.repository.count("/") != 1:
            raise SelectionError("repository must be owner/name")
        if args.command == "admit-version":
            # release-tag.yml asks this, from protected main's copy, before it
            # mints. Exit 3 is a refusal, kept apart from 2, which is a read
            # that could not be made.
            refusal = version_refusal(args.version.removeprefix("v"))
            print(json.dumps({"version": args.version, "admitted": refusal is None, "reason": refusal}))
            return 0 if refusal is None else 3
        if args.command == "bind-candidate":
            # The publish job's binding: a preflight record may be filed for a
            # commit on protected main, or for the commit a live merge group of
            # main will land, and for nothing else.
            if _sha(args.sha) is None:
                raise SelectionError(f"--sha must be a 40-character lowercase sha, got {args.sha!r}")
            binding = bind_merge_group(
                args.sha, read_queue_binding(gh_json, args.repository, args.sha, Path(args.workspace))
            )
            print(json.dumps(binding, sort_keys=True))
            return 0 if binding.get("bound") in ("main", "merge-group") else 1
        if args.command == "inheritance":
            # release-tag.yml's mint asks this before it judges the required
            # contexts, so the mint and the cut bind a commit to an export pull
            # request by the one rule above. An answer exits 0 either way; only
            # a read that could not be made exits 2, and the mint then grades
            # main's own push runs exactly as it did before inheritance existed.
            if _sha(args.sha) is None:
                raise SelectionError(f"--sha must be a 40-character lowercase sha, got {args.sha!r}")
            if not EXPORT_INHERITANCE:
                print(json.dumps({
                    "sha": args.sha,
                    "inherited": False,
                    "reasons": [
                        "export inheritance is off: a pull request head's release contexts can come "
                        "from stand-in jobs, so only main's own push runs or the merge queue's build "
                        "of this commit are evidence"
                    ],
                }, sort_keys=True))
                return 0
            if args.document:
                with open(args.document, encoding="utf-8") as handle:
                    read = json.load(handle)
            else:
                if not args.workspace:
                    raise SelectionError("inheritance needs --workspace (or --document)")
                read = read_inheritance(gh_json, args.repository, args.sha, Path(args.workspace))
            print(json.dumps(resolve_inheritance(args.sha, read, args.repository), sort_keys=True))
            return 0
        if args.snapshot:
            with open(args.snapshot, encoding="utf-8") as handle:
                document = json.load(handle)
            grades = document.get("grades") if isinstance(document, dict) else None
            if not isinstance(grades, dict):
                raise SelectionError("a snapshot document must carry its grades")

            if document.get("mode") == "merge-group":
                return _emit(judge_queue(document))

            def grade(sha: str) -> dict[str, Any]:
                pages = grades.get(sha)
                if not isinstance(pages, dict):
                    raise SelectionError(f"snapshot carries no grade for {sha}")
                return grade_candidate(
                    sha,
                    pages.get("check_runs"),
                    pages.get("workflow_runs"),
                    pages.get("inheritance"),
                    str(document.get("repository") or EXPECTED_REPOSITORY),
                    pages.get("parents"),
                )

            queued = document.get("queue_builds") if isinstance(document.get("queue_builds"), dict) else {}
            return _emit(judge(document, grade, lambda sha: list(queued.get(sha) or [])))
        if not args.workspace:
            raise SelectionError("select needs --workspace (or --snapshot)")
        workspace = Path(args.workspace)
        if args.queue_sha:
            # A merge_group completion. While the group is still queued the
            # cut judges that one commit; once it has landed it is an ordinary
            # main commit, judged with everything else in the version range.
            if _sha(args.queue_sha) is None:
                raise SelectionError(f"--queue-sha must be a 40-character lowercase sha, got {args.queue_sha!r}")
            queue = gather_queue(args.repository, workspace, args.queue_sha)
            if queue["binding"].get("bound") != "main":
                return _emit(judge_queue(queue))
        snapshot = gather(args.repository, workspace)
        return _emit(
            judge(
                snapshot,
                lambda sha: grade_live(gh_json, args.repository, sha, workspace),
                lambda sha: read_merge_group_rc_builds(gh_json, args.repository, sha),
            )
        )
    except SelectionError as exc:
        print(f"::error title=Release cut could not decide::{exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
