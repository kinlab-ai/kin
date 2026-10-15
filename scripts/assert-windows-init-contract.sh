#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
#
# The native Windows `kin init` admission contract.
#
# Exact Git import and an empty native repository must both publish complete
# graph authority. A populated directory with no Git history must be admitted
# as well: Kin starts a native repository there with nothing committed and
# reports what the directory holds as that repository's uncommitted work, so
# none of it becomes recorded history until someone commits it.
#
# A new store lives in the central stores directory under the Kin home, and the
# working tree carries nothing. Publication is therefore judged the way every
# command finds a store: `kin store path`, run inside the tree, has to name a
# store that holds a manifest. Each fixture gets a Kin home of its own beside
# it, so the runner's home is never touched and each stores directory holds
# exactly what one admission published. The in-tree layout that
# `KIN_STORE_LAYOUT=in-tree` selects is still supported, so the exact-Git shape
# is admitted once more under it and must publish the tree's own `.kin`.
#
# Both CI jobs that own a native Windows executable run this same script. The
# pull-request job makes the behavior reviewable; the landing installer job
# proves the release-profile binary from the exact commit used by release.
#
# Usage: assert-windows-init-contract.sh <kin executable> <scratch directory>

set -euo pipefail

usage() {
  echo "usage: assert-windows-init-contract.sh <kin executable> <scratch directory>" >&2
  exit 2
}

kin_bin="${1-}"
scratch="${2-}"
if [ -z "$kin_bin" ] || [ -z "$scratch" ]; then
  usage
fi
if [ ! -x "$kin_bin" ]; then
  echo "::error::not an executable kin binary: $kin_bin"
  exit 1
fi

# Every assertion runs from inside a fixture directory, so both arguments have
# to survive the `cd`.
kin_bin="$(cd "$(dirname "$kin_bin")" && pwd)/$(basename "$kin_bin")"
mkdir -p "$scratch"
scratch="$(cd "$scratch" && pwd)"

# Where each store goes is this script's decision alone, made per fixture.
unset KIN_STORE KIN_STORE_LAYOUT KIN_REGISTRY_PATH

# `kin init` starts a daemon for each repository it admits. Reap them rather
# than leaving runner processes behind.
trap 'MSYS_NO_PATHCONV=1 taskkill.exe /F /IM kin-daemon.exe >/dev/null 2>&1 || true; rm -rf "$scratch"/*/kin-home/bin 2>/dev/null || true' EXIT

# What `kin init` prints when it takes a populated directory in. The colon is
# part of the wording: a Git import reports "Uncommitted worktree state:", which
# shares every character before it.
POPULATED_UNCOMMITTED="Uncommitted work:"

failures=0

fail() {
  local message="$1"
  local log="$2"
  failures=$((failures + 1))
  echo "::error::$message"
  echo "--- captured kin init output: $log ---"
  cat "$log" || true
  echo "--- end $log ---"
}

occurrences() {
  grep -Fc -- "$1" "$2" || true
}

require_text() {
  local label="$1"
  local needle="$2"
  local log="$3"
  if [ "$(occurrences "$needle" "$log")" -eq 0 ]; then
    fail "$label did not report: $needle" "$log"
  fi
}

# Deliberately not `find`: on a Windows runner that name also belongs to
# `System32\\find.exe`, whose arguments mean something else. An unmatched glob
# stays literal because `nullglob` is off, and the `-e` test rejects it.
count_matching() {
  local directory="$1"
  local pattern="$2"
  local matches=0
  local entry
  for entry in "$directory"/$pattern; do
    if [ -e "$entry" ]; then
      matches=$((matches + 1))
    fi
  done
  echo "$matches"
}

# Both admission paths stage a new store in a PARENT directory and rename it
# into place: an in-tree store beside its working tree, a central store beside
# itself in the stores directory. A count taken inside the admitted directory
# could never fail, so this counts in the tree's parent and in the fixture's
# stores directory.
require_no_stage_residue() {
  local label="$1"
  local dir="$2"
  local log="$3"
  local parent
  parent="$(dirname "$dir")"
  local stores
  stores="$(fixture_home "$dir")/stores"
  local staged
  staged="$(count_matching "$parent" '.kin.init-*')"
  if [ "$staged" != "0" ]; then
    fail "$label left $staged unpublished stage entries in $parent" "$log"
  fi
  staged="$(count_matching "$stores" '.kin.init-*')"
  if [ "$staged" != "0" ]; then
    fail "$label left $staged unpublished stage entries in $stores" "$log"
  fi
}

fixture_home() {
  printf '%s\n' "$(dirname "$1")/kin-home"
}

# Run a command inside one fixture, against that fixture's Kin home and store
# layout. A native Windows process is handed its Kin home in Windows form
# rather than trusting Git Bash to convert an environment value.
fixture_kin() {
  local dir="$1"
  local layout="$2"
  shift 2
  local home
  home="$(fixture_home "$dir")"
  if [ ! -x "$home/bin/kin.exe" ]; then
    mkdir -p "$home/bin"
    cp "$kin_bin" "$(dirname "$kin_bin")/kin-daemon.exe" "$home/bin/"
  fi
  if command -v cygpath > /dev/null 2>&1; then
    home="$(cygpath -w "$home")"
  fi
  (
    cd "$dir"
    export KIN_HOME="$home"
    if [ "$layout" = "in-tree" ]; then
      export KIN_STORE_LAYOUT=in-tree
    fi
    "$@"
  )
}

