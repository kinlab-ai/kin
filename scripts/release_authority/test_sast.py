# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Checks that pin sast.yml, secret-scan.yml and advisory-sweep.yml."""

from __future__ import annotations

import os
import re
import subprocess
import tempfile
from pathlib import Path

from .test_helpers import (
    WORKFLOWS,
    classifier_active_job_source,
    expect_assertion,
    replace_exactly_once,
    workflow_job_blocks,
    workflow_step_shell,
)


# The full cargo-deny check set, spelled once. A merge group that ran a subset
# of this would stop reading the advisory database on the one event that reads
# the merged tree, which is the fix that was tried and refused: see the step's
# own comment in sast.yml.
CARGO_DENY_FULL_CHECK = (
    "cargo deny --log-level warn --manifest-path ./Cargo.toml --all-features check"
)


def assert_cargo_deny_reads_every_advisory(sast: str) -> None:
    """Keep advisories in every cargo-deny run, and make a merge group say why.

    Skipping `advisories` inside `merge_group` stops a freshly published
    advisory from ejecting an innocent pull request, and it also stops the one
    gate that reads the merged tree from reading the advisory database at all.
    The sweep closes that window by landing the bump on a schedule instead, so
    this check stays whole and only its failure message changes.
    """

    job = workflow_job_blocks(sast).get("cargo-deny")
    if job is None:
        raise AssertionError("SAST must declare the cargo-deny job")
    shell = workflow_step_shell(job, "Run cargo-deny")
    # Continuations joined first, so a wrapped invocation is judged whole, and
    # the shell grammar around it stripped, so `if cargo deny ...; then` reads
    # as the invocation it is.
    joined = re.sub(r"\\\n\s*", " ", shell)
    gating = []
    for line in joined.splitlines():
        stripped = line.strip()
        if stripped.startswith("if "):
            stripped = stripped[3:]
        if stripped.endswith("; then"):
            stripped = stripped[: -len("; then")]
        if stripped.startswith("cargo deny"):
            gating.append(re.sub(r"\s+", " ", stripped).strip())
    if not gating or gating[0] != CARGO_DENY_FULL_CHECK:
        raise AssertionError(
            "the cargo-deny gate must run the whole default check set, advisories "
            f"included, on every event (first invocation: {gating[0] if gating else 'none'})"
        )
    narrowed = f"{CARGO_DENY_FULL_CHECK} bans licenses sources"
    if narrowed in re.sub(r"\s+", " ", joined):
        raise AssertionError(
            "the cargo-deny gate must not narrow its check set: dropping advisories "
            "inside a merge group stops the one run that reads the merged tree from "
            "reading the advisory database"
        )
    if "--annotate-merge-group" not in shell:
        raise AssertionError(
            "a merge-group cargo-deny failure must name the advisory and its bump "
            "command, or the queue ejects an entry with no cause recorded anywhere"
        )
    if not shell.rstrip().endswith("exit 1"):
        raise AssertionError(
            "a merge-group cargo-deny failure must still fail: the advisory is real "
            "and only its message changes"
        )


