// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Cargo-only authority changes through the actual CLI and owned daemon.
use serde_json::Value;
use std::{collections::BTreeSet, fs, path::Path};
mod common;

const MANIFEST: &str = "[package]\nname='fixture'\nedition='2021'\nautolib=false\nautobins=false\n[lib]\npath='app.rs'\n";

fn git(repo: &Path, args: &[&str]) {
    let out = common::Command::new("git")
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", kin_git::empty_global_git_config())
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn kin(runtime: &common::IsolatedDaemonRuntime, repo: &Path, args: &[&str]) -> Value {
    let out = runtime
        .kin_command()
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", kin_git::empty_global_git_config())
        .env("KIN_DAEMON_BIN", runtime.daemon_bin())
        .env("KIN_DAEMON_DISABLE_LSP", "1")
        .env("KIN_DAEMON_AUTO_EMBED", "0")
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "kin {args:?}: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&out.stdout).into_owned()))
}

fn trace(runtime: &common::IsolatedDaemonRuntime, repo: &Path) -> Value {
    kin(
        runtime,
        repo,
        &[
            "trace-data-flow",
            "--focal",
            "run",
            "--direction",
            "calls",
            "--depth",
            "2",
            "--no-bodies",
        ],
    )
}

/// The ids the graph gives each `work` declared in `file`.
///
/// A trace reply is the agent's semantic surface, which names no file, so a
/// chain step is matched to the declaration it must reach by entity id. The
/// ids come from `kin graph export`, the operator's drawable projection, which
/// still carries each node's file. The declaration stays in the working tree
/// in every state these tests reach, so an empty set would make every
/// withdrawal assertion vacuous and is refused.
fn work_declared_in(
    runtime: &common::IsolatedDaemonRuntime,
    repo: &Path,
    file: &str,
) -> BTreeSet<String> {
    let export = kin(
        runtime,
        repo,
        &["graph", "export", "--limit", "0", "--path", file, "--json"],
    );
    let declared: BTreeSet<String> = export["nodes"]
        .as_array()
        .expect("graph export nodes")
        .iter()
        .filter(|node| {
            node["file"] == file
                && node["name"]
                    .as_str()
                    .is_some_and(|name| name == "work" || name.ends_with("::work"))
        })
        .filter_map(|node| node["id"].as_str().map(str::to_owned))
        .collect();
    assert!(
        !declared.is_empty(),
        "the graph must hold the `work` declared in {file}: {export}"
    );
    declared
}

/// The chain steps that reach the `work` declared in `file`.
fn local_work<'a>(
    runtime: &common::IsolatedDaemonRuntime,
    repo: &Path,
    report: &'a Value,
    file: &str,
) -> Vec<&'a Value> {
    let declared = work_declared_in(runtime, repo, file);
    report["chain"]
        .as_array()
        .expect("trace chain")
        .iter()
        .filter(|step| {
            step["entity_id"]
                .as_str()
                .is_some_and(|id| declared.contains(id))
        })
        .collect()
}

fn owned_work<'a>(
    runtime: &common::IsolatedDaemonRuntime,
    repo: &Path,
    report: &'a Value,
) -> Vec<&'a Value> {
    local_work(runtime, repo, report, "owner.rs")
}

