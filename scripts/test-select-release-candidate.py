#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Deterministic contract tests for the automatic release-candidate selection.

`scripts/select-release-candidate.py` decides which main commit the proof loop
proves next, and both of its judgments are pure functions over one snapshot, so
every case here is a fixture and a verdict with no GitHub in the loop. The
fixtures are shaped on real listings read on 2026-09-02:

  * 57228997f carried three check-runs per required name, because a CI push run
    was rerun to attempt 3, and a second `gitleaks (full history)` produced by a
    push to the candidate branch. That sha is what taught the fleet that a rerun
    is what makes a sha unmintable, whatever the copies concluded.
  * 59c2239c2's `Windows installer + vector release build` concluded cancelled
    at the 60-minute job cap while Acceptance passed, so a red required context
    has to disqualify a sha its Acceptance run says nothing about.
  * 390fbdc54 carried every required context exactly once and became v0.6.4.

The inheritance and timeout cases read real listings instead, kept in
scripts/fixtures/release-candidate/pr1848-08d48c27.json: export pull request
1848, the squash 08d48c27 it landed as on 2026-10-12 with an identical tree,
main's own runs for that commit, and the RC Build attempts the cut spent on it
when the macOS arm64 leg hit its 90-minute bound twice. Every negative case is
the smallest edit to that real read that breaks one condition.

The workflow contract tests pin the shape of release-cut.yml, because a step
that stops calling the selector reads exactly like one that never needed it.
"""

from __future__ import annotations

import ast
import copy
import importlib.util
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
SELECTOR_PATH = ROOT / "scripts" / "select-release-candidate.py"
CUT_WORKFLOW_PATH = ROOT / ".github" / "workflows" / "release-cut.yml"
RELEASE_TAG_PATH = ROOT / ".github" / "workflows" / "release-tag.yml"
RC_BUILD_PATH = ROOT / ".github" / "workflows" / "rc-build.yml"
RELEASE_BOT_DOC = ROOT / "docs" / "release-bot.md"
REAL_FIXTURE_PATH = ROOT / "scripts" / "fixtures" / "release-candidate" / "pr1848-08d48c27.json"
REPO = "kinlab-ai/kin"
VERSION = "0.6.5"
NEWEST = "a" * 40
MIDDLE = "b" * 40
OLDEST = "c" * 40
FOREIGN = "d" * 40
CI_SUITE = 91_000_000_001
ACCEPTANCE_SUITE = 91_000_000_002
SAST_SUITE = 91_000_000_003
SECRET_SUITE = 91_000_000_004
CI_RUN = 33_000_000_001
ACCEPTANCE_RUN = 33_000_000_002
SAST_RUN = 33_000_000_003
SECRET_RUN = 33_000_000_004


def job_block(source: str, job: str) -> str:
    """One job's own YAML, from its two-space key to the next one.

    Both workflows carry more than one `artifact:` matrix, so a whole-file
    search reads rows from a job the assertion is not about: rc-build.yml's
    capability matrix repeats the build artifacts. Slicing by job is what keeps a contract test honest about its subject.
    """

    match = re.search(rf"(?ms)^  {re.escape(job)}:\n(.*?)(?=^  [a-z][a-z0-9_-]*:\n|\Z)", source)
    if match is None:
        raise AssertionError(f"no job named {job} in this workflow")
    return match.group(1)


def matrix_artifacts(block: str) -> list[str]:
    return re.findall(r"(?m)^ +artifact: ([A-Za-z0-9._-]+)$", block)


def job_names(source: str) -> list[str]:
    return re.findall(r"(?m)^  ([a-z][a-z0-9_-]*):\n", source)


def job_needs(block: str) -> list[str]:
    match = re.search(r"(?m)^    needs: (.+)$", block)
    if match is None:
        return []
    value = match.group(1).strip()
    if value.startswith("["):
        return [name.strip() for name in value.strip("[]").split(",") if name.strip()]
    return [value]


def job_if(block: str) -> str:
    """The job-level `if`, block scalar or inline, flattened to one line."""

    match = re.search(r"(?m)^    if: (.*)$", block)
    if match is None:
        return ""
    first = match.group(1).strip()
    if not first.startswith(">-") and not first.startswith("|"):
        return first
    body = block.split(match.group(0), 1)[1].splitlines()
    lines = []
    for line in body:
        if not line.strip():
            continue
        if not line.startswith("      "):
            break
        if line.strip().startswith("#"):
            continue
        lines.append(line.strip())
    return " ".join(lines)


def textwrap_dedent(text: str) -> str:
    import textwrap

    return textwrap.dedent(text)


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"could not load {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


selector = load_module("kin_release_candidate_selector", SELECTOR_PATH)


# ---------------------------------------------------------------------------
# Fixture builders for the two listings the grader reads.
# ---------------------------------------------------------------------------


def check_run(
    name: str,
    *,
    suite: int,
    status: str = "completed",
    conclusion: str | None = "success",
    app: tuple[int, str] = selector.GITHUB_ACTIONS_APP,
    started_at: str | None = None,
    completed_at: str | None = None,
) -> dict[str, Any]:
    return {
        "name": name,
        "status": status,
        "conclusion": conclusion,
        "check_suite": {"id": suite},
        "app": {"id": app[0], "slug": app[1]},
        "started_at": started_at,
        "completed_at": completed_at,
    }


def workflow_run(
    path: str,
    *,
    run_id: int,
    suite: int,
    sha: str,
    workflow_id: int,
    event: str = "push",
    branch: str = "main",
    status: str = "completed",
    conclusion: str | None = "success",
) -> dict[str, Any]:
    return {
        "id": run_id,
        "workflow_id": workflow_id,
        "path": path,
        "event": event,
        "head_branch": branch,
        "head_sha": sha,
        "status": status,
        "conclusion": conclusion,
        "check_suite_id": suite,
    }


def pages(items: list[dict[str, Any]], key: str, total: int | None = None) -> list[dict[str, Any]]:
    return [{"total_count": len(items) if total is None else total, key: items}]


def green_checks(sha: str = NEWEST) -> list[dict[str, Any]]:
    suites = {
        ".github/workflows/ci.yml": CI_SUITE,
        ".github/workflows/sast.yml": SAST_SUITE,
        ".github/workflows/secret-scan.yml": SECRET_SUITE,
    }
    runs = []
    for name, _workflow_id, path in selector.REQUIRED_CONTEXTS:
        conclusion = "skipped" if name in selector.SKIPPABLE_REQUIRED else "success"
        runs.append(check_run(name, suite=suites[path], conclusion=conclusion))
    return runs


def green_workflow_runs(sha: str = NEWEST) -> list[dict[str, Any]]:
    return [
        workflow_run(".github/workflows/ci.yml", run_id=CI_RUN, suite=CI_SUITE, sha=sha, workflow_id=245803170),
        workflow_run(
            ".github/workflows/acceptance.yml",
            run_id=ACCEPTANCE_RUN,
            suite=ACCEPTANCE_SUITE,
            sha=sha,
            workflow_id=339594603,
        ),
        workflow_run(".github/workflows/sast.yml", run_id=SAST_RUN, suite=SAST_SUITE, sha=sha, workflow_id=251549972),
        workflow_run(
            ".github/workflows/secret-scan.yml",
            run_id=SECRET_RUN,
            suite=SECRET_SUITE,
            sha=sha,
            workflow_id=293452372,
        ),
    ]


def grade_pages(checks: list[dict[str, Any]], runs: list[dict[str, Any]]) -> dict[str, Any]:
    return {"check_runs": pages(checks, "check_runs"), "workflow_runs": pages(runs, "workflow_runs")}


def green_grade(sha: str = NEWEST) -> dict[str, Any]:
    return grade_pages(green_checks(sha), green_workflow_runs(sha))


def with_duplicate(grade: dict[str, Any], name: str, suite: int) -> dict[str, Any]:
    """Append a second check-run under one required name, total_count included.

    The total has to move with it. `flatten_pages` asserts the listed length
    against `total_count` precisely so a short page cannot read as a sha with
    fewer checks, and a fixture that appends without bumping the total tests
    that assertion rather than the duplication it meant to test.
    """

    grade = copy.deepcopy(grade)
    page = grade["check_runs"][0]
    page["check_runs"].append(check_run(name, suite=suite))
    page["total_count"] = len(page["check_runs"])
    return grade


def rc_build(
    sha: str,
    *,
    run_id: int = 34_000_000_001,
    status: str = "completed",
    conclusion: str | None = "success",
    artifacts: list[str] | None = None,
    created_at: str = "2026-09-02T20:00:00Z",
) -> dict[str, Any]:
    if artifacts is None:
        artifacts = list(selector.RC_RELEASE_ARTIFACTS)
    return {
        "id": run_id,
        "head_sha": sha,
        "head_branch": selector.candidate_branch(VERSION),
        "event": "workflow_dispatch",
        "status": status,
        "conclusion": conclusion,
        "created_at": created_at,
        "artifacts": sorted(artifacts),
    }


def snapshot(**overrides: Any) -> dict[str, Any]:
    document = {
        "repository": REPO,
        "version": VERSION,
        "range": [NEWEST, MIDDLE, OLDEST],
        "tag_exists": False,
        "evidence": {},
        "candidate": None,
        "rc_builds": [],
    }
    document.update(overrides)
    return document


class Grader:
    """Serve a grade per sha and record which shas were actually graded."""

    def __init__(self, grades: dict[str, dict[str, Any]]) -> None:
        self.grades = grades
        self.calls: list[str] = []

    def __call__(self, sha: str) -> dict[str, Any]:
        self.calls.append(sha)
        if sha not in self.grades:
            raise AssertionError(f"unexpected grading of {sha}")
        return selector.grade_sha(sha, self.grades[sha]["check_runs"], self.grades[sha]["workflow_runs"])


# ---------------------------------------------------------------------------
# Grading one sha.
# ---------------------------------------------------------------------------


class GradeShaTests(unittest.TestCase):
    def grade(self, checks: list[dict[str, Any]], runs: list[dict[str, Any]], sha: str = NEWEST) -> dict[str, Any]:
        return selector.grade_sha(sha, pages(checks, "check_runs"), pages(runs, "workflow_runs"))

    def test_green_sha(self) -> None:
        verdict = self.grade(green_checks(), green_workflow_runs())
        self.assertEqual(verdict["verdict"], selector.GREEN, verdict)
        self.assertEqual(verdict["dead"], [])
        self.assertEqual(verdict["pending"], [])

    def test_a_rerun_duplicating_one_required_context_is_dead(self) -> None:
        """57228997f: a rerun claims the name twice and the mint calls that ambiguous."""

        checks = green_checks()
        checks.append(check_run("Falsify guards", suite=CI_SUITE))
        verdict = self.grade(checks, green_workflow_runs())
        self.assertEqual(verdict["verdict"], selector.DEAD)
        self.assertTrue(
            any("ambiguous required check: Falsify guards" in line for line in verdict["dead"]),
            verdict["dead"],
        )

    def retried(self, first: dict[str, Any], second: dict[str, Any]) -> dict[str, Any]:
        checks = [check for check in green_checks() if check["name"] != "Falsify guards"]
        checks += [first, second]
        return self.grade(checks, green_workflow_runs())

    def test_one_failed_attempt_retried_to_success_is_green_and_reported(self) -> None:
        verdict = self.retried(
            check_run("Falsify guards", suite=CI_SUITE, conclusion="failure",
                      started_at="2026-09-28T10:00:00Z", completed_at="2026-09-28T10:20:00Z"),
            check_run("Falsify guards", suite=CI_SUITE,
                      started_at="2026-09-28T10:25:00Z", completed_at="2026-09-28T10:45:00Z"),
        )
        self.assertEqual(verdict["verdict"], selector.GREEN, verdict)
        self.assertEqual(verdict["retried"], ["Falsify guards"])

    def test_a_pass_followed_by_a_failure_is_not_a_retry(self) -> None:
        verdict = self.retried(
            check_run("Falsify guards", suite=CI_SUITE,
                      started_at="2026-09-28T10:00:00Z", completed_at="2026-09-28T10:20:00Z"),
            check_run("Falsify guards", suite=CI_SUITE, conclusion="failure",
                      started_at="2026-09-28T10:25:00Z", completed_at="2026-09-28T10:45:00Z"),
        )
        self.assertEqual(verdict["verdict"], selector.DEAD, verdict)

    def test_attempts_that_overlap_are_not_a_retry(self) -> None:
        verdict = self.retried(
            check_run("Falsify guards", suite=CI_SUITE, conclusion="failure",
                      started_at="2026-09-28T10:00:00Z", completed_at="2026-09-28T10:30:00Z"),
            check_run("Falsify guards", suite=CI_SUITE,
                      started_at="2026-09-28T10:25:00Z", completed_at="2026-09-28T10:45:00Z"),
        )
        self.assertEqual(verdict["verdict"], selector.DEAD, verdict)

    def test_a_second_retry_stays_ambiguous(self) -> None:
        checks = [check for check in green_checks() if check["name"] != "Falsify guards"]
        checks += [
            check_run("Falsify guards", suite=CI_SUITE, conclusion="failure",
                      started_at="2026-09-28T10:00:00Z", completed_at="2026-09-28T10:20:00Z"),
            check_run("Falsify guards", suite=CI_SUITE, conclusion="failure",
                      started_at="2026-09-28T10:25:00Z", completed_at="2026-09-28T10:45:00Z"),
            check_run("Falsify guards", suite=CI_SUITE,
                      started_at="2026-09-28T10:50:00Z", completed_at="2026-09-28T11:10:00Z"),
        ]
        verdict = self.grade(checks, green_workflow_runs())
        self.assertEqual(verdict["verdict"], selector.DEAD, verdict)

    def test_a_red_required_context_is_dead(self) -> None:
        """59c2239c2: the Windows leg was cancelled at the job cap."""

        checks = [
            check_run(name, suite=CI_SUITE, conclusion="cancelled")
            if name == "Windows installer + vector release build"
            else run
            for name, run in ((entry["name"], entry) for entry in green_checks())
        ]
        verdict = self.grade(checks, green_workflow_runs())
        self.assertEqual(verdict["verdict"], selector.DEAD)
        self.assertIn(
            "required check not green: Windows installer + vector release build (conclusion=cancelled)",
            verdict["dead"],
        )

    def test_a_running_required_context_is_pending_not_dead(self) -> None:
        checks = green_checks()
        checks[0] = check_run(checks[0]["name"], suite=CI_SUITE, status="in_progress", conclusion=None)
        verdict = self.grade(checks, green_workflow_runs())
        self.assertEqual(verdict["verdict"], selector.PENDING)
        self.assertTrue(any("not completed" in line for line in verdict["pending"]))

    def test_a_missing_required_context_is_dead_once_its_producer_finished(self) -> None:
        checks = [entry for entry in green_checks() if entry["name"] != "cargo-deny"]
        verdict = self.grade(checks, green_workflow_runs())
        self.assertEqual(verdict["verdict"], selector.DEAD)
        self.assertTrue(any("missing required check: cargo-deny" in line for line in verdict["dead"]))

    def test_a_missing_required_context_waits_while_its_producer_runs(self) -> None:
        checks = [entry for entry in green_checks() if entry["name"] != "cargo-deny"]
        runs = [
            workflow_run(
                ".github/workflows/sast.yml",
                run_id=SAST_RUN,
                suite=SAST_SUITE,
                sha=NEWEST,
                workflow_id=251549972,
                status="in_progress",
                conclusion=None,
            )
            if run["path"] == ".github/workflows/sast.yml"
            else run
            for run in green_workflow_runs()
        ]
        verdict = self.grade(checks, runs)
        self.assertEqual(verdict["verdict"], selector.PENDING)

    def test_dco_may_be_skipped_but_nothing_else_may(self) -> None:
        checks = green_checks()
        for entry in checks:
            if entry["name"] == "Falsify guards":
                entry["conclusion"] = "skipped"
        verdict = self.grade(checks, green_workflow_runs())
        self.assertEqual(verdict["verdict"], selector.DEAD)
        self.assertTrue(any("Falsify guards" in line for line in verdict["dead"]))

    def test_a_red_ci_push_run_is_dead_even_with_green_contexts(self) -> None:
        runs = [
            workflow_run(
                ".github/workflows/ci.yml",
                run_id=CI_RUN,
                suite=CI_SUITE,
                sha=NEWEST,
                workflow_id=245803170,
                conclusion="cancelled",
            )
            if run["path"] == ".github/workflows/ci.yml"
            else run
            for run in green_workflow_runs()
        ]
        verdict = self.grade(green_checks(), runs)
        self.assertEqual(verdict["verdict"], selector.DEAD)
        self.assertTrue(any("CI push run" in line for line in verdict["dead"]))

    def test_a_red_acceptance_push_run_is_dead(self) -> None:
        """Acceptance grades main's push and nothing else, so a red one stops the cut."""

        runs = [
            workflow_run(
                ".github/workflows/acceptance.yml",
                run_id=ACCEPTANCE_RUN,
                suite=ACCEPTANCE_SUITE,
                sha=NEWEST,
                workflow_id=339594603,
                conclusion="failure",
            )
            if run["path"] == ".github/workflows/acceptance.yml"
            else run
            for run in green_workflow_runs()
        ]
        verdict = self.grade(green_checks(), runs)
        self.assertEqual(verdict["verdict"], selector.DEAD)
        self.assertTrue(any("Acceptance push run" in line for line in verdict["dead"]))

    def test_an_absent_acceptance_run_is_pending(self) -> None:
        runs = [run for run in green_workflow_runs() if run["path"] != ".github/workflows/acceptance.yml"]
        verdict = self.grade(green_checks(), runs)
        self.assertEqual(verdict["verdict"], selector.PENDING)
        self.assertTrue(any("no Acceptance push run" in line for line in verdict["pending"]))

    def test_a_context_from_another_app_is_dead(self) -> None:
        checks = green_checks()
        checks[0] = check_run(checks[0]["name"], suite=CI_SUITE, app=(99, "impostor"))
        verdict = self.grade(checks, green_workflow_runs())
        self.assertEqual(verdict["verdict"], selector.DEAD)
        self.assertTrue(any("claimed by another app" in line for line in verdict["dead"]))

    def test_a_candidate_branch_twin_is_discounted_not_counted(self) -> None:
        """57228997f carried a second gitleaks from the candidate branch's own push."""

        checks = green_checks()
        checks.append(check_run("gitleaks (full history)", suite=91_000_000_099))
        runs = green_workflow_runs()
        runs.append(
            workflow_run(
                ".github/workflows/secret-scan.yml",
                run_id=33_000_000_099,
                suite=91_000_000_099,
                sha=NEWEST,
                workflow_id=293452372,
                branch=f"release/v{VERSION}-candidate",
            )
        )
        verdict = self.grade(checks, runs)
        self.assertEqual(verdict["verdict"], selector.GREEN, verdict)

    def test_a_second_producer_of_a_required_name_on_main_is_dead(self) -> None:
        checks = green_checks()
        checks.append(check_run("Falsify guards", suite=91_000_000_098))
        runs = green_workflow_runs()
        runs.append(
            workflow_run(
                ".github/workflows/impostor.yml",
                run_id=33_000_000_098,
                suite=91_000_000_098,
                sha=NEWEST,
                workflow_id=999,
            )
        )
        verdict = self.grade(checks, runs)
        self.assertEqual(verdict["verdict"], selector.DEAD)
        self.assertTrue(any("provenance mismatch" in line for line in verdict["dead"]))

    def test_a_truncated_listing_refuses_rather_than_reading_as_green(self) -> None:
        checks = green_checks()
        with self.assertRaises(selector.SelectionError) as caught:
            selector.grade_sha(
                NEWEST,
                pages(checks, "check_runs", total=len(checks) + 5),
                pages(green_workflow_runs(), "workflow_runs"),
            )
        self.assertIn("incomplete", str(caught.exception))

    def test_a_loose_sha_refuses(self) -> None:
        with self.assertRaises(selector.SelectionError):
            selector.grade_sha("abc", pages([], "check_runs"), pages([], "workflow_runs"))


