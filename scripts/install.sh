#!/usr/bin/env sh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
#
# Kin installer — one command to install the full semantic development environment.
#
# Usage:
#   curl -fsSL https://get.kinlab.dev/install | sh
#
# Options (via env vars):
#   KIN_VERSION=X.Y.Z     Pin a specific version (default: latest)
#   KIN_HOME=~/.kin       Install directory (preferred; default: ~/.kin)
#   KIN_DIR=~/.kin        Install directory compatibility alias
#   KIN_NO_SETUP=1        Skip interactive setup after install
#   KIN_REGISTRY_REPAIR=1 Explicitly repair safe registry modes (files to 0600, and no
#                         group or world write on the Kin home)

set -eu

# Kin keeps its registry only in a directory no other user can write, and
# refuses one that its group or others can. Distributions that set
# USERGROUPS_ENAB, Ubuntu and Debian among them, give every login a umask of
# 0002, under which the `mkdir -p` below would create ~/.kin group writable and
# the registry check would then refuse the install it belongs to. Take group
# and world write out of this script's umask and leave the rest of it alone,
# so a stricter umask such as 077 still narrows what is created.
umask go-w

# ── Config ──────────────────────────────────────────────────────────────

KIN_DIR="${KIN_HOME:-${KIN_DIR:-$HOME/.kin}}"
case "$KIN_DIR" in
    /*) ;;
    *) KIN_DIR="$(pwd)/$KIN_DIR" ;;
esac
KIN_BIN="$KIN_DIR/bin"
KIN_LIB="$KIN_DIR/lib"
export KIN_HOME="$KIN_DIR"
export KIN_DIR="$KIN_DIR"
GITHUB_ORG="kinlab-ai"
GITHUB_REPO="kin"
# Override KIN_BASE_URL to install from a mirror or a local `file://` path
# (offline/airgapped installs and CI smoke tests).
BASE_URL="${KIN_BASE_URL:-https://github.com/$GITHUB_ORG/$GITHUB_REPO/releases}"

# ── Helpers ─────────────────────────────────────────────────────────────

info() { printf '  \033[36m→\033[0m %s\n' "$*"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
err()  { printf '  \033[31m✗\033[0m %s\n' "$*" >&2; }
bold() { printf '\033[1m%s\033[0m' "$*"; }
is_truthy() {
    normalized=$(printf '%s' "${1:-}" | tr '[:upper:]' '[:lower:]' | awk '{$1=$1};1')
    case "$normalized" in
        1|true|yes|on) return 0 ;;
        *) return 1 ;;
    esac
}

detect_os() {
    case "$(uname -s)" in
        Linux*)  echo "linux" ;;
        Darwin*) echo "macos" ;;
        CYGWIN*|MINGW*|MSYS*) echo "windows" ;;
        *) err "Unsupported OS: $(uname -s)"; exit 1 ;;
    esac
}

detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)  echo "x86_64" ;;
        aarch64|arm64) echo "aarch64" ;;
        *) err "Unsupported architecture: $(uname -m)"; exit 1 ;;
    esac
}

has_cmd() { command -v "$1" >/dev/null 2>&1; }

# Migration needs parsed official metadata and a parsed final identity. Refuse
# rather than infer those values from text when no JSON parser is available.
json_value() {
    json_file="$1"
    json_path="$2"
    if has_cmd python3; then
        python3 -c 'import json,sys
v=json.load(open(sys.argv[1]))
for k in sys.argv[2].split("."): v=v[k]
if not isinstance(v,(str,bool,int)): raise ValueError("expected scalar")
print(str(v).lower() if isinstance(v,bool) else v)' "$json_file" "$json_path"
    elif has_cmd node; then
        node -e 'let v=JSON.parse(require("node:fs").readFileSync(process.argv[1],"utf8")); for(const k of process.argv[2].split("."))v=v[k]; if(!["string","boolean","number"].includes(typeof v))throw Error("expected scalar"); process.stdout.write(String(v)+"\n")' "$json_file" "$json_path"
    elif has_cmd jq; then
        jq -r ".$json_path | if type == \"string\" or type == \"boolean\" or type == \"number\" then . else error(\"expected scalar\") end" "$json_file"
    else
        err "Migrating an older install requires python3, node, or jq to verify official release metadata and runtime identity."
        return 1
    fi
}

fetch_json() {
    if has_cmd curl; then
        curl -fsSL -H 'Accept: application/vnd.github+json' -H 'User-Agent: Kin-Installer' "$1" -o "$2"
    elif has_cmd wget; then
        wget -q --header='Accept: application/vnd.github+json' --header='User-Agent: Kin-Installer' "$1" -O "$2"
    else
        err "Neither curl nor wget found. Install one and retry."
        return 1
    fi
}

physical_runtime_path() (
    case "$1" in /*) ;; *) return 1 ;; esac
    runtime_parent=$(dirname "$1")
    runtime_name=$(basename "$1")
    runtime_parent=$(cd "$runtime_parent" && pwd -P) || return 1
    printf '%s/%s\n' "$runtime_parent" "$runtime_name"
)

verify_selected_runtime() {
    status_file="$1"
    for check in 'schema:kin.runtime-status.v1' 'ready:true' 'mode:managed' 'pair_matches:true' 'restart_needed:false'; do
        field="${check%%:*}"
        wanted="${check#*:}"
        [ "$(json_value "$status_file" "$field" 2>/dev/null)" = "$wanted" ] || return 1
    done
    reported_home=$(json_value "$status_file" kin_home 2>/dev/null) || return 1
    case "$reported_home" in /*) ;; *) return 1 ;; esac
    [ "$(cd "$reported_home" && pwd -P)" = "$MANAGED_ROOT" ] || return 1
    for component in cli daemon; do
        [ "$(json_value "$status_file" "$component.version" 2>/dev/null)" = "$VERSION" ] || return 1
        [ "$(json_value "$status_file" "$component.commit_sha" 2>/dev/null)" = "$SOURCE_SHA" ] || return 1
        [ "$(json_value "$status_file" "$component.target" 2>/dev/null)" = "$RUST_TARGET" ] || return 1
        [ "$(json_value "$status_file" "$component.source_dirty" 2>/dev/null)" = false ] || return 1
        json_value "$status_file" "$component.sha256" 2>/dev/null | grep -Eq '^[0-9a-fA-F]{64}$' || return 1
    done
    cli_profile=$(json_value "$status_file" cli.build_profile 2>/dev/null) || return 1
    [ -n "$cli_profile" ] && [ "$cli_profile" = "$(json_value "$status_file" daemon.build_profile 2>/dev/null)" ] || return 1
    [ "$(physical_runtime_path "$(json_value "$status_file" cli.path 2>/dev/null)")" = "$MANAGED_ROOT/bin/kin$BIN_EXT" ] || return 1
    [ "$(physical_runtime_path "$(json_value "$status_file" daemon.path 2>/dev/null)")" = "$MANAGED_ROOT/bin/kin-daemon$BIN_EXT" ] || return 1
}

INSTALL_STAGE=""
KEEP_STAGE=0
cleanup_stage() {
    if [ -n "$INSTALL_STAGE" ]; then
        if [ "$KEEP_STAGE" = 1 ]; then
            err "Migration is pending. Verified staging files were retained at $INSTALL_STAGE."
        else
            rm -rf "$INSTALL_STAGE"
        fi
    fi
}
trap cleanup_stage EXIT

# Succeeds when $KIN_DIR is a real directory the installing user owns that its
# group or other users can write. find does not follow a symlink here, so a
# link in that position never matches.
kin_dir_is_shared_write() {
    [ -n "$(find "$KIN_DIR" -prune -type d -user "$(id -u)" \( -perm -020 -o -perm -002 \) -print 2>/dev/null)" ]
}

# ── Detect platform ────────────────────────────────────────────────────

OS="$(detect_os)"
if [ "$OS" = windows ]; then
    err "Native Windows installs and updates require the official PowerShell installer."
    err "Open PowerShell and run: irm https://get.kinlab.dev/install.ps1 | iex"
    exit 1
fi
ARCH="$(detect_arch)"
TARGET="${OS}-${ARCH}"

# The POSIX installer consumes the macOS and Linux tar archives. PowerShell
# owns native Windows ZIP extraction and its synchronous update worker.
BIN_EXT=""

printf '\n'
printf '  \033[1;36mKin Installer\033[0m\n'
printf '  A graph-native code repository for AI agents and the people who build with them.\n'
printf '\n'

info "Platform: $OS ($ARCH)"

# ── Declare the one host tool Kin needs but does not install ──────────

# Installing Kin needs no git, and Kin reads a repository's Git history itself
# rather than shelling out for it. The host binary is still required in two
# places a first-run user reaches immediately: `kin setup` validates workspace
# MCP authority through git when it runs inside a Git repository, and adopting
# an existing project means committing it to Git first, which `kin init` asks
# for. Declaring that here beats letting the first real command fail on it.
if ! has_cmd git; then
    info 'git not found. Kin installs and runs without it, but `kin setup` inside a Git repository and adopting an existing project both require it.'
fi

# ── Existing installs use the native transactional updater ─────────────

PREVIOUS_VERSION=""
LEGACY_BOOTSTRAP=0
if [ -e "$KIN_BIN/kin$BIN_EXT" ] || [ -L "$KIN_BIN/kin$BIN_EXT" ] ||    [ -e "$KIN_BIN/kin-daemon$BIN_EXT" ] || [ -L "$KIN_BIN/kin-daemon$BIN_EXT" ]; then
    if [ ! -x "$KIN_BIN/kin$BIN_EXT" ] || [ -L "$KIN_BIN/kin$BIN_EXT" ]; then
        err "An incomplete or invalid managed install exists at $KIN_BIN. No component was replaced."
        exit 1
    fi
    if [ -n "${KIN_VERSION:-}" ] || [ -n "${KIN_BASE_URL:-}" ]; then
        err "Pinned or mirrored reinstalls cannot replace an existing managed runtime independently."
        err "Use kin update with its verified release pins, or select a separate development binary."
        exit 1
    fi
    # A typed refusal still identifies a current API whose updater can repair
    # the pair. An old release's unknown command must never authorize raw copies.
    RUNTIME_PROBE=$("$KIN_BIN/kin$BIN_EXT" runtime status --json 2>/dev/null) || true
    if printf '%s\n' "$RUNTIME_PROBE" | grep -Eq '"schema"[[:space:]]*:[[:space:]]*"kin\.runtime-status\.v1"'; then
        info "Updating the existing managed runtime through its native transaction..."
        "$KIN_BIN/kin$BIN_EXT" update --apply
        "$KIN_BIN/kin$BIN_EXT" runtime status --json
        if ! is_truthy "${KIN_NO_SETUP:-}"; then
            "$KIN_BIN/kin$BIN_EXT" setup --no-interactive
        fi
        ok "Managed runtime updated and verified"
        exit 0
    fi
    LEGACY_BOOTSTRAP=1
    ORIGINAL_KIN_HOME="$KIN_DIR"
    info "Older managed runtime detected. Staging an official release for native migration..."
fi

# ── Resolve version ────────────────────────────────────────────────────

if [ -n "${KIN_VERSION:-}" ]; then
    VERSION="$KIN_VERSION"
    info "Version: $VERSION (pinned)"
else
    info "Fetching latest version..."
    # Resolve the latest tag via the `releases/latest` REDIRECT
    # (Location -> .../releases/tag/vX.Y.Z) instead of api.github.com, whose
    # 60-requests/hour ANONYMOUS rate limit is trivially hit from a shared,
    # NAT'd, corporate, or CI IP and returns 403.
    LATEST_URL="https://github.com/$GITHUB_ORG/$GITHUB_REPO/releases/latest"
    if has_cmd curl; then
        RESOLVED=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$LATEST_URL")
    elif has_cmd wget; then
        RESOLVED=$(wget -q -S -O /dev/null "$LATEST_URL" 2>&1 | sed -n 's/.*[Ll]ocation: *//p' | tail -1)
    else
        err "Neither curl nor wget found. Install one and retry."
        exit 1
    fi
    VERSION=$(printf '%s' "$RESOLVED" | sed -n 's#.*/releases/tag/v\([^/[:space:]]*\).*#\1#p')

    if [ -z "$VERSION" ]; then
        err "Could not determine latest version. Set KIN_VERSION manually."
        exit 1
    fi
    info "Version: $VERSION (latest)"
