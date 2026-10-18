// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Opt-in receiver-local coordination through the existing MCP dispatcher.
//! No request may choose the repository, workspace, owner, lifetime or clock.

use crate::local_repository_authority::ActiveLocalRepositoryAuthority;
use crate::state::DaemonState;
use axum::http::HeaderMap;
use kin_db::VersionedAuthorityState;
use kin_model::entity_lease::{
    supports_entity_edit_lease, EntityEditLease, EntityEditLeaseClaim, EntityEditLeaseContext,
    EntityEditLeaseMutation, EntityEditLeaseResource,
};
use kin_model::{AuthorId, EntityId, Hash256, OperationId, RepositoryTransaction, SessionId};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) const READ: &str = "kin_entity_leases";
pub(crate) const WRITE: &str = "kin_entity_lease";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    entity_ids: Vec<EntityId>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlInput {
    request_id: String,
    mutation: PublicMutation,
}
#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum PublicMutation {
    Acquire {
        entity_id: EntityId,
        expected_source_hash: String,
    },
    Renew {
        entity_id: EntityId,
        authority_id: uuid::Uuid,
        fence: String,
    },
    Release {
        entity_id: EntityId,
        authority_id: uuid::Uuid,
        fence: String,
    },
}
impl PublicMutation {
    fn bound(
        self,
        workspace_id: kin_model::WorkspaceId,
    ) -> Result<EntityEditLeaseMutation, String> {
        Ok(match self {
            Self::Acquire {
                entity_id,
                expected_source_hash,
            } => EntityEditLeaseMutation::Acquire {
                resource: EntityEditLeaseResource {
                    workspace_id,
                    entity_id,
                },
                expected_source_hash: Hash256::from_hex(&expected_source_hash).map_err(|_| {
                    "expected_source_hash must be the complete entity body SHA-256 hex digest"
                })?,
            },
            Self::Renew {
                entity_id,
                authority_id,
                fence,
            } => EntityEditLeaseMutation::Renew {
                claim: EntityEditLeaseClaim {
                    authority_id,
                    resource: EntityEditLeaseResource {
                        workspace_id,
                        entity_id,
                    },
                    fence,
                },
            },
            Self::Release {
                entity_id,
                authority_id,
                fence,
            } => EntityEditLeaseMutation::Release {
                claim: EntityEditLeaseClaim {
                    authority_id,
                    resource: EntityEditLeaseResource {
                        workspace_id,
                        entity_id,
                    },
                    fence,
                },
            },
        })
    }
}
fn shaped(mut arguments: HashMap<String, Value>) -> Value {
    for key in [
        "max_chars",
        "max_response_chars",
        "explain",
        "compact",
        "answer_only",
        "max_output_bytes",
    ] {
        arguments.remove(key);
    }
    serde_json::to_value(arguments).expect("JSON arguments remain JSON")
}
fn view(entry: &EntityEditLease, caller: SessionId) -> Value {
    json!({"authority_id":entry.authority_id,"workspace_id":entry.resource.workspace_id,
        "entity_id":entry.resource.entity_id,"session_id":entry.session_id,"held_by_caller":entry.session_id==caller,
        "fence":entry.fence.to_string(),"claim":{"authority_id":entry.authority_id,"resource":entry.resource,"fence":entry.fence.to_string()},"source_hash":entry.source_hash.to_string(),"expires_at_ms":entry.expires_at_ms})
}
fn operation_id(
    repository: &str,
    workspace: kin_model::WorkspaceId,
    caller: SessionId,
    request_id: &str,
) -> OperationId {
    let mut digest = Sha256::new();
    digest.update(b"kin-entity-edit-lease-request-v1\0");
    digest.update(serde_json::to_vec(&(repository, workspace, caller, request_id)).unwrap());
    let bytes = digest.finalize();
    OperationId::from_uuid(uuid::Uuid::from_bytes(bytes[..16].try_into().unwrap()))
}
fn receipt_view(
    operation: &kin_model::RepositoryOperationRecord,
    caller: SessionId,
    replayed: bool,
) -> Value {
    json!({"schema":"kin.entity_edit_lease.receipt.v1","replayed":replayed,
        "receipt":{"operation_id":operation.operation_id,"transaction_hash":operation.transaction_hash,
            "generation":operation.roots_after.generation.to_string(),"roots_before":operation.roots_before,"roots_after":operation.roots_after},
        "lease":operation.entity_edit_lease.as_ref().and_then(|event| event.lease.as_ref()).map(|entry| view(entry,caller)),
        "coordination_only":true,"replay_returns_original_expiry":true,
        "next":{"command":"kin_entity_leases","reason":"Read current ownership before resuming interrupted work. Replay requires the same authenticated session and unchanged request."}})
}

