// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A second checkout of a Git state this machine already prepared reuses the
//! prepared store by identity instead of deriving it again.
//!
//! Every test builds real stores with `kin init` over real Git clones and
//! checks the outcome through the product: the reuse report `kin init --json`
//! prints, the store the reuse published, the store it copied from, the
//! catalog and the registry. Language servers and the embedding pass are off,
//! so the tests measure what the reuse itself does.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

mod common;

use common::{Command, IsolatedDaemonRuntime};

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
/// `A` and `B` beside it. Both clones hold identical refs.
fn source_and_clones(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let source = root.join("source");
    fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q", "--initial-branch=main"]);
    git(&source, &["config", "user.name", "Kin Test"]);
    git(&source, &["config", "user.email", "kin@example.invalid"]);
    fs::write(
        source.join("helpers.py"),
        "def helper(value):\n    return value + 1\n\n\ndef twice(value):\n    return helper(helper(value))\n",
    )
    .unwrap();
    fs::write(
        source.join("app.py"),
        "from helpers import helper, twice\n\n\ndef run():\n    return twice(helper(1))\n",
    )
    .unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-q", "-m", "seed"]);
    let a = root.join("A");
    let b = root.join("B");
    for clone in [&a, &b] {
        git(
            root,
            &[
                "clone",
                "-q",
                source.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        git(clone, &["config", "user.name", "Kin Test"]);
        git(clone, &["config", "user.email", "kin@example.invalid"]);
    }
    (source, a, b)
}

fn kin(runtime: &IsolatedDaemonRuntime, dir: &Path, args: &[&str]) -> std::process::Output {
    runtime
        .kin_command()
        .args(args)
        .env("KIN_EMBED_BACKEND", "cpu")
        .env("KIN_INFER_FORCE_CPU", "1")
        .env("KIN_DAEMON_AUTO_EMBED", "0")
        .env("KIN_DAEMON_DISABLE_LSP", "1")
        .env("KIN_DAEMON_BIN", runtime.daemon_bin())
        .current_dir(dir)
        .output()
        .expect("run kin")
}

/// `kin init --defer-enrich --json` in `dir`, which must succeed.
fn init(runtime: &IsolatedDaemonRuntime, dir: &Path) -> Value {
    let output = kin(runtime, dir, &["init", "--defer-enrich", "--json"]);
    assert!(
        output.status.success(),
        "kin init in {} failed: stdout={} stderr={}",
        dir.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "kin init printed no JSON document ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn stop_daemon(runtime: &IsolatedDaemonRuntime, dir: &Path) {
    let _ = kin(runtime, dir, &["daemon", "stop"]);
}

fn manifest(runtime: &IsolatedDaemonRuntime, dir: &Path) -> (String, String) {
    let manifest = kin_core::KinManifest::load(&runtime.layout_for(dir).manifest_path()).unwrap();
    (manifest.repo_id, manifest.workspace_id)
}

/// Every file of a store, by path, with its content digest, leaving out the
/// running daemon's own files, which say nothing about what the store holds.
fn store_digest(runtime: &IsolatedDaemonRuntime, dir: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .to_string();
            if relative.starts_with("daemon") || relative.starts_with("logs") {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                walk(root, &path, out);
            } else {
                let digest: [u8; 32] = Sha256::digest(fs::read(&path).unwrap()).into();
                out.insert(relative, hex::encode(digest));
            }
        }
    }
    let mut out = BTreeMap::new();
    let layout = runtime.layout_for(dir);
    walk(layout.root(), layout.root(), &mut out);
    out
}

fn roots(runtime: &IsolatedDaemonRuntime, dir: &Path) -> kin_model::RootBundle {
    let layout = runtime.layout_for(dir);
    let binding = kin_core::LocalRepositoryAuthorityBinding::from_layout(&layout).unwrap();
    let frozen = binding
        .freeze_existing_read_only(std::time::Duration::from_secs(10))
        .unwrap();
    frozen.roots().clone()
}

fn catalog(runtime: &IsolatedDaemonRuntime) -> Vec<Value> {
    let path = runtime
        .registry_path()
        .with_file_name(kin_core::prepared_store::PREPARED_STORE_CATALOG_FILE_NAME);
    let Ok(raw) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let value: toml::Value = toml::from_str(&raw).unwrap();
    serde_json::to_value(value).unwrap()["stores"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn canonical(path: &Path) -> String {
    path.canonicalize().unwrap().display().to_string()
}

/// The acceptance scenario: a second checkout of the same Git state reuses the
/// prepared store, derives nothing, serves the same graph, and leaves the
/// store it copied untouched. The registry then holds both locations under
/// one repository identity, and each store stays its own.
#[test]
fn a_second_checkout_reuses_the_prepared_store_and_derives_nothing() {
    let root = tempfile::tempdir().unwrap();
    let (_source, a, b) = source_and_clones(root.path());
    let runtime = IsolatedDaemonRuntime::new(root.path());

    let cold = init(&runtime, &a);
    assert!(
        cold.get("reuse").is_none(),
        "the first checkout has nothing to reuse: {cold}"
    );
    let entries = catalog(&runtime);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["location"], canonical(&a));

    let a_store = store_digest(&runtime, &a);
    let a_roots = roots(&runtime, &a);

    let reused = init(&runtime, &b);
    let report = &reused["reuse"];
    assert_eq!(report["schema"], "kin.init-reuse.v1", "{reused}");
    assert_eq!(report["route"], "current", "{reused}");
    assert_eq!(report["donor"], canonical(&a));
    assert!(
        report["carried"]["entities"].as_u64().unwrap() > 0,
        "{reused}"
    );
    assert_eq!(report["carried"]["tracked_artifacts"], 2, "{reused}");
    assert!(report["carried"]["files"].as_u64().unwrap() > 0);
    for step in [
        "lookup_ms",
        "open_ms",
        "copy_ms",
        "verify_ms",
        "finish_ms",
        "total_ms",
    ] {
        assert!(report["timings"][step].is_u64(), "{step} missing: {reused}");
    }

    // The same repository and workspace, in a store of B's own, holding the
    // same graph.
    assert_eq!(manifest(&runtime, &b), manifest(&runtime, &a));
    let b_roots = roots(&runtime, &b);
    assert_eq!(b_roots.history, a_roots.history);
    assert_eq!(b_roots.ref_state, a_roots.ref_state);
    let layout = runtime.layout_for(&b);
    assert!(
        layout.is_external(),
        "reuse preserves default central storage"
    );
    assert!(!layout.root().join("daemon.pid").exists());
    for parent in [b.as_path(), layout.root().parent().unwrap()] {
        assert!(
            fs::read_dir(parent).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".kin-reuse")),
            "the staging directory was left behind at {}",
            parent.display()
        );
    }
    // Derived by this build: current, and no re-derivation happened.
    assert!(!kin_core::hydration_semantics::standing(&layout).is_gap());

    // The donor is exactly as it was.
    assert_eq!(
        store_digest(&runtime, &a),
        a_store,
        "reusing a store changed it"
    );
    assert_eq!(roots(&runtime, &a), a_roots);

    // Both locations are registered, and both are catalogued.
    let registry = kin_core::registry::KinRegistry::load_from(runtime.registry_path()).unwrap();
    let (repository, _) = manifest(&runtime, &a);
    let locations = registry
        .locations_of(&repository)
        .map(|row| row.path.clone())
        .collect::<Vec<_>>();
    assert_eq!(locations.len(), 2, "{locations:?}");
    let entries = catalog(&runtime);
    let b_entry = entries
        .iter()
        .find(|entry| entry["location"] == canonical(&b))
        .expect("the reused store is catalogued in turn");
    assert_eq!(b_entry["reused_from"], canonical(&a));

    // The reused store is its own: a commit in B moves B and leaves A alone.
    fs::write(
        b.join("helpers.py"),
        "def helper(value):\n    return value + 2\n\n\ndef twice(value):\n    return helper(helper(value))\n",
    )
    .unwrap();
    let committed = kin(&runtime, &b, &["commit", "-m", "change the helper"]);
    assert!(
        committed.status.success(),
        "kin commit in the reused store failed: {}",
        String::from_utf8_lossy(&committed.stderr)
    );
    assert_ne!(roots(&runtime, &b).ref_state, b_roots.ref_state);
    assert_eq!(roots(&runtime, &a).ref_state, a_roots.ref_state);
    assert_eq!(roots(&runtime, &a).history, a_roots.history);
}

