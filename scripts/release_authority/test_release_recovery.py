# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Checks that pin .github/workflows/release-recovery.yml."""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import textwrap
from pathlib import Path

from .test_helpers import (
    RELEASE_GATE_FIXTURE_SHA,
    RELEASE_RECOVERY,
    expect_assertion,
    job_step_active_lines,
    require,
)


def recovery_step_script(release_recovery: str, anchor: str) -> str:
    """Extract a recovery step's script exactly as the controller runs it."""

    if anchor not in release_recovery:
        raise AssertionError(
            f"release-recovery no longer carries {anchor.strip()!r}"
        )
    start = release_recovery.index(anchor)
    remainder = release_recovery[start + 1 :]
    end = (
        release_recovery.index("\n      - name:", start + 1)
        if "\n      - name:" in remainder
        else len(release_recovery)
    )
    step = release_recovery[start:end]
    marker = "        run: |\n"
    return textwrap.dedent(step[step.index(marker) + len(marker) :])


def assert_recovery_closes_only_shipped_alarms(release_recovery: str) -> None:
    script = recovery_step_script(
        release_recovery,
        "      - name: Close blocked-release alarms whose tag has since shipped\n",
    )
    with tempfile.TemporaryDirectory(prefix="kin-recovery-close-") as directory:
        root = Path(directory)
        gh = root / "gh"
        gh.write_text(
            "#!/usr/bin/env python3\n"
            "import argparse, json, os, pathlib, sys\n"
            "args = sys.argv[1:]\n"
            "if args[:2] == ['issue', 'list']:\n"
            "    parser = argparse.ArgumentParser()\n"
            "    for flag in ('--repo', '--label', '--state', '--limit', '--json', '--jq'):\n"
            "        parser.add_argument(flag)\n"
            "    parser.parse_args(args[2:])\n"
            "    if os.environ.get('LIST_FAIL') == '1': sys.exit(23)\n"
            "    print(os.environ['ISSUES'])\n"
            "elif args[0] == 'api':\n"
            "    print(os.environ['RUNS'])\n"
            "elif args[:2] == ['issue', 'close']:\n"
            "    with open(os.environ['CLOSED'], 'a') as f: f.write(json.dumps(args) + '\\n')\n"
            "else: sys.exit('unexpected gh invocation: ' + repr(args))\n",
            encoding="utf-8",
        )
        gh.chmod(0o755)
        closed = root / "closed.jsonl"
        prefix = "Release blocked after automatic retries: "
        issues = [
            {"number": 1, "title": prefix + "v0.7.17"},
            {"number": 2, "title": prefix + "v0.7.18"},
            {"number": 3, "title": "Unrelated v0.7.17"},
            {"number": 4, "title": prefix + "not-a-tag"},
        ]
        shipped = {
            "head_branch": "v0.7.17", "event": "push", "status": "completed",
            "conclusion": "success", "path": ".github/workflows/release.yml",
            "created_at": "2026-01-01T00:00:00Z", "html_url": "https://example.test/release/17",
        }
        env = {
            **os.environ, "PATH": str(root) + os.pathsep + os.environ["PATH"],
            "GH_TOKEN": "fixture", "ALARM_TOKEN": "fixture", "ALARM_REPO": "fixture/infra", "LIST_FAIL": "0",
            "ALARM_LABEL": "kin", "REPO": "fixture/kin", "CLOSED": str(closed),
            "ISSUES": json.dumps(issues), "RUNS": json.dumps({"workflow_runs": [shipped]}),
        }
        cases = [
            ({}, ["1"], 0),
            ({"ISSUES": "[]"}, [], 0),
            ({"LIST_FAIL": "1"}, [], 23),
        ]
        for field, value in (("conclusion", "failure"), ("status", "in_progress"),
                             ("event", "workflow_dispatch"), ("path", "other.yml")):
            cases.append(({"RUNS": json.dumps({"workflow_runs": [{**shipped, field: value}]})}, [], 0))
        for overrides, expected, code in cases:
            closed.unlink(missing_ok=True)
            result = subprocess.run(["bash", "-c", script], env={**env, **overrides},
                                    capture_output=True, text=True)
            assert result.returncode == code, result.stderr
            calls = [json.loads(line) for line in closed.read_text().splitlines()] if closed.exists() else []
            assert [call[2] for call in calls] == expected, calls
            if calls:
                assert shipped["html_url"] in calls[0][-1]
        broken = script.replace(' | jq -r --arg p "$prefix"', ' \\\n  --jq --arg p "$prefix"')
        assert broken != script, "recovery alarm filter mutation did not apply"
        result = subprocess.run(["bash", "-c", broken], env=env, capture_output=True, text=True)
        assert result.returncode != 0 and "argument --jq" in result.stderr, result.stderr


