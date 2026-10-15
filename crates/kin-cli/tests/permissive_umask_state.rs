// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A permissive login umask cannot authorize Development to write or repair
//! global setup state. Release acceptance grades Managed-created state modes.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;
use std::time::Duration;

mod common;

const RUN_TIMEOUT: Duration = Duration::from_secs(120);

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    cwd: PathBuf,
}

impl Fixture {
    fn kin_home(&self) -> PathBuf {
        self.home.join("kin-home")
    }
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().expect("fixture root");
    let home = root.path().join("home");
    let cwd = root.path().join("outside-repository");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    for directory in [root.path(), home.as_path()] {
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(home.join(".claude.json"), b"{}").unwrap();
    Fixture {
        _root: root,
        home,
        cwd,
    }
}

fn kin_under_usergroups_umask(fixture: &Fixture, args: &[&str]) -> Output {
    let mut argv = vec![
        "-c".to_string(),
        "umask 0002 && exec \"$0\" \"$@\"".to_string(),
        env!("CARGO_BIN_EXE_kin").to_string(),
    ];
    argv.extend(args.iter().map(|arg| arg.to_string()));
    common::Command::new("/bin/sh")
        .args(&argv)
        .current_dir(&fixture.cwd)
        .env("HOME", &fixture.home)
        .env("USERPROFILE", &fixture.home)
        .env("KIN_HOME", fixture.kin_home())
        .env("KIN_RUNTIME_MODE", "development")
        .env("KIN_VFS_DISABLE", "1")
        .env("KIN_NO_DAEMON", "1")
        .env_remove("KIN_DIR")
        .env_remove("KIN_REGISTRY_PATH")
        .env_remove("KIN_DAEMON_URL")
        .env_remove("KIN_MCP_REPO")
        .output_within(RUN_TIMEOUT)
        .expect("run development kin under umask 0002")
}

fn setup(fixture: &Fixture, extra: &[&str]) -> Output {
    let mut args = vec![
        "setup",
        "--no-interactive",
        "--intent",
        "agent",
        "--skip-mcp-check",
        "--shell",
        "zsh",
    ];
    args.extend_from_slice(extra);
    kin_under_usergroups_umask(fixture, &args)
}

fn assert_development_refusal(fixture: &Fixture, output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stderr.contains("development mode cannot"), "{stderr}");
    assert_eq!(
        std::fs::read(fixture.home.join(".claude.json")).unwrap(),
        b"{}"
    );
    for untouched in [".kin", ".zshrc", ".zshenv", ".codex", ".cursor"] {
        assert!(
            !fixture.home.join(untouched).exists(),
            "setup wrote {untouched}"
        );
    }
}

#[test]
fn development_setup_under_umask_0002_creates_no_global_state() {
    let fixture = fixture();
    assert_development_refusal(&fixture, &setup(&fixture, &[]));
    assert!(!fixture.kin_home().exists());
}

#[test]
fn development_setup_and_doctor_fix_leave_a_group_writable_home_unchanged() {
    let fixture = fixture();
    let kin_home = fixture.kin_home();
    let bin = kin_home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::set_permissions(&kin_home, std::fs::Permissions::from_mode(0o775)).unwrap();
    for extra in [&[][..], &["--verbose"][..]] {
        assert_development_refusal(&fixture, &setup(&fixture, extra));
        assert_eq!(
            std::fs::metadata(&kin_home).unwrap().permissions().mode() & 0o777,
            0o775
        );
    }
    assert_development_refusal(
        &fixture,
        &kin_under_usergroups_umask(&fixture, &["doctor", "--fix"]),
    );
    assert_eq!(
        std::fs::metadata(&kin_home).unwrap().permissions().mode() & 0o777,
        0o775
    );
    assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(&kin_home).unwrap().count(), 1);
}