/// Whether the store serving `dir` lets language-server enrichment run.
fn enrichment_enabled(runtime: &IsolatedDaemonRuntime, dir: &Path) -> bool {
    kin_core::enrichment_setting::repository_enabled(&runtime.layout_for(dir))
}

/// `kin init <flag> --json` in `dir`, which must reuse a prepared store.
fn reuse_with(runtime: &IsolatedDaemonRuntime, dir: &Path, flag: &str) -> Value {
    let output = kin(runtime, dir, &["init", flag, "--json"]);
    assert!(
        output.status.success(),
        "kin init {flag} in {} failed: stdout={} stderr={}",
        dir.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let reused: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reused["reuse"]["schema"], "kin.init-reuse.v1", "{reused}");
    reused
}

/// A reused copy records the enrichment setting the command asked for, never
/// the donor's. `--no-enrich` over an enriching donor switches the copy off,
/// and the donor stays on.
#[test]
fn no_enrich_switches_the_reused_copy_off_and_leaves_the_donor_on() {
    let root = tempfile::tempdir().unwrap();
    let (_source, a, b) = source_and_clones(root.path());
    let runtime = IsolatedDaemonRuntime::new(root.path());

    init(&runtime, &a);
    assert!(
        enrichment_enabled(&runtime, &a),
        "--defer-enrich leaves it on"
    );

    let reused = reuse_with(&runtime, &b, "--no-enrich");
    assert_eq!(reused["cross_file_enrichment"]["state"], "off", "{reused}");
    assert_eq!(
        reused["cross_file_enrichment"]["reason"], "switched_off",
        "{reused}"
    );
    assert!(
        !enrichment_enabled(&runtime, &b),
        "the copy records enrichment off"
    );
    assert!(
        enrichment_enabled(&runtime, &a),
        "the donor keeps its own setting"
    );
}

