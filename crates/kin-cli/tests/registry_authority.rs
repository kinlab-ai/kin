// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

mod common;

use common::Command;

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn unsafe_authority() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path().join("registry.toml");
    let lock = root.path().join("registry.lock");
    std::fs::write(&registry, "repos = []\n").unwrap();
    std::fs::write(&lock, b"").unwrap();
    std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    (root, registry, lock)
}

fn kin(root: &Path, registry: &Path) -> Command<'static> {
    let home = root.join("home");
    let kin_home = root.join("isolated-kin-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&kin_home).unwrap();
    std::fs::set_permissions(&kin_home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_kin"));
    command
        .current_dir(root)
        .env("HOME", &home)
        .env("KIN_HOME", &kin_home)
        .env("KIN_REGISTRY_PATH", registry)
        .env("PATH", "/usr/bin:/bin");
    command
}

fn json_report(output: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "expected a JSON diagnostic: {error}; status: {}; stdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn broken_managed_kin(root: &Path, registry: &Path) -> (Command<'static>, std::path::PathBuf) {
    let managed_home = root.join("home").join(".kin");
    let bin = managed_home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::set_permissions(&managed_home, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (name, body) in [
        ("kin", b"invalid managed CLI image\n".as_slice()),
        ("kin-daemon", b"invalid managed daemon image\n".as_slice()),
    ] {
        let path = bin.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut command = kin(root, registry);
    command
        .env("KIN_HOME", &managed_home)
        .env("KIN_RUNTIME_MODE", "managed");
    (command, managed_home)
}

fn assert_broken_managed_pair_unchanged(managed_home: &Path) {
    assert_eq!(
        std::fs::read(managed_home.join("bin/kin")).unwrap(),
        b"invalid managed CLI image\n"
    );
    assert_eq!(
        std::fs::read(managed_home.join("bin/kin-daemon")).unwrap(),
        b"invalid managed daemon image\n"
    );
    assert_eq!(std::fs::read_dir(managed_home).unwrap().count(), 1);
    assert_eq!(
        std::fs::read_dir(managed_home.join("bin")).unwrap().count(),
        2
    );
}

#[test]
fn registry_authority_json_fails_closed_without_reading_or_repairing() {
    let (root, registry, lock) = unsafe_authority();
    let private_sentinel = "FIR_1481_CONTENT_MUST_NOT_ESCAPE";
    std::fs::write(
        &registry,
        format!("repos = []\nprivate = \"{private_sentinel}\"\n"),
    )
    .unwrap();
    let output = kin(root.path(), &registry)
        .args(["registry", "authority", "--json"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains(private_sentinel));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(private_sentinel));
    let report = json_report(&output);
    assert!(report["checks"].as_array().unwrap().iter().any(|check| {
        check["state"] == "repairable_permissions"
            && check["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("expected 0600"))
    }));
    assert_eq!(mode(&registry), 0o644);
    assert_eq!(mode(&lock), 0o644);
}

#[test]
fn registry_authority_json_remains_read_only_with_a_broken_managed_pair() {
    let (root, registry, lock) = unsafe_authority();
    let registry_before = std::fs::read(&registry).unwrap();
    let lock_before = std::fs::read(&lock).unwrap();
    let (mut command, managed_home) = broken_managed_kin(root.path(), &registry);
    let output = command
        .args(["registry", "authority", "--json"])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "unsafe authority must still be refused"
    );
    let report = json_report(&output);
    assert!(report["checks"].as_array().unwrap().iter().any(|check| {
        check["state"] == "repairable_permissions"
            && check["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("expected 0600"))
    }));
    assert_eq!(std::fs::read(&registry).unwrap(), registry_before);
    assert_eq!(std::fs::read(&lock).unwrap(), lock_before);
    assert_eq!(mode(&registry), 0o644);
    assert_eq!(mode(&lock), 0o644);
    assert_broken_managed_pair_unchanged(&managed_home);
}

#[test]
fn registry_authority_fix_requires_admission_and_preserves_unsafe_permissions() {
    let (root, registry, lock) = unsafe_authority();
    let registry_before = std::fs::read(&registry).unwrap();
    let lock_before = std::fs::read(&lock).unwrap();
    let (mut command, managed_home) = broken_managed_kin(root.path(), &registry);
    let output = command
        .args(["registry", "authority", "--fix", "--json"])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "a broken managed pair cannot authorize repair"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("runtime admission refused")
            || stderr.contains(
                "managed runtime requires a clean release build with a full known source commit and target"
            ),
        "mutation must be refused by runtime qualification or admission; status: {}; stdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        stderr
    );
    assert_eq!(std::fs::read(&registry).unwrap(), registry_before);
    assert_eq!(std::fs::read(&lock).unwrap(), lock_before);
    assert_eq!(mode(&registry), 0o644);
    assert_eq!(mode(&lock), 0o644);
    assert_broken_managed_pair_unchanged(&managed_home);
}

#[test]
fn registry_authority_initialize_requires_admission_and_creates_nothing() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path().join("registry.toml");
    let lock = root.path().join("registry.lock");
    let (mut command, managed_home) = broken_managed_kin(root.path(), &registry);
    let output = command
        .args(["registry", "authority", "--initialize", "--json"])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "a broken managed pair cannot authorize initialization"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("runtime admission refused")
            || stderr.contains(
                "managed runtime requires a clean release build with a full known source commit and target"
            ),
        "mutation must be refused by runtime qualification or admission; status: {}; stdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        stderr
    );
    assert!(
        !registry.exists(),
        "refused initialization must not create the registry"
    );
    assert!(
        !lock.exists(),
        "refused initialization must not create the registry lock"
    );
    assert!(!registry.with_extension("tmp").exists());
    assert_broken_managed_pair_unchanged(&managed_home);
}

#[test]
fn doctor_json_and_human_report_the_same_read_only_contract() {
    let (root, registry, lock) = unsafe_authority();
    let json = kin(root.path(), &registry)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let report = json_report(&json);
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "registry_authority")
        .unwrap();
    assert_eq!(check["status"], "misconfigured");
    assert_eq!(check["fixable"], true);

    let human = kin(root.path(), &registry).arg("doctor").output().unwrap();
    assert!(human.status.success());
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(stdout.contains("Registry authority"));
    assert!(stdout.contains("expected 0600"));
    assert!(stdout.contains("kin doctor --fix"));
    assert_eq!(mode(&registry), 0o644);
    assert_eq!(mode(&lock), 0o644);
}

#[test]
fn development_doctor_fix_refuses_registry_permission_repair() {
    let (root, registry, lock) = unsafe_authority();
    let registry_before = std::fs::read(&registry).unwrap();
    let lock_before = std::fs::read(&lock).unwrap();
    let output = kin(root.path(), &registry)
        .env("KIN_RUNTIME_MODE", "development")
        .args(["doctor", "--fix"])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("development mode cannot run setup or doctor repairs")
            && stderr.contains("use the admitted managed runtime"),
        "doctor repair must require managed admission: {stderr}"
    );
    assert!(output.stdout.is_empty(), "doctor repair must not start");
    assert_eq!(std::fs::read(&registry).unwrap(), registry_before);
    assert_eq!(std::fs::read(&lock).unwrap(), lock_before);
    assert_eq!(mode(&registry), 0o644);
    assert_eq!(mode(&lock), 0o644);
}