def assert_advisory_sweep_authority(sweep: str, release_train: str) -> None:
    """Pin what lets the sweep write to the repository unattended."""

    header = sweep.split("\njobs:", maxsplit=1)[0]
    if "\n  schedule:\n" not in header:
        raise AssertionError("the advisory sweep must run on a schedule, not on demand only")
    if "types: [advisory-sweep]" not in header:
        raise AssertionError(
            "the advisory sweep's manual path must be a repository dispatch pinned to "
            "one action, which always resolves this workflow file from the default branch"
        )
    if "ALLOWED_ACTORS: |" not in sweep:
        raise AssertionError(
            "the advisory sweep's manual path must be gated to the reviewed actor set"
        )
    for pin, label in (
        (
            "actions/create-github-app-token@bcd2ba49218906704ab6c1aa796996da409d3eb1",
            "release App token minter",
        ),
        (
            "actions/checkout@9c091bb21b7c1c1d1991bb908d89e4e9dddfe3e0",
            "checkout",
        ),
    ):
        if pin not in sweep:
            raise AssertionError(
                f"the advisory sweep must pin its {label} to the reviewed object"
            )
        if pin not in release_train:
            raise AssertionError(
                f"the advisory sweep's {label} pin no longer matches release-train.yml"
            )
    if "environment: release-tag" not in sweep:
        raise AssertionError(
            "the advisory sweep must declare the environment its App credentials are "
            "scoped to, or it silently falls back to repository-scoped copies"
        )
    for needle, label in (
        # Two distinct probes, pinned separately rather than as one substring.
        # The fallback made the step run `cargo metadata --locked` twice, and a
        # single substring needle is satisfied by either one surviving, so it
        # would pass a tree where the other had lost `--locked`.
        (
            "if ! cargo metadata --locked --format-version 1",
            "decide whether the minimal entry rewrite resolved before it publishes anything",
        ),
        (
            "cargo metadata --locked --format-version 1 > /dev/null",
            "prove the hand-written lock is one cargo would produce",
        ),
        (
            "cargo deny --log-level warn --manifest-path ./Cargo.toml --all-features check advisories",
            "prove the advisory the bump exists for is actually gone",
        ),
        (
            "git diff --name-only | grep -vx 'Cargo.lock'",
            "refuse a bump that touched anything but the lock",
        ),
        (
            'cmp -s "$RUNNER_TEMP/Cargo.lock.base" ./Cargo.lock',
            "refuse a bump that moved the lock nowhere, measured against the base it "
            "started from rather than against HEAD",
        ),
        (
            "--verify-move",
            "prove every planned bump landed at its planned version and checksum with "
            "no package entering or leaving the tree, which replaced counting lock "
            "lines: the old count only held for a fix needing nothing but the crate "
            "the advisory named, and it refused RUSTSEC-2026-0285's real fix",
        ),
        (
            'cargo update -p "${crate}@${from}" --precise "$to"',
            "fall back to cargo's own resolution when the minimal entry rewrite does "
            "not resolve, instead of failing the sweep and leaving the advisory for a "
            "human",
        ),
    ):
        if needle not in sweep:
            raise AssertionError(
                f"the advisory sweep must {label} before it opens a pull request"
            )
    for job_step, gate in (
        ("Open or update the bump pull request", "steps.plan.outputs.bumps != '0'"),
        ("Arm protected auto-merge", "steps.plan.outputs.bumps != '0'"),
        ("Open or update the unfixable-advisory issue", "steps.plan.outputs.unfixable != '0'"),
    ):
        anchor = f"      - name: {job_step}\n"
        if anchor not in sweep:
            raise AssertionError(f"the advisory sweep must declare the {job_step} step")
        block = sweep[sweep.index(anchor) : sweep.index(anchor) + 400]
        if f"if: {gate}" not in block:
            raise AssertionError(
                f"the advisory sweep's {job_step} step must be gated on {gate}, so a "
                "clean sweep opens nothing"
            )



SECRET_SCAN = WORKFLOWS / "secret-scan.yml"
SECRET_SCAN_STEP = "Scan the history this event lands"
SECRET_SCAN_TRIGGERS = "on:\n  push:\n    branches: [main]\n  pull_request:\n  merge_group:\n"


def git_commit(repository: Path, name: str) -> str:
    (repository / name).write_text(name + "\n", encoding="utf-8")
    environment = {"PATH": os.defpath, "HOME": str(repository), "GIT_CONFIG_NOSYSTEM": "1",
                   "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.invalid",
                   "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.invalid"}
    for args in (["add", name], ["commit", "-qm", f"{name}\n\nSigned-off-by: t <t@example.invalid>"]):
        subprocess.run(["git", *args], cwd=repository, env=environment, check=True,
                       capture_output=True, timeout=30)
    return subprocess.run(["git", "rev-parse", "HEAD"], cwd=repository, env=environment,
                          check=True, capture_output=True, text=True, timeout=30).stdout.strip()


