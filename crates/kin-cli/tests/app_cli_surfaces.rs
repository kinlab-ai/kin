// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The machine surfaces a program drives Kin through, run as the real binary
//! in an isolated home: `kin update --check-only --json` answers with one JSON
//! object even when there is nothing to check. Development shell-only setup
//! refuses all global writes; release acceptance grades its Managed behavior.

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
        .env("KIN_HOME", home.join("kin-home"))
        .env("KIN_RUNTIME_MODE", "development")
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
    let kin_home = home.join("kin-home");
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

fn assert_development_shell_refusal(output: &Output, home: &Path) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stderr.contains("development mode cannot"), "{stderr}");
    assert!(
        stdout.trim().is_empty(),
        "refused setup must not report an install: {stdout}"
    );
    for untouched in [
        "kin-home",
        ".kin",
        ".zshenv",
        ".cursor",
        ".codex",
        ".gemini",
        ".codeium",
        ".lmstudio",
        ".grok",
        "registry.toml",
    ] {
        assert!(!home.join(untouched).exists(), "setup wrote {untouched}");
    }
}

#[test]
fn development_shell_only_setup_writes_neither_profiles_nor_client_entries() {
    let (_root, home, cwd) = fixture();
    let original = b"{\"mcpServers\": {}}\n";
    std::fs::write(home.join(".claude.json"), original).unwrap();
    for args in [
        vec!["setup", "--shell-only", "--shell", "zsh", "--json"],
        vec!["setup", "--shell-only", "--shell", "zsh"],
    ] {
        let output = kin(&home, &cwd, &args);
        assert_development_shell_refusal(&output, &home);
        assert!(!home.join(".zshrc").exists());
        assert_eq!(std::fs::read(home.join(".claude.json")).unwrap(), original);
    }
}

#[cfg(unix)]
#[test]
fn development_shell_only_setup_does_not_attempt_to_repair_a_blocked_profile() {
    let (_root, home, cwd) = fixture();
    let profile = home.join(".zshrc");
    std::fs::create_dir(&profile).unwrap();
    let output = kin(
        &home,
        &cwd,
        &["setup", "--shell-only", "--shell", "zsh", "--json"],
    );
    assert_development_shell_refusal(&output, &home);
    assert!(profile.is_dir());
    assert_eq!(std::fs::read_dir(&profile).unwrap().count(), 0);
    assert!(!home.join(".claude.json").exists());
}
