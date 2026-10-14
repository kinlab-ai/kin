// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! `kin doctor --install-language-servers` says what it did (FIR-2502).
//!
//! Both stranger arms on v0.5.43 typed this flag, were told nothing, and
//! converted a large repository with no enrichment at all. Two silences caused
//! it: the flag was dead without `--fix`, and with `--fix` it declined to
//! install through branches that printed nothing. Neither is reachable from a
//! unit test, because both are about what a stranger sees on a terminal, so the
//! cheap half of that surface is driven here against the real binary.
//!
//! Nothing below needs a daemon, a network or a repository.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tempfile::tempdir;

mod common;

use common::Command;

/// A development invocation with isolated home and configuration roots.
fn kin_command(home: &std::path::Path, cwd: &std::path::Path) -> Command<'static> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kin"));
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home.join("AppData/Roaming"))
        .env("LOCALAPPDATA", home.join("AppData/Local"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("KIN_HOME", home.join("development-runtime"))
        .env("KIN_RUNTIME_MODE", "development");
    command.current_dir(cwd);
    command
}

fn home_contents(home: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut contents = BTreeMap::new();
    let mut directories = vec![home.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(&directory).expect("read isolated home") {
            let entry = entry.expect("read isolated home entry");
            let path = entry.path();
            let relative = path
                .strip_prefix(home)
                .expect("owned home path")
                .to_path_buf();
            let kind = entry.file_type().expect("read isolated home entry type");
            if kind.is_dir() {
                contents.insert(relative, None);
                directories.push(path);
            } else {
                assert!(kind.is_file(), "unexpected isolated home entry: {path:?}");
                contents.insert(relative, Some(std::fs::read(path).expect("read fixture")));
            }
        }
    }
    contents
}

/// A `HOME` and a working directory that is definitely not a Kin repository.
fn scratch() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let root = tempdir().expect("temp root");
    let home = root.path().join("home");
    let cwd = root.path().join("elsewhere");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    (root, home, cwd)
}

/// Hole one, end to end. The flag used to be bound at `doctor`'s signature and
/// first read past the `--fix` early return, so this exact command printed an
/// ordinary health report and exited 0 having installed nothing.
#[test]
fn the_flag_without_fix_says_what_it_needs_instead_of_exiting_quietly() {
    let (_root, home, cwd) = scratch();

    let output = kin_command(&home, &cwd)
        .args(["doctor", "--install-language-servers"])
        .output()
        .expect("run kin doctor --install-language-servers");

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "the read-only notice must run. status={} stderr={stderr} stdout={stdout}",
        output.status
    );
    assert!(
        stderr.contains("Nothing installed."),
        "the no-op has to be stated. stderr={stderr} stdout={stdout}"
    );
    assert!(
        stderr.contains("only runs under `--fix`"),
        "the reason has to name the missing flag. stderr={stderr}"
    );
    assert!(
        stderr.contains("kin doctor --fix --install-language-servers"),
        "the refusal has to name the command that works. stderr={stderr}"
    );
}

/// A plain `kin doctor` must not gain a line about a repair nobody asked for.
/// Without this, the fix for a silent no-op becomes noise on every run, and the
/// assertion above could pass on a message that always prints.
#[test]
fn a_doctor_run_that_did_not_ask_says_nothing_about_language_servers() {
    let (_root, home, cwd) = scratch();

    let output = kin_command(&home, &cwd)
        .args(["doctor"])
        .output()
        .expect("run kin doctor");

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        !stderr.contains("Nothing installed."),
        "a run that never asked was told anyway. stderr={stderr}"
    );
    assert!(
        !stderr.contains("--install-language-servers"),
        "a run that never asked was told anyway. stderr={stderr}"
    );
}

/// The notice is a diagnostic, not part of the report. `kin doctor --json`
/// promises parseable stdout, so a fix for one silent failure must not become
/// the cause of an unparseable one.
#[test]
fn the_notice_leaves_json_stdout_parseable() {
    let (_root, home, cwd) = scratch();

    let output = kin_command(&home, &cwd)
        .args(["doctor", "--install-language-servers", "--json"])
        .output()
        .expect("run kin doctor --install-language-servers --json");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "the read-only JSON report must run. status={} stderr={stderr} stdout={stdout}",
        output.status
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!("doctor --json stdout must parse: {e}. stderr={stderr} stdout={stdout}")
    });
    assert!(
        report["checks"].is_array(),
        "the report is still the report. stdout={stdout}"
    );
    assert!(
        stderr.contains("only runs under `--fix`"),
        "the notice still has to reach the operator. stderr={stderr}"
    );
}

/// Development builds must refuse global repairs before they write any
/// configuration. The language-server decision unit tests cover the managed
/// repair's outside-repository notice independently.
#[test]
fn development_doctor_repairs_refuse_global_writes() {
    let (_root, home, cwd) = scratch();
    std::fs::write(home.join(".zshrc"), "# retain shell configuration\n")
        .expect("write shell fixture");
    let client = home.join(".config/kin-client");
    std::fs::create_dir_all(&client).expect("create client fixture");
    std::fs::write(client.join("config.json"), "{\"retain\":true}\n")
        .expect("write client fixture");
    let before = home_contents(&home);

    for args in [
        vec!["doctor", "--fix"],
        vec!["doctor", "--fix", "--install-language-servers"],
    ] {
        let output = kin_command(&home, &cwd)
            .args(&args)
            .output()
            .expect("run development doctor repair");
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        assert!(
            !output.status.success(),
            "development repair must refuse. args={args:?} stderr={stderr} stdout={stdout}"
        );
        assert!(
            stderr.contains("development mode cannot run setup or doctor repairs")
                && stderr.contains("use the admitted managed runtime"),
            "the refusal must identify the required runtime. args={args:?} stderr={stderr}"
        );
        assert!(stdout.is_empty(), "repair must not start: {stdout}");
        assert_eq!(
            home_contents(&home),
            before,
            "repair changed the owned home"
        );
    }
}