def assert_secret_scan_scope(secret_scan: str) -> None:
    """One producer per event, each scanning exactly the history that event lands.

    `gitleaks git` with no `--log-opts` runs `git log --all`, and the checkout
    fetches every branch, so a scan of main or of the merge queue's commit read
    every unrelated branch in the repository and failed on one of them. A
    pull request scans its own range, base..head, and every other event scans
    what is reachable from the commit it builds. Pushes to other branches do
    not run the job, because the queue's own branch push would otherwise
    publish `gitleaks (full history)` a second time beside the merge_group run.
    The step is executed against a real repository with a recording gitleaks.
    """

    if SECRET_SCAN_TRIGGERS not in secret_scan:
        raise AssertionError(
            "secret scan scope requires exactly push to main, pull_request and merge_group"
        )
    job = workflow_job_blocks(secret_scan)["gitleaks"]
    active = classifier_active_job_source(job)
    if "\n  if:" in active:
        raise AssertionError("secret scan scope requires the gitleaks job to run on every event it triggers on")
    if "  name: gitleaks (full history)" not in active:
        raise AssertionError("secret scan scope requires the gitleaks (full history) context name")
    shell = workflow_step_shell(job, SECRET_SCAN_STEP)
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        repository = root / "repository"
        repository.mkdir()
        subprocess.run(["git", "init", "-q", "-b", "main", str(repository)], check=True,
                       capture_output=True, timeout=30)
        base = git_commit(repository, "base")
        head = git_commit(repository, "change")
        bin_dir = root / "bin"
        bin_dir.mkdir()
        recorder = bin_dir / "gitleaks"
        recorder.write_text('#!/bin/sh\nprintf "%s\\n" "$@" > "$RECORD"\n', encoding="utf-8")
        recorder.chmod(0o755)
        for label, event, extra, expected in (
            ("a pull request scans its own range", "pull_request",
             {"BASE_SHA": base, "HEAD_SHA": head, "GITHUB_SHA": head}, f"--log-opts=--full-history {base}..{head}"),
            ("the merge queue scans what its commit reaches", "merge_group",
             {"GITHUB_SHA": head}, f"--log-opts=--full-history {head}"),
            ("main scans what its commit reaches", "push",
             {"GITHUB_SHA": head}, f"--log-opts=--full-history {head}"),
            ("an unknown base refuses rather than scanning nothing", "pull_request",
             {"BASE_SHA": "0" * 40, "HEAD_SHA": head, "GITHUB_SHA": head}, None),
        ):
            record = root / f"{event}.args"
            if record.exists():
                record.unlink()
            result = subprocess.run(
                ["bash", "--noprofile", "--norc", "-c", shell],
                cwd=repository,
                env={"PATH": f"{bin_dir}{os.pathsep}{os.defpath}", "EVENT_NAME": event,
                     "RUNNER_TEMP": str(root), "RECORD": str(record), "HOME": str(root),
                     "BASE_SHA": "", "HEAD_SHA": "", **extra},
                capture_output=True, text=True, timeout=60, check=False,
            )
            if expected is None:
                if result.returncode == 0:
                    raise AssertionError(f"secret scan scope: {label}: the step exited 0")
                continue
            arguments = record.read_text(encoding="utf-8").splitlines() if record.exists() else []
            if result.returncode != 0 or expected not in arguments:
                raise AssertionError(
                    f"secret scan scope: {label}: expected {expected!r} among gitleaks arguments "
                    f"{arguments}: {result.stdout}{result.stderr}"
                )


def check_secret_scan_scope() -> None:
    secret_scan = SECRET_SCAN.read_text(encoding="utf-8")
    assert_secret_scan_scope(secret_scan)
    for label, original, replacement in (
        ("the scan reads every branch again", ' \\\n            --log-opts="--full-history $commits"', ""),
        ("a pull request scans all of history", 'commits="${BASE_SHA}..${HEAD_SHA}"', 'commits="${HEAD_SHA}"'),
        ("every branch push publishes the context again", "    branches: [main]\n", '    branches: ["**"]\n'),
        ("the merge queue stops producing the context", "  merge_group:\n", ""),
        ("a pull request is skipped again", "    timeout-minutes: 15\n",
         "    timeout-minutes: 15\n    if: github.event_name == 'push'\n"),
    ):
        mutant = replace_exactly_once(secret_scan, original, replacement, label)
        expect_assertion(label, "secret scan scope", lambda mutant=mutant: assert_secret_scan_scope(mutant))