# The store Kin's own discovery names for a fixture, in a form this shell can
# test, or nothing when no store serves it. A Windows build may print the
# verbatim `\\?\C:\...` form, which `cygpath` does not read.
discovered_store() {
  local dir="$1"
  local layout="$2"
  local log="$3"
  local answer
  if ! answer="$(fixture_kin "$dir" "$layout" "$kin_bin" store path 2>> "$log")"; then
    return 0
  fi
  answer="$(printf '%s\n' "$answer" | tr -d '\r' | tail -n 1)"
  answer="${answer#'\\?\'}"
  if [ -n "$answer" ] && command -v cygpath > /dev/null 2>&1; then
    answer="$(cygpath -u "$answer")"
  fi
  printf '%s\n' "$answer"
}

make_git_fixture() {
  local dir="$1"
  mkdir -p "$dir"
  (
    cd "$dir"
    git init -q
    git config user.email ci@firelock.ai
    git config user.name ci
    printf 'fn probe() {}\n' > probe.rs
    git add probe.rs
    # Hooks and signing are neutralized for this invocation without persisting
    # a local compatibility blocker in the fixture.
    git -c core.hooksPath= -c commit.gpgsign=false commit -qm probe
  )
}

# Admit a fixture under the default (central) layout, or under `in-tree` when
# the fourth argument says so.
require_admitted() {
  local label="$1"
  local dir="$2"
  local log="$3"
  local layout="${4:-central}"
  local home
  local store
  home="$(fixture_home "$dir")"
  if ! fixture_kin "$dir" "$layout" "$kin_bin" init > "$log" 2>&1; then
    fail "$label failed" "$log"
  fi
  store="$(discovered_store "$dir" "$layout" "$log")"
  if [ -z "$store" ]; then
    fail "$label returned success but no store serves $dir" "$log"
  elif [ ! -f "$store/manifest.json" ]; then
    fail "$label returned success but the store serving $dir holds no manifest: $store" "$log"
  fi
  if [ "$layout" = "in-tree" ]; then
    if [ ! -d "$dir/.kin" ]; then
      fail "$label returned success without publishing $dir/.kin" "$log"
    elif [ -n "$store" ] && [ "$(basename "$store")" != ".kin" ]; then
      fail "$label is served by $store instead of $dir/.kin" "$log"
    fi
  else
    if [ -e "$dir/.kin" ]; then
      fail "$label wrote $dir/.kin although the default layout keeps the working tree clean" "$log"
    fi
    if [ "$(count_matching "$home/stores" '*/manifest.json')" != "1" ]; then
      fail "$label did not publish exactly one store in $home/stores" "$log"
    elif [ -n "$store" ] && [ ! -f "$home/stores/$(basename "$store")/manifest.json" ]; then
      fail "$label is served by $store instead of its store in $home/stores" "$log"
    fi
  fi
  require_no_stage_residue "$label" "$dir" "$log"
}

# Distinguish an admission result from a binary that never started. Windows
# gives the main thread a smaller default stack than Unix, which has regressed
# this command tree before.
startup_log="$scratch/kin-version.txt"
if ! "$kin_bin" --version > "$startup_log" 2>&1; then
  fail "kin --version did not run, so nothing below reports on admission" "$startup_log"
fi

git_boundary="$scratch/exact-git/repo"
git_log="$scratch/kin-init-exact-git.txt"
make_git_fixture "$git_boundary"
require_admitted "Windows exact-Git admission" "$git_boundary" "$git_log"

in_tree_boundary="$scratch/exact-git-in-tree/repo"
in_tree_log="$scratch/kin-init-exact-git-in-tree.txt"
make_git_fixture "$in_tree_boundary"
require_admitted "Windows in-tree exact-Git admission" "$in_tree_boundary" "$in_tree_log" in-tree

native_boundary="$scratch/native-unborn/repo"
native_log="$scratch/kin-init-native-unborn.txt"
mkdir -p "$native_boundary"
require_admitted "Windows native-unborn bootstrap" "$native_boundary" "$native_log"

# A directory that already holds something and has no Git history is admitted
# like the empty one, and the report has to say that its content is waiting as
# uncommitted work rather than pass over it in silence.
populated_boundary="$scratch/native-populated/repo"
populated_log="$scratch/kin-init-native-populated.txt"
mkdir -p "$populated_boundary"
printf 'untracked\n' > "$populated_boundary/stray.txt"
require_admitted "Windows native-populated bootstrap" "$populated_boundary" "$populated_log"
require_text "Windows native-populated bootstrap" "$POPULATED_UNCOMMITTED" "$populated_log"

if [ "$failures" -ne 0 ]; then
  echo "::error::Windows admission violated its shipped contract ($failures check(s) failed)"
  exit 1
fi
echo "Windows admitted exact Git, native-empty and native-populated repositories into central stores and exact Git into its own .kin, published complete authority without stage residue, and reported the populated directory's content as uncommitted work."