fi

# ── Download ────────────────────────────────────────────────────────────

ARCHIVE="kin-${TARGET}.tar.gz"
URL="$BASE_URL/download/v${VERSION}/${ARCHIVE}"
CHECKSUM_URL="${URL}.sha256"

info "Downloading $ARCHIVE..."

if [ "$LEGACY_BOOTSTRAP" = 1 ]; then
    MANAGED_ROOT=$(cd "$KIN_DIR" && pwd -P)
    STAGE_PARENT=$(cd "${TMPDIR:-/tmp}" && pwd -P)
    case "$STAGE_PARENT/" in
        "$MANAGED_ROOT/"*) err "Migration staging must be outside the managed Kin home."; exit 1 ;;
    esac
fi
INSTALL_STAGE=$(mktemp -d "${TMPDIR:-/tmp}/kin-install.XXXXXXXX")
chmod 700 "$INSTALL_STAGE"
if [ "$LEGACY_BOOTSTRAP" = 1 ]; then
    if ! printf '%s\n' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'; then
        err "Official release metadata returned an invalid version. No component was replaced."
        exit 1
    fi
    RELEASE_VERSION="${VERSION%%+*}"
    case "$RELEASE_VERSION" in
        *-*) CHANNEL=alpha; PRERELEASE=true ;;
        *) CHANNEL=stable; PRERELEASE=false ;;
    esac
    case "$TARGET" in
        linux-x86_64) RUST_TARGET=x86_64-unknown-linux-musl ;;
        linux-aarch64) RUST_TARGET=aarch64-unknown-linux-musl ;;
        macos-x86_64) RUST_TARGET=x86_64-apple-darwin ;;
        macos-aarch64) RUST_TARGET=aarch64-apple-darwin ;;
        *) err "No supported native migration target for $TARGET."; exit 1 ;;
    esac
    fetch_json "https://api.github.com/repos/$GITHUB_ORG/$GITHUB_REPO/releases/tags/v${VERSION}" "$INSTALL_STAGE/release.json"
    if [ "$(json_value "$INSTALL_STAGE/release.json" tag_name)" != "v$VERSION" ] ||
       [ "$(json_value "$INSTALL_STAGE/release.json" draft)" != false ] ||
       [ "$(json_value "$INSTALL_STAGE/release.json" prerelease)" != "$PRERELEASE" ]; then
        err "Official release metadata does not admit the selected version and channel."
        exit 1
    fi
    fetch_json "https://api.github.com/repos/$GITHUB_ORG/$GITHUB_REPO/git/ref/tags/v${VERSION}" "$INSTALL_STAGE/tag-ref.json"
    if [ "$(json_value "$INSTALL_STAGE/tag-ref.json" ref)" != "refs/tags/v$VERSION" ]; then
        err "Official tag metadata did not identify the exact selected release tag."
        exit 1
    fi
    TAG_RECORD="$INSTALL_STAGE/tag-ref.json"
    TAG_DEPTH=0
    while :; do
        SOURCE_SHA=$(json_value "$TAG_RECORD" object.sha)
        SOURCE_TYPE=$(json_value "$TAG_RECORD" object.type)
        if ! printf '%s\n' "$SOURCE_SHA" | grep -Eq '^[0-9a-fA-F]{40}$'; then
            err "Official tag metadata did not resolve to a full source commit. No component was replaced."
            exit 1
        fi
        SOURCE_SHA=$(printf '%s' "$SOURCE_SHA" | tr '[:upper:]' '[:lower:]')
        case "$SOURCE_TYPE" in
            commit) break ;;
            tag)
                if [ "$TAG_DEPTH" -ge 5 ]; then
                    err "Official tag metadata exceeded the supported annotation depth."
                    exit 1
                fi
                TAG_DEPTH=$((TAG_DEPTH + 1))
                REQUESTED_TAG_SHA="$SOURCE_SHA"
                TAG_RECORD="$INSTALL_STAGE/tag-$TAG_DEPTH.json"
                fetch_json "https://api.github.com/repos/$GITHUB_ORG/$GITHUB_REPO/git/tags/$SOURCE_SHA" "$TAG_RECORD"
                if [ "$(json_value "$TAG_RECORD" sha | tr '[:upper:]' '[:lower:]')" != "$REQUESTED_TAG_SHA" ]; then
                    err "Official annotation metadata did not identify the requested tag object."
                    exit 1
                fi
                ;;
            *) err "Official release tag does not resolve to a commit."; exit 1 ;;
        esac
    done
