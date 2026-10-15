// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Development setup leaves shell profiles and client configurations untouched,
//! including profiles that Managed release acceptance exercises as write failures.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

mod common;

const SETUP_TIMEOUT: Duration = Duration::from_secs(120);

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    cwd: PathBuf,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().expect("setup root");
    let home = root.path().join("home");
    let cwd = root.path().join("outside-repository");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(home.join(".claude.json"), b"{}").unwrap();
    Fixture {
        _root: root,
        home,
        cwd,
    }
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
            "--shell",
            "zsh",
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

fn assert_development_refusal(output: &Output, home: &Path) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stderr.contains("development mode cannot"), "{stderr}");
    assert!(!stdout.contains("AI client MCP configuration:"), "{stdout}");
    assert_eq!(std::fs::read(home.join(".claude.json")).unwrap(), b"{}");
    for untouched in ["kin-home", ".kin", ".zshenv", ".codex", ".cursor"] {
        assert!(!home.join(untouched).exists(), "setup wrote {untouched}");
    }
}

#[cfg(unix)]
#[test]
fn development_setup_preserves_a_read_only_symlinked_shell_profile() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = fixture();
    let store = fixture.home.join("store");
    std::fs::create_dir(&store).unwrap();
    let managed = store.join("zshrc");
    let original = b"# managed shell profile\n";
    std::fs::write(&managed, original).unwrap();
    std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o444)).unwrap();
    let profile = fixture.home.join(".zshrc");
    std::os::unix::fs::symlink(&managed, &profile).unwrap();

    assert_development_refusal(&run_agent_setup(&fixture.home, &fixture.cwd), &fixture.home);
    assert_eq!(std::fs::read_link(&profile).unwrap(), managed);
    assert_eq!(std::fs::read(&managed).unwrap(), original);
    assert_eq!(
        std::fs::metadata(&managed).unwrap().permissions().mode() & 0o777,
        0o444
    );
}

#[test]
fn development_setup_preserves_an_unusable_shell_profile_directory() {
    let fixture = fixture();
    let profile = fixture.home.join(".zshrc");
    std::fs::create_dir(&profile).unwrap();
    assert_development_refusal(&run_agent_setup(&fixture.home, &fixture.cwd), &fixture.home);
    assert!(profile.is_dir());
    assert_eq!(std::fs::read_dir(&profile).unwrap().count(), 0);
}
