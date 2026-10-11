#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
#
# Cargo target runner (see .cargo/config.toml). Cargo starts every executable
# it runs through this script: test binaries under `cargo test`, and the
# program under `cargo run`.
#
# A test binary gets the same environment nextest's setup script gives every
# test process (scripts/test-env/setup.sh): a fresh TMPDIR for the run, a
# private Kin registry with its store index and central stores, and relaxed
# drive flushes. Plain `cargo test` runs every test of a binary as threads of
# one process, so this environment has to be fixed before the binary starts.
# A test that set it partway through the run would move the registry under the
# stores other tests had already created. Without it, those tests would also
# write stores into the developer's real Kin home.
#
# Everything else is started unchanged, with its arguments, exit status and
# signals intact: an executable outside a `deps` directory, which is how
# `cargo run` and `cargo run --example` start, and any test binary nextest
# starts, because nextest has already run the setup script for its run.
set -u

case "${1:-}" in
    */deps/*) ;;
    *) exec "$@" ;;
esac
if [ -n "${NEXTEST:-}" ] || [ -n "${NEXTEST_RUN_ID:-}" ] || [ -n "${KIN_TEST_ENV_FLUSH:-}" ]; then
    exec "$@"
fi

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || exec "$@"
env_file=$(mktemp "${TMPDIR:-/tmp}/kin-test-env.XXXXXX") || exec "$@"
if ! NEXTEST_ENV="$env_file" sh "$here/setup.sh"; then
    rm -f "$env_file"
    exit 1
fi
while IFS= read -r line; do
    case "$line" in
        [A-Za-z_]*=*) export "$line" ;;
    esac
done < "$env_file"
rm -f "$env_file"
exec "$@"
