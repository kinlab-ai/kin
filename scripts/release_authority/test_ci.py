# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Checks that pin .github/workflows/ci.yml."""

from __future__ import annotations

import os
import re
import subprocess
import tempfile
import textwrap
from pathlib import Path

from .test_helpers import (
    CI_JOB_DISPLAY_NAMES,
    RELEASE_SUITE_EVENT_FILTER,
    RELEASE_VERSION_FALSIFIER,
    ROOT,
    WORKFLOWS,
    active_lines,
    assert_rust_cache_steps,
    assert_workflow_job_census,
    classifier_active_job_source,
    expect_assertion,
    optional_job_display_name,
    replace_exactly_once,
    require,
    workflow_job_blocks,
    workflow_step_shell,
)


RUST_TOOLCHAIN_ACTION = ROOT / ".github" / "actions" / "rust-toolchain" / "action.yml"
TOOLCHAIN_PRUNE = (
    ROOT / ".github" / "actions" / "rust-toolchain" / "prune-image-toolchains.sh"
)

WINDOWS_NPM_PROOF = ROOT / "scripts" / "prove-windows-npm-first-run.mjs"
CANONICAL_NPM_PROVISION = ROOT / "packages" / "kin" / "lib" / "provision.mjs"
CANONICAL_NPM_PROVISION_TEST = ROOT / "packages" / "kin" / "test" / "provision.test.mjs"
COMPAT_NPM_PROVISION = ROOT / "packages" / "kin-mcp" / "src" / "index.js"
COMPAT_NPM_PROVISION_TEST = ROOT / "packages" / "kin-mcp" / "test" / "index.test.js"

ASSERTION_REACHABILITY = ROOT / "scripts" / "test-assertion-reachability.py"

ASSERTION_REACHABILITY_POLICY = "scripts/test-assertion-reachability.py"

RELEASE_VERSION_GUARD = ROOT / "scripts" / "check-release-version.mjs"
RELEASE_VERSION_GUARD_POLICY = "scripts/check-release-version.mjs"
RELEASE_VERSION_SUITE_POLICY = "scripts/check-release-version.test.mjs"
RELEASE_INTENT_SUITE_POLICY = "scripts/release-intent.test.mjs"

RELEASE_VERSION_FALSIFIER_POLICY = "scripts/falsify-release-version-guards.py"

CI_APT_INSTALL = ROOT / "scripts" / "ci-apt-install.sh"

DOCS_ONLY_WORKFLOW_HEADER = textwrap.dedent(
    """\
    name: CI
    on:
      push:
        branches: [main]
      pull_request:
        branches: [main]
        types: [opened, synchronize, reopened]
      merge_group:
      repository_dispatch:
        types: [dependency-updated]
    permissions:
      contents: read
    concurrency:
      group: ${{ github.workflow }}-${{ github.ref }}-${{ github.ref == 'refs/heads/main' && github.sha || '' }}
      cancel-in-progress: ${{ github.ref != 'refs/heads/main' }}
    env:
      CARGO_TERM_COLOR: always
      RUSTFLAGS: "-D warnings"
    """
).rstrip()
DOCS_ONLY_CLASSIFIER_SHELL = textwrap.dedent(
    """\
    set -euo pipefail
    docs_only=false
    if [ "$EVENT_NAME" = "pull_request" ]; then
      changed="$(
        /usr/bin/env -i \\
          HOME=/dev/null \\
          PATH=/usr/bin:/bin \\
          LC_ALL=C \\
          GIT_CONFIG_NOSYSTEM=1 \\
          GIT_CONFIG_SYSTEM=/dev/null \\
          GIT_CONFIG_GLOBAL=/dev/null \\
          GIT_CONFIG_COUNT=0 \\
          GIT_ATTR_NOSYSTEM=1 \\
          GIT_PAGER=cat \\
          GIT_TERMINAL_PROMPT=0 \\
          /usr/bin/git \\
            --no-pager \\
            --no-replace-objects \\
            --no-lazy-fetch \\
            --no-optional-locks \\
            --literal-pathspecs \\
            --git-dir="$WORKSPACE/.git" \\
            --work-tree="$WORKSPACE" \\
            -c core.attributesFile=/dev/null \\
            -c core.fsmonitor=false \\
            -c diff.external= \\
            -c diff.renames=false \\
            -c diff.ignoreSubmodules=none \\
            diff \\
            --no-ext-diff \\
            --no-textconv \\
            --no-renames \\
            --ignore-submodules=none \\
            --submodule=short \\
            --name-only \\
            "$BASE_SHA...$HEAD_SHA" \\
            --
      )"
      if [ -n "$changed" ]; then
        docs_only=true
        while IFS= read -r path; do
          case "$path" in
            .github/workflows/ci.yml) docs_only=false; break ;;
            *.md | docs/*) ;;
            .github/workflows/*) ;;
            *) docs_only=false; break ;;
          esac
        done <<< "$changed"
      fi
      printf '%s\\n' "$changed"
    fi
    echo "docs_only=$docs_only" >> "$GITHUB_OUTPUT"
    """
).rstrip()
DOCS_ONLY_CLASSIFIER_JOB = textwrap.dedent(
    """\
    changes:
      name: Classify diff scope
      runs-on: ubuntu-latest
      outputs:
        docs_only: ${{ steps.classify.outputs.docs_only }}
      steps:
        - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
          with:
            fetch-depth: 0
        - name: Classify changed paths
          id: classify
          shell: /usr/bin/bash --noprofile --norc -p -e -u -o pipefail {0}
          env:
            PATH: /usr/bin:/bin
            BASH_ENV: ""
            ENV: ""
            LD_AUDIT: ""
            LD_LIBRARY_PATH: /dev/null
            LD_PRELOAD: ""
            WORKSPACE: ${{ github.workspace }}
            EVENT_NAME: ${{ github.event_name }}
            BASE_SHA: ${{ github.event.pull_request.base.sha }}
            HEAD_SHA: ${{ github.event.pull_request.head.sha }}
          run: |
            set -euo pipefail
            docs_only=false
            if [ "$EVENT_NAME" = "pull_request" ]; then
              changed="$(
                /usr/bin/env -i \\
                  HOME=/dev/null \\
                  PATH=/usr/bin:/bin \\
                  LC_ALL=C \\
                  GIT_CONFIG_NOSYSTEM=1 \\
                  GIT_CONFIG_SYSTEM=/dev/null \\
                  GIT_CONFIG_GLOBAL=/dev/null \\
                  GIT_CONFIG_COUNT=0 \\
                  GIT_ATTR_NOSYSTEM=1 \\
                  GIT_PAGER=cat \\
                  GIT_TERMINAL_PROMPT=0 \\
                  /usr/bin/git \\
                    --no-pager \\
                    --no-replace-objects \\
                    --no-lazy-fetch \\
                    --no-optional-locks \\
                    --literal-pathspecs \\
                    --git-dir="$WORKSPACE/.git" \\
                    --work-tree="$WORKSPACE" \\
                    -c core.attributesFile=/dev/null \\
                    -c core.fsmonitor=false \\
                    -c diff.external= \\
                    -c diff.renames=false \\
                    -c diff.ignoreSubmodules=none \\
                    diff \\
                    --no-ext-diff \\
                    --no-textconv \\
                    --no-renames \\
                    --ignore-submodules=none \\
                    --submodule=short \\
                    --name-only \\
                    "$BASE_SHA...$HEAD_SHA" \\
                    --
              )"
              if [ -n "$changed" ]; then
                docs_only=true
                while IFS= read -r path; do
                  case "$path" in
                    .github/workflows/ci.yml) docs_only=false; break ;;
                    *.md | docs/*) ;;
                    .github/workflows/*) ;;
                    *) docs_only=false; break ;;
                  esac
                done <<< "$changed"
              fi
              printf '%s\\n' "$changed"
            fi
            echo "docs_only=$docs_only" >> "$GITHUB_OUTPUT"
    """
).rstrip()
# The inert pull-request producer of both expanded `Check & Test` names. It
# covered documentation-only diffs alone until the admission core took the ubuntu and
# macOS suites off the pull-request path; it now covers every pull request, and
# it carries no `needs:` so it reports without waiting on the classifier. Its
# condition and the two aggregates' are mutually exclusive on every event, which
# is what keeps one required name from having two check runs behind it.
PULL_REQUEST_CHECK_STUB = textwrap.dedent(
    """\
    check-pr-fast-path:
      name: Check & Test
      if: ${{ !cancelled() && github.event_name == 'pull_request' }}
      runs-on: ubuntu-latest
      strategy:
        matrix:
          os: [ubuntu-latest, macos-latest]
      steps:
        - name: Report the pull-request fast path
          run: |
            echo "Check & Test runs in the merge queue and on every main commit."
            echo "Admission is the fast gate; see .github/workflows/ci.yml."
    """
).rstrip()
# The ubuntu shards publish `Check & Test ubuntu shard (1)` and `(2)`, which no
# ruleset requires, and `check-aggregate` is what publishes the required
# `Check & Test (ubuntu-latest)`. The job id stays `check` rather than becoming
# `check-ubuntu` for two reasons neither of which is visible from this file:
# `bin/kin-precheck` in the umbrella enumerates this job's gate list by that id,
# and `rust-cache` composes its key from the job id when no shared key is given,
# so the feature workers reach this job's warm dependency graph by asking for
# `check`. Renaming it would cost the ubuntu permutations their cache exactly
# the way the macOS leg lost its own.
REAL_CHECK_JOB_AUTHORITY = textwrap.dedent(
    """\
    check:
      name: Check & Test ubuntu shard
      needs: changes
      if: >-
        ${{ !cancelled()
        && needs.changes.outputs.docs_only != 'true'
        && github.event_name != 'pull_request' }}
      runs-on: ${{ vars.KIN_HEAVY_RUNNER || 'ubuntu-latest' }}
      timeout-minutes: 60
      env:
        CARGO_INCREMENTAL: "0"
        CARGO_PROFILE_DEV_DEBUG: "0"
        CARGO_PROFILE_TEST_DEBUG: "0"
      strategy:
        fail-fast: false
        matrix:
          shard: [1, 2, 3]
      steps:
    """
).rstrip()
# The ubuntu counterpart of MACOS_SHARD_AGGREGATE_AUTHORITY, pinned for the same
# two reasons: the one-value matrix keeps a skipped aggregate from publishing the
# expanded required name beside the documentation-only stub's, and admitting only
# `success` from the shard roll-up keeps a skipped or cancelled shard from
# leaving half the ubuntu suite unrun behind a green required context.
UBUNTU_SHARD_AGGREGATE_AUTHORITY = textwrap.dedent(
    """\
    check-aggregate:
      name: Check & Test
      needs: [changes, check-lint, check]
      if: >-
        ${{ !cancelled()
        && needs.changes.outputs.docs_only != 'true'
        && github.event_name != 'pull_request' }}
      runs-on: ubuntu-latest
      timeout-minutes: 5
      strategy:
        matrix:
          os: [ubuntu-latest]
      steps:
    """
).rstrip()
# Indented as `classifier_active_job_source` renders a dedented job block.
# The `check` job runner, pinned here as well as inside
# REAL_CHECK_JOB_AUTHORITY, because `shards` binds to blocks.get("check"),
# so one job is pinned in two places and both have to move together.
# The value is the heavy-runner switch rather than a label: the job reads
# `vars.KIN_HEAVY_RUNNER` and falls back to ubuntu-latest when it is unset,
# so what is pinned here is that the shards keep reading the switch. A
# hardcoded label cannot fall back, and on 2026-08-30 the kin-16core pool
# stopped assigning runners and left every job pinned to it queued forever.
# The aggregate is deliberately NOT on the switch: it compiles nothing and
# runs in seconds, so it stays on ubuntu-latest.
UBUNTU_SHARD_RUNNER = "  runs-on: ${{ vars.KIN_HEAVY_RUNNER || 'ubuntu-latest' }}"
UBUNTU_SHARD_INDEPENDENT_LEGS = "    fail-fast: false"
UBUNTU_SHARD_MATRIX = "      shard: [1, 2, 3]"
UBUNTU_SHARD_PARTITION = (
    "      run: cargo nextest run --locked --no-fail-fast --features kin-cli/embeddings,kin-daemon/embeddings --partition count:${{ matrix.shard }}/3"
)
UBUNTU_SHARD_DOCTESTS = "      run: cargo test --doc --locked --features kin-cli/embeddings,kin-daemon/embeddings"
UBUNTU_SHARD_SUCCESS_GATE = 'if [ "$SHARDS" != "success" ]; then'
# The lint job publishes `Check & Test ubuntu lint`, which no ruleset requires,
# and the aggregate admits only its `success`. Its admission matches the shards'
# so it runs on exactly the events they do, and its cargo environment matches
# `check`'s because it restores that job's cache.
UBUNTU_LINT_AUTHORITY = textwrap.dedent(
    """\
    check-lint:
      name: Check & Test ubuntu lint
      needs: changes
      if: >-
        ${{ !cancelled()
        && needs.changes.outputs.docs_only != 'true'
        && github.event_name != 'pull_request' }}
      runs-on: ${{ vars.KIN_HEAVY_RUNNER || 'ubuntu-latest' }}
      timeout-minutes: 45
      env:
        CARGO_INCREMENTAL: "0"
        CARGO_PROFILE_DEV_DEBUG: "0"
        CARGO_PROFILE_TEST_DEBUG: "0"
      steps:
    """
).rstrip()
LINT_SUCCESS_GATE = 'if [ "$LINT" != "success" ]; then'
# The gates `check-lint` runs once and the test shards never run. Each reads
# source, or checks rather than tests, and cannot answer differently on a second
# runner. They are named individually rather than counted: a bar set from a
# count is crossed by any twenty of them, and the one that goes missing is the
# one nobody named. This is the ladder-coverage lesson in docs/traps.md applied
# to a step list.
UBUNTU_LINT_GATES = (
    "Check the README language count against the adapter registry",
    "Install runtime guard tools",
    "Validate installer checksum fixtures",
    "Validate that every release gate assertion still runs",
    "Validate mandatory Homebrew release outcome gate",
    "Validate automatic release policy",
    "Check the RC build mirrors the release build",
    "Check formatting",
    "Check Zero File-Search Invariant",
    "Check Zero File-Search Invariant (answer modules)",
    "Test the filesystem boundary lint",
    "Falsify the release policy and Windows leg gates",
    "Clippy",
    "Check the filesystem boundary lint",
    "Check Runtime Boundaries",
    "Check Private Repo Coupling",
    "Test Private Repo Coupling Guard",
    "Check the Linux release target (musl)",
    "Check the aarch64 Linux release target (musl)",
    "Doc tests",
)
# What a partition consumes, and therefore what no shard may skip. A partition
# that ran without one of these is not a partition of the suite.
UBUNTU_SHARD_BOTH_LEGS_STEPS = (
    "Install Rust toolchain",
    "Verify kin cargo registry config",
    "Cache cargo registry and build",
    "Build",
    "Install nextest",
    "Install language servers for the enrichment proof",
    "Test",
)
# The macOS shards publish `Check & Test macOS shard (1)` and `(2)`, which no
# ruleset requires, and the aggregate is what publishes the required
# `Check & Test (macos-latest)`. Two properties make that safe and both are
# pinned here, because neither is observable from the required context's name:
# the aggregate carries a one-value matrix, without which a SKIPPED aggregate
# would publish the expanded name beside the documentation-only stub's and put
# two check runs under one required name; and it admits only `success` from the
# shard roll-up, without which a skipped or cancelled shard would leave half the
# macOS suite unrun behind a green required context.
MACOS_SHARD_AGGREGATE_AUTHORITY = textwrap.dedent(
    """\
    check-macos-aggregate:
      name: Check & Test
      needs: [changes, check-macos-lint, check-macos]
      if: >-
        ${{ !cancelled()
        && needs.changes.outputs.docs_only != 'true'
        && github.event_name != 'pull_request' }}
      runs-on: ubuntu-latest
      timeout-minutes: 5
      strategy:
        matrix:
          os: [macos-latest]
      steps:
    """
).rstrip()
# Indented as `classifier_active_job_source` renders a dedented job block, which
# is the form these are matched against.
# The one paid runner in this file, behind the fallback variable that returns
# the shards to the standard pool during a Depot outage. See assert_paid_runner_scope.
MACOS_HEAVY_RUNNER = "${{ vars.KIN_MACOS_HEAVY_RUNNER || 'depot-macos-26' }}"
MACOS_SHARD_RUNNER = "  runs-on: " + MACOS_HEAVY_RUNNER
MACOS_SHARD_INDEPENDENT_LEGS = "    fail-fast: false"
MACOS_SHARD_MATRIX = "      shard: [1, 2]"
MACOS_SHARD_PARTITION = (
    "      run: cargo nextest run --locked --no-fail-fast --features kin-cli/metal,kin-daemon/metal --partition count:${{ matrix.shard }}/2"
)
MACOS_SHARD_DOCTESTS = "      run: cargo test --doc --locked --features kin-cli/metal,kin-daemon/metal"
MACOS_SHARD_SUCCESS_GATE = 'if [ "$SHARDS" != "success" ]; then'
# The macOS lint job, pinned for the reasons the ubuntu one is: it runs on the
# shards' events, on macOS, under the shards' cargo environment because it
# restores their cache.
MACOS_LINT_AUTHORITY = textwrap.dedent(
    """\
    check-macos-lint:
      name: Check & Test macOS lint
      needs: changes
      if: >-
        ${{ !cancelled()
        && needs.changes.outputs.docs_only != 'true'
        && github.event_name != 'pull_request' }}
      runs-on: macos-latest
      timeout-minutes: 50
      env:
        CARGO_INCREMENTAL: "0"
        CARGO_PROFILE_DEV_DEBUG: "0"
        CARGO_PROFILE_TEST_DEBUG: "0"
      steps:
    """
).rstrip()
# What only the macOS lint job runs: the clippy pass over macOS-gated code, the
# boundary lint that needs its name resolution, and the doctests nextest cannot
# run. Named, for the reason the ubuntu list is.
MACOS_LINT_GATES = (
    "Clippy",
    "Check the filesystem boundary lint",
    "Doc tests",
)
MACOS_SHARD_STEPS = (
    "Install Rust toolchain",
    "Verify kin cargo registry config",
    "Cache cargo registry and build",
    "Build",
    "Install nextest",
    "Test",
)
# Each worker resolves its own feature set. The required contexts belong to
# per-platform aggregates, whose one-value OS matrices preserve the existing
# names without creating a second expanded check when the real path is skipped.
FEATURE_PERMUTATION_WORKERS = {
    "feature-tests-ubuntu-shards": ("ubuntu-latest", "feature-tests"),
    "feature-tests-macos-shards": ("macos-latest", "feature-tests-macos-aggregate"),
}
FEATURE_PERMUTATION_WORKER_AUTHORITY = textwrap.dedent(r"""
    @JOB@:
      name: Feature permutation shard
      needs: changes
      if: >-
        ${{ !cancelled()
        && needs.changes.outputs.docs_only != 'true'
        && github.event_name != 'pull_request' }}
      runs-on: ${{ matrix.os }}
      timeout-minutes: 60
      env:
        CARGO_INCREMENTAL: "0"
        CARGO_PROFILE_DEV_DEBUG: "0"
        CARGO_PROFILE_TEST_DEBUG: "0"
      strategy:
        fail-fast: false
        matrix:
          os: [@OS@]
          phase: [gcs, hosted, libraries, featureless-cli, featureless-daemon]
          partition: [1, 2]
          exclude:
            - phase: gcs
              partition: 2
            - phase: hosted
              partition: 2
            - phase: libraries
              partition: 2
      steps:
""").strip()
# The phases a featureless suite is split into. The partition matrix and the
# nextest denominator are one fact in two places, so both are checked against
# this, and a matrix shorter than the denominator, which runs that fraction of
# the suite and reports success, is refused.
FEATURE_PERMUTATION_PARTITIONS = 2
FEATURE_PERMUTATION_AGGREGATE_AUTHORITY = textwrap.dedent(r"""
    @JOB@:
      name: Feature permutation tests
      needs: [changes, @WORKER@]
      if: >-
        ${{ !cancelled()
        && needs.changes.outputs.docs_only != 'true'
        && github.event_name != 'pull_request' }}
      runs-on: ubuntu-latest
      timeout-minutes: 5
      strategy:
        matrix:
          os: [@OS@]
      steps:
""").strip()
FEATURE_PERMUTATION_PR_STUB = textwrap.dedent(r"""
    feature-tests-pr-fast-path:
      name: Feature permutation tests
      if: ${{ !cancelled() && github.event_name == 'pull_request' }}
      runs-on: ubuntu-latest
      strategy:
        matrix:
          os: [ubuntu-latest, macos-latest]
      steps:
        - name: Report the pull-request fast path
          run: |
            echo "This job runs no tests. The feature permutations run on push to"
            echo "main, after the merge, and not on this pull request."
            echo "Admission before the merge is the fast gate."
""").strip()
# Pin executable step boundaries, ordering and conditions, rather than searching
# for commands which could survive in comments or on the wrong matrix group.
FEATURE_PERMUTATION_STEPS = (
    textwrap.dedent(r"""
        - uses: actions/checkout@v7
    """).strip(),
    textwrap.dedent(r"""
        - name: Install Rust toolchain
          uses: ./.github/actions/rust-toolchain
    """).strip(),
    textwrap.dedent(r"""
        - name: Verify kin cargo registry config
          run: |
            test -f .cargo/config.toml
            grep -q '^\[registries\.kin\]' .cargo/config.toml
            if grep -q '^\[patch\.kin\]' .cargo/config.toml; then echo "::error::[patch.kin] git pins must be removed. kin-* resolve from the Kin registry"; exit 1; fi
    """).strip(),
    textwrap.dedent(r"""
        - name: Cache cargo registry and build
          uses: Swatinem/rust-cache@c19371144df3bb44fab255c43d04cbc2ab54d1c4 # v2.9.1
          with:
            add-rust-environment-hash-key: false
            shared-key: ${{ matrix.os == 'macos-latest' && 'check-macos' || 'check' }}
            save-if: false
    """).strip(),
    textwrap.dedent(r"""
        - name: Install nextest
          uses: taiki-e/install-action@v2
          with:
            tool: nextest
    """).strip(),
    textwrap.dedent(r"""
        - name: Test gcs feature
          if: matrix.phase == 'gcs'
          run: cargo nextest run -p kin-daemon --features gcs --locked
    """).strip(),
    textwrap.dedent(r"""
        - name: Test hosted gcs spine cursor hold
          if: matrix.phase == 'gcs' && matrix.os == 'ubuntu-latest'
          run: >-
            cargo test -p kin-daemon --features gcs --locked
            state::tests::hosted_spine_refuses_persistence_without_cursor_cas
            -- --exact --nocapture --test-threads=1
    """).strip(),
    textwrap.dedent(r"""
        - name: Test kin-spine durable publication paths
          if: matrix.phase == 'hosted'
          run: cargo nextest run -p kin-spine --features firestore --locked
    """).strip(),
    textwrap.dedent(r"""
        - name: Test hosted production features
          if: matrix.phase == 'hosted'
          run: cargo nextest run -p kin-daemon --features gcs,firestore --locked
    """).strip(),
    textwrap.dedent(r"""
        - name: Test hosted production durable spine selection
          if: matrix.phase == 'hosted' && matrix.os == 'ubuntu-latest'
          run: >-
            cargo test -p kin-daemon --features gcs,firestore --locked
            state::tests::hosted_production_feature_pair_selects_durable_backend
            -- --exact --nocapture --test-threads=1
    """).strip(),
    textwrap.dedent(r"""
        - name: Test each Kin library on its own minimal features
          if: matrix.phase == 'libraries'
          run: |
            set -euo pipefail
            python3 - <<'LIBS' > "$RUNNER_TEMP/libraries.txt"
            import tomllib
            root = tomllib.load(open("Cargo.toml", "rb"))
            deps = root["workspace"]["dependencies"]
            for name, spec in sorted(deps.items()):
                if isinstance(spec, dict) and "path" in spec and "registry" in spec:
                    print(name)
            LIBS
            count="$(wc -l < "$RUNNER_TEMP/libraries.txt" | tr -d ' ')"
            if [ "$count" -lt 7 ]; then
              echo "::error title=No Kin libraries resolved::read $count from the root manifest"
              exit 1
            fi
            while IFS= read -r crate; do
              echo "::group::cargo test -p $crate --no-default-features"
              cargo test -p "$crate" --no-default-features --lib --tests --locked
              echo "::endgroup::"
            done < "$RUNNER_TEMP/libraries.txt"
    """).strip(),
    textwrap.dedent(r"""
        - name: Test featureless kin-cli
          if: matrix.phase == 'featureless-cli'
          run: cargo nextest run -p kin-cli --no-default-features --lib --tests --locked --partition count:${{ matrix.partition }}/2
    """).strip(),
    textwrap.dedent(r"""
        - name: Test featureless kin-daemon
          if: matrix.phase == 'featureless-daemon'
          run: cargo nextest run -p kin-daemon --no-default-features --lib --tests --locked --partition count:${{ matrix.partition }}/2
    """).strip(),
)
FEATURE_PERMUTATION_AGGREGATE_STEP = textwrap.dedent(r"""
    - name: Require every feature group to have succeeded
      env:
        SHARDS: ${{ needs.@WORKER@.result }}
      run: |
        set -euo pipefail
        if [ "$SHARDS" != "success" ]; then
          echo "::error title=Feature groups did not pass::the feature permutation groups reported '$SHARDS'"
          exit 1
        fi
        echo "every feature permutation group succeeded"
""").strip()


