// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Preview and confirmation run against ordinary persisted synthetic stores.
//! No supervisor or daemon process is started by these in-process route tests.

use super::*;
use kin_cli::commands::merge::{
    MergeConfirmRequest, MergeOutcome, MergePreviewRequest, MergePreviewResponse,
};

async fn post(
    app: &axum::Router,
    route: &str,
    payload: &impl serde::Serialize,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(
            Request::post(route)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&bytes)})),
    )
}

async fn preview(app: &axum::Router) -> MergePreviewResponse {
    let (status, body) = post(
        app,
        "/commands/merge/preview",
        &MergePreviewRequest {
            source: kin_model::RefName::branch(b"feature").unwrap(),
        },
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_value(body).unwrap()
}

fn confirm(preview: &MergePreviewResponse) -> MergeConfirmRequest {
    MergeConfirmRequest {
        source: preview.expected.source_ref.clone(),
        operation_id: kin_model::OperationId::new(),
        actor: AuthorId::new("Synthetic merge preview test"),
        expected: preview.expected.clone(),
    }
}

/// File names and bytes, including symlink destinations. Test IO is the
/// explicit boundary: product preview never walks the working filesystem.
fn bytes_below(root: &std::path::Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn visit(
        root: &std::path::Path,
        at: &std::path::Path,
        result: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>,
    ) {
        for entry in std::fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            if kind.is_dir() {
                result.insert(relative, Vec::new());
                visit(root, &path, result);
            } else if kind.is_symlink() {
                result.insert(
                    relative,
                    std::fs::read_link(&path)
                        .unwrap()
                        .as_os_str()
                        .as_encoded_bytes()
                        .to_vec(),
                );
            } else {
                result.insert(relative, std::fs::read(path).unwrap());
            }
        }
    }
    let mut result = std::collections::BTreeMap::new();
    visit(root, root, &mut result);
    result
}

/// Keep complete path/byte equality, but never print repository bodies when
/// it fails. A bounded digest report identifies the changed fixture members.
fn assert_bytes_unchanged(
    root: &std::path::Path,
    before: &std::collections::BTreeMap<PathBuf, Vec<u8>>,
    reason: &str,
) {
    use sha2::{Digest, Sha256};
    let after = bytes_below(root);
    let paths: std::collections::BTreeSet<_> = before.keys().chain(after.keys()).collect();
    let changed: Vec<_> = paths
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .collect();
    let describe = |value: Option<&Vec<u8>>| match value {
        Some(bytes) => format!(
            "{} bytes sha256={}",
            bytes.len(),
            hex::encode(Sha256::digest(bytes))
        ),
        None => "absent".to_string(),
    };
    let details: Vec<_> = changed
        .iter()
        .take(16)
        .map(|path| {
            format!(
                "{}: {} -> {}",
                path.display(),
                describe(before.get(*path)),
                describe(after.get(*path))
            )
        })
        .collect();
    assert!(
        changed.is_empty(),
        "{reason}: {} changed path(s); first {}:\n{}",
        changed.len(),
        details.len(),
        details.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(repository_commit)]
async fn merge_preview_writes_nothing_and_competing_confirmations_fast_forward_once() {
    let (state, layout, repository, main, feature) =
        universal_branch_test_state("merge-preview-read-only");
    // Router startup initializes package-registry directories. A production
    // request reaches an already-built router, so measure that same boundary.
    let app = router(Arc::clone(&state));
    let binding = state.local_repository_authority_binding().unwrap();
    let namespace = layout.kindb_namespace_path(binding.repository_id().as_str());
    let storage_before = bytes_below(&namespace);
    let working_before = bytes_below(&repository);
    let held = preview(&app).await;
    assert_eq!(held.outcome, MergeOutcome::FastForward);
    assert_eq!(held.expected.ours_change, main);
    assert_eq!(held.expected.theirs_change, feature);
    assert!(!held.limitations.is_empty());
    assert_eq!(held.conflicts_total, 0);
    assert_bytes_unchanged(
        &namespace,
        &storage_before,
        "preview must not recover, clean, or publish storage",
    );
    assert_bytes_unchanged(
        &repository,
        &working_before,
        "preview must not admit or project working files",
    );
    assert_eq!(branch_change(&state), main);

    let request = confirm(&held);
    let competing_request = confirm(&held);
    let gate = state.coordination_gate.lock().await;
    let first_app = app.clone();
    let second_app = app.clone();
    let mut first =
        Box::pin(async move { post(&first_app, "/commands/merge/confirm", &request).await });
    let mut second =
        Box::pin(
            async move { post(&second_app, "/commands/merge/confirm", &competing_request).await },
        );
    // Both route futures reach the held mutation gate before either may
    // proceed. Spawn them on separate runtime workers after releasing it;
    // whichever wins must invalidate the other's identical preview.
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(first.as_mut(), cx).is_pending());
        assert!(std::future::Future::poll(second.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    drop(gate);
    let (first, second) = tokio::join!(tokio::spawn(first), tokio::spawn(second));
    let (first, second) = (first.unwrap(), second.unwrap());
    let statuses = [first.0, second.0];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::OK)
            .count(),
        1,
        "{first:?} {second:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::CONFLICT)
            .count(),
        1,
        "{first:?} {second:?}"
    );
    let success = if first.0 == StatusCode::OK {
        first.1
    } else {
        second.1
    };
    assert_eq!(success["report"]["outcome"], "fast_forward");
    assert_eq!(branch_change(&state), feature);
    drop(app);
    drop(state);
    let reopened = Arc::new(DaemonState::open(layout).unwrap());
    assert_eq!(
        branch_change(&reopened),
        feature,
        "confirmed result must survive reopen"
    );
    let reopened_app = router(Arc::clone(&reopened));
    let held = preview(&reopened_app).await;
    assert_eq!(held.outcome, MergeOutcome::AlreadyUpToDate);
    let before_noop = bytes_below(&namespace);
    let (status, body) = post(&reopened_app, "/commands/merge/confirm", &confirm(&held)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["report"]["outcome"], "already_up_to_date");
    assert_eq!(body["mutated"], false);
    assert_bytes_unchanged(
        &namespace,
        &before_noop,
        "already-up-to-date confirmation must not publish",
    );
}