def recovery_escalation_source(release_recovery: str) -> str:
    """Extract the escalation step exactly as the recovery controller runs it."""

    return recovery_step_script(
        release_recovery,
        "      - name: Alert after automatic retries are exhausted\n",
    )


RECOVERY_CLASSIFIER_ANCHOR = (
    "      - name: Classify the failure signature across attempts\n"
)
RECOVERY_FIXTURE_RUN_ID = "4242"


def write_attempt_job_fixtures(
    responses: Path,
    attempt_signatures: list[object],
) -> None:
    """Write one jobs-API response per attempt in the shape GitHub returns.

    A failure entry is `(job, step)`, or `(job, step, job_conclusion)` when the
    job's own conclusion differs from its failing step's — the shape a matrix
    leg takes when a sibling's failure cancels it mid-step. A `None` step is a
    job that failed while recording no failing step at all.
    """

    for index, signature in enumerate(attempt_signatures, start=1):
        if signature is None:
            continue
        # A single failing job makes the jobs API's ordering irrelevant,
        # which is the shape production never produces for a matrix
        # failure. Every attempt therefore carries its failures in the
        # order given, so a caller can vary it the way real queue-time job
        # ids do.
        failures = [signature] if isinstance(signature, tuple) else list(signature)
        jobs: list[dict[str, object]] = [
            {
                "name": "Preflight",
                "conclusion": "success",
                "steps": [{"name": "Checkout", "conclusion": "success"}],
            }
        ]
        for failure in failures:
            job, step_name = failure[0], failure[1]
            job_conclusion = failure[2] if len(failure) > 2 else "failure"
            steps: list[dict[str, object]] = [
                {"name": "Checkout", "conclusion": "success"}
            ]
            if step_name is not None:
                steps.append({"name": step_name, "conclusion": "failure"})
            jobs.append(
                {
                    "name": job,
                    "conclusion": job_conclusion,
                    "steps": steps,
                }
            )
        payload = {"jobs": jobs}
        target = (
            f"repos_kinlab-ai_kin_actions_runs_{RECOVERY_FIXTURE_RUN_ID}"
            f"_attempts_{index}_jobs"
        )
        (responses / target).write_text(json.dumps(payload), encoding="utf-8")


def read_step_outputs(path: Path) -> dict[str, str]:
    outputs: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if "=" in line:
            key, _, value = line.partition("=")
            outputs[key] = value
    return outputs


def run_recovery_classifier_in(
    root: Path,
    source: str,
    attempt_signatures: list[object],
) -> dict[str, str]:
    """Run the classifier inside a prepared fixture root and return its outputs.

    The root must already carry `responses/` and the `gh` stub in `bin/`. The
    classifier's signature file lands in this same directory because the alert
    step reads it from RUNNER_TEMP, which is how the two steps share one
    classification inside a single job.

    The classifier decides whether a retry is spent, so it must never fail the
    reconcile job: a controller that dies here neither retries nor alerts, and
    the release lane silently stops moving. That is asserted on every run
    rather than in one dedicated case.
    """

    script = root / "classify.sh"
    script.write_text(source, encoding="utf-8")
    outputs_file = root / "classifier-output"
    outputs_file.write_text("", encoding="utf-8")
    environment = dict(os.environ)
    environment["PATH"] = f"{root / 'bin'}{os.pathsep}{environment['PATH']}"
    environment.update(
        {
            "FIXTURE": str(root),
            "GH_TOKEN": "fixture",
            "REPO": "kinlab-ai/kin",
            "RUN_ID": RECOVERY_FIXTURE_RUN_ID,
            "ATTEMPTS": str(len(attempt_signatures)),
            "RUNNER_TEMP": str(root),
            "GITHUB_OUTPUT": str(outputs_file),
        }
    )
    completed = subprocess.run(
        ["bash", str(script)],
        capture_output=True,
        text=True,
        env=environment,
        timeout=60,
        check=False,
    )
    if completed.returncode != 0:
        raise AssertionError(
            "the recovery classifier must never fail the reconcile job, "
            "because a controller that dies here neither retries nor alerts: "
            f"{completed.stdout}{completed.stderr}"
        )
    return read_step_outputs(outputs_file)


