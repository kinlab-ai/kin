#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

# A runner can briefly lose DNS or the download connection during setup. Keep
# rustup's arguments and exit status, and retry only those transient failures.
set -euo pipefail

if [ "$#" -eq 0 ] || [ -z "$1" ]; then
  echo "toolchain argument is required" >&2
  exit 2
fi

log=$(mktemp "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/kin-rustup-install.XXXXXX")
trap 'rm -f "$log"' EXIT

attempt=1
while :; do
  set +e
  rustup toolchain install "$@" 2>&1 | tee "$log"
  statuses=("${PIPESTATUS[@]}")
  set -e
  status=${statuses[0]}
  if [ "${statuses[1]}" -ne 0 ]; then
    exit "${statuses[1]}"
  fi
  if [ "$status" -eq 0 ]; then
    exit 0
  fi

  # Never retry verification failures or an unavailable toolchain/component,
  # even if another line in the same attempt also mentions a network problem.
  if grep -Eiq 'checksum failed|checksum mismatch|signature verification failed|certificate|invalid peer|tls error|tls handshake|ssl connect error|permission denied|access is denied|no space left on device|read-only file system|component .* (is |was )?unavailable|could not find .*toolchain|no release found|404 Not Found|status code: 404' "$log"; then
    exit "$status"
  fi
  if ! grep -Eiq 'Temporary failure in name resolution|failed to lookup address information|Could not resolve host|dns error|connection (reset|refused)|ConnectionReset|ConnectionRefused|timed out|operation timeout|status code: (502|503|504)|HTTP[^[:cntrl:]]* (502|503|504)' "$log"; then
    exit "$status"
  fi
  if [ "$attempt" -ge 3 ]; then
    exit "$status"
  fi

  delay=$((attempt * 5))
  echo "rustup download failed transiently; retrying in ${delay}s (attempt $((attempt + 1))/3)" >&2
  sleep "$delay"
  attempt=$((attempt + 1))
done
