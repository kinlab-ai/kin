// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Persisted synthetic stores exercise the human branch-switch boundary.
//! These route tests start no daemon process and use no external provider.

use super::*;
use kin_cli::commands::branch::{
    BranchResponse, BranchSwitchConfirmRequest, BranchSwitchPreviewRequest,
    BranchSwitchPreviewResponse,
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

fn preview_request() -> BranchSwitchPreviewRequest {
    BranchSwitchPreviewRequest {
        name: kin_model::RefName::branch(b"feature").unwrap(),
    }
}

async fn preview(app: &axum::Router) -> BranchSwitchPreviewResponse {
    let (status, body) = post(app, "/commands/branch/preview", &preview_request()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_value(body).unwrap()
}

fn confirm(preview: &BranchSwitchPreviewResponse) -> BranchSwitchConfirmRequest {
    BranchSwitchConfirmRequest {
        expected: preview.expected.clone(),
        operation_id: kin_model::OperationId::new(),
        actor: AuthorId::new("Synthetic branch preview test"),
    }
}

/// Test IO alone walks the synthetic store, including names and symlink targets.
fn bytes_below(root: &FsPath) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &FsPath, at: &FsPath, out: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                out.insert(relative, Vec::new());
                visit(root, &path, out);
            } else if kind.is_symlink() {
                out.insert(
                    relative,
                    std::fs::read_link(path)
                        .unwrap()
                        .as_os_str()
                        .as_encoded_bytes()
                        .to_vec(),
                );
            } else {
                out.insert(relative, std::fs::read(path).unwrap());
            }
        }
    }
    let mut result = std::collections::BTreeMap::new();
    visit(root, root, &mut result);
    result
}

fn assert_bytes_unchanged(root: &FsPath, before: &std::collections::BTreeMap<PathBuf, Vec<u8>>) {
    let after = bytes_below(root);
    let paths: std::collections::BTreeSet<_> = before.keys().chain(after.keys()).collect();
    let changed: Vec<_> = paths
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .collect();
    assert!(
        changed.is_empty(),
        "unexpected writes beneath {}: {:?}",
        root.display(),
        changed.iter().take(16).collect::<Vec<_>>()
    );
}

#[tokio::test]
#[serial_test::serial(repository_commit)]
async fn branch_preview_writes_nothing_and_confirmation_survives_reopen() {
    let (state, layout, repository, main, feature) =
        universal_branch_test_state("branch-preview-read-only");
    // Router setup can create package-registry directories; measure requests
    // against the already-constructed router a client actually reaches.
    let app = router(Arc::clone(&state));
    let binding = state.local_repository_authority_binding().unwrap();
    let namespace = layout.kindb_namespace_path(binding.repository_id().as_str());
    let storage_before = bytes_below(&namespace);
    let working_before = bytes_below(&repository);
    let held = preview(&app).await;
    assert_eq!(held.expected.target_change, feature);
    assert_eq!(held.expected.target_ref, preview_request().name);
    assert!(!held.pending_work.present);
    assert_bytes_unchanged(&namespace, &storage_before);
    assert_bytes_unchanged(&repository, &working_before);
    assert_eq!(branch_change(&state), main);

    let request = confirm(&held);
    let (status, body) = post(&app, "/commands/branch/confirm", &request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let response: BranchResponse = serde_json::from_value(body).unwrap();
    assert!(response.mutated);
    assert_eq!(response.operation_id, Some(request.operation_id));
    let report = response
        .report
        .expect("confirmation must report authoritative branches");
    assert_eq!(report.repository_id, held.expected.repository_id);
    assert_eq!(report.workspace_id, held.expected.workspace_id);
    assert_eq!(
        report.authority_generation,
        held.expected.roots.generation + 1
    );
    assert_eq!(
        report.workspace_head,
        kin_model::WorkspaceHead::Symbolic {
            target: held.expected.target_ref.clone()
        }
    );
    assert!(report.branches.iter().any(|branch| {
        branch.name == held.expected.target_ref
            && branch.active
            && branch.target == held.expected.target_head
    }));
    assert_eq!(
        std::fs::read(repository.join("selected/compose.yaml")).unwrap(),
        b"services:\n  api:\n    image: feature\n"
    );
    drop(app);
    drop(state);
    let reopened = Arc::new(DaemonState::open(layout).unwrap());
    assert_eq!(branch_change(&reopened), feature);
}

#[tokio::test]
#[serial_test::serial(repository_commit)]
async fn branch_confirmation_refuses_changed_expectations_without_writes() {
    let (state, layout, repository, main, _) =
        universal_branch_test_state("branch-preview-expectations");
    let app = router(Arc::clone(&state));
    let held = preview(&app).await;
    let mut requests = Vec::new();
    let mut request = confirm(&held);
    request.expected.repository_id = kin_model::RepositoryId::new("other-repository").unwrap();
    requests.push(request);
    let mut request = confirm(&held);
    request.expected.workspace_id = kin_model::WorkspaceId::new();
    requests.push(request);
    let mut request = confirm(&held);
    request.expected.roots.generation += 1;
    requests.push(request);
    // Matching sequence numbers do not substitute for the exact root bundle.
    let mut wire = serde_json::to_value(confirm(&held)).unwrap();
    wire["expected"]["roots"]["collaboration"]["hash"] = json!(Hash256::from_bytes([7; 32]));
    requests.push(serde_json::from_value(wire).unwrap());
    let mut request = confirm(&held);
    request.expected.workspace_generation += 1;
    requests.push(request);
    let mut request = confirm(&held);
    request.expected.workspace_head = kin_model::WorkspaceHead::Symbolic {
        target: held.expected.target_ref.clone(),
    };
    requests.push(request);
    let mut request = confirm(&held);
    request.expected.workspace_base = None;
    requests.push(request);
    let mut request = confirm(&held);
    request.expected.workspace_tree = Hash256::from_bytes([7; 32]);
    requests.push(request);
    let mut request = confirm(&held);
    request.expected.source_ref_head = None;
    requests.push(request);
    let mut request = confirm(&held);
    request.expected.target_ref = kin_model::RefName::branch(b"missing").unwrap();
    requests.push(request);
    let mut request = confirm(&held);
    request.expected.target_head = held.expected.workspace_base.clone().unwrap();
    requests.push(request);
    let mut request = confirm(&held);
    request.expected.target_change = main;
    requests.push(request);

    let namespace = layout.kindb_namespace_path(held.expected.repository_id.as_str());
    let storage_before = bytes_below(&namespace);
    let working_before = bytes_below(&repository);
    for request in requests {
        let (status, body) = post(&app, "/commands/branch/confirm", &request).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["refused_before_write"], true, "{body}");
    }
    assert_bytes_unchanged(&namespace, &storage_before);
    assert_bytes_unchanged(&repository, &working_before);
}