def execute_recovery_classifier(
    source: str,
    attempt_signatures: list[object],
) -> dict[str, str]:
    """Run the classifier against scripted attempt jobs and return its outputs."""

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        responses = root / "responses"
        responses.mkdir()
        write_attempt_job_fixtures(responses, attempt_signatures)
        binaries = root / "bin"
        binaries.mkdir()
        write_recovery_gh_stub(binaries)
        return run_recovery_classifier_in(root, source, attempt_signatures)


def write_recovery_gh_stub(binaries: Path) -> None:
    """A `gh` that answers `-q` with the real `jq`.

    A stub that returned canned answers would exercise the shell and leave the
    jq that does the classifying unproven.
    """

    (binaries / "gh").write_text(
        textwrap.dedent(
            """\
                #!/usr/bin/env bash
                set -uo pipefail
                case "$1" in
                  api)
                    shift
                    path=""; query=""; slurp=0
                    while [ $# -gt 0 ]; do
                      case "$1" in
                        -q|--jq) shift; query="$1" ;;
                        --slurp) slurp=1 ;;
                        -*) ;;
                        *) [ -n "$path" ] || path="$1" ;;
                      esac
                      shift
                    done
                    path="${path%%\\?*}"
                    file="$FIXTURE/responses/$(printf '%s' "$path" | tr '/' '_')"
                    if [ -f "$file.error" ]; then
                      cat "$file.error" >&2
                      exit 1
                    fi
                    if [ ! -f "$file" ]; then
                      echo "gh: Not Found (HTTP 404)" >&2
                      exit 1
                    fi
                    # `--slurp` returns an array of page responses, which is
                    # the shape the caller's query has to handle.
                    if [ "$slurp" = 1 ]; then
                      body="$(jq -c '[.]' < "$file")"
                    else
                      body="$(cat "$file")"
                    fi
                    if [ -n "$query" ]; then
                      jq -r "$query" <<< "$body"
                    else
                      printf '%s\n' "$body"
                    fi
                    ;;
                  label)
                    # The alert creates its source-repository label on the
                    # alarm repository before the issue that carries it.
                    shift
                    if [ "$1" = create ]; then
                      printf '%s' "$2" > "$FIXTURE/label-created"
                    fi
                    exit 0
                    ;;
                  issue)
                    shift
                    case "$1" in
                      list) exit 0 ;;
                      create)
                        shift
                        # The token the create authenticated with is the one
                        # fact that separates an alarm filed through the
                        # release App from one that fell back to the workflow
                        # token, so it is recorded beside the arguments.
                        printf '%s' "${GH_TOKEN:-}" > "$FIXTURE/issue-token"
                        while [ $# -gt 0 ]; do
                          case "$1" in
                            --title) shift; printf '%s' "$1" > "$FIXTURE/issue-title" ;;
                            --body-file) shift; cp "$1" "$FIXTURE/issue-body" ;;
                            --repo) shift; printf '%s' "$1" > "$FIXTURE/issue-repo" ;;
                            --label) shift; printf '%s' "$1" > "$FIXTURE/issue-label" ;;
                          esac
                          shift
                        done
                        ;;
                    esac
                    ;;
                esac
                """
        ),
        encoding="utf-8",
    )
    (binaries / "gh").chmod(0o755)