#[test]
fn cargo_only_retarget_and_recovery_change_real_trace_across_cold_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    let runtime = common::IsolatedDaemonRuntime::new(repo);
    git(repo, &["init", "--initial-branch=main"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
    git(repo, &["config", "user.name", "Cargo Authority Fixture"]);
    git(repo, &["config", "user.email", "cargo@example.invalid"]);
    for (path, body) in [
        ("Cargo.toml", MANIFEST),
        ("app.rs", "pub mod owner; pub mod caller;"),
        ("owner.rs", "pub fn work() {}"),
        (
            "caller.rs",
            "use crate::owner::work; pub fn run() { work(); }",
        ),
        ("other.rs", "pub fn unrelated() {}"),
        ("decoy.rs", "pub fn work() {}"),
    ] {
        fs::write(repo.join(path), body).unwrap();
    }
    git(repo, &["add", "--all"]);
    git(repo, &["commit", "-m", "custom Cargo root"]);
    kin(&runtime, repo, &["init", ".", "--json"]);
    let before = trace(&runtime, repo);
    println!("CARGO_INITIAL_TRACE={before}");
    let calls = owned_work(&runtime, repo, &before);
    assert_eq!(
        calls.len(),
        1,
        "exact Cargo root must reach owner: {before}"
    );
    let target = calls[0]["entity_id"].clone();
    let caller = before["focal_entity"]["entity_id"].clone();
    fs::write(
        repo.join("Cargo.toml"),
        MANIFEST.replace("app.rs", "other.rs"),
    )
    .unwrap();
    kin(
        &runtime,
        repo,
        &["commit", "-m", "retarget Cargo root only"],
    );
    let after = trace(&runtime, repo);
    println!("CARGO_RETARGET_TRACE={after}");
    assert!(
        owned_work(&runtime, repo, &after).is_empty(),
        "obsolete Cargo authority must withdraw: {after}"
    );
    assert_eq!(after["focal_entity"]["entity_id"], caller);
    kin(&runtime, repo, &["daemon", "stop"]);
    let cold = trace(&runtime, repo);
    assert!(
        owned_work(&runtime, repo, &cold).is_empty(),
        "cold view must not resurrect Cargo authority: {cold}"
    );
    fs::write(repo.join("Cargo.toml"), MANIFEST).unwrap();
    kin(
        &runtime,
        repo,
        &["commit", "-m", "restore Cargo authority only"],
    );
    let recovered = trace(&runtime, repo);
    println!("CARGO_RECOVERED_TRACE={recovered}");
    assert_eq!(
        owned_work(&runtime, repo, &recovered).len(),
        1,
        "{recovered}"
    );
    assert_eq!(
        owned_work(&runtime, repo, &recovered)[0]["entity_id"],
        target
    );
    assert_eq!(recovered["focal_entity"]["entity_id"], caller);
    kin(&runtime, repo, &["daemon", "stop"]);
    let cold = trace(&runtime, repo);
    assert_eq!(owned_work(&runtime, repo, &cold).len(), 1, "{cold}");
    assert_eq!(owned_work(&runtime, repo, &cold)[0]["entity_id"], target);
}

fn init_git_fixture(repo: &Path) {
    git(repo, &["init", "--initial-branch=main"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
    git(repo, &["config", "user.name", "Cargo Authority Fixture"]);
    git(repo, &["config", "user.email", "cargo@example.invalid"]);
    git(repo, &["add", "--all"]);
    git(repo, &["commit", "-m", "initial Rust project"]);
}

#[test]
fn single_source_crate_manifest_removal_withdraws_and_restores_real_binding() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    let runtime = common::IsolatedDaemonRuntime::new(repo);
    fs::write(repo.join("Cargo.toml"), MANIFEST).unwrap();
    fs::write(
        repo.join("app.rs"),
        "pub mod owner { pub fn work() {} } use crate::owner::work; pub fn run() { work(); }",
    )
    .unwrap();
    init_git_fixture(repo);
    kin(&runtime, repo, &["init", ".", "--json"]);
    let before = trace(&runtime, repo);
    assert_eq!(
        local_work(&runtime, repo, &before, "app.rs").len(),
        1,
        "initial one-source binding: {before}"
    );
    let target = local_work(&runtime, repo, &before, "app.rs")[0]["entity_id"].clone();
    let caller = before["focal_entity"]["entity_id"].clone();
    fs::remove_file(repo.join("Cargo.toml")).unwrap();
    kin(
        &runtime,
        repo,
        &["commit", "-m", "remove only Cargo authority"],
    );
    let after = trace(&runtime, repo);
    assert!(
        local_work(&runtime, repo, &after, "app.rs").is_empty(),
        "one-source removal must withdraw: {after}"
    );
    assert_eq!(after["focal_entity"]["entity_id"], caller);
    kin(&runtime, repo, &["daemon", "stop"]);
    let cold = trace(&runtime, repo);
    assert!(
        local_work(&runtime, repo, &cold, "app.rs").is_empty(),
        "cold withdrawal: {cold}"
    );
    fs::write(repo.join("Cargo.toml"), MANIFEST).unwrap();
    kin(&runtime, repo, &["commit", "-m", "restore Cargo authority"]);
    let recovered = trace(&runtime, repo);
    assert_eq!(
        local_work(&runtime, repo, &recovered, "app.rs").len(),
        1,
        "{recovered}"
    );
    assert_eq!(
        local_work(&runtime, repo, &recovered, "app.rs")[0]["entity_id"],
        target
    );
    kin(&runtime, repo, &["daemon", "stop"]);
    let cold = trace(&runtime, repo);
    assert_eq!(
        local_work(&runtime, repo, &cold, "app.rs").len(),
        1,
        "{cold}"
    );
}

#[test]
fn empty_or_use_only_root_change_rechecks_unchanged_real_callers() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    let runtime = common::IsolatedDaemonRuntime::new(repo);
    const ROOT: &str = "pub mod owner; pub mod caller;";
    for (path, body) in [
        ("Cargo.toml", MANIFEST),
        ("app.rs", ROOT),
        ("owner.rs", "pub fn work() {}"),
        (
            "caller.rs",
            "use crate::owner::work; pub fn run() { work(); }",
        ),
    ] {
        fs::write(repo.join(path), body).unwrap();
    }
    init_git_fixture(repo);
    kin(&runtime, repo, &["init", ".", "--json"]);
    let before = trace(&runtime, repo);
    assert_eq!(owned_work(&runtime, repo, &before).len(), 1, "{before}");
    let target = owned_work(&runtime, repo, &before)[0]["entity_id"].clone();
    let caller = before["focal_entity"]["entity_id"].clone();
    for body in ["", "use std::fmt::Debug;"] {
        fs::write(repo.join("app.rs"), body).unwrap();
        kin(
            &runtime,
            repo,
            &["commit", "-m", "remove module membership from root only"],
        );
        let after = trace(&runtime, repo);
        assert!(
            owned_work(&runtime, repo, &after).is_empty(),
            "entity-free root change must withdraw: {after}"
        );
        assert_eq!(after["focal_entity"]["entity_id"], caller);
        kin(&runtime, repo, &["daemon", "stop"]);
        let cold = trace(&runtime, repo);
        assert!(owned_work(&runtime, repo, &cold).is_empty(), "{cold}");
        fs::write(repo.join("app.rs"), ROOT).unwrap();
        kin(
            &runtime,
            repo,
            &["commit", "-m", "restore original module membership"],
        );
        let recovered = trace(&runtime, repo);
        assert_eq!(
            owned_work(&runtime, repo, &recovered).len(),
            1,
            "{recovered}"
        );
        assert_eq!(
            owned_work(&runtime, repo, &recovered)[0]["entity_id"],
            target
        );
    }
}

#[test]
fn unrelated_python_name_does_not_withdraw_existing_cargo_binding() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path();
    let runtime = common::IsolatedDaemonRuntime::new(repo);
    for (path, body) in [
        ("Cargo.toml", MANIFEST),
        ("app.rs", "pub mod owner; pub mod caller;"),
        ("owner.rs", "pub fn work() {}"),
        (
            "caller.rs",
            "use crate::owner::work; pub fn run() { work(); }",
        ),
    ] {
        fs::write(repo.join(path), body).unwrap();
    }
    init_git_fixture(repo);
    kin(&runtime, repo, &["init", ".", "--json"]);
    let before = trace(&runtime, repo);
    assert_eq!(
        owned_work(&runtime, repo, &before).len(),
        1,
        "initial Rust binding: {before}"
    );
    let target = owned_work(&runtime, repo, &before)[0]["entity_id"].clone();
    for body in ["def work():\n    return 1\n", "def work():\n    return 2\n"] {
        fs::write(repo.join("unrelated.py"), body).unwrap();
        kin(
            &runtime,
            repo,
            &["commit", "-m", "unrelated Python source sharing a name"],
        );
        let after = trace(&runtime, repo);
        assert_eq!(
            owned_work(&runtime, repo, &after).len(),
            1,
            "unrelated source must preserve Rust binding: {after}"
        );
        assert_eq!(owned_work(&runtime, repo, &after)[0]["entity_id"], target);
        kin(&runtime, repo, &["daemon", "stop"]);
        let cold = trace(&runtime, repo);
        assert_eq!(
            owned_work(&runtime, repo, &cold).len(),
            1,
            "cold Rust binding: {cold}"
        );
        assert_eq!(owned_work(&runtime, repo, &cold)[0]["entity_id"], target);
    }
}
