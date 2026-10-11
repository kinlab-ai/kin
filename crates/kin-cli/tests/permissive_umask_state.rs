// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Kin's state stays private to its user under a permissive umask.
//!
//! Ubuntu, Debian and every distribution that sets `USERGROUPS_ENAB` give a
//! login the umask 0002. Under it `kin setup` made `~/.kin`, its config and
//! shell files and the supervisor's records group writable, and the registry
//! then refused the directory it lives in, so setup reported a misconfigured
//! registry and still exited 0. These drive the real binary under that umask.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

mod common;

/// Wall-clock cap for one scripted `kin` run, which takes seconds.
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    cwd: PathBuf,
}

impl Fixture {
    fn kin_home(&self) -> PathBuf {
        self.home.join(".kin")
    }
}

/// An isolated home with no `~/.kin` yet, so every directory under it is one
/// Kin creates. With `claude_code`, Claude Code is detected and configured.
fn fixture(claude_code: bool) -> Fixture {
    let root = tempfile::tempdir().expect("fixture root");
    let home = root.path().join("home");
    let cwd = root.path().join("outside-repository");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    for directory in [root.path(), home.as_path()] {
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755))
            .expect("fix fixture mode");
    }
    if claude_code {
        std::fs::write(home.join(".claude.json"), b"{}").expect("seed Claude Code config");
    }
    Fixture {
        _root: root,
        home,
        cwd,
    }
}

/// Run `kin <args>` under umask 0002, the way a login on Ubuntu starts it.
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
        .env("KIN_VFS_DISABLE", "1")
        .env("KIN_NO_DAEMON", "1")
        .env_remove("KIN_DIR")
        .env_remove("KIN_REGISTRY_PATH")
        .env_remove("KIN_DAEMON_URL")
        .env_remove("KIN_MCP_REPO")
        .output_within(RUN_TIMEOUT)
        .expect("run kin")
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

/// Every directory and file below `root` that its group or others can write,
/// with its mode. Symlinks are skipped: their own mode means nothing.
fn shared_writable(root: &Path) -> Vec<(PathBuf, u32)> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path).expect("inspect created state");
        if metadata.file_type().is_symlink() {
            continue;
        }
        let mode = metadata.permissions().mode() & 0o7777;
        if mode & 0o022 != 0 {
            found.push((path.clone(), mode));
        }
        if metadata.is_dir() {
            for entry in std::fs::read_dir(&path).expect("list created state") {
                pending.push(entry.expect("directory entry").path());
            }
        }
    }
    found
}

fn transcript(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn setup_under_umask_0002_creates_nothing_group_writable() {
    let fixture = fixture(true);

    let output = setup(&fixture, &[]);
    assert!(output.status.success(), "{}", transcript(&output));
    let kin_home = fixture.kin_home();
    assert!(
        kin_home.join("config").join("setup.toml").is_file(),
        "setup wrote nothing under the Kin home, so this proves nothing:\n{}",
        transcript(&output)
    );

    let initialize =
        kin_under_usergroups_umask(&fixture, &["registry", "authority", "--initialize"]);
    assert!(initialize.status.success(), "{}", transcript(&initialize));
    assert!(kin_home.join("registry.toml").is_file());

    let mut exposed = shared_writable(&kin_home);
    for dotfile in [".zshenv", ".zshrc", ".claude.json"] {
        let path = fixture.home.join(dotfile);
        if path.exists() && path.metadata().unwrap().permissions().mode() & 0o022 != 0 {
            exposed.push((path.clone(), path.metadata().unwrap().permissions().mode()));
        }
    }
    assert!(
        exposed.is_empty(),
        "state Kin created under umask 0002 is group or world writable: {exposed:#?}"
    );
}

/// A Kin home an earlier install left group writable: setup fails and names
/// the directory and the exact repair, `kin doctor --fix` makes that repair
/// with consent, and setup then succeeds.
#[test]
fn setup_refuses_a_group_writable_kin_home_until_doctor_fix_repairs_it() {
    let fixture = fixture(false);
    let kin_home = fixture.kin_home();
    std::fs::create_dir_all(kin_home.join("bin")).expect("create the earlier install");
    std::fs::set_permissions(&kin_home, std::fs::Permissions::from_mode(0o775))
        .expect("leave it group writable");

    // The short summary and the full record each end the run themselves.
    for extra in [&[][..], &["--verbose"][..]] {
        let refused = setup(&fixture, extra);
        let text = transcript(&refused);
        assert!(
            !refused.status.success(),
            "setup {extra:?} must not exit 0 with a registry Kin refuses:\n{text}"
        );
        // The registry decides the exit itself. A client that fails on the same
        // directory also fails setup, and which clients a host detects is not
        // something this fixture controls, so the status alone proves too little.
        assert!(
            String::from_utf8_lossy(&refused.stderr)
                .contains("kin setup cannot use the local registry"),
            "setup {extra:?} must end on the registry refusal:\n{text}"
        );
        let remedy = format!("chmod go-w {}", kin_home.display());
        assert!(
            text.contains(&remedy),
            "the refusal must name the exact repair `{remedy}`:\n{text}"
        );
        assert!(
            !text.contains("move the unsafe object aside"),
            "a mode problem is not an unsafe object:\n{text}"
        );
        assert_eq!(
            std::fs::metadata(&kin_home).unwrap().permissions().mode() & 0o777,
            0o775,
            "setup never repairs without consent"
        );
    }

    let repaired = kin_under_usergroups_umask(&fixture, &["doctor", "--fix"]);
    assert_eq!(
        std::fs::metadata(&kin_home).unwrap().permissions().mode() & 0o777,
        0o755,
        "doctor --fix must remove group and world write:\n{}",
        transcript(&repaired)
    );

    let after = setup(&fixture, &[]);
    assert!(after.status.success(), "{}", transcript(&after));
}
