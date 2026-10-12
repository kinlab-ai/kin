// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! What a guide counts in a real Python store, through the real binaries.
//!
//! A Python class's recorded text holds its methods, so a method edit records
//! the class as modified too, and `@overload` stubs are entities of their own
//! that share their implementation's name. Neither shows up in a graph built
//! by hand: the first needs the parser's spans and the store's text, and the
//! second the Python adapter's own word on which entities only declare. These
//! drive `kin` itself on a small package and read its JSON.

use serde_json::Value;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

mod common;

use common::Command;

fn run_git(repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", kin_git::empty_global_git_config())
        .current_dir(repo)
        .output()
        .expect("run git")
}

fn require_git(repo: &Path, args: &[&str]) -> String {
    let output = run_git(repo, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn kin(
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
    let output = kin(runtime, repo, args);
    assert!(
        output.status.success(),
        "kin {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn guide(runtime: &common::IsolatedDaemonRuntime, repo: &Path, args: &[&str]) -> Value {
    let mut command = vec!["review", "guide"];
    command.extend_from_slice(args);
    command.push("--json");
    let output = require_kin(runtime, repo, &command);
    let guide: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "kin {command:?} should print a guide as JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(guide["schema"], "kin.review.guide.v1");
    guide
}

fn step_names(guide: &Value) -> Vec<String> {
    let mut names: Vec<String> = guide["chapters"]
        .as_array()
        .expect("a guide lists its chapters")
        .iter()
        .flat_map(|chapter| {
            chapter["steps"]
                .as_array()
                .expect("a chapter lists its steps")
        })
        .filter_map(|step| step["name"].as_str().map(str::to_string))
        .collect();
    names.sort();
    names
}

const SIGNER: &str = "class Signer:\n    default_digest_method = \"sha1\"\n\n    def __init__(self, secret_key):\n        self.secret_key = secret_key\n\n    def verify_signature(self, value, sig):\n        return sig == value\n";

const SERIALIZER: &str = "from typing import overload\n\nfrom .signer import Signer\n\n\nclass Serializer:\n    @overload\n    def __init__(self, secret_key: str, signer: None = None) -> None: ...\n\n    @overload\n    def __init__(self, secret_key: str, signer: type[Signer]) -> None: ...\n\n    def __init__(self, secret_key, signer=None):\n        self.signer = (signer or Signer)(secret_key)\n";

/// One method's edit is one step. The class that holds it is read through
/// it and named as such, a class whose own attribute moved is walked, a short
/// Git id and a change prefix are taken as typed, and `@overload` stubs that
/// reach a changed class read as one callable.
///
/// Falsify by dropping the `through` filter in `assemble`: the method edit
/// then walks `Signer` as a step of its own.
#[test]
fn a_method_edit_reads_through_its_class_and_overloads_read_as_one() {
    let root = tempdir().expect("temp root");
    let repo = root.path().join("repo");
    let runtime = common::IsolatedDaemonRuntime::new(&repo);
    fs::create_dir_all(repo.join("src/pkg")).expect("create package");
    require_git(&repo, &["init", "--initial-branch=main"]);
    require_git(&repo, &["config", "commit.gpgsign", "false"]);
    require_git(&repo, &["config", "user.name", "Ada Lovelace"]);
    require_git(&repo, &["config", "user.email", "ada@example.com"]);
    fs::write(repo.join("src/pkg/__init__.py"), b"").expect("write package");
    fs::write(repo.join("src/pkg/signer.py"), SIGNER).expect("write signer");
    fs::write(repo.join("src/pkg/serializer.py"), SERIALIZER).expect("write serializer");
    require_git(&repo, &["add", "--all"]);
    require_git(&repo, &["commit", "-m", "Add the signer and serializer"]);
    let short_git = require_git(&repo, &["rev-parse", "--short=7", "HEAD"]);
    require_kin(&runtime, &repo, &["init", ".", "--json"]);

    // The short Git id `git log --oneline` prints names the imported change.
    let imported = guide(&runtime, &repo, &[short_git.as_str()]);
    assert!(
        imported["change"]["git_commit"]
            .as_str()
            .is_some_and(|commit| commit.starts_with(&short_git)),
        "{short_git} should name the imported commit: {:#}",
        imported["change"]
    );
    assert_eq!(imported["change"]["author"], "Ada Lovelace");

    fs::write(
        repo.join("src/pkg/signer.py"),
        SIGNER.replace("return sig == value", "return bool(sig) and sig == value"),
    )
    .expect("edit the method");
    require_kin(
        &runtime,
        &repo,
        &["commit", "-m", "Refuse an empty signature"],
    );
    let method = guide(&runtime, &repo, &[]);
    assert_eq!(
        step_names(&method),
        vec!["Signer.verify_signature"],
        "the method is the step and the class is read through it: {method:#}"
    );
    assert_eq!(
        method["summary"]["containers_through_members"], 1,
        "{:#}",
        method["summary"]
    );
    assert_eq!(method["summary"]["through_members"][0], "Signer");

    // The footer's command takes the twelve characters the page shows.
    let id = method["change"]["id"].as_str().unwrap().to_string();
    let details = require_kin(&runtime, &repo, &["review", &id[..12], "--details"]);
    assert!(
        String::from_utf8_lossy(&details.stdout).contains(&id),
        "kin review {} --details reviews that change: {}",
        &id[..12],
        String::from_utf8_lossy(&details.stdout)
    );

    // An attribute of the class is the class's own text: it is walked.
    fs::write(
        repo.join("src/pkg/signer.py"),
        SIGNER
            .replace("return sig == value", "return bool(sig) and sig == value")
            .replace("\"sha1\"", "\"sha256\""),
    )
    .expect("edit the attribute");
    require_kin(&runtime, &repo, &["commit", "-m", "Default to sha256"]);
    let attribute = guide(&runtime, &repo, &[]);
    let names = step_names(&attribute);
    assert!(
        names
            .iter()
            .any(|name| name == "Signer" || name.starts_with("Signer.default_digest_method")),
        "a class attribute that moved is walked: {attribute:#}"
    );

    // A change to what the class declares lists what reaches it, and the
    // stubs of `Serializer.__init__` are one callable there.
    fs::write(
        repo.join("src/pkg/signer.py"),
        SIGNER
            .replace("return sig == value", "return bool(sig) and sig == value")
            .replace("\"sha1\"", "\"sha256\"")
            .replace("class Signer:", "class Signer(object):"),
    )
    .expect("edit the declaration");
    require_kin(
        &runtime,
        &repo,
        &["commit", "-m", "Make the signer an object"],
    );
    let declared = guide(&runtime, &repo, &[]);
    let signer = declared["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|chapter| chapter["steps"].as_array().unwrap())
        .find(|step| step["name"] == "Signer")
        .unwrap_or_else(|| panic!("the class declaration changed: {declared:#}"));
    let inits: Vec<&Value> = signer["connections"]["consumers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["entity"]["name"] == "Serializer.__init__")
        .collect();
    assert_eq!(
        inits.len(),
        1,
        "the implementation and its stubs are one row: {:#}",
        signer["connections"]
    );
    assert!(
        inits[0]["overloads"].as_u64().unwrap_or(0) >= 1,
        "the row counts the stubs it stands for: {:#}",
        inits[0]
    );
}
