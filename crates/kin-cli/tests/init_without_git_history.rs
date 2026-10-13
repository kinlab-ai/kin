// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! `kin init` in a directory that holds a project and no Git history, through
//! the real binaries.
//!
//! Such a directory used to be refused: Kin would not derive recorded
//! authority from whatever a directory happened to hold. It still does not.
//! The repository starts with no change in it, what the directory holds is
//! taken in as that repository's uncommitted work by the admission every
//! working copy gets, and it becomes history only when a person commits it.

use serde_json::Value;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

mod common;

use common::Command;

fn run_git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", kin_git::empty_global_git_config())
        .current_dir(repo)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_kin(
    runtime: &common::IsolatedDaemonRuntime,
    repo: &Path,
    args: &[&str],
) -> std::process::Output {
    runtime
        .kin_command()
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", kin_git::empty_global_git_config())
        .env("KIN_DAEMON_BIN", runtime.daemon_bin())
        .env("KIN_DAEMON_DISABLE_LSP", "1")
        .current_dir(repo)
        .output()
        .expect("run kin")
}

fn require_kin(
    runtime: &common::IsolatedDaemonRuntime,
    repo: &Path,
    args: &[&str],
) -> std::process::Output {
    let output = run_kin(runtime, repo, args);
    assert!(
        output.status.success(),
        "kin {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn json(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "not JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// How many changes the repository's history holds.
fn changes(runtime: &common::IsolatedDaemonRuntime, repo: &Path) -> usize {
    json(&require_kin(runtime, repo, &["log", "--json"]))["entries"]
        .as_array()
        .expect("the log lists its entries")
        .len()
}

/// The working copy against the repository's history: entities and artifacts
/// it adds. Measured with the repository's daemon running, since a diff with
/// no daemon answers from recorded authority alone and says so by its status.
fn uncommitted(runtime: &common::IsolatedDaemonRuntime, repo: &Path) -> (u64, u64) {
    require_kin(runtime, repo, &["admit"]);
    let diff = json(&require_kin(runtime, repo, &["diff", "--json"]));
    let count = |name: &str| diff["summary"][name].as_u64().expect("a count");
    (count("entities_added"), count("artifacts_added"))
}

fn write_project(repo: &Path) {
    fs::create_dir_all(repo.join("pkg")).expect("create the package");
    fs::write(
        repo.join("pkg/shapes.py"),
        "import math\n\n\ndef area(radius):\n    return math.pi * radius * radius\n\n\n\
         def perimeter(radius):\n    return 2 * math.pi * radius\n",
    )
    .expect("write a module");
    fs::write(
        repo.join("pkg/report.py"),
        "from pkg.shapes import area\n\n\ndef describe(radius):\n    return area(radius)\n",
    )
    .expect("write a second module");
    fs::write(repo.join("NOTES.md"), "notes for the team\n").expect("write notes");
}

#[test]
fn a_project_with_no_git_history_starts_with_no_change_and_its_content_as_uncommitted_work() {
    let root = tempdir().expect("temp root");
    let repo = root.path().join("plain");
    write_project(&repo);
    let runtime = common::IsolatedDaemonRuntime::new(&repo);

    let init = require_kin(&runtime, &repo, &["init"]);
    let said = String::from_utf8_lossy(&init.stdout);
    let waiting = said
        .lines()
        .find(|line| line.trim_start().starts_with("Uncommitted work:"))
        .unwrap_or_else(|| panic!("init reports the work that is waiting: {said}"));
    assert!(
        waiting.contains("3 artifacts") && waiting.contains("nothing is recorded until you commit"),
        "{waiting}"
    );
    assert!(!waiting.to_lowercase().contains("file"), "{waiting}");
    assert!(!said.contains("Workspace: empty"), "{said}");
    assert!(said.contains("kin commit -m"), "{said}");

    // Nothing was recorded: the history is empty, and everything the
    // directory held reads as work the repository has not committed.
    assert_eq!(changes(&runtime, &repo), 0);
    let (entities, artifacts) = uncommitted(&runtime, &repo);
    assert_eq!(artifacts, 3);
    assert!(entities >= 3, "the functions are entities: {entities}");

    // Committing is what records it, and leaves nothing uncommitted. A
    // project with no Git history has no Git identity either, so the author
    // is the one the repository's own configuration names.
    let config = runtime.layout_for(&repo).config_path();
    let settings = fs::read_to_string(&config).unwrap_or_default();
    fs::write(
        &config,
        format!("default_author = \"Ada Lovelace <ada@example.com>\"\n{settings}"),
    )
    .expect("name the repository's author");
    require_kin(&runtime, &repo, &["commit", "-m", "Add the project"]);
    assert_eq!(changes(&runtime, &repo), 1);
    assert_eq!(uncommitted(&runtime, &repo), (0, 0));
}

#[test]
fn an_empty_directory_still_starts_an_empty_repository() {
    let root = tempdir().expect("temp root");
    let repo = root.path().join("empty");
    fs::create_dir_all(&repo).expect("create the directory");
    let runtime = common::IsolatedDaemonRuntime::new(&repo);

    let init = require_kin(&runtime, &repo, &["init"]);
    let said = String::from_utf8_lossy(&init.stdout);
    assert!(said.contains("Workspace: empty"), "{said}");
    assert!(!said.contains("Uncommitted work"), "{said}");
    assert!(!said.contains("kin commit"), "{said}");
    assert_eq!(changes(&runtime, &repo), 0);
    assert_eq!(uncommitted(&runtime, &repo), (0, 0));
}

#[test]
fn a_git_project_is_still_admitted_from_its_checked_out_commit() {
    let root = tempdir().expect("temp root");
    let repo = root.path().join("tracked");
    write_project(&repo);
    run_git(&repo, &["init", "--initial-branch=main"]);
    run_git(&repo, &["config", "commit.gpgsign", "false"]);
    run_git(&repo, &["config", "user.name", "Ada Lovelace"]);
    run_git(&repo, &["config", "user.email", "ada@example.com"]);
    run_git(&repo, &["add", "--all"]);
    run_git(&repo, &["commit", "-m", "first commit"]);
    let runtime = common::IsolatedDaemonRuntime::new(&repo);

    let init = require_kin(&runtime, &repo, &["init"]);
    let said = String::from_utf8_lossy(&init.stdout);
    assert!(said.contains("Imported: "), "{said}");
    assert!(!said.contains("Uncommitted work:"), "{said}");
    // Its one commit is its one change, and nothing is left uncommitted.
    assert_eq!(changes(&runtime, &repo), 1);
    assert_eq!(uncommitted(&runtime, &repo), (0, 0));
}

#[test]
fn a_home_directory_is_refused_in_one_sentence_and_no_repository_is_started() {
    let root = tempdir().expect("temp root");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("create the home");
    fs::write(home.join("letter.txt"), "kept here\n").expect("write something a person keeps");
    // Run without the isolated runtime, which owns `HOME`: the refusal comes
    // before anything is started, so there is no daemon to contain.
    let kin = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_kin"))
            .args(args)
            .env("HOME", &home)
            .env("KIN_HOME", root.path().join("kin-home"))
            .env("KIN_REGISTRY_PATH", root.path().join("registry.toml"))
            .env("KIN_NO_DAEMON", "1")
            .current_dir(&home)
            .output()
            .expect("run kin")
    };

    let refused = kin(&["init"]);
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(
        !refused.status.success(),
        "a home directory is no project: stdout={} stderr={said}",
        String::from_utf8_lossy(&refused.stdout)
    );
    assert!(
        said.contains("Kin does not start a repository in your home directory itself"),
        "{said}"
    );
    assert!(!home.join(".kin").exists(), "nothing was started in it");
    assert!(
        !root.path().join("kin-home").join("stores").exists(),
        "no store was written for it"
    );
    let found = kin(&["store", "path"]);
    assert!(
        !found.status.success() || String::from_utf8_lossy(&found.stdout).trim().is_empty(),
        "no store serves the home directory: {}",
        String::from_utf8_lossy(&found.stdout)
    );

    // A project inside that home is a project.
    let project = home.join("code").join("app");
    write_project(&project);
    let started = Command::new(env!("CARGO_BIN_EXE_kin"))
        .arg("init")
        .env("HOME", &home)
        .env("KIN_HOME", root.path().join("kin-home"))
        .env("KIN_REGISTRY_PATH", root.path().join("registry.toml"))
        .env("KIN_NO_DAEMON", "1")
        .current_dir(&project)
        .output()
        .expect("run kin init");
    assert!(
        started.status.success(),
        "a project in a home directory starts: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    // With no daemon to read it, the work is said to be waiting and no count
    // is claimed for it.
    let said = String::from_utf8_lossy(&started.stdout);
    assert!(
        said.contains("Uncommitted work: what this directory already holds"),
        "{said}"
    );
}