def execute_recovery_escalation(
    release_recovery: str,
    attempt_signatures: list[object],
    *,
    release: dict[str, object] | None = None,
    release_error: str | None = None,
) -> tuple[subprocess.CompletedProcess[str], str]:
    """Run the reconcile's classify and alert steps the way the job runs them.

    The alert no longer computes the signature itself; it reports what the
    classifier that spent or withheld the retry decided. Driving the two steps
    in order, through one runner temp directory and the classifier's real step
    outputs, is what keeps this a test of the controller rather than of a
    hand-copied duplicate of its logic.
    """

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        script = root / "escalate.sh"
        script.write_text(
            recovery_escalation_source(release_recovery), encoding="utf-8"
        )
        responses = root / "responses"
        responses.mkdir()
        write_attempt_job_fixtures(responses, attempt_signatures)
        binaries = root / "bin"
        binaries.mkdir()
        write_recovery_gh_stub(binaries)
        # The real classifier, run first and into this same runner temp, so the
        # alert reads the signature file and outputs the controller produced.
        classifier_outputs = run_recovery_classifier_in(
            root,
            recovery_step_script(release_recovery, RECOVERY_CLASSIFIER_ANCHOR),
            attempt_signatures,
        )
        release_response = responses / "repos_kinlab-ai_kin_releases_tags_v9.9.9"
        if release is not None and release_error is not None:
            raise AssertionError("release fixture cannot be both readable and failed")
        if release is not None:
            release_response.write_text(
                json.dumps(release),
                encoding="utf-8",
            )
        if release_error is not None:
            Path(f"{release_response}.error").write_text(
                release_error,
                encoding="utf-8",
            )
        environment = dict(os.environ)
        environment["PATH"] = f"{binaries}{os.pathsep}{environment['PATH']}"
        environment.update(
            {
                "FIXTURE": str(root),
                "GH_TOKEN": "fixture",
                "REPO": "kinlab-ai/kin",
                "TAG": "v9.9.9",
                "SHA": RELEASE_GATE_FIXTURE_SHA,
                "RUN_ID": RECOVERY_FIXTURE_RUN_ID,
                "RUN_URL": (
                    f"https://example.invalid/runs/{RECOVERY_FIXTURE_RUN_ID}"
                ),
                "ATTEMPTS": str(len(attempt_signatures)),
                "RUNNER_TEMP": str(root),
                # resolve declares exhaustion at the third attempt; the alert
                # distinguishes that from a repeat stop that left budget unspent.
                "EXHAUSTED": "true" if len(attempt_signatures) >= 3 else "false",
                "UNKNOWN": classifier_outputs.get("unknown", ""),
                "REPEATED": classifier_outputs.get("repeated", ""),
                "DISTINCT": classifier_outputs.get("distinct", ""),
                # The alarm lives on kin-infra and is filed through the App
                # token the job minted for it, never through the workflow
                # token above. Distinct fixture values, so the recorded create
                # can only match by using each for what it is for.
                "ALARM_TOKEN": RECOVERY_ALARM_FIXTURE_TOKEN,
                "ALARM_REPO": RECOVERY_ALARM_FIXTURE_REPO,
                "ALARM_LABEL": RECOVERY_ALARM_FIXTURE_LABEL,
            }
        )
        completed = subprocess.run(
            ["bash", str(script)],
            capture_output=True,
            text=True,
            env=environment,
            timeout=60,
            check=False,
        )
        body_path = root / "issue-body"
        body = body_path.read_text(encoding="utf-8") if body_path.exists() else ""
        if body_path.exists():
            assert_recovery_alert_filed_on_alarm_repository(root)
        return completed, body


RECOVERY_ALARM_FIXTURE_TOKEN = "alarm-fixture-token"
RECOVERY_ALARM_FIXTURE_REPO = "kinlab-ai/kin-infra"
RECOVERY_ALARM_FIXTURE_LABEL = "kin"


def assert_recovery_alert_filed_on_alarm_repository(root: Path) -> None:
    """Every alert the recovery controller files lands on kin-infra, labelled
    with the source repository, through the alarm token and no other.

    Read off what the stub recorded rather than off the workflow text, so a
    create that quietly went back to `--repo "$REPO"` or to the workflow token
    fails here even when every string the text checks pin is still present.
    """

    recorded = {
        name: (root / name).read_text(encoding="utf-8") if (root / name).exists() else None
        for name in ("issue-repo", "issue-label", "issue-token", "label-created")
    }
    expected = {
        "issue-repo": RECOVERY_ALARM_FIXTURE_REPO,
        "issue-label": RECOVERY_ALARM_FIXTURE_LABEL,
        "issue-token": RECOVERY_ALARM_FIXTURE_TOKEN,
        "label-created": RECOVERY_ALARM_FIXTURE_LABEL,
    }
    for name, want in expected.items():
        if recorded[name] != want:
            raise AssertionError(
                "recovery alert was not filed on the alarm repository through the "
                f"alarm token: {name} recorded {recorded[name]!r}, expected {want!r}"
            )