fi

# Download the archive and its per-artifact checksum. Interactive terminals get
# the downloader's live byte/percent meter for the large archive; piped and CI
# installs stay quiet. The checksum fetch remains quiet in both modes. The
# release workflow publishes "<archive>.sha256" next to every archive (shasum
# -a 256 format: "<hash>  <filename>"). Both downloads must succeed.
if has_cmd curl; then
    if [ -t 2 ]; then
        curl -fL "$URL" -o "$INSTALL_STAGE/$ARCHIVE"
    else
        curl -fsSL "$URL" -o "$INSTALL_STAGE/$ARCHIVE"
    fi
    # Tolerate a failed checksum fetch here so the explicit, friendly error
    # below fires (instead of a bare curl error under `set -e`).
    curl -fsSL "$CHECKSUM_URL" -o "$INSTALL_STAGE/$ARCHIVE.sha256" 2>/dev/null || true
elif has_cmd wget; then
    if [ -t 2 ]; then
        wget "$URL" -O "$INSTALL_STAGE/$ARCHIVE"
    else
        wget -q "$URL" -O "$INSTALL_STAGE/$ARCHIVE"
    fi
    wget -q "$CHECKSUM_URL" -O "$INSTALL_STAGE/$ARCHIVE.sha256" 2>/dev/null || true
