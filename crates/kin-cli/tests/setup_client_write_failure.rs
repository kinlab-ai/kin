// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Development setup refuses global client writes before inspecting or changing
//! client configuration. Release acceptance covers admitted Managed setup.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

mod common;

const SETUP_TIMEOUT: Duration = Duration::from_secs(120);

fn fixture(claude_config: &[u8]) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir().expect("setup root");
    let home = root.path().join("home");
    let cwd = root.path().join("outside-repository");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    std::fs::write(home.join(".claude.json"), claude_config).expect("seed Claude Code config");
    (root, home, cwd)
}

fn run_agent_setup(home: &Path, cwd: &Path) -> Output {
    common::Command::new(env!("CARGO_BIN_EXE_kin"))
        .args([
            "setup",
            "--no-interactive",
            "--verbose",
            "--intent",
            "agent",
            "--skip-mcp-check",
        ])
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

fn assert_refused_without_writes(output: &Output, home: &Path, original: &[u8]) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stderr.contains("development mode cannot"), "{stderr}");
    assert!(!stdout.contains("=== Health checklist ==="), "{stdout}");
    assert_eq!(std::fs::read(home.join(".claude.json")).unwrap(), original);
    for untouched in ["kin-home", ".kin", ".zshrc", ".zshenv", ".codex", ".cursor"] {
        assert!(!home.join(untouched).exists(), "setup wrote {untouched}");
    }
}

#[test]
fn development_setup_leaves_an_invalid_client_config_unchanged() {
    let original = b"{ this is not json\n";
    let (_root, home, cwd) = fixture(original);
    let output = run_agent_setup(&home, &cwd);
    assert_refused_without_writes(&output, &home, original);
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("not valid JSON"),
        "Development admission must refuse before a client write is attempted"
    );
}

#[test]
fn development_setup_also_refuses_a_writable_client_config() {
    let original = b"{\"mcpServers\": {}, \"keep\": true}\n";
    let (_root, home, cwd) = fixture(original);
    let output = run_agent_setup(&home, &cwd);
    assert_refused_without_writes(&output, &home, original);
}
