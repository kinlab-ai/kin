// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The machine surfaces a program drives Kin through, run as the real binary
//! in an isolated home: `kin update --check-only --json` answers with one JSON
//! object even when there is nothing to check, and `kin setup --shell-only`
//! adds kin to the shell and connects no AI client.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

mod common;

const RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// An isolated home and a directory outside any repository to run from.
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir().expect("fixture root");
    let home = root.path().join("home");
    let cwd = root.path().join("outside-repository");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    (root, home, cwd)
}

fn kin(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    common::Command::new(env!("CARGO_BIN_EXE_kin"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("KIN_HOME", home.join(".kin"))
        .env("KIN_VFS_DISABLE", "1")
        .env("KIN_NO_DAEMON", "1")
        .env("KIN_REGISTRY_PATH", home.join("registry.toml"))
        .env_remove("KIN_DIR")
        .env_remove("KIN_DAEMON_URL")
        .env_remove("KIN_MCP_REPO")
        .output_within(RUN_TIMEOUT)
        .unwrap_or_else(|error| panic!("run kin {args:?}: {error}"))
}

/// Stdout parsed as exactly one JSON object, with nothing before or after it.
fn one_json_object(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|error| {
        panic!(
            "stdout is not one JSON object ({error}):\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(
        value.is_object(),
        "stdout is JSON but not an object: {value}"
    );
    value
}

#[test]
fn update_check_only_json_reports_a_missing_install_root_as_json() {
    let (_root, home, cwd) = fixture();
    let kin_home = home.join(".kin");
    assert!(!kin_home.exists());

    let output = kin(&home, &cwd, &["update", "--check-only", "--json"]);
    let report = one_json_object(&output);

    assert_eq!(
        output.status.code(),
        Some(1),
        "a check with no verdict exits 1: {report}"
    );
    assert_eq!(report["schema"], "kin.update-check.v1");
    assert_eq!(report["current_version"], env!("CARGO_PKG_VERSION"));
    assert!(report["latest_version"].is_null(), "{report}");
    assert_eq!(report["update_available"], false);
    assert_eq!(report["reason"], "install_root_missing");
    // A test binary in a Cargo target directory is no install method Kin
    // knows, and saying so is the honest answer.
    assert_eq!(report["install_method"], "unknown");
    let message = report["message"].as_str().expect("a message");
    assert!(message.contains("installer"), "{message}");
    assert!(
        !kin_home.exists(),
        "a read-only check must not create the install root"
    );
}

#[test]
fn setup_shell_only_adds_kin_to_the_shell_and_connects_no_client() {
    let (_root, home, cwd) = fixture();
    let kin_home = home.join(".kin");
    // The layout that puts kin's bin directory on PATH.
    std::fs::create_dir_all(kin_home.join("bin")).expect("create the managed bin directory");
    // Claude Code's own install evidence, so setup detects a client it would
    // otherwise connect.
    let claude_config = b"{\"mcpServers\": {}}\n";
    std::fs::write(home.join(".claude.json"), claude_config).expect("seed Claude Code config");

    let output = kin(
        &home,
        &cwd,
        &["setup", "--shell-only", "--shell", "zsh", "--json"],
    );
    let report = one_json_object(&output);
    assert!(output.status.success(), "{report}");
    assert_eq!(report["schema"], "kin.setup-shell.v1");
    assert_eq!(report["shell"], "zsh");
    assert_eq!(report["status"], "installed");
    assert_eq!(report["clients_connected"], serde_json::json!([]));
    assert!(report["reason"].is_null(), "{report}");
    assert_eq!(report["add_by_hand"], serde_json::json!([]));

    let zshrc = std::fs::read_to_string(home.join(".zshrc")).expect("read .zshrc");
    assert!(zshrc.contains("kin-vfs"), "the hook is sourced:\n{zshrc}");
    let zshenv = std::fs::read_to_string(home.join(".zshenv")).expect("read .zshenv");
    assert!(
        zshenv.contains(&kin_home.join("bin").display().to_string()),
        "kin's bin directory is on PATH:\n{zshenv}"
    );

    // No AI client was touched.
    assert_eq!(
        std::fs::read(home.join(".claude.json")).expect("read Claude Code config"),
        claude_config,
        "the client's config must be left exactly as it was"
    );
    for client_config in [
        ".cursor/mcp.json",
        ".codex/config.toml",
        ".gemini/settings.json",
        ".codeium/windsurf/mcp_config.json",
        ".lmstudio/mcp.json",
        ".grok/config.toml",
    ] {
        assert!(
            !home.join(client_config).exists(),
            "--shell-only wrote {client_config}"
        );
    }

    // Recorded like any setup's shell step, so doctor and uninstall find it.
    let ledger = std::fs::read_to_string(kin_home.join("config").join("setup-ledger.json"))
        .expect("read the install ledger");
    assert!(ledger.contains("zsh"), "{ledger}");

    // Again, for a person: the same step, already in place, said in words.
    let again = kin(&home, &cwd, &["setup", "--shell-only", "--shell", "zsh"]);
    let stdout = String::from_utf8_lossy(&again.stdout);
    assert!(again.status.success(), "{stdout}");
    assert!(stdout.contains("No AI client was connected"), "{stdout}");
    assert!(stdout.contains("Open a new terminal"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(home.join(".zshrc")).expect("reread .zshrc"),
        zshrc,
        "a second run must not add the hook twice"
    );
}

#[cfg(unix)]
#[test]
fn setup_shell_only_json_names_what_to_add_by_hand_when_the_profile_refuses() {
    let (_root, home, cwd) = fixture();
    // A directory where the profile should be, so no write can land.
    std::fs::create_dir_all(home.join(".zshrc")).expect("block .zshrc");

    let output = kin(
        &home,
        &cwd,
        &["setup", "--shell-only", "--shell", "zsh", "--json"],
    );
    let report = one_json_object(&output);
    assert_eq!(output.status.code(), Some(1), "{report}");
    assert_eq!(report["schema"], "kin.setup-shell.v1");
    assert_eq!(report["status"], "not_written");
    assert!(report["reason"]
        .as_str()
        .is_some_and(|reason| !reason.is_empty()));
    assert_eq!(report["clients_connected"], serde_json::json!([]));
    assert!(report["next"].is_null(), "{report}");
}
