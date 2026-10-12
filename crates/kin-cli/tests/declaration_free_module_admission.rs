// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A repository holding modules that declare nothing still admits, commits and
//! stashes edits through the real `kin` and `kin-daemon` binaries.
//!
//! A module with no declarations, such as one holding only a comment or a root
//! `index.js` that re-exports another module, is kept as a local module when a
//! reconcile restores its dependencies, so an import of it stays local. Every
//! admission reconciles in a private graph that carries no derived layouts,
//! and requiring one there refused every edit in such a repository: `kin
//! commit` and `kin stash` answered HTTP 500.

use std::fs;
use std::path::Path;
use std::process::Output;

use serde_json::Value;
use tempfile::tempdir;

mod common;

use common::{Command, IsolatedDaemonRuntime};

const QUIET: &str = "// nothing is declared in this module\n";
const INDEX: &str = "module.exports = require('./web/lib');\n";
const LIB: &str =
    "function triple(value) {\n  return value * 3;\n}\n\nmodule.exports = { triple };\n";
const CALLER: &str = "var lib = require('..');\n\nfunction main() {\n  return lib.triple(2);\n}\n\nmodule.exports = { main };\n";

fn git(repo: &Path, args: &[&str]) {
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

fn kin(runtime: &IsolatedDaemonRuntime, repo: &Path, args: &[&str]) -> Output {
    runtime
        .kin_command()
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", kin_git::empty_global_git_config())
        .env("KIN_EMBED_BACKEND", "cpu")
        .env("KIN_DAEMON_AUTO_EMBED", "0")
        .env("KIN_DAEMON_DISABLE_LSP", "1")
        .env("KIN_DAEMON_READY_TIMEOUT_SECS", "180")
        .env("KIN_DAEMON_BIN", runtime.daemon_bin())
        .current_dir(repo)
        .output()
        .expect("run kin")
}

fn succeed(runtime: &IsolatedDaemonRuntime, repo: &Path, args: &[&str]) -> String {
    let output = kin(runtime, repo, args);
    assert!(
        output.status.success(),
        "kin {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("kin stdout is UTF-8")
}

/// Whether `kin search` finds a declaration named `name` in `module`.
fn declares(runtime: &IsolatedDaemonRuntime, repo: &Path, name: &str, module: &str) -> bool {
    let found: Value = serde_json::from_str(&succeed(runtime, repo, &["search", name, "--json"]))
        .expect("search emits JSON");
    found
        .as_array()
        .expect("search rows")
        .iter()
        .any(|row| row["name"] == name && row["file"] == module)
}

#[test]
fn a_module_that_declares_nothing_does_not_stop_a_commit_or_a_stash() {
    let root = tempdir().expect("temp root");
    let repo = root.path().join("repo");
    fs::create_dir_all(repo.join("web")).expect("create the repository");
    fs::create_dir_all(repo.join("test")).expect("create the repository");
    fs::write(repo.join("web/quiet.js"), QUIET).unwrap();
    fs::write(repo.join("index.js"), INDEX).unwrap();
    fs::write(repo.join("web/lib.js"), LIB).unwrap();
    fs::write(repo.join("test/main.js"), CALLER).unwrap();
    git(&repo, &["init", "--initial-branch=main"]);
    git(&repo, &["config", "user.email", "kin@example.invalid"]);
    git(&repo, &["config", "user.name", "Kin"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["add", "--all"]);
    git(&repo, &["commit", "-m", "two modules that declare nothing"]);
    let runtime = IsolatedDaemonRuntime::new(&repo);
    let init = kin(&runtime, &repo, &["init", "--json"]);
    assert!(
        init.status.success(),
        "kin init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    // An edit admitted and committed while those modules hold no declaration.
    let committed = LIB.replace(
        "\nmodule.exports",
        "\nfunction later(value) {\n  return value;\n}\n\nmodule.exports",
    );
    fs::write(repo.join("web/lib.js"), &committed).unwrap();
    succeed(&runtime, &repo, &["commit", "-m", "Add later"]);
    assert!(
        declares(&runtime, &repo, "later", "web/lib.js"),
        "the commit did not publish the new declaration"
    );

    // An edit sealed in a stash and restored.
    let stashed = committed.replace(
        "\nmodule.exports",
        "\nfunction stashed(value) {\n  return value;\n}\n\nmodule.exports",
    );
    fs::write(repo.join("web/lib.js"), &stashed).unwrap();
    succeed(&runtime, &repo, &["stash", "push", "--yes"]);
    assert_eq!(
        fs::read_to_string(repo.join("web/lib.js")).unwrap(),
        committed,
        "the stash did not return the working copy to its base"
    );
    succeed(&runtime, &repo, &["stash", "pop"]);
    assert_eq!(
        fs::read_to_string(repo.join("web/lib.js")).unwrap(),
        stashed,
        "the stash did not restore the sealed edit"
    );
    assert!(
        declares(&runtime, &repo, "stashed", "web/lib.js"),
        "the restored edit's declaration is not served"
    );
}
