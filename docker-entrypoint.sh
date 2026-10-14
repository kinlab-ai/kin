#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
set -e

# Resolve the on-disk workspace that backs the daemon's Kin repo layout. Both
# storage modes need a real, writable `.kin/` tree: local mode keeps its graph
# objects there, and GCS mode still needs a valid repo layout on disk so
# sidecars and health checks never observe a half-initialized tree. Default to
# an in-image tmp path that a plain emptyDir volume can back identically in
# either mode; override with KIN_WORKSPACE_DIR (for example a volume mounted at
# /workspace on legacy deployments).
WORKSPACE_DIR="${KIN_WORKSPACE_DIR:-/tmp/kin-workspace}"

# Fail with an actionable message instead of dying on a bare mkdir when the
# resolved path is not writable for the container's runtime user — e.g. an
# image-root path such as /workspace with no volume mounted over it.
if ! mkdir -p "${WORKSPACE_DIR}/.kin" 2>/dev/null || [ ! -w "${WORKSPACE_DIR}/.kin" ]; then
    echo "[entrypoint] FATAL: workspace '${WORKSPACE_DIR}' is not writable for uid $(id -u)." >&2
    echo "[entrypoint] Mount a writable volume there, or set KIN_WORKSPACE_DIR to a writable path (an emptyDir-backed /tmp/kin-workspace is the default)." >&2
    exit 1
fi

KIN_DIR="$(cd "${WORKSPACE_DIR}/.kin" && pwd -P)"

activate_managed_pair() {
    runtime_home="$1"
    mkdir -p "${runtime_home}/bin" || return 1
    runtime_home="$(cd "${runtime_home}" && pwd -P)"
    cli="${runtime_home}/bin/kin"
    daemon="${runtime_home}/bin/kin-daemon"
    if [ -e "${cli}" ] || [ -L "${cli}" ] || [ -e "${daemon}" ] || [ -L "${daemon}" ]; then
        if [ ! -f "${cli}" ] || [ ! -x "${cli}" ] || [ ! -f "${daemon}" ] || [ ! -x "${daemon}" ]; then
            echo "[entrypoint] FATAL: canonical image pair is incomplete; existing files were preserved." >&2
            return 1
        fi
    else
        staging="$(mktemp -d "${runtime_home}/bin/.image-pair.XXXXXX")" || return 1
        if ! cp /usr/local/bin/kin "${staging}/kin" \
            || ! cp /usr/local/bin/kin-daemon "${staging}/kin-daemon" \
            || ! chmod 0755 "${staging}/kin" "${staging}/kin-daemon"; then
            rm -rf "${staging}"
            return 1
        fi
        if ! ln "${staging}/kin" "${cli}"; then
            rm -rf "${staging}"
            return 1
        fi
        if ! ln "${staging}/kin-daemon" "${daemon}"; then
            [ ! "${cli}" -ef "${staging}/kin" ] || rm -f "${cli}"
            rm -rf "${staging}"
            return 1
        fi
        rm -rf "${staging}"
    fi
    if [ "$(sha256sum < "${cli}")" != "$(sha256sum < /usr/local/bin/kin)" ] \
        || [ "$(sha256sum < "${daemon}")" != "$(sha256sum < /usr/local/bin/kin-daemon)" ]; then
        echo "[entrypoint] FATAL: canonical pair differs from image bytes; existing files were preserved." >&2
        return 1
    fi
    if ! KIN_HOME="${runtime_home}" KIN_RUNTIME_MODE=managed \
        "${cli}" runtime status --json >&2; then
        echo "[entrypoint] FATAL: canonical image pair is not ready in managed mode." >&2
        return 1
    fi
    printf '%s\n' "${runtime_home}"
}

SELECTED_RUNTIME_HOME="${KIN_HOME:-${HOME}/.kin}"
if [ "${KIN_STORAGE:-local}" = local ]; then
    SELECTED_RUNTIME_HOME="$(activate_managed_pair "${SELECTED_RUNTIME_HOME}")" || exit 1
fi

# Paths every layout version creates. HEAD, objects/ and kindb/graph.kndb were
# git-shaped artifacts of the pre-authority layout and are NOT produced by a
# current `kin init`, so testing for them was unconditionally true and turned the
# branch below into an unconditional wipe on every start. These four are present
# in both the pre-authority and the current layout, so an existing repository of
# either vintage is left alone and the daemon decides whether it can open it.
layout_is_materialized() {
    [ -f "${KIN_DIR}/manifest.json" ] \
        && [ -f "${KIN_DIR}/config.toml" ] \
        && [ -f "${KIN_DIR}/version" ] \
        && [ -d "${KIN_DIR}/kindb" ]
}

# Refuse to delete a tree that has a filesystem mounted inside it. The registry
# pod mounts its published-crates volume at .kin/packages, and `rm -rf` unlinks
# the volume's contents before it fails EBUSY on the mount point itself, so the
# destructive path loses data first and crashloops second.
kin_dir_contains_mount() {
    [ -r /proc/self/mountinfo ] || return 1
    while read -r _ _ _ _ mount_point _; do
        case "${mount_point}" in
            "${KIN_DIR}" | "${KIN_DIR}"/*) return 0 ;;
        esac
    done < /proc/self/mountinfo
    return 1
}

# Materialize a real Kin repo layout. Re-init only when the tree is missing or
# incomplete so an existing repo on a persistent volume is never clobbered.
if ! layout_is_materialized; then
    if kin_dir_contains_mount; then
        echo "[entrypoint] FATAL: '${KIN_DIR}' is not a materialized Kin layout, but a filesystem is mounted inside it." >&2
        echo "[entrypoint] Refusing to re-initialize, because that deletes the mounted volume's contents before it fails on the mount point." >&2
        echo "[entrypoint] Prepare the repository before anything is mounted under .kin, or mount that volume outside the repository." >&2
        exit 1
    fi
    if [ "${KIN_STORAGE:-local}" != local ]; then
        SELECTED_RUNTIME_HOME="$(activate_managed_pair "${SELECTED_RUNTIME_HOME}")" || exit 1
    fi
    rm -rf "${KIN_DIR}"
    KIN_HOME="${SELECTED_RUNTIME_HOME}" KIN_RUNTIME_MODE=managed \
        "${SELECTED_RUNTIME_HOME}/bin/kin" init "${WORKSPACE_DIR}"
fi

# Start kin-daemon. Let K8s handle restarts via restartPolicy. Using exec so the
# daemon is PID 1 and receives signals (SIGTERM) directly for a clean shutdown.
# kin-daemon parses --repo last-wins, so appending the resolved workspace keeps
# the prepared directory authoritative regardless of any --repo carried by the
# image CMD or pod args; configure the workspace via KIN_WORKSPACE_DIR instead.
echo "[entrypoint] Starting kin-daemon (storage=${KIN_STORAGE:-local}, workspace=${WORKSPACE_DIR})..."
if [ "${KIN_STORAGE:-local}" = local ]; then
    export KIN_HOME="${SELECTED_RUNTIME_HOME}" KIN_RUNTIME_MODE=managed
    exec "${SELECTED_RUNTIME_HOME}/bin/kin-daemon" "$@" --repo "${WORKSPACE_DIR}"
fi
exec /usr/local/bin/kin-daemon "$@" --repo "${WORKSPACE_DIR}"
