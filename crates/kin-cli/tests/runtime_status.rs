// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The status contract is exercised across the real CLI process boundary.
#![cfg(unix)]

use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const SHA: &str = "1111111111111111111111111111111111111111";

struct Fixture {
    root: tempfile::TempDir,
    home: PathBuf,
    working: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("managed");
        let working = root.path().join("repository");
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::create_dir_all(&working).unwrap();
        Self {
            root,
            home,
            working,
        }
    }

    fn pair(&self, daemon_sha: &str, dirty: bool, known: bool) {
        self.cli(dirty, known);
        self.daemon(daemon_sha, dirty, known, "aarch64-apple-darwin", "release");
    }

    fn daemon(&self, sha: &str, dirty: bool, known: bool, target: &str, profile: &str) {
        executable(
            &self.home.join("bin/kin-daemon"),
            &json!({
                "schema": "kin.daemon.compat.v2", "version": "0.8.6",
                "build": {
                    "sha": sha, "dirty": dirty, "source_known": known,
                    "target": target, "build_profile": profile
                }
            }),
        );
    }

    fn cli(&self, dirty: bool, known: bool) {
        executable(
            &self.home.join("bin/kin"),
            &json!({
                "kin_version": "0.8.6", "kin_commit": SHA,
                "kin_dirty": dirty, "kin_source_known": known,
                "kin_target": "aarch64-apple-darwin", "kin_build_profile": "release"
            }),
        );
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kin"));
        command
            .args(["runtime", "status", "--json"])
            .current_dir(&self.working)
            .env("HOME", self.root.path())
            .env("KIN_HOME", &self.home)
            .env_remove("KIN_DIR")
            .env_remove("KIN_DAEMON_URL")
            .env_remove("KIN_DAEMON_BIN")
            .env_remove("KIN_RUNTIME_MODE");
        command
    }

    fn status(&self) -> (bool, Value) {
        let output = self.command().output().unwrap();
        let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "status did not return JSON: {error}; stderr={}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.success(), report)
    }
}

