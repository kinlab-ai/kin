// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! `kin review guide --working` and `kin review guide <base>..`, through the
//! real binaries.
//!
//! A guide of one recorded change reads both of its sides from recorded state.
//! The other two do not. A guide of uncommitted work reads "after" from the
//! working tree, which only the daemon that watches the tree can answer, and a
//! guide of a range reads "before" at the change two refs share. Neither can be
//! shown from a graph built by hand: the first needs a real edit that a real
//! daemon noticed, and the second needs changes that `kin commit` recorded one
//! on top of another. These drive `kin` itself and read its JSON.

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

fn require_git(repo: &Path, args: &[&str]) {
    let output = run_git(repo, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn require_kin(
    runtime: &common::IsolatedDaemonRuntime,
    repo: &Path,
    args: &[&str],
) -> std::process::Output {
    let output = runtime
        .kin_command()
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", kin_git::empty_global_git_config())
        .env("KIN_DAEMON_BIN", runtime.daemon_bin())
        .env("KIN_DAEMON_DISABLE_LSP", "1")
        .current_dir(repo)
        .output()
        .expect("run kin");
    assert!(
        output.status.success(),
        "kin {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

const TTL_BEFORE: &str = "pub fn cache_ttl_hours() -> u64 {\n    24\n}\n";
const TTL_AFTER: &str = "pub fn cache_ttl_hours() -> u64 {\n    12\n}\n";
const REPORT: &str =
    "pub fn cache_ttl_seconds() -> u64 {\n    crate::ttl::cache_ttl_hours() * 3600\n}\n";
const REPORT_AFTER: &str =
    "pub fn cache_ttl_seconds() -> u64 {\n    crate::ttl::cache_ttl_hours() * 60 * 60\n}\n";

fn initialize(runtime: &common::IsolatedDaemonRuntime, repo: &Path) {
    fs::create_dir_all(repo.join("src")).expect("create source directory");
    require_git(repo, &["init", "--initial-branch=main"]);
    require_git(repo, &["config", "commit.gpgsign", "false"]);
    require_git(repo, &["config", "user.name", "Ada Lovelace"]);
    require_git(repo, &["config", "user.email", "ada@example.com"]);
    fs::write(repo.join("src/lib.rs"), b"pub mod report;\npub mod ttl;\n").expect("write lib");
    fs::write(repo.join("src/ttl.rs"), TTL_BEFORE).expect("write ttl");
    fs::write(repo.join("src/report.rs"), REPORT).expect("write report");
    require_git(repo, &["add", "--all"]);
    require_git(repo, &["commit", "-m", "first commit"]);
    require_kin(runtime, repo, &["init", ".", "--json"]);
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

/// Every step of a guide, whatever chapter holds it.
fn steps(guide: &Value) -> Vec<&Value> {
    guide["chapters"]
        .as_array()
        .expect("a guide lists its chapters")
        .iter()
        .flat_map(|chapter| {
            chapter["steps"]
                .as_array()
                .expect("a chapter lists its steps")
        })
        .collect()
}

fn step<'a>(guide: &'a Value, name: &str) -> &'a Value {
    steps(guide)
        .into_iter()
        .find(|step| step["name"] == name)
        .unwrap_or_else(|| panic!("the guide should walk {name}: {guide:#}"))
}

/// The edit a person has not committed is walked like a recorded change, with
/// "before" from the last change and "after" from the working tree, and it is
/// gone from the walk the moment it is committed.
///
/// Falsify by reading "after" at the last recorded change in
/// `build_working_guide`: both sides then print 24 and the step reads as
/// unchanged text.
#[test]
fn uncommitted_work_is_walked_from_the_working_tree() {
    let root = tempdir().expect("temp root");
    let repo = root.path().join("repo");
    let runtime = common::IsolatedDaemonRuntime::new(&repo);
    initialize(&runtime, &repo);

    let clean = guide(&runtime, &repo, &["--working"]);
    assert_eq!(clean["kind"], "working");
    assert_eq!(clean["change"]["id"], "WORKSPACE");
    assert!(
        steps(&clean).is_empty(),
        "a tree that matches the last change has nothing to walk: {clean:#}"
    );

    fs::write(repo.join("src/ttl.rs"), TTL_AFTER).expect("edit ttl");
    let working = guide(&runtime, &repo, &["--working"]);
    assert_eq!(working["kind"], "working");
    assert_eq!(
        steps(&working).len(),
        1,
        "one function was edited, and the file's other declarations are not walked: {working:#}"
    );
    assert_eq!(working["working_tree"]["differs"], true, "{working:#}");
    let ttl = step(&working, "cache_ttl_hours");
    assert_eq!(ttl["change"], "modified");
    let before = &ttl["before"];
    let after = &ttl["after"];
    assert_eq!(before["state"], "recorded");
    assert_eq!(after["state"], "recorded");
    assert!(
        before["text"].as_str().unwrap().contains("24"),
        "before is the last recorded change's text: {before}"
    );
    assert!(
        after["text"].as_str().unwrap().contains("12"),
        "after is the working tree's text: {after}"
    );
    assert_eq!(after["working"], true);
    assert!(
        after.get("change_id").is_none(),
        "no change holds the working tree's text yet: {after}"
    );
    assert_eq!(
        before["change_id"], working["change"]["base"],
        "before names the change it was read at"
    );
    let consumers: Vec<&str> = ttl["connections"]["consumers"]
        .as_array()
        .expect("a step lists what reaches it")
        .iter()
        .filter_map(|consumer| consumer["entity"]["name"].as_str())
        .collect();
    assert!(
        consumers.contains(&"cache_ttl_seconds"),
        "the caller in another file reaches the edited function: {consumers:?}"
    );
    assert!(
        working["reviews"].as_array().unwrap().is_empty(),
        "nothing is recorded against work that is not a change yet"
    );

    require_kin(
        &runtime,
        &repo,
        &["commit", "-m", "Shorten the cache to 12 hours"],
    );
    let committed = guide(&runtime, &repo, &["--working"]);
    assert!(
        steps(&committed).is_empty(),
        "committed work is no longer uncommitted: {committed:#}"
    );
    assert_eq!(committed["working_tree"]["differs"], false, "{committed:#}");

    // Work no entity shows is still uncommitted. The guide walks no step for
    // it and says the working tree differs, so a reader is not told it is
    // clean. It is read at once, with no wait for the watcher, because the
    // command admits the working copy before it reads.
    fs::write(
        repo.join("NOTES.md"),
        b"Twelve hours, per the ops review.\n",
    )
    .expect("write notes");
    let notes = guide(&runtime, &repo, &["--working"]);
    assert!(steps(&notes).is_empty(), "{notes:#}");
    assert_eq!(notes["working_tree"]["differs"], true, "{notes:#}");
    assert_eq!(notes["working_tree"]["derived"], true);
    assert!(
        !notes.to_string().contains("NOTES.md"),
        "a guide speaks in entities and names no path for work no entity shows: {notes:#}"
    );
}

/// A range walks every change between two refs as one review. "Before" is the
/// text at the base and "after" the text at the head, so an entity two changes
/// touched is one step and not two.
#[test]
fn a_range_walks_several_recorded_changes_as_one() {
    let root = tempdir().expect("temp root");
    let repo = root.path().join("repo");
    let runtime = common::IsolatedDaemonRuntime::new(&repo);
    initialize(&runtime, &repo);

    fs::write(repo.join("src/ttl.rs"), TTL_AFTER).expect("edit ttl");
    require_kin(
        &runtime,
        &repo,
        &["commit", "-m", "Shorten the cache to 12 hours"],
    );
    let first = guide(&runtime, &repo, &[]);
    let imported = first["change"]["base"]
        .as_str()
        .expect("the first native change names the import it follows")
        .to_string();
    let first_id = first["change"]["id"].as_str().unwrap().to_string();

    fs::write(repo.join("src/report.rs"), REPORT_AFTER).expect("edit report");
    require_kin(&runtime, &repo, &["commit", "-m", "Spell the seconds out"]);
    let second = guide(&runtime, &repo, &[]);
    let second_id = second["change"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        steps(&second).len(),
        1,
        "the newest change alone walks only its own edit: {second:#}"
    );

    let spelled = format!("{imported}..");
    let range = guide(&runtime, &repo, &[spelled.as_str()]);
    assert_eq!(range["kind"], "range");
    assert_eq!(range["range"]["merge_base"], imported.as_str());
    assert_eq!(range["range"]["changes_total"], 2);
    let listed: Vec<&str> = range["range"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|change| change["id"].as_str())
        .collect();
    assert_eq!(
        listed,
        vec![second_id.as_str(), first_id.as_str()],
        "newest first"
    );

    let mut names: Vec<&str> = steps(&range)
        .into_iter()
        .filter_map(|step| step["name"].as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, vec!["cache_ttl_hours", "cache_ttl_seconds"]);
    let ttl = step(&range, "cache_ttl_hours");
    assert!(ttl["before"]["text"].as_str().unwrap().contains("24"));
    assert!(ttl["after"]["text"].as_str().unwrap().contains("12"));
    assert_eq!(ttl["before"]["change_id"], imported.as_str());
    assert_eq!(ttl["after"]["change_id"], second_id.as_str());
    // The caller changed in the same range, and the guide says so.
    let caller = ttl["connections"]["consumers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|consumer| consumer["entity"]["name"] == "cache_ttl_seconds")
        .expect("the caller reaches the edited function");
    assert_eq!(caller["changed_here"], true);

    // The same range by its two flags.
    let flagged = guide(&runtime, &repo, &["--base", imported.as_str()]);
    assert_eq!(flagged["range"]["changes_total"], 2);
    assert_eq!(steps(&flagged).len(), 2);
}