def classifier_shell_source(classifier: str) -> str:
    """Extract the classifier's complete active shell block."""

    lines = classifier.splitlines()
    run_lines = [index for index, line in enumerate(lines) if line.strip() == "run: |"]
    if len(run_lines) != 1:
        raise AssertionError(
            "diff classifier must contain exactly one closed-form shell run block"
        )
    run_line = run_lines[0]
    run_indent = len(lines[run_line]) - len(lines[run_line].lstrip())
    shell_lines: list[str] = []
    for index in range(run_line + 1, len(lines)):
        line = lines[index]
        if not line.strip():
            shell_lines.append(line)
            continue
        indent = len(line) - len(line.lstrip())
        if indent <= run_indent:
            break
        shell_lines.append(line)

    source = textwrap.dedent("\n".join(shell_lines)).strip()
    active_lines = [
        line.rstrip()
        for line in source.splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    return "\n".join(active_lines)


def workflow_active_header_source(workflow: str) -> str:
    """Return the active workflow-level contract above the jobs mapping."""

    marker = "\njobs:\n"
    if workflow.count(marker) != 1:
        raise AssertionError(
            "docs_only authority requires exactly one canonical jobs mapping"
        )
    return classifier_active_job_source(workflow.split(marker, 1)[0])


def assert_toolchain_prune_is_wired(action: str) -> None:
    """Keep the cache key a function of this repository, not of the runner image.

    `Swatinem/rust-cache` hashes every entry of `rustup toolchain list` into half its key.
    Two ubuntu-latest jobs sixty-five seconds apart drew images carrying 1.97.1 and 1.98.0,
    hashed to 8374e8ea and 6ea01539 against one `Cargo.lock`, and could not restore each
    other's cache. Nothing in this repository chose either version.

    The obvious fix is the wrong one and has its own arm below: turning off
    `add-rust-environment-hash-key` gates BOTH halves of the key in that action's source,
    so the key freezes at its bare prefix and stops varying on `Cargo.lock` at all.

    Order is the load-bearing part here. The image's set has to be recorded BEFORE the
    install, because afterwards nothing can tell an image toolchain from the one this action
    added, and a prune that reads the list after the install would remove the toolchain it
    just installed or nothing at all.
    """

    for step in (
        "Record the toolchains the runner image shipped",
        "Prune toolchains this repository never named",
    ):
        if action.count(f"- name: {step}\n") != 1:
            raise AssertionError(
                f"the rust-toolchain action must declare exactly one step named {step!r}; "
                "without it the cargo cache key carries whatever toolchain the runner "
                "image happened to ship"
            )
    record = action.index("- name: Record the toolchains the runner image shipped")
    install = action.index("- name: Install the toolchain")
    prune = action.index("- name: Prune toolchains this repository never named")
    if not record < install < prune:
        raise AssertionError(
            "the image toolchain set must be recorded before the install and pruned after "
            "it; recorded after the install it cannot tell the image's toolchains from ours"
        )
    # The needle is the INVOCATION, never the name. The step's own comment names the
    # script, so a needle of `prune-image-toolchains.sh` matches a step that calls nothing
    # and this assertion could not fail. That is how it read on the first run of the
    # mutation below, which is the only reason it is written this way.
    if 'bash "$GITHUB_ACTION_PATH/prune-image-toolchains.sh"' not in action:
        raise AssertionError(
            "the prune step must call prune-image-toolchains.sh, which is the copy the "
            "behavioral cases below drive"
        )
    if "repo_pin=$repo_pin" not in action and 'repo_pin=$repo_pin' not in action:
        raise AssertionError(
            "the resolve step must publish repo_pin, because the prune keeps the channel "
            "rust-toolchain.toml names as well as the toolchain the caller asked for"
        )


def run_prune(
    tmp: Path,
    resolved: str,
    preinstalled: list[str],
    repo_pin: str = "",
    uninstall_fails: str = "",
    uninstall_fails_after: str = "",
    after: list[str] | None = None,
) -> subprocess.CompletedProcess[str]:
    """Drive the prune script against a stub rustup, so its logic is testable off CI.

    The stub is the whole point. Asserting the script's TEXT would pass on a script that
    removes the wrong toolchain, and a check that cannot fail is not evidence. `after` lets
    a case model a rustup that reports something the prune did not achieve, which is the
    straggler the assertion exists to catch.
    """

    bindir = tmp / "bin"
    bindir.mkdir(parents=True, exist_ok=True)
    state = tmp / "installed.txt"
    state.write_text("\n".join(preinstalled) + "\n", encoding="utf-8")
    forced = tmp / "forced-after.txt"
    if after is not None:
        forced.write_text("\n".join(after) + "\n", encoding="utf-8")
    stub = bindir / "rustup"
    stub.write_text(
        "#!/usr/bin/env bash\n"
        "set -euo pipefail\n"
        f'STATE="{state}"\n'
        f'FORCED="{forced}"\n'
        f'FAILS="{uninstall_fails}"\n'
        f'FAILS_AFTER="{uninstall_fails_after}"\n'
        'if [ "$1" = "toolchain" ] && [ "$2" = "list" ]; then\n'
        '  if [ -f "$FORCED" ]; then cat "$FORCED"; else cat "$STATE"; fi\n'
        "  exit 0\n"
        "fi\n"
        'if [ "$1" = "toolchain" ] && [ "$2" = "uninstall" ]; then\n'
        '  if [ -n "$FAILS" ] && [ "$3" = "$FAILS" ]; then echo "stub refuses $3" >&2; exit 1; fi\n'
        '  grep -v "^$3\\( \\|$\\)" "$STATE" > "$STATE.new" || true\n'
        '  mv "$STATE.new" "$STATE"\n'
        '  if [ -n "$FAILS_AFTER" ] && [ "$3" = "$FAILS_AFTER" ]; then echo "stub errored after removing $3" >&2; exit 1; fi\n'
        "  exit 0\n"
        "fi\n"
        "exit 0\n",
        encoding="utf-8",
    )
    stub.chmod(0o755)
    before = tmp / "before.txt"
    before.write_text("\n".join(preinstalled) + "\n", encoding="utf-8")
    env = dict(os.environ)
    env["PATH"] = f"{bindir}{os.pathsep}{env['PATH']}"
    return subprocess.run(
        ["bash", str(TOOLCHAIN_PRUNE), resolved, str(before), repo_pin],
        capture_output=True,
        text=True,
        env=env,
        check=False,
    )


def assert_toolchain_prune_behavior() -> None:
    """The prune keeps what this repository names and removes what the image shipped."""

    with tempfile.TemporaryDirectory() as raw:
        tmp = Path(raw)

        # The measured case: the image shipped a second stable nobody here chose.
        run = run_prune(
            tmp / "image-extra",
            "1.96.0",
            [
                "1.96.0-x86_64-unknown-linux-gnu (default)",
                "1.98.0-x86_64-unknown-linux-gnu",
            ],
            repo_pin="1.96.0",
        )
        if run.returncode != 0:
            raise AssertionError(f"the prune refused an ordinary image: {run.stderr}")
        if "removing 1.98.0-x86_64-unknown-linux-gnu" not in run.stdout:
            raise AssertionError(
                f"the prune left the image's own toolchain installed: {run.stdout}"
            )
        if "keeping 1.96.0-x86_64-unknown-linux-gnu" not in run.stdout:
            raise AssertionError(
                f"the prune removed the toolchain this repository pins: {run.stdout}"
            )

        # fuzz.yml's shape: a caller-supplied nightly, and a bare `cargo` that
        # rust-toolchain.toml resolves to the stable pin. Both survive.
        run = run_prune(
            tmp / "two-named",
            "nightly-2026-06-17",
            [
                "1.96.0-x86_64-unknown-linux-gnu",
                "1.98.0-x86_64-unknown-linux-gnu",
                "nightly-2026-06-17-x86_64-unknown-linux-gnu",
            ],
            repo_pin="1.96.0",
        )
        if run.returncode != 0:
            raise AssertionError(f"the prune refused the fuzz shape: {run.stderr}")
        for kept in ("1.96.0-x86_64", "nightly-2026-06-17-x86_64"):
            if f"keeping {kept}" not in run.stdout:
                raise AssertionError(
                    f"the prune removed {kept}, which this repository names: {run.stdout}"
                )
        if "removing 1.98.0-x86_64-unknown-linux-gnu" not in run.stdout:
            raise AssertionError(
                f"the prune kept a toolchain nobody named: {run.stdout}"
            )

        # A prefix must not be read as a match in either direction.
        run = run_prune(
            tmp / "prefix",
            "1.9",
            ["1.96.0-x86_64-unknown-linux-gnu"],
        )
        if run.returncode == 0:
            raise AssertionError(
                "channel 1.9 matched toolchain 1.96.0, so a prefix is being read as a "
                "match and the wrong toolchain can be kept"
            )

        # A refused uninstall must stop the job. Swallowed, it leaves a third toolchain in
        # the list under a green run, which is exactly the defect being fixed.
        run = run_prune(
            tmp / "refused",
            "1.96.0",
            [
                "1.96.0-x86_64-unknown-linux-gnu",
                "1.98.0-x86_64-unknown-linux-gnu",
            ],
            repo_pin="1.96.0",
            uninstall_fails="1.98.0-x86_64-unknown-linux-gnu",
        )
        if run.returncode == 0:
            raise AssertionError(
                "a refused uninstall exited zero, so a toolchain this repository never "
                "chose survives into the cache key under a green run"
            )
        if "could not uninstall" not in run.stderr:
            raise AssertionError(
                f"a refused uninstall did not name itself: {run.stderr}"
            )

        # The same refusal from a rustup that removed the toolchain and THEN errored. It
        # leaves no straggler, so the assertion below cannot shield it, and the loud
        # failure is the only thing that can stop the job. Without this case the
        # "could not uninstall" assertion is unreachable: every refusal the other stub mode
        # produces is caught by the straggler check first, and an assertion nothing can
        # reach is not evidence.
        run = run_prune(
            tmp / "refused-after",
            "1.96.0",
            [
                "1.96.0-x86_64-unknown-linux-gnu",
                "1.98.0-x86_64-unknown-linux-gnu",
            ],
            repo_pin="1.96.0",
            uninstall_fails_after="1.98.0-x86_64-unknown-linux-gnu",
        )
        if run.returncode == 0:
            raise AssertionError(
                "an uninstall that errored after doing the work exited zero, so nothing "
                "stops a job whose toolchain removal is only half trustworthy"
            )
        if "could not uninstall" not in run.stderr:
            raise AssertionError(
                f"a refusal that left no straggler was not named: {run.stderr}"
            )

        # And the straggler assertion, which is what turns this from a hope into a check:
        # a rustup that reports an unnamed toolchain still installed must fail the step,
        # however it got there.
        run = run_prune(
            tmp / "straggler",
            "1.96.0",
            ["1.96.0-x86_64-unknown-linux-gnu"],
            repo_pin="1.96.0",
            after=[
                "1.96.0-x86_64-unknown-linux-gnu",
                "1.99.0-x86_64-unknown-linux-gnu",
            ],
        )
        if run.returncode == 0:
            raise AssertionError(
                "a toolchain this repository never named survived the prune and the step "
                "still passed, so the invariant is asserted nowhere"
            )
        if "survived the prune" not in run.stderr:
            raise AssertionError(f"the straggler was not named: {run.stderr}")


def assert_docs_only_classifier_guard(workflow: str) -> None:
    """Require the exact workflow header, classifier job, and classifier shell."""

    if workflow_active_header_source(workflow) != DOCS_ONLY_WORKFLOW_HEADER:
        raise AssertionError(
            "diff classifier workflow header must exactly match the closed-form "
            "process environment authority contract"
        )
    classifier = workflow_job_blocks(workflow).get("changes")
    if classifier is None:
        raise AssertionError("diff classifier workflow must contain the changes job")
    job_source = classifier_active_job_source(classifier)
    if job_source != DOCS_ONLY_CLASSIFIER_JOB:
        raise AssertionError(
            "diff classifier active job must exactly match the closed-form "
            "docs_only authority contract"
        )
    if classifier_shell_source(classifier) != DOCS_ONLY_CLASSIFIER_SHELL:
        raise AssertionError("diff classifier shell extraction disagrees with its job")


WINDOWS_DAEMON_SIBLING_BUILD = (
    "- name: Build the sibling Windows daemon the authority tests drive"
)
WINDOWS_DAEMON_COMPILE_STEP = (
    "- name: Compile and run native Windows runtime authority tests"
)
WINDOWS_DAEMON_LIFECYCLE_TEST = '"daemon_status_and_stop_lifecycle"'


def assert_windows_daemon_sibling_build(ci_job: str) -> None:
    """Require the Windows leg to build the daemon its lifecycle test drives.

    The lifecycle test runs a real daemon, and the harness takes it from the
    `kin-daemon` beside the `kin` binary the test was built with. Every other
    `kin-daemon` invocation in the leg compiles `--lib`, which produces no
    binary, so without an explicit build nothing writes one for that target
    before the test reads it.

    The harness does cover a missing daemon by rebuilding one, but a rebuild
    lands beside the test only when it repeats the same `--target`, so a leg
    that leaves the build to the harness is asserting a harness behavior rather
    than supplying its own input. Ordering is pinned as well as presence: this
    build sitting after the tests is how the leg failed for days while reporting
    a daemon missing from a directory the job did eventually populate.

    Judged on active lines and scoped to the step's own block: the `--target`
    that decides where the binary lands must appear inside the sibling build
    step itself, and ordering compares step-name positions, so a comment
    quoting the test name can neither satisfy a policy nor invert the order.
    """

    active_job = "\n".join(active_lines(ci_job))
    for step in (WINDOWS_DAEMON_SIBLING_BUILD, WINDOWS_DAEMON_COMPILE_STEP):
        require(active_job, step, "native Windows daemon lifecycle prerequisite")
    build_start = active_job.index(WINDOWS_DAEMON_SIBLING_BUILD)
    compile_start = active_job.index(WINDOWS_DAEMON_COMPILE_STEP)
    if build_start > compile_start:
        raise AssertionError(
            "native Windows daemon lifecycle prerequisite must build the msvc "
            "kin-daemon binary before the lifecycle test reads it"
        )
    sibling_build_block = active_job[build_start:compile_start]
    for policy in (
        "-p kin-daemon --no-default-features --bin kin-daemon",
        "--target x86_64-pc-windows-msvc",
    ):
        require(
            sibling_build_block,
            policy,
            "native Windows daemon lifecycle prerequisite",
        )
    require(
        active_job[compile_start:],
        WINDOWS_DAEMON_LIFECYCLE_TEST,
        "native Windows daemon lifecycle prerequisite",
    )


WINDOWS_AUTHORITY_JOBS = (
    "windows-authority-tests",
    "windows-authority-cli-tests",
    "windows-authority-runtime-tests",
)
WINDOWS_AUTHORITY_LEG_HELPERS = "source ./scripts/windows-authority-legs.sh"
# The one job-level condition these three may carry, matched whole. Whole rather
# than by substring, because `github.event_name != 'pull_request'` is a
# substring of every condition that also excludes the merge queue or the push,
# and that is the thing this rule exists to catch.
WINDOWS_AUTHORITY_ADMITTED_IF = "${{ " + RELEASE_SUITE_EVENT_FILTER + " }}"
# Every native Windows leg, wherever it runs. The set is what the three jobs
# owe between them, so splitting or rebalancing them stays a free choice while
# dropping one does not: a leg that leaves this file has to leave it visibly.
WINDOWS_AUTHORITY_LEGS = (
    "kin-registry library",
    "kin-core registry authority",
    "kin-core capability-owned config replacement",
    "retained config capability exclusion",
    "kin-git library",
    "MCP Windows sibling daemon discovery",
    "kin-core repository initialization",
    "managed-install spawn fence",
    "daemon shutdown identity",
    "kin-cli Windows modules",
    "full managed uninstall safety",
    "editor extension detection",
    "native full managed uninstall lifecycle",
    "native managed-daemon ownership scan",
    "native install authority contention and crash recovery",
    "native managed-daemon spawn admission",
    "bounded CLI test subprocesses",
    "bounded daemon probe subprocesses",
    "daemon status and stop lifecycle",
    "spawned-child Windows authority ordering",
    "isolated runtime process-tree containment",
    "direct daemon-child containment",
    "late daemon-descendant containment",
    "daemon isolation support",
    "durable merge resolution containment compile",
    "editor setup guidance",
)


# The admission core. Two jobs decide whether a pull request may land
# and a third publishes the required name for the sharded one. Everything about
# that arrangement is invisible from the context name, which is why it is pinned
# here the way the Check & Test aggregates are: a required context that goes
# green on a shard that never ran is worse than no context at all, and an
# admission gate is the one place where that is cheapest to introduce and
# hardest to notice.
FAST_GATE_LINT_STEPS = (
    "run: cargo fmt -- --check",
    "run: python3 scripts/check-quarantine.py",
    "cargo clippy --all-targets --all-features -- $allows",
    "bash scripts/release-policy-gate.sh <<'RELEASE_POLICY'",
)
# What the sharded half owes. The listing assertion is the load-bearing one: the
# scope selector narrows the run to the packages a diff can have broken, and a
# nextest filter matching nothing grades nothing and exits 0, printing the same
# summary a clean pass prints. Without a listing count the whole job is a check
# that cannot fail.
FAST_GATE_SHARD_STEPS = (
    "python3 scripts/changed-crate-scope.py",
    "cargo nextest list --locked",
    'print("::error title=Empty test selection::the scope lists zero tests")',
    "cargo nextest run --locked",
    "run: python3 scripts/check-quarantine.py --report-junit",
)
FAST_GATE_SHARD_MATRIX = "shard: [1, 2, 3]"
FAST_GATE_SHARD_INDEPENDENT_LEGS = "fail-fast: false"
FIRESTORE_RESPONSE_BODY_CONTRACT_COMMAND = (
    "cargo test -p kin-spine --features firestore --locked"
)
FIRESTORE_RESPONSE_BODY_CONTRACT_LISTING = (
    "cargo test -p kin-spine --features firestore --locked -- --list"
)
FIRESTORE_RESPONSE_BODY_CONTRACT_TESTS = (
    "firestore::transient_retry_tests::query_documents_retries_a_body_that_fails_to_arrive_over_the_wire",
    "firestore::transient_retry_tests::commit_write_batch_does_not_replay_after_a_truncated_success_body",
    "firestore::transient_retry_tests::get_document_does_not_retry_a_permission_denied_with_a_truncated_body",
)
FAST_GATE_AGGREGATE_ALWAYS_RUNS = "if: ${{ !cancelled() }}"
# The Windows cross-check is in here rather than left advisory because the job
# publishes no required context of its own. Without this line a red Windows leg
# sits beside six green required contexts and the pull request merges.
FAST_GATE_AGGREGATE_NEEDS = "needs: [changes, fast-gate-tests, windows-cross-check]"
FAST_GATE_AGGREGATE_SUCCESS_GATE = 'if [ "$SHARDS" != "success" ]; then'


def assert_fast_gate_authority(workflow: str) -> None:
    """Pin the admission core so it cannot go green having graded nothing.

    Three properties, none of them observable from the required context's name.

    The aggregate ALWAYS runs. It is the only producer of the required name, and
    a required context nobody reports blocks a merge forever with every visible
    check green, which is a silent hang rather than a failure. So its condition
    is `!cancelled()` and nothing narrower, and it decides inside its own step.

    The aggregate admits `success` from the shards and nothing else. `skipped`
    and `cancelled` mean part of the selection never ran, and a required context
    green on that is worse than no context at all. The documentation-only case
    is the one exemption and it is spelled out in the step rather than inferred
    from a job condition, so a reader can see which case is being passed.

    The shards assert their LISTING before they run. The scope selector narrows
    to the packages a diff can have broken, and a nextest filter matching
    nothing grades nothing and exits 0 with the same summary a clean pass
    prints. The count is what separates the two, and it is the only thing that
    can, so it is required here rather than left to a reviewer to notice.
    """

    jobs = workflow_job_blocks(workflow)
    for job_id in ("fast-gate-lint", "fast-gate-tests", "fast-gate-tests-aggregate"):
        if job_id not in jobs:
            raise AssertionError(
                f"the admission core lost {job_id}; what blocks a pull request "
                "from landing is not something to remove quietly"
            )

    lint = active_lines(jobs["fast-gate-lint"])
    for policy in FAST_GATE_LINT_STEPS:
        if not any(policy in line for line in lint):
            raise AssertionError(
                "the admission core's lint and policy half must keep running "
                f"`{policy}`: 31 of 64 measured pull-request failures were "
                "formatting, clippy or a policy script"
            )
    lint_text = "\n".join(lint)
    if (
        FIRESTORE_RESPONSE_BODY_CONTRACT_COMMAND not in lint_text
        or FIRESTORE_RESPONSE_BODY_CONTRACT_LISTING not in lint_text
        or any(test_name not in lint_text for test_name in FIRESTORE_RESPONSE_BODY_CONTRACT_TESTS)
    ):
        raise AssertionError(
            "the admission core must run the Firestore post-status response "
            "contract with the firestore feature enabled and an exact listing; "
            "default tests compile neither the retry nor Commit body behavior"
        )

    shard = jobs["fast-gate-tests"]
    shard_lines = active_lines(shard)
    for policy in FAST_GATE_SHARD_STEPS:
        if not any(policy in line for line in shard_lines):
            raise AssertionError(
                f"the admission core's sharded half must keep running `{policy}`"
            )
    for policy in (FAST_GATE_SHARD_MATRIX, FAST_GATE_SHARD_INDEPENDENT_LEGS):
        if policy not in shard_lines:
            raise AssertionError(
                f"the admission core's shards must keep `{policy}`: one red "
                "shard cancelling its siblings makes the aggregate report one "
                "cause where there may be three"
            )

    # The matrix list and the partition denominator are one fact written in two
    # places, so they are checked against EACH OTHER rather than both pinned to
    # a number. Pinning both would let a reviewer satisfy the guard by writing
    # the same digit twice while the two lines still disagreed about what ran.
    #
    # Only one direction is dangerous. A denominator BELOW the matrix length
    # fails loudly, because nextest refuses a partition index above the count.
    # A denominator ABOVE it is silent: `shard: [1, 2, 3]` against
    # `count:${{ matrix.shard }}/5` runs three fifths of the selection, every
    # shard passes, and the aggregate reports the required context green. That
    # is the same grades-nothing-and-exits-0 shape the listing assertion above
    # exists to catch, arriving through the matrix instead of through a filter.
    shard_text = "\n".join(shard_lines)
    matrix_match = re.search(r"shard: \[([0-9,\s]+)\]", shard_text)
    if matrix_match is None:
        raise AssertionError(
            "the admission core's shards lost their matrix list, so the "
            "selection no longer has a declared number of parts"
        )
    shards = [int(value) for value in matrix_match.group(1).split(",")]
    partition_match = re.search(
        r"--partition count:\$\{\{ matrix\.shard \}\}/([0-9]+)", shard_text
    )
    if partition_match is None:
        raise AssertionError(
            "the admission core's shards must partition by their matrix index; "
            "without `--partition count:${{ matrix.shard }}/<n>` every shard "
            "runs the whole selection and the gate pays for it N times over"
        )
    denominator = int(partition_match.group(1))
    if shards != list(range(1, len(shards) + 1)) or denominator != len(shards):
        raise AssertionError(
            "the admission core's shard matrix and partition denominator are "
            f"one fact in two places and they disagree: matrix {shards} against "
            f"denominator {denominator}. A denominator above the matrix length "
            "silently drops that fraction of the selection and still reports "
            "success"
        )

    aggregate = jobs["fast-gate-tests-aggregate"]
    aggregate_lines = active_lines(aggregate)
    conditions = [line for line in aggregate_lines if line.startswith("if:")]
    if conditions != [FAST_GATE_AGGREGATE_ALWAYS_RUNS]:
        raise AssertionError(
            "the admission core's aggregate publishes the required name and "
            f"must run on every event, with `{FAST_GATE_AGGREGATE_ALWAYS_RUNS}` "
            f"and nothing narrower; it carries {conditions}. A required context "
            "nobody reports is a silent hang, not a failure"
        )
    if FAST_GATE_AGGREGATE_NEEDS not in aggregate_lines:
        raise AssertionError(
            "the admission core's aggregate must wait on the shards it grades; "
            f"missing `{FAST_GATE_AGGREGATE_NEEDS}`"
        )
    if FAST_GATE_AGGREGATE_SUCCESS_GATE not in aggregate_lines:
        raise AssertionError(
            "the admission core's aggregate must admit only `success` from the "
            "shards: `skipped` and `cancelled` mean part of the selection never "
            "ran, and a required context green on that is worse than none"
        )


def assert_windows_authority_split(ci_jobs: dict[str, str]) -> None:
    """Hold the three native Windows jobs to what one job used to owe.

    The legs were split across three jobs because one job could not finish
    inside any cap worth setting: across 119 sampled runs, 32 of the 106 allowed
    to finish were destroyed by the 60-minute limit, and the 70 survivors
    averaged 52.6 minutes against it. Three things have to stay true for the
    split to be an improvement rather than a way to lose coverage quietly.

    Every leg still runs exactly once. A split is the cheapest possible place to
    drop a test, because the leg simply is not in the job you are reading and
    the other job looks complete on its own.

    No job takes a job-level `if:` other than the one reviewed for the admission core,
    which takes them off pull requests and leaves them on the merge queue and on
    every commit that reaches main. These jobs are the merge group's only proof
    of the native Windows admission contract, and that reasoning did not change
    when one job became three, nor when the pull-request half of CI was thinned:
    a filter excluding the queue or the push is still what would quietly move
    this proof to the tag, and is still refused. The longest of the three
    measured 11.7 minutes, which no admission gate carries against a ten-minute
    open-to-merge bar, and `merge_pr_ready` refuses while any check is pending,
    so leaving them on pull requests set the lane's clock whether or not any
    ruleset named them.

    The step budgets sum to less than the job cap. GitHub CANCELS a job that
    hits its own timeout, and a cancelled job is silent on a check no ruleset
    requires and skips its cache save, so the next run starts colder than the
    one that timed out. A step that overruns its own budget FAILS instead, which
    is loud and still runs the save. Keeping the sum under the cap is what makes
    the loud failure arrive first, whichever step is the slow one.
    """

    combined = "\n".join(
        "\n".join(active_lines(ci_jobs[job])) for job in WINDOWS_AUTHORITY_JOBS
    )
    for leg in WINDOWS_AUTHORITY_LEGS:
        occurrences = combined.count(f'"{leg}"')
        if occurrences != 1:
            raise AssertionError(
                "the native Windows authority jobs must run every reviewed leg "
                f"exactly once; '{leg}' appears {occurrences} times across "
                + ", ".join(WINDOWS_AUTHORITY_JOBS)
            )

    for job_id in WINDOWS_AUTHORITY_JOBS:
        block = ci_jobs[job_id]
        conditions = re.findall(r"(?m)^    if: (.*)$", block)
        if conditions != [] and conditions != [WINDOWS_AUTHORITY_ADMITTED_IF]:
            raise AssertionError(
                "the Windows authority jobs must stay on the merge queue and "
                "on every main commit, so admission behavior is asserted "
                "before a release commit can carry it. The only reviewed "
                f"condition is `{WINDOWS_AUTHORITY_ADMITTED_IF}`, and "
                f"{job_id} carries {conditions}"
            )
        require(block, WINDOWS_AUTHORITY_LEG_HELPERS, f"shared leg helpers in {job_id}")
        job_cap = re.search(r"(?m)^    timeout-minutes: (?P<cap>\d+)$", block)
        if job_cap is None:
            raise AssertionError(f"{job_id} must declare one job timeout")
        step_budgets = [
            int(budget)
            for budget in re.findall(r"(?m)^        timeout-minutes: (\d+)$", block)
        ]
        if not step_budgets:
            raise AssertionError(
                f"{job_id} must budget its long steps, or a slow step can only "
                "be reported by cancelling the job"
            )
        if sum(step_budgets) >= int(job_cap.group("cap")):
            raise AssertionError(
                f"{job_id} step budgets sum to {sum(step_budgets)} against a job "
                f"cap of {job_cap.group('cap')}: an overrun there is cancelled "
                "silently instead of failing loudly"
            )


def assert_windows_npm_first_run_proof(ci_job: str, proof_source: str) -> None:
    """Require both public npm surfaces to pass a real native Windows first run.

    Unit tests on Linux can prove package policy, but not Windows executable
    names or sibling-daemon discovery. Keep the canonical package, compatibility
    wrapper, built binary pair, and PATH-free runtime exercise in one required
    Windows job so none can be inferred from the others.
    """

    active_job_lines = active_lines(ci_job)
    active_job = "\n".join(active_job_lines)
    for command in (
        "npm test --prefix ./packages/kin",
        "npm run lint --prefix ./packages/kin",
        "npm test --prefix ./packages/kin-mcp",
        "npm run lint --prefix ./packages/kin-mcp",
    ):
        if command not in active_job_lines:
            raise AssertionError(
                "native Windows npm first-run CI proof is missing exact command: "
                f"{command}"
            )
    for policy in (
        "actions/setup-node@v7",
        "node-version: 20",
        "Test both public npm surfaces on native Windows",
        "node --check ./scripts/prove-windows-npm-first-run.mjs",
        "MCP Windows sibling daemon discovery",
        "daemon_delegate::tests::windows_daemon_discovery_finds_platform_sibling_without_path",
        "-p kin-mcp --no-default-features --lib",
        "Build the Windows binaries the admission and npm assertions drive",
        "-p kin-cli -p kin-daemon --no-default-features",
        "--bin kin --bin kin-daemon",
        "Prove both npm surfaces from built Windows binaries",
        "KIN_NPM_PROOF_KIN_BIN: ${{ github.workspace }}/target/x86_64-pc-windows-msvc/debug/kin.exe",
        "KIN_NPM_PROOF_DAEMON_BIN: ${{ github.workspace }}/target/x86_64-pc-windows-msvc/debug/kin-daemon.exe",
        "node ./scripts/prove-windows-npm-first-run.mjs",
    ):
        require(active_job, policy, "native Windows npm first-run CI proof")

    npm_tests = active_job.index("npm test --prefix ./packages/kin")
    binary_build = active_job.index(
        "Build the Windows binaries the admission and npm assertions drive"
    )
    runtime_proof = active_job.index(
        "Prove both npm surfaces from built Windows binaries"
    )
    if not npm_tests < binary_build < runtime_proof:
        raise AssertionError(
            "native Windows npm proof must run package tests before building the "
            "exact binaries its first-run integration drives"
        )

    active_proof = "\n".join(active_lines(proof_source))
    for policy in (
        "if (process.platform !== 'win32' && !hostOverride) {",
        "requireBuiltBinary('KIN_NPM_PROOF_KIN_BIN', expectedKinName)",
        "requireBuiltBinary('KIN_NPM_PROOF_DAEMON_BIN', expectedDaemonName)",
        "await copyExecutable(builtKin, managedKin)",
        "await copyExecutable(builtDaemon, managedDaemon)",
        "writeLauncherStamp(targetKinVersion(), { KIN_HOME: kinHome })",
        "setEnv(env, 'KIN_NO_PROVISION', '1')",
        "deleteEnv(env, 'KIN_DAEMON_BIN')",
        "assertPathExcludes(env, path.dirname(managedKin), '@kinlab/kin')",
        "assert.ok(path.isAbsolute(managedKin), '@kinlab/kin managed binary must be absolute')",
        "[canonicalKinLauncher, 'status', '--json']",
        "[canonicalKinLauncher, 'search', 'greet', '--json']",
        "launcher: canonicalMcpLauncher",
        "setEnv(env, 'KIN_MCP_AUTO_INIT', '1')",
        "launcher: compatibilityMcpLauncher",
        "name: 'locate'",
        "assert.match(rendered, /greet/i",
        "holdsEntity(replies, 'greet', 'crate')",
        "['daemon', 'stop', '--all', '--json']",
    ):
        require(active_proof, policy, "native Windows npm first-run harness")

    canonical_start = proof_source.index("async function proveCanonical(")
    compatibility_start = proof_source.index(
        "async function proveCompatibility(", canonical_start
    )
    canonical = "\n".join(
        active_lines(proof_source[canonical_start:compatibility_start])
    )
    if re.search(r"setEnv\(env, ['\"]KIN_DAEMON_BIN['\"]", canonical):
        raise AssertionError(
            "canonical Windows npm proof must not inject KIN_DAEMON_BIN; the "
            "absolute managed kin.exe must discover its sibling daemon"
        )
    for policy in (
        "const hostilePath = [managedBinDir, readEnv(process.env, 'PATH') || '']",
        "setEnv(env, 'PATH', pathWithoutDirectory(hostilePath, managedBinDir))",
    ):
        require(active_proof, policy, "non-vacuous PATH-free Windows npm proof")


def assert_windows_npm_archive_authority(
    canonical_source: str,
    canonical_test: str,
    compatibility_source: str,
    compatibility_test: str,
) -> None:
    """Pin real-ZIP extraction to Windows system authority in both packages."""

    for label, source, end_marker in (
        (
            "canonical npm provisioner",
            canonical_source,
            "/**\n * Download, verify, and install the pinned Kin release.",
        ),
        (
            "compatibility npm provisioner",
            compatibility_source,
            "async function installFromArchive(",
        ),
    ):
        start = source.index("function archiveExtraction(")
        end = source.index(end_marker, start)
        extraction = "\n".join(active_lines(source[start:end]))
        require(
            "\n".join(active_lines(source)),
            "path.win32.join(systemRoot, 'System32', 'tar.exe')",
            f"{label} absolute System32 extraction authority",
        )
        for policy in (
            # The TARGET decides the archive layout.
            "if (platform === 'win32') {",
            # The HOST decides the extractor, and it arrives as a parameter
            # defaulting to the real platform rather than being read inline.
            # That default is what keeps production honest; the parameter is
            # what makes the cross-target case testable from one machine. It
            # used to read `process.platform` here, and a Unix archive unpacked
            # on a Windows host was then reachable only on the native Windows
            # leg, which does not grade a pull request. It reddened main once.
            "host = process.platform",
            "if (host === 'win32') {",
            "executable: windowsSystemTarPath(env)",
            "executable: '/usr/bin/unzip'",
            # The Unix arm names its extractor absolutely too, falling back to
            # the same System32 bsdtar on a Windows host rather than to a PATH
            # lookup. A bare `tar` here is what this line pins shut: PATH
            # belongs to whoever started the process, and the archive being
            # unpacked is the one whose SHA-256 was just verified.
            "host === 'win32' ? windowsSystemTarPath(env) : unixSystemToolPath('tar')",
        ):
            require(extraction, policy, f"{label} Windows ZIP extraction authority")

    for label, test_source, test_name in (
        (
            "canonical npm provisioner",
            canonical_test,
            "provision uses deterministic Windows ZIP extraction under a hostile PATH",
        ),
        (
            "compatibility npm provisioner",
            compatibility_test,
            "ensureKinBinary installs the flat native Windows zip and .exe pair",
        ),
    ):
        active_test = "\n".join(active_lines(test_source))
        for policy in (
            test_name,
            "environmentWithHostileTar",
            "process.platform === 'win32' ? 'tar.exe' : 'tar'",
            "env.PATH = [hostileBin, originalPath]",
            "windowsSystemTarPath()",
            "'/usr/bin/zip'",
            "subarray(0, 4).toString('hex')",
            "'504b0304'",
            # The host/target matrix, asserted on every runner rather than only
            # on the one whose platform the branch happens to name.
            "the extractor is chosen by the host and the layout by the target",
        ):
            require(active_test, policy, f"{label} genuine Windows ZIP regression")


def job_display_name(job: str) -> str:
    """Return the required exact one-line display name for a CI job block."""

    name = optional_job_display_name(job)
    if name is None:
        raise AssertionError(
            "every CI job must carry exactly one one-line display name"
        )
    return name


def real_check_job_authority_source(job: str) -> str:
    """Return the real check job's admission fields and top-level controls."""

    active_lines = classifier_active_job_source(job).splitlines()
    try:
        steps = active_lines.index("  steps:")
    except ValueError as error:
        raise AssertionError(
            "real Check & Test job is missing its steps mapping"
        ) from error
    authority = active_lines[: steps + 1]
    authority.extend(
        line
        for line in active_lines[steps + 1 :]
        if len(line) - len(line.lstrip()) == 2
    )
    return "\n".join(authority)


def assert_assertion_reachability_gate_wired(workflow: str) -> None:
    """Keep the gate that proves this suite's own checks are reachable.

    This suite cannot notice that one of its assertions stopped being called;
    that is how `assert_windows_public_support_contract` sat defined and
    unreferenced after a stale-base merge removed its call site. A separate
    gate answers that question, and this assertion is the other half of the
    pair: the reachability gate reports an orphaned check in this file, and
    this check reports a reachability gate that CI no longer runs. Removing
    either one turns the other red, so neither can be lost quietly.
    """

    # Match the invocation exactly rather than searching for the path. A
    # substring search accepts `run: # python3 <path>`, which leaves the path
    # in a line that does not itself start with `#` while running nothing.
    # active_lines() is the wrong tool here for a second reason: it strips
    # `/* */` for the shell and JavaScript it is normally handed, and ci.yml
    # holds a bare `docs/*` glob and a later `**/Cargo.lock` that the pattern
    # spans as one 24KB comment, taking most of the check job with it.
    command = f"python3 {ASSERTION_REACHABILITY_POLICY}"
    invocations = {command, f"run: {command}"}
    if not any(line.strip() in invocations for line in workflow.splitlines()):
        raise AssertionError(
            "ci.yml must run "
            f"{ASSERTION_REACHABILITY_POLICY}; without it an assertion can lose "
            "its call site and keep reporting green"
        )
    if not ASSERTION_REACHABILITY.is_file():
        raise AssertionError(
            f"{ASSERTION_REACHABILITY_POLICY} is missing; the release gates "
            "would no longer prove their own checks run"
        )


def assert_release_version_gate_wired(ci: str) -> None:
    """Keep the version gate running on the pull request that carries a release.

    `scripts/check-release-version.mjs` has carried the right refusal since
    before the release train existed, and until this job it ran on no pull
    request at all. Its one real invocation sits inside the train's own
    branch-writer step, against a tree that step has just regenerated, so it
    passes once and never re-runs on the head that merges. A release pull
    request could therefore lose its version bump after that step and every
    check would stay green, after which the mint reads a workspace version
    whose tag already exists, reports nothing to release, and exits 0 on a
    fifteen-minute cron with no tag cut and nothing raised.

    Every clause below is load-bearing. The event restriction is what keeps the
    job off the merge queue, where there is no pull request to read a base sha
    from. The branch and label conditions are what scope it to releases: the
    gate refuses a release-affecting diff that does not move the version, and
    asking that of every pull request is a policy change wearing a bug fix's
    clothes. `fetch-depth: 0` is what lets it read the base commit's manifest
    at all. And the base and labels reach the script through the environment,
    never interpolated into a command line, because a pull-request label is
    text an outside contributor writes.
    """

    for path, policy in (
        (RELEASE_VERSION_GUARD, RELEASE_VERSION_GUARD_POLICY),
        (RELEASE_VERSION_FALSIFIER, RELEASE_VERSION_FALSIFIER_POLICY),
    ):
        if not path.is_file():
            raise AssertionError(
                f"{policy} is missing; a release pull request could drop its "
                "version bump and no check would go red"
            )

    job = workflow_job_blocks(ci).get("release-version")
    if job is None:
        raise AssertionError(
            "ci.yml must run the release version gate on pull requests; without "
            "that job the gate's only invocation is the release train's own "
            "branch writer, which never sees the head that merges"
        )

    # Comment-aware, but line by line rather than through `active_lines`.
    # That helper strips `/* ... */` with DOTALL, and ci.yml carries a `/*` and
    # a `*/` about 46,000 characters apart in unrelated shell, so running it
    # over the whole workflow deletes roughly a third of the file and every
    # assertion built on the result passes without ever seeing the lines it
    # names.
    job_lines = {
        line.strip()
        for line in job.splitlines()
        if line.strip() and not line.strip().startswith("#")
    }
    for clause in (
        "github.event_name == 'pull_request' &&",
        "(github.head_ref == 'automation/release-next' ||",
        "contains(github.event.pull_request.labels.*.name, 'release:automated'))",
        "fetch-depth: 0",
        "BASE_SHA: ${{ github.event.pull_request.base.sha }}",
        "PR_LABELS: ${{ join(github.event.pull_request.labels.*.name, ',') }}",
        f"run: node {RELEASE_VERSION_GUARD_POLICY}",
    ):
        if clause not in job_lines:
            raise AssertionError(
                "ci.yml must run the release version gate against the release "
                f"pull request's own base and labels; missing `{clause}`"
            )

    # Match whole invocations rather than searching for the path, which a
    # commented-out line would still satisfy.
    ci_lines = {line.strip() for line in ci.splitlines()}
    missing = sorted(
        command
        for command in (
            f"run: python3 ./{RELEASE_VERSION_FALSIFIER_POLICY}",
            f"./{RELEASE_INTENT_SUITE_POLICY} \\",
            f"{RELEASE_INTENT_SUITE_POLICY} \\",
            f"./{RELEASE_VERSION_SUITE_POLICY} \\",
            f"{RELEASE_VERSION_SUITE_POLICY} \\",
        )
        if command not in ci_lines
    )
    if missing:
        raise AssertionError(
            "ci.yml must run " + " and ".join(missing) + "; the guards' own "
            "suites and the run that watches them fail are the only evidence "
            "that either gate can refuse anything"
        )


def assert_check_consumer_authority(workflow: str) -> None:
    """Pin both jobs that can emit the release-required Check & Test contexts."""

    blocks = workflow_job_blocks(workflow)
    actual_names = {job: job_display_name(block) for job, block in blocks.items()}
    if actual_names != CI_JOB_DISPLAY_NAMES:
        raise AssertionError(
            "Check & Test consumer authority requires the exact reviewed CI job "
            "identity and display-name map"
        )

    stub = blocks.get("check-pr-fast-path")
    real = blocks.get("check")
    if stub is None or classifier_active_job_source(stub) != PULL_REQUEST_CHECK_STUB:
        raise AssertionError(
            "Check & Test consumer authority requires the exact inert "
            "pull-request job"
        )
    if (
        real is None
        or real_check_job_authority_source(real) != REAL_CHECK_JOB_AUTHORITY
    ):
        raise AssertionError(
            "Check & Test consumer authority requires the exact real check admission "
            "and matrix contract"
        )


def feature_permutation_steps(job: str) -> list[str]:
    """Return complete canonical step contracts, preserving duplicate steps."""

    rendered = classifier_active_job_source(job)
    marker = "\n  steps:\n"
    if rendered.count(marker) != 1:
        raise AssertionError("feature permutation authority requires one steps mapping")
    source = rendered.split(marker, 1)[1]
    starts = [match.start() for match in re.finditer(r"(?m)^    - ", source)]
    if not starts or starts[0] != 0:
        raise AssertionError("feature permutation authority requires canonical steps")
    return [
        textwrap.dedent(source[start:end]).strip()
        for start, end in zip(starts, starts[1:] + [len(source)])
    ]


def assert_feature_permutation_authority(workflow: str) -> None:
    """Keep every reviewed feature command behind one fail-closed OS context.

    A matrix worker can succeed while every test step is skipped, so an aggregate
    result alone cannot prove coverage. Pin the two groups, command inventory and
    routing together. Separate OS workers keep one OS failure from deciding the
    other OS's aggregate; fail-fast is off so each group records its own outcome.
    """

    blocks = workflow_job_blocks(workflow)
    stub = blocks.get("feature-tests-pr-fast-path")
    if stub is None or classifier_active_job_source(stub) != FEATURE_PERMUTATION_PR_STUB:
        raise AssertionError("feature permutation authority requires the exact PR stub")
    for worker, (os_name, aggregate) in FEATURE_PERMUTATION_WORKERS.items():
        job = blocks.get(worker)
        if job is None:
            raise AssertionError(f"feature permutation authority lost worker {worker}")
        expected_header = FEATURE_PERMUTATION_WORKER_AUTHORITY.replace("@JOB@", worker).replace("@OS@", os_name)
        if real_check_job_authority_source(job) != expected_header:
            raise AssertionError(f"feature permutation authority changed worker admission, environment or matrix: {worker}")
        if feature_permutation_steps(job) != list(FEATURE_PERMUTATION_STEPS):
            raise AssertionError(f"feature permutation authority changed ordered commands, setup or group routing: {worker}")
        # The join the exact matches above imply, stated where a reader looks
        # for it: every partition the denominator names is a matrix leg.
        active = classifier_active_job_source(job)
        partitions = re.findall(r"(?m)^\s+partition: \[([^\]]*)\]$", active)
        denominators = set(re.findall(r"--partition count:\$\{\{ matrix\.partition \}\}/(\d+)", active))
        expected = ", ".join(str(n) for n in range(1, FEATURE_PERMUTATION_PARTITIONS + 1))
        if partitions != [expected] or denominators != {str(FEATURE_PERMUTATION_PARTITIONS)}:
            raise AssertionError(
                f"feature permutation authority requires the partition matrix and the "
                f"nextest denominator to name the same {FEATURE_PERMUTATION_PARTITIONS} parts: {worker}"
            )
        gate = blocks.get(aggregate)
        if gate is None:
            raise AssertionError(f"feature permutation authority lost aggregate {aggregate}")
        expected_header = FEATURE_PERMUTATION_AGGREGATE_AUTHORITY.replace("@JOB@", aggregate).replace("@OS@", os_name).replace("@WORKER@", worker)
        if real_check_job_authority_source(gate) != expected_header:
            raise AssertionError(f"feature permutation authority changed aggregate admission, dependencies or matrix: {aggregate}")
        expected_step = FEATURE_PERMUTATION_AGGREGATE_STEP.replace("@WORKER@", worker)
        if feature_permutation_steps(gate) != [expected_step]:
            raise AssertionError(f"feature permutation authority requires an exact success-only aggregate: {aggregate}")


def assert_feature_permutation_aggregate_results(workflow: str) -> None:
    """Exercise the actual aggregate shell with successful and absent work."""

    blocks = workflow_job_blocks(workflow)
    for _, aggregate in FEATURE_PERMUTATION_WORKERS.values():
        shell = workflow_step_shell(blocks[aggregate], "Require every feature group to have succeeded")
        for outcome in ("success", "failure", "skipped", "cancelled", "", "neutral", "timed_out"):
            expected = 0 if outcome == "success" else 1
            result = subprocess.run(
                ["bash", "--noprofile", "--norc", "-euo", "pipefail", "-c", shell],
                env={"PATH": os.defpath, "SHARDS": outcome},
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
            if result.returncode != expected:
                raise AssertionError(
                    f"feature permutation aggregate {aggregate} admitted {outcome!r} incorrectly: "
                    f"expected exit {expected}, got {result.returncode}: {result.stdout}{result.stderr}"
                )


def assert_feature_permutation_falsification(workflow: str) -> None:
    """Prove omitted, duplicated, misrouted and incomplete work is refused."""

    blocks = workflow_job_blocks(workflow)
    for worker, (os_name, aggregate) in FEATURE_PERMUTATION_WORKERS.items():
        job = blocks[worker]
        mutations = [
            ("the worker leaves its OS", f"os: [{os_name}]", "os: [windows-latest]"),
            ("the worker mixes both OS outcomes", f"os: [{os_name}]", "os: [ubuntu-latest, macos-latest]"),
            ("one phase disappears", "phase: [gcs, hosted, libraries, featureless-cli, featureless-daemon]", "phase: [gcs, hosted, libraries, featureless-cli]"),
            ("a phase is duplicated", "phase: [gcs, hosted, libraries, featureless-cli, featureless-daemon]", "phase: [gcs, hosted, libraries, featureless-cli, featureless-daemon, gcs]"),
            ("a featureless partition disappears", "        partition: [1, 2]\n", "        partition: [1]\n"),
            ("an unpartitioned phase runs twice", "          - phase: gcs\n            partition: 2\n", ""),
            ("a partitioned suite loses a part", "          - phase: libraries\n            partition: 2\n", "          - phase: libraries\n            partition: 2\n          - phase: featureless-cli\n            partition: 2\n"),
            ("one failure cancels its sibling", "fail-fast: false", "fail-fast: true"),
            ("a worker masks all of its failures", "    timeout-minutes: 60\n", "    timeout-minutes: 60\n    continue-on-error: true\n"),
            ("a worker weakens warnings", '      CARGO_INCREMENTAL: "0"', '      CARGO_INCREMENTAL: "0"\n      RUSTFLAGS: ""'),
            ("a cache sharer changes build environment", 'CARGO_PROFILE_TEST_DEBUG: "0"', 'CARGO_PROFILE_TEST_DEBUG: "2"'),
            ("a feature worker writes its owner's cache", "save-if: false", "save-if: ${{ github.ref == 'refs/heads/main' }}"),
            ("the image toolchains split the cache key", "add-rust-environment-hash-key: false", "add-rust-environment-hash-key: true"),
            ("the cache action moves to an unreviewed ref", "Swatinem/rust-cache@c19371144df3bb44fab255c43d04cbc2ab54d1c4", "Swatinem/rust-cache@v2"),
            ("the macOS cache loses its owner", "shared-key: ${{ matrix.os == 'macos-latest' && 'check-macos' || 'check' }}", "shared-key: check"),
            ("the toolchain setup is conditional", "- name: Install Rust toolchain\n", "- name: Install Rust toolchain\n        if: matrix.phase == 'gcs'\n"),
            ("a test is routed to the wrong phase", "if: matrix.phase == 'featureless-cli'", "if: matrix.phase == 'gcs'"),
            ("a hosted command runs in every phase", "        if: matrix.phase == 'hosted'\n", ""),
            ("a test overrides the warnings gate", "- name: Test gcs feature\n", "- name: Test gcs feature\n        env:\n          RUSTFLAGS: \"\"\n"),
            ("a test failure becomes advisory", "- name: Test gcs feature\n", "- name: Test gcs feature\n        continue-on-error: true\n"),
            ("Ubuntu exact assertions lose their platform guard", "if: matrix.phase == 'gcs' && matrix.os == 'ubuntu-latest'", "if: matrix.phase == 'gcs'"),
            ("Ubuntu exact assertions become unreachable", "if: matrix.phase == 'gcs' && matrix.os == 'ubuntu-latest'", "if: matrix.phase == 'featureless-cli' && matrix.os == 'macos-latest'"),
            ("minimal libraries are hardcoded", 'for name, spec in sorted(deps.items()):', 'for name, spec in []:'),
            ("a package loses its integration targets", "--no-default-features --lib --tests --locked", "--no-default-features --lib --locked"),
            ("a test failure is masked", "run: cargo nextest run -p kin-daemon --features gcs --locked", "run: cargo nextest run -p kin-daemon --features gcs --locked || true"),
            ("a featureless suite stops partitioning", " --partition count:${{ matrix.partition }}/2\n", "\n"),
            ("the partition denominator drifts above the matrix", "--partition count:${{ matrix.partition }}/2", "--partition count:${{ matrix.partition }}/3"),
            ("a featureless suite returns to cargo test", "run: cargo nextest run -p kin-cli --no-default-features", "run: cargo test -p kin-cli --no-default-features"),
        ]
        # Remove and duplicate every test STEP, including both exact Ubuntu
        # assertions. The list comparison also rejects reordered commands.
        matches = list(re.finditer(r"(?m)^      - name: Test .+$", job))
        for index, match in enumerate(matches):
            start = match.start()
            following = re.search(r"(?m)^      - ", job[match.end():])
            end = match.end() + following.start() if following else len(job)
            step = job[start:end]
            name = match.group().strip().removeprefix("- name: ")
            mutations.extend(((f"{name} is omitted", step, ""), (f"{name} is duplicated", step, step + "\n" + step)))
        first = job.index("      - name: Test gcs feature\n")
        second = job.index("      - name: Test hosted gcs spine cursor hold\n")
        third = job.index("      - name: Test kin-spine durable publication paths\n")
        first_two = job[first:third]
        mutations.append(("hosted command order changes", first_two, job[second:third] + job[first:second]))
        for label, old, new in mutations:
            if old not in job:
                raise AssertionError(f"feature permutation falsification could not identify {worker}: {label}")
            mutant = workflow.replace(job, job.replace(old, new, 1), 1)
            expect_assertion(label, "feature permutation authority", lambda mutant=mutant: assert_feature_permutation_authority(mutant))

        gate = blocks[aggregate]
        for label, old, new in (
            ("the aggregate no longer waits on its worker", f"needs: [changes, {worker}]", "needs: changes"),
            ("the aggregate reads the other OS result", f"needs.{worker}.result", "needs.check.result"),
            ("the aggregate accepts skipped work", 'if [ "$SHARDS" != "success" ]; then', 'if [ "$SHARDS" = "failure" ]; then'),
            ("the aggregate loses its required suffix", f"os: [{os_name}]", "os: []"),
            ("the aggregate accepts cancellation", "!cancelled()", "always()"),
            ("a failed worker suppresses the aggregate", "!cancelled()", "success()"),
            ("the aggregate can emit a second expanded check", f"os: [{os_name}]", "os: [ubuntu-latest, macos-latest]"),
            ("the aggregate masks its failure", "exit 1", "exit 0"),
        ):
            if old not in gate:
                raise AssertionError(f"feature permutation falsification could not identify {aggregate}: {label}")
            mutant = workflow.replace(gate, gate.replace(old, new, 1), 1)
            expect_assertion(label, "feature permutation authority", lambda mutant=mutant: assert_feature_permutation_authority(mutant))
        mutant = workflow.replace(gate, gate.replace('if [ "$SHARDS" != "success" ]; then', 'if [ "$SHARDS" = "failure" ]; then', 1), 1)
        expect_assertion("runtime aggregate admits skipped groups", "feature permutation aggregate", lambda mutant=mutant: assert_feature_permutation_aggregate_results(mutant))
    stub = blocks["feature-tests-pr-fast-path"]
    for label, old, new in (
        ("the PR stub also runs on main", "github.event_name == 'pull_request'", "github.event_name != 'merge_group'"),
        # The merge queue grades export pull requests like any other, so the
        # stub has to publish their names too; an exclusion leaves an export
        # waiting on two contexts nothing will create.
        ("the PR stub skips export pull requests", "github.event_name == 'pull_request' }}", "github.event_name == 'pull_request' && !startsWith(github.head_ref, 'feature/public-export-') }}"),
        ("the PR stub expands the wrong context", "os: [ubuntu-latest, macos-latest]", "os: [ubuntu-latest]"),
        ("the PR stub ignores cancellation", "!cancelled()", "always()"),
    ):
        if old not in stub:
            raise AssertionError(f"feature permutation falsification could not identify PR stub: {label}")
        mutant = workflow.replace(stub, stub.replace(old, new, 1), 1)
        expect_assertion(label, "feature permutation authority", lambda mutant=mutant: assert_feature_permutation_authority(mutant))


def assert_partition_join(job: str, context: str) -> None:
    """Require a shard matrix of exactly 1..N beside a `count:.../N` partition.

    The matrix and the denominator are one fact in two places. A matrix shorter
    than the denominator dispatches that fraction of the suite and every shard
    reports success; a longer one runs a partition nextest refuses, or runs one
    twice. Neither is visible from the required context's name.
    """

    active = classifier_active_job_source(job)
    matrices = re.findall(r"(?m)^\s+shard: \[([^\]]*)\]$", active)
    denominators = re.findall(r"--partition count:\$\{\{ matrix\.shard \}\}/(\d+)", active)
    if len(matrices) != 1 or len(denominators) != 1:
        raise AssertionError(
            f"{context} requires one shard matrix and one partition denominator"
        )
    total = int(denominators[0])
    expected = ", ".join(str(n) for n in range(1, total + 1))
    if matrices[0] != expected:
        raise AssertionError(
            f"{context}: the shard matrix [{matrices[0]}] and the partition denominator "
            f"{total} are one fact in two places and they disagree"
        )


def assert_lint_and_shard_split(
    lint: str, shards: str, gates: tuple[str, ...], shard_steps: tuple[str, ...], context: str
) -> None:
    """The lint job runs every named gate once, and the shards run none of them.

    A gate that left the lint job runs nowhere, and one that moved onto the
    shards runs once per partition for one answer and puts its minutes back in
    front of a partition. Both arrive as a green run. A step a partition
    consumes must be on every shard, unconditioned, or a shard ran without it
    and was not a partition of the suite.
    """

    lint_conditions = shard_step_conditions(lint)
    shard_conditions = shard_step_conditions(shards)
    for gate in gates:
        if gate not in lint_conditions:
            raise AssertionError(
                f"{context} requires the gate {gate!r} in its lint job; a gate that "
                "left it is a gate nothing runs"
            )
        if "matrix." in lint_conditions[gate]:
            raise AssertionError(
                f"{context} requires {gate!r} to run unconditionally in its lint job"
            )
        if gate in shard_conditions:
            raise AssertionError(
                f"{context} requires {gate!r} to run once, in the lint job, and not on "
                "the shards, where it pays once per partition for one answer"
            )
    for step in shard_steps:
        if step not in shard_conditions:
            raise AssertionError(
                f"{context} requires the step {step!r}, which a partition consumes"
            )
        if "matrix.shard" in shard_conditions[step]:
            raise AssertionError(
                f"{context} requires {step!r} on every shard; a shard that ran "
                "without it did not run a partition of the suite"
            )


def assert_macos_shard_authority(workflow: str) -> None:
    """Pin the sharded macOS producer of `Check & Test (macos-latest)`.

    Sharding a required context is the cheapest possible way to lose part of a
    test suite. The shards and the lint job publish names no ruleset requires,
    so nothing outside this file notices what they run; the aggregate publishes
    the name the ruleset does require, and it is a five-second job that compiles
    nothing. Everything that makes it evidence rather than decoration is
    asserted here, because none of it is visible from the context name:

    the aggregate admits only `success` from the lint job and from the shard
    roll-up, so a SKIPPED or CANCELLED one fails it instead of passing it; it
    carries a one-value matrix, so a skipped aggregate publishes the bare name
    and cannot put a second check run under the required expanded one beside
    the documentation-only stub; the shards run the partitions, which together
    are the suite one unpartitioned run ran; the lint job runs clippy, the
    boundary lint and the doctests nextest does not run, once; and fail-fast is
    off, so a red shard cannot cancel a sibling that was passing.
    """

    blocks = workflow_job_blocks(workflow)
    shards = blocks.get("check-macos")
    lint = blocks.get("check-macos-lint")
    aggregate = blocks.get("check-macos-aggregate")
    if shards is None or lint is None or aggregate is None:
        raise AssertionError(
            "macOS shard authority requires the shard job, its lint job and its aggregate"
        )
    if real_check_job_authority_source(aggregate) != MACOS_SHARD_AGGREGATE_AUTHORITY:
        raise AssertionError(
            "macOS shard authority requires the exact reviewed aggregate admission "
            "and matrix contract"
        )
    if real_check_job_authority_source(lint) != MACOS_LINT_AUTHORITY:
        raise AssertionError(
            "macOS shard authority requires the exact reviewed lint job admission, "
            "runner and environment"
        )

    active_shards = classifier_active_job_source(shards)
    for policy in (
        MACOS_SHARD_RUNNER,
        MACOS_SHARD_INDEPENDENT_LEGS,
        MACOS_SHARD_MATRIX,
        MACOS_SHARD_PARTITION,
    ):
        require(active_shards, policy, "macOS shard authority")
    assert_partition_join(shards, "macOS shard authority")
    require(classifier_active_job_source(lint), MACOS_SHARD_DOCTESTS, "macOS shard authority")
    assert_lint_and_shard_split(
        lint, shards, MACOS_LINT_GATES, MACOS_SHARD_STEPS, "macOS shard authority"
    )
    active_aggregate = classifier_active_job_source(aggregate)
    for gate in (MACOS_SHARD_SUCCESS_GATE, LINT_SUCCESS_GATE):
        require(active_aggregate, gate, "macOS shard authority")


FALSIFY_HARNESS = ROOT / "scripts" / "falsify-zero-file-search.py"
# The sharded falsification's load-bearing lines. The shards are processes on
# one runner rather than jobs, so the required `Falsify guards` context still
# names one job, and these are what keep the split from shrinking the proof:
# every shard is started with its own share, every shard's exit is read, and
# the receipts must add up to every probe unit exactly once.
FALSIFY_SHARD_POLICIES = (
    'python3 scripts/falsify-zero-file-search.py --shard "$shard/$shards" "$shard_root/$shard"',
    'wait "$pid" || failed=1',
    'receipt="$(grep -E "^FALSIFY_SHARD $shard/$shards ran=[0-9]+ total=[0-9]+$" "$shard_root/$shard.log" || true)"',
    'if [[ -n "$total_seen" && "$total" != "$total_seen" ]]; then',
    'if [[ -z "$total_seen" || "$total_seen" -eq 0 || "$ran_sum" -ne "$total_seen" ]]; then',
    "if [[ $failed -ne 0 ]]; then",
)


def assert_falsify_guard_sharding(workflow: str) -> None:
    """Hold the sharded Zero File-Search falsification to the whole proof.

    The harness splits its probe units across processes, each on its own copy
    of the tree, and prints a receipt of how many units it ran out of how many
    exist. A wrapper that stopped reading a shard's exit, stopped summing the
    receipts, or started every shard on the same share would still print the
    same green line while proving a fraction of what it claims.
    """

    job = workflow_job_blocks(workflow).get("falsify-guards")
    if job is None:
        raise AssertionError("falsify guard sharding requires the falsify-guards job")
    step = workflow_step_shell(job, "Falsify Zero File-Search guards")
    for policy in FALSIFY_SHARD_POLICIES:
        if step.count(policy) != 1:
            raise AssertionError(
                f"falsify guard sharding requires exactly one {policy!r} in its step"
            )
    shards = re.findall(r"(?m)^shards=(\d+)$", step)
    if shards != ["4"]:
        raise AssertionError(
            "falsify guard sharding requires one fixed shard count of 4, the runner's cores"
        )


def assert_falsify_harness_refuses_bad_shards() -> None:
    """A shard argument the harness cannot honour exits 2 before any probe runs."""

    with tempfile.TemporaryDirectory() as directory:
        for argument in ("0/4", "5/4", "4", "1/0", "x/4"):
            result = subprocess.run(
                ["python3", str(FALSIFY_HARNESS), "--shard", argument, directory],
                capture_output=True, text=True, timeout=60, check=False,
            )
            if result.returncode != 2:
                raise AssertionError(
                    f"falsify guard sharding: --shard {argument} exited {result.returncode}, not 2: "
                    f"{result.stdout}{result.stderr}"
                )


def shard_step_conditions(job: str) -> dict[str, str]:
    """Map each named step of a job block to its `if:` condition, or "".

    Read from the rendered active source rather than by regex over the raw file,
    because a step's condition is decided by which step it sits under and a
    line-oriented search cannot see that. A step that carries no condition maps
    to the empty string, which is what makes "runs on both legs" an assertion
    rather than the absence of one.
    """

    conditions: dict[str, str] = {}
    name = None
    for line in classifier_active_job_source(job).splitlines():
        indent = len(line) - len(line.lstrip())
        if line.startswith("    - "):
            name = None
            body = line[len("    - ") :]
            if body.startswith("name: "):
                name = body[len("name: ") :].strip()
                conditions[name] = ""
        elif name is not None and indent == 6 and line.lstrip().startswith("if: "):
            conditions[name] = line.lstrip()[len("if: ") :].strip()
        elif indent <= 4 and not line.startswith("    - "):
            name = None
    return conditions


def assert_ubuntu_shard_authority(workflow: str) -> None:
    """Pin the sharded ubuntu producer of `Check & Test (ubuntu-latest)`.

    The macOS docstring above states why sharding a required context needs its
    own authority, and every word of it applies here. What is different is that
    the ubuntu lint job is also where every source-reading gate in this
    repository runs, so the split can lose a gate as well as part of a suite,
    and the two failures look identical from outside: a faster green run.

    So the gate list is pinned by NAME rather than by count. A count is crossed
    by any twenty of twenty-one, and the one that goes missing is the one nobody
    named. Each gate must run in the lint job and on no shard, and each step a
    partition consumes must be on every shard with no shard condition, because a
    partition that ran without its build or its nextest install is not a
    partition of the suite.
    """

    blocks = workflow_job_blocks(workflow)
    shards = blocks.get("check")
    lint = blocks.get("check-lint")
    aggregate = blocks.get("check-aggregate")
    if shards is None or lint is None or aggregate is None:
        raise AssertionError(
            "ubuntu shard authority requires the shard job, its lint job and its aggregate"
        )
    if real_check_job_authority_source(aggregate) != UBUNTU_SHARD_AGGREGATE_AUTHORITY:
        raise AssertionError(
            "ubuntu shard authority requires the exact reviewed aggregate admission "
            "and matrix contract"
        )
    if real_check_job_authority_source(lint) != UBUNTU_LINT_AUTHORITY:
        raise AssertionError(
            "ubuntu shard authority requires the exact reviewed lint job admission, "
            "runner and environment"
        )

    active_shards = classifier_active_job_source(shards)
    for policy in (
        UBUNTU_SHARD_RUNNER,
        UBUNTU_SHARD_INDEPENDENT_LEGS,
        UBUNTU_SHARD_MATRIX,
        UBUNTU_SHARD_PARTITION,
    ):
        require(active_shards, policy, "ubuntu shard authority")
    assert_partition_join(shards, "ubuntu shard authority")
    require(classifier_active_job_source(lint), UBUNTU_SHARD_DOCTESTS, "ubuntu shard authority")
    assert_lint_and_shard_split(
        lint, shards, UBUNTU_LINT_GATES, UBUNTU_SHARD_BOTH_LEGS_STEPS, "ubuntu shard authority"
    )
    active_aggregate = classifier_active_job_source(aggregate)
    for gate in (UBUNTU_SHARD_SUCCESS_GATE, LINT_SUCCESS_GATE):
        require(active_aggregate, gate, "ubuntu shard authority")


def cached_job_cache_inputs(workflow: str) -> dict[str, dict[str, str]]:
    """Map every job running `rust-cache` to that step's `with:` inputs."""

    inputs: dict[str, dict[str, str]] = {}
    for job, block in workflow_job_blocks(workflow).items():
        lines = classifier_active_job_source(block).splitlines()
        for index, line in enumerate(lines):
            if "uses: Swatinem/rust-cache@" not in line:
                continue
            found: dict[str, str] = {}
            for follow in lines[index + 1 :]:
                indent = len(follow) - len(follow.lstrip())
                stripped = follow.strip()
                if stripped == "with:":
                    continue
                # A new step, or a shallower key, ends this step's inputs.
                if stripped.startswith("- ") or indent <= 4:
                    break
                if ": " in stripped:
                    key, value = stripped.split(": ", 1)
                    found[key.strip()] = value.strip()
            inputs[job] = found
            break
    return inputs


def job_cargo_environment(block: str) -> dict[str, str]:
    """The job-level `env:` mapping, which rust-cache would otherwise hash."""

    lines = classifier_active_job_source(block).splitlines()
    try:
        start = lines.index("  env:")
    except ValueError:
        return {}
    environment: dict[str, str] = {}
    for line in lines[start + 1 :]:
        indent = len(line) - len(line.lstrip())
        if indent <= 2:
            break
        if ": " in line:
            key, value = line.strip().split(": ", 1)
            environment[key.strip()] = value.strip()
    return environment


def assert_shared_cache_key_jobs_declare_one_environment(workflow: str) -> None:
    """Keep the jobs that SHARE a cargo cache key on one declared environment.

    The shared cache key turned `add-rust-environment-hash-key` off, because the hash it
    computes includes every toolchain the runner image happens to carry, so two
    jobs in one run drew different keys off the same prefix and could not share
    an entry. That fix is only safe because of what the hash's other half
    covered.

    The hash covers the toolchain versions AND every CARGO, CC, CFLAGS, CXX,
    CMAKE and RUST environment variable. Toolchain sensitivity survives the
    change, because the pin lives in `rust-toolchain.toml` and the toolchain
    action, both of which are in the key's lockfiles half. Environment
    sensitivity does not survive it.

    That loss is harmless for a job keyed on its own job id, which shares with
    nothing. It is not harmless where a `shared-key` deliberately points one
    job at another's entry: those jobs now agree on a key while declaring
    whatever environment they like, and a divergence would be a silent cache
    collision rather than a failure. Nothing else in this repository would
    notice, which is why it is asserted here.

    The check is the JOIN rather than the endpoints: it reads which jobs
    actually share a key out of the workflow and requires each such group to
    declare one environment, so a new sharer is covered the day it is added
    rather than the day someone remembers to list it.
    """

    inputs = cached_job_cache_inputs(workflow)
    if not inputs:
        raise AssertionError(
            "shared cache-key authority found no job running rust-cache, so it "
            "graded nothing; the extraction is broken or the action was renamed"
        )
    for job, found in inputs.items():
        if found.get("add-rust-environment-hash-key") != "false":
            raise AssertionError(
                "shared cache-key authority requires every rust-cache step to set "
                f"add-rust-environment-hash-key: false; {job} does not"
            )

    blocks = workflow_job_blocks(workflow)
    # Which jobs share which key, read from the workflow rather than listed.
    # A `shared-key` naming another job's id joins that job's group; an
    # expression naming several joins all of them.
    groups: dict[str, set[str]] = {}
    for job, found in inputs.items():
        shared = found.get("shared-key")
        if shared is None:
            continue
        for other in inputs:
            if f"'{other}'" in shared or shared == other:
                groups.setdefault(other, {other}).add(job)
    if not groups:
        raise AssertionError(
            "shared cache-key authority found no shared-key at all, so it graded "
            "nothing; if sharing was removed on purpose, remove this check with it"
        )

    for owner, members in sorted(groups.items()):
        environments = {job: job_cargo_environment(blocks[job]) for job in sorted(members)}
        distinct = {tuple(sorted(env.items())) for env in environments.values()}
        if len(distinct) != 1:
            raise AssertionError(
                "shared cache-key authority requires every job sharing the "
                f"{owner!r} cargo cache key to declare one environment, because "
                "add-rust-environment-hash-key is off and nothing else would "
                f"notice a divergence: {environments}"
            )


def execute_docs_only_classifier(
    classifier: str,
    *,
    cwd: Path = ROOT,
    environment_overrides: dict[str, str] | None = None,
    privileged: bool = True,
    shell_wrapper: str | None = None,
) -> tuple[subprocess.CompletedProcess[str], list[str]]:
    """Execute a classifier mutant on a push and return its emitted outputs."""

    source = classifier_shell_source(classifier)
    with tempfile.TemporaryDirectory() as directory:
        output = Path(directory) / "github-output"
        source_path = Path(directory) / "classifier.sh"
        source_path.write_text(source, encoding="utf-8")
        environment = os.environ.copy()
        environment.update(
            {
                "BASH_ENV": "",
                "EVENT_NAME": "push",
                "BASE_SHA": "unused-on-push",
                "HEAD_SHA": "unused-on-push",
                "WORKSPACE": str(cwd),
                "GITHUB_OUTPUT": str(output),
            }
        )
        if environment_overrides is not None:
            environment.update(environment_overrides)
        command = [
            "bash",
            "--noprofile",
            "--norc",
            *(["-p"] if privileged else []),
            "-e",
            "-u",
            "-o",
            "pipefail",
            str(source_path),
        ]
        if shell_wrapper is not None:
            command = [
                "bash",
                "--noprofile",
                "--norc",
                "-c",
                shell_wrapper,
                "classifier-wrapper",
                str(source_path),
            ]
        result = subprocess.run(
            command,
            cwd=cwd,
            env=environment,
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        outputs = (
            output.read_text(encoding="utf-8").splitlines() if output.exists() else []
        )
    return result, outputs


def assert_classifier_bypass_rejected(
    label: str,
    workflow: str,
    *,
    execute: bool = True,
) -> None:
    """Prove a shell-equivalent bypass is rejected and would otherwise win."""

    expect_assertion(
        label,
        "closed-form",
        lambda: assert_docs_only_classifier_guard(workflow),
    )
    if not execute:
        return

    classifier = workflow_job_blocks(workflow)["changes"]
    result, outputs = execute_docs_only_classifier(classifier)
    assert_classifier_execution_won(label, result, outputs)


def assert_classifier_execution_won(
    label: str,
    result: subprocess.CompletedProcess[str],
    outputs: list[str],
    *,
    changed_path: str | None = None,
) -> None:
    """Prove a rejected authority mutation would report a docs-only push."""

    if result.returncode != 0:
        raise AssertionError(
            f"classifier falsification did not execute: {label}: "
            f"{result.stdout}{result.stderr}"
        )
    if not outputs or outputs[-1] != "docs_only=true":
        raise AssertionError(
            f"classifier falsification did not override push output: {label}: {outputs}"
        )
    if changed_path is not None and changed_path not in result.stdout.splitlines():
        raise AssertionError(
            f"classifier falsification did not inspect {changed_path}: "
            f"{label}: {result.stdout}"
        )


def assert_classifier_execution_failed_closed(
    label: str,
    result: subprocess.CompletedProcess[str],
    outputs: list[str],
) -> None:
    """Prove hostile inherited shell state leaves a push classified as code."""

    if result.returncode != 0:
        raise AssertionError(
            f"classifier fail-closed proof did not execute: {label}: "
            f"{result.stdout}{result.stderr}"
        )
    if outputs != ["docs_only=false"]:
        raise AssertionError(f"classifier did not fail closed: {label}: {outputs}")


def assert_classifier_execution_code_bearing(
    label: str,
    result: subprocess.CompletedProcess[str],
    outputs: list[str],
    *,
    changed_paths: tuple[str, ...],
) -> None:
    """Prove the real classifier rejects and reports a code-bearing diff."""

    if result.returncode != 0:
        raise AssertionError(
            f"classifier code-bearing proof did not execute: {label}: "
            f"{result.stdout}{result.stderr}"
        )
    if outputs != ["docs_only=false"]:
        raise AssertionError(
            f"classifier admitted a code-bearing diff: {label}: {outputs}"
        )
    reported = set(result.stdout.splitlines())
    missing = [path for path in changed_paths if path not in reported]
    if missing:
        raise AssertionError(
            f"classifier omitted code-bearing paths: {label}: {missing}: "
            f"{result.stdout}"
        )


def bash_supports_nameref() -> bool:
    """Return whether the local bash can execute the hosted nameref bypass."""

    probe = subprocess.run(
        [
            "bash",
            "--noprofile",
            "--norc",
            "-c",
            'target=original; original=false; declare -n ref="$target"; '
            'ref=true; [ "$original" = true ]',
        ],
        capture_output=True,
        text=True,
        timeout=10,
        check=False,
    )
    return probe.returncode == 0


def git_fixture_environment(directory: Path) -> dict[str, str]:
    """Return a process and Git-authority-clean fixture environment."""

    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("GIT_")
        and key not in {"LD_AUDIT", "LD_LIBRARY_PATH", "LD_PRELOAD"}
    }
    global_config = directory / "git-global-config"
    global_config.write_text("", encoding="utf-8")
    environment.update(
        {
            "PATH": "/usr/bin:/bin",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_SYSTEM": "/dev/null",
            "GIT_CONFIG_GLOBAL": str(global_config),
            "GIT_CONFIG_COUNT": "0",
            "GIT_ATTR_NOSYSTEM": "1",
        }
    )
    return environment


def run_fixture_git(
    repository: Path,
    environment: dict[str, str],
    *args: str,
) -> str:
    """Run the reviewed hosted-runner Git path for a hermetic fixture."""

    result = subprocess.run(
        [
            "/usr/bin/git",
            "-c",
            "maintenance.auto=false",
            "-c",
            "gc.auto=0",
            *args,
        ],
        cwd=repository,
        env=environment,
        capture_output=True,
        text=True,
        timeout=10,
        check=False,
    )
    if result.returncode != 0:
        raise AssertionError(
            f"classifier Git fixture failed: git {args}: {result.stdout}{result.stderr}"
        )
    return result.stdout.strip()


def create_docs_only_git_fixture(directory: Path) -> tuple[Path, str, str]:
    """Create a hermetic two-commit repository with one docs-only change."""

    repository = directory / "repository"
    repository.mkdir()
    environment = git_fixture_environment(directory)

    def git(*args: str) -> str:
        return run_fixture_git(repository, environment, *args)

    git("init", "--initial-branch=main")
    git("config", "user.email", "kin@example.invalid")
    git("config", "user.name", "Kin")
    (repository / "README.md").write_text("base\n", encoding="utf-8")
    git("add", "--all")
    git("commit", "-m", "seed")
    base_sha = git("rev-parse", "HEAD")

    docs = repository / "docs"
    docs.mkdir()
    (docs / "release-bot.md").write_text("docs-only\n", encoding="utf-8")
    git("add", "--all")
    git("commit", "-m", "docs only")
    head_sha = git("rev-parse", "HEAD")
    return repository, base_sha, head_sha


def assert_branch_name_never_changes_the_classification(workflow: str) -> None:
    """A docs-only diff classifies the same from an export branch as from any.

    The merge queue grades the commit that lands, export or not, so the branch
    name has nothing left to decide here. Branch names are attacker-chosen
    text, and a classifier that read one could be steered by it.
    """

    classifier = workflow_job_blocks(workflow)["changes"]
    with tempfile.TemporaryDirectory() as directory:
        fixture, base_sha, head_sha = create_docs_only_git_fixture(Path(directory))
        for head_ref, expected in (
            ("feature/public-export-kin-0123abc", "docs_only=true"),
            ("docs/ordinary-branch", "docs_only=true"),
        ):
            result, outputs = execute_docs_only_classifier(
                classifier,
                cwd=fixture,
                environment_overrides={
                    "EVENT_NAME": "pull_request",
                    "BASE_SHA": base_sha,
                    "HEAD_SHA": head_sha,
                    "HEAD_REF": head_ref,
                    "WORKSPACE": str(fixture),
                },
            )
            if result.returncode != 0 or not outputs or outputs[-1] != expected:
                raise AssertionError(
                    f"a docs-only pull request from {head_ref} must report "
                    f"{expected}: {outputs} {result.stdout}{result.stderr}"
                )


def create_classifier_authority_attack_fixture(
    directory: Path,
    base_workflow: str,
    hostile_workflow: str,
) -> tuple[Path, str, str, dict[str, str], tuple[Path, ...]]:
    """Create a code diff plus executable, Git-env, and replace-object attacks."""

    repository = directory / "authority-repository"
    repository.mkdir()
    environment = git_fixture_environment(directory)

    def git(*args: str) -> str:
        return run_fixture_git(repository, environment, *args)

    git("init", "--initial-branch=main")
    git("config", "user.email", "kin@example.invalid")
    git("config", "user.name", "Kin")
    (repository / "README.md").write_text("base\n", encoding="utf-8")
    source = repository / "src"
    source.mkdir()
    (source / "lib.rs").write_text("pub fn authority() -> bool { true }\n")
    workflows = repository / ".github" / "workflows"
    workflows.mkdir(parents=True)
    (workflows / "ci.yml").write_text(base_workflow, encoding="utf-8")
    git("add", "--all")
    git("commit", "-m", "seed")
    base_sha = git("rev-parse", "HEAD")

    docs = repository / "docs"
    docs.mkdir()
    (docs / "release-bot.md").write_text("docs-only\n", encoding="utf-8")
    git("add", "--all")
    git("commit", "-m", "docs only")
    docs_sha = git("rev-parse", "HEAD")

    (source / "lib.rs").write_text("pub fn authority() -> bool { false }\n")
    (workflows / "ci.yml").write_text(hostile_workflow, encoding="utf-8")
    (repository / ".gitattributes").write_text(
        "src/lib.rs diff=hostile\n",
        encoding="utf-8",
    )
    attack = repository / "attack"
    attack.mkdir()
    path_git = attack / "git"
    path_git.write_text(
        "#!/bin/sh\nprintf '%s\\n' docs/decoy.md\n",
        encoding="utf-8",
    )
    path_git.chmod(0o755)
    external_marker = directory / "external-diff-ran"
    external_diff = attack / "external-diff"
    external_diff.write_text(
        f"#!/bin/sh\n: > {external_marker!s}\nexit 0\n",
        encoding="utf-8",
    )
    external_diff.chmod(0o755)
    textconv_marker = directory / "textconv-ran"
    textconv = attack / "textconv"
    textconv.write_text(
        f"#!/bin/sh\n: > {textconv_marker!s}\nprintf decoy\n",
        encoding="utf-8",
    )
    textconv.chmod(0o755)
    hostile_config = attack / "gitconfig"
    hostile_config.write_text(
        "[diff]\n"
        f"\texternal = {external_diff!s}\n"
        "\trenames = true\n"
        '[diff "hostile"]\n'
        f"\ttextconv = {textconv!s}\n",
        encoding="utf-8",
    )
    git("add", "--all")
    git("commit", "-m", "code and workflow attack")
    head_sha = git("rev-parse", "HEAD")
    git("replace", head_sha, docs_sha)

    hostile_environment = {
        "PATH": f"{attack!s}:/usr/local/bin:/usr/bin:/bin",
        "HOME": str(attack),
        "XDG_CONFIG_HOME": str(attack),
        "LD_LIBRARY_PATH": str(attack),
        "GIT_DIR": str(attack / "decoy.git"),
        "GIT_WORK_TREE": str(attack),
        "GIT_INDEX_FILE": str(attack / "index"),
        "GIT_COMMON_DIR": str(attack / "common"),
        "GIT_OBJECT_DIRECTORY": str(attack / "objects"),
        "GIT_ALTERNATE_OBJECT_DIRECTORIES": str(attack / "alternate-objects"),
        "GIT_EXEC_PATH": str(attack),
        "GIT_CONFIG": str(hostile_config),
        "GIT_CONFIG_NOSYSTEM": "0",
        "GIT_CONFIG_SYSTEM": str(hostile_config),
        "GIT_CONFIG_GLOBAL": str(hostile_config),
        "GIT_CONFIG_COUNT": "2",
        "GIT_CONFIG_KEY_0": "diff.external",
        "GIT_CONFIG_VALUE_0": str(external_diff),
        "GIT_CONFIG_KEY_1": "diff.hostile.textconv",
        "GIT_CONFIG_VALUE_1": str(textconv),
        "GIT_CONFIG_PARAMETERS": "'diff.renames'='true'",
        "GIT_ATTR_NOSYSTEM": "0",
        "GIT_EXTERNAL_DIFF": str(external_diff),
        "GIT_DIFF_OPTS": "--unified=0",
        "GIT_REPLACE_REF_BASE": "refs/replace",
    }
    return (
        repository,
        base_sha,
        head_sha,
        hostile_environment,
        (external_marker, textconv_marker),
    )


RELEASE_PR_HEAD_REF = "automation/release-next"


def assert_release_pr_ci_scope_cannot_widen(ci: str) -> None:
    """A release-PR CI carve-out must be unable to reach the queue or main.

    The release pull request is the one whose whole content is the version
    bump, and the merge queue is what proves it. A carve-out scoped loosely
    enough to match `merge_group` would merge that pull request with no full
    pass anywhere. One that matched `push` would leave a skipped required
    context on the release sha, which the mint reads as not green and refuses
    permanently. Neither failure announces itself as a scoping mistake: the
    first is invisible until something ships broken, and the second looks like
    a broken mint rather than a broken condition.

    `github.head_ref` is what makes the scoping structural rather than
    conventional: it is set only on pull-request events and is empty under
    both `push` and `merge_group`, so a condition written against it cannot
    match either however the rest of the expression is edited. This assertion
    passes vacuously today, because ci.yml carries no such carve-out yet, and
    starts constraining the moment one lands.
    """

    for job, block in workflow_job_blocks(ci).items():
        active = "\n".join(active_lines(block))
        if RELEASE_PR_HEAD_REF not in active:
            continue
        if "github.head_ref" not in active:
            raise AssertionError(
                f"ci.yml job {job} scopes the release pull request by something "
                "other than github.head_ref, which is the only selector that is "
                "empty under push and merge_group"
            )
        if "github.event_name == 'pull_request'" not in active:
            raise AssertionError(
                f"ci.yml job {job} carves out the release pull request without "
                "pinning the pull_request event, so the carve-out's reach "
                "depends on expression evaluation rather than on the event"
            )
        for reaching in ("github.ref ", "github.ref=", "github.ref==", "github.base_ref"):
            if reaching in active:
                raise AssertionError(
                    f"ci.yml job {job} scopes the release pull request with a "
                    f"context that is populated off pull requests: {reaching.strip()}"
                )



def check_windows_legs_and_release_target_compile_guard(
    ci_workflow: str, release: str
) -> None:
    ci_jobs = workflow_job_blocks(ci_workflow)
    windows_npm_proof = WINDOWS_NPM_PROOF.read_text(encoding="utf-8")
    assert_windows_npm_first_run_proof(
        ci_jobs["windows-authority-tests"], windows_npm_proof
    )
    for label, replacement in (
        ("the Windows search proof accepts another entity", "holdsEntity(replies, 'other', 'crate')"),
        ("the Windows search proof accepts another module", "holdsEntity(replies, 'greet', 'other')"),
    ):
        expect_assertion(
            label,
            "native Windows npm first-run harness",
            lambda replacement=replacement: assert_windows_npm_first_run_proof(
                ci_jobs["windows-authority-tests"],
                windows_npm_proof.replace(
                    "holdsEntity(replies, 'greet', 'crate')", replacement, 1
                ),
            ),
        )
    assert_windows_daemon_sibling_build(ci_jobs["windows-authority-runtime-tests"])
    windows_job = ci_jobs["windows-authority-runtime-tests"]
    sibling_build_start = windows_job.index(WINDOWS_DAEMON_SIBLING_BUILD)
    sibling_build_end = windows_job.index(
        "      - name: Compile and run native Windows runtime authority tests"
    )
    sibling_build_block = windows_job[sibling_build_start:sibling_build_end]
    for label, mutated_job, expected in (
        (
            "the Windows leg stops building the daemon its lifecycle test drives",
            windows_job.replace(sibling_build_block, "", 1),
            "native Windows daemon lifecycle prerequisite",
        ),
        (
            "the sibling daemon build drifts back after the tests that read it",
            windows_job.replace(sibling_build_block, "", 1) + sibling_build_block,
            "before the lifecycle test reads it",
        ),
    ):
        expect_assertion(
            label,
            expected,
            lambda mutated_job=mutated_job: assert_windows_daemon_sibling_build(
                mutated_job
            ),
        )
    canonical_npm_provision = CANONICAL_NPM_PROVISION.read_text(encoding="utf-8")
    canonical_npm_provision_test = CANONICAL_NPM_PROVISION_TEST.read_text(
        encoding="utf-8"
    )
    compat_npm_provision = COMPAT_NPM_PROVISION.read_text(encoding="utf-8")
    compat_npm_provision_test = COMPAT_NPM_PROVISION_TEST.read_text(
        encoding="utf-8"
    )
    assert_windows_npm_archive_authority(
        canonical_npm_provision,
        canonical_npm_provision_test,
        compat_npm_provision,
        compat_npm_provision_test,
    )
    for label, canonical_source, canonical_test, compat_source, compat_test, expected in (
        (
            "canonical npm Windows extraction falls back to PATH tar",
            canonical_npm_provision.replace(
                "path.win32.join(systemRoot, 'System32', 'tar.exe')", "'tar'", 1
            ),
            canonical_npm_provision_test,
            compat_npm_provision,
            compat_npm_provision_test,
            "absolute System32 extraction authority",
        ),
        (
            "compatibility npm Windows extraction falls back to PATH tar",
            canonical_npm_provision,
            canonical_npm_provision_test,
            compat_npm_provision.replace(
                "path.win32.join(systemRoot, 'System32', 'tar.exe')", "'tar'", 1
            ),
            compat_npm_provision_test,
            "absolute System32 extraction authority",
        ),
        (
            "canonical npm Windows fixture stops proving real ZIP bytes",
            canonical_npm_provision,
            canonical_npm_provision_test.replace("'504b0304'", "'00000000'", 1),
            compat_npm_provision,
            compat_npm_provision_test,
            "genuine Windows ZIP regression",
        ),
        (
            "compatibility npm Windows fixture stops proving real ZIP bytes",
            canonical_npm_provision,
            canonical_npm_provision_test,
            compat_npm_provision,
            compat_npm_provision_test.replace("'504b0304'", "'00000000'", 1),
            "genuine Windows ZIP regression",
        ),
        (
            "canonical npm ZIP proof loses its hostile PATH",
            canonical_npm_provision,
            canonical_npm_provision_test.replace(
                "env.PATH = [hostileBin, originalPath]",
                "env.PATH = originalPath",
                1,
            ),
            compat_npm_provision,
            compat_npm_provision_test,
            "genuine Windows ZIP regression",
        ),
        (
            "compatibility npm ZIP proof loses its hostile PATH",
            canonical_npm_provision,
            canonical_npm_provision_test,
            compat_npm_provision,
            compat_npm_provision_test.replace(
                "env.PATH = [hostileBin, originalPath]",
                "env.PATH = originalPath",
                1,
            ),
            "genuine Windows ZIP regression",
        ),
    ):
        expect_assertion(
            label,
            expected,
            lambda canonical_source=canonical_source,
            canonical_test=canonical_test,
            compat_source=compat_source,
            compat_test=compat_test: assert_windows_npm_archive_authority(
                canonical_source,
                canonical_test,
                compat_source,
                compat_test,
            ),
        )
    for label, mutated_job, mutated_proof, expected in (
        (
            "canonical npm tests disappear from native Windows",
            ci_jobs["windows-authority-tests"].replace(
                "          npm test --prefix ./packages/kin\n", "", 1
            ),
            windows_npm_proof,
            "native Windows npm first-run CI proof",
        ),
        (
            "MCP sibling daemon discovery is compiled but never executed on Windows",
            ci_jobs["windows-authority-tests"].replace(
                "          run_required_exact \"MCP Windows sibling daemon discovery\" \\\n"
                "            \"daemon_delegate::tests::windows_daemon_discovery_finds_platform_sibling_without_path\" \\\n"
                "            --locked --target x86_64-pc-windows-msvc \\\n"
                "            -p kin-mcp --no-default-features --lib\n",
                "",
                1,
            ),
            windows_npm_proof,
            "native Windows npm first-run CI proof",
        ),
        (
            "canonical runtime regains a KIN_DAEMON_BIN rescue",
            ci_jobs["windows-authority-tests"],
            windows_npm_proof.replace(
                "  deleteEnv(env, 'KIN_DAEMON_BIN');\n"
                "  assert.equal(readEnv(env, 'KIN_DAEMON_BIN'), undefined);\n"
                "  assertPathExcludes(env, path.dirname(managedKin), '@kinlab/kin');\n",
                "  setEnv(env, 'KIN_DAEMON_BIN', managedDaemon);\n"
                "  assertPathExcludes(env, path.dirname(managedKin), '@kinlab/kin');\n",
                1,
            ),
            "must not inject KIN_DAEMON_BIN",
        ),
        (
            "canonical runtime stops proving daemon-backed status",
            ci_jobs["windows-authority-tests"],
            windows_npm_proof.replace(
                "[canonicalKinLauncher, 'status', '--json']",
                "[canonicalKinLauncher, '--version']",
                1,
            ),
            "native Windows npm first-run harness",
        ),
        (
            "compatibility wrapper is inferred from the canonical MCP entrypoint",
            ci_jobs["windows-authority-tests"],
            windows_npm_proof.replace(
                "launcher: compatibilityMcpLauncher",
                "launcher: canonicalMcpLauncher",
                1,
            ),
            "native Windows npm first-run harness",
        ),
    ):
        expect_assertion(
            label,
            expected,
            lambda mutated_job=mutated_job, mutated_proof=mutated_proof: (
                assert_windows_npm_first_run_proof(mutated_job, mutated_proof)
            ),
        )
    for job_id in ("windows-authority-tests", "windows-installer"):
        for policy in (
            "- name: Assert the Windows admission contract",
            "bash ./scripts/assert-windows-init-contract.sh",
        ):
            require(ci_jobs[job_id], policy, f"shared Windows admission proof in {job_id}")
        if ci_jobs[job_id].count("run: ./scripts/test-install-checksum.ps1") != 2:
            raise AssertionError(
                f"{job_id} must execute the installer warning/checksum contract "
                "once under PowerShell 7 and once under Windows PowerShell 5.1"
            )
        for shell in ("shell: pwsh", "shell: powershell"):
            require(
                ci_jobs[job_id],
                shell,
                f"dual-engine Windows installer authority in {job_id}",
            )
    # The Windows arm of the config writer shares no code with the Unix one, so
    # a leg that never runs its cases proves nothing about the transaction
    # `kin init` depends on.
    require(
        ci_jobs["windows-authority-tests"],
        "config::capability_owned_config_replacement_tests",
        "native Windows config replacement proof",
    )
    # The retained directory handle's exclusion is a Windows sharing-rule
    # property. The module doc claims it, so the leg has to keep proving it, or
    # the claim reverts to an assertion nobody checks.
    require(
        ci_jobs["windows-authority-tests"],
        "config::windows::capability_exclusion_tests",
        "native retained-capability exclusion proof",
    )
    # `kin init` is what the admission script drives, and it lives two steps
    # later behind a build. When it refuses, the script reports one line naming
    # a path that both the config writer and the metadata seal report against,
    # so the end-to-end leg cannot say which component refused. Running
    # admission in process here names a test instead. This is required rather
    # than merely present because `init.rs` carried no native Windows coverage
    # at all until it was added, which is how a refusal in it stayed unlocated
    # across repeated attempts.
    require(
        ci_jobs["windows-authority-tests"],
        "init::tests",
        "native repository initialization proof",
    )
    require(
        ci_jobs["windows-authority-tests"],
        "target/x86_64-pc-windows-msvc/debug/kin.exe",
        "pull-request Windows admission proof",
    )
    for policy in (
        "- name: Test both public npm surfaces on native Windows",
        "npm test --prefix ./packages/kin-mcp",
        "npm run lint --prefix ./packages/kin-mcp",
    ):
        require(
            ci_jobs["windows-authority-tests"],
            policy,
            "native Windows npm MCP provisioning proof",
        )
    require(
        ci_jobs["windows-installer"],
        "target/x86_64-pc-windows-msvc/release/kin.exe",
        "landing-push Windows admission proof",
    )
    assert_windows_authority_split(ci_jobs)
    for label, job_id, mutated_block, expected in (
        (
            "a native Windows leg is dropped in the split",
            "windows-authority-cli-tests",
            ci_jobs["windows-authority-cli-tests"].replace(
                '"native managed-daemon ownership scan"', '"renamed leg"', 1
            ),
            "exactly once",
        ),
        (
            "two Windows jobs both claim the same leg",
            "windows-authority-runtime-tests",
            ci_jobs["windows-authority-runtime-tests"].replace(
                '"daemon isolation support"', '"kin-git library"', 1
            ),
            "exactly once",
        ),
        (
            "a Windows authority job is taken off the merge queue",
            "windows-authority-runtime-tests",
            ci_jobs["windows-authority-runtime-tests"].replace(
                "    runs-on: windows-latest",
                "    if: github.event_name != 'merge_group'\n    runs-on: windows-latest",
                1,
            ),
            "must stay on the merge queue",
        ),
        (
            "a Windows authority job takes an unreviewed job-level condition",
            "windows-authority-runtime-tests",
            ci_jobs["windows-authority-runtime-tests"].replace(
                f"    if: {WINDOWS_AUTHORITY_ADMITTED_IF}\n", "    if: ${{ false }}\n", 1
            ),
            "The only reviewed condition is",
        ),
        (
            "the reviewed condition is widened to exclude the push as well",
            "windows-authority-runtime-tests",
            ci_jobs["windows-authority-runtime-tests"].replace(
                f"    if: {WINDOWS_AUTHORITY_ADMITTED_IF}\n",
                "    if: ${{ github.event_name != 'pull_request'"
                " && github.event_name != 'push' }}\n",
                1,
            ),
            "The only reviewed condition is",
        ),
        (
            "a Windows authority job stops sourcing the shared leg helpers",
            "windows-authority-cli-tests",
            ci_jobs["windows-authority-cli-tests"].replace(
                f"          {WINDOWS_AUTHORITY_LEG_HELPERS}\n", "", 1
            ),
            "shared leg helpers",
        ),
        (
            "the step budgets grow past the job cap they must fire before",
            "windows-authority-cli-tests",
            ci_jobs["windows-authority-cli-tests"].replace(
                "    timeout-minutes: 45", "    timeout-minutes: 30", 1
            ),
            "failing loudly",
        ),
        (
            "a Windows authority job budgets no step at all",
            "windows-authority-cli-tests",
            re.sub(
                r"(?m)^        timeout-minutes: \d+\n",
                "",
                ci_jobs["windows-authority-cli-tests"],
            ),
            "must budget its long steps",
        ),
    ):
        expect_assertion(
            label,
            expected,
            lambda job_id=job_id, mutated_block=mutated_block: (
                assert_windows_authority_split({**ci_jobs, job_id: mutated_block})
            ),
        )

    # The Linux release artifacts are the only musl compilation Kin performs,
    # and until this guard existed no required context compiled for that target.
    # A target_env cfg is invisible to a glibc build, so a dependency resolving
    # a glibc-only libc entry point passed every check, reached a tag, and then
    # failed the release run, where the only remedy is another version cut. Pin
    # the guard to the release matrix so it cannot drift off the target whose
    # artifacts it protects, and pin its package set so it cannot shrink below
    # what the release builds. It lives in the required Check & Test job on
    # purpose: a guard in a context no ruleset requires cannot refuse a merge.
    release_musl_targets = {
        match.group("target")
        for match in re.finditer(
            r"(?m)^\s+target: (?P<target>\S+-linux-musl)$",
            workflow_job_blocks(release)["build"],
        )
    }
    if not release_musl_targets:
        raise AssertionError(
            "the release build matrix must name at least one musl target for "
            "the pull-request release-target compile guard to protect"
        )
    for policy in (
        "- name: Check the Linux release target (musl)",
        "musl-tools",
        "-p kin-cli -p kin-daemon",
    ):
        require(ci_jobs["check-lint"], policy, "pull-request Linux release-target compile guard")
    # Every musl target the release workflow ships must compile before a merge,
    # not first inside the tagged release run. Deriving the requirement from the
    # release matrix rather than naming one target keeps the two from drifting
    # apart in the direction that costs a version cut: a release target that no
    # required context compiles. The aarch64 leg needs a C compiler for the same
    # dependency build scripts, and Ubuntu carries no aarch64 musl one, so it
    # compiles those C sources with the aarch64 glibc cross toolchain and proves
    # the Rust half exactly; the native ubuntu-24.04-arm release leg is what
    # proves the C half against musl.
    for target in sorted(release_musl_targets):
        for policy in (
            f"rustup target add {target}",
            f"cargo check --locked --target {target}",
        ):
            require(
                ci_jobs["check-lint"],
                policy,
                f"pull-request compile guard coverage of release musl target {target}",
            )
    if "aarch64-unknown-linux-musl" in release_musl_targets:
        require(
            ci_jobs["check-lint"],
            "gcc-aarch64-linux-gnu",
            "aarch64 release-target compile guard C toolchain",
        )
    for package in ("-p kin-cli", "-p kin-daemon"):
        require(
            workflow_job_blocks(release)["build"],
            package,
            "release core binary package set the compile guard mirrors",
        )

    # P3-4's content landed without a guard, so a deliberate deletion of the
    # index refresh was invisible to the suite. The refresh now lives in the
    # bounded apt helper every apt site in the check job calls, so the guard
    # follows it there: the aarch64 leg must call the helper for its cross
    # toolchain, and the helper must still refresh the index before installing,
    # under a timeout, or a stalled mirror holds the job to its one-hour bound
    # and the merge queue ejects the entry without marking the pull request.
    require(
        ci_jobs["check-lint"],
        "scripts/ci-apt-install.sh gcc-aarch64-linux-gnu",
        "aarch64 release-target compile guard apt install",
    )
    ci_apt_install = CI_APT_INSTALL.read_text(encoding="utf-8")
    for policy in (
        'timeout "$UPDATE_BOUND" sudo apt-get "${APT_OPTS[@]}" update',
        'timeout "$INSTALL_BOUND" sudo apt-get "${APT_OPTS[@]}" install --yes "$@"',
        "-o Acquire::http::Timeout=30",
        "-o Acquire::Retries=3",
    ):
        require(
            ci_apt_install,
            policy,
            "bounded apt helper index refresh and install",
        )
    for job_id in ("check", "check-lint"):
        if "sudo apt-get" in ci_jobs[job_id]:
            raise AssertionError(
                f"the {job_id} job must reach apt only through scripts/ci-apt-install.sh, "
                "so every apt call stays bounded"
            )



def check_windows_installer_admission(ci_workflow: str) -> None:
    windows_start = ci_workflow.index("  windows-installer:")
    windows_end = ci_workflow.index("\n  check:", windows_start)
    windows_job = ci_workflow[windows_start:windows_end]
    ci_windows_bytes = windows_job.index(
        "      - name: Preserve tracked bytes on Windows"
    )
    ci_checkout = windows_job.index("      - uses: actions/checkout@v7")
    if ci_windows_bytes >= ci_checkout:
        raise AssertionError(
            "Windows CI must disable line-ending conversion before checkout"
        )
    for policy in (
        "git config --global core.autocrlf false",
        '["cat-file", "blob", "HEAD:Cargo.lock"]',
        "if (!workingLock.equals(trackedLock))",
        "meta.dependency_provenance !== expectedLock",
        "does not match tracked Git bytes",
    ):
        require(windows_job, policy, "Windows exact lockfile provenance regression")

    # The Windows installer leg is off the pull-request path, so the only
    # places it can prove anything are the queue's build and the landing push.
    # The only reason that is safe is that release-tag.yml refuses to mint a
    # tag unless this exact check is present and green on the release sha. Pin
    # every half together: dropping one silently turns a required release gate
    # into a job that never runs on the commit being released.
    #
    # The merge-queue half is load-bearing now rather than optional. The mint
    # keys off the merge-group-proven sha, and this leg is the one required
    # context the queue used to skip, so a build that skips it is a build the
    # mint must refuse. Restoring a merge_group exclusion here would make the
    # queue's proof incomplete for exactly the platform that has been killing
    # tags, and the mint would refuse every release until someone noticed.
    installer_start = ci_workflow.index("  windows-installer:")
    installer_end = ci_workflow.index("\n  changes:", installer_start)
    installer_job = ci_workflow[installer_start:installer_end]
    for policy in (
        "name: Windows installer + vector release build",
        "github.event_name != 'pull_request'",
        "needs.changes.outputs.docs_only != 'true'",
    ):
        require(installer_job, policy, "main-only Windows installer admission")
    if "github.event_name != 'merge_group'" in installer_job:
        raise AssertionError(
            "the Windows installer leg must run inside the merge queue: the "
            "release mint keys off the queue-proven sha and refuses a build "
            "that skipped this required context"
        )
    require(
        ci_workflow,
        "  push:\n    branches: [main]",
        "Windows installer proof still reaching every main commit",
    )
    require(
        ci_workflow,
        "  merge_group:",
        "Windows installer proof reaching the queue build the mint keys off",
    )



def check_docs_only_classifier_wiring(ci_workflow: str) -> str:
    # Main's classifier must keep reporting false so the release-critical
    # installer actually runs. The release-tag gate independently requires an
    # exact success conclusion, but preserving both controls keeps a main push
    # from silently losing the proof and discovering that only at tag time.
    # Enforce the classifier as a closed form at the whole-job boundary. The
    # reviewed shell is meaningless without its checkout, output, id, event,
    # SHA, shell, and startup-environment bindings, so those are one authority
    # contract rather than independently mutable text.
    # The span ends at the next job header rather than at a named sibling. It
    # used to name `check-docs-only`, which put this reader one job rename or
    # one inserted job away from a ValueError that reads like a missing
    # classifier rather than like a moved neighbour.
    classifier_start = ci_workflow.index("  changes:")
    next_job = re.search(
        r"(?m)^  [A-Za-z0-9_.-]+:$", ci_workflow[classifier_start + len("  changes:") :]
    )
    if next_job is None:
        raise AssertionError("the classifier job must be followed by another job")
    classifier_end = classifier_start + len("  changes:") + next_job.start()
    classifier = ci_workflow[classifier_start:classifier_end].rstrip("\n")
    assert_docs_only_classifier_guard(ci_workflow)
    assert_branch_name_never_changes_the_classification(ci_workflow)
    assert_assertion_reachability_gate_wired(ci_workflow)
    return classifier



def check_rust_toolchain_prune() -> None:
    toolchain_action = RUST_TOOLCHAIN_ACTION.read_text(encoding="utf-8")
    assert_toolchain_prune_is_wired(toolchain_action)
    assert_toolchain_prune_behavior()
    for label, original, replacement, expected in (
        (
            "the prune step is dropped, so the image decides the cache key",
            "    - name: Prune toolchains this repository never named",
            "    - name: Prune nothing at all",
            "must declare exactly one step named",
        ),
        (
            "the image set is recorded after the install, where it cannot tell them apart",
            "    - name: Record the toolchains the runner image shipped",
            "    - name: Recorded too late",
            "must declare exactly one step named",
        ),
        (
            "the prune stops calling the script the behavioral cases drive",
            'bash "$GITHUB_ACTION_PATH/prune-image-toolchains.sh" \\',
            "true \\",
            "must call prune-image-toolchains.sh",
        ),
    ):
        mutant = replace_exactly_once(toolchain_action, original, replacement, label)
        expect_assertion(
            label,
            expected,
            lambda mutant=mutant: assert_toolchain_prune_is_wired(mutant),
        )



def check_release_version_and_reachability_gates(ci_workflow: str) -> None:
    assert_release_version_gate_wired(ci_workflow)
    expect_assertion(
        "ci.yml drops the job that runs the version gate on a release PR",
        "ci.yml must run the release version gate on pull requests",
        lambda: assert_release_version_gate_wired(
            ci_workflow.replace("  release-version:\n", "  release-version-disabled:\n")
        ),
    )
    expect_assertion(
        "the version gate job survives with its command commented out",
        "missing `run: node",
        lambda: assert_release_version_gate_wired(
            ci_workflow.replace(
                f"run: node {RELEASE_VERSION_GUARD_POLICY}",
                f"run: # node {RELEASE_VERSION_GUARD_POLICY}",
            )
        ),
    )
    expect_assertion(
        "the version gate stops reading the release PR's own base commit",
        "missing `BASE_SHA",
        lambda: assert_release_version_gate_wired(
            ci_workflow.replace(
                "BASE_SHA: ${{ github.event.pull_request.base.sha }}",
                "BASE_SHA: ''",
            )
        ),
    )
    expect_assertion(
        "the version gate loses the deep fetch its base manifest needs",
        "missing `fetch-depth: 0`",
        lambda: assert_release_version_gate_wired(
            ci_workflow.replace(
                "          # The gate reads Cargo.toml out of the base commit. A shallow\n"
                "          # pull-request checkout does not carry it, and `git show` would fail\n"
                "          # in a way that reads like a broken gate rather than a missing fetch.\n"
                "          fetch-depth: 0\n",
                "",
            )
        ),
    )
    expect_assertion(
        "the version gate stops being scoped to the release branch",
        "missing `(github.head_ref",
        lambda: assert_release_version_gate_wired(
            ci_workflow.replace(
                "      (github.head_ref == 'automation/release-next' ||\n"
                "      contains(github.event.pull_request.labels.*.name, 'release:automated'))\n",
                "      true\n",
            )
        ),
    )
    expect_assertion(
        "ci.yml keeps both version guards but drops their falsification",
        "ci.yml must run",
        lambda: assert_release_version_gate_wired(
            ci_workflow.replace(
                RELEASE_VERSION_FALSIFIER_POLICY, "scripts/absent.py"
            )
        ),
    )
    expect_assertion(
        "ci.yml stops running the release-intent gate's own suite",
        "ci.yml must run",
        lambda: assert_release_version_gate_wired(
            ci_workflow.replace(RELEASE_INTENT_SUITE_POLICY, "scripts/absent.test.mjs")
        ),
    )
    expect_assertion(
        "ci.yml stops running the version gate's own suite",
        "ci.yml must run",
        lambda: assert_release_version_gate_wired(
            ci_workflow.replace(RELEASE_VERSION_SUITE_POLICY, "scripts/absent.test.mjs")
        ),
    )
    expect_assertion(
        "ci.yml drops the step that proves every assertion still runs",
        "ci.yml must run",
        lambda: assert_assertion_reachability_gate_wired(
            ci_workflow.replace(ASSERTION_REACHABILITY_POLICY, "scripts/absent.py")
        ),
    )
    expect_assertion(
        "the reachability gate survives only as a commented-out ci.yml step",
        "ci.yml must run",
        lambda: assert_assertion_reachability_gate_wired(
            ci_workflow.replace(
                f"run: python3 {ASSERTION_REACHABILITY_POLICY}",
                f"# run: python3 {ASSERTION_REACHABILITY_POLICY}",
            )
        ),
    )
    expect_assertion(
        "the reachability step keeps its path but comments out the command",
        "ci.yml must run",
        lambda: assert_assertion_reachability_gate_wired(
            ci_workflow.replace(
                f"run: python3 {ASSERTION_REACHABILITY_POLICY}",
                f"run: # python3 {ASSERTION_REACHABILITY_POLICY}",
            )
        ),
    )



# The merge queue grades the commit that lands, so a pull request, export
# pull requests included, runs only the light half of admission. These are the
# lint and policy steps that compile and so move to the queue. Each carries
# exactly the pull-request skip, because any narrower condition, one that also
# left out `merge_group`, would take clippy off the commit that lands.
PULL_REQUEST_SKIP = "github.event_name != 'pull_request'"
FAST_GATE_QUEUE_ONLY_STEPS = (
    "Cache cargo registry and build",
    "Clippy",
    "Check the filesystem boundary lint",
    "Test Firestore post-status response semantics",
    "Test hosted object-store destination composition",
)
# Jobs that grade on the queue and on main and report as skipped on a pull
# request, beside the release suites the shard authorities above already pin.
QUEUE_ONLY_JOBS = (
    "fast-gate-tests",
    "windows-cross-check",
    "mcp-surface-contract",
    "library-packages",
)
ACCEPTANCE_WORKFLOW = WORKFLOWS / "acceptance.yml"
ACCEPTANCE_SUITE_IF = "${{ github.event_name != 'pull_request' }}"
ACCEPTANCE_GATE_IF = "${{ !cancelled() && github.event_name != 'pull_request' }}"


def job_condition(job: str) -> str:
    """The job-level `if:` of a job block, folded onto one line, or ""."""

    lines = classifier_active_job_source(job).splitlines()
    for index, line in enumerate(lines):
        if line.startswith("  if: "):
            value = line[len("  if: "):].strip()
            if value in (">-", ">", "|"):
                folded = []
                for follow in lines[index + 1:]:
                    if not follow.startswith("    "):
                        break
                    folded.append(follow.strip())
                value = " ".join(folded)
            return value
    return ""


def assert_merge_queue_admission(workflow: str, acceptance: str) -> None:
    """A pull request is admitted light; the queue's commit is graded in full.

    Three properties, none of them visible from a context name. The compiling
    steps and jobs skip a pull request and nothing else, so every one of them
    runs on `merge_group` and on main. The fast-gate aggregate passes a pull
    request without its shards and still admits only success from them in the
    queue. And Acceptance skips a pull request in every job while its gate runs
    on every queue commit the run was not cancelled for, so the queue cannot
    land a commit whose Product Acceptance never ran.
    """

    if "\n  merge_group:\n" not in workflow or "\n  merge_group:\n" not in acceptance:
        raise AssertionError(
            "merge queue admission requires ci.yml and acceptance.yml to run on merge_group"
        )
    blocks = workflow_job_blocks(workflow)
    for job_id, block in blocks.items():
        if "'merge_group'" in job_condition(block):
            raise AssertionError(
                f"merge queue admission refuses a job condition naming merge_group: {job_id}; "
                "the queue's commit is the one that lands and every job grades it"
            )
    lint_conditions = shard_step_conditions(blocks["fast-gate-lint"])
    for step, condition in lint_conditions.items():
        expected = PULL_REQUEST_SKIP if step in FAST_GATE_QUEUE_ONLY_STEPS else ""
        if condition != expected:
            raise AssertionError(
                f"merge queue admission requires the lint step {step!r} to carry "
                f"{expected or 'no condition'!r}, not {condition!r}"
            )
    for step in FAST_GATE_QUEUE_ONLY_STEPS:
        if step not in lint_conditions:
            raise AssertionError(f"merge queue admission lost the lint step {step!r}")
    for job_id in QUEUE_ONLY_JOBS:
        condition = job_condition(blocks[job_id])
        if PULL_REQUEST_SKIP not in condition:
            raise AssertionError(
                f"merge queue admission requires {job_id} to skip a pull request: {condition!r}"
            )

    shell = workflow_step_shell(
        blocks["fast-gate-tests-aggregate"], "Require every fast-gate shard to have succeeded"
    )
    for label, event, docs, shards, windows, expected in (
        ("a pull request passes without its shards", "pull_request", "false", "skipped", "skipped", 0),
        ("the queue refuses skipped shards", "merge_group", "false", "skipped", "success", 1),
        ("the queue refuses a skipped cross-check", "merge_group", "false", "success", "skipped", 1),
        ("the queue refuses a failed shard", "merge_group", "false", "failure", "success", 1),
        ("the queue admits a full pass", "merge_group", "false", "success", "success", 0),
        ("main refuses cancelled shards", "push", "false", "cancelled", "success", 1),
    ):
        result = subprocess.run(
            ["bash", "--noprofile", "--norc", "-euo", "pipefail", "-c", shell],
            env={"PATH": os.defpath, "EVENT_NAME": event, "DOCS_ONLY": docs,
                 "SHARDS": shards, "WINDOWS": windows},
            capture_output=True, text=True, timeout=10, check=False,
        )
        if result.returncode != expected:
            raise AssertionError(
                f"merge queue admission: {label}: expected exit {expected}, got "
                f"{result.returncode}: {result.stdout}{result.stderr}"
            )

    acceptance_blocks = workflow_job_blocks(acceptance)
    gate = acceptance_blocks.get("acceptance")
    if gate is None:
        raise AssertionError("merge queue admission lost the Product Acceptance job")
    for job_id, block in acceptance_blocks.items():
        expected = ACCEPTANCE_GATE_IF if job_id == "acceptance" else ACCEPTANCE_SUITE_IF
        if job_condition(block) != expected:
            raise AssertionError(
                f"merge queue admission requires acceptance job {job_id} to carry "
                f"{expected!r}, not {job_condition(block)!r}"
            )
    gate_needs = re.findall(r"(?m)^\s+- (acceptance-[a-z-]+)$", classifier_active_job_source(gate))
    if sorted(gate_needs) != sorted(job for job in acceptance_blocks if job != "acceptance"):
        raise AssertionError(
            "merge queue admission requires Product Acceptance to wait on every other "
            f"Acceptance job; it needs {gate_needs}"
        )


PAID_RUNNER_WORKFLOWS = ("ci.yml", "acceptance.yml", "sast.yml", "secret-scan.yml")


def assert_paid_runner_scope(workflow: str, others: dict[str, str]) -> None:
    """Admit Depot's runner for the macOS Check & Test shards and nothing else.

    A paid label reached by a pull request spends the organisation's credit on
    a fork's code, and one with no fallback stops the merge queue when the
    provider does. So the label appears exactly once across these workflows,
    as the shards' `runs-on`, behind `vars.KIN_MACOS_HEAVY_RUNNER`, and that
    job skips every pull request.
    """

    sources = {"ci.yml": workflow, **others}
    sites = []
    for name, source in sources.items():
        for line in active_lines(source):
            if "depot-" in line:
                sites.append((name, line.strip()))
    if sites != [("ci.yml", MACOS_SHARD_RUNNER.strip())]:
        raise AssertionError(
            "paid runner scope admits the Depot label once, as the macOS Check & Test "
            f"shards' runner behind its fallback variable; found {sites}"
        )
    shards = workflow_job_blocks(workflow)["check-macos"]
    if MACOS_SHARD_RUNNER not in classifier_active_job_source(shards).splitlines():
        raise AssertionError("paid runner scope requires the Depot label on the check-macos job itself")
    if PULL_REQUEST_SKIP not in job_condition(shards):
        raise AssertionError(
            "paid runner scope requires the job on the paid runner to skip every pull request"
        )


def check_paid_runner_scope(ci_workflow: str) -> None:
    others = {name: (WORKFLOWS / name).read_text(encoding="utf-8")
              for name in PAID_RUNNER_WORKFLOWS if name != "ci.yml"}
    assert_paid_runner_scope(ci_workflow, others)
    blocks = workflow_job_blocks(ci_workflow)
    for label, job_id, old, new in (
        ("the lint job moves onto the paid runner", "check-macos-lint",
         "    runs-on: macos-latest\n", "    runs-on: depot-macos-26\n"),
        ("the paid runner loses its fallback", "check-macos",
         MACOS_SHARD_RUNNER + "\n", "  runs-on: depot-macos-26\n"),
        ("a pull request reaches the paid runner", "check-macos",
         "      && github.event_name != 'pull_request' }}\n", " }}\n"),
    ):
        block = blocks[job_id]
        if block.count(old) != 1:
            raise AssertionError(f"paid runner scope falsification could not identify {label}")
        mutant = ci_workflow.replace(block, block.replace(old, new, 1), 1)
        expect_assertion(label, "paid runner scope",
                         lambda mutant=mutant: assert_paid_runner_scope(mutant, others))
    mutant = dict(others)
    mutant["acceptance.yml"] = others["acceptance.yml"].replace(
        "runs-on: ubuntu-latest", "runs-on: depot-ubuntu-24.04", 1)
    expect_assertion("an Acceptance job moves onto a paid runner", "paid runner scope",
                     lambda: assert_paid_runner_scope(ci_workflow, mutant))


RELEASE_PREFLIGHT_STAND_IN = WORKFLOWS / "release-preflight-stand-in.yml"
RELEASE_PREFLIGHT_CONTEXT = "Release preflight"


def assert_release_preflight_stand_in(stand_in: str, others: dict[str, str]) -> None:
    """One `Release preflight` per event: this one on a pull request, Release Cut's in the queue.

    The stand-in's workflow must start on `pull_request` and on nothing else:
    a skipped job still publishes a check run under its name, so a stand-in in
    a workflow that also ran on merge_group would put a skipped `Release
    preflight` beside Release Cut's status on every queue commit, and a skipped
    check passes a required one. No other job in these workflows may take the
    name either.
    """

    if "\non:\n  pull_request:\n    branches: [main]\n\npermissions:" not in stand_in:
        raise AssertionError(
            "release preflight stand-in requires a workflow triggered by pull_request alone"
        )
    blocks = workflow_job_blocks(stand_in)
    if {job: job_display_name(block) for job, block in blocks.items()} != {
        "release-preflight": RELEASE_PREFLIGHT_CONTEXT
    }:
        raise AssertionError("release preflight stand-in requires exactly one job named Release preflight")
    if job_condition(blocks["release-preflight"]):
        raise AssertionError("release preflight stand-in must report on every pull request")
    for name, source in others.items():
        for job_id, block in workflow_job_blocks(source).items():
            if optional_job_display_name(block) == RELEASE_PREFLIGHT_CONTEXT:
                raise AssertionError(
                    f"release preflight stand-in found a second producer of the name: {name}:{job_id}"
                )


def check_release_preflight_stand_in(ci_workflow: str) -> None:
    stand_in = RELEASE_PREFLIGHT_STAND_IN.read_text(encoding="utf-8")
    others = {"ci.yml": ci_workflow, **{name: (WORKFLOWS / name).read_text(encoding="utf-8")
              for name in PAID_RUNNER_WORKFLOWS if name != "ci.yml"}}
    assert_release_preflight_stand_in(stand_in, others)
    for label, old, new in (
        ("the stand-in also runs in the merge queue", "  pull_request:\n    branches: [main]\n",
         "  pull_request:\n    branches: [main]\n  merge_group:\n"),
        ("the stand-in also runs on main", "  pull_request:\n    branches: [main]\n",
         "  push:\n    branches: [main]\n  pull_request:\n    branches: [main]\n"),
        ("the stand-in skips some pull requests", "    runs-on: ubuntu-latest\n",
         "    if: github.event.pull_request.head.repo.full_name == github.repository\n    runs-on: ubuntu-latest\n"),
        ("the stand-in publishes another name", "    name: Release preflight\n", "    name: Release preflight stand-in\n"),
    ):
        mutant = replace_exactly_once(stand_in, old, new, label)
        expect_assertion(label, "release preflight stand-in",
                         lambda mutant=mutant: assert_release_preflight_stand_in(mutant, others))
    second = dict(others)
    second["ci.yml"] = ci_workflow.replace(
        "    name: Falsify guards\n", "    name: Release preflight\n", 1)
    expect_assertion("ci.yml publishes a second Release preflight", "release preflight stand-in",
                     lambda: assert_release_preflight_stand_in(stand_in, second))


def check_merge_queue_admission(ci_workflow: str) -> None:
    acceptance = ACCEPTANCE_WORKFLOW.read_text(encoding="utf-8")
    assert_merge_queue_admission(ci_workflow, acceptance)
    blocks = workflow_job_blocks(ci_workflow)
    for label, job_id, old, new in (
        ("clippy also skips the merge queue", "fast-gate-lint",
         "      - name: Clippy\n        if: github.event_name != 'pull_request'\n",
         "      - name: Clippy\n        if: github.event_name == 'push'\n"),
        ("clippy runs on every pull request again", "fast-gate-lint",
         "      - name: Clippy\n        if: github.event_name != 'pull_request'\n",
         "      - name: Clippy\n"),
        ("a policy step stops running on pull requests", "fast-gate-lint",
         "      - name: Check formatting\n",
         "      - name: Check formatting\n        if: github.event_name != 'pull_request'\n"),
        ("the shards leave the merge queue", "fast-gate-tests",
         "&& github.event_name != 'pull_request' }}", "&& github.event_name == 'push' }}"),
        ("the MCP surface runs on every pull request again", "mcp-surface-contract",
         "      && github.event_name != 'pull_request' }}\n", " }}\n"),
        ("the aggregate passes the queue without its shards", "fast-gate-tests-aggregate",
         'if [ "$EVENT_NAME" = "pull_request" ]; then', 'if [ "$EVENT_NAME" != "push" ]; then'),
        ("a release suite excludes the merge queue", "falsify-guards",
         "&& github.event_name != 'pull_request' }}",
         "&& github.event_name != 'pull_request' && github.event_name != 'merge_group' }}"),
    ):
        block = blocks[job_id]
        if block.count(old) != 1:
            raise AssertionError(f"merge queue admission falsification could not identify {label}")
        mutant = ci_workflow.replace(block, block.replace(old, new, 1), 1)
        acceptance = ACCEPTANCE_WORKFLOW.read_text(encoding="utf-8")
        expect_assertion(label, "merge queue admission",
                         lambda mutant=mutant, acceptance=acceptance: assert_merge_queue_admission(mutant, acceptance))
    acceptance = ACCEPTANCE_WORKFLOW.read_text(encoding="utf-8")
    acceptance_blocks = workflow_job_blocks(acceptance)
    for label, job_id, old, new in (
        ("Product Acceptance stops running after a failed suite job", "acceptance",
         "if: ${{ !cancelled() && github.event_name != 'pull_request' }}",
         "if: ${{ github.event_name != 'pull_request' }}"),
        ("Product Acceptance runs on pull requests again", "acceptance",
         "if: ${{ !cancelled() && github.event_name != 'pull_request' }}",
         "if: ${{ !cancelled() }}"),
        ("a suite job runs on export pull requests again", "acceptance-brownfield",
         "if: ${{ github.event_name != 'pull_request' }}",
         "if: ${{ github.event_name != 'pull_request' || startsWith(github.head_ref, 'feature/public-export-') }}"),
        ("the gate stops waiting on a suite job", "acceptance",
         "      - acceptance-surface-suites\n", ""),
    ):
        block = acceptance_blocks[job_id]
        if block.count(old) != 1:
            raise AssertionError(f"merge queue admission falsification could not identify {label}")
        mutant = acceptance.replace(block, block.replace(old, new, 1), 1)
        expect_assertion(label, "merge queue admission",
                         lambda mutant=mutant: assert_merge_queue_admission(ci_workflow, mutant))


def check_ci_job_authority(ci_workflow: str) -> None:
    assert_check_consumer_authority(ci_workflow)
    check_merge_queue_admission(ci_workflow)
    check_paid_runner_scope(ci_workflow)
    check_release_preflight_stand_in(ci_workflow)
    assert_falsify_guard_sharding(ci_workflow)
    assert_falsify_harness_refuses_bad_shards()
    falsify_job_block = workflow_job_blocks(ci_workflow)["falsify-guards"]
    for label, old, new in (
        ("a shard's failure is ignored", 'wait "$pid" || failed=1', 'wait "$pid" || true'),
        ("the receipts stop being summed against the total",
         '"$ran_sum" -ne "$total_seen"', '"$ran_sum" -lt 0'),
        ("every shard runs the same share",
         '--shard "$shard/$shards"', '--shard "1/$shards"'),
        ("the shard count drifts from the runner's cores", "          shards=4\n", "          shards=2\n"),
        ("a failed shard no longer fails the step", "          if [[ $failed -ne 0 ]]; then\n",
         "          if [[ $failed -ne 0 && -n \"${NEVER:-}\" ]]; then\n"),
    ):
        if falsify_job_block.count(old) != 1:
            raise AssertionError(f"falsify guard sharding falsification could not identify {label}")
        mutant = ci_workflow.replace(falsify_job_block, falsify_job_block.replace(old, new, 1), 1)
        expect_assertion(label, "falsify guard sharding",
                         lambda mutant=mutant: assert_falsify_guard_sharding(mutant))
    assert_feature_permutation_authority(ci_workflow)
    assert_feature_permutation_aggregate_results(ci_workflow)
    assert_feature_permutation_falsification(ci_workflow)
    assert_macos_shard_authority(ci_workflow)
    assert_ubuntu_shard_authority(ci_workflow)
    assert_fast_gate_authority(ci_workflow)
    fast_gate_blocks = workflow_job_blocks(ci_workflow)
    for label, owner, old, new, expected in (
        (
            "the aggregate stops always running, so a pull request waits on a "
            "context nothing will report",
            "fast-gate-tests-aggregate",
            f"    {FAST_GATE_AGGREGATE_ALWAYS_RUNS}",
            "    if: ${{ !cancelled() && github.event_name != 'pull_request' }}",
            "must run on every event",
        ),
        (
            "the aggregate accepts a shard result that is not success",
            "fast-gate-tests-aggregate",
            FAST_GATE_AGGREGATE_SUCCESS_GATE,
            'if [ "$SHARDS" = "failure" ]; then',
            "admit only `success`",
        ),
        (
            "the aggregate stops waiting on the shards it grades",
            "fast-gate-tests-aggregate",
            f"    {FAST_GATE_AGGREGATE_NEEDS}",
            "    needs: changes",
            "must wait on the shards",
        ),
        (
            "the shards stop asserting the selection lists any test",
            "fast-gate-tests",
            'print("::error title=Empty test selection::the scope lists zero tests")',
            "pass",
            "sharded half must keep running",
        ),
        (
            "the scope selector is dropped and the shards test whatever is left",
            "fast-gate-tests",
            "python3 scripts/changed-crate-scope.py",
            "true scripts/changed-crate-scope-disabled.py",
            "sharded half must keep running",
        ),
        (
            "one red shard is allowed to cancel its passing siblings",
            "fast-gate-tests",
            f"      {FAST_GATE_SHARD_INDEPENDENT_LEGS}",
            "      fail-fast: true",
            "shards must keep",
        ),
        (
            "the quarantine clock stops running in the admission core",
            "fast-gate-lint",
            "        run: python3 scripts/check-quarantine.py\n",
            "        run: true\n",
            "lint and policy half must keep running",
        ),
        (
            # The DENOMINATOR moves and the matrix does not, which is the
            # mutation only the join can catch. Mutating the matrix instead
            # would red on FAST_GATE_SHARD_MATRIX one assertion earlier and
            # prove nothing about this one; that arm was written first and the
            # falsification loop's own wrong-reason check rejected it.
            #
            # Its effect is the silent kind. Matrix [1, 2, 3] against
            # `count:${{ matrix.shard }}/7` means parts 4 through 7 are never
            # dispatched, so four sevenths of the selection never runs, all
            # three shards pass, and the required context reports success.
            "the partition denominator drifts above the shard matrix",
            "fast-gate-tests",
            "--partition count:${{ matrix.shard }}/3",
            "--partition count:${{ matrix.shard }}/7",
            "one fact in two places and they disagree",
        ),
        (
            "the Firestore post-status response contract is no longer selected by the admission core",
            "fast-gate-lint",
            FIRESTORE_RESPONSE_BODY_CONTRACT_TESTS[0],
            "removed_firestore_response_body_contract",
            "Firestore post-status response contract",
        ),
    ):
        # Scoped to the owning job block rather than the whole file: two jobs
        # carry `if: ${{ !cancelled() }}`, and a whole-file replace would mutate
        # whichever came first and grade a job this assertion is not about.
        block = fast_gate_blocks[owner]
        if block.count(old) != 1:
            raise AssertionError(
                f"admission core falsification could not identify {label}"
            )
        mutant_workflow = ci_workflow.replace(block, block.replace(old, new, 1), 1)
        expect_assertion(
            label,
            expected,
            lambda mutant=mutant_workflow: assert_fast_gate_authority(mutant),
        )
    assert_shared_cache_key_jobs_declare_one_environment(ci_workflow)
    consumer_blocks = workflow_job_blocks(ci_workflow)
    stub_check = consumer_blocks["check-pr-fast-path"]
    real_check = consumer_blocks["check"]

    def falsify_job(job_id: str, label: str, old: str, new: str, expected: str, check) -> None:
        block = consumer_blocks[job_id]
        if block.count(old) != 1:
            raise AssertionError(f"{expected} falsification could not identify {label}")
        mutant_workflow = ci_workflow.replace(block, block.replace(old, new, 1), 1)
        expect_assertion(label, expected, lambda: check(mutant_workflow))

    for platform, shards_id, lint_id, aggregate_id, features, count, check in (
        ("macOS", "check-macos", "check-macos-lint", "check-macos-aggregate",
         "kin-cli/metal,kin-daemon/metal", 2, assert_macos_shard_authority),
        ("ubuntu", "check", "check-lint", "check-aggregate",
         "kin-cli/embeddings,kin-daemon/embeddings", 3, assert_ubuntu_shard_authority),
    ):
        expected = f"{platform} shard authority"
        matrix = "        shard: [" + ", ".join(str(n) for n in range(1, count + 1)) + "]"
        for label, old, new in (
            ("the aggregate accepts a shard result that is not success",
             'if [ "$SHARDS" != "success" ]; then', 'if [ "$SHARDS" = "failure" ]; then'),
            ("the aggregate accepts a lint result that is not success",
             'if [ "$LINT" != "success" ]; then', 'if [ "$LINT" = "failure" ]; then'),
            ("the aggregate loses the matrix that keeps a skip from expanding",
             f"        os: [{'macos-latest' if platform == 'macOS' else 'ubuntu-latest'}]", "        os: []"),
            ("the aggregate stops waiting on the shards",
             f"    needs: [changes, {lint_id}, {shards_id}]", f"    needs: [changes, {lint_id}]"),
            ("the aggregate stops waiting on the lint job",
             f"    needs: [changes, {lint_id}, {shards_id}]", f"    needs: [changes, {shards_id}]"),
            ("the aggregate takes the display name of the shard job",
             "    name: Check & Test", f"    name: Check & Test {platform} aggregate"),
        ):
            falsify_job(aggregate_id, f"{platform}: {label}", old, new, expected, check)
        for label, old, new in (
            ("a shard stops partitioning and every leg runs the whole suite",
             f"--features {features} --partition count:${{{{ matrix.shard }}}}/{count}",
             f"--features {features}"),
            ("one red shard is allowed to cancel its passing siblings",
             "      fail-fast: false", "      fail-fast: true"),
            ("the shard count changes without the partition denominator",
             matrix, matrix[:-1] + f", {count + 1}]"),
            # The DENOMINATOR moves and the matrix does not, which only the join
            # catches: parts beyond the matrix are never dispatched, every shard
            # passes, and the required context reports success.
            ("the partition denominator drifts above the shard matrix",
             f"count:${{{{ matrix.shard }}}}/{count}", f"count:${{{{ matrix.shard }}}}/{count + 2}"),
            ("a step the partition consumes is pinned to one shard",
             "      - name: Build\n", "      - name: Build\n        if: matrix.shard == 1\n"),
            ("a step the partition consumes leaves the job",
             "      - name: Install nextest\n", "      - name: Install nextest (moved)\n"),
            ("a lint gate moves onto the shards and runs once per partition",
             "      - name: Build\n",
             "      - name: Doc tests\n        run: cargo test --doc --locked\n\n      - name: Build\n"),
        ):
            falsify_job(shards_id, f"{platform}: {label}", old, new, expected, check)
        for label, old, new in (
            ("the doctest pass nextest cannot run disappears",
             f"        run: cargo test --doc --locked --features {features}",
             "        run: echo doctests skipped"),
            ("clippy is renamed off the pinned gate list",
             "      - name: Clippy\n", "      - name: Clippy (moved)\n"),
            ("a gate only runs on some events",
             "      - name: Clippy\n", "      - name: Clippy\n        if: matrix.shard == 1\n"),
            ("the lint job leaves its admission",
             "    needs: changes\n", "    needs: [changes, dco]\n"),
        ):
            falsify_job(lint_id, f"{platform}: {label}", old, new, expected, check)
    falsify_job("check-macos-lint", "check-macos-lint leaves macOS", "    runs-on: macos-latest",
                "    runs-on: ubuntu-latest", "macOS shard authority",
                assert_macos_shard_authority)
    falsify_job("check-macos", "check-macos leaves its Apple Silicon runner",
                MACOS_SHARD_RUNNER, "  runs-on: ubuntu-latest", "macOS shard authority",
                assert_macos_shard_authority)
    for label, owner, old, new in (
        # The shards no longer run on ubuntu-latest, so mutating away from it
        # stopped identifying anything the day the runner moved. What the arm
        # has always meant is "the shards leave their pinned runner", so it
        # names the pinned value rather than the historical one.
        ("the ubuntu shards leave their pinned runner", "check",
         "    runs-on: ${{ vars.KIN_HEAVY_RUNNER || 'ubuntu-latest' }}", "    runs-on: macos-latest"),
        ("a source-reading gate is renamed off the pinned list", "check-lint",
         "      - name: Check Runtime Boundaries\n", "      - name: Check Runtime Boundaries (moved)\n"),
        ("the Linux musl release-target check leaves the lint job", "check-lint",
         "      - name: Check the Linux release target (musl)\n",
         "      - name: Check the Linux release target, renamed\n"),
    ):
        falsify_job(owner, label, old, new, "ubuntu shard authority", assert_ubuntu_shard_authority)

    # The shared cache-key guard, driven in every direction it can fail.
    for label, old, new in (
        (
            "a cache step lets the runner's toolchain set back into the key",
            "          add-rust-environment-hash-key: false",
            "          add-rust-environment-hash-key: true",
        ),
        (
            # Target the `shared-key` line itself rather than one job's
            # expression. Unsharing a SINGLE job stopped falsifying this the day
            # main added a second and third job keyed on `check`: the group was
            # no longer emptied, the survivors still agreed, and the guard
            # correctly stayed green while the arm expected red. Commenting out
            # every shared-key leaves nothing sharing at all, which is what the
            # label claims and what the refusal above is for, and it cannot be
            # broken again by a new sharer arriving.
            "the shared-key expression stops naming a job, so nothing shares",
            "          shared-key: ",
            "          # falsification removed shared-key: ",
        ),
        (
            "the action is renamed, so the extraction silently finds no cache step",
            "uses: Swatinem/rust-cache@c19371144df3bb44fab255c43d04cbc2ab54d1c4",
            "uses: Renamed/rust-cache@c19371144df3bb44fab255c43d04cbc2ab54d1c4",
        ),
    ):
        if ci_workflow.count(old) < 1:
            raise AssertionError(
                f"shared cache-key falsification could not identify {label}"
            )
        expect_assertion(
            label,
            "shared cache-key authority",
            lambda mutant=ci_workflow.replace(
                old, new
            ): assert_shared_cache_key_jobs_declare_one_environment(mutant),
        )

    # The one that matters most, because it is the collision the guard exists
    # for and the only one nothing else in this repository would notice.
    for worker in FEATURE_PERMUTATION_WORKERS:
        feature_tests = consumer_blocks[worker]
        diverged_env = feature_tests.replace(
            '      CARGO_PROFILE_TEST_DEBUG: "0"',
            '      CARGO_PROFILE_TEST_DEBUG: "2"',
            1,
        )
        if diverged_env == feature_tests:
            raise AssertionError(
                f"shared cache-key falsification could not diverge {worker}'s environment"
            )
        expect_assertion(
            "a job sharing the cargo cache key declares a different environment",
            "shared cache-key authority",
            lambda mutant=ci_workflow.replace(feature_tests, diverged_env, 1):
            assert_shared_cache_key_jobs_declare_one_environment(mutant),
        )

    stub_condition = (
        "    if: ${{ !cancelled() && github.event_name == 'pull_request' "
        "}}"
    )
    real_check_condition = (
        "    if: >-\n"
        "      ${{ !cancelled()\n"
        "      && needs.changes.outputs.docs_only != 'true'\n"
        f"      && {RELEASE_SUITE_EVENT_FILTER} }}}}"
    )
    swapped_stub_check = stub_check.replace(
        stub_condition,
        real_check_condition,
        1,
    )
    swapped_real_check = real_check.replace(
        real_check_condition,
        stub_condition,
        1,
    )
    swapped_consumers = ci_workflow.replace(
        stub_check,
        swapped_stub_check,
        1,
    ).replace(
        real_check,
        swapped_real_check,
        1,
    )
    expect_assertion(
        "stub and real Check & Test conditions swapped",
        "Check & Test consumer authority",
        lambda: assert_check_consumer_authority(swapped_consumers),
    )

    for label, old, new in (
        (
            "pull-request Check & Test job identity changed",
            "  check-pr-fast-path:",
            "  check-context-spoof:",
        ),
        (
            "pull-request Check & Test display name changed",
            "    name: Check & Test",
            "    name: Documentation shortcut",
        ),
        (
            "pull-request Check & Test gained a dependency it must not wait on",
            "    runs-on: ubuntu-latest",
            "    needs: changes\n    runs-on: ubuntu-latest",
        ),
        (
            "pull-request Check & Test runner changed",
            "    runs-on: ubuntu-latest",
            "    runs-on: ${{ matrix.os }}",
        ),
        (
            "pull-request Check & Test matrix changed",
            "        os: [ubuntu-latest, macos-latest]",
            "        os: [ubuntu-latest]",
        ),
        (
            "pull-request Check & Test inert step changed",
            '          echo "Admission is the fast gate; '
            'see .github/workflows/ci.yml."',
            '          echo "unreviewed shortcut"',
        ),
        (
            # The one that matters most after the admission core: narrowing this back to
            # documentation-only diffs leaves every ordinary pull request with
            # no producer of either expanded `Check & Test` name at all, which
            # is a silent hang rather than a failure.
            "pull-request Check & Test narrowed back to documentation diffs",
            "    if: ${{ !cancelled() && github.event_name == 'pull_request' }}",
            "    if: ${{ !cancelled() && "
            "needs.changes.outputs.docs_only == 'true' }}",
        ),
    ):
        if stub_check.count(old) != 1:
            raise AssertionError(
                f"Check & Test consumer falsification could not identify {label}"
            )
        mutant_job = stub_check.replace(old, new, 1)
        mutant_workflow = ci_workflow.replace(stub_check, mutant_job, 1)
        expect_assertion(
            label,
            "Check & Test consumer authority",
            lambda mutant_workflow=mutant_workflow: assert_check_consumer_authority(
                mutant_workflow
            ),
        )

    for label, old, new in (
        (
            # The sharp regression now that the aggregate owns the required
            # name: a shard claiming `Check & Test` publishes
            # `Check & Test (ubuntu-latest, 1)` and `(2)`, and its bare name
            # collides with the aggregate's on a skip.
            "real Check & Test display name changed",
            "    name: Check & Test ubuntu shard",
            "    name: Check & Test",
        ),
        (
            "real Check & Test dependency changed",
            "    needs: changes",
            "    needs: dco",
        ),
        (
            # The real check job moved to the larger runner; the pull-request
            # stub above did not, which is why only this arm changes and the
            # stub's two arms still name ubuntu-latest.
            "real Check & Test runner detached from its shard matrix",
            "    runs-on: ${{ vars.KIN_HEAVY_RUNNER || 'ubuntu-latest' }}",
            "    runs-on: ${{ matrix.os }}",
        ),
        (
            # Restoring a platform matrix is the specific regression to catch:
            # `check` would publish expanded platform names again, beside the
            # aggregates', and put two check runs under one required context
            # name.
            "real Check & Test matrix changed",
            "        shard: [1, 2, 3]",
            "        os: [ubuntu-latest, macos-latest]",
        ),
    ):
        if real_check.count(old) != 1:
            raise AssertionError(
                f"Check & Test consumer falsification could not identify {label}"
            )
        mutant_job = real_check.replace(old, new, 1)
        mutant_workflow = ci_workflow.replace(real_check, mutant_job, 1)
        expect_assertion(
            label,
            "Check & Test consumer authority",
            lambda mutant_workflow=mutant_workflow: assert_check_consumer_authority(
                mutant_workflow
            ),
        )

    # Any job that is not a reviewed producer will do, and Library packages is
    # one no change here touches. It used to be Code Coverage, which is gone.
    unrelated_job = consumer_blocks["library-packages"]
    if unrelated_job.count("    name: Library packages") != 1:
        raise AssertionError(
            "Check & Test consumer falsification could not identify an unrelated job name"
        )
    duplicate_context = ci_workflow.replace(
        unrelated_job,
        unrelated_job.replace(
            "    name: Library packages",
            "    name: Check & Test",
            1,
        ),
        1,
    )
    expect_assertion(
        "unrelated job spoofs the Check & Test display context",
        "Check & Test consumer authority",
        lambda: assert_check_consumer_authority(duplicate_context),
    )



def check_docs_only_classifier_authority(
    ci_workflow: str, classifier: str, workflow_sources: dict[Path, str]
) -> None:
    for label, old, new in (
        (
            "classifier job output remapped to a constant",
            "      docs_only: ${{ steps.classify.outputs.docs_only }}",
            '      docs_only: "true"',
        ),
        (
            "classifier checkout pin changed to a moving ref",
            "      - uses: actions/checkout@"
            "3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1",
            "      - uses: actions/checkout@v7",
        ),
        (
            "classifier checkout depth changed",
            "          fetch-depth: 0",
            "          fetch-depth: 1",
        ),
        (
            "classifier step id changed",
            "        id: classify",
            "        id: classify_mutant",
        ),
    ):
        if classifier.count(old) != 1:
            raise AssertionError(
                f"diff classifier falsification could not identify {label}"
            )
        mutant = classifier.replace(old, new, 1)
        mutant_workflow = ci_workflow.replace(classifier, mutant, 1)
        expect_assertion(
            label,
            "exactly match the closed-form docs_only authority contract",
            lambda mutant_workflow=mutant_workflow: assert_docs_only_classifier_guard(
                mutant_workflow
            ),
        )

    workflow_environment_needle = '  RUSTFLAGS: "-D warnings"'
    if ci_workflow.count(workflow_environment_needle) != 1:
        raise AssertionError(
            "diff classifier falsification could not identify workflow environment"
        )
    workflow_environment_attacks = (
        (
            "workflow PATH resolves Git from a checked-out executable",
            "  PATH: ${{ github.workspace }}/attack:/usr/local/bin:/usr/bin:/bin",
        ),
        (
            "workflow Git repository and index authority redirected",
            "  GIT_DIR: ${{ github.workspace }}/attack/decoy.git\n"
            "  GIT_WORK_TREE: ${{ github.workspace }}/attack\n"
            "  GIT_INDEX_FILE: ${{ github.workspace }}/attack/index",
        ),
        (
            "workflow Git configuration and external diff authority injected",
            '  GIT_CONFIG_NOSYSTEM: "0"\n'
            "  GIT_CONFIG_GLOBAL: ${{ github.workspace }}/attack/gitconfig\n"
            '  GIT_CONFIG_COUNT: "1"\n'
            "  GIT_CONFIG_KEY_0: diff.external\n"
            "  GIT_CONFIG_VALUE_0: ${{ github.workspace }}/attack/external-diff\n"
            "  GIT_EXTERNAL_DIFF: ${{ github.workspace }}/attack/external-diff",
        ),
        (
            "workflow Git object, replacement, and executable authority injected",
            "  GIT_OBJECT_DIRECTORY: ${{ github.workspace }}/attack/objects\n"
            "  GIT_ALTERNATE_OBJECT_DIRECTORIES: "
            "${{ github.workspace }}/attack/alternate-objects\n"
            "  GIT_EXEC_PATH: ${{ github.workspace }}/attack\n"
            "  GIT_REPLACE_REF_BASE: refs/replace",
        ),
    )
    path_attack_workflow = ""
    for label, injected_environment in workflow_environment_attacks:
        mutant_workflow = ci_workflow.replace(
            workflow_environment_needle,
            f"{workflow_environment_needle}\n{injected_environment}",
            1,
        )
        if not path_attack_workflow:
            path_attack_workflow = mutant_workflow
        expect_assertion(
            label,
            "workflow header must exactly match the closed-form",
            lambda mutant_workflow=mutant_workflow: assert_docs_only_classifier_guard(
                mutant_workflow
            ),
        )

    # The exact PATH attack remains a structurally valid member of the old
    # census/cache/consumer contracts. Only the workflow-level process contract
    # should reject it, and the runtime proof below must remain safe if that
    # header defense is accidentally bypassed.
    path_attack_sources = dict(workflow_sources)
    path_attack_sources[WORKFLOWS / "ci.yml"] = path_attack_workflow
    assert_workflow_job_census(path_attack_sources)
    assert_rust_cache_steps(path_attack_sources)
    assert_check_consumer_authority(path_attack_workflow)
    with tempfile.TemporaryDirectory() as directory:
        (
            fixture,
            code_base_sha,
            code_head_sha,
            hostile_environment,
            execution_markers,
        ) = create_classifier_authority_attack_fixture(
            Path(directory),
            ci_workflow,
            path_attack_workflow,
        )
        fake_git = subprocess.run(
            ["git", "diff", "--name-only", f"{code_base_sha}...{code_head_sha}"],
            cwd=fixture,
            env={"PATH": hostile_environment["PATH"]},
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        if fake_git.returncode != 0 or fake_git.stdout.splitlines() != [
            "docs/decoy.md"
        ]:
            raise AssertionError(
                "workflow PATH attack fixture did not substitute its executable: "
                f"{fake_git.stdout}{fake_git.stderr}"
            )

        result, outputs = execute_docs_only_classifier(
            classifier,
            cwd=fixture,
            environment_overrides={
                **hostile_environment,
                "EVENT_NAME": "pull_request",
                "BASE_SHA": code_base_sha,
                "HEAD_SHA": code_head_sha,
                "WORKSPACE": str(fixture),
            },
        )
        assert_classifier_execution_code_bearing(
            "absolute environment-clean Git rejects workflow PATH and Git authority",
            result,
            outputs,
            changed_paths=(".github/workflows/ci.yml", "src/lib.rs"),
        )
        executed_markers = [marker for marker in execution_markers if marker.exists()]
        if executed_markers:
            raise AssertionError(
                "classifier executed an inherited external diff or textconv: "
                f"{executed_markers}"
            )

    environment_binding = (
        "          PATH: /usr/bin:/bin\n"
        '          BASH_ENV: ""\n'
        '          ENV: ""\n'
        '          LD_AUDIT: ""\n'
        "          LD_LIBRARY_PATH: /dev/null\n"
        '          LD_PRELOAD: ""\n'
        "          WORKSPACE: ${{ github.workspace }}\n"
        "          EVENT_NAME: ${{ github.event_name }}\n"
        "          BASE_SHA: ${{ github.event.pull_request.base.sha }}\n"
        "          HEAD_SHA: ${{ github.event.pull_request.head.sha }}"
    )
    if classifier.count(environment_binding) != 1:
        raise AssertionError(
            "diff classifier falsification could not identify its environment binding"
        )
    with tempfile.TemporaryDirectory() as directory:
        fixture, base_sha, head_sha = create_docs_only_git_fixture(Path(directory))
        fixed_range_binding = (
            "          PATH: /usr/bin:/bin\n"
            '          BASH_ENV: ""\n'
            '          ENV: ""\n'
            '          LD_AUDIT: ""\n'
            "          LD_LIBRARY_PATH: /dev/null\n"
            '          LD_PRELOAD: ""\n'
            f"          WORKSPACE: {fixture!s}\n"
            "          EVENT_NAME: pull_request\n"
            f"          BASE_SHA: {base_sha}\n"
            f"          HEAD_SHA: {head_sha}"
        )
        fixed_range = classifier.replace(
            environment_binding,
            fixed_range_binding,
            1,
        )
        expect_assertion(
            "push classifier bound to a fixed historical docs-only range",
            "exactly match the closed-form docs_only authority contract",
            lambda: assert_docs_only_classifier_guard(
                ci_workflow.replace(classifier, fixed_range, 1)
            ),
        )
        result, outputs = execute_docs_only_classifier(
            fixed_range,
            cwd=fixture,
            environment_overrides={
                "EVENT_NAME": "pull_request",
                "BASE_SHA": base_sha,
                "HEAD_SHA": head_sha,
                "WORKSPACE": str(fixture),
            },
        )
        assert_classifier_execution_won(
            "push classifier bound to a fixed historical docs-only range",
            result,
            outputs,
            changed_path="docs/release-bot.md",
        )

        hostile_bash_env = Path(directory) / "hostile-bash-env"
        hostile_bash_env.write_text(
            f"EVENT_NAME=pull_request\nBASE_SHA={base_sha}\nHEAD_SHA={head_sha}\n",
            encoding="utf-8",
        )
        bash_env_mutant = classifier.replace(
            '          BASH_ENV: ""',
            f'          BASH_ENV: "{hostile_bash_env}"',
            1,
        )
        expect_assertion(
            "classifier shell startup overridden through BASH_ENV",
            "exactly match the closed-form docs_only authority contract",
            lambda: assert_docs_only_classifier_guard(
                ci_workflow.replace(classifier, bash_env_mutant, 1)
            ),
        )
        result, outputs = execute_docs_only_classifier(
            bash_env_mutant,
            cwd=fixture,
            environment_overrides={"BASH_ENV": str(hostile_bash_env)},
        )
        assert_classifier_execution_failed_closed(
            "privileged classifier shell ignores BASH_ENV startup injection",
            result,
            outputs,
        )

        shell_line = (
            "        shell: /usr/bin/bash --noprofile --norc -p -e -u -o pipefail {0}"
        )
        unprivileged_shell = classifier.replace(
            shell_line,
            "        shell: /usr/bin/bash --noprofile --norc -e -u -o pipefail {0}",
            1,
        )
        expect_assertion(
            "classifier shell drops privileged startup",
            "exactly match the closed-form docs_only authority contract",
            lambda: assert_docs_only_classifier_guard(
                ci_workflow.replace(classifier, unprivileged_shell, 1)
            ),
        )
        hostile_functions = {
            "BASH_FUNC_echo%%": (
                '() { builtin echo "docs_only=true" >> "$GITHUB_OUTPUT"; }'
            ),
        }
        result, outputs = execute_docs_only_classifier(
            classifier,
            cwd=fixture,
            environment_overrides=hostile_functions,
        )
        assert_classifier_execution_failed_closed(
            "privileged classifier rejects an inherited output function",
            result,
            outputs,
        )
        result, outputs = execute_docs_only_classifier(
            unprivileged_shell,
            cwd=fixture,
            environment_overrides=hostile_functions,
            privileged=False,
        )
        assert_classifier_execution_won(
            "unprivileged classifier imports a hostile output function",
            result,
            outputs,
        )

        hostile_shell = classifier.replace(
            shell_line,
            "        shell: bash --noprofile --norc -c "
            "'exec env EVENT_NAME=pull_request "
            f"BASE_SHA={base_sha} HEAD_SHA={head_sha} BASH_ENV= "
            '/usr/bin/bash --noprofile --norc "$1"\' wrapper {0}',
            1,
        )
        expect_assertion(
            "classifier custom shell overrides event and SHA authority",
            "exactly match the closed-form docs_only authority contract",
            lambda: assert_docs_only_classifier_guard(
                ci_workflow.replace(classifier, hostile_shell, 1)
            ),
        )
        result, outputs = execute_docs_only_classifier(
            hostile_shell,
            cwd=fixture,
            environment_overrides={
                "FIXED_BASE_SHA": base_sha,
                "FIXED_HEAD_SHA": head_sha,
            },
            shell_wrapper=(
                'exec env EVENT_NAME=pull_request BASE_SHA="$FIXED_BASE_SHA" '
                'HEAD_SHA="$FIXED_HEAD_SHA" BASH_ENV= '
                'bash --noprofile --norc "$1"'
            ),
        )
        assert_classifier_execution_won(
            "classifier custom shell overrides event and SHA authority",
            result,
            outputs,
            changed_path="docs/release-bot.md",
        )

    classifier_falsification_needle = (
        '          fi\n          echo "docs_only=$docs_only" >> "$GITHUB_OUTPUT"'
    )
    if classifier.count(classifier_falsification_needle) != 1:
        raise AssertionError(
            "diff classifier falsification could not identify the outer guard boundary"
        )
    truthy_after_guard = classifier.replace(
        classifier_falsification_needle,
        "          fi\n"
        "          docs_only=true\n"
        '          echo "docs_only=$docs_only" >> "$GITHUB_OUTPUT"',
        1,
    )
    assert_classifier_bypass_rejected(
        "docs_only=true after the matching pull_request fi",
        ci_workflow.replace(classifier, truthy_after_guard, 1),
    )
    for label, command in (
        ("single-quoted docs_only assignment", "docs_only='true'"),
        ("double-quoted docs_only assignment", 'docs_only="true"'),
        ("indirect printf docs_only assignment", "printf -v docs_only true"),
        (
            "computed-name docs_only assignment",
            'name=docs_$(printf only); printf -v "$name" true',
        ),
        ("eval-computed docs_only assignment", "eval 'docs_'only=true"),
        ("quoted-hash docs_only assignment", "printf '#'; docs_only=true"),
    ):
        bypass = classifier.replace(
            classifier_falsification_needle,
            f"          fi\n          {command}\n"
            '          echo "docs_only=$docs_only" >> "$GITHUB_OUTPUT"',
            1,
        )
        assert_classifier_bypass_rejected(
            label,
            ci_workflow.replace(classifier, bypass, 1),
        )
    nameref_bypass = classifier.replace(
        classifier_falsification_needle,
        "          fi\n"
        '          name=docs_only; declare -n ref="$name"; ref=true\n'
        '          echo "docs_only=$docs_only" >> "$GITHUB_OUTPUT"',
        1,
    )
    assert_classifier_bypass_rejected(
        "nameref docs_only assignment",
        ci_workflow.replace(classifier, nameref_bypass, 1),
        execute=bash_supports_nameref(),
    )
    duplicate_computed_output = classifier.replace(
        classifier_falsification_needle,
        "          fi\n"
        '          echo "docs_only=$docs_only" >> "$GITHUB_OUTPUT"\n'
        "          name=docs_$(printf only); "
        'printf \'%s=true\\n\' "$name" >> "$GITHUB_OUTPUT"',
        1,
    )
    assert_classifier_bypass_rejected(
        "duplicate computed docs_only output",
        ci_workflow.replace(classifier, duplicate_computed_output, 1),
    )
    comment_only = classifier.replace(
        classifier_falsification_needle,
        "          fi\n"
        "          # docs_only='true' is intentionally ignored in comments\n"
        '          echo "docs_only=$docs_only" >> "$GITHUB_OUTPUT"',
        1,
    )
    assert_docs_only_classifier_guard(ci_workflow.replace(classifier, comment_only, 1))



def check_release_pr_ci_scope(ci_workflow: str) -> None:
    assert_release_pr_ci_scope_cannot_widen(ci_workflow)
    # This guard has to be able to fail before it is worth carrying, and it
    # currently constrains nothing in the tree, so falsify it against the two
    # shapes a loose carve-out actually takes.
    release_pr_job_start = ci_workflow.index("  npm-launchers:")
    release_pr_job_end = ci_workflow.index("\n  windows-authority-tests:")
    release_pr_job = ci_workflow[release_pr_job_start:release_pr_job_end]
    for label, condition, expected in (
        (
            "a release-PR carve-out written against github.ref",
            "    if: >-\n      ${{ github.ref != 'refs/heads/automation/release-next' }}\n",
            "other than github.head_ref",
        ),
        (
            "a release-PR carve-out that never pins the pull_request event",
            "    if: >-\n      ${{ github.head_ref != 'automation/release-next' }}\n",
            "without pinning the pull_request event",
        ),
    ):
        mutated_ci = (
            ci_workflow[:release_pr_job_start]
            + release_pr_job.replace(
                "    name: npm launcher tests\n",
                "    name: npm launcher tests\n" + condition,
                1,
            )
            + ci_workflow[release_pr_job_end:]
        )
        expect_assertion(
            label,
            expected,
            lambda source=mutated_ci: assert_release_pr_ci_scope_cannot_widen(source),
        )