#[tokio::test]
#[serial_test::serial(repository_commit)]
async fn merge_confirmation_refuses_each_changed_expectation_without_publication() {
    let (state, layout, _, _, _) = universal_branch_test_state("merge-preview-expectations");
    let app = router(Arc::clone(&state));
    let held = preview(&app).await;
    let mut requests = Vec::new();
    let mut request = confirm(&held);
    request.expected.workspace_id = kin_model::WorkspaceId::new();
    requests.push(request);
    let mut request = confirm(&held);
    request.expected.repository_id = kin_model::RepositoryId::new("different-repository").unwrap();
    requests.push(request);
    let mut request = confirm(&held);
    request.expected.roots.generation += 1;
    requests.push(request);
    // A matching sequence alone is insufficient: every authority partition
    // must match the preview, including collaboration and receiver-local state.
    for partition in [
        "history",
        "ref_state",
        "ref_log",
        "collaboration",
        "replication",
        "local_state",
    ] {
        let mut wire = serde_json::to_value(confirm(&held)).unwrap();
        wire["expected"]["roots"][partition]["hash"] =
            serde_json::json!(Hash256::from_bytes([7; 32]));
        requests.push(serde_json::from_value(wire).unwrap());
    }
    let mut request = confirm(&held);
    request.expected.workspace_tree_hash = Hash256::from_bytes([7; 32]);
    requests.push(request);
    let mut request = confirm(&held);
    request.expected.ours_change = held.expected.theirs_change;
    requests.push(request);
    let mut request = confirm(&held);
    request.expected.theirs_change = held.expected.ours_change;
    requests.push(request);
    let mut request = confirm(&held);
    request.expected.target_ref = kin_model::RefName::branch(b"another-target").unwrap();
    requests.push(request);
    let mut request = confirm(&held);
    request.expected.source_ref = kin_model::RefName::branch(b"another-source").unwrap();
    requests.push(request);
    let namespace = layout.kindb_namespace_path(held.expected.repository_id.as_str());
    let before = bytes_below(&namespace);
    for request in requests {
        let (status, body) = post(&app, "/commands/merge/confirm", &request).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.to_string().contains("stale merge preview"), "{body}");
        assert_eq!(body["refused_before_write"], true);
        assert_eq!(branch_change(&state), held.expected.ours_change);
    }
    assert_bytes_unchanged(&namespace, &before, "stale expectations must not publish");
}