# ---------------------------------------------------------------------------
# The decision.
# ---------------------------------------------------------------------------


class ReadRcBuildsTests(unittest.TestCase):
    """The read that decides what `_usable` is allowed to see.

    `_usable` never sees an expired artifact, because the read filters them out
    before the judgment. That filter is the whole of the expiry refusal, so it
    is asserted here rather than assumed by a fixture that hands over an empty
    list.
    """

    BRANCH = f"release/v{VERSION}-candidate"

    def read(self, runs: list[dict[str, Any]], artifacts: dict[int, list[dict[str, Any]]]):
        def fetch(endpoint: str) -> Any:
            if endpoint == selector.rc_build_runs_endpoint(REPO, self.BRANCH, 1):
                return {"total_count": len(runs), "workflow_runs": runs}
            for run_id, listing in artifacts.items():
                if endpoint == selector.run_artifacts_endpoint(REPO, run_id, 1):
                    return {"total_count": len(listing), "artifacts": listing}
            raise AssertionError(f"unexpected endpoint {endpoint}")

        return selector.read_rc_builds(fetch, REPO, self.BRANCH)

    def run_row(self, run_id: int, *, status: str = "completed", conclusion: str | None = "success"):
        return {
            "id": run_id,
            "head_sha": MIDDLE,
            "head_branch": self.BRANCH,
            "event": "workflow_dispatch",
            "status": status,
            "conclusion": conclusion,
            "created_at": "2026-09-02T22:52:23Z",
        }

    def test_every_live_archive_is_read_and_the_run_is_usable(self) -> None:
        listing = [{"name": name, "expired": False} for name in selector.RC_RELEASE_ARTIFACTS]
        builds = self.read([self.run_row(1)], {1: listing})
        self.assertEqual(builds[0]["artifacts"], sorted(selector.RC_RELEASE_ARTIFACTS))
        self.assertTrue(selector._usable(builds[0]))

    def test_an_expired_archive_is_dropped_and_costs_the_run_its_usability(self) -> None:
        listing = [
            {"name": name, "expired": name == "kin-macos-aarch64"}
            for name in selector.RC_BUILD_ARTIFACTS
        ]
        builds = self.read([self.run_row(1)], {1: listing})
        self.assertNotIn("kin-macos-aarch64", builds[0]["artifacts"])
        self.assertFalse(selector._usable(builds[0]))

    def test_an_unsuccessful_run_costs_no_artifact_read_and_is_not_usable(self) -> None:
        for status, conclusion in (("completed", "failure"), ("in_progress", None)):
            with self.subTest(status=status, conclusion=conclusion):
                builds = self.read([self.run_row(1, status=status, conclusion=conclusion)], {})
                self.assertEqual(builds[0]["artifacts"], [])
                self.assertFalse(selector._usable(builds[0]))


class JudgeTests(unittest.TestCase):
    def assertDecision(self, decision: Any, expected: str, needle: str = "") -> None:
        self.assertEqual(decision.decision, expected, decision.reason)
        if needle:
            self.assertIn(needle, decision.reason)

    def test_a_finalized_tag_stands_the_cut_down(self) -> None:
        """Today's tree: main carries 0.6.4 and v0.6.4 exists, so there is nothing to cut."""

        grader = Grader({})
        decision = selector.judge(snapshot(tag_exists=True), grader)
        self.assertDecision(decision, selector.STAND_DOWN, "already exists")
        self.assertEqual(grader.calls, [], "a tagged version must cost no grading")

    def test_a_fully_evidenced_candidate_stands_the_cut_down(self) -> None:
        grader = Grader({})
        decision = selector.judge(
            snapshot(evidence={MIDDLE: [selector.PREFLIGHT_RECORD, selector.STRANGER_RECORD]}),
            grader,
        )
        self.assertDecision(decision, selector.STAND_DOWN, "awaits the mint")
        self.assertEqual(decision.candidate, MIDDLE)
        self.assertEqual(decision.stranger_state, selector.STRANGER_COMPLETE)
        self.assertEqual(grader.calls, [])

    def test_a_half_evidenced_candidate_asks_for_the_stranger_and_names_its_archive(self) -> None:
        grader = Grader({})
        decision = selector.judge(
            snapshot(
                candidate=MIDDLE,
                evidence={MIDDLE: [selector.PREFLIGHT_RECORD]},
                rc_builds=[rc_build(MIDDLE)],
            ),
            grader,
        )
        self.assertDecision(decision, selector.STRANGER, "will tell that release from a measured one")
        # The rc-build run travels with the decision, because the stranger has
        # to run on the very bytes the published preflight judged.
        self.assertEqual(decision.rc_run, 34_000_000_001)
        self.assertEqual(decision.stranger_state, selector.STRANGER_PENDING)
        command = decision.details["stranger_command"]
        self.assertIn("bin/kin-stranger prepare", command)
        self.assertIn("--arms green,brown,vcs", command)
        self.assertIn(f"--candidate-sha {MIDDLE}", command)
        self.assertEqual(grader.calls, [], "a filed preflight must cost no grading")

    def test_a_half_evidenced_candidate_whose_archives_expired_refuses(self) -> None:
        """The record names an archive sha256; a rebuild is not guaranteed to reproduce it."""

        decision = selector.judge(
            snapshot(candidate=MIDDLE, evidence={MIDDLE: [selector.PREFLIGHT_RECORD]}, rc_builds=[]),
            Grader({}),
        )
        self.assertDecision(decision, selector.REFUSE, "no rc-build still holds the archives it judged")
        self.assertEqual(decision.candidate, MIDDLE)
        # The refusal is about the cut, not about the mint. Since 2026-09-04 a
        # preflight-only sha is mint-eligible, so this candidate is going to be
        # tagged with first contact permanently unrecordable, and the reason a
        # human reads has to say that rather than implying the release is held.
        self.assertIn("the mint may still tag it", decision.reason)
        self.assertEqual(decision.stranger_state, selector.STRANGER_PENDING)

    def test_an_expired_rc_build_is_not_a_usable_archive_source(self) -> None:
        stale = rc_build(MIDDLE, artifacts=[])
        decision = selector.judge(
            snapshot(candidate=MIDDLE, evidence={MIDDLE: [selector.PREFLIGHT_RECORD]}, rc_builds=[stale]),
            Grader({}),
        )
        self.assertDecision(decision, selector.REFUSE, "no rc-build still holds")

    def test_one_expired_archive_costs_the_run_its_usability(self) -> None:
        """A partially aged-out run cannot feed the leg whose archive is gone."""

        survivor = rc_build(MIDDLE, artifacts=["kin-linux-aarch64", "kin-linux-x86_64"])
        self.assertFalse(selector._usable(survivor))
        grader = Grader({MIDDLE: green_grade(MIDDLE)})
        decision = selector.judge(snapshot(candidate=MIDDLE, rc_builds=[survivor]), grader)
        self.assertDecision(decision, selector.ARM)

    def test_both_records_beat_the_stranger_decision(self) -> None:
        """Once stranger.env lands the candidate belongs to the mint, not to another run."""

        decision = selector.judge(
            snapshot(
                candidate=MIDDLE,
                evidence={MIDDLE: [selector.PREFLIGHT_RECORD, selector.STRANGER_RECORD]},
                rc_builds=[rc_build(MIDDLE)],
            ),
            Grader({}),
        )
        self.assertDecision(decision, selector.STAND_DOWN, "awaits the mint")

    def test_a_preflight_only_sha_is_mint_eligible_and_stands_the_cut_down(self) -> None:
        """The founder decision of 2026-09-03: a missing stranger must not hold a release.

        release-tag.yml lists any sha carrying `preflight.json` and runs its
        proof gate with `KIN_RELEASE_REQUIRE=preflight`, so this sha is already
        a tag. The cut arming a newer candidate for the same version would only
        race that tag.
        """

        grader = Grader({})
        decision = selector.judge(
            snapshot(evidence={MIDDLE: [selector.PREFLIGHT_RECORD]}),
            grader,
        )
        self.assertDecision(decision, selector.STAND_DOWN, "awaits the mint")
        self.assertEqual(decision.candidate, MIDDLE)
        self.assertEqual(decision.stranger_state, selector.STRANGER_PENDING)
        self.assertIn("promoted carrying a notice that names that gap", decision.reason)
        self.assertEqual(decision.details["mintable"], [MIDDLE])
        self.assertEqual(grader.calls, [], "a filed preflight must cost no grading")

    def test_the_stranger_still_runs_for_a_mint_eligible_candidate(self) -> None:
        """Mint-eligible is not stranger-optional for the cut.

        `release-cut.yml`'s stranger job is gated on
        `needs.select.outputs.decision == 'stranger'`. If a preflight-only sha
        stood the cut down before reaching the candidate block, that job would
        never run again and "nice to have" would quietly become "deleted".
        """

        grader = Grader({})
        decision = selector.judge(
            snapshot(
                candidate=MIDDLE,
                evidence={MIDDLE: [selector.PREFLIGHT_RECORD]},
                rc_builds=[rc_build(MIDDLE)],
            ),
            grader,
        )
        self.assertDecision(decision, selector.STRANGER)
        self.assertEqual(decision.candidate, MIDDLE)
        self.assertEqual(decision.details["mintable"], [MIDDLE])
        self.assertEqual(grader.calls, [])

    def test_the_newest_green_sha_is_armed(self) -> None:
        grader = Grader({NEWEST: green_grade(NEWEST)})
        decision = selector.judge(snapshot(), grader)
        self.assertDecision(decision, selector.ARM)
        self.assertEqual(decision.candidate, NEWEST)
        self.assertEqual(decision.move, selector.MOVE_CREATE)
        self.assertEqual(decision.branch, f"release/v{VERSION}-candidate")
        self.assertEqual(grader.calls, [NEWEST], "a green newest sha must cost exactly one grading")

    def test_an_older_green_sha_behind_a_red_newest_is_armed(self) -> None:
        red = with_duplicate(green_grade(NEWEST), "Falsify guards", CI_SUITE)
        grader = Grader({NEWEST: red, MIDDLE: green_grade(MIDDLE)})
        decision = selector.judge(snapshot(), grader)
        self.assertDecision(decision, selector.ARM)
        self.assertEqual(decision.candidate, MIDDLE)
        self.assertEqual(grader.calls, [NEWEST, MIDDLE])
        self.assertIn(NEWEST, decision.details["disqualified"])

    def test_a_pending_newest_is_skipped_rather_than_waited_for(self) -> None:
        """On a busy night the newest sha is always mid-grading; waiting never converges."""

        pending = green_grade(NEWEST)
        pending["check_runs"][0]["check_runs"][0]["status"] = "in_progress"
        pending["check_runs"][0]["check_runs"][0]["conclusion"] = None
        grader = Grader({NEWEST: pending, MIDDLE: green_grade(MIDDLE)})
        decision = selector.judge(snapshot(), grader)
        self.assertDecision(decision, selector.ARM)
        self.assertEqual(decision.candidate, MIDDLE)
        self.assertIn(NEWEST, decision.details["skipped_pending"])

    def test_no_green_sha_anywhere_refuses_and_names_the_newest(self) -> None:
        dead = {
            sha: with_duplicate(green_grade(sha), "Falsify guards", CI_SUITE)
            for sha in (NEWEST, MIDDLE, OLDEST)
        }
        decision = selector.judge(snapshot(), Grader(dead))
        self.assertDecision(decision, selector.REFUSE, "no reviewed main commit")
        self.assertIn(NEWEST, decision.reason)
        self.assertIn("ambiguous required check: Falsify guards", decision.reason)

    def test_every_sha_pending_stands_down_rather_than_refusing(self) -> None:
        waiting = {}
        for sha in (NEWEST, MIDDLE, OLDEST):
            grade = green_grade(sha)
            grade["check_runs"][0]["check_runs"][0]["status"] = "queued"
            grade["check_runs"][0]["check_runs"][0]["conclusion"] = None
            waiting[sha] = grade
        decision = selector.judge(snapshot(), Grader(waiting))
        self.assertDecision(decision, selector.STAND_DOWN, "still being graded")

    def test_a_green_candidate_with_no_rc_build_is_armed_without_moving_the_branch(self) -> None:
        grader = Grader({MIDDLE: green_grade(MIDDLE)})
        decision = selector.judge(snapshot(candidate=MIDDLE), grader)
        self.assertDecision(decision, selector.ARM)
        self.assertEqual(decision.candidate, MIDDLE)
        self.assertEqual(decision.move, selector.MOVE_NONE)

    def test_a_running_rc_build_stands_down(self) -> None:
        grader = Grader({MIDDLE: green_grade(MIDDLE)})
        decision = selector.judge(
            snapshot(candidate=MIDDLE, rc_builds=[rc_build(MIDDLE, status="in_progress", conclusion=None)]),
            grader,
        )
        self.assertDecision(decision, selector.STAND_DOWN, "in_progress")
        self.assertEqual(decision.rc_run, 34_000_000_001)

    def test_a_successful_rc_build_holding_every_archive_is_proof(self) -> None:
        grader = Grader({MIDDLE: green_grade(MIDDLE)})
        decision = selector.judge(snapshot(candidate=MIDDLE, rc_builds=[rc_build(MIDDLE)]), grader)
        self.assertDecision(decision, selector.PROOF)
        self.assertEqual(decision.rc_run, 34_000_000_001)
        self.assertIn("bin/kin-stranger run", decision.details["stranger_command"])

    def test_an_rc_build_missing_an_archive_is_not_proof(self) -> None:
        """The preflight downloads one archive per row; a run short of one cannot feed it."""

        partial = rc_build(MIDDLE, artifacts=["kin-macos-aarch64"])
        grader = Grader({MIDDLE: green_grade(MIDDLE)})
        decision = selector.judge(snapshot(candidate=MIDDLE, rc_builds=[partial]), grader)
        self.assertDecision(decision, selector.ARM)
        self.assertEqual(decision.candidate, MIDDLE)

    def test_a_failed_rc_build_holding_every_archive_is_not_proof(self) -> None:
        """A red run's artifacts are not evidence; only a successful build is."""

        failed = rc_build(MIDDLE, conclusion="failure")
        grader = Grader({MIDDLE: green_grade(MIDDLE)})
        decision = selector.judge(snapshot(candidate=MIDDLE, rc_builds=[failed]), grader)
        self.assertDecision(decision, selector.ARM)
        self.assertEqual(decision.candidate, MIDDLE)

    def test_the_deadlocked_shape_of_2026_09_02_is_usable(self) -> None:
        """Regression: run 33692452573 for febf5d851, the shape that burned the loop.

        The rc-build succeeded and carried its archives under their bare names,
        none expired. The selector asked for `kin-release-preflight-` prefixed
        names, which only release-cut.yml's own preflight job ever uploads and
        only on the `proof` decision this reading could never produce, so the
        loop armed a fresh build every cycle and killed the candidate at the
        attempt limit. That run carried three archives; a run is usable now only
        when it holds every artifact release.yml ships, under the same bare
        names, so the three alone no longer are.
        """

        then = rc_build(
            MIDDLE,
            run_id=33_692_452_573,
            artifacts=["kin-macos-aarch64", "kin-linux-x86_64", "kin-linux-aarch64"],
        )
        self.assertFalse(selector._usable(then), then["artifacts"])
        live = rc_build(MIDDLE, run_id=33_692_452_573, artifacts=list(selector.RC_RELEASE_ARTIFACTS))
        self.assertTrue(selector._usable(live), live["artifacts"])
        grader = Grader({MIDDLE: green_grade(MIDDLE)})
        decision = selector.judge(snapshot(candidate=MIDDLE, rc_builds=[live]), grader)
        self.assertDecision(decision, selector.PROOF, "still holds its archives")
        self.assertEqual(decision.rc_run, 33_692_452_573)

    def test_a_run_carrying_only_leg_record_names_is_not_usable(self) -> None:
        """The inverse control: leg-record names alone are not candidate archives.

        release-cut.yml uploads `kin-release-preflight-<artifact>` into its OWN
        run. If those names ever appear on an rc-build run, they are still not
        the archives the preflight downloads by bare name.
        """

        legs = rc_build(MIDDLE, artifacts=[f"kin-release-preflight-{name}" for name in selector.RC_BUILD_ARTIFACTS])
        self.assertFalse(selector._usable(legs))

    def test_exhausted_rc_build_attempts_kill_the_candidate_and_pick_the_next(self) -> None:
        spent = [
            rc_build(MIDDLE, run_id=1, conclusion="failure", created_at="2026-09-02T19:00:00Z"),
            rc_build(MIDDLE, run_id=2, conclusion="failure", created_at="2026-09-02T20:00:00Z"),
        ]
        grader = Grader({MIDDLE: green_grade(MIDDLE), NEWEST: green_grade(NEWEST)})
        decision = selector.judge(snapshot(candidate=MIDDLE, rc_builds=spent), grader)
        self.assertDecision(decision, selector.ARM)
        self.assertEqual(decision.candidate, NEWEST)
        self.assertEqual(decision.move, selector.MOVE_FAST_FORWARD)
        self.assertTrue(any("attempts exhausted" in note for note in decision.details["notes"]))

    def test_a_dead_candidate_is_replaced_by_the_newest_green_sha(self) -> None:
        dead = with_duplicate(green_grade(MIDDLE), "cargo-deny", SAST_SUITE)
        grader = Grader({MIDDLE: dead, NEWEST: green_grade(NEWEST)})
        decision = selector.judge(snapshot(candidate=MIDDLE), grader)
        self.assertDecision(decision, selector.ARM)
        self.assertEqual(decision.candidate, NEWEST)
        self.assertEqual(decision.move, selector.MOVE_FAST_FORWARD)

    def test_a_dead_candidate_replaced_by_an_older_sha_resets_rather_than_fast_forwards(self) -> None:
        dead = with_duplicate(green_grade(MIDDLE), "cargo-deny", SAST_SUITE)
        newest_dead = with_duplicate(green_grade(NEWEST), "cargo-deny", SAST_SUITE)
        grader = Grader({MIDDLE: dead, NEWEST: newest_dead, OLDEST: green_grade(OLDEST)})
        decision = selector.judge(snapshot(candidate=MIDDLE), grader)
        self.assertDecision(decision, selector.ARM)
        self.assertEqual(decision.candidate, OLDEST)
        self.assertEqual(decision.move, selector.MOVE_RESET)

    def test_a_candidate_off_the_version_range_refuses(self) -> None:
        decision = selector.judge(snapshot(candidate=FOREIGN), Grader({}))
        self.assertDecision(decision, selector.REFUSE, "is not a reviewed main commit")

    def test_a_pending_candidate_stands_down_without_replacing_it(self) -> None:
        pending = green_grade(MIDDLE)
        pending["check_runs"][0]["check_runs"][0]["status"] = "in_progress"
        pending["check_runs"][0]["check_runs"][0]["conclusion"] = None
        grader = Grader({MIDDLE: pending})
        decision = selector.judge(snapshot(candidate=MIDDLE), grader)
        self.assertDecision(decision, selector.STAND_DOWN, "being graded again")
        self.assertEqual(grader.calls, [MIDDLE])

    def test_malformed_snapshots_refuse(self) -> None:
        for broken, needle in (
            (snapshot(version="0.6"), "semver"),
            (snapshot(range=[]), "non-empty"),
            (snapshot(range=[NEWEST, NEWEST]), "repeats"),
            (snapshot(range=["nope"]), "40-character"),
            (snapshot(candidate="nope"), "not a 40-character"),
        ):
            with self.assertRaises(selector.SelectionError):
                selector.judge(broken, Grader({}))
        with self.assertRaises(selector.SelectionError):
            selector.judge("not an object", Grader({}))


