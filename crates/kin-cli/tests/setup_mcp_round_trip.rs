// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Development setup cannot write client registrations or claim an MCP round
//! trip. Admitted Managed transports are exercised by release acceptance.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

mod common;

const SETUP_TIMEOUT: Duration = Duration::from_secs(120);

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
    plain: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("setup root");
        let home = root.path().join("home");
        let repo = root.path().join("repo");
        let plain = root.path().join("outside-repository");
        for path in [&home, &repo, &plain] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(home.join(".claude.json"), b"{}").unwrap();
        std::fs::write(repo.join("sentinel.txt"), b"repository stays unchanged\n").unwrap();
        Self {
            _root: root,
            home,
            repo,
            plain,
        }
    }

    fn setup(&self, dir: &Path, extra: &[&str]) -> Output {
        common::Command::new(env!("CARGO_BIN_EXE_kin"))
            .args([
                "setup",
                "--no-interactive",
                "--verbose",
                "--intent",
                "agent",
            ])
            .args(extra)
            .current_dir(dir)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("KIN_HOME", self.home.join("kin-home"))
            .env("KIN_RUNTIME_MODE", "development")
            .env(
                "KIN_REGISTRY_PATH",
                self.home.join("kin-home/registry.toml"),
            )
            .env("KIN_VFS_DISABLE", "1")
            .env("KIN_NO_DAEMON", "1")
            .env("KIN_MCP_SCAN_ROOT", dir)
            .env_remove("KIN_STORE")
            .env_remove("KIN_STORE_LAYOUT")
            .env_remove("KIN_DIR")
            .env_remove("KIN_DAEMON_URL")
            .env_remove("KIN_MCP_REPO")
            .output_within(SETUP_TIMEOUT)
            .expect("run development setup")
    }

    fn assert_refused_without_writes(&self, output: &Output) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "stdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(stderr.contains("development mode cannot"), "{stderr}");
        assert!(!stdout.contains("the entry answered"), "{stdout}");
        assert!(!stdout.contains("the round trip failed at"), "{stdout}");
        assert!(!stdout.contains("Kin is set up on"), "{stdout}");
        assert_eq!(
            std::fs::read(self.home.join(".claude.json")).unwrap(),
            b"{}"
        );
        assert_eq!(
            std::fs::read(self.repo.join("sentinel.txt")).unwrap(),
            b"repository stays unchanged\n"
        );
        for untouched in [
            ".kin",
            ".zshrc",
            ".zshenv",
            ".codex",
            ".cursor",
            "kin-home/config",
            "kin-home/registry.toml",
            "kin-home/services",
        ] {
            assert!(
                !self.home.join(untouched).exists(),
                "setup wrote {untouched}"
            );
        }
        assert!(!self.repo.join(".kin").exists());
    }
}

#[test]
fn development_setup_cannot_write_a_client_entry_or_claim_a_round_trip() {
    let fixture = Fixture::new();
    fixture.assert_refused_without_writes(&fixture.setup(&fixture.repo, &[]));
    assert!(!fixture.home.join("kin-home").exists());
}

#[test]
fn development_setup_leaves_a_broken_selected_launcher_unchanged() {
    let fixture = Fixture::new();
    let bin = fixture.home.join("kin-home/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let launcher = bin.join(if cfg!(windows) { "kin.exe" } else { "kin" });
    let original = b"this is not a Kin binary\n";
    std::fs::write(&launcher, original).unwrap();
    fixture.assert_refused_without_writes(&fixture.setup(&fixture.repo, &[]));
    assert_eq!(std::fs::read(&launcher).unwrap(), original);
    assert_eq!(std::fs::read_dir(&bin).unwrap().count(), 1);
}

#[test]
fn skipping_the_mcp_check_cannot_bypass_development_setup_refusal() {
    let fixture = Fixture::new();
    fixture.assert_refused_without_writes(&fixture.setup(&fixture.repo, &["--skip-mcp-check"]));
    assert!(!fixture.home.join("kin-home").exists());
}

#[test]
fn running_outside_a_repository_cannot_bypass_development_setup_refusal() {
    let fixture = Fixture::new();
    fixture.assert_refused_without_writes(&fixture.setup(&fixture.plain, &[]));
    assert!(!fixture.home.join("kin-home").exists());
}