fn executable(path: &Path, payload: &Value) {
    let payload = serde_json::to_string(payload).unwrap();
    std::fs::write(path, format!("#!/bin/sh\nprintf '%s\\n' '{payload}'\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn runtime_status_admits_exact_pair_without_opening_repository() {
    let fixture = Fixture::new();
    fixture.pair(SHA, false, true);
    std::fs::create_dir(fixture.working.join(".kin")).unwrap();
    std::fs::write(fixture.working.join(".kin/not-a-store"), "sentinel").unwrap();
    let (success, report) = fixture.status();
    assert!(success, "{report}");
    assert_eq!(report["schema"], "kin.runtime-status.v1");
    assert_eq!(report["mode"], "managed");
    assert_eq!(report["ready"], true);
    assert_eq!(report["pair_matches"], true);
    assert_eq!(report["release_kind"], "unknown");
    assert_eq!(report["channel"], Value::Null);
    assert_eq!(report["cli"]["commit_sha"], SHA);
    assert_eq!(report["cli"]["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(report["daemon"]["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(
        std::fs::read_dir(fixture.working.join(".kin"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn runtime_status_refuses_same_version_from_different_source() {
    let fixture = Fixture::new();
    fixture.pair("2222222222222222222222222222222222222222", false, true);
    let (success, report) = fixture.status();
    assert!(!success);
    assert_eq!(report["ready"], false);
    assert_eq!(report["pair_matches"], false);
    assert_eq!(report["reason"], "pair_source_mismatch");
    assert_eq!(report["cli"]["version"], report["daemon"]["version"]);
    assert_ne!(report["cli"]["commit_sha"], report["daemon"]["commit_sha"]);
}

#[test]
fn runtime_status_refuses_target_and_profile_mismatch() {
    let fixture = Fixture::new();
    fixture.cli(false, true);
    fixture.daemon(SHA, false, true, "x86_64-apple-darwin", "release");
    let (success, report) = fixture.status();
    assert!(!success);
    assert_eq!(report["reason"], "pair_source_mismatch");
    fixture.daemon(SHA, false, true, "aarch64-apple-darwin", "debug");
    let (success, report) = fixture.status();
    assert!(!success);
    assert_eq!(report["pair_matches"], false);
}

#[test]
fn runtime_status_development_home_is_deterministic_and_not_managed_home() {
    let fixture = Fixture::new();
    fixture.pair(SHA, true, true);
    let read = || {
        let output = fixture
            .command()
            .arg("--development")
            .arg("--binary")
            .arg(fixture.home.join("bin/kin"))
            .env_remove("KIN_HOME")
            .output()
            .unwrap();
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(output.status.success(), "{report}");
        PathBuf::from(report["kin_home"].as_str().unwrap())
    };
    let first = read();
    assert_eq!(first, read());
    assert_ne!(first, fixture.home);
    assert!(first.to_string_lossy().contains("/.kin/development/"));
    assert_eq!(first.file_name().unwrap().len(), 64);
    assert!(!first.exists());
}

#[test]
fn runtime_status_development_cannot_reuse_default_managed_home() {
    let fixture = Fixture::new();
    fixture.pair(SHA, true, true);
    let output = fixture
        .command()
        .arg("--development")
        .arg("--binary")
        .arg(fixture.home.join("bin/kin"))
        .env("KIN_HOME", fixture.root.path().join(".kin"))
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!output.status.success());
    assert_eq!(report["reason"], "development_home_conflict");
}

#[test]
fn runtime_status_refuses_unknown_source_and_dirty_managed_pair() {
    let fixture = Fixture::new();
    fixture.pair(SHA, false, false);
    let (success, report) = fixture.status();
    assert!(!success);
    assert_eq!(report["reason"], "source_identity_unknown");
    fixture.pair(SHA, true, true);
    let (success, report) = fixture.status();
    assert!(!success);
    assert_eq!(report["reason"], "managed_identity_refused");
}

#[test]
fn runtime_status_admits_explicit_isolated_dirty_development_pair() {
    let fixture = Fixture::new();
    fixture.pair(SHA, true, true);
    let output = fixture
        .command()
        .arg("--development")
        .arg("--binary")
        .arg(fixture.home.join("bin/kin"))
        .env("KIN_HOME", fixture.root.path().join("isolated-development"))
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success(), "{report}");
    assert_eq!(report["mode"], "development");
    assert_eq!(report["release_kind"], "development");
    assert_eq!(report["pair_matches"], true);
    assert_eq!(report["cli"]["source_dirty"], true);
    assert!(!fixture.root.path().join("isolated-development").exists());
}

#[test]
fn runtime_status_reports_missing_daemon_as_complete_refusal_json() {
    let fixture = Fixture::new();
    fixture.cli(false, true);
    let (success, report) = fixture.status();
    assert!(!success);
    assert_eq!(report["reason"], "daemon_probe_failed");
    assert_eq!(report["cli"]["commit_sha"], SHA);
    assert!(report["daemon"]["path"]
        .as_str()
        .unwrap()
        .ends_with("/bin/kin-daemon"));
    assert_eq!(report["daemon"]["sha256"], Value::Null);
}

#[test]
fn runtime_status_preserves_native_updater_restart_fence() {
    let fixture = Fixture::new();
    fixture.pair(SHA, false, true);
    std::fs::write(fixture.home.join("update-restart-ack-required.json"), "{}").unwrap();
    let (success, report) = fixture.status();
    assert!(!success);
    assert_eq!(report["pair_matches"], true);
    assert_eq!(report["restart_needed"], true);
    assert_eq!(report["reason"], "restart_required");
}

#[test]
fn runtime_status_honors_kin_home_before_kin_dir() {
    let fixture = Fixture::new();
    fixture.pair(SHA, false, true);
    let output = fixture
        .command()
        .env("KIN_DIR", fixture.root.path().join("wrong-root"))
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success(), "{report}");
    assert_eq!(
        PathBuf::from(report["kin_home"].as_str().unwrap()),
        fixture.home.canonicalize().unwrap()
    );
}

#[test]
fn runtime_status_ignores_empty_home_before_kin_dir() {
    let fixture = Fixture::new();
    fixture.pair(SHA, false, true);
    let output = fixture
        .command()
        .env("KIN_HOME", "")
        .env("KIN_DIR", &fixture.home)
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success(), "{report}");
    assert_eq!(
        PathBuf::from(report["kin_home"].as_str().unwrap()),
        fixture.home.canonicalize().unwrap()
    );
}

#[test]
fn development_cli_refuses_global_setup_doctor_repair_and_update_mutations() {
    let fixture = Fixture::new();
    for args in [
        vec!["setup"],
        vec!["doctor", "--fix"],
        vec!["doctor", "--reclaim-staging"],
        vec!["setup", "doctor", "--install-language-servers"],
        vec!["update", "--apply"],
        vec!["update", "--cleanup-windows-worker", "unused-handoff.json"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_kin"))
            .args(&args)
            .env("HOME", fixture.root.path())
            .env("KIN_HOME", fixture.root.path().join("isolated-development"))
            .env("KIN_RUNTIME_MODE", "development")
            .env_remove("KIN_DIR")
            .env_remove("KIN_DAEMON_URL")
            .current_dir(&fixture.working)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("development mode cannot"),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(!fixture.root.path().join("isolated-development").exists());
    assert!(!fixture.root.path().join(".zshrc").exists());
    assert!(!fixture.root.path().join(".codex").exists());
}

#[test]
fn windows_worker_cleanup_refuses_combined_update_actions_before_dispatch() {
    let fixture = Fixture::new();
    for extra in [
        vec!["--apply"],
        vec!["--check-only"],
        vec!["--ack-restart"],
        vec!["--prepare-windows-worker"],
        vec!["--windows-worker", "other-handoff.json"],
        vec!["--set-policy", "auto"],
        vec!["--channel", "stable"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_kin"))
            .args(["update", "--cleanup-windows-worker", "unused-handoff.json"])
            .args(&extra)
            .env("HOME", fixture.root.path())
            .env("KIN_HOME", fixture.root.path().join("isolated-development"))
            .env("KIN_RUNTIME_MODE", "development")
            .env_remove("KIN_DIR")
            .env_remove("KIN_DAEMON_URL")
            .current_dir(&fixture.working)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{extra:?} unexpectedly succeeded");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("cannot be used with"),
            "{extra:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(!fixture.root.path().join("isolated-development").exists());
}

#[test]
fn runtime_status_deadline_includes_descendant_inherited_output_pipes() {
    let fixture = Fixture::new();
    fixture.pair(SHA, false, true);
    std::fs::write(
        fixture.home.join("bin/kin"),
        "#!/bin/sh\nsleep 60 &\nprintf '%s\\n' '{\"kin_version\":\"0.8.6\"}'\nexit 0\n",
    )
    .unwrap();
    let started = std::time::Instant::now();
    let (success, report) = fixture.status();
    assert!(!success);
    assert_eq!(report["reason"], "cli_probe_failed");
    assert!(report["detail"]
        .as_str()
        .unwrap()
        .contains("output exceeded"));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "capture waited for descendant instead of respecting its deadline"
    );
}

#[test]
fn runtime_status_binary_override_requires_development() {
    let fixture = Fixture::new();
    fixture.pair(SHA, false, true);
    let output = fixture
        .command()
        .arg("--binary")
        .arg(fixture.home.join("bin/kin"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--development"));
}

#[test]
fn runtime_status_records_channel_without_claiming_published_release() {
    let fixture = Fixture::new();
    fixture.pair(SHA, false, true);
    std::fs::write(fixture.home.join("update.toml"), "channel = \"alpha\"\n").unwrap();
    let (success, report) = fixture.status();
    assert!(success, "{report}");
    assert_eq!(report["channel"], "alpha");
    assert_eq!(report["release_kind"], "unknown");
}

#[test]
fn runtime_status_refuses_symbolic_link_managed_root() {
    let fixture = Fixture::new();
    fixture.pair(SHA, false, true);
    let alias = fixture.root.path().join("managed-alias");
    std::os::unix::fs::symlink(&fixture.home, &alias).unwrap();
    let output = fixture.command().env("KIN_HOME", &alias).output().unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!output.status.success());
    assert_eq!(report["reason"], "managed_path_invalid");
    assert!(std::fs::symlink_metadata(alias)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn runtime_status_refuses_managed_component_links_but_allows_explicit_development_alias() {
    let fixture = Fixture::new();
    fixture.pair(SHA, false, true);
    let real = fixture.home.join("bin/real-kin");
    let alias = fixture.home.join("bin/kin");
    std::fs::rename(&alias, &real).unwrap();
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let (success, report) = fixture.status();
    assert!(!success);
    assert_eq!(report["reason"], "managed_path_invalid");
    let output = fixture
        .command()
        .arg("--development")
        .arg("--binary")
        .arg(&alias)
        .env("KIN_HOME", fixture.root.path().join("isolated-development"))
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success(), "{report}");
    assert_eq!(
        PathBuf::from(report["cli"]["path"].as_str().unwrap()),
        real.canonicalize().unwrap()
    );
}