fi

# ── Verify checksum ───────────────────────────────────────────────────

if [ ! -s "$INSTALL_STAGE/$ARCHIVE.sha256" ]; then
    err "Could not download checksum file: $CHECKSUM_URL"
    err "Refusing to install an unverified download. Aborting."
    exit 1
fi

EXPECTED=$(cut -d' ' -f1 "$INSTALL_STAGE/$ARCHIVE.sha256")
if ! printf '%s\n' "$EXPECTED" | grep -Eq '^[0-9a-fA-F]{64}$'; then
    err "Checksum file was empty or malformed: $CHECKSUM_URL"
    err "Refusing to install an unverified download. Aborting."
    exit 1
fi
EXPECTED=$(printf '%s' "$EXPECTED" | tr '[:upper:]' '[:lower:]')

if has_cmd shasum; then
    ACTUAL=$(shasum -a 256 "$INSTALL_STAGE/$ARCHIVE" | cut -d' ' -f1)
elif has_cmd sha256sum; then
    ACTUAL=$(sha256sum "$INSTALL_STAGE/$ARCHIVE" | cut -d' ' -f1)
else
    err "No SHA-256 tool (shasum or sha256sum) found."
    err "Cannot verify the download integrity. Install one and retry. Aborting."
    exit 1
fi

if [ "$ACTUAL" != "$EXPECTED" ]; then
    err "SHA-256 checksum mismatch!"
    err "Expected: $EXPECTED"
    err "Got:      $ACTUAL"
    err "The download may be corrupted or tampered with. Aborting."
    exit 1
fi
ok "SHA-256 checksum verified"

# ── Extract ─────────────────────────────────────────────────────────────

info "Installing to $KIN_DIR..."

if [ "$LEGACY_BOOTSTRAP" = 0 ]; then
    mkdir -p "$KIN_BIN" "$KIN_LIB"
fi

# Everything under $KIN_DIR must end owned by the user doing the install. GNU
# tar restores the uid/gid recorded in the archive when it runs as root, and
# release archives are built by CI under an unrelated uid, so a root install
# otherwise lands foreign-owned binaries and a foreign-owned projection shim in
# the user's own home. Kin then refuses to verify the shim it just installed
# ("managed config is not owned by the current user") and `kin doctor` reports
# the install ledger STALE on a perfectly good install.
#
# --no-same-owner is the fix, but it is not universal: busybox tar spells it -o
# and rejects the long option. Fall back to a plain extraction and repair the
# ownership directly, which is the only case where the chown has anything to do.
if ! tar --no-same-owner -xzf "$INSTALL_STAGE/$ARCHIVE" -C "$INSTALL_STAGE" 2>/dev/null; then
    tar xzf "$INSTALL_STAGE/$ARCHIVE" -C "$INSTALL_STAGE"
fi