#[tokio::test]
#[serial_test::serial(repository_commit)]
async fn merge_confirmation_refuses_a_target_committed_after_preview() {
    let (state, layout, repository, _, _) =
        universal_branch_test_state("merge-preview-target-moved");
    let app = router(Arc::clone(&state));
    let held = preview(&app).await;
    std::fs::write(repository.join("selected/later.txt"), b"later work\n").unwrap();
    let current = commit_through_api(
        &app,
        kin_model::OperationId::new(),
        "advance target after preview",
    )
    .await;
    assert_ne!(current, held.expected.ours_change);
    let (status, body) = post(&app, "/commands/merge/confirm", &confirm(&held)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.to_string().contains("stale merge preview"), "{body}");
    assert_eq!(branch_change(&state), current);
    let fresh = preview(&app).await;
    assert_eq!(fresh.expected.ours_change, current);
    assert_eq!(fresh.outcome, MergeOutcome::Merged);
    let (status, body) = post(&app, "/commands/merge/confirm", &confirm(&fresh)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["report"]["outcome"], "merged");
    assert_eq!(body["mutated"], true);
    let merged = branch_change(&state);
    assert_ne!(merged, current);
    assert_ne!(merged, fresh.expected.theirs_change);
    assert_eq!(
        std::fs::read(repository.join("selected/later.txt")).unwrap(),
        b"later work\n"
    );
    assert_eq!(
        std::fs::read(repository.join("selected/compose.yaml")).unwrap(),
        b"services:\n  api:\n    image: feature\n"
    );
    drop(app);
    drop(state);
    let reopened = Arc::new(DaemonState::open(layout).unwrap());
    assert_eq!(branch_change(&reopened), merged);
}

#[tokio::test]
#[serial_test::serial(repository_commit)]
async fn merge_confirmation_refuses_a_source_committed_after_preview() {
    let (state, _, repository, main, _) = universal_branch_test_state("merge-preview-source-moved");
    let app = router(Arc::clone(&state));
    let held = preview(&app).await;
    let switch = |name: &[u8]| kin_cli::commands::branch::BranchRequest::Switch {
        name: kin_model::RefName::branch(name).unwrap(),
        operation_id: kin_model::OperationId::new(),
        actor: AuthorId::new("Synthetic merge preview test"),
    };
    let (status, body) = post(&app, "/commands/branch", &switch(b"feature")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    std::fs::write(
        repository.join("selected/later-source.txt"),
        b"later source work\n",
    )
    .unwrap();
    let current = commit_through_api(
        &app,
        kin_model::OperationId::new(),
        "advance source after preview",
    )
    .await;
    assert_ne!(current, held.expected.theirs_change);
    let (status, body) = post(&app, "/commands/branch", &switch(b"main")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = post(&app, "/commands/merge/confirm", &confirm(&held)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.to_string().contains("stale merge preview"), "{body}");
    assert_eq!(
        branch_change(&state),
        main,
        "new source head must not be silently merged"
    );
    let fresh = preview(&app).await;
    assert_eq!(fresh.expected.theirs_change, current);
}

#[tokio::test]
#[serial_test::serial(repository_commit)]
async fn merge_conflict_preview_does_not_park_until_confirmed() {
    let (state, layout, repository, _, _) = universal_branch_test_state("merge-preview-conflicts");
    let app = router(Arc::clone(&state));
    std::fs::write(
        repository.join("selected/compose.yaml"),
        b"services:\n  api:\n    image: different-main\n",
    )
    .unwrap();
    let current = commit_through_api(
        &app,
        kin_model::OperationId::new(),
        "create conflict on target",
    )
    .await;
    let held = preview(&app).await;
    assert_eq!(held.outcome, MergeOutcome::Conflicted);
    assert!(held.conflicts_total > 0);
    let frozen = state
        .local_repository_authority_binding()
        .unwrap()
        .freeze_existing_read_only(Duration::from_secs(2))
        .unwrap();
    assert!(crate::repository_merge_state::workspace_merge_record(
        frozen.authority().metadata(),
        held.expected.workspace_id
    )
    .is_none());
    drop(frozen);
    let (status, body) = post(&app, "/commands/merge/confirm", &confirm(&held)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["report"]["outcome"], "conflicted");
    assert_eq!(body["mutated"], true);
    assert_eq!(
        branch_change(&state),
        current,
        "parking conflicts must not advance target"
    );
    drop(app);
    drop(state);
    let reopened = Arc::new(DaemonState::open(layout).unwrap());
    let frozen = reopened
        .local_repository_authority_binding()
        .unwrap()
        .freeze_existing_read_only(Duration::from_secs(2))
        .unwrap();
    let record = crate::repository_merge_state::workspace_merge_record(
        frozen.authority().metadata(),
        held.expected.workspace_id,
    )
    .unwrap();
    assert!(record.state.is_in_progress());
    assert_eq!(record.unresolved().count(), held.conflicts_total);
    drop(frozen);
    assert_eq!(branch_change(&reopened), current);
}