def assert_recovery_escalation_classifies(release_recovery: str) -> None:
    """Recovery must classify before advising without inventing root-cause proof.

    A repeated job/step surface is useful evidence that another immediate rerun
    is unlikely to help, but it cannot distinguish a source-bound failure from
    an external outage that repeatedly reaches the same step. The issue must
    preserve that distinction before it tells an operator to burn a version.
    """

    compile_failure = ("build-artifacts (x86_64-unknown-linux-musl)", "Build release binaries")

    repeated, body = execute_recovery_escalation(
        release_recovery,
        [compile_failure, compile_failure, compile_failure],
    )
    if repeated.returncode == 0:
        raise AssertionError(
            "recovery escalation stopped failing the run: "
            f"{repeated.stdout}{repeated.stderr}"
        )
    for needle in (
        "Classification: **repeated failure signature**",
        # The strongest claim the alert can make, and it is only earned when
        # every attempt really did produce one signature. Pinning the sentence
        # itself is what keeps the all-attempts count from being deleted as
        # redundant to the last-two stop rule it does not duplicate.
        "Every observed attempt failed in the same job and step set",
        "another immediate rerun is unlikely to help",
        "recut only after confirming a source-bound defect",
        "preserve this tag for a same-release rerun if the cause is external",
        "- attempt 1: build-artifacts (x86_64-unknown-linux-musl) / Build release binaries",
        "after 3 attempt(s)",
        "No GitHub Release object exists for `v9.9.9`.",
    ):
        if needle not in body:
            raise AssertionError(
                f"identical repeated signatures were not reported: {needle!r}: {body}"
            )
    if "a rerun cannot reach a different outcome" in body:
        raise AssertionError(
            f"step identity was overstated as root-cause proof: {body}"
        )

    # The only shape that can reach the exhausted alert with a repeat verdict.
    # The controller stops at two identical attempts, so a run that spent all
    # three necessarily failed differently first, and the verdict text has to
    # describe the equality the classifier actually established rather than
    # claiming every attempt matched. Claiming it would contradict the
    # per-attempt list three lines below it and send an operator past attempt 1,
    # which is the evidence that the cause is not purely deterministic.
    _, tail_repeat_body = execute_recovery_escalation(
        release_recovery,
        [("publish", "Push image"), compile_failure, compile_failure],
    )
    for needle in (
        "Classification: **repeated failure signature**",
        "The two most recent attempts failed in the same job and step set",
        "Earlier attempts failed differently",
        "- attempt 1: publish / Push image",
        "- attempt 3: build-artifacts (x86_64-unknown-linux-musl) / Build release binaries",
    ):
        if needle not in tail_repeat_body:
            raise AssertionError(
                "a repeat established only across the last two attempts was not "
                f"reported as one: {needle!r}: {tail_repeat_body}"
            )
    if "Every observed attempt failed in the same job and step set" in tail_repeat_body:
        raise AssertionError(
            "the alert claimed every attempt failed identically while its own "
            f"per-attempt list shows attempt 1 differing: {tail_repeat_body}"
        )

    # The stop that spends one confirmation instead of the whole budget. The
    # alert still fires, and it must not describe unspent budget as exhausted:
    # the remedy for a repeat differs from the remedy for a run that tried
    # everything, and the tag is still eligible for a same-release rerun.
    stopped, stopped_body = execute_recovery_escalation(
        release_recovery,
        [compile_failure, compile_failure],
    )
    if stopped.returncode == 0:
        raise AssertionError(
            "a confirmed repeat did not raise the release alarm: "
            f"{stopped.stdout}{stopped.stderr}"
        )
    for needle in (
        "Classification: **repeated failure signature**",
        "stopped retrying",
        "after 2 attempt(s)",
    ):
        if needle not in stopped_body:
            raise AssertionError(
                f"a repeat stop was not reported as one: {needle!r}: {stopped_body}"
            )
    if "exhausted its retries" in stopped_body:
        raise AssertionError(
            "a repeat stop that left retry budget unspent was reported as "
            f"exhaustion: {stopped_body}"
        )

    _, transient_body = execute_recovery_escalation(
        release_recovery,
        [
            ("build-artifacts (x86_64-apple-darwin)", "Notarize"),
            ("publish", "Push image"),
            compile_failure,
        ],
    )
    for needle in (
        "Classification: **varying failure signatures**",
        "Diagnose and rerun the same release",
    ):
        if needle not in transient_body:
            raise AssertionError(
                f"differing failures lost the rerun advice: {needle!r}: "
                f"{transient_body}"
            )

    # An attempt whose jobs cannot be read is not evidence that the failure
    # repeated. Claiming deterministic from an unreadable attempt would retire
    # a recoverable release on a transient API error.
    _, unreadable_body = execute_recovery_escalation(
        release_recovery,
        [compile_failure, None, compile_failure],
    )
    for needle in (
        "Classification: **indeterminate**",
        "- attempt 2: unrecorded",
        "neither a deterministic nor a transient cause is established",
        "Inspect the unrecorded attempts",
    ):
        if needle not in unreadable_body:
            raise AssertionError(
                "an unreadable attempt was folded into a deterministic verdict: "
                f"{needle!r}: {unreadable_body}"
            )
    if "The attempts did not fail identically" in unreadable_body:
        raise AssertionError(
            "an unreadable attempt was reported as an observed difference: "
            f"{unreadable_body}"
        )

    # The shape production actually produces. Release builds are a matrix, so
    # more than one leg fails, and the jobs API orders by job id, which is
    # minted at queue time. Real attempts of run 30627672394 returned the two
    # failing legs in the order below: aarch64 first, then x86_64 first, then
    # aarch64 again. Reducing the failing set to its first element reads that
    # as two different failures and advises a rerun that cannot work, which
    # is verbatim the advice this suite exists to eliminate.
    aarch64 = (
        "Build (kin-linux-aarch64)",
        "Build kin-cli + kin-daemon (native)",
    )
    x86_64 = (
        "Build (kin-linux-x86_64)",
        "Build kin-cli + kin-daemon (native)",
    )
    _, matrix_body = execute_recovery_escalation(
        release_recovery,
        [[aarch64, x86_64], [x86_64, aarch64], [aarch64, x86_64]],
    )
    if "Classification: **repeated failure signature**" not in matrix_body:
        raise AssertionError(
            "a matrix failure that repeated identically was classified by the "
            f"order its legs happened to queue in: {matrix_body}"
        )

    # Every attempt unreadable is the case that isolates the unknown count.
    # The signatures all match each other, so nothing but that count stands
    # between a total read failure and a verdict of "deterministic, recut it".
    _, blind_body = execute_recovery_escalation(release_recovery, [None, None, None])
    if "Classification: **indeterminate**" not in blind_body:
        raise AssertionError(
            "a run whose attempts could not be read at all was classified "
            f"deterministic: {blind_body}"
        )

    # A mint that reported failure on a release it had already created would
    # otherwise send this controller to repair a healthy release.
    _, existing_release_body = execute_recovery_escalation(
        release_recovery,
        [compile_failure, compile_failure, compile_failure],
        release={
            "draft": False,
            "prerelease": True,
            "assets": [{"name": "kin-x86_64.tar.gz"}],
        },
    )
    if (
        "already exists with 1 asset(s)" not in existing_release_body
        or "prerelease=true" not in existing_release_body
        or "Confirm it is genuinely incomplete before repairing it."
        not in existing_release_body
    ):
        raise AssertionError(
            "recovery advised repair without saying the release already exists: "
            f"{existing_release_body}"
        )

    # Only a real 404 proves absence. A permissions, rate-limit, server, or
    # network failure is unknown state and must never be rewritten as "no
    # Release", which would send repair after an object that may exist.
    _, unreadable_release_body = execute_recovery_escalation(
        release_recovery,
        [compile_failure, compile_failure, compile_failure],
        release_error="gh: API rate limit exceeded (HTTP 403)\n",
    )
    for needle in (
        "GitHub Release state for `v9.9.9` could not be read",
        "Do not infer that the Release is absent",
    ):
        if needle not in unreadable_release_body:
            raise AssertionError(
                "recovery converted an unreadable Release API state into absence: "
                f"{needle!r}: {unreadable_release_body}"
            )
    if "No GitHub Release object exists" in unreadable_release_body:
        raise AssertionError(
            "recovery claimed Release absence after a non-404 API failure: "
            f"{unreadable_release_body}"
        )