pub(crate) async fn call(
    state: Arc<DaemonState>,
    headers: HeaderMap,
    arguments: HashMap<String, Value>,
    authenticated: bool,
    write: bool,
) -> kin_mcp::ToolCallResult {
    let result = async {
        if !authenticated { return Err("entity_lease_auth_required: this capability requires the daemon's enforced local bearer authentication".into()); }
        let caller = headers.get("X-Kin-Session").and_then(|value| value.to_str().ok())
            .and_then(|value| uuid::Uuid::parse_str(value).ok()).map(SessionId)
            .filter(|id| !id.0.is_nil()).ok_or("entity_lease_session_required: use the connection's live session")?;
        if state.get_session_scope(&caller).await.is_some() {
            return Err("entity leases describe the current workspace; leave the historical session scope first".into());
        }
        let _coordination = state.coordination_gate.lock().await;
        let request = shaped(arguments);
        let task_state = Arc::clone(&state);
        tokio::task::spawn_blocking(move || -> Result<Value,String> {
            let state = task_state;
            let _persistence = state.persist_lock.lock().map_err(|_| "daemon persistence lock poisoned")?;
            let sessions = crate::api::mcp_session_registry_snapshot(&state).map_err(|(_,message)| message)?;
            sessions.require_live_session(&caller.to_string())?;
            let authority = ActiveLocalRepositoryAuthority::open_bound(&state).map_err(|error| error.into_error().to_string())?;
            if !write {
                let input: ReadInput = serde_json::from_value(request).map_err(|_| "entity leases take only entity_ids")?;
                if input.entity_ids.is_empty() || input.entity_ids.len()>16 || input.entity_ids.iter().collect::<std::collections::HashSet<_>>().len()!=input.entity_ids.len() {
                    return Err("read one to sixteen distinct entity IDs".into());
                }
                let held = authority.manager.read_authority();
                let graph = held.workspace_graph_snapshot(&authority.workspace_id).map_err(|error|error.to_string())?
                    .ok_or("entity lease workspace unavailable")?;
                let now = chrono::Utc::now().timestamp_millis();
                let mut entries = Vec::new();
                for id in input.entity_ids {
                    if !graph.entities.get(&id).is_some_and(supports_entity_edit_lease) { return Err("entity leases require parsed independent source declarations".into()); }
                    let lease = held.metadata().entity_edit_leases.entries.iter().find(|entry| entry.resource.workspace_id==authority.workspace_id && entry.resource.entity_id==id && entry.expires_at_ms>now);
                    let resource = EntityEditLeaseResource {workspace_id:authority.workspace_id,entity_id:id};
                    let source_hash = authority.manager.entity_edit_lease_source_hash_at(&held,resource).map_err(|error|error.to_string())?;
                    entries.push(json!({"entity_id":id,"source_hash":source_hash.to_string(),"lease":lease.map(|entry|view(entry,caller))}));
                }
                return Ok(json!({"schema":"kin.entity_edit_leases.v1","repository_id":authority.repository_id,
                    "workspace_id":authority.workspace_id,"generation":held.generation().to_string(),"leases":entries,
                    "coordination_only":true,"coverage":"Kin-managed parsed entity body edits and deletions at this receiver. Native files and provider writes are not locked."}));
            }
            sessions.require_write_capability(&caller.to_string(), kin_mcp::session::WriteDoor::Commit)?;
            let input: ControlInput = serde_json::from_value(request).map_err(|_| "invalid entity lease control: use request_id and an acquire, renew or release mutation; owner, clock and lifetime are not request fields")?;
            if input.request_id.trim().is_empty() || input.request_id.len()>256 { return Err("entity lease request_id must be nonblank and at most 256 UTF-8 bytes".into()); }
            let mutation = input.mutation.bound(authority.workspace_id)?; mutation.validate()?;
            let context = EntityEditLeaseContext { session_id:caller, mutation:Some(mutation.clone()), claims:vec![] };
            let id = operation_id(authority.repository_id.as_str(),authority.workspace_id,caller,&input.request_id);
            let held = authority.manager.read_authority();
            if let Some(operation) = held.metadata().operation_log.iter().find(|operation|operation.operation_id==id) {
                if operation.entity_edit_lease.as_ref().map(|event|&event.context)!=Some(&context) {
                    return Err("entity_lease_request_conflict: this request_id already names different work; retry the original unchanged".into());
                }
                let operation = operation.clone();
                let needs_repair = state.snapshot_generation.load(std::sync::atomic::Ordering::SeqCst) == operation.roots_before.generation
                    && held.roots() == &operation.roots_after;
                drop(held);
                if needs_repair {
                    let receipt = kin_model::RepositoryCommitReceipt { operation_id:operation.operation_id, repository_id:operation.repository_id.clone(),
                        transaction_hash:operation.transaction_hash,outcome:kin_model::RepositoryCommitOutcome::IdempotentReplay,
                        generation:operation.roots_after.generation, roots_before:operation.roots_before.clone(),roots_after:operation.roots_after.clone(),operation:operation.clone() };
                    let _mutation = state.begin_graph_authority_mutation();
                    state.record_entity_lease_control_commit(&receipt).map_err(|error|error.to_string())?;
                    crate::binding_history::restore_review_publication(&state,&authority).map_err(|error|error.to_string())?;
                    state.bump_version(); state.mark_dirty();
                }
                return Ok(receipt_view(&operation,caller,true));
            }
            crate::local_repository_authority::require_daemon_authority_generation(&state,held.roots(),"controlling an entity edit lease").map_err(|error|error.to_string())?;
            let transaction = RepositoryTransaction { schema_version:5,operation_id:id,repository_id:authority.repository_id.clone(),
                expected_generation:held.generation(),expected_roots:held.roots().clone(),actor:AuthorId::new(format!("Kin session <mcp-agent:{caller}>")),
                reason:"Update entity edit coordination".into(),external_objects:vec![],git_authority_delta:None,changes:vec![],aliases:vec![],ref_mutations:vec![],
                default_ref_mutation:None,workspace_mutation:None,local_overlay_delta:None,merge_transaction_delta:None,sealed_observation:None,
                collaboration_delta:None,git_snapshot_proofs:vec![],entity_edit_lease:Some(mutation) };
            drop(held);
            let _mutation = state.begin_graph_authority_mutation();
            let receipt = authority.manager.commit_repository_transaction_with_entity_edit_leases_qualified(transaction,&context,
                Some(&kin_index::binding_history::LocalBindingHistoryVerifier),None).map_err(|error|format!("entity lease control refused: {error}; retry this exact request_id to resolve a lost response"))?;
            state.record_entity_lease_control_commit(&receipt).map_err(|error|format!("entity lease is durable but local cursor finalization failed: {error}; retry the original request_id"))?;
            crate::binding_history::restore_review_publication(&state,&authority).map_err(|error|format!("entity lease is durable but local binding restoration failed: {error}"))?;
            state.bump_version(); state.mark_dirty();
            state.emit_event(crate::state::DaemonEvent::RepositoryAuthorityChanged { repository_id:receipt.repository_id.to_string(),
                operation_id:receipt.operation_id,previous_generation:receipt.roots_before.generation,new_generation:receipt.generation });
            Ok(receipt_view(&receipt.operation,caller,false))
        }).await.map_err(|error|format!("entity lease task failed: {error}"))?
    }.await;
    match result {
        Ok(value) => kin_mcp::ToolCallResult::text(value.to_string()),
        Err(error) => kin_mcp::ToolCallResult::error(error),
    }
}