# Find the extracted directory (archive contains a subdirectory)
EXTRACT_DIR=$(find "$INSTALL_STAGE" -mindepth 1 -maxdepth 1 -type d -name "kin-*" | head -1)
if [ -z "$EXTRACT_DIR" ]; then
    EXTRACT_DIR="$INSTALL_STAGE"
fi

# Only root can be handed foreign ownership by tar, and only root can repair it.
# Every file below is moved, not copied, so fixing the extracted tree here is
# what makes the installed tree caller-owned.
if [ "$(id -u)" = "0" ]; then
    chown -R "$(id -u):$(id -g)" "$EXTRACT_DIR" 2>/dev/null || true
fi

# kin-daemon is mandatory — `kin status`/`kin search` and the MCP server all
# require it. Assert it is present in the extracted archive BEFORE moving
# anything, so a daemon-less archive (e.g. a stale build) aborts cleanly instead
# of leaving a half-installed, daemon-less environment.
if [ ! -f "$EXTRACT_DIR/kin-daemon$BIN_EXT" ]; then
    err "kin-daemon$BIN_EXT missing from the downloaded archive."
    err "kin status/search and the MCP server require it. Refusing a daemon-less install. Aborting."
    exit 1
fi

# The notification identity is part of every macOS release contract. Validate
# its minimal launchable shape before replacing any installed binary so a
# malformed archive cannot leave a new CLI paired with an old notifier bundle.
if [ "$OS" = "macos" ]; then
    NOTIFIER_ROOT="$EXTRACT_DIR/KinNotifier.app"
    NOTIFIER_EXEC="$NOTIFIER_ROOT/Contents/MacOS/KinNotifier"
    NOTIFIER_PLIST="$NOTIFIER_ROOT/Contents/Info.plist"
    if [ -L "$NOTIFIER_ROOT" ] || [ ! -d "$NOTIFIER_ROOT" ]; then
        err "KinNotifier.app missing or unsafe in the downloaded macOS archive."
        err "No installed binary or notification bundle was replaced."
        exit 1
    fi
    # `test -f/-d` follows intermediate symlink components, so leaf checks
    # alone would accept `Contents -> Payload`. Walk the complete extracted
    # tree without following links and admit only real directories and regular
    # files before any live path is changed. FIFOs, devices, sockets, and every
    # symlink (including a directory ancestor) fail this boundary.
    if ! NOTIFIER_UNSAFE_ENTRY=$(find "$NOTIFIER_ROOT" ! \( -type d -o -type f \) -print -quit 2>/dev/null); then
        err "KinNotifier.app could not be inspected safely in the downloaded macOS archive."
        err "No installed binary or notification bundle was replaced."
        exit 1
    fi
    if [ -n "$NOTIFIER_UNSAFE_ENTRY" ]; then
        err "KinNotifier.app contains a symlink or special entry: $NOTIFIER_UNSAFE_ENTRY"
        err "No installed binary or notification bundle was replaced."
        exit 1
    fi
    if [ -L "$NOTIFIER_EXEC" ] || [ ! -f "$NOTIFIER_EXEC" ] || [ ! -s "$NOTIFIER_EXEC" ] || [ ! -x "$NOTIFIER_EXEC" ]; then
        err "KinNotifier.app/Contents/MacOS/KinNotifier is missing, unsafe, empty, or not executable."
        err "No installed binary or notification bundle was replaced."
        exit 1
    fi
    if [ -L "$NOTIFIER_PLIST" ] || [ ! -f "$NOTIFIER_PLIST" ] || [ ! -s "$NOTIFIER_PLIST" ]; then
        err "KinNotifier.app/Contents/Info.plist is missing, unsafe, or empty."
        err "No installed binary or notification bundle was replaced."
        exit 1
    fi
fi

if [ "$LEGACY_BOOTSTRAP" = 1 ]; then
    if [ -L "$EXTRACT_DIR/kin$BIN_EXT" ] || [ ! -f "$EXTRACT_DIR/kin$BIN_EXT" ] ||
       [ -L "$EXTRACT_DIR/kin-daemon$BIN_EXT" ]; then
        err "The official archive contains unsafe native components. No component was replaced."
        exit 1
    fi
    chmod +x "$EXTRACT_DIR/kin$BIN_EXT"
    KEEP_STAGE=1
    BOOTSTRAP_EXIT=0
    (
        unset KIN_DIR KIN_DAEMON_BIN KIN_MANAGED_BIN KIN_BINARY_PATH KIN_MCP_KIN_BINARY KIN_DAEMON_URL
        KIN_HOME="$ORIGINAL_KIN_HOME"
        KIN_RUNTIME_MODE=managed
        export KIN_HOME KIN_RUNTIME_MODE
        cd "$EXTRACT_DIR"
        "$EXTRACT_DIR/kin$BIN_EXT" update --bootstrap-published \
            --channel "$CHANNEL" --expect-version "$VERSION" --expect-sha "$SOURCE_SHA" \
            --expect-archive-sha256 "$ACTUAL" --expect-target "$RUST_TARGET" \
            --expect-archive-name "$ARCHIVE" --apply
    ) || BOOTSTRAP_EXIT=$?
    if [ "$BOOTSTRAP_EXIT" -ne 0 ]; then
        err "Native migration refused or requires recovery. The installer performed no direct component replacement."
        exit "$BOOTSTRAP_EXIT"
    fi
    STATUS_EXIT=0
    (
        unset KIN_DIR KIN_DAEMON_BIN KIN_MANAGED_BIN KIN_BINARY_PATH KIN_MCP_KIN_BINARY KIN_DAEMON_URL
        KIN_HOME="$ORIGINAL_KIN_HOME"
        KIN_RUNTIME_MODE=managed
        export KIN_HOME KIN_RUNTIME_MODE
        "$KIN_BIN/kin$BIN_EXT" runtime status --json
    ) > "$INSTALL_STAGE/final-status.json" || STATUS_EXIT=$?
    if [ "$STATUS_EXIT" -ne 0 ] || ! verify_selected_runtime "$INSTALL_STAGE/final-status.json"; then
        err "Native migration has not produced the selected ready runtime. Completion remains pending."
        exit 1
    fi
    KEEP_STAGE=0
    if ! is_truthy "${KIN_NO_SETUP:-}"; then
        (
            unset KIN_DIR KIN_DAEMON_BIN KIN_MANAGED_BIN KIN_BINARY_PATH KIN_MCP_KIN_BINARY KIN_DAEMON_URL
            KIN_HOME="$ORIGINAL_KIN_HOME"
            KIN_RUNTIME_MODE=managed
            export KIN_HOME KIN_RUNTIME_MODE
            "$KIN_BIN/kin$BIN_EXT" setup --no-interactive
        )
    fi
    ok "Managed runtime migrated and verified"
    exit 0