# ---------------------------------------------------------------------------
# Inheriting an export pull request's grade: the real PR 1848 / 08d48c27 pair.
# ---------------------------------------------------------------------------


REAL = json.loads(REAL_FIXTURE_PATH.read_text(encoding="utf-8"))
# 08d48c27 staged 0.8.2, which was cancelled and is now retired, so the cut
# stands down on it whatever it carries. The judgments below are about grading
# and RC Build attempts, not about the version, so they judge the same real
# commit as the release it would now be: 0.8.4.
RELEASABLE = "0.8.4"
REAL_MAIN = REAL["main"]["sha"]
REAL_HEAD = REAL["head"]["sha"]
REAL_BASE = REAL["pull"]["base"]["sha"]
REAL_TREE = REAL["main"]["tree"]
REAL_REF = REAL["pull"]["head"]["ref"]
REAL_CI_RUN = 38_181_962_218
REAL_ACCEPTANCE_RUN = 38_181_961_757
# Fifteen minutes after PR 1848 landed at 00:15:52Z. Main's own CI and
# Acceptance push runs were queued at 00:15:55Z and concluded at 01:42:19Z and
# 00:53:50Z, so this is the window inheritance exists for.
AT_LANDING = "2026-10-12T00:30:00Z"


def main_listings_at(moment: str | None = None) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """Main's real listings as they stood at `moment`, or as read when None.

    A run or check-run that had not started is left out, and one that had not
    finished reads in progress, which is what the API answered at that time.
    """

    checks, runs = [], []
    for run in copy.deepcopy(REAL["main"]["workflow_runs"]):
        if moment is not None and run["created_at"] > moment:
            continue
        if moment is not None and run["status"] == "completed" and run["updated_at"] > moment:
            run.update(status="in_progress", conclusion=None)
        runs.append(run)
    for check in copy.deepcopy(REAL["main"]["check_runs"]):
        if moment is not None and (check["started_at"] or "") > moment:
            continue
        if moment is not None and check["completed_at"] and check["completed_at"] > moment:
            check.update(status="in_progress", conclusion=None, completed_at=None)
        checks.append(check)
    return checks, runs


def main_pages_at(moment: str | None = None) -> dict[str, Any]:
    checks, runs = main_listings_at(moment)
    return grade_pages(checks, runs)


def real_inheritance_read() -> dict[str, Any]:
    """The read `read_inheritance` makes for 08d48c27, from the real listings."""

    head = REAL["head"]
    return copy.deepcopy(
        {
            "sha": REAL_MAIN,
            "commit": {"tree": REAL_TREE, "parents": REAL["main"]["parents"]},
            "associated_pulls": REAL["associated_pulls"],
            "pull": REAL["pull"],
            "head": {"sha": head["sha"], "tree": head["tree"], "base_is_ancestor": head["base_is_ancestor"]},
            "check_runs": pages(head["check_runs"], "check_runs"),
            "workflow_runs": pages(head["workflow_runs"], "workflow_runs"),
        }
    )


def head_check(read: dict[str, Any], name: str) -> dict[str, Any]:
    matches = [check for check in read["check_runs"][0]["check_runs"] if check["name"] == name]
    if len(matches) != 1:
        raise AssertionError(f"the real head carries {len(matches)} {name!r} check-runs, not one")
    return matches[0]


def head_workflow(read: dict[str, Any], path: str, event: str) -> dict[str, Any]:
    matches = [
        run
        for run in read["workflow_runs"][0]["workflow_runs"]
        if run["path"] == path and run["event"] == event
    ]
    if len(matches) != 1:
        raise AssertionError(f"the real head carries {len(matches)} {path} {event} runs, not one")
    return matches[0]


def grade_at_landing(read: Any, moment: str | None = AT_LANDING) -> dict[str, Any]:
    """Grade the real landing with export inheritance switched on for the test.

    It is off in the product (EXPORT_INHERITANCE). These cases keep its binding
    and grading honest for the day it is switched back on.
    """

    main = main_pages_at(moment)
    return selector.grade_candidate(
        REAL_MAIN, main["check_runs"], main["workflow_runs"], read, REPO, export_inheritance=True
    )


class RealExportHeadTests(unittest.TestCase):
    """The PR head graded on its own, by the rules the push grade applies."""

    def grade(self, read: dict[str, Any]) -> dict[str, Any]:
        return selector.grade_sha(
            REAL_HEAD,
            read["check_runs"],
            read["workflow_runs"],
            provenance=selector.Provenance(REAL_HEAD, REAL_REF, selector.KIND_EXPORT),
        )

    def test_the_real_head_is_green_on_its_own_runs(self) -> None:
        verdict = self.grade(real_inheritance_read())
        self.assertEqual(verdict["verdict"], selector.GREEN, verdict)
        self.assertEqual(verdict["runs"], {"CI": REAL_CI_RUN, "Acceptance": REAL_ACCEPTANCE_RUN})

    def test_the_skipped_pull_request_twin_of_gitleaks_is_not_evidence(self) -> None:
        """secret-scan.yml publishes the name twice at the head: skipped, then green.

        The skipped copy is the pull_request run the workflow's own `if:` skips
        for a same-repository branch; the green one is the branch push. Reading
        both as evidence would make every export head ambiguous.
        """

        read = real_inheritance_read()
        names = [check["conclusion"] for check in read["check_runs"][0]["check_runs"]
                 if check["name"] == "gitleaks (full history)"]
        self.assertEqual(sorted(names), ["skipped", "success"])
        self.assertEqual(self.grade(read)["verdict"], selector.GREEN)
        # Under the other event the green copy is no longer evidence, so the
        # rule is about the event and not merely about picking a green one.
        head_workflow(read, ".github/workflows/secret-scan.yml", "push")["event"] = "pull_request"
        head_workflow_runs = read["workflow_runs"][0]["workflow_runs"]
        self.assertEqual(sum(run["path"].endswith("secret-scan.yml") for run in head_workflow_runs), 2)
        self.assertEqual(self.grade(read)["verdict"], selector.DEAD)

    def test_the_acceptance_job_must_have_run_on_the_head(self) -> None:
        read = real_inheritance_read()
        head_check(read, "Product Acceptance")["conclusion"] = "skipped"
        verdict = self.grade(read)
        self.assertEqual(verdict["verdict"], selector.DEAD)
        self.assertIn("required check not green: Product Acceptance (conclusion=skipped)", verdict["dead"])

    def test_the_head_is_graded_on_its_own_branch_and_sha(self) -> None:
        read = real_inheritance_read()
        head_workflow(read, ".github/workflows/ci.yml", "pull_request")["head_branch"] = "feature/lookalike"
        verdict = self.grade(read)
        self.assertNotEqual(verdict["verdict"], selector.GREEN)
        self.assertIn(f"no CI pull_request run on {REAL_REF} at this sha yet", verdict["pending"])


