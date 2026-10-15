// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Editor intent cannot bypass Development's refusal of global setup writes.
//! Managed release acceptance checks installed and missing extension guidance.

use std::path::Path;
use std::time::Duration;

mod common;

const SETUP_TIMEOUT: Duration = Duration::from_secs(120);

#[test]
fn development_editor_setup_refuses_with_or_without_an_installed_extension() {
    for installed in [false, true] {
        let root = tempfile::tempdir().expect("editor setup root");
        let home = root.path().join("home");
        let cwd = root.path().join("outside-repository");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let manifest = home.join(".vscode/extensions/firelock.kin-editor-test/package.json");
        let original =
            b"{\"publisher\":\"firelock\",\"name\":\"kin-editor\",\"version\":\"0.0.0-test\"}";
        if installed {
            std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
            std::fs::write(&manifest, original).unwrap();
        }
        run_editor_setup(&home, &cwd);
        if installed {
            assert_eq!(std::fs::read(&manifest).unwrap(), original);
        } else {
            assert!(!home.join(".vscode").exists());
        }
    }
}

fn run_editor_setup(home: &Path, cwd: &Path) {
    let output = common::Command::new(env!("CARGO_BIN_EXE_kin"))
        .args([
            "setup",
            "--intent",
            "editor",
            "--no-interactive",
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
        .expect("run development editor setup");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stderr.contains("development mode cannot"), "{stderr}");
    assert!(!stdout.contains("Kin is set up on"), "{stdout}");
    for untouched in [
        "kin-home",
        ".kin",
        ".zshrc",
        ".zshenv",
        ".claude.json",
        ".codex",
        ".cursor",
    ] {
        assert!(!home.join(untouched).exists(), "setup wrote {untouched}");
    }
}