def assert_recovery_stops_on_repeated_signature(release_recovery: str) -> None:
    """A deterministic failure must cost one confirmation, not the whole budget.

    The controller exists for transient failures. A failure that reproduces
    with an identical failing-step signature is not transient in any way a
    rerun can fix, and each blind retry costs a full Release run. The classifier
    therefore runs BEFORE the rerun is requested, and its verdict has to be
    driven by the real jobs API shapes rather than by a stub's canned answer.
    """

    classify_at = release_recovery.index(
        "      - name: Classify the failure signature across attempts\n"
    )
    rerun_at = release_recovery.index("      - name: Re-run failed jobs\n")
    if classify_at > rerun_at:
        raise AssertionError(
            "release recovery must classify the failure signature before it "
            "requests any rerun, because a rerun issued ahead of the verdict "
            "is exactly the blind retry this controller exists to prevent"
        )

    source = recovery_step_script(
        release_recovery,
        "      - name: Classify the failure signature across attempts\n",
    )
    compile_failure = ("build-artifacts (x86_64-unknown-linux-musl)", "Build release binaries")
    notarize = ("build-artifacts (aarch64-apple-darwin)", "Notarize")

    outputs = execute_recovery_classifier(source, [compile_failure, compile_failure])
    if outputs.get("repeated") != "true":
        raise AssertionError(
            "a second identical failing-step signature did not stop the retry "
            f"budget: {outputs}"
        )

    outputs = execute_recovery_classifier(source, [compile_failure, notarize])
    if outputs.get("repeated") != "false":
        raise AssertionError(
            f"a differing second failure was reported as deterministic: {outputs}"
        )

    # The stop rule is last-two equality, not all-attempts equality, and the two
    # are only distinguishable on this shape. A failure that reproduces after a
    # differing first attempt still has to stop, because the budget it would
    # spend buys another identical failure. The all-attempts count is reported
    # beside it rather than deciding it, so the alert can say which of the two
    # it observed instead of asserting the stronger one either way.
    outputs = execute_recovery_classifier(
        source, [notarize, compile_failure, compile_failure]
    )
    if outputs.get("repeated") != "true":
        raise AssertionError(
            "a failure that reproduced after a differing first attempt was not "
            f"read as a repeat: {outputs}"
        )
    if outputs.get("distinct") != "2":
        raise AssertionError(
            "the all-attempts signature count did not observe the differing "
            f"first attempt: {outputs}"
        )

    # One attempt cannot repeat anything. Stopping here would retire a release
    # on its first failure and defeat the controller's entire purpose.
    outputs = execute_recovery_classifier(source, [compile_failure])
    if outputs.get("repeated") != "false":
        raise AssertionError(
            f"a single first failure was treated as a repeat: {outputs}"
        )

    # A transient API error is not evidence that the release failure repeats.
    outputs = execute_recovery_classifier(source, [compile_failure, None])
    if outputs.get("repeated") != "false" or outputs.get("unknown") == "0":
        raise AssertionError(
            f"an unreadable attempt was folded into a repeat verdict: {outputs}"
        )
    outputs = execute_recovery_classifier(source, [None, None])
    if outputs.get("repeated") != "false":
        raise AssertionError(
            "two unreadable attempts compared equal to each other and stopped "
            f"the retries: {outputs}"
        )

    # A shape GitHub can produce, hardened against rather than observed here:
    # when one matrix leg fails its siblings can be cancelled, and a cancelled
    # job still carries the step that actually failed. Filtering on the job's
    # own conclusion first drops exactly that evidence and reads a repeat as two
    # unreadable attempts, which retries a deterministic failure to the end of
    # the budget. The runs this classifier was built from carried no cancelled
    # job at all, so a step-primary read is defence and not the repair: what
    # cost three attempts there was classifying only once the budget was gone.
    cancelled = ("build-artifacts (aarch64-unknown-linux-musl)", "Build release binaries", "cancelled")
    outputs = execute_recovery_classifier(source, [cancelled, cancelled])
    if outputs.get("repeated") != "true":
        raise AssertionError(
            "a failing step inside a cancelled matrix leg was not read as "
            f"failure evidence: {outputs}"
        )

    # A job that failed while recording no failing step at all — a runner
    # startup failure — is still a signature, not an unreadable attempt.
    startup = ("publish", None, "failure")
    outputs = execute_recovery_classifier(source, [startup, startup])
    if outputs.get("repeated") != "true" or outputs.get("unknown") != "0":
        raise AssertionError(
            f"a job-level failure with no failing step was unreadable: {outputs}"
        )

    # The order matrix legs queue in must not decide the verdict.
    aarch64 = ("Build (kin-linux-aarch64)", "Build kin-cli + kin-daemon (native)")
    x86_64 = ("Build (kin-linux-x86_64)", "Build kin-cli + kin-daemon (native)")
    outputs = execute_recovery_classifier(
        source, [[aarch64, x86_64], [x86_64, aarch64]]
    )
    if outputs.get("repeated") != "true":
        raise AssertionError(
            "a matrix failure that repeated identically was classified by the "
            f"order its legs happened to queue in: {outputs}"
        )


