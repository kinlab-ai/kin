#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
#
# nextest setup script. It hands every test process its settings through
# NEXTEST_ENV. A switch with a value it does not know stops the run; anything
# else it cannot do leaves the test environment as it was.
#
# A fresh TMPDIR for each run. `kin init` stages a store beside its working
# tree and scans that parent directory for abandoned stages, and most fixtures
# put their working tree directly under TMPDIR, so every init lists TMPDIR.
# Fixtures that leak their stores make that listing grow run after run: a
# developer TMPDIR holding 236,000 leaked entries took 0.87 s to list and made
# a 0.14 s daemon fixture cost 1.2 to 1.5 s. Each run now gets its own directory
# under /tmp/kin-nextest-<uid>, and runs older than twelve hours are removed.
# KIN_TEST_SHARED_TMPDIR=1 keeps the inherited TMPDIR and places the run
# registry beside those fixtures, on the same filesystem.
#
# Relaxed drive flushes. KIN_TEST_FLUSH=fsync tells debug builds of Kin, and
# the kin and kin-daemon processes that tests start, to flush with a plain
# fsync instead of the standard library's F_FULLFSYNC on Apple platforms (see
# crates/kin-blobs, module durable). F_FULLFSYNC empties the whole drive cache and costs
# about 5 ms a file on an Apple SSD, so parallel store-heavy tests queue behind
# each other's flushes; no test can observe the difference. Other platforms
# already flush with fsync, so the setting changes nothing there.
# KIN_TEST_FULL_FLUSH=1 sets KIN_TEST_FLUSH=full instead. KIN_TEST_ENV_FLUSH
# records the effective mode, relaxed or full, for run receipts.
#
# A private registry for the run. `kin init` keeps a new store in the central
# stores directory beside the registry and records it in the store index
# there, so a fixture that does not pin its own KIN_REGISTRY_PATH would
# otherwise create stores and index entries in the developer's real Kin home,
# and leave them behind when its working tree is deleted. Each run gets
# KIN_REGISTRY_PATH=<run>/kin-home/registry.toml, which moves the registry,
# the store index and the central stores together. KIN_HOME is left alone so
# installed tools and model caches resolve as before.
set -u

[ -n "${NEXTEST_ENV:-}" ] || exit 0

# Both switches accept only unset, 0 or 1, so a typo cannot silently measure
# the wrong configuration.
check_switch() {
    case "$2" in
        "" | 0 | 1) ;;
        *)
            echo "test-env: $1 must be unset, 0 or 1, not '$2'" >&2
            exit 1
            ;;
    esac
}
check_switch KIN_TEST_FULL_FLUSH "${KIN_TEST_FULL_FLUSH-}"
check_switch KIN_TEST_SHARED_TMPDIR "${KIN_TEST_SHARED_TMPDIR-}"

# Default to the short /tmp path: macOS's per-user TMPDIR is already about
# 50 characters long, and Unix socket paths have a 104-byte limit. A caller
# preserving its own TMPDIR also chooses that path budget. Keep its central
# stores on that filesystem: exact projection publication moves staged source
# from the store into the working tree, which cannot rename across devices.
run_parent="/tmp"
if [ "${KIN_TEST_SHARED_TMPDIR:-0}" = "1" ]; then
    run_parent="${TMPDIR:-/tmp}"
fi
root="${run_parent%/}/kin-nextest-$(id -u)"
run_dir=""
if mkdir -p -m 700 "$root" 2>/dev/null && [ -O "$root" ]; then
    find "$root" -mindepth 1 -maxdepth 1 -type d -mmin +720 -exec rm -rf {} + 2>/dev/null
    run_dir=$(mktemp -d "$root/r.XXXXXX" 2>/dev/null) || run_dir=""
fi
if [ -n "$run_dir" ]; then
    if [ "${KIN_TEST_SHARED_TMPDIR:-0}" != "1" ]; then
        echo "TMPDIR=$run_dir/" >> "$NEXTEST_ENV"
    fi
    if mkdir -p -m 700 "$run_dir/kin-home"; then
        echo "KIN_REGISTRY_PATH=$run_dir/kin-home/registry.toml" >> "$NEXTEST_ENV"
    fi
fi

if [ "${KIN_TEST_FULL_FLUSH:-0}" = "1" ]; then
    echo "KIN_TEST_FLUSH=full" >> "$NEXTEST_ENV"
    echo "KIN_TEST_ENV_FLUSH=full" >> "$NEXTEST_ENV"
else
    echo "KIN_TEST_FLUSH=fsync" >> "$NEXTEST_ENV"
    echo "KIN_TEST_ENV_FLUSH=relaxed" >> "$NEXTEST_ENV"
fi