fi

# The freshly downloaded binary owns the registry-authority contract used by
# `kin doctor` and `kin update`. Run its content-free check before replacing
# any installed binary. Unsafe existing state is never silently chmodded or
# overwritten; the operator must explicitly repair it and rerun the installer.
chmod +x "$EXTRACT_DIR/kin$BIN_EXT"
# Its report is shown only when the check refuses, so a clean install stays
# quiet and a refusal still names the paths it inspected.
REGISTRY_AUTHORITY_STATUS=0
# KIN_REGISTRY_REPAIR=1 is consent to a permission-only repair. Taking group
# and world write off the Kin home is one, and doing it here keeps that consent
# good for whichever release is being installed, including one whose own
# repair predates the case.
if is_truthy "${KIN_REGISTRY_REPAIR:-}" && kin_dir_is_shared_write; then
    chmod go-w "$KIN_DIR"
    ok "Removed group and world write from $KIN_DIR"
fi
if is_truthy "${KIN_REGISTRY_REPAIR:-}"; then
    REGISTRY_AUTHORITY_REPORT=$("$EXTRACT_DIR/kin$BIN_EXT" registry authority --fix --initialize 2>&1) || REGISTRY_AUTHORITY_STATUS=$?
else
    REGISTRY_AUTHORITY_REPORT=$("$EXTRACT_DIR/kin$BIN_EXT" registry authority --initialize 2>&1) || REGISTRY_AUTHORITY_STATUS=$?
fi
if [ "$REGISTRY_AUTHORITY_STATUS" -ne 0 ]; then
    printf '%s\n' "$REGISTRY_AUTHORITY_REPORT" >&2
    err "Unsafe local registry authority blocks installation."
    err "No installed binary or registry authority file was replaced."
    # The umask above keeps a fresh install out of this state, but a directory
    # left by an earlier install, or made by hand, keeps whatever mode it was
    # given. Group or world write on the Kin home itself is the one case with
    # a remedy this script can name exactly, so name it.
    if kin_dir_is_shared_write; then
        KIN_DIR_MODE=$(ls -ld "$KIN_DIR" | awk '{print $1}')
        KIN_DIR_QUOTED="'$(printf '%s' "$KIN_DIR" | sed "s/'/'\\\\''/g")'"
        err "$KIN_DIR can be written by its group or by other users ($KIN_DIR_MODE), and Kin keeps its registry only in a directory no other user can write."
        err "Remove that access, then rerun the installer:"
        err "    chmod go-w $KIN_DIR_QUOTED"
        err "Or rerun the installer with KIN_REGISTRY_REPAIR=1 to have Kin remove it."
    else
        err "Inspect the paths above. For permission-only drift, rerun with KIN_REGISTRY_REPAIR=1."
    fi
    exit 1
fi

# Move binaries. kin-daemon is mandatory (asserted above); kin-vfs is the
# optional filesystem-projection client.
HAVE_VFS=0
for bin in kin kin-daemon kin-vfs; do
    if [ -f "$EXTRACT_DIR/$bin$BIN_EXT" ]; then
        mv "$EXTRACT_DIR/$bin$BIN_EXT" "$KIN_BIN/$bin$BIN_EXT"
        chmod +x "$KIN_BIN/$bin$BIN_EXT"
        [ "$bin" = "kin-vfs" ] && HAVE_VFS=1
    fi
done

# Move the projection shim library if the archive bundled it (Linux .so /
# macOS .dylib). It is consumed by the shell hooks via $KIN_DIR/lib.
HAVE_SHIM=0
for lib in libkin_vfs_shim.so libkin_vfs_shim.dylib kin_vfs_shim.dll; do
    if [ -f "$EXTRACT_DIR/$lib" ]; then
        mv "$EXTRACT_DIR/$lib" "$KIN_LIB/$lib"
        HAVE_SHIM=1
    fi
