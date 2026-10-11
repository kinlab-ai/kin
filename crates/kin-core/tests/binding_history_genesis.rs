// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use kin_model::{BindingHistoryObservation, EntityStore};
use std::path::Path;
use std::sync::Arc;

#[cfg(unix)]
#[test]
fn kin_process_group_guardian_worker() {
    let requested = std::env::var_os(kin_daemon_spawn::PROCESS_GROUP_GUARDIAN_MODE_ENV).is_some();
    let dispatched = kin_daemon_spawn::run_process_group_guardian_if_requested()
        .expect("dispatch owned Git fixture guardian");
    assert_eq!(dispatched, requested);
}

fn git_batch(root: &Path, steps: &[&[&str]]) {
    let mut batch = kin_git::test_support::FixtureGitBatch::new();
    for args in steps {
        batch.git(*args);
    }
    batch.run_in(root);
}

/// How the workspace's lineage starts: the qualification of its first step.
fn lineage_start(result: &kin_core::InitResult) -> Option<String> {
    let manager = kin_db::RepositoryAuthorityManager::open(
        result.repository_id.clone(),
        Arc::new(kin_db::LocalFileBackend::new(result.layout.kindb_dir())),
    )
    .unwrap();
    let metadata = serde_json::to_value(manager.read_authority().metadata()).unwrap();
    metadata["binding_history"][0]["proof"][0]["qualification"]
        .as_str()
        .map(str::to_string)
}

fn observe(result: &kin_core::InitResult) -> (BindingHistoryObservation, usize) {
    let manager = kin_db::RepositoryAuthorityManager::open(
        result.repository_id.clone(),
        Arc::new(kin_db::LocalFileBackend::new(result.layout.kindb_dir())),
    )
    .unwrap();
    let lease = manager.read_authority();
    let snapshot = lease
        .workspace_graph_snapshot(&result.workspace_id)
        .unwrap()
        .unwrap();
    let calls = snapshot
        .relations
        .values()
        .filter(|r| r.kind == kin_model::RelationKind::Calls)
        .count();
    let graph = kin_db::InMemoryGraph::from_snapshot_without_text_index(snapshot).unwrap();
    (graph.binding_history_observation(), calls)
}

#[test]
fn binding_history_actual_native_genesis_is_checked_on_cold_read() {
    let root = tempfile::tempdir().unwrap();
    let initialized = kin_core::init(root.path()).unwrap();
    assert!(matches!(
        observe(&initialized).0,
        BindingHistoryObservation::Checked { .. }
    ));
}

/// A Git history whose every transition is accounted for starts its lineage at
/// the history's genesis. One with a withdrawal the transition verifier cannot
/// account for makes no claim about that history: its lineage starts at the
/// bootstrap instead, as a re-derivation of the graph init just derived, so the
/// store is still checked from its first generation.
#[test]
fn binding_history_actual_git_history_genesis_is_claimed_only_when_every_withdrawal_is_accounted() {
    for removed in [false, true] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("caller.py"),
            "from local import work\ndef run():\n    return work(value=1)\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("local.py"),
            "def work(value):\n    return value\n",
        )
        .unwrap();
        // The same Git commands in the same order, one fixture launch for
        // each run of them, since every launch starts its own guardian.
        let excludes = root.path().join(".git/fixture-excludes");
        let hooks = root.path().join(".git/hooks");
        git_batch(
            root.path(),
            &[
                &["init", "--initial-branch=main"],
                &["config", "user.email", "binding-history@example.invalid"],
                &["config", "user.name", "Binding History Test"],
                &["config", "core.excludesFile", excludes.to_str().unwrap()],
                &["config", "core.hooksPath", hooks.to_str().unwrap()],
                &["add", "."],
                &["commit", "-m", "Actual initial local binding"],
            ],
        );
        // Written once `git init` has made `.git`, before init reads it.
        std::fs::write(&excludes, "").unwrap();
        if removed {
            std::fs::remove_file(root.path().join("local.py")).unwrap();
        } else {
            std::fs::write(
                root.path().join("README.md"),
                "Healthy retained local binding.\n",
            )
            .unwrap();
        }
        git_batch(
            root.path(),
            &[
                &["add", "-A"],
                &["commit", "-m", "Second historical observation"],
            ],
        );
        let initialized = kin_core::init_from_git(root.path()).unwrap();
        let (observation, calls) = observe(&initialized);
        if removed {
            assert!(
                matches!(observation, BindingHistoryObservation::Checked { .. }),
                "{observation:?}"
            );
            assert_eq!(
                lineage_start(&initialized).as_deref(),
                Some("CheckedRederivation"),
                "an unaccounted withdrawal must not be claimed as a checked history genesis"
            );
        } else {
            assert_eq!(
                lineage_start(&initialized).as_deref(),
                Some("CheckedHistoryGenesis")
            );
            assert!(calls > 0, "fixture must contain a real admitted call");
            assert!(
                matches!(observation, BindingHistoryObservation::Checked { .. }),
                "{observation:?}"
            );
            let manager = kin_db::RepositoryAuthorityManager::open(
                initialized.repository_id.clone(),
                Arc::new(kin_db::LocalFileBackend::new(
                    initialized.layout.kindb_dir(),
                )),
            )
            .unwrap();
            let before = manager
                .read_authority()
                .workspace_graph_snapshot(&initialized.workspace_id)
                .unwrap()
                .unwrap();
            let call = before
                .relations
                .values()
                .find(|relation| relation.kind == kin_model::RelationKind::Calls)
                .unwrap()
                .clone();
            let graph =
                kin_db::InMemoryGraph::from_snapshot_without_text_index(before.clone()).unwrap();
            graph.remove_relation(&call.id).unwrap();
            assert_eq!(
                graph.binding_history_observation(),
                BindingHistoryObservation::Unproven
            );
            assert!(!graph.qualify_binding_history_derivation(&before,
                &kin_index::binding_history::LocalBindingHistoryVerifier, &|digest| manager.load_source_blob(digest)).unwrap(),
                "removing an actual call without an obligation or semantic discharge must remain Unknown");
            let mut unqualified = before.clone();
            unqualified.verified_binding_history = None;
            graph.upsert_relation(&call).unwrap();
            assert!(
                !graph
                    .qualify_binding_history_derivation(
                        &unqualified,
                        &kin_index::binding_history::LocalBindingHistoryVerifier,
                        &|digest| manager.load_source_blob(digest)
                    )
                    .unwrap(),
                "current body and positive edges cannot launder unknown prior history"
            );
        }
    }
}