class RealInheritanceTests(unittest.TestCase):
    def assertFallsBack(self, read: dict[str, Any], needle: str) -> dict[str, Any]:
        verdict = grade_at_landing(read)
        self.assertEqual(verdict["evidence"]["source"], selector.EVIDENCE_PUSH, verdict["evidence"])
        self.assertEqual(verdict["verdict"], selector.PENDING, verdict)
        self.assertIn("refused", verdict["evidence"]["inheritance"])
        self.assertIn(needle, verdict["evidence"]["inheritance"])
        return verdict

    def test_the_landing_inherits_the_export_heads_grade(self) -> None:
        """08d48c27 fifteen minutes after landing, with main's CI still running."""

        verdict = grade_at_landing(real_inheritance_read())
        self.assertEqual(verdict["verdict"], selector.GREEN, verdict)
        evidence = verdict["evidence"]
        self.assertEqual(evidence["source"], selector.EVIDENCE_EXPORT)
        self.assertEqual(evidence["pull"], 1848)
        self.assertEqual(evidence["head_sha"], REAL_HEAD)
        self.assertEqual(evidence["head_ref"], REAL_REF)
        self.assertEqual(evidence["base_sha"], REAL_BASE)
        self.assertEqual(evidence["tree"], REAL_TREE)
        self.assertEqual(evidence["runs"], {"CI": REAL_CI_RUN, "Acceptance": REAL_ACCEPTANCE_RUN})
        # What main's own grade was waiting on stays in the record, so the
        # decision says what it did not wait for.
        self.assertEqual(evidence["main_push"]["verdict"], selector.PENDING)
        self.assertIn("CI push run 38187338698 is in_progress", evidence["main_push"]["pending"])

    def test_the_squash_subject_locates_the_pull_request_while_the_index_lags(self) -> None:
        """The landing push can wake the cut before GitHub indexes the merge."""

        read = real_inheritance_read()
        read["associated_pulls"] = []
        read["subject_pull"] = 1848
        verdict = grade_at_landing(read)
        self.assertEqual(verdict["verdict"], selector.GREEN, verdict)
        self.assertEqual(verdict["evidence"]["pull"], 1848)

        # The subject only locates. Another pull request claiming the commit,
        # or a subject naming one that did not merge as it, still refuses.
        claimed = real_inheritance_read()
        claimed["subject_pull"] = 1848
        claimed["associated_pulls"] = [{"number": 1849, "merge_commit_sha": REAL_MAIN}]
        self.assertEqual(grade_at_landing(claimed)["verdict"], selector.PENDING)

    def test_main_graded_without_inheritance_is_pending_at_landing(self) -> None:
        """The control: this is the 86 to 93 minutes inheritance takes off the path."""

        verdict = grade_at_landing(None)
        self.assertEqual(verdict["verdict"], selector.PENDING)
        self.assertEqual(verdict["evidence"], {"source": selector.EVIDENCE_PUSH, "sha": REAL_MAIN})

    def test_main_green_on_its_own_is_graded_as_before_and_reads_nothing_more(self) -> None:
        def unread() -> dict[str, Any]:
            raise AssertionError("a green push grade must not spend the inheritance reads")

        verdict = grade_at_landing(unread, moment=None)
        self.assertEqual(verdict["verdict"], selector.GREEN, verdict)
        self.assertEqual(verdict["evidence"]["source"], selector.EVIDENCE_PUSH)
        self.assertEqual(verdict["runs"], {"CI": 38_187_338_698, "Acceptance": 38_187_338_482})

    def test_an_unreadable_inheritance_read_falls_back_rather_than_failing_the_cut(self) -> None:
        def unreadable() -> dict[str, Any]:
            raise selector.SelectionError("gh api repos/kinlab-ai/kin/pulls/1848 failed: HTTP 502")

        verdict = grade_at_landing(unreadable)
        self.assertEqual(verdict["verdict"], selector.PENDING)
        self.assertIn("unreadable, so not considered", verdict["evidence"]["inheritance"])
        self.assertIn("HTTP 502", verdict["evidence"]["inheritance"])

    def test_a_tree_that_differs_falls_back(self) -> None:
        read = real_inheritance_read()
        read["head"]["tree"] = "f" * 40
        self.assertFallsBack(read, f"{REAL_MAIN}'s tree {REAL_TREE} is not #1848's head tree")

    def test_a_commit_that_is_not_the_squash_merge_falls_back(self) -> None:
        read = real_inheritance_read()
        read["pull"]["merge_commit_sha"] = "e" * 40
        self.assertFallsBack(read, f"#1848 is not merged as {REAL_MAIN}")

        unmerged = real_inheritance_read()
        unmerged["pull"]["merged"] = False
        self.assertFallsBack(unmerged, f"#1848 is not merged as {REAL_MAIN}")

        unnamed = real_inheritance_read()
        unnamed["associated_pulls"] = [{"number": 1848, "merge_commit_sha": "e" * 40}]
        self.assertFallsBack(unnamed, f"no pull request names {REAL_MAIN} as its merge commit")

        named_elsewhere = real_inheritance_read()
        named_elsewhere["associated_pulls"] = []
        named_elsewhere["subject_pull"] = 1848
        named_elsewhere["pull"]["merge_commit_sha"] = "e" * 40
        self.assertFallsBack(named_elsewhere, f"#1848 is not merged as {REAL_MAIN}")

        twice = real_inheritance_read()
        twice["associated_pulls"].append({"number": 1849, "merge_commit_sha": REAL_MAIN})
        self.assertFallsBack(twice, f"2 pull requests name {REAL_MAIN} as their merge commit")

        merge_commit = real_inheritance_read()
        merge_commit["commit"]["parents"] = [REAL_BASE, REAL_HEAD]
        self.assertFallsBack(merge_commit, "has 2 parents, so it is not the squash of #1848")

    def test_a_parent_that_is_not_the_base_falls_back(self) -> None:
        read = real_inheritance_read()
        read["commit"]["parents"] = ["9" * 40]
        self.assertFallsBack(read, f"{REAL_MAIN}'s parent {'9' * 40} is not #1848's base {REAL_BASE}")

    def test_a_base_that_is_not_the_heads_ancestor_falls_back(self) -> None:
        """Then a pull_request run could have tested the head merged with other commits."""

        read = real_inheritance_read()
        read["head"]["base_is_ancestor"] = False
        self.assertFallsBack(read, "is not an ancestor of its head")

    def test_a_head_that_reads_back_differently_falls_back(self) -> None:
        read = real_inheritance_read()
        read["head"]["sha"] = "8" * 40
        self.assertFallsBack(read, f"#1848's head read back as {'8' * 40}, not {REAL_HEAD}")

    def test_only_an_export_branch_of_this_repository_is_admitted(self) -> None:
        other_branch = real_inheritance_read()
        other_branch["pull"]["head"]["ref"] = "chore/lane-x"
        self.assertFallsBack(other_branch, "is not an export branch")

        fork = real_inheritance_read()
        fork["pull"]["head"]["repo"]["full_name"] = "someone/kin"
        self.assertFallsBack(fork, "#1848's head is not a branch of kinlab-ai/kin")

        elsewhere = real_inheritance_read()
        elsewhere["pull"]["base"]["ref"] = "release/v0.8.2"
        self.assertFallsBack(elsewhere, "#1848 does not target kinlab-ai/kin main")

    def test_a_red_context_on_the_head_falls_back(self) -> None:
        read = real_inheritance_read()
        head_check(read, "Falsify guards")["conclusion"] = "failure"
        self.assertFallsBack(read, "required check not green: Falsify guards (conclusion=failure)")

    def test_a_duplicated_context_on_the_head_falls_back(self) -> None:
        read = real_inheritance_read()
        twin = copy.deepcopy(head_check(read, "cargo-deny"))
        twin["id"] += 1
        read["check_runs"][0]["check_runs"].append(twin)
        read["check_runs"][0]["total_count"] += 1
        self.assertFallsBack(read, "ambiguous required check: cargo-deny (2 check-runs under one provenance")

    def test_a_context_from_the_wrong_app_falls_back(self) -> None:
        read = real_inheritance_read()
        head_check(read, "Check & Test (ubuntu-latest)")["app"] = {"id": 99, "slug": "impostor"}
        self.assertFallsBack(
            read, "required check name claimed by another app: Check & Test (ubuntu-latest) ('impostor')"
        )

    def test_a_context_from_the_wrong_workflow_falls_back(self) -> None:
        read = real_inheritance_read()
        head_workflow(read, ".github/workflows/sast.yml", "pull_request")["workflow_id"] = 999
        self.assertFallsBack(read, "required check workflow provenance mismatch: cargo-deny")

    def test_a_head_still_being_graded_falls_back(self) -> None:
        read = real_inheritance_read()
        head_workflow(read, ".github/workflows/acceptance.yml", "pull_request").update(
            status="in_progress", conclusion=None
        )
        self.assertFallsBack(read, f"Acceptance pull_request run {REAL_ACCEPTANCE_RUN} is in_progress")

    def test_a_red_on_mains_own_runs_is_never_overridden(self) -> None:
        """The same tree failing on main is a verdict, whatever the pull request said."""

        main = main_pages_at(AT_LANDING)
        for check in main["check_runs"][0]["check_runs"]:
            if check["name"] == "cargo-deny":
                check["conclusion"] = "failure"
        verdict = selector.grade_candidate(
            REAL_MAIN, main["check_runs"], main["workflow_runs"], real_inheritance_read(), REPO,
            export_inheritance=True,
        )
        self.assertEqual(verdict["verdict"], selector.DEAD)
        self.assertIn("not considered", verdict["evidence"]["inheritance"])
        self.assertIn("cargo-deny", verdict["evidence"]["inheritance"])

    def test_a_second_writer_on_main_is_never_overridden(self) -> None:
        main = main_pages_at(AT_LANDING)
        for check in main["check_runs"][0]["check_runs"]:
            if check["name"] == "gitleaks (full history)":
                check["app"] = {"id": 99, "slug": "impostor"}
        verdict = selector.grade_candidate(
            REAL_MAIN, main["check_runs"], main["workflow_runs"], real_inheritance_read(), REPO,
            export_inheritance=True,
        )
        self.assertEqual(verdict["verdict"], selector.DEAD)
        self.assertIn("claimed by another app", verdict["evidence"]["inheritance"])

    def test_a_cancelled_main_run_is_no_verdict_and_inheritance_stands(self) -> None:
        main = main_pages_at(AT_LANDING)
        for run in main["workflow_runs"][0]["workflow_runs"]:
            if run["path"] == ".github/workflows/ci.yml" and run["event"] == "push":
                run.update(status="completed", conclusion="cancelled")
        verdict = selector.grade_candidate(
            REAL_MAIN, main["check_runs"], main["workflow_runs"], real_inheritance_read(), REPO,
            export_inheritance=True,
        )
        self.assertEqual(verdict["verdict"], selector.GREEN, verdict)
        self.assertIn("CI push run 38187338698 concluded cancelled", verdict["evidence"]["main_push"]["dead"])