done

# Move the notification bundle if the archive carries it (macOS only). macOS
# reads a notification's sender name, icon, and grouping from the posting
# process's bundle; without this, Kin's notifications are credited to Script
# Editor. Replaced whole rather than merged so a stale executable can never be
# left inside a newer bundle.
HAVE_NOTIFIER=0
if [ -d "$EXTRACT_DIR/KinNotifier.app" ]; then
    rm -rf "$KIN_LIB/KinNotifier.app"
    mv "$EXTRACT_DIR/KinNotifier.app" "$KIN_LIB/KinNotifier.app"
    chmod +x "$KIN_LIB/KinNotifier.app/Contents/MacOS/KinNotifier" 2>/dev/null || true
    HAVE_NOTIFIER=1
fi

"$KIN_BIN/kin$BIN_EXT" runtime status --json

ok "Binaries installed (kin, kin-daemon)"

if [ "$HAVE_NOTIFIER" = "1" ]; then
    # Registering with LaunchServices is what lets the notification daemon
    # validate the bundle; an unregistered app is refused outright. Authorization
    # itself is NOT requested here: an unanswered prompt is recorded as a
    # permanent denial, so it must be raised interactively by `kin setup`.
    LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
    [ -x "$LSREGISTER" ] && "$LSREGISTER" -f "$KIN_LIB/KinNotifier.app" >/dev/null 2>&1 || true
    ok "Notification identity installed (KinNotifier.app)"
elif [ "$OS" = "macos" ]; then
    # Every macOS release archive is supposed to carry the bundle. Without it
    # Kin still runs, but every notification is credited to Script Editor, and
    # that downgrade is invisible unless it is said out loud here.
    err "This archive carries no KinNotifier.app; notifications will post as Script Editor, not as Kin"
fi

if [ "$HAVE_VFS" = "1" ] && [ "$HAVE_SHIM" = "1" ]; then
    # The release archive can contain a projection built for a different libc
    # than the host. In particular, the static-musl Kin core runs on Alpine,
    # while today's kin-vfs and shim are glibc-linked. Test the installed CLI
    # before claiming the optional projection is usable, and do not leave a
    # command behind that the host loader cannot execute.
    if "$KIN_BIN/kin-vfs$BIN_EXT" --help >/dev/null 2>&1; then
        ok "Filesystem projection installed (kin-vfs + shim)"
    else
        rm -f "$KIN_BIN/kin-vfs$BIN_EXT" \
            "$KIN_LIB/libkin_vfs_shim.so" \
            "$KIN_LIB/libkin_vfs_shim.dylib" \
            "$KIN_LIB/kin_vfs_shim.dll"
        info "Filesystem projection is unavailable on this platform; core CLI and daemon are fully functional without it."
    fi
else
    # An install being upgraded may still carry the projection pair from a
    # release that bundled it. Leaving it puts a v0.7.17 kin-vfs on PATH beside
    # a daemon several releases newer, and the installer is the documented
    # recovery for exactly those installs, so it is the one that has to clear
    # them. `kin update` does the same for the release after this one: the
    # components stay in its managed list as optional, and an archive that does
    # not carry one removes the stale copy rather than keeping it.
    if [ -e "$KIN_BIN/kin-vfs$BIN_EXT" ] || [ -e "$KIN_LIB/libkin_vfs_shim.so" ] ||
        [ -e "$KIN_LIB/libkin_vfs_shim.dylib" ] || [ -e "$KIN_LIB/kin_vfs_shim.dll" ]; then
        rm -f "$KIN_BIN/kin-vfs$BIN_EXT" \
            "$KIN_LIB/libkin_vfs_shim.so" \
            "$KIN_LIB/libkin_vfs_shim.dylib" \
            "$KIN_LIB/kin_vfs_shim.dll"
        info "Removed the filesystem projection left by an earlier release; this archive does not carry one. The core CLI and daemon are fully functional without it."
    else
        info "Filesystem projection (kin-vfs) not bundled in this archive. The core CLI and daemon are fully functional without it."
    fi
fi

# ── PATH setup ──────────────────────────────────────────────────────────

add_to_path() (
    rc_file="$1"
    line="export PATH=\"$KIN_BIN:\$PATH\""

    if [ -f "$rc_file" ] && grep -F -q "$KIN_BIN" "$rc_file" 2>/dev/null; then
        return 0  # Already configured
    fi

    printf '\n# Kin\n%s\n' "$line" >> "$rc_file"
    ok "Added $KIN_BIN to PATH in $rc_file"
)