#[tokio::test]
#[serial_test::serial(repository_commit)]
async fn branch_confirmation_refuses_work_committed_after_preview() {
    let (state, layout, repository, _, _) = universal_branch_test_state("branch-preview-moved");
    let app = router(Arc::clone(&state));
    let held = preview(&app).await;
    std::fs::write(repository.join("later.txt"), b"new committed work\n").unwrap();
    let current = commit_through_api(
        &app,
        kin_model::OperationId::new(),
        "advance source after branch preview",
    )
    .await;
    let namespace = layout.kindb_namespace_path(held.expected.repository_id.as_str());
    let storage_before = bytes_below(&namespace);
    let working_before = bytes_below(&repository);
    let (status, body) = post(&app, "/commands/branch/confirm", &confirm(&held)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["refused_before_write"], true, "{body}");
    assert_bytes_unchanged(&namespace, &storage_before);
    assert_bytes_unchanged(&repository, &working_before);
    assert_eq!(branch_change(&state), current);
}

#[tokio::test]
#[serial_test::serial(repository_commit)]
async fn branch_confirmation_carries_admitted_pending_work() {
    let (state, layout, repository, _, feature) =
        universal_branch_test_state("branch-preview-carry");
    let app = router(Arc::clone(&state));
    // Both branches share outside.txt, so this pending edit can move safely.
    std::fs::write(repository.join("outside.txt"), b"pending local work\n").unwrap();
    admit_through_api(&app).await;
    let held = preview(&app).await;
    assert!(held.pending_work.present);
    assert_eq!(held.pending_work.carried_count, 1);
    assert_eq!(held.pending_work.absorbed_count, 0);
    let (status, body) = post(&app, "/commands/branch/confirm", &confirm(&held)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["report"].is_object(), "{body}");
    assert_eq!(branch_change(&state), feature);
    assert_eq!(
        std::fs::read(repository.join("outside.txt")).unwrap(),
        b"pending local work\n"
    );
    assert_eq!(
        std::fs::read(repository.join("selected/compose.yaml")).unwrap(),
        b"services:\n  api:\n    image: feature\n"
    );
    drop(app);
    drop(state);
    let reopened = Arc::new(DaemonState::open(layout).unwrap());
    let frozen = reopened
        .local_repository_authority_binding()
        .unwrap()
        .freeze_existing_read_only(Duration::from_secs(2))
        .unwrap();
    let workspace = frozen
        .authority()
        .metadata()
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == held.expected.workspace_id)
        .unwrap();
    assert!(workspace.holds_uncommitted_work());
    assert_eq!(
        workspace.head,
        kin_model::WorkspaceHead::Symbolic {
            target: held.expected.target_ref,
        }
    );
}

