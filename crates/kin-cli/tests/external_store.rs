// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A store kept outside its working tree, through the product.
//!
//! `kin init --store <dir>` and `KIN_STORE` put the store somewhere other than
//! `<working tree>/.kin`. The working tree then carries nothing Kin wrote: no
//! store, no pointer file and no Git exclude rule, so the store never appears
//! in the tree's diff and a read-only checkout can be served. Every later
//! command finds the store from the working tree. Language servers and the
//! embedding pass are off, so these tests measure placement and discovery.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

mod common;

use common::{Command, IsolatedDaemonRuntime};

/// Copy a regular fixture tree without relying on a platform-specific utility.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        let destination = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            assert!(kind.is_file(), "fixtures contain only regular files");
            fs::copy(entry.path(), destination).unwrap();
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A Git repository with two Python files that call each other, cloned to
/// each of `names` beside it.
fn clones(root: &Path, names: &[&str]) -> Vec<PathBuf> {
    let source = root.join("source");
    fs::create_dir_all(source.join("pkg")).unwrap();
    git(&source, &["init", "-q", "--initial-branch=main"]);
    git(&source, &["config", "user.name", "Kin Test"]);
    git(&source, &["config", "user.email", "kin@example.invalid"]);
    fs::write(
        source.join("pkg/helpers.py"),
        "def helper(value):\n    return value + 1\n\n\ndef twice(value):\n    return helper(helper(value))\n",
    )
    .unwrap();
    fs::write(
        source.join("app.py"),
        "from pkg.helpers import helper, twice\n\n\ndef run():\n    return twice(helper(1))\n",
    )
    .unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-q", "-m", "seed"]);
    names
        .iter()
        .map(|name| {
            let clone = root.join(name);
            git(
                root,
                &[
                    "clone",
                    "-q",
                    source.to_str().unwrap(),
                    clone.to_str().unwrap(),
                ],
            );
            git(&clone, &["config", "user.name", "Kin Test"]);
            git(&clone, &["config", "user.email", "kin@example.invalid"]);
            clone
        })
        .collect()
}

fn kin_with(
    runtime: &IsolatedDaemonRuntime,
    dir: &Path,
    args: &[&str],
    envs: &[(&str, &Path)],
) -> std::process::Output {
    let mut command = runtime.kin_command();
    command
        .args(args)
        .env("KIN_EMBED_BACKEND", "cpu")
        .env("KIN_INFER_FORCE_CPU", "1")
        .env("KIN_DAEMON_AUTO_EMBED", "0")
        .env("KIN_DAEMON_DISABLE_LSP", "1")
        .env("KIN_DAEMON_BIN", runtime.daemon_bin())
        .current_dir(dir);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("run kin")
}

fn kin(runtime: &IsolatedDaemonRuntime, dir: &Path, args: &[&str]) -> std::process::Output {
    kin_with(runtime, dir, args, &[])
}

