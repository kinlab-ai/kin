// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Development setup never prints a successful wizard summary or writes global
//! setup state. Release acceptance grades the Managed short and verbose output.

use std::path::Path;
use std::process::Output;
use std::time::Duration;

mod common;

const SETUP_TIMEOUT: Duration = Duration::from_secs(120);

fn run_setup(home: &Path, cwd: &Path, extra: &[&str]) -> Output {
    common::Command::new(env!("CARGO_BIN_EXE_kin"))
        .args([
            "setup",
            "--no-interactive",
            "--intent",
            "agent",
            "--skip-mcp-check",
            "--shell",
            "zsh",
        ])
        .args(extra)
        .current_dir(cwd)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("KIN_HOME", home.join("kin-home"))
        .env("KIN_RUNTIME_MODE", "development")
        .env("KIN_VFS_DISABLE", "1")
        .env("KIN_NO_DAEMON", "1")
        .env("KIN_REGISTRY_PATH", home.join("kin-home/registry.toml"))
        .env_remove("KIN_DIR")
        .env_remove("KIN_DAEMON_URL")
        .env_remove("KIN_MCP_REPO")
        .output_within(SETUP_TIMEOUT)
        .expect("run development setup")
}

fn assert_development_refusal(output: &Output, home: &Path) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stderr.contains("development mode cannot"), "{stderr}");
    assert!(!stdout.contains("Kin is set up on"), "{stdout}");
    assert!(!stdout.contains("=== Health checklist ==="), "{stdout}");
    for untouched in [
        "kin-home",
        ".kin",
        ".zshrc",
        ".zshenv",
        ".codex",
        ".claude.json",
    ] {
        assert!(!home.join(untouched).exists(), "setup wrote {untouched}");
    }
}

#[test]
fn development_setup_refuses_both_short_and_verbose_wizard_runs() {
    let root = tempfile::tempdir().expect("setup root");
    let home = root.path().join("home");
    let cwd = root.path().join("outside-repository");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    for extra in [&[][..], &["--verbose"][..]] {
        assert_development_refusal(&run_setup(&home, &cwd, extra), &home);
    }
}

#[test]
fn a_legacy_mode_flag_cannot_enable_development_setup_and_help_remains_available() {
    let root = tempfile::tempdir().expect("setup root");
    let home = root.path().join("home");
    let cwd = root.path().join("outside-repository");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    assert_development_refusal(&run_setup(&home, &cwd, &["--mode", "native"]), &home);

    let output = common::Command::new(env!("CARGO_BIN_EXE_kin"))
        .args(["setup", "--help"])
        .current_dir(&cwd)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("KIN_HOME", home.join("kin-home"))
        .env("KIN_RUNTIME_MODE", "development")
        .env_remove("KIN_DIR")
        .output_within(SETUP_TIMEOUT)
        .expect("run setup help");
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!help.contains("--mode"), "{help}");
    assert!(!home.join("kin-home").exists());
}