# The file a bash login shell will actually read in this home.
#
# bash runs the FIRST of .bash_profile, .bash_login and .profile that it can
# read and none of the rest, and .bashrc is not among them: a login bash reads
# .bashrc only when one of these sources it. When a home has none of the three
# there is nothing to append to, and .bash_profile is the file to create,
# because it is the one bash looks at first and the one that belongs to bash
# alone. .profile is shared with sh and every other POSIX shell, so the
# installer never conjures that one.
bash_login_rc() (
    for candidate in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
        if [ -f "$candidate" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    printf '%s\n' "$HOME/.bash_profile"
)

# bash's PATH line needs two homes. .bashrc serves an interactive non-login
# shell and nothing else, so a line written only there is invisible to
# `bash -lc`, to an ssh login and to a macOS Terminal tab, which opens a login
# shell by default. `kin setup` resolves the same three names in the same order
# and writes the same two files; the two must agree or one writes where the
# other never reads.
add_bash_to_path() (
    add_to_path "$HOME/.bashrc"
    login_rc="$(bash_login_rc)"
    if [ ! -f "$login_rc" ]; then
        # Kin is bringing this file into existence, so it owes the user the
        # pairing a bash home normally has. The interactivity guard is
        # deliberate: ~/.bashrc carries the projection hook, which activates the
        # VFS overlay on entry, and `bash -lc` is a login shell that is not
        # interactive.
        cat > "$login_rc" <<'KIN_BASH_PROFILE_EOF'
# Created by the Kin installer.
#
# A login bash reads .bash_profile, .bash_login or .profile, the first one only,
# and never .bashrc, so Kin's PATH line below has to live here. Sourcing
# ~/.bashrc keeps an interactive login shell equivalent to an interactive
# non-login one; the guard keeps it out of `bash -lc`, which is not interactive.
case $- in
    *i*) [ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc" ;;
esac
KIN_BASH_PROFILE_EOF
        ok "Created $login_rc, which is the file a bash login shell reads"
    fi
    add_to_path "$login_rc"
)

# Whether this home belongs to a bash user at all. An existing bash startup
# file or a bash $SHELL says yes; .profile alone does not, because nearly every
# Linux home has one whatever shell it runs.
uses_bash() {
    [ -f "$HOME/.bashrc" ] ||
        [ -f "$HOME/.bash_profile" ] ||
        [ -f "$HOME/.bash_login" ] ||
        [ "$(basename "${SHELL:-}")" = "bash" ]
}

# zsh reads .zshenv on every launch and .zshrc only when interactive, so the
# PATH line goes in .zshenv: a script, a Makefile, a launchd job or an agent
# shelling out through a non-interactive zsh could not find kin at all when it
# went in .zshrc, and .zshenv covers the interactive case as well.
case "$OS" in
    macos)
        if [ -f "$HOME/.zshrc" ] || [ -f "$HOME/.zshenv" ] || [ "$(basename "${SHELL:-}")" = "zsh" ]; then
            add_to_path "$HOME/.zshenv"
        fi
        if uses_bash; then
            add_bash_to_path
        fi
        ;;
    linux)
        if uses_bash; then
            add_bash_to_path
        fi
        if [ -f "$HOME/.zshrc" ] || [ -f "$HOME/.zshenv" ]; then
            add_to_path "$HOME/.zshenv"
        fi
        ;;
esac

# ── Verify ──────────────────────────────────────────────────────────────

export PATH="$KIN_BIN:$PATH"

if has_cmd "$KIN_BIN/kin$BIN_EXT"; then
    INSTALLED_VERSION=$("$KIN_BIN/kin$BIN_EXT" --version 2>/dev/null | awk '{print $2}')
    if [ -n "$PREVIOUS_VERSION" ] && [ -n "$INSTALLED_VERSION" ] && [ "$PREVIOUS_VERSION" != "$INSTALLED_VERSION" ]; then
        ok "kin upgraded: $PREVIOUS_VERSION → $INSTALLED_VERSION"
    else
        ok "kin ${INSTALLED_VERSION:-installed}"
    fi
else
    err "Installation failed: kin binary not found"
    exit 1
fi

# ── Run setup ───────────────────────────────────────────────────────────

if is_truthy "${KIN_NO_SETUP:-}"; then
    printf '\n'
    info "Skipping setup (KIN_NO_SETUP=1). Run 'kin setup' when ready."
else
    printf '\n'
    # Setup is a best-effort post-install convenience — the binaries are already
    # installed and verified above, so a non-zero exit from `kin setup` (e.g. no
    # agent clients to configure on a bare CI/Docker host) must NOT fail the
    # install under `set -e`.
    set +e
    # When piped (curl | sh), stdin is consumed by the pipe.
    # Reopen /dev/tty so the interactive wizard can read keyboard input.
    if [ -t 0 ]; then
        # Already in a TTY — run directly
        "$KIN_BIN/kin$BIN_EXT" setup
    elif [ -e /dev/tty ] && ( : < /dev/tty ) 2>/dev/null; then
        # Piped but a usable controlling TTY is available — read keyboard input
        # from it. The `( : < /dev/tty )` probe confirms the device can actually
        # be OPENED — on CI/Docker /dev/tty often exists but has no controlling
        # terminal, so opening it errors ("cannot open /dev/tty") and the bare
        # `[ -e /dev/tty ]` check is not enough. The probe runs in a SUBSHELL:
        # a redirection failure on the `:` special built-in exits its shell, so
        # in a bare brace group it would terminate the whole non-interactive
        # installer (POSIX dash) before the fallback below can run.
        "$KIN_BIN/kin$BIN_EXT" setup < /dev/tty
    else
        # No usable TTY (CI, Docker, piped without a controlling terminal) —
        # run non-interactive so the install never exits non-zero here.
        "$KIN_BIN/kin$BIN_EXT" setup --no-interactive
    fi
    set -e
fi

printf '\n'
ok "Done! Open a new terminal, then run 'kin init' in a Git repository."
printf '\n'
