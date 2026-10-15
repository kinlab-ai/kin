// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use kin_cli::commands::transfer::{DerivedViewRefresh, WorkspaceFollow};
use kin_cli::git_update::{GitUpdateRequest, GitUpdateResponse, GitUpdateStatus};
use kin_model::{AuthorId, ChangeStore};

use crate::local_repository_authority::{
    require_fresh_daemon_workspace, ActiveLocalRepositoryAuthority,
};
use crate::state::{DaemonEvent, DaemonState};

type Refusal = (StatusCode, String);

pub(crate) async fn status(
    State(state): State<Arc<DaemonState>>,
) -> Result<Json<GitUpdateStatus>, Refusal> {
    require_local_daemon(&state)?;
    let read = tokio::task::spawn_blocking(move || {
        let _persistence = state
            .persist_lock
            .lock()
            .map_err(|_| internal("daemon persistence lock poisoned"))?;
        let authority = ActiveLocalRepositoryAuthority::open_bound(&state)
            .map_err(|error| conflict(error.into_error()))?;
        let lease = authority.manager.read_authority();
        let mut status = kin_core::git_update::git_update_status(&lease, authority.workspace_id);
        if let Err(reason) =
            kin_core::history_state::require_complete_for(&state.layout, "Git update")
        {
            status.can_update = false;
            status.reason = Some(reason);
        }
        Ok(Json(status))
    })
    .await
    .map_err(internal)?;
    read
}

pub(crate) async fn update(
    headers: HeaderMap,
    State(state): State<Arc<DaemonState>>,
    Json(request): Json<GitUpdateRequest>,
) -> Result<Json<GitUpdateResponse>, Refusal> {
    require_local_daemon(&state)?;
    if headers.contains_key("x-kin-session") {
        return Err(conflict(
            "Git updates require primary workspace authority without session scope",
        ));
    }
    tokio::task::spawn_blocking(move || {
        // Keep this gate inside the worker so cancellation of the HTTP request
        // cannot release it while the blocking publication is still running.
        let _coordination = state.coordination_gate.blocking_lock();
        run_update(&state, &request).map(Json)
    })
    .await
    .map_err(internal)?
}

fn run_update(
    state: &DaemonState,
    request: &GitUpdateRequest,
) -> Result<GitUpdateResponse, Refusal> {
    kin_core::history_state::require_complete_for(&state.layout, "Git update").map_err(conflict)?;
    let (mut response, destination) = {
        let _graph_mutation = state.begin_graph_authority_mutation();
        let _persistence = state
            .persist_lock
            .lock()
            .map_err(|_| internal("daemon persistence lock poisoned"))?;
        let authority = ActiveLocalRepositoryAuthority::open_bound(state)
            .map_err(|error| conflict(error.into_error()))?;
        let workspace_graph = authority
            .manager
            .workspace_graph_snapshot(&authority.repository_id, &authority.workspace_id)
            .map_err(internal)?
            .ok_or_else(|| conflict("current workspace graph is absent"))?;
        let roots = authority.manager.read_authority().roots().clone();
        require_fresh_daemon_workspace(state, &roots, &workspace_graph, "Git update")
            .map_err(conflict)?;
        let drift = kin_core::report_repository_workspace_projection_drift(
            state.layout.working_dir(),
            &workspace_graph.resolved_tree,
            &authority.manager,
        )
        .map_err(internal)?;
        if !drift.is_clean() {
            return Err(conflict("the tracked working projection changed; admit or preserve those changes before a Git update"));
        }
        let mut plan = kin_core::git_update::prepare_git_update(
            &authority.manager,
            authority.workspace_id,
            request,
            &state.blobs,
        )
        .map_err(conflict)?;
        plan.seal_in_repository(&authority.manager, &state.blobs)
            .map_err(internal)?;
        let mut derived_views = DerivedViewRefresh::Current;
        let generation = if let Some(transaction) = plan.transaction.take() {
            let changes = transaction.changes.clone();
            // The workspace remains on its previous base during admission.
            // The projection seam proves its exact tracked bytes and keeps
            // the projection frozen through the root/ref compare-and-swap.
            let (_, receipt, freeze) =
                kin_core::verify_repository_workspace_projection_and_commit_repository_transaction(
                    state.layout.working_dir(),
                    &plan.workspace.tree,
                    &authority.manager,
                    transaction,
                )
                .map_err(publication_refusal)?;
            crate::api::relabel_held_authority_after_own_commit(state);
            let refresh = (|| -> Result<(), String> {
                state
                    .record_repository_authority_commit(receipt.generation)
                    .map_err(|error| error.to_string())?;
                for change in &changes {
                    state
                        .graph
                        .create_change(change)
                        .map_err(|error| error.to_string())?;
                }
                Ok(())
            })();
            // The seal reads graph-owned content, which takes the store's
            // shared lock, so it is checked once the freeze has released the
            // exclusive one. Read under the freeze, it would wait forever.
            drop(freeze);
            let refresh = refresh.and_then(|()| {
                plan.verify_published_seal(&authority.manager)
                    .map_err(|error| error.to_string())
            });
            if let Err(detail) = refresh {
                derived_views = DerivedViewRefresh::Stale { detail };
            }
            state.bump_version();
            state.emit_event(DaemonEvent::GraphRootChanged {
                old_root_hash: None,
                new_root_hash: plan.head.to_string(),
            });
            receipt.generation
        } else {
            let (_, freeze) = kin_core::verify_repository_workspace_projection(
                state.layout.working_dir(),
                &plan.workspace.tree,
                &authority.manager,
            )
            .map_err(publication_refusal)?;
            let generation = freeze.roots().generation;
            drop(freeze);
            generation
        };
        (
            GitUpdateResponse {
                operation_id: request.operation_id,
                previous_head: plan.previous_head,
                head: plan.head,
                admitted_changes: plan.admitted_changes,
                authority_generation: generation,
                workspace_follow: WorkspaceFollow::AlreadyCurrent {
                    authority_generation: generation,
                },
                derived_views,
            },
            request.destination_ref.clone(),
        )
    };
    // Admission has released the persistence/graph gates. Following takes the
    // same gates independently and reports a failed projection honestly after
    // history has already become durable.
    response.workspace_follow = match &response.derived_views {
        DerivedViewRefresh::Current => crate::repository_branch::follow_moved_ref(
            state,
            &destination,
            &AuthorId::new("kin-git-update"),
        ),
        DerivedViewRefresh::Stale { detail } => WorkspaceFollow::Behind {
            detail: format!("Git history was admitted, but derived views are behind: {detail}"),
        },
    };
    Ok(response)
}

fn require_local_daemon(state: &DaemonState) -> Result<(), Refusal> {
    if !state.is_initialized.load(Ordering::Relaxed) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "daemon not fully initialized".to_string(),
        ));
    }
    if state.storage_backend.is_some() {
        return Err(conflict("Git updates require local repository authority"));
    }
    Ok(())
}

fn conflict(error: impl std::fmt::Display) -> Refusal {
    (StatusCode::CONFLICT, error.to_string())
}
fn internal(error: impl std::fmt::Display) -> Refusal {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn publication_refusal(error: kin_core::KinError) -> Refusal {
    match error {
        kin_core::KinError::RepositoryCommitIndeterminate(_)
        | kin_core::KinError::Graph(_)
        | kin_core::KinError::Io { .. } => internal(error),
        _ => conflict(error),
    }
}