/// The other way round: `--defer-enrich` over a donor switched off leaves the
/// copy on, so the next daemon on it runs the sweep, and the donor stays off.
#[test]
fn defer_enrich_leaves_the_reused_copy_on_over_a_switched_off_donor() {
    let root = tempfile::tempdir().unwrap();
    let (_source, a, b) = source_and_clones(root.path());
    let runtime = IsolatedDaemonRuntime::new(root.path());

    let output = kin(&runtime, &a, &["init", "--no-enrich", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!enrichment_enabled(&runtime, &a));

    let reused = reuse_with(&runtime, &b, "--defer-enrich");
    assert_eq!(reused["cross_file_enrichment"]["state"], "owed", "{reused}");
    assert_eq!(
        reused["cross_file_enrichment"]["reason"], "not_requested",
        "{reused}"
    );
    assert!(
        enrichment_enabled(&runtime, &b),
        "the copy records enrichment on"
    );
    assert!(
        !enrichment_enabled(&runtime, &a),
        "the donor keeps its own setting"
    );
}

/// Moving a prepared store changes where it is, not what it is: once a daemon
/// has served it at its new location, a checkout of the same Git state reuses
/// it from there, and the entry for the old location is forgotten.
#[test]
fn a_moved_store_is_reused_from_where_it_is_now() {
    let root = tempfile::tempdir().unwrap();
    let (_source, a, b) = source_and_clones(root.path());
    let runtime = IsolatedDaemonRuntime::new(root.path());
    init(&runtime, &a);
    let moved = root.path().join("moved");
    fs::rename(&a, &moved).unwrap();

    // Serving the store at its new location records it there: `kin init` over
    // an existing store starts its daemon.
    let reopened = kin(&runtime, &moved, &["init"]);
    assert!(
        reopened.status.success(),
        "reopening the moved store failed: {}",
        String::from_utf8_lossy(&reopened.stderr)
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !catalog(&runtime)
        .iter()
        .any(|entry| entry["location"] == canonical(&moved))
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the moved store was never recorded where it is now: {:?}",
            catalog(&runtime)
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    let reused = init(&runtime, &b);
    assert_eq!(reused["reuse"]["route"], "current", "{reused}");
    assert_eq!(reused["reuse"]["donor"], canonical(&moved));
    assert!(
        !catalog(&runtime).iter().any(|entry| {
            entry["location"]
                .as_str()
                .is_some_and(|location| location.ends_with("/A"))
        }),
        "the entry for the old location must be forgotten: {:?}",
        catalog(&runtime)
    );
    assert_eq!(manifest(&runtime, &b), manifest(&runtime, &moved));
}

/// A store another build of this version derived is reused only through `kin
/// upgrade`: the copy is re-derived before it is published and records this
/// build's derivation, and the store it came from keeps its own record.
#[test]
fn a_store_another_build_derived_is_reused_only_through_upgrade() {
    let root = tempfile::tempdir().unwrap();
    let (_source, a, b) = source_and_clones(root.path());
    let runtime = IsolatedDaemonRuntime::new(root.path());
    init(&runtime, &a);

    // Another build of this version derived A.
    let record_path = runtime
        .layout_for(&a)
        .kindb_dir()
        .join("hydration-semantics");
    let mut record: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    record["derivation"] = Value::String(format!("sha256:{}", "0".repeat(64)));
    fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
    let a_record = fs::read(&record_path).unwrap();

    let reused = init(&runtime, &b);
    let report = &reused["reuse"];
    assert_eq!(report["route"], "rederived", "{reused}");
    assert_eq!(report["standing"], "other_build", "{reused}");
    assert!(
        report["reason"]
            .as_str()
            .unwrap()
            .contains("by a build of Kin whose derivation differs from this build's"),
        "{reused}"
    );
    assert_eq!(report["upgrade"]["state"], "upgraded", "{reused}");
    assert!(
        report["upgrade"]["source_files"].as_u64().unwrap() > 0,
        "{reused}"
    );
    let layout = runtime.layout_for(&b);
    assert!(
        !kin_core::hydration_semantics::standing(&layout).is_gap(),
        "{}",
        kin_core::hydration_semantics::standing(&layout).sentence()
    );
    let b_record: Value =
        serde_json::from_slice(&fs::read(layout.kindb_dir().join("hydration-semantics")).unwrap())
            .unwrap();
    assert_eq!(
        b_record["upgrade"]["derivation"],
        kin_core::hydration_semantics::binary_derivation()
    );
    assert_eq!(
        fs::read(&record_path).unwrap(),
        a_record,
        "the donor was changed"
    );

    // A record from before derivation identities takes the same route and is
    // never relabelled as current by the copy alone. B is now a current
    // prepared store for the same state and would be chosen first, so it goes.
    stop_daemon(&runtime, &b);
    fs::remove_dir_all(runtime.layout_for(&b).root()).unwrap();
    let c = root.path().join("C");
    git(
        root.path(),
        &[
            "clone",
            "-q",
            root.path().join("source").to_str().unwrap(),
            c.to_str().unwrap(),
        ],
    );
    git(&c, &["config", "user.name", "Kin Test"]);
    git(&c, &["config", "user.email", "kin@example.invalid"]);
    record.as_object_mut().unwrap().remove("derivation");
    fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
    let legacy = init(&runtime, &c);
    assert_eq!(legacy["reuse"]["route"], "rederived", "{legacy}");
    assert!(
        legacy["reuse"]["reason"]
            .as_str()
            .unwrap()
            .contains("recorded no derivation identity"),
        "{legacy}"
    );
}

/// Changed inputs are never reused: another commit, uncommitted work, and a
/// prepared store that moved on since its admission each admit the checkout
/// from scratch, and say why when a prepared store was passed over.
#[test]
fn changed_inputs_are_not_reused() {
    let root = tempfile::tempdir().unwrap();
    let (_source, a, b) = source_and_clones(root.path());
    let runtime = IsolatedDaemonRuntime::new(root.path());
    init(&runtime, &a);

    // Uncommitted work in the checkout.
    fs::write(b.join("scratch.py"), "x = 1\n").unwrap();
    let output = kin(&runtime, &b, &["init", "--defer-enrich"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("Reused the prepared store"),
        "a checkout with uncommitted work must not reuse"
    );
    assert_ne!(
        manifest(&runtime, &b),
        manifest(&runtime, &a),
        "B was admitted from scratch"
    );

    // Another commit: another Git state, so no candidate at all.
    let c = root.path().join("C");
    git(
        root.path(),
        &[
            "clone",
            "-q",
            root.path().join("source").to_str().unwrap(),
            c.to_str().unwrap(),
        ],
    );
    git(&c, &["config", "user.name", "Kin Test"]);
    git(&c, &["config", "user.email", "kin@example.invalid"]);
    fs::write(c.join("more.py"), "def more():\n    return 3\n").unwrap();
    git(&c, &["add", "more.py"]);
    git(&c, &["commit", "-q", "-m", "more"]);
    let fresh = init(&runtime, &c);
    assert!(fresh.get("reuse").is_none(), "{fresh}");
    assert_ne!(manifest(&runtime, &c), manifest(&runtime, &a));

    // A prepared store that moved on since its admission, here with a native
    // commit, is passed over and named. B, admitted from scratch above, is a
    // valid prepared store for the same state and would be reused, so only A
    // is left to find.
    stop_daemon(&runtime, &b);
    fs::remove_dir_all(runtime.layout_for(&b).root()).unwrap();
    fs::write(
        a.join("helpers.py"),
        "def helper(value):\n    return value\n",
    )
    .unwrap();
    let committed = kin(&runtime, &a, &["commit", "-m", "native change"]);
    assert!(
        committed.status.success(),
        "{}",
        String::from_utf8_lossy(&committed.stderr)
    );
    git(&a, &["checkout", "-q", "--", "helpers.py"]);
    let d = root.path().join("D");
    git(
        root.path(),
        &[
            "clone",
            "-q",
            root.path().join("source").to_str().unwrap(),
            d.to_str().unwrap(),
        ],
    );
    let output = kin(&runtime, &d, &["init", "--defer-enrich"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("was not reused") && stderr.contains(&canonical(&a)),
        "a passed-over store must be named with its reason: {stderr}"
    );
    assert_ne!(manifest(&runtime, &d), manifest(&runtime, &a));
}

/// `kin clone` over Git transport reuses a prepared store for the state its
/// clone produced, exactly as `kin init` does.
#[test]
fn a_git_clone_of_a_prepared_state_reuses_it() {
    let root = tempfile::tempdir().unwrap();
    let (source, a, _b) = source_and_clones(root.path());
    let runtime = IsolatedDaemonRuntime::new(root.path());
    init(&runtime, &a);

    let target = root.path().join("cloned");
    let output = kin(
        &runtime,
        root.path(),
        &[
            "clone",
            "--verbose",
            source.to_str().unwrap(),
            target.to_str().unwrap(),
        ],
    );
    assert!(
        output.status.success(),
        "kin clone failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Reused the prepared store at") && stdout.contains(&canonical(&a)),
        "{stdout}"
    );
    assert_eq!(manifest(&runtime, &target), manifest(&runtime, &a));
}