#[tokio::test]
#[serial_test::serial(repository_commit)]
async fn branch_confirmation_does_not_admit_unreviewed_working_copy_drift() {
    let (state, layout, repository, _, _) =
        universal_branch_test_state("branch-preview-no-admission");
    let app = router(Arc::clone(&state));
    let held = preview(&app).await;
    std::fs::write(
        repository.join("outside.txt"),
        b"not admitted or reviewed\n",
    )
    .unwrap();
    // Projection acquisition refreshes only its diagnostic holder timestamp.
    // Seed an old valid record so the refresh cannot hide within one clock second.
    let lock_relative = PathBuf::from(".kin/reconciliation/projection.lock");
    let lock_path = repository.join(&lock_relative);
    assert!(std::fs::symlink_metadata(&lock_path).unwrap().is_file());
    let holder_prefix = format!("pid={} start_unix_s=", std::process::id());
    let old_holder = format!("{holder_prefix}1");
    std::fs::write(&lock_path, &old_holder).unwrap();

    let namespace = layout.kindb_namespace_path(held.expected.repository_id.as_str());
    let storage_before = bytes_below(&namespace);
    let mut working_before = bytes_below(&repository);
    let request_started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (status, body) = post(&app, "/commands/branch/confirm", &confirm(&held)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    // Once switching starts, its failures make no no-write promise.
    assert_ne!(body["refused_before_write"], true, "{body}");
    assert_bytes_unchanged(&namespace, &storage_before);

    assert!(std::fs::symlink_metadata(&lock_path).unwrap().is_file());
    let holder_after = std::fs::read_to_string(&lock_path).unwrap();
    let refreshed_at: u64 = holder_after
        .strip_prefix(&holder_prefix)
        .unwrap()
        .parse()
        .unwrap();
    let request_finished = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!((request_started..=request_finished).contains(&refreshed_at));
    assert_eq!(holder_after, format!("{holder_prefix}{refreshed_at}"));
    assert_ne!(holder_after, old_holder);
    // Keep the lock present and every other repository byte exact; only this
    // validated same-holder diagnostic refresh is permitted for an attempted switch.
    assert_eq!(
        working_before.insert(lock_relative, holder_after.into_bytes()),
        Some(old_holder.into_bytes())
    );
    assert_bytes_unchanged(&repository, &working_before);
}

#[tokio::test]
#[serial_test::serial(repository_commit)]
async fn branch_preview_and_confirmation_refuse_a_parked_merge() {
    let (state, layout, repository, _, _) = universal_branch_test_state("branch-preview-parked");
    let app = router(Arc::clone(&state));
    std::fs::write(
        repository.join("selected/compose.yaml"),
        b"services:\n  api:\n    image: divergent-main\n",
    )
    .unwrap();
    commit_through_api(&app, kin_model::OperationId::new(), "create merge conflict").await;
    let held = preview(&app).await;
    let merge = kin_cli::commands::merge::MergeRequest {
        source: held.expected.target_ref.clone(),
        operation_id: kin_model::OperationId::new(),
        actor: AuthorId::new("Synthetic branch preview test"),
    };
    let (status, body) = post(&app, "/commands/merge", &merge).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["report"]["outcome"], "conflicted", "{body}");
    let mut request = confirm(&held);
    let frozen = state
        .local_repository_authority_binding()
        .unwrap()
        .freeze_existing_read_only(Duration::from_secs(2))
        .unwrap();
    // Parking changes only the roots and merge record. Supply the current
    // roots so confirmation must reject the parked merge itself, not staleness.
    request.expected.roots = frozen.authority().metadata().roots.clone();
    drop(frozen);
    let namespace = layout.kindb_namespace_path(held.expected.repository_id.as_str());
    let storage_before = bytes_below(&namespace);
    let working_before = bytes_below(&repository);
    let (status, body) = post(&app, "/commands/branch/preview", &preview_request()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = post(&app, "/commands/branch/confirm", &request).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["refused_before_write"], true, "{body}");
    assert_bytes_unchanged(&namespace, &storage_before);
    assert_bytes_unchanged(&repository, &working_before);
}

#[tokio::test]
#[serial_test::serial(repository_commit)]
async fn branch_confirmation_postcommit_failure_and_replay_remain_indeterminate() {
    let (state, layout, repository, _, _) =
        universal_branch_test_state("branch-preview-postcommit");
    let app = router(Arc::clone(&state));
    let held = preview(&app).await;
    let request = confirm(&held);
    state
        .repository_command_fail_after_authority_once
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, body) = post(&app, "/commands/branch/confirm", &request).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_ne!(body["refused_before_write"], true, "{body}");
    let frozen = state
        .local_repository_authority_binding()
        .unwrap()
        .freeze_existing_read_only(Duration::from_secs(2))
        .unwrap();
    let metadata = frozen.authority().metadata();
    assert_eq!(
        metadata.roots.generation,
        held.expected.roots.generation + 1
    );
    let workspace = metadata
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == held.expected.workspace_id)
        .unwrap();
    assert_eq!(
        workspace.head,
        kin_model::WorkspaceHead::Symbolic {
            target: held.expected.target_ref.clone(),
        }
    );
    drop(frozen);
    let namespace = layout.kindb_namespace_path(held.expected.repository_id.as_str());
    let storage_before = bytes_below(&namespace);
    let working_before = bytes_below(&repository);
    let (status, body) = post(&app, "/commands/branch/confirm", &request).await;
    assert!(!status.is_success(), "{body}");
    assert_ne!(body["refused_before_write"], true, "{body}");
    assert_bytes_unchanged(&namespace, &storage_before);
    assert_bytes_unchanged(&repository, &working_before);
}