fn succeeded(output: &std::process::Output, what: &str) -> Value {
    assert!(
        output.status.success(),
        "{what} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{what} printed no JSON document ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn canonical(path: &Path) -> String {
    path.canonicalize().unwrap().to_string_lossy().to_string()
}

/// Everything `git status` would ever name, ignored files included.
fn git_status_everything(dir: &Path) -> String {
    git(
        dir,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    )
}

fn stop_daemon(runtime: &IsolatedDaemonRuntime, dir: &Path) {
    let _ = kin(runtime, dir, &["daemon", "stop"]);
}

/// The store answers for the tree the way an in-tree one does: a reference
/// question about a helper names its callers.
fn assert_serves_references(runtime: &IsolatedDaemonRuntime, dir: &Path) {
    let output = kin(runtime, dir, &["refs", "helper"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "kin refs failed: {text}");
    assert!(
        text.contains("twice") && text.contains("run"),
        "the callers of helper are named: {text}"
    );
}

#[test]
fn a_store_kept_outside_the_tree_serves_it_and_never_appears_in_its_diff() {
    let root = tempfile::tempdir().unwrap();
    let a = clones(root.path(), &["A"]).remove(0);
    let runtime = IsolatedDaemonRuntime::new(root.path());
    let exclude_before = fs::read_to_string(a.join(".git/info/exclude")).unwrap_or_default();
    let store = root.path().join("stores/A");

    let init = succeeded(
        &kin(
            &runtime,
            &a,
            &[
                "init",
                "--defer-enrich",
                "--json",
                "--store",
                store.to_str().unwrap(),
            ],
        ),
        "kin init --store",
    );
    assert!(store.join("manifest.json").is_file(), "{init}");
    assert!(!a.join(".kin").exists(), "the tree holds no store");
    assert_eq!(
        git_status_everything(&a),
        "",
        "the working tree's diff names nothing Kin wrote"
    );
    assert_eq!(
        fs::read_to_string(a.join(".git/info/exclude")).unwrap_or_default(),
        exclude_before,
        "no ignore rule is needed, so none is written"
    );

    let located = succeeded(
        &kin(&runtime, &a.join("pkg"), &["store", "path", "--json"]),
        "kin store path",
    );
    assert_eq!(located["schema"], "kin.store-path.v1");
    assert_eq!(located["external"], true, "{located}");
    assert_eq!(located["store"], canonical(&store), "{located}");
    assert_eq!(located["working_dir"], canonical(&a), "{located}");

    assert_serves_references(&runtime, &a);
    // The daemon's own files live in the store too.
    assert!(store.join("daemon.log").is_file());
    assert_eq!(git_status_everything(&a), "");

    // A second init finds the store it already has. (With `--json` a second
    // init refuses by design, so the reopen is asked in words.)
    let again = kin(&runtime, &a, &["init", "--defer-enrich"]);
    assert!(
        again.status.success(),
        "a reopen succeeds: {}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert!(!a.join(".kin").exists());
    stop_daemon(&runtime, &a);
}

#[test]
fn current_only_external_store_keeps_its_binding_when_full_history_attaches() {
    let root = tempfile::tempdir().unwrap();
    let tree = clones(root.path(), &["history"]).remove(0);
    let runtime = IsolatedDaemonRuntime::new(root.path());
    let store = root.path().join("stores/history");
    let exclude_before = fs::read_to_string(tree.join(".git/info/exclude")).unwrap_or_default();

    let init = succeeded(
        &kin(
            &runtime,
            &tree,
            &[
                "init",
                "--defer-enrich",
                "--json",
                "--history",
                "none",
                "--store",
                store.to_str().unwrap(),
            ],
        ),
        "current-only external admission",
    );
    assert_eq!(init["history_state"]["state"], "partial", "{init}");
    assert!(store.join("working-tree.json").is_file());
    assert!(!tree.join(".kin").exists());
    assert_eq!(git_status_everything(&tree), "");

    let history = succeeded(
        &kin(&runtime, &tree, &["init", "--history", "full", "--json"]),
        "attach full history to an external store",
    );
    assert_eq!(history["history_state"]["state"], "complete", "{history}");
    let located = succeeded(
        &kin(&runtime, &tree.join("pkg"), &["store", "path", "--json"]),
        "discover external store after history attachment",
    );
    assert_eq!(located["store"], canonical(&store), "{located}");
    assert_eq!(located["working_dir"], canonical(&tree), "{located}");
    assert_serves_references(&runtime, &tree);
    stop_daemon(&runtime, &tree);
    assert!(!tree.join(".kin").exists());
    assert_eq!(git_status_everything(&tree), "");
    assert_eq!(
        fs::read_to_string(tree.join(".git/info/exclude")).unwrap_or_default(),
        exclude_before,
    );
}

// This fixture removes POSIX write permission bits from the entire checkout.
#[cfg(unix)]
#[test]
fn a_read_only_checkout_is_served_from_a_store_outside_it() {
    use std::os::unix::fs::PermissionsExt;

    fn set_writable(path: &Path, writable: bool) {
        let metadata = fs::symlink_metadata(path).unwrap();
        if metadata.file_type().is_symlink() {
            return;
        }
        let mut mode = metadata.permissions().mode();
        if writable {
            mode |= 0o200;
        } else {
            mode &= !0o222;
        }
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                set_writable(&entry.unwrap().path(), writable);
            }
        }
    }

    let root = tempfile::tempdir().unwrap();
    let b = clones(root.path(), &["B"]).remove(0);
    let runtime = IsolatedDaemonRuntime::new(root.path());
    let store = root.path().join("stores/B");
    // Everything Kin could write into: the tree and its Git directory.
    set_writable(&b, false);

    let init = kin(
        &runtime,
        &b,
        &[
            "init",
            "--defer-enrich",
            "--json",
            "--store",
            store.to_str().unwrap(),
        ],
    );
    let served = init
        .status
        .success()
        .then(|| assert_serves_references(&runtime, &b));
    stop_daemon(&runtime, &b);
    set_writable(&b, true);
    succeeded(&init, "kin init --store over a read-only checkout");
    assert!(served.is_some());
    assert!(!b.join(".kin").exists());
    assert_eq!(git_status_everything(&b), "");
}

#[test]
fn kin_store_and_reuse_carry_a_store_kept_outside_its_tree_to_another() {
    let root = tempfile::tempdir().unwrap();
    let mut trees = clones(root.path(), &["C", "D"]);
    let d = trees.pop().unwrap();
    let c = trees.pop().unwrap();
    let runtime = IsolatedDaemonRuntime::new(root.path());
    let c_store = root.path().join("stores/C");
    let d_store = root.path().join("stores/D");

    succeeded(
        &kin_with(
            &runtime,
            &c,
            &["init", "--defer-enrich", "--json"],
            &[("KIN_STORE", &c_store)],
        ),
        "kin init with KIN_STORE",
    );
    assert!(c_store.join("manifest.json").is_file());
    assert!(!c.join(".kin").exists());
    stop_daemon(&runtime, &c);

    // D is a checkout of the same Git state, so it reuses C's store, copied to
    // D's own location outside D.
    let reused = succeeded(
        &kin(
            &runtime,
            &d,
            &[
                "init",
                "--defer-enrich",
                "--json",
                "--store",
                d_store.to_str().unwrap(),
            ],
        ),
        "kin init --store reusing a prepared store",
    );
    let report = &reused["reuse"];
    assert_eq!(report["route"], "current", "{reused}");
    assert_eq!(report["donor"], canonical(&c), "{reused}");
    assert!(d_store.join("manifest.json").is_file());
    assert!(!d.join(".kin").exists());
    assert_eq!(git_status_everything(&d), "");
    // The copy serves D, not the tree its donor served.
    let d_layout = kin_core::KinLayout::new(d_store.clone());
    assert_eq!(canonical(d_layout.working_dir()), canonical(&d));
    let c_layout = kin_core::KinLayout::new(c_store.clone());
    assert_eq!(canonical(c_layout.working_dir()), canonical(&c));

    assert_serves_references(&runtime, &d);
    stop_daemon(&runtime, &d);
}

#[test]
fn a_copied_store_attached_to_another_checkout_serves_it() {
    let root = tempfile::tempdir().unwrap();
    let mut trees = clones(root.path(), &["E", "F"]);
    let f = trees.pop().unwrap();
    let e = trees.pop().unwrap();
    let runtime = IsolatedDaemonRuntime::new(root.path());
    let e_store = root.path().join("stores/E");
    let f_store = root.path().join("stores/F");

    succeeded(
        &kin(
            &runtime,
            &e,
            &[
                "init",
                "--defer-enrich",
                "--json",
                "--store",
                e_store.to_str().unwrap(),
            ],
        ),
        "kin init --store",
    );
    stop_daemon(&runtime, &e);
    copy_tree(&e_store, &f_store);

    // The store E still serves cannot be taken.
    let refused = kin(
        &runtime,
        &f,
        &["store", "attach", e_store.to_str().unwrap()],
    );
    assert!(!refused.status.success(), "a live store is not taken");

    let attached = succeeded(
        &kin(
            &runtime,
            &f,
            &["store", "attach", f_store.to_str().unwrap(), "--json"],
        ),
        "kin store attach",
    );
    assert_eq!(attached["schema"], "kin.store-attach.v1");
    assert_eq!(attached["working_dir"], canonical(&f), "{attached}");
    assert!(!f.join(".kin").exists());
    assert_eq!(git_status_everything(&f), "");
    assert_serves_references(&runtime, &f);
    stop_daemon(&runtime, &f);

    // E is untouched and still served by its own store.
    let e_layout = kin_core::KinLayout::new(e_store.clone());
    assert_eq!(canonical(e_layout.working_dir()), canonical(&e));
    assert_serves_references(&runtime, &e);
    stop_daemon(&runtime, &e);
}

/// A store kept outside the tree is found by the tree's path, so a checkout
/// that moves loses it and a copy never had it. Any command now says which:
/// a copy is told the store stays with the checkout it was copied from, and a
/// checkout that moved takes its store back and answers, with one line
/// saying so, rather than "not a Kin repository".
#[test]
fn a_moved_checkout_takes_its_store_back_and_a_copy_is_told_whose_it_is() {
    let root = tempfile::tempdir().unwrap();
    let mut trees = clones(root.path(), &["G"]);
    let g = trees.pop().unwrap();
    let runtime = IsolatedDaemonRuntime::new(root.path());
    let g_store = root.path().join("stores/G");
    succeeded(
        &kin(
            &runtime,
            &g,
            &[
                "init",
                "--defer-enrich",
                "--json",
                "--store",
                g_store.to_str().unwrap(),
            ],
        ),
        "kin init --store",
    );
    stop_daemon(&runtime, &g);

    // A copy beside the original: its store stays with the original.
    let copy = root.path().join("G-copy");
    copy_tree(&g, &copy);
    let refused = kin(&runtime, &copy, &["refs", "helper"]);
    let said = String::from_utf8_lossy(&refused.stderr).to_string();
    assert!(!refused.status.success(), "a copy is not served: {said}");
    assert!(
        said.contains("this is a copy of") && said.contains(&canonical(&g)),
        "{said}"
    );
    assert!(said.contains("run `kin init` here"), "{said}");
    fs::remove_dir_all(&copy).unwrap();

    // The checkout moves: the next command takes its store back and answers.
    let moved = root.path().join("moved/G");
    fs::create_dir_all(moved.parent().unwrap()).unwrap();
    fs::rename(&g, &moved).unwrap();
    let answered = kin(&runtime, &moved, &["refs", "helper"]);
    let said = String::from_utf8_lossy(&answered.stderr).to_string();
    assert!(answered.status.success(), "{said}");
    assert!(said.contains("this checkout moved from"), "{said}");
    assert_serves_references(&runtime, &moved);
    let layout = kin_core::KinLayout::new(g_store.clone());
    assert_eq!(canonical(layout.working_dir()), canonical(&moved));
    stop_daemon(&runtime, &moved);
}
