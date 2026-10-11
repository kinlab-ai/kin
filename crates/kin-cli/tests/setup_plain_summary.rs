// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! `kin setup` without a terminal prints the short plain summary, and
//! `--verbose` the full record.
//!
//! An installer with no terminal, a CI log and a pipe used to get the full
//! record by default: over a hundred and fifty lines of every step and every
//! health check, in Kin's internal terms. They now get the summary a person at
//! a terminal gets, without the live line, and the full record stays one flag
//! away for the scripts and checks that read it.

use std::path::Path;
use std::process::Output;
use std::time::Duration;

mod common;

/// Wall-clock cap for one scripted `kin setup` run, which takes seconds.
const SETUP_TIMEOUT: Duration = Duration::from_secs(120);

fn run_setup(home: &Path, cwd: &Path, extra: &[&str]) -> Output {
    let mut command = common::Command::new(env!("CARGO_BIN_EXE_kin"));
    command
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
        .env("KIN_HOME", home.join(".kin"))
        .env("KIN_VFS_DISABLE", "1")
        .env("KIN_NO_DAEMON", "1")
        .env("KIN_REGISTRY_PATH", home.join("registry.toml"))
        .env_remove("KIN_DIR")
        .env_remove("KIN_DAEMON_URL")
        .env_remove("KIN_MCP_REPO");
    command.output_within(SETUP_TIMEOUT).expect("run kin setup")
}

#[test]
fn setup_without_a_terminal_prints_the_short_summary_and_verbose_the_full_record() {
    let root = tempfile::tempdir().expect("setup root");
    let home = root.path().join("home");
    let cwd = root.path().join("outside-repository");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&cwd).expect("create cwd");

    let short = run_setup(&home, &cwd, &[]);
    let stdout = String::from_utf8_lossy(&short.stdout);
    let stderr = String::from_utf8_lossy(&short.stderr);
    assert!(
        short.status.success(),
        "setup failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("This run asks nothing and takes the defaults."),
        "the summary says up front that nothing is asked:\n{stdout}"
    );
    assert!(
        stdout.contains("Kin is set up on"),
        "the summary ends on its verdict:\n{stdout}"
    );
    assert!(
        stdout.contains("taken without asking; kin setup --verbose lists each one"),
        "the summary counts the defaults it took and says where they are listed:\n{stdout}"
    );
    assert!(
        !stdout.contains("=== Health checklist ===") && !stdout.contains("Hardware check"),
        "the full record stays behind --verbose:\n{stdout}"
    );
    assert!(
        !stdout.contains('\u{1b}'),
        "a pipe gets no escape codes:\n{stdout}"
    );
    assert!(
        stdout.lines().count() <= 30,
        "the summary is short: {} lines\n{stdout}",
        stdout.lines().count()
    );

    let full = run_setup(&home, &cwd, &["--verbose"]);
    let stdout = String::from_utf8_lossy(&full.stdout);
    assert!(full.status.success(), "verbose setup failed:\n{stdout}");
    assert!(
        stdout.contains("=== Health checklist ===")
            && stdout.contains("This run was not interactive, so it answered these for you:"),
        "--verbose prints the full record with every decision it took:\n{stdout}"
    );
}

/// `--mode` changed nothing for a long time. It stays accepted so a script
/// that passes it keeps running, and says it is ignored rather than staying
/// silent.
#[test]
fn setup_says_the_mode_flag_is_ignored() {
    let root = tempfile::tempdir().expect("setup root");
    let home = root.path().join("home");
    let cwd = root.path().join("outside-repository");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&cwd).expect("create cwd");

    let output = run_setup(&home, &cwd, &["--mode", "native"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "--mode must not fail a run:\n{stderr}"
    );
    assert!(
        stderr.contains("--mode is ignored"),
        "the flag says it is ignored:\n{stderr}"
    );

    let help = common::Command::new(env!("CARGO_BIN_EXE_kin"))
        .args(["setup", "--help"])
        .env("HOME", &home)
        .output_within(SETUP_TIMEOUT)
        .expect("run kin setup --help");
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(
        !help.contains("--mode"),
        "the help no longer offers it:\n{help}"
    );
}