def check_cargo_deny_and_advisory_sweep(
    advisory_sweep: str, release_train: str, sast: str
) -> None:
    check_secret_scan_scope()
    assert_cargo_deny_reads_every_advisory(sast)
    assert_advisory_sweep_authority(advisory_sweep, release_train)
    for label, source, original, replacement, expected, check in (
        (
            "the gate stops running the whole default check set",
            sast,
            f"{CARGO_DENY_FULL_CHECK}; then",
            f"{CARGO_DENY_FULL_CHECK} advisories; then",
            "must run the whole default check set",
            "sast",
        ),
        (
            "a merge group narrows the check set back to the refused subset",
            sast,
            "            --annotate-merge-group\n",
            "            --annotate-merge-group\n"
            f"          {CARGO_DENY_FULL_CHECK} bans licenses sources\n",
            "must not narrow its check set",
            "sast",
        ),
        (
            "a merge-group failure stops naming the advisory that caused it",
            sast,
            "--annotate-merge-group",
            "--dry-run",
            "must name the advisory and its bump command",
            "sast",
        ),
        (
            "a merge-group advisory failure is downgraded to a pass",
            sast,
            "            --annotate-merge-group\n          exit 1\n",
            "            --annotate-merge-group\n          exit 0\n",
            "must still fail",
            "sast",
        ),
        (
            "the sweep loses its schedule and runs only when asked",
            advisory_sweep,
            "on:\n  schedule:\n",
            "on:\n  # schedule removed\n",
            "must run on a schedule",
            "sweep",
        ),
        (
            "the sweep's manual path stops being pinned to one dispatch action",
            advisory_sweep,
            "types: [advisory-sweep]",
            "types: [anything]",
            "pinned to one action",
            "sweep",
        ),
        (
            "the sweep stops gating its manual path on the reviewed actors",
            advisory_sweep,
            "ALLOWED_ACTORS: |",
            "UNUSED_ACTORS: |",
            "gated to the reviewed actor set",
            "sweep",
        ),
        (
            # Anchored on the post-fallback probe rather than on the bare
            # `cargo metadata --locked`, which the fallback made ambiguous: the
            # step now runs it twice, once to decide whether the minimal rewrite
            # resolved and once on cargo's own output. Either losing `--locked`
            # is the defect, and this mutates the second, which is the one
            # standing between a published branch and an unbuildable lock.
            "the sweep stops proving the resolved lock is coherent",
            advisory_sweep,
            "cargo metadata --locked --format-version 1 > /dev/null",
            "cargo metadata --format-version 1 > /dev/null",
            "prove the hand-written lock",
            "sweep",
        ),
        (
            "the sweep stops noticing that the minimal rewrite did not resolve",
            advisory_sweep,
            "if ! cargo metadata --locked --format-version 1",
            "if ! cargo metadata --format-version 1",
            "decide whether the minimal entry rewrite resolved",
            "sweep",
        ),
        (
            "the sweep stops proving the advisory it bumped for is gone",
            advisory_sweep,
            "--all-features check advisories\n\n      - name: Publish",
            "--all-features check\n\n      - name: Publish",
            "prove the advisory the bump exists for is actually gone",
            "sweep",
        ),
        (
            "the sweep stops refusing a bump that escaped the lockfile",
            advisory_sweep,
            "git diff --name-only | grep -vx 'Cargo.lock'",
            "git diff --name-only | grep -vx 'nothing-matches-this'",
            "refuse a bump that touched anything but the lock",
            "sweep",
        ),
        (
            "the sweep stops checking the bump against the base it started from",
            advisory_sweep,
            'cmp -s "$RUNNER_TEMP/Cargo.lock.base" ./Cargo.lock',
            'cmp -s ./Cargo.lock ./Cargo.lock',
            "measured against the base it started from",
            "sweep",
        ),
        (
            "the sweep goes back to counting lock lines instead of proving the move",
            advisory_sweep,
            "            --verify-move \\\n",
            "            --render pr-body \\\n",
            "no package entering or leaving the tree",
            "sweep",
        ),
        (
            "the sweep loses the fallback and fails on any transitive fix again",
            advisory_sweep,
            'cargo update -p "${crate}@${from}" --precise "$to"',
            'echo "would have bumped ${crate} to $to"',
            "fall back to cargo's own resolution",
            "sweep",
        ),
        (
            "a clean sweep opens a pull request anyway",
            advisory_sweep,
            "      - name: Arm protected auto-merge\n        if: steps.plan.outputs.bumps != '0'\n",
            "      - name: Arm protected auto-merge\n        if: always()\n",
            "must be gated on steps.plan.outputs.bumps != '0'",
            "sweep",
        ),
        (
            "an unfixable advisory opens a pull request instead of an issue",
            advisory_sweep,
            "      - name: Open or update the unfixable-advisory issue\n        if: steps.plan.outputs.unfixable != '0'\n",
            "      - name: Open or update the unfixable-advisory issue\n        if: always()\n",
            "must be gated on steps.plan.outputs.unfixable != '0'",
            "sweep",
        ),
    ):
        mutant = replace_exactly_once(source, original, replacement, label)
        if check == "sast":
            expect_assertion(
                label,
                expected,
                lambda mutant=mutant: assert_cargo_deny_reads_every_advisory(mutant),
            )
        else:
            expect_assertion(
                label,
                expected,
                lambda mutant=mutant: assert_advisory_sweep_authority(mutant, release_train),
            )