class RealInheritanceDecisionTests(unittest.TestCase):
    """The decision record names the pull request and the runs it inherited."""

    def landing_snapshot(self) -> dict[str, Any]:
        document = snapshot(version=RELEASABLE, range=[REAL_MAIN, REAL_BASE])
        document["grades"] = {REAL_MAIN: {**main_pages_at(AT_LANDING), "inheritance": real_inheritance_read()}}
        return document

    def test_the_landing_arms_on_inherited_evidence(self) -> None:
        document = self.landing_snapshot()
        grades = document["grades"]
        decision = selector.judge(
            document,
            lambda sha: selector.grade_candidate(
                sha, grades[sha]["check_runs"], grades[sha]["workflow_runs"], grades[sha]["inheritance"], REPO,
                export_inheritance=True,
            ),
        )
        self.assertEqual(decision.decision, selector.ARM, decision.reason)
        self.assertEqual(decision.candidate, REAL_MAIN)
        self.assertEqual(decision.move, selector.MOVE_CREATE)
        for needle in (
            "inherits CI and Acceptance evidence from export pull request #1848",
            f"CI run {REAL_CI_RUN}",
            f"Acceptance run {REAL_ACCEPTANCE_RUN}",
            f"its tree {REAL_TREE} is the head {REAL_HEAD}'s tree",
            "CI push run 38187338698 is in_progress",
        ):
            self.assertIn(needle, decision.reason)
        self.assertEqual(decision.details["evidence"]["pull"], 1848)

    def test_the_shipped_selector_inherits_nothing_while_the_switch_is_off(self) -> None:
        """A pull request head's contexts can be stand-ins now, so the product does not inherit.

        The same real landing that arms with the switch on stands down through
        the command line, which runs the product default, and the record says
        main's own push runs are what it waits for.
        """

        self.assertFalse(selector.EXPORT_INHERITANCE)
        document = self.landing_snapshot()
        document["range"] = [REAL_MAIN]
        with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as handle:
            json.dump(document, handle)
            path = handle.name
        result = subprocess.run(
            [sys.executable, str(SELECTOR_PATH), "select", "--snapshot", path],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        decision = json.loads(result.stdout)
        self.assertEqual(decision["decision"], selector.STAND_DOWN, decision)
        self.assertIn("still being graded", decision["reason"])

    def test_the_inheritance_command_tells_the_mint_nothing_is_inherited(self) -> None:
        def run(read: dict[str, Any], sha: str = REAL_MAIN) -> subprocess.CompletedProcess:
            with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as handle:
                json.dump(read, handle)
                path = handle.name
            return subprocess.run(
                [sys.executable, str(SELECTOR_PATH), "inheritance", "--sha", sha, "--document", path],
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )

        answered = run(real_inheritance_read())
        self.assertEqual(answered.returncode, 0, answered.stderr)
        answer = json.loads(answered.stdout)
        self.assertFalse(answer["inherited"], answer)
        self.assertIn("export inheritance is off", answer["reasons"][0])
        self.assertEqual(run(real_inheritance_read(), sha="abc").returncode, 2)

    def test_the_binding_the_mint_would_read_is_still_exact(self) -> None:
        """What `inheritance` answers once the switch is back on, from the pure rule."""

        answer = selector.resolve_inheritance(REAL_MAIN, real_inheritance_read(), REPO)
        self.assertTrue(answer["inherited"], answer)
        self.assertEqual(
            {key: answer[key] for key in ("sha", "pull", "head_sha", "head_ref", "base_sha", "tree")},
            {
                "sha": REAL_MAIN,
                "pull": 1848,
                "head_sha": REAL_HEAD,
                "head_ref": REAL_REF,
                "base_sha": REAL_BASE,
                "tree": REAL_TREE,
            },
        )

def git(workspace: Path, *arguments: str) -> str:
    result = subprocess.run(
        [
            "git", "-C", str(workspace),
            "-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false",
            *arguments,
        ],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    return result.stdout.strip()


class ReadInheritanceTests(unittest.TestCase):
    """The live read's git half, against a real repository shaped like an export.

    The origin carries main with a squash on top of the base, and
    refs/pull/<n>/head carrying the export head, which is how GitHub keeps a
    merged pull request's head. The workspace is a clone of main alone, like
    the protected-main checkout the cut and the mint read from.
    """

    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        root = Path(self.directory.name)
        self.origin = root / "origin.git"
        self.author = root / "author"
        subprocess.run(["git", "init", "--quiet", "--bare", str(self.origin)], check=True)
        subprocess.run(["git", "init", "--quiet", "-b", "main", str(self.author)], check=True)
        (self.author / "lib.rs").write_text("one\n", encoding="utf-8")
        git(self.author, "add", "lib.rs")
        git(self.author, "commit", "--quiet", "-s", "-m", "base")
        self.base = git(self.author, "rev-parse", "HEAD")
        (self.author / "lib.rs").write_text("two\n", encoding="utf-8")
        git(self.author, "commit", "--quiet", "-s", "-am", "export head")
        self.head = git(self.author, "rev-parse", "HEAD")
        tree = git(self.author, "rev-parse", "HEAD^{tree}")
        self.squash = git(self.author, "commit-tree", tree, "-p", self.base, "-m", "Update kin source (#7)")
        self.lagging_index = False
        git(self.author, "push", "--quiet", str(self.origin), f"{self.squash}:refs/heads/main",
            f"{self.head}:refs/pull/7/head")
        self.workspace = root / "workspace"
        subprocess.run(["git", "clone", "--quiet", "--branch", "main", str(self.origin), str(self.workspace)], check=True)

    def tearDown(self) -> None:
        self.directory.cleanup()

    def fetch(self, head_sha: str) -> Any:
        read = real_inheritance_read()
        for run in read["workflow_runs"][0]["workflow_runs"]:
            run["head_sha"] = head_sha
        pull = {
            "number": 7,
            "merged": True,
            "merge_commit_sha": self.squash,
            "head": {"sha": head_sha, "ref": REAL_REF, "repo": {"full_name": REPO}},
            "base": {"sha": self.base, "ref": "main", "repo": {"full_name": REPO}},
        }
        answers = {
            selector.commit_pulls_endpoint(REPO, self.squash): (
                [] if self.lagging_index else [{"number": 7, "merge_commit_sha": self.squash}]
            ),
            selector.pull_endpoint(REPO, 7): pull,
            selector.check_runs_endpoint(REPO, head_sha, 1): read["check_runs"][0],
            selector.workflow_runs_endpoint(REPO, head_sha, 1): read["workflow_runs"][0],
        }

        def fetch(endpoint: str) -> Any:
            if endpoint not in answers:
                raise AssertionError(f"unexpected endpoint {endpoint}")
            return copy.deepcopy(answers[endpoint])

        return fetch

    def test_the_read_binds_the_squash_from_git_and_the_head_inherits(self) -> None:
        read = selector.read_inheritance(self.fetch(self.head), REPO, self.squash, self.workspace)
        self.assertEqual(read["commit"]["parents"], [self.base])
        self.assertEqual(read["commit"]["tree"], read["head"]["tree"])
        self.assertEqual(read["head"]["sha"], self.head)
        self.assertTrue(read["head"]["base_is_ancestor"])
        answer = selector.resolve_inheritance(self.squash, read, REPO)
        self.assertTrue(answer["inherited"], answer)
        self.assertEqual(answer["pull"], 7)

    def test_the_subject_of_the_squash_in_git_locates_the_pull_request(self) -> None:
        self.lagging_index = True
        read = selector.read_inheritance(self.fetch(self.head), REPO, self.squash, self.workspace)
        self.assertEqual(read["associated_pulls"], [])
        self.assertEqual(read["subject_pull"], 7)
        self.assertTrue(selector.resolve_inheritance(self.squash, read, REPO)["inherited"])

    def test_a_head_built_on_another_base_is_read_as_not_descending_from_it(self) -> None:
        """The head's tree matches, but it was built beside the base rather than on it."""

        git(self.author, "checkout", "--quiet", "--orphan", "elsewhere")
        (self.author / "lib.rs").write_text("two\n", encoding="utf-8")
        git(self.author, "add", "lib.rs")
        git(self.author, "commit", "--quiet", "-s", "-m", "unrelated history, same tree")
        stray = git(self.author, "rev-parse", "HEAD")
        git(self.author, "push", "--quiet", "--force", str(self.origin), f"{stray}:refs/pull/7/head")
        read = selector.read_inheritance(self.fetch(stray), REPO, self.squash, self.workspace)
        self.assertEqual(read["commit"]["tree"], read["head"]["tree"])
        self.assertFalse(read["head"]["base_is_ancestor"])
        answer = selector.resolve_inheritance(self.squash, read, REPO)
        self.assertFalse(answer["inherited"])
        self.assertIn("is not an ancestor of its head", answer["reasons"][0])


# ---------------------------------------------------------------------------
# A runner timeout is a retry, not a spent rc-build attempt.
# ---------------------------------------------------------------------------


REAL_TIMEOUTS = REAL["rc_builds"]["timed_out"]
REAL_CONCURRENCY_CANCEL = REAL["rc_builds"]["concurrency_cancelled"]
REAL_MANUAL_CANCEL = REAL["rc_builds"]["manually_cancelled"]


def real_build(build: dict[str, Any], **overrides: Any) -> dict[str, Any]:
    build = copy.deepcopy(build)
    build.update(head_sha=REAL_MAIN, artifacts=[])
    build.update(overrides)
    return build


def cancelled_job(build: dict[str, Any]) -> dict[str, Any]:
    """The macOS arm64 build leg, cancelled in every real run kept here."""

    jobs = [
        job for job in build["jobs"]
        if job["conclusion"] == "cancelled" and job["name"] == "RC Build (kin-macos-aarch64)"
    ]
    if len(jobs) != 1:
        raise AssertionError(f"run {build['id']} has {len(jobs)} cancelled macOS arm64 build legs")
    return jobs[0]


class RcBuildTimeoutTests(unittest.TestCase):
    def test_both_attempts_spent_on_08d48c27_were_runner_timeouts(self) -> None:
        for build in REAL_TIMEOUTS:
            with self.subTest(run=build["id"]):
                reason = selector.rc_build_timeout(build)
                self.assertIsNotNone(reason)
                self.assertIn("RC Build (kin-macos-aarch64) hit its 1h30m0s bound after 1h30m", reason)

    def test_a_concurrency_cancel_and_a_manual_cancel_are_not_timeouts(self) -> None:
        self.assertIsNone(selector.rc_build_timeout(REAL_CONCURRENCY_CANCEL))
        self.assertIsNone(selector.rc_build_timeout(REAL_MANUAL_CANCEL))

    def test_a_timeout_text_on_a_job_that_did_not_run_that_long_is_not_a_timeout(self) -> None:
        build = copy.deepcopy(REAL_TIMEOUTS[0])
        cancelled_job(build)["completed_at"] = "2026-10-12T02:13:56Z"
        self.assertIsNone(selector.rc_build_timeout(build))

    def test_a_killed_step_that_ended_before_the_bound_is_not_a_timeout(self) -> None:
        build = copy.deepcopy(REAL_TIMEOUTS[0])
        for step in cancelled_job(build)["steps"]:
            if step["conclusion"] == "cancelled":
                step["completed_at"] = "2026-10-12T02:13:33Z"
        self.assertIsNone(selector.rc_build_timeout(build))

    def test_a_timeout_beside_a_manual_cancel_is_not_a_timeout(self) -> None:
        build = copy.deepcopy(REAL_TIMEOUTS[0])
        cancelled_job(build)["annotations"].insert(
            0, {"annotation_level": "failure", "message": "The run was canceled by @someone."}
        )
        self.assertIsNone(selector.rc_build_timeout(build))

    def test_a_failed_leg_beside_a_timeout_spends_the_attempt(self) -> None:
        build = copy.deepcopy(REAL_TIMEOUTS[0])
        next(job for job in build["jobs"] if job["name"] == "RC Build (kin-linux-x86_64)")["conclusion"] = "failure"
        self.assertIsNone(selector.rc_build_timeout(build))

    def test_the_older_timeout_text_is_read_too(self) -> None:
        build = copy.deepcopy(REAL_TIMEOUTS[0])
        job = cancelled_job(build)
        job["annotations"][0]["message"] = (
            "The job running on runner GitHub Actions 2 has exceeded the maximum execution time of 90 minutes."
        )
        self.assertIn("hit its 90 minutes bound", selector.rc_build_timeout(build))

    def judge(self, builds: list[dict[str, Any]]) -> Any:
        grader = Grader({REAL_MAIN: main_pages_at(None)})
        return selector.judge(
            snapshot(version=RELEASABLE, range=[REAL_MAIN], candidate=REAL_MAIN, rc_builds=builds),
            grader,
        )

    def test_the_candidate_that_died_on_2026_10_12_is_armed_again(self) -> None:
        builds = [real_build(build) for build in reversed(REAL_TIMEOUTS)]
        decision = self.judge(builds)
        self.assertEqual(decision.decision, selector.ARM, decision.reason)
        self.assertEqual(decision.candidate, REAL_MAIN)
        self.assertEqual(decision.move, selector.MOVE_NONE)
        self.assertEqual(decision.details["attempts"], [])
        self.assertEqual(len(decision.details["timeouts"]), 2)
        self.assertIn("runner timeouts retried rather than spent: 38198979588 (cancelled:", decision.reason)

    def test_timeouts_have_a_budget_of_their_own(self) -> None:
        third = real_build(REAL_TIMEOUTS[1], id=38_300_000_000, created_at="2026-10-12T05:00:00Z")
        builds = [third] + [real_build(build) for build in reversed(REAL_TIMEOUTS)]
        decision = self.judge(builds)
        self.assertNotEqual(decision.candidate, REAL_MAIN, decision.reason)
        self.assertTrue(
            any("rc-build hit its runner timeout 3 times" in note for note in decision.details["notes"]),
            decision.details,
        )

    def test_a_failure_still_spends_an_attempt_beside_a_timeout(self) -> None:
        failed = real_build(REAL_TIMEOUTS[1], id=38_300_000_001, conclusion="failure", created_at="2026-10-12T05:00:00Z")
        decision = self.judge([failed, real_build(REAL_TIMEOUTS[0])])
        self.assertEqual(decision.decision, selector.ARM, decision.reason)
        self.assertEqual(decision.details["attempts"], ["38300000001 (failure)"])
        self.assertEqual(len(decision.details["timeouts"]), 1)
        second_failure = real_build(REAL_TIMEOUTS[1], id=38_300_000_002, conclusion="failure",
                                    created_at="2026-10-12T06:00:00Z")
        dead = self.judge([second_failure, failed, real_build(REAL_TIMEOUTS[0])])
        self.assertNotEqual(dead.candidate, REAL_MAIN)
        self.assertTrue(any("rc-build attempts exhausted" in note for note in dead.details["notes"]))

    def test_manual_and_superseding_cancels_still_spend_attempts(self) -> None:
        builds = [real_build(REAL_MANUAL_CANCEL, created_at="2026-10-12T06:00:00Z"),
                  real_build(REAL_CONCURRENCY_CANCEL, created_at="2026-10-12T05:00:00Z")]
        decision = self.judge(builds)
        self.assertNotEqual(decision.candidate, REAL_MAIN)
        self.assertTrue(any("rc-build attempts exhausted" in note for note in decision.details["notes"]))

    def test_the_read_carries_a_cancelled_runs_jobs_and_annotations(self) -> None:
        build = REAL_TIMEOUTS[0]
        branch = build["head_branch"]
        run_row = {key: build[key] for key in ("id", "head_sha", "head_branch", "event", "status", "conclusion", "created_at")}
        answers: dict[str, Any] = {
            selector.rc_build_runs_endpoint(REPO, branch, 1): {"total_count": 1, "workflow_runs": [run_row]},
            selector.run_jobs_endpoint(REPO, build["id"], 1): {
                "total_count": len(build["jobs"]),
                "jobs": [{key: value for key, value in job.items() if key != "annotations"} for job in build["jobs"]],
            },
        }
        for job in build["jobs"]:
            if job["conclusion"] == "cancelled":
                answers[selector.annotations_endpoint(REPO, job["id"])] = job["annotations"]

        def fetch(endpoint: str) -> Any:
            if endpoint not in answers:
                raise AssertionError(f"unexpected endpoint {endpoint}")
            return copy.deepcopy(answers[endpoint])

        builds = selector.read_rc_builds(fetch, REPO, branch)
        self.assertEqual(len(builds), 1)
        self.assertEqual(
            [job["name"] for job in builds[0]["jobs"] if job["annotations"]],
            ["RC Build (kin-macos-aarch64)"],
        )
        self.assertIsNotNone(selector.rc_build_timeout(builds[0]))


class TimedOutRetryTests(unittest.TestCase):
    """settled_retry admits a timed-out first attempt, and only a timed-out one."""

    TIMEOUT = REAL_TIMEOUTS[0]

    def annotations(self, message: str) -> list[dict[str, str]]:
        return [
            {"annotation_level": "failure", "message": message},
            {"annotation_level": "failure", "message": "The operation was canceled."},
        ]

    def grade(self, first: dict[str, Any]) -> dict[str, Any]:
        checks = [check for check in green_checks() if check["name"] != "Feature permutation tests (macos-latest)"]
        second = check_run(
            "Feature permutation tests (macos-latest)",
            suite=CI_SUITE,
            started_at="2026-09-28T11:35:00Z",
            completed_at="2026-09-28T12:50:00Z",
        )
        return selector.grade_sha(NEWEST, pages(checks + [first, second], "check_runs"),
                                  pages(green_workflow_runs(), "workflow_runs"))

    def first(self, message: str, completed_at: str = "2026-09-28T11:30:38Z") -> dict[str, Any]:
        attempt = check_run(
            "Feature permutation tests (macos-latest)",
            suite=CI_SUITE,
            conclusion="cancelled",
            started_at="2026-09-28T10:00:00Z",
            completed_at=completed_at,
        )
        attempt["annotations"] = self.annotations(message)
        return attempt

    def test_a_job_that_hit_its_own_timeout_and_then_passed_is_one_retry(self) -> None:
        message = cancelled_job(self.TIMEOUT)["annotations"][0]["message"]
        verdict = self.grade(self.first(message))
        self.assertEqual(verdict["verdict"], selector.GREEN, verdict)
        self.assertEqual(verdict["retried"], ["Feature permutation tests (macos-latest)"])

    def test_a_cancel_that_was_not_a_timeout_stays_ambiguous(self) -> None:
        for label, message, completed_at in (
            ("superseded", cancelled_job(REAL_CONCURRENCY_CANCEL)["annotations"][0]["message"], "2026-09-28T11:30:38Z"),
            ("manual", cancelled_job(REAL_MANUAL_CANCEL)["annotations"][0]["message"], "2026-09-28T11:30:38Z"),
            ("early", cancelled_job(self.TIMEOUT)["annotations"][0]["message"], "2026-09-28T10:20:00Z"),
        ):
            with self.subTest(label=label):
                verdict = self.grade(self.first(message, completed_at))
                self.assertEqual(verdict["verdict"], selector.DEAD, verdict)
                self.assertTrue(any("ambiguous required check" in line for line in verdict["dead"]))

    def test_a_cancel_with_no_annotations_stays_ambiguous(self) -> None:
        attempt = self.first("unused")
        attempt["annotations"] = []
        self.assertEqual(self.grade(attempt)["verdict"], selector.DEAD)

    def test_the_live_grade_reads_annotations_only_for_cancelled_required_checks(self) -> None:
        cancelled = self.first("unused")
        del cancelled["annotations"]
        cancelled["id"] = 77
        checks = green_checks() + [cancelled, check_run("Code Coverage", suite=CI_SUITE, conclusion="cancelled")]
        checks[-1]["id"] = 78
        read: list[str] = []

        def fetch(endpoint: str) -> Any:
            read.append(endpoint)
            if endpoint == selector.check_runs_endpoint(REPO, NEWEST, 1):
                return {"total_count": len(checks), "check_runs": copy.deepcopy(checks)}
            if endpoint == selector.workflow_runs_endpoint(REPO, NEWEST, 1):
                return {"total_count": len(green_workflow_runs()), "workflow_runs": green_workflow_runs()}
            if endpoint == selector.annotations_endpoint(REPO, 77):
                return self.annotations(cancelled_job(self.TIMEOUT)["annotations"][0]["message"])
            raise AssertionError(f"unexpected endpoint {endpoint}")

        selector.grade_live(fetch, REPO, NEWEST)
        self.assertIn(selector.annotations_endpoint(REPO, 77), read)
        self.assertNotIn(selector.annotations_endpoint(REPO, 78), read)


# ---------------------------------------------------------------------------
# The merge queue: the group commit is the release candidate.
# ---------------------------------------------------------------------------


QUEUE_REF_REAL = f"gh-readonly-queue/main/pr-1848-{REAL_BASE}"
QUEUE_PRODUCERS = (
    ".github/workflows/ci.yml",
    ".github/workflows/acceptance.yml",
    ".github/workflows/sast.yml",
    ".github/workflows/secret-scan.yml",
)


def merge_group_pages(moment: str | None = None) -> dict[str, Any]:
    """08d48c27's real listings as the merge queue's build of it would read.

    The same runs and check-runs, carried by the group commit's queue ref under
    the merge_group event, and no push build on main: the landing that ships
    0.8.4 is graded on the queue's build alone.
    """

    checks, runs = main_listings_at(moment)
    for run in runs:
        if run["head_branch"] == "main" and run["event"] == "push" and run["path"] in QUEUE_PRODUCERS:
            run.update(event="merge_group", head_branch=QUEUE_REF_REAL)
    return grade_pages(checks, runs)


def grade_queued_landing(pages_: dict[str, Any], parents: list[str] | None = None) -> dict[str, Any]:
    return selector.grade_candidate(
        REAL_MAIN, pages_["check_runs"], pages_["workflow_runs"], None, REPO, parents or [REAL_BASE]
    )


def queue_runs(pages_: dict[str, Any]) -> list[dict[str, Any]]:
    return pages_["workflow_runs"][0]["workflow_runs"]


def queue_checks(pages_: dict[str, Any]) -> list[dict[str, Any]]:
    return pages_["check_runs"][0]["check_runs"]


def queue_rc_build(**overrides: Any) -> dict[str, Any]:
    """The real RC Build of 08d48c27, as the queue's build of the group commit."""

    build = real_build(REAL_TIMEOUTS[0], event="merge_group", head_branch=QUEUE_REF_REAL, run_attempt=1)
    build.update(overrides)
    return build


def usable_queue_rc_build(**overrides: Any) -> dict[str, Any]:
    return queue_rc_build(conclusion="success", artifacts=sorted(selector.RC_RELEASE_ARTIFACTS), **overrides)


class MergeGroupGradeTests(unittest.TestCase):
    def test_a_queued_landing_is_graded_on_the_queues_build_of_it(self) -> None:
        verdict = grade_queued_landing(merge_group_pages())
        self.assertEqual(verdict["verdict"], selector.GREEN, verdict)
        evidence = verdict["evidence"]
        self.assertEqual(evidence["source"], selector.EVIDENCE_MERGE_GROUP)
        self.assertEqual((evidence["queue_ref"], evidence["pull"], evidence["base_sha"]), (QUEUE_REF_REAL, 1848, REAL_BASE))
        self.assertEqual(evidence["runs"], {"CI": 38_187_338_698, "Acceptance": 38_187_338_482})
        self.assertIn("no CI push run on main at this sha yet", evidence["main_push"]["pending"])

    def test_a_queue_build_of_another_parent_proved_another_tree(self) -> None:
        # With no push build at all, main's own grade is dead without a red:
        # its contexts are missing, which is no verdict, and nothing stands in.
        verdict = grade_queued_landing(merge_group_pages(), parents=["9" * 40])
        self.assertEqual(verdict["verdict"], selector.DEAD)
        self.assertEqual(verdict["evidence"]["source"], selector.EVIDENCE_PUSH)
        self.assertIn("proved a different tree", verdict["evidence"]["merge_group"])

    def test_two_uncancelled_queue_builds_are_ambiguous(self) -> None:
        pages_ = merge_group_pages()
        ci = next(run for run in queue_runs(pages_) if run["path"] == ".github/workflows/ci.yml")
        queue_runs(pages_).append({**ci, "id": ci["id"] + 1, "check_suite_id": ci["check_suite_id"] + 1})
        pages_["workflow_runs"][0]["total_count"] += 1
        verdict = grade_queued_landing(pages_)
        self.assertNotEqual(verdict["verdict"], selector.GREEN)
        self.assertIn("ambiguous merge-group evidence", verdict["evidence"]["merge_group"])

    def test_a_branch_that_looks_like_the_queue_is_not_the_queue(self) -> None:
        pages_ = merge_group_pages()
        for run in queue_runs(pages_):
            if run["head_branch"] == QUEUE_REF_REAL:
                run["head_branch"] = f"chore/gh-readonly-queue/main/pr-1848-{REAL_BASE}"
        verdict = grade_queued_landing(pages_)
        self.assertNotEqual(verdict["verdict"], selector.GREEN)
        self.assertIn("is not on a merge-queue ref of main", verdict["evidence"]["merge_group"])

    def test_a_red_context_in_the_queue_build_is_not_evidence(self) -> None:
        pages_ = merge_group_pages()
        for check in queue_checks(pages_):
            if check["name"] == "Falsify guards":
                check["conclusion"] = "failure"
        verdict = grade_queued_landing(pages_)
        self.assertNotEqual(verdict["verdict"], selector.GREEN)
        self.assertIn("required check not green: Falsify guards (conclusion=failure)", verdict["evidence"]["merge_group"])

    def test_the_queue_build_must_have_run_acceptance(self) -> None:
        pages_ = merge_group_pages()
        for check in queue_checks(pages_):
            if check["name"] == "Product Acceptance":
                check["conclusion"] = "skipped"
        verdict = grade_queued_landing(pages_)
        self.assertNotEqual(verdict["verdict"], selector.GREEN)
        self.assertIn("Product Acceptance (conclusion=skipped)", verdict["evidence"]["merge_group"])

    def test_gitleaks_from_a_push_to_the_queue_ref_is_not_the_queue_build(self) -> None:
        """secret-scan.yml publishes under merge_group in the queue now, like the rest."""

        pages_ = merge_group_pages()
        for run in queue_runs(pages_):
            if run["path"] == ".github/workflows/secret-scan.yml" and run["head_branch"] == QUEUE_REF_REAL:
                run["event"] = "push"
        verdict = grade_queued_landing(pages_)
        self.assertNotEqual(verdict["verdict"], selector.GREEN)
        self.assertIn("missing required check: gitleaks (full history)", verdict["evidence"]["merge_group"])

    def test_a_red_main_push_is_never_overridden_by_the_queue(self) -> None:
        pages_ = merge_group_pages()
        sast = next(run for run in queue_runs(pages_) if run["path"] == ".github/workflows/sast.yml")
        queue_runs(pages_).append(
            {**sast, "id": sast["id"] + 7, "event": "push", "head_branch": "main", "conclusion": "failure",
             "check_suite_id": 777_000_001}
        )
        queue_checks(pages_).append(
            check_run("cargo-deny", suite=777_000_001, conclusion="failure")
        )
        pages_["workflow_runs"][0]["total_count"] += 1
        pages_["check_runs"][0]["total_count"] += 1
        verdict = grade_queued_landing(pages_)
        self.assertEqual(verdict["verdict"], selector.DEAD)
        self.assertIn("not considered", verdict["evidence"]["inheritance"])

    def test_a_commit_that_never_went_through_the_queue_is_graded_on_main_alone(self) -> None:
        verdict = grade_queued_landing(main_pages_at(AT_LANDING))
        self.assertEqual(verdict["verdict"], selector.PENDING)
        self.assertNotIn("merge_group", verdict["evidence"])


class QueuedLandingDecisionTests(unittest.TestCase):
    """A queued landing on main: preflight the queue's archives, never rebuild them."""

    def judge(self, builds: list[dict[str, Any]], *, candidate: str | None = None) -> Any:
        pages_ = merge_group_pages()
        return selector.judge(
            snapshot(version=RELEASABLE, range=[REAL_MAIN], candidate=candidate),
            lambda sha: grade_queued_landing(pages_),
            lambda sha: [copy.deepcopy(build) for build in builds],
        )

    def test_a_usable_queue_build_is_preflighted_rather_than_rebuilt(self) -> None:
        build = usable_queue_rc_build()
        decision = self.judge([build])
        self.assertEqual(decision.decision, selector.PROOF, decision.reason)
        self.assertEqual((decision.candidate, decision.rc_run, decision.move), (REAL_MAIN, build["id"], None))
        self.assertIn("landed through the merge queue", decision.reason)
        self.assertIn(f"graded on the merge queue's build of this very commit on {QUEUE_REF_REAL}", decision.reason)
        self.assertEqual(decision.details["rc_source"], selector.EVIDENCE_MERGE_GROUP)

    def test_a_running_queue_build_is_waited_for(self) -> None:
        build = queue_rc_build(status="in_progress", conclusion=None)
        decision = self.judge([build])
        self.assertEqual(decision.decision, selector.STAND_DOWN, decision.reason)
        self.assertEqual(decision.rc_run, build["id"])

    def test_a_queue_build_its_runner_timeout_killed_is_re_run(self) -> None:
        build = queue_rc_build()
        decision = self.judge([build])
        self.assertEqual(decision.decision, selector.RETRY, decision.reason)
        self.assertEqual(decision.rc_run, build["id"])
        self.assertIn("re-running its failed jobs rather than spending the candidate", decision.reason)

    def test_a_queue_build_that_timed_out_on_every_attempt_falls_back_to_a_dispatch(self) -> None:
        decision = self.judge([queue_rc_build(run_attempt=selector.RC_BUILD_TIMEOUT_LIMIT)])
        self.assertEqual(decision.decision, selector.ARM, decision.reason)
        self.assertEqual(decision.move, selector.MOVE_CREATE)
        self.assertTrue(any("on all 3 attempts" in note for note in decision.details["notes"]), decision.details)

    def test_a_failed_queue_build_falls_back_to_a_dispatch(self) -> None:
        decision = self.judge([queue_rc_build(conclusion="failure", jobs=[])])
        self.assertEqual(decision.decision, selector.ARM, decision.reason)
        self.assertTrue(any("concluded failure" in note for note in decision.details["notes"]))

    def test_an_armed_candidate_is_not_built_again_beside_a_usable_queue_build(self) -> None:
        build = usable_queue_rc_build()
        decision = self.judge([build], candidate=REAL_MAIN)
        self.assertEqual(decision.decision, selector.PROOF, decision.reason)
        self.assertEqual(decision.rc_run, build["id"])
        running = self.judge([queue_rc_build(status="in_progress", conclusion=None)], candidate=REAL_MAIN)
        self.assertEqual(running.decision, selector.STAND_DOWN, running.reason)

    def test_a_landing_with_no_queue_build_arms_its_own(self) -> None:
        decision = self.judge([])
        self.assertEqual(decision.decision, selector.ARM, decision.reason)
        self.assertEqual(decision.move, selector.MOVE_CREATE)


def queue_snapshot(**overrides: Any) -> dict[str, Any]:
    document = {
        "mode": "merge-group",
        "repository": REPO,
        "sha": REAL_MAIN,
        "version": RELEASABLE,
        "binding": {"bound": "merge-group", "ref": QUEUE_REF_REAL, "pull": 1848, "base": REAL_BASE, "stacked": False},
        "tag_exists": False,
        "evidence": {},
        "rc_builds": [usable_queue_rc_build()],
    }
    document.update(overrides)
    return document


class QueueWindowTests(unittest.TestCase):
    """The group is still queued: file its preflight before the queue lands it."""

    def assertQueue(self, document: dict[str, Any], decision: str, state: str | None, needle: str) -> Any:
        verdict = selector.judge_queue(document)
        self.assertEqual(verdict.decision, decision, verdict.reason)
        self.assertEqual(verdict.queue_state, state, verdict.reason)
        self.assertIn(needle, verdict.reason)
        return verdict

    def test_a_release_groups_usable_build_is_preflighted_inside_the_queue_window(self) -> None:
        verdict = self.assertQueue(queue_snapshot(), selector.PROOF, selector.QUEUE_PROOF, "before the queue lands it")
        self.assertEqual((verdict.candidate, verdict.queue_sha), (REAL_MAIN, REAL_MAIN))
        self.assertEqual(verdict.rc_run, REAL_TIMEOUTS[0]["id"])
        self.assertEqual(verdict.branch, f"release/v{RELEASABLE}-candidate")

    def test_the_queue_waits_while_the_build_runs_or_has_not_started(self) -> None:
        self.assertQueue(queue_snapshot(rc_builds=[queue_rc_build(status="queued", conclusion=None)]),
                         selector.STAND_DOWN, None, "is queued")
        self.assertQueue(queue_snapshot(rc_builds=[]), selector.STAND_DOWN, None, "no RC Build has started")

    def test_a_timeout_in_the_queue_fails_the_group_rather_than_outliving_its_window(self) -> None:
        """A leg that timed out spent its 60-minute bound; a re-run cannot land within 90."""

        self.assertGreater(2 * selector.MERGE_GROUP_LEG_MINUTES, selector.QUEUE_RESPONSE_MINUTES)
        self.assertLess(selector.MERGE_GROUP_WORST_MINUTES, selector.QUEUE_RESPONSE_MINUTES)
        self.assertQueue(queue_snapshot(rc_builds=[queue_rc_build()]), selector.REFUSE, selector.QUEUE_FAILURE,
                         "cannot finish inside the queue's 90-minute window")

    def test_a_build_that_cannot_be_proven_fails_the_group(self) -> None:
        self.assertQueue(queue_snapshot(rc_builds=[queue_rc_build(run_attempt=3)]),
                         selector.REFUSE, selector.QUEUE_FAILURE, "on all 3 attempts")
        self.assertQueue(queue_snapshot(rc_builds=[queue_rc_build(conclusion="failure", jobs=[])]),
                         selector.REFUSE, selector.QUEUE_FAILURE, "concluded failure")
        self.assertQueue(queue_snapshot(rc_builds=[queue_rc_build(conclusion="success")]),
                         selector.REFUSE, selector.QUEUE_FAILURE, "without all six release artifacts")

    def test_a_usable_run_wins_beside_any_other(self) -> None:
        """The record names the run it judged, so a second run of the commit is not ambiguous."""

        usable = usable_queue_rc_build(id=1)
        verdict = selector.judge_queue(queue_snapshot(rc_builds=[queue_rc_build(conclusion="failure", jobs=[]), usable]))
        self.assertEqual((verdict.decision, verdict.rc_run), (selector.PROOF, 1), verdict.reason)

    def test_a_group_that_stages_no_release_is_let_through(self) -> None:
        self.assertQueue(queue_snapshot(tag_exists=True), selector.STAND_DOWN, selector.QUEUE_SUCCESS, "already tagged")
        self.assertQueue(queue_snapshot(version="0.8.2"), selector.STAND_DOWN, selector.QUEUE_SUCCESS, "is retired")
        self.assertQueue(queue_snapshot(evidence={REAL_MAIN: [selector.PREFLIGHT_RECORD]}),
                         selector.STAND_DOWN, selector.QUEUE_SUCCESS, "already filed")

    def test_a_commit_that_is_not_a_live_group_gets_nothing(self) -> None:
        verdict = self.assertQueue(
            queue_snapshot(binding={"bound": None, "reason": "its group left the queue without landing"}),
            selector.STAND_DOWN, None, "left the queue",
        )
        self.assertIsNone(verdict.queue_sha)

    def test_the_command_line_hands_the_queue_its_state(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            snapshot_path = Path(directory) / "queue.json"
            output_path = Path(directory) / "output"
            snapshot_path.write_text(json.dumps({**queue_snapshot(), "grades": {}}), encoding="utf-8")
            result = subprocess.run(
                [sys.executable, str(SELECTOR_PATH), "select", "--snapshot", str(snapshot_path)],
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                env={**os.environ, "GITHUB_OUTPUT": str(output_path)},
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            written = output_path.read_text(encoding="utf-8")
        self.assertIn(f"queue_sha={REAL_MAIN}\n", written)
        self.assertIn("queue_state=proof\n", written)
        self.assertIn("decision=proof\n", written)


class BindMergeGroupTests(unittest.TestCase):
    REF = f"refs/heads/{QUEUE_REF_REAL}"

    def read(self, **overrides: Any) -> dict[str, Any]:
        read = {
            "sha": REAL_MAIN,
            "landed": False,
            "refs": [{"ref": self.REF, "sha": REAL_MAIN}],
            "parents": [REAL_BASE],
            "base_on_main": True,
        }
        read.update(overrides)
        return read

    def test_a_live_group_on_protected_main_is_bound(self) -> None:
        binding = selector.bind_merge_group(REAL_MAIN, self.read())
        self.assertEqual(binding["bound"], "merge-group", binding)
        self.assertEqual((binding["ref"], binding["pull"], binding["base"]), (QUEUE_REF_REAL, 1848, REAL_BASE))

    def test_a_landed_commit_is_main_history(self) -> None:
        self.assertEqual(selector.bind_merge_group(REAL_MAIN, self.read(landed=True, refs=[]))["bound"], "main")

    def test_a_group_stacked_behind_another_live_group_is_bound(self) -> None:
        stacked = selector.bind_merge_group(
            REAL_MAIN,
            self.read(base_on_main=False,
                      refs=[{"ref": self.REF, "sha": REAL_MAIN},
                            {"ref": f"refs/heads/gh-readonly-queue/main/pr-1847-{'3' * 40}", "sha": REAL_BASE}]),
        )
        self.assertEqual((stacked["bound"], stacked["stacked"]), ("merge-group", True))

    def test_everything_else_is_refused(self) -> None:
        for label, read, needle in (
            ("left the queue", self.read(refs=[]), "left the queue without landing"),
            ("two refs", self.read(refs=[{"ref": self.REF, "sha": REAL_MAIN}] * 2), "2 live merge-queue refs"),
            ("not main's queue", self.read(refs=[{"ref": f"refs/heads/gh-readonly-queue/release/pr-1848-{REAL_BASE}", "sha": REAL_MAIN}]),
             "is not a merge-queue ref of main"),
            ("parent differs", self.read(parents=["9" * 40]), "are not the base"),
            ("merge commit", self.read(parents=[REAL_BASE, REAL_HEAD]), "are not the base"),
            ("base off main", self.read(base_on_main=False), "neither protected main nor another live group"),
        ):
            with self.subTest(label=label):
                binding = selector.bind_merge_group(REAL_MAIN, read)
                self.assertIsNone(binding["bound"], binding)
                self.assertIn(needle, binding["reason"])


class GatherQueueTests(unittest.TestCase):
    """The live queue read against a real repository with a merge-queue ref."""

    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        root = Path(self.directory.name)
        self.origin = root / "origin.git"
        author = root / "author"
        subprocess.run(["git", "init", "--quiet", "--bare", str(self.origin)], check=True)
        subprocess.run(["git", "init", "--quiet", "-b", "main", str(author)], check=True)
        (author / "Cargo.toml").write_text('[workspace.package]\nversion = "0.8.2"\n', encoding="utf-8")
        git(author, "add", "Cargo.toml")
        git(author, "commit", "--quiet", "-s", "-m", "base")
        self.base = git(author, "rev-parse", "HEAD")
        (author / "Cargo.toml").write_text('[workspace.package]\nversion = "0.8.4"\n', encoding="utf-8")
        git(author, "commit", "--quiet", "-s", "-am", "Kin 0.8.4 (#7)")
        self.group = git(author, "rev-parse", "HEAD")
        self.queue_ref = f"refs/heads/gh-readonly-queue/main/pr-7-{self.base}"
        # The origin takes the refs by fetching them, the way the queue writes
        # its own ref server side; a push would run the author's push hooks.
        git(self.origin, "fetch", "--quiet", str(author), f"{self.base}:refs/heads/main", f"{self.group}:{self.queue_ref}")
        self.author = author
        self.workspace = root / "workspace"
        subprocess.run(["git", "clone", "--quiet", "--branch", "main", str(self.origin), str(self.workspace)], check=True)
        git(self.workspace, "fetch", "--quiet", "origin", "main")
        git(self.workspace, "update-ref", "refs/remotes/origin/main", "FETCH_HEAD")

    def tearDown(self) -> None:
        self.directory.cleanup()

    def fetch(self) -> Any:
        build = usable_queue_rc_build(head_sha=self.group, head_branch=self.queue_ref.removeprefix("refs/heads/"))
        answers = {
            selector.queue_refs_endpoint(REPO): [{"ref": self.queue_ref, "object": {"sha": self.group}}],
            selector.evidence_tree_endpoint(REPO): {"tree": [], "truncated": False},
            selector.merge_group_rc_build_runs_endpoint(REPO, self.group, 1): {
                "total_count": 1,
                "workflow_runs": [{key: build[key] for key in ("id", "head_sha", "head_branch", "event", "status",
                                                              "conclusion", "created_at", "run_attempt")}],
            },
            selector.run_artifacts_endpoint(REPO, build["id"], 1): {
                "total_count": len(selector.RC_RELEASE_ARTIFACTS),
                "artifacts": [{"name": name, "expired": False} for name in selector.RC_RELEASE_ARTIFACTS],
            },
        }

        def fetch(endpoint: str) -> Any:
            if endpoint == selector.tag_endpoint(REPO, "0.8.4"):
                raise selector.NotFound("HTTP 404")
            if endpoint not in answers:
                raise AssertionError(f"unexpected endpoint {endpoint}")
            return copy.deepcopy(answers[endpoint])

        return fetch

    def test_a_queued_release_group_is_bound_and_preflighted(self) -> None:
        document = selector.gather_queue(REPO, self.workspace, self.group, self.fetch())
        self.assertEqual(document["binding"]["bound"], "merge-group", document["binding"])
        self.assertEqual(document["binding"]["base"], self.base)
        self.assertEqual(document["version"], "0.8.4")
        verdict = selector.judge_queue(document)
        self.assertEqual(verdict.decision, selector.PROOF, verdict.reason)

    def test_the_group_reads_as_landed_once_main_carries_it(self) -> None:
        git(self.origin, "update-ref", "refs/heads/main", self.group)
        git(self.workspace, "fetch", "--quiet", "origin", "main")
        git(self.workspace, "update-ref", "refs/remotes/origin/main", "FETCH_HEAD")
        read = selector.read_queue_binding(self.fetch(), REPO, self.group, self.workspace)
        self.assertTrue(read["landed"])
        self.assertEqual(selector.bind_merge_group(self.group, read)["bound"], "main")


class RetiredVersionTests(unittest.TestCase):
    def test_a_retired_version_is_never_cut(self) -> None:
        grader = Grader({})
        decision = selector.judge(snapshot(version="0.8.2", candidate=MIDDLE, rc_builds=[rc_build(MIDDLE)]), grader)
        self.assertEqual(decision.decision, selector.STAND_DOWN)
        self.assertIn("v0.8.2 is retired and will never be tagged", decision.reason)
        self.assertEqual(grader.calls, [], "a retired version must cost no grading")

    def test_a_leftover_candidates_evidence_outside_the_staged_range_is_never_read(self) -> None:
        """release/v0.8.2-candidate and its evidence stay behind once main stages 0.8.4."""

        grader = Grader({NEWEST: green_grade(NEWEST)})
        decision = selector.judge(
            snapshot(version=RELEASABLE, range=[NEWEST], evidence={FOREIGN: [selector.PREFLIGHT_RECORD]}),
            grader,
        )
        self.assertEqual(decision.decision, selector.ARM, decision.reason)
        self.assertEqual(decision.candidate, NEWEST)
        self.assertEqual(decision.branch, f"release/v{RELEASABLE}-candidate")

    def test_the_mint_asks_the_same_rule(self) -> None:
        for version, code in (("0.8.2", 3), ("v0.8.3", 3), ("0.8.4", 0), ("v0.8.4", 0)):
            with self.subTest(version=version):
                result = subprocess.run(
                    [sys.executable, str(SELECTOR_PATH), "admit-version", "--version", version],
                    check=False,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                self.assertEqual(result.returncode, code, result.stdout + result.stderr)
                self.assertEqual(json.loads(result.stdout)["admitted"], code == 0)


# ---------------------------------------------------------------------------
# Trigger validation.
# ---------------------------------------------------------------------------


def trigger_context(**overrides: Any) -> dict[str, Any]:
    context = {
        "event_name": "schedule",
        "event_action": "",
        "actor": "troyjr4103",
        "repository": REPO,
        "default_branch": "main",
        "ref": "refs/heads/main",
        "workflow_sha": NEWEST,
        "event": {},
    }
    context.update(overrides)
    return context


def completion_event(**overrides: Any) -> dict[str, Any]:
    run = {
        "path": ".github/workflows/ci.yml",
        "event": "push",
        "head_branch": "main",
        "head_repository": {"full_name": REPO},
        "status": "completed",
        "conclusion": "success",
        "head_sha": NEWEST,
        "id": CI_RUN,
    }
    run.update(overrides)
    return {"workflow_run": run}


def push_event(**overrides: Any) -> dict[str, Any]:
    event = {
        "ref": "refs/heads/main",
        "before": OLDEST,
        "after": NEWEST,
        "deleted": False,
        "repository": {"full_name": REPO},
    }
    event.update(overrides)
    return event


class TriggerTests(unittest.TestCase):
    def test_the_sweep_is_admitted(self) -> None:
        verdict = selector.validate_trigger(**trigger_context())
        self.assertEqual(verdict["trigger"], selector.TRIGGER_SWEEP)

    def test_a_ci_completion_for_a_main_push_is_admitted(self) -> None:
        verdict = selector.validate_trigger(
            **trigger_context(event_name="workflow_run", event_action="completed", event=completion_event())
        )
        self.assertEqual(verdict["trigger"], selector.TRIGGER_CI)

    def test_an_rc_build_completion_on_a_candidate_branch_is_admitted(self) -> None:
        verdict = selector.validate_trigger(
            **trigger_context(
                event_name="workflow_run",
                event_action="completed",
                event=completion_event(
                    path=".github/workflows/rc-build.yml",
                    event="workflow_dispatch",
                    head_branch=f"release/v{VERSION}-candidate",
                ),
            )
        )
        self.assertEqual(verdict["trigger"], selector.TRIGGER_RC_BUILD)
        self.assertEqual(verdict["run_id"], CI_RUN)

    def test_completions_that_are_not_occasions_refuse(self) -> None:
        for overrides, needle in (
            ({"head_branch": "chore/lane-x"}, "occasion only for a push to main"),
            ({"event": "pull_request"}, "occasion only for a push to main"),
            ({"head_repository": {"full_name": "someone/fork"}}, "first-party"),
            ({"status": "in_progress"}, "not completed"),
            ({"path": ".github/workflows/acceptance.yml"}, "is neither"),
        ):
            with self.assertRaises(selector.SelectionError) as caught:
                selector.validate_trigger(
                    **trigger_context(
                        event_name="workflow_run", event_action="completed", event=completion_event(**overrides)
                    )
                )
            self.assertIn(needle, str(caught.exception))

    def test_an_rc_build_completion_off_a_candidate_branch_refuses(self) -> None:
        with self.assertRaises(selector.SelectionError) as caught:
            selector.validate_trigger(
                **trigger_context(
                    event_name="workflow_run",
                    event_action="completed",
                    event=completion_event(path=".github/workflows/rc-build.yml", event="workflow_dispatch", head_branch="main"),
                )
            )
        self.assertIn("release/v<version>-candidate", str(caught.exception))

    def test_the_kick_is_admitted_only_from_the_allowlist(self) -> None:
        for actor in sorted(selector.KICK_ACTORS):
            verdict = selector.validate_trigger(
                **trigger_context(
                    event_name="repository_dispatch",
                    event_action=selector.KICK_ACTION,
                    actor=actor,
                    event={"action": selector.KICK_ACTION, "client_payload": {"reason": "cut it"}},
                )
            )
            self.assertEqual(verdict["trigger"], selector.TRIGGER_KICK)
            self.assertEqual(verdict["actor"], actor)
        with self.assertRaises(selector.SelectionError) as caught:
            selector.validate_trigger(
                **trigger_context(
                    event_name="repository_dispatch",
                    event_action=selector.KICK_ACTION,
                    actor="a-stranger",
                    event={"action": selector.KICK_ACTION},
                )
            )
        self.assertIn("may not kick", str(caught.exception))

    def test_kick_refusals(self) -> None:
        for overrides, event, needle in (
            ({"event_action": "release_something_else"}, {"action": "release_something_else"}, "event action must be"),
            ({}, {"action": "mismatch"}, "differs from the trigger context"),
            ({}, {"action": selector.KICK_ACTION, "client_payload": {"sha": NEWEST}}, "reason and nothing else"),
            ({}, {"action": selector.KICK_ACTION, "client_payload": {"reason": "x" * 201}}, "at most 200"),
        ):
            context = {
                "event_name": "repository_dispatch",
                "event_action": selector.KICK_ACTION,
                "event": event,
            }
            context.update(overrides)
            with self.assertRaises(selector.SelectionError) as caught:
                selector.validate_trigger(**trigger_context(**context))
            self.assertIn(needle, str(caught.exception))

    def test_context_that_is_not_protected_main_refuses(self) -> None:
        for overrides, needle in (
            ({"repository": "someone/kin"}, "repository must be"),
            ({"default_branch": "trunk"}, "default branch must be"),
            ({"ref": "refs/heads/chore/lane"}, "workflow ref must be"),
            ({"workflow_sha": "abc"}, "40-character"),
            ({"event_name": "pull_request_target"}, "event name must be"),
            ({"event": []}, "event document must be an object"),
        ):
            with self.assertRaises(selector.SelectionError) as caught:
                selector.validate_trigger(**trigger_context(**overrides))
            self.assertIn(needle, str(caught.exception))

    def test_merge_group_completions_are_occasions(self) -> None:
        """The queue's RC Build of a group, to preflight it; its CI, to release a group that stages nothing."""

        for path in (".github/workflows/rc-build.yml", ".github/workflows/ci.yml"):
            with self.subTest(path=path):
                verdict = selector.validate_trigger(
                    **trigger_context(
                        event_name="workflow_run",
                        event_action="completed",
                        event=completion_event(path=path, event="merge_group", head_branch=QUEUE_REF_REAL,
                                               head_sha=REAL_MAIN),
                    )
                )
                self.assertEqual(verdict["trigger"], selector.TRIGGER_MERGE_GROUP)
                self.assertEqual((verdict["workflow"], verdict["head_sha"]), (path, REAL_MAIN))

    def test_merge_group_completions_off_mains_queue_refuse(self) -> None:
        for branch in (f"gh-readonly-queue/release/pr-1-{REAL_BASE}", "feature/public-export-kin-x", "main"):
            with self.subTest(branch=branch):
                with self.assertRaises(selector.SelectionError) as caught:
                    selector.validate_trigger(
                        **trigger_context(
                            event_name="workflow_run",
                            event_action="completed",
                            event=completion_event(path=".github/workflows/rc-build.yml", event="merge_group",
                                                   head_branch=branch),
                        )
                    )
                self.assertIn("gh-readonly-queue/main/", str(caught.exception))

    def test_a_push_to_main_is_admitted(self) -> None:
        """The landing push itself, so an inherited grade arms without waiting for CI."""

        verdict = selector.validate_trigger(**trigger_context(event_name="push", event=push_event()))
        self.assertEqual(verdict, {"trigger": selector.TRIGGER_PUSH, "head_sha": NEWEST})

    def test_pushes_that_are_not_occasions_refuse(self) -> None:
        for overrides, event, needle in (
            ({"ref": "refs/heads/feature/public-export-kin-x"}, push_event(), "workflow ref must be"),
            ({}, push_event(ref="refs/heads/feature/public-export-kin-x"), "only on refs/heads/main"),
            ({}, push_event(repository={"full_name": "someone/kin"}), "not first-party"),
            ({}, push_event(after=MIDDLE), "differs from the workflow sha"),
            ({}, push_event(deleted=True), "differs from the workflow sha"),
            ({"event_action": "synchronize"}, push_event(), "action must be empty"),
        ):
            with self.subTest(needle=needle, overrides=overrides):
                with self.assertRaises(selector.SelectionError) as caught:
                    selector.validate_trigger(
                        **trigger_context(**{"event_name": "push", "event": event, **overrides})
                    )
                self.assertIn(needle, str(caught.exception))


# ---------------------------------------------------------------------------
# Contracts against the mint and the workflow.
# ---------------------------------------------------------------------------


class StrangerCommandRenderingTests(unittest.TestCase):
    """Every public instruction renders the two-stage form, and nothing renders the old one.

    `bin/kin-stranger run` REFUSES without `--run <id>`, and the run id comes from
    `prepare`, so a one-line `run --archive <path>` is not a shortcut, it is a
    command that cannot work. Two places told a reader to type exactly that:
    `docs/release-bot.md` and the summary `release-cut.yml` writes when the
    hosted stranger leaves no record. Both were added by this lane on
    2026-09-04 and both were wrong the moment they shipped.

    The rendering lives in two files that cannot share a string, one Markdown
    and one shell heredoc, so the coupling is asserted here instead.
    """

    #: The one shape a reader may be handed. `prepare` first, and every `run`
    #: carrying the same `--run` id.
    PREPARE = "bin/kin-stranger prepare --run"
    RUN = "bin/kin-stranger run --run"

    def rendered_lines(self, text: str) -> list[str]:
        """Every kin-stranger command line in a file, stripped of shell quoting."""

        lines = []
        for raw in text.splitlines():
            stripped = raw.strip()
            # A workflow renders its lines through `echo "..."`; a doc renders
            # them bare. Take the payload either way.
            match = re.fullmatch(r"""echo ["'](.*)["']""", stripped)
            if match:
                stripped = match.group(1)
            if "bin/kin-stranger " in stripped and not stripped.startswith("#"):
                lines.append(stripped)
        return lines

    def command_lines(self, text: str) -> list[str]:
        """Only the lines that render a command, not prose that names the tool."""

        return [
            line
            for line in self.rendered_lines(text)
            if re.search(r"bin/kin-stranger (prepare|run|resume) ", line)
        ]

    def test_the_doc_and_the_cut_summary_render_the_same_block(self) -> None:
        doc = self.command_lines(RELEASE_BOT_DOC.read_text(encoding="utf-8"))
        cut = self.command_lines(CUT_WORKFLOW_PATH.read_text(encoding="utf-8"))
        # The summary renders the same three lines the doc does. Anything the
        # workflow renders beyond them belongs to the standby job or to the job
        # actually driving the tool, which are not this assertion's business.
        self.assertTrue(doc, "docs/release-bot.md renders no kin-stranger command at all")
        for line in doc:
            self.assertIn(
                line,
                cut,
                "the cut summary and the doc have drifted; a reader is handed two "
                f"different commands for the same job: {line!r}",
            )

    def test_every_rendered_run_carries_a_run_id(self) -> None:
        """The defect itself. `run` without `--run` is refused by the tool."""

        for path in (RELEASE_BOT_DOC, CUT_WORKFLOW_PATH, SELECTOR_PATH):
            for line in self.command_lines(path.read_text(encoding="utf-8")):
                if "bin/kin-stranger run" not in line:
                    continue
                self.assertIn(
                    self.RUN,
                    line,
                    f"{path.name} renders a run with no --run id, which the tool "
                    f"refuses: {line!r}",
                )

    def test_a_rendered_run_is_always_preceded_by_a_prepare(self) -> None:
        """A run id has to come from somewhere, and `prepare` is the only source."""

        for path in (RELEASE_BOT_DOC, CUT_WORKFLOW_PATH):
            lines = self.command_lines(path.read_text(encoding="utf-8"))
            seen_prepare = False
            for line in lines:
                if line.startswith(self.PREPARE):
                    seen_prepare = True
                    continue
                if line.startswith(self.RUN):
                    self.assertTrue(
                        seen_prepare,
                        f"{path.name} renders a run before any prepare, so the run "
                        f"id it names was never created: {line!r}",
                    )

    def test_the_selector_still_renders_the_two_stage_form(self) -> None:
        """The positive control, and the shape the other two were corrected to.

        `stranger_command` has rendered prepare-then-run since it was written.
        If this ever goes red the needles above are wrong rather than the tree.
        """

        command = selector.stranger_command(VERSION, MIDDLE, 34_000_000_001)
        rendered = command.splitlines()
        self.assertTrue(
            any(line.startswith(self.PREPARE) for line in rendered),
            f"the selector stopped rendering a prepare line: {command}",
        )
        self.assertTrue(
            any(line.startswith(self.RUN) for line in rendered),
            f"the selector stopped rendering a run with a run id: {command}",
        )
        self.assertLess(
            next(i for i, l in enumerate(rendered) if l.startswith(self.PREPARE)),
            next(i for i, l in enumerate(rendered) if l.startswith(self.RUN)),
            f"the selector renders run before prepare: {command}",
        )


class ContractTests(unittest.TestCase):
    def test_the_required_contexts_match_the_mints_own_list(self) -> None:
        """A selector that picks a sha on a different list picks one the mint refuses."""

        source = RELEASE_TAG_PATH.read_text(encoding="utf-8")
        match = re.search(r"(?m)^          REQUIRED_CHECKS: \|\n((?:            .*\n)+)", source)
        self.assertIsNotNone(match, "release-tag.yml no longer declares REQUIRED_CHECKS as a block")
        # Only the block scalar's own 12-space body. Reading to the next key
        # would swallow the comment block between the two lists, which is how
        # this assertion first read eighteen prose lines as required contexts.
        mint = [line.strip() for line in match.group(1).splitlines() if line.strip()]
        self.assertEqual([name for name, _id, _path in selector.REQUIRED_CONTEXTS], mint)

    def test_the_provenance_table_matches_the_mints_own(self) -> None:
        source = RELEASE_TAG_PATH.read_text(encoding="utf-8")
        for name, workflow_id, path in selector.REQUIRED_CONTEXTS:
            pattern = (
                re.escape(f'"{name}": (')
                + r"\s*"
                + re.escape(f"{workflow_id:_}".replace("_", "_"))
                + r"?"
            )
            # The mint writes the id with underscore separators for the two
            # non-ci workflows and plain digits for ci.yml, so match either.
            plain = str(workflow_id)
            grouped = f"{workflow_id:,}".replace(",", "_")
            block = re.search(rf'(?ms)"{re.escape(name)}": \((.*?)\),\n', source)
            self.assertIsNotNone(block, f"release-tag.yml no longer binds {name}")
            body = block.group(1)
            self.assertTrue(
                plain in body or grouped in body,
                f"{name} is bound to a different workflow id in release-tag.yml: {body}",
            )
            self.assertIn(path, body, f"{name} is bound to a different workflow path")

    def test_every_emitted_output_is_republished_as_a_select_job_output(self) -> None:
        """A value decided here and dropped in the workflow is decided nowhere.

        `_emit` writes one line per key to GITHUB_OUTPUT, and only the keys
        release-cut.yml lists under the `select` job's `outputs:` reach any
        other job. `stranger_state` was added on both sides at once; this
        assertion is what stops a future key landing on one side alone.
        """

        source = CUT_WORKFLOW_PATH.read_text(encoding="utf-8")
        select = job_block(source, "select")
        for key in (*selector.OUTPUT_KEYS, "reason"):
            self.assertRegex(
                select,
                rf"(?m)^      {re.escape(key)}: \$\{{\{{ steps\.select\.outputs\.{re.escape(key)}",
                f"release-cut.yml's select job does not republish {key}",
            )

    def test_the_usable_artifacts_are_the_names_rc_build_actually_uploads(self) -> None:
        """The deadlock, as an assertion.

        `_usable` judges an rc-build run, so the names it requires have to be
        the names that run uploads. They were once the leg-record names
        release-cut.yml uploads into its own run, which no rc-build can carry,
        so no candidate could ever be proven and the preflight that would have
        produced those records was gated on the decision they blocked.
        """

        source = RC_BUILD_PATH.read_text(encoding="utf-8")
        build = job_block(source, "build")
        # Candidate-only native install probes add archives without expanding
        # the existing automatic preflight's three required platform legs.
        self.assertEqual(
            set(matrix_artifacts(build)),
            set(selector.RC_BUILD_ARTIFACTS) | {"kin-macos-x86_64", "kin-windows-x86_64"},
            "RC archive inventory must include every preflight and native install artifact",
        )
        # The row value is only the artifact name if the upload step names it
        # unadorned. A prefix added here would make every row a different
        # artifact than the selector asks for, silently.
        self.assertIn("          name: ${{ matrix.artifact }}\n", build)

    def test_the_preflight_downloads_exactly_the_artifacts_the_selector_requires(self) -> None:
        """The consumer side: each leg pulls one archive from the run by bare name."""

        source = CUT_WORKFLOW_PATH.read_text(encoding="utf-8")
        preflight = job_block(source, "preflight")
        self.assertEqual(
            sorted(matrix_artifacts(preflight)),
            sorted(selector.RC_BUILD_ARTIFACTS),
            "release-cut.yml's preflight matrix and the selector's usable set have drifted",
        )
        self.assertIn('--name "$ARTIFACT"', preflight)

    def test_the_leg_records_are_uploaded_by_the_cut_not_by_the_candidate_build(self) -> None:
        """Where the prefixed names live, so nobody re-derives the wrong owner.

        A reader who believes rc-build.yml uploads the leg records writes the
        selector's usable set as the prefixed names again. rc-build.yml uploads
        one thing, and release-cut.yml's preflight job uploads the other.
        """

        cut = CUT_WORKFLOW_PATH.read_text(encoding="utf-8")
        self.assertIn("          name: kin-release-preflight-${{ matrix.artifact }}\n", cut)
        self.assertIn("          pattern: kin-release-preflight-*\n", cut)
        rc = RC_BUILD_PATH.read_text(encoding="utf-8")
        self.assertNotIn("name: kin-release-preflight-", rc)

    def test_every_job_downstream_of_a_skipped_arm_survives_that_skip(self) -> None:
        """The second deadlock, as an assertion.

        `arm` runs only on the `arm` decision, so it is skipped on the proof
        path, and a skip propagates along the dependency chain to every job
        that does not override it. `preflight` overrides it with `always()`;
        `publish`, one link further down, did not, so on 2026-09-03 run
        33699387471 decided `proof`, all three preflight legs passed and filed
        their leg records, and the publisher skipped at once with zero steps.
        It had never been evaluated before, because no cycle had ever reached
        `proof`.

        The rule this pins: any job that can be reached from `arm` through
        `needs` has to carry a status function, or an upstream skip decides it.
        """

        source = CUT_WORKFLOW_PATH.read_text(encoding="utf-8")
        blocks = {name: job_block(source, name) for name in job_names(source)}
        self.assertIn("arm", blocks, "release-cut.yml no longer has an arm job")
        self.assertIn(
            "needs.select.outputs.decision == 'arm'",
            job_if(blocks["arm"]),
            "arm is no longer the conditional job this rule is about",
        )

        downstream: set[str] = set()
        changed = True
        while changed:
            changed = False
            for name, block in blocks.items():
                if name in downstream or name == "arm":
                    continue
                if any(need == "arm" or need in downstream for need in job_needs(block)):
                    downstream.add(name)
                    changed = True

        self.assertTrue(downstream, "nothing needs arm, so this rule guards nothing")
        for name in sorted(downstream):
            self.assertIn(
                "always()",
                job_if(blocks[name]),
                f"job {name!r} is downstream of the conditional arm job without always(), "
                "so an arm skip skips it whatever its own condition says",
            )

    def test_the_publisher_still_refuses_a_preflight_that_did_not_pass(self) -> None:
        """`always()` must not become a gate that publishes anything.

        The publisher survives an unrelated upstream skip, and nothing more: it
        still has to read the preflight matrix's own result as success, and it
        still only runs on the proof decision.
        """

        condition = job_if(job_block(CUT_WORKFLOW_PATH.read_text(encoding="utf-8"), "publish"))
        self.assertIn("needs.preflight.result == 'success'", condition)
        self.assertIn("needs.select.outputs.decision == 'proof'", condition)

    def test_the_publisher_records_the_token_scope_before_it_decides(self) -> None:
        """A refusal has to name its own evidence, and cannot be caused by naming it.

        The publisher proves its token against GET /repos before writing, and
        the push half of that check reads a block reporting the authenticated
        USER's permissions. What an App installation token gets there was
        guessed wrong twice on 2026-09-03 while four cycles refused with a
        message naming the verdict and never the evidence. So the scope is
        recorded first, and the recording is barred from failing the job:
        a diagnostic that can break the thing it explains is worse than none.
        """

        publish = job_block(CUT_WORKFLOW_PATH.read_text(encoding="utf-8"), "publish")
        record = publish.find("- name: Record what the evidence token can do")
        write = publish.find("- name: Publish preflight.json for the candidate")
        self.assertNotEqual(record, -1, "the publish job no longer records the token scope")
        self.assertNotEqual(write, -1, "the publish job no longer publishes")
        self.assertLess(
            record,
            write,
            "the token scope is recorded after the write it exists to explain, so a refusal "
            "at the write leaves no record of what the token looked like",
        )
        diagnostic = publish[record:write]
        self.assertIn("if: always()", diagnostic)
        self.assertIn("exit 0", diagnostic)
        self.assertNotIn("set -euo pipefail", diagnostic)

    def test_the_workflow_calls_the_selector_and_binds_its_trigger(self) -> None:
        workflow = CUT_WORKFLOW_PATH.read_text(encoding="utf-8")
        for needle in (
            "python3 scripts/select-release-candidate.py validate-trigger",
            "python3 scripts/select-release-candidate.py select",
            "python3 scripts/verify-protected-main-history.py",
            "node scripts/release-proof/merge-preflight-records.mjs",
            "scripts/release-proof/bin/kin-evidence-publish",
        ):
            self.assertIn(needle, workflow, f"release-cut.yml must run {needle}")

    def test_the_stranger_is_gated_on_a_variable_rather_than_a_runner_query(self) -> None:
        """A job whose labels match no online runner queues; it does not skip.

        GITHUB_TOKEN cannot list runners (`administration` is not among the
        permissions a workflow token can hold) and the release App carries
        contents, issues and pull-requests only, so live availability is not
        readable from inside the run. The switch is therefore explicit, and
        every path has to be covered: one job when it is set, one when it is not.
        """

        workflow = CUT_WORKFLOW_PATH.read_text(encoding="utf-8")
        jobs = dict(re.findall(r"(?ms)^  ([a-z0-9-]+):\n(.*?)(?=^  [a-z0-9-]+:\n|\Z)", workflow))
        self.assertIn("stranger", jobs)
        self.assertIn("stranger-standby", jobs)
        self.assertIn("vars.KIN_STRANGER_RUNNER != ''", jobs["stranger"])
        self.assertIn("vars.KIN_STRANGER_RUNNER == ''", jobs["stranger-standby"])
        self.assertIn("runs-on: ${{ vars.KIN_STRANGER_RUNNER }}", jobs["stranger"])
        # The standby path is the one that keeps a release moving without a
        # runner, so it has to carry the whole command rather than a pointer.
        for needle in ("bin/kin-stranger prepare", "--arms green,brown,vcs", "--candidate-sha", "::warning::"):
            self.assertIn(needle, jobs["stranger-standby"], f"the standby path must print {needle}")

    def test_the_stranger_refuses_before_spending_an_arm_on_a_missing_credential(self) -> None:
        workflow = CUT_WORKFLOW_PATH.read_text(encoding="utf-8")
        jobs = dict(re.findall(r"(?ms)^  ([a-z0-9-]+):\n(.*?)(?=^  [a-z0-9-]+:\n|\Z)", workflow))
        stranger = jobs["stranger"]
        self.assertIn("KIN_STRANGER_ANTHROPIC_API_KEY", stranger)
        self.assertIn('if [ -z "${STRANGER_KEY:-}" ]; then', stranger)
        # An interrupted run resumes rather than re-preparing: `prepare` refuses
        # a reused container without --force because a reused container tests an
        # upgrade path, which is a different question.
        self.assertIn("bin/kin-stranger resume", stranger)
        # Read the executable lines only. A bare substring search cannot tell a
        # comment explaining why --force is wrong from a command using it, and
        # would fail on the explanation that keeps the decision reviewable.
        active = [
            line for line in stranger.splitlines()
            if line.strip() and not line.strip().startswith("#")
        ]
        self.assertFalse(
            [line for line in active if "--force" in line],
            "the stranger job must never force a reused container: that tests an upgrade path",
        )

    def test_the_stranger_never_publishes_and_the_publisher_never_drives(self) -> None:
        """The App key must not reach the machine running a Claude driver on candidate bytes."""

        workflow = CUT_WORKFLOW_PATH.read_text(encoding="utf-8")
        jobs = dict(re.findall(r"(?ms)^  ([a-z0-9-]+):\n(.*?)(?=^  [a-z0-9-]+:\n|\Z)", workflow))
        self.assertNotIn("KIN_RELEASE_BOT_PRIVATE_KEY", jobs["stranger"])
        self.assertNotIn("kin-evidence-publish", jobs["stranger"])
        self.assertNotIn("environment:", jobs["stranger"])
        self.assertIn("environment: release-tag", jobs["publish-stranger"])
        self.assertIn("kin-evidence-publish", jobs["publish-stranger"])
        self.assertNotIn("ANTHROPIC_API_KEY", jobs["publish-stranger"])
        # --require-archive is what binds the record to the bytes the preflight
        # judged, checked at write time rather than by the gate at release time.
        # Read the executable lines only: the comment above that step explains
        # the flag, and a bare substring search is satisfied by the explanation
        # alone, so removing the flag itself left this assertion green.
        publisher_active = [
            line for line in jobs["publish-stranger"].splitlines()
            if line.strip() and not line.strip().startswith("#")
        ]
        self.assertTrue(
            [line for line in publisher_active if "--require-archive" in line],
            "the stranger record must be published with --require-archive, which binds it "
            "to an archive the published preflight actually judged",
        )

    def test_the_workflow_keeps_the_release_app_off_the_proof_runners(self) -> None:
        """The preflight job downloads and judges; only the publish job may write."""

        workflow = CUT_WORKFLOW_PATH.read_text(encoding="utf-8")
        jobs = dict(re.findall(r"(?ms)^  ([a-z0-9-]+):\n(.*?)(?=^  [a-z0-9-]+:\n|\Z)", workflow))
        self.assertIn("preflight", jobs)
        self.assertNotIn("KIN_RELEASE_BOT_PRIVATE_KEY", jobs["preflight"])
        self.assertNotIn("environment:", jobs["preflight"])
        for job in ("arm", "publish"):
            self.assertIn(job, jobs)
            self.assertIn("environment: release-tag", jobs[job])

    def test_the_workflow_declares_only_trusted_triggers(self) -> None:
        workflow = CUT_WORKFLOW_PATH.read_text(encoding="utf-8")
        header = re.search(r"(?ms)^on:\n(.*?)(?=^[a-z])", workflow)
        self.assertIsNotNone(header)
        self.assertNotIn("workflow_dispatch", header.group(1), "a dispatch could select branch-controlled code")
        self.assertNotIn("pull_request", header.group(1))
        for needle in ("workflow_run:", "repository_dispatch:", "schedule:", selector.KICK_ACTION):
            self.assertIn(needle, header.group(1))
        # The landing push, on main alone: a push run resolves this workflow
        # from the branch pushed, and validate_trigger refuses any other ref.
        self.assertRegex(header.group(1), r"(?m)^  push:\n    branches: \[main\]\n  workflow_run:\n")

    def gate_source(self) -> str:
        source = RELEASE_TAG_PATH.read_text(encoding="utf-8")
        start = source.index("      - name: Verify required checks are green")
        step = source[start:source.index("\n      - name:", start + 1)]
        return step[step.index("python3 - <<'PY'\n"):]

    def test_the_mint_reads_an_export_head_under_the_selectors_events(self) -> None:
        """The cut and the mint must agree on which build of the head is evidence."""

        table = re.search(r"(?s)export_pull_events = (\{.*?\})\n", self.gate_source())
        self.assertIsNotNone(table, "release-tag.yml's gate no longer declares export_pull_events")
        self.assertEqual(ast.literal_eval(table.group(1)), selector.EXPORT_PULL_EVENTS)
        self.assertIn(f'.startswith("{selector.EXPORT_BRANCH_PREFIX}")', self.gate_source())

    def test_the_mint_tells_a_timeout_from_a_cancel_by_the_selectors_texts(self) -> None:
        """A retry the selector admits and the mint refuses arms a sha that cannot be tagged."""

        gate = self.gate_source()
        timeout = re.search(r'(?s)timeout_annotation = re\.compile\(\s*(r".*?")\s*(r".*?")\s*\)', gate)
        self.assertIsNotNone(timeout, "release-tag.yml's gate no longer declares timeout_annotation")
        self.assertEqual(
            ast.literal_eval(timeout.group(1)) + ast.literal_eval(timeout.group(2)),
            selector.TIMEOUT_ANNOTATION.pattern,
        )
        others = re.search(r"(?s)other_cancel_annotations = \((.*?)\n\s*\)\n", gate)
        self.assertIsNotNone(others, "release-tag.yml's gate no longer declares other_cancel_annotations")
        self.assertEqual(
            [ast.literal_eval(literal) for literal in re.findall(r'r"[^"]*"', others.group(1))],
            [pattern.pattern for pattern in selector.OTHER_CANCEL_ANNOTATIONS],
        )

    def test_the_cut_tells_the_queue_under_the_selectors_context(self) -> None:
        """The queue requires a context by name; a renamed one is waited on forever."""

        report = job_block(CUT_WORKFLOW_PATH.read_text(encoding="utf-8"), "report-to-queue")
        self.assertIn(f'-f context="{selector.QUEUE_STATUS_CONTEXT}"', report)
        self.assertIn("    permissions:\n      statuses: write\n", report)
        self.assertIn("if: always() && needs.select.outputs.queue_sha != ''", report)

    def test_the_cut_wakes_on_the_queues_builds(self) -> None:
        select = job_block(CUT_WORKFLOW_PATH.read_text(encoding="utf-8"), "select")
        self.assertIn("github.event.workflow_run.event == 'merge_group'", select)
        self.assertIn("startsWith(github.event.workflow_run.head_branch, 'gh-readonly-queue/main/')", select)
        self.assertIn('queue=(--queue-sha "$QUEUE_SHA")', select)

    def test_the_mint_vetoes_on_the_selectors_acceptance_contexts(self) -> None:
        table = re.search(r"(?s)acceptance_provenance = (\{.*?\n          \})\n", self.gate_source())
        self.assertIsNotNone(table, "release-tag.yml's gate no longer declares acceptance_provenance")
        self.assertEqual(
            ast.literal_eval(textwrap_dedent(table.group(1))),
            {name: (workflow_id, path) for name, workflow_id, path in selector.ACCEPTANCE_CONTEXTS},
        )

    def test_a_usable_run_holds_every_artifact_release_yml_ships(self) -> None:
        """The selector's usable set is the locator's, or a filed record strands the tag.

        scripts/locate-rc-build.mjs and RC Build's daemon image job land with
        the build-once change; until they are in this tree only the archive
        half of the set can be read off the build matrix.
        """

        build = job_block(RC_BUILD_PATH.read_text(encoding="utf-8"), "build")
        self.assertLessEqual(set(matrix_artifacts(build)), set(selector.RC_RELEASE_ARTIFACTS))
        self.assertLessEqual(set(selector.RC_BUILD_ARTIFACTS), set(selector.RC_RELEASE_ARTIFACTS))
        locator = ROOT / "scripts" / "locate-rc-build.mjs"
        if not locator.exists():
            self.skipTest("scripts/locate-rc-build.mjs lands with the build-once change")
        source = locator.read_text(encoding="utf-8")
        declared = source.split("export const RC_ARTIFACTS", 1)[0]
        named = re.findall(r'(?m)^\s*\{?\s*artifact: "([a-z0-9_-]+)"', declared)
        self.assertIn("kin-daemon-image", named, "the locator no longer names the daemon image artifact")
        self.assertEqual(sorted(named), sorted(selector.RC_RELEASE_ARTIFACTS))

    def test_the_build_jobs_carry_the_name_the_timeout_rule_reads(self) -> None:
        """A renamed build job would make every timeout read as a spent attempt again."""

        build = job_block(RC_BUILD_PATH.read_text(encoding="utf-8"), "build")
        self.assertIn(f"    name: {selector.RC_BUILD_JOB_PREFIX}${{{{ matrix.artifact }}}})\n", build)


class CommandLineTests(unittest.TestCase):
    def run_selector(self, *arguments: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(SELECTOR_PATH), *arguments],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

    def test_a_snapshot_is_judged_and_its_decision_printed(self) -> None:
        document = snapshot()
        document["grades"] = {NEWEST: green_grade(NEWEST)}
        with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as handle:
            json.dump(document, handle)
            path = handle.name
        result = self.run_selector("select", "--snapshot", path)
        self.assertEqual(result.returncode, 0, result.stderr)
        decision = json.loads(result.stdout)
        self.assertEqual(decision["decision"], selector.ARM)
        self.assertEqual(decision["candidate"], NEWEST)

    def test_a_refusal_exits_zero_and_a_broken_read_exits_two(self) -> None:
        """A refusal is a hold and exits 0 with a warning; an unreadable snapshot still fails."""

        document = snapshot()
        document["grades"] = {
            sha: with_duplicate(green_grade(sha), "Falsify guards", CI_SUITE)
            for sha in document["range"]
        }
        with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as handle:
            json.dump(document, handle)
            path = handle.name
        refusal = self.run_selector("select", "--snapshot", path)
        self.assertEqual(refusal.returncode, 0, refusal.stdout)
        self.assertEqual(json.loads(refusal.stdout)["decision"], selector.REFUSE)
        self.assertIn("::warning title=Release cut refused", refusal.stderr)

        document.pop("grades")
        with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as handle:
            json.dump(document, handle)
            broken = handle.name
        unreadable = self.run_selector("select", "--snapshot", broken)
        self.assertEqual(unreadable.returncode, 2, unreadable.stdout)
        self.assertIn("could not decide", unreadable.stderr)

    def test_validate_trigger_exits_zero_only_on_an_admitted_trigger(self) -> None:
        with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as handle:
            json.dump(completion_event(), handle)
            event = handle.name
        admitted = self.run_selector(
            "validate-trigger",
            "--event-file", event,
            "--event-name", "workflow_run",
            "--event-action", "completed",
            "--actor", "troyjr4103",
            "--repository", REPO,
            "--default-branch", "main",
            "--ref", "refs/heads/main",
            "--workflow-sha", NEWEST,
        )
        self.assertEqual(admitted.returncode, 0, admitted.stderr)
        refused = self.run_selector(
            "validate-trigger",
            "--event-file", event,
            "--event-name", "workflow_run",
            "--event-action", "completed",
            "--actor", "troyjr4103",
            "--repository", REPO,
            "--default-branch", "main",
            "--ref", "refs/heads/chore/lane",
            "--workflow-sha", NEWEST,
        )
        self.assertEqual(refused.returncode, 2)


if __name__ == "__main__":
    unittest.main(verbosity=2)