def assert_recovery_abandonment_stand_down(release_recovery: str) -> None:
    """Pin both retry and alert to the active reviewed-abandonment condition."""

    condition = "steps.record.outputs.abandoned != 'true'"
    for marker, duty in (
        ("name: Re-run failed jobs", "bounded retry"),
        (
            "name: Classify the failure signature across attempts",
            "failure-signature classification",
        ),
        (
            "name: Alert after automatic retries are exhausted",
            "exhausted-retry alert",
        ),
    ):
        active = "\n".join(job_step_active_lines(release_recovery, "reconcile", marker))
        if condition not in active:
            raise AssertionError(
                f"release recovery's {duty} must actively stand down for a tag "
                "the tracked abandonment record already retired"
            )



def check_release_recovery_policy(release_recovery: str) -> None:
    for policy in (
        "name: Release Recovery",
        'workflows: ["Release"]',
        'cron: "3,18,33,48 * * * *"',
        "actions: write",
        "contents: read",
        "issues: write",
        "github.event.workflow_run.conclusion == 'failure'",
        "github.event.workflow_run.conclusion == 'timed_out'",
        "github.event.workflow_run.conclusion == 'startup_failure'",
        "actions/workflows/release.yml/runs?per_page=100",
        '.status == "requested"',
        '.path == ".github/workflows/release.yml"',
        '.head_repository.full_name == $repo',
        "rerun-failed-jobs",
        '[ "$attempt" -ge 3 ]',
        "name: Classify the failure signature across attempts",
        "steps.classify.outputs.repeated != 'true'",
        "steps.classify.outputs.repeated == 'true'",
        "Release blocked after automatic retries",
        "scripts/abandoned-release-tags.json",
        "scripts/select-admissible-release-tag.py",
        '[ "$head" != "$GITHUB_SHA" ]',
        "reconciled by abandonment",
    ):
        require(release_recovery, policy, "bounded automatic release recovery")



def check_recovery_alarms_and_abandonment(release_recovery: str) -> None:
    assert_recovery_closes_only_shipped_alarms(release_recovery)
    assert_recovery_abandonment_stand_down(release_recovery)
    expect_assertion(
        "recovery repeats its stand-down condition only in comments",
        "must actively stand down",
        lambda: assert_recovery_abandonment_stand_down(
            release_recovery.replace(
                "          steps.record.outputs.abandoned != 'true'",
                "          # steps.record.outputs.abandoned != 'true'",
            )
        ),
    )



def check_recovery_forbidden_authority(release_recovery: str) -> None:
    for forbidden in (
        "workflow_dispatch:",
        "contents: write",
        "packages: write",
        "id-token: write",
        "conclusion == 'cancelled'",
    ):
        if forbidden in release_recovery:
            raise AssertionError(
                f"release recovery contains forbidden authority or retry state: {forbidden}"
            )



def check_recovery_escalation() -> None:
    assert_recovery_escalation_classifies(
        RELEASE_RECOVERY.read_text(encoding="utf-8")
    )
    assert_recovery_stops_on_repeated_signature(
        RELEASE_RECOVERY.read_text(encoding="utf-8")
    )
