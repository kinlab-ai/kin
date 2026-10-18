// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Shared publication fence for parsed entity body edits and deletions.
//! Every repository writer reaches this gate; a missing verified context is
//! an ordinary unclaimed writer, not a lease bypass.

use super::*;
use kin_model::entity_lease::{
    supports_entity_edit_lease, EntityEditLeaseContext, EntityEditLeaseEvent,
    EntityEditLeaseMutation, EntityEditLeaseResource,
};

fn refusal(message: impl Into<String>) -> KinDbError {
    ModelError::Conflict(message.into()).into()
}

pub(super) fn verified_hash<B: StorageBackend + ?Sized>(
    graph: &GraphSnapshot,
    workspace: &WorkspaceState,
    resource: EntityEditLeaseResource,
    backend: &B,
    repository: &RepositoryId,
) -> Result<Option<Hash256>, KinDbError> {
    let Some(entity) = graph
        .entities
        .get(&resource.entity_id)
        .filter(|entity| supports_entity_edit_lease(entity))
    else {
        return Ok(None);
    };
    let span = entity
        .span
        .as_ref()
        .expect("supported source entity has a span");
    let origin = entity
        .file_origin
        .as_ref()
        .expect("supported source entity has an origin");
    if span.file != *origin {
        return Err(refusal("ENTITY_LEASE_DERIVATION_REQUIRED"));
    }
    let path = RepoPath::from_utf8(origin.0.clone())
        .map_err(|_| refusal("ENTITY_LEASE_DERIVATION_REQUIRED"))?;
    let Some(artifact) = workspace.tree.artifact_at_path(&path) else {
        return Ok(None);
    };
    let digest = match artifact.entry {
        TreeEntry::Blob { hash, .. } => hash,
        _ => return Err(refusal("ENTITY_LEASE_DERIVATION_REQUIRED")),
    };
    if entity.recorded_span_source_digest().as_ref() != Some(&digest) {
        return Err(refusal("ENTITY_LEASE_DERIVATION_REQUIRED"));
    }
    let body = backend
        .load_source_blob_bounded(repository.as_str(), *digest.as_bytes(), 16 * 1024 * 1024)?
        .ok_or_else(|| refusal("ENTITY_LEASE_SOURCE_UNAVAILABLE"))?;
    verify_source_blob_digest(*digest.as_bytes(), &body, repository.as_str())?;
    let bytes = body
        .get(span.start_byte..span.end_byte)
        .filter(|_| span.start_byte < span.end_byte)
        .ok_or_else(|| refusal("ENTITY_LEASE_DERIVATION_REQUIRED"))?;
    let actual = Hash256::from_bytes(Sha256::digest(bytes).into());
    Ok(Some(actual))
}

pub(super) fn fence_successor<B: StorageBackend + ?Sized>(
    current: &RepositoryAuthorityState,
    successor: &GraphSnapshot,
    metadata: &mut PersistedRepositoryAuthority,
    transaction: &RepositoryTransaction,
    source: NativeAdmissionSource,
    context: Option<&EntityEditLeaseContext>,
    now_ms: i64,
    backend: &B,
) -> Result<Option<EntityEditLeaseEvent>, KinDbError> {
    if let Some(context) = context {
        context.validate().map_err(refusal)?;
        if context.mutation.as_ref() != transaction.entity_edit_lease.as_ref() {
            return Err(refusal(
                "entity lease context does not match the exact transaction",
            ));
        }
    }
    if let Some(mutation) = &transaction.entity_edit_lease {
        if !matches!(source, NativeAdmissionSource::LocalWorkspace) {
            return Err(refusal(
                "receiver-local entity lease control cannot be imported",
            ));
        }
        let context = context
            .ok_or_else(|| refusal("entity lease control requires an authenticated session"))?;
        let resource = match mutation {
            EntityEditLeaseMutation::Acquire { resource, .. } => *resource,
            EntityEditLeaseMutation::Renew { claim }
            | EntityEditLeaseMutation::Release { claim } => claim.resource,
        };
        let graph = current
            .workspace_graph_snapshot(&resource.workspace_id)?
            .ok_or_else(|| refusal("entity lease workspace is unavailable"))?;
        let workspace = current
            .metadata()
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == resource.workspace_id)
            .ok_or_else(|| refusal("entity lease workspace is unavailable"))?;
        let source_hash = verified_hash(
            &graph,
            workspace,
            resource,
            backend,
            &transaction.repository_id,
        )?
        .ok_or_else(|| refusal("entity leases require a parsed independent source declaration"))?;
        let lease = metadata
            .entity_edit_leases
            .apply_control(context, source_hash, now_ms)
            .map_err(refusal)?;
        metadata.validate_entity_edit_leases()?;
        return Ok(Some(EntityEditLeaseEvent {
            context: context.clone(),
            lease,
        }));
    }
    let claims = context
        .map(|context| context.claims.as_slice())
        .unwrap_or(&[]);
    let mut resources: BTreeSet<_> = metadata
        .entity_edit_leases
        .entries
        .iter()
        .filter(|entry| entry.expires_at_ms > now_ms)
        .map(|entry| entry.resource)
        .collect();
    resources.extend(claims.iter().map(|claim| claim.resource));
    if resources.is_empty() {
        return Ok(None);
    }
    let mut old_graphs = HashMap::new();
    let mut new_graphs = HashMap::new();
    for resource in resources {
        if !old_graphs.contains_key(&resource.workspace_id) {
            let graph = current
                .workspace_graph_snapshot(&resource.workspace_id)?
                .ok_or_else(|| refusal("ENTITY_LEASE_SOURCE_CHANGED"))?;
            old_graphs.insert(resource.workspace_id, graph);
            let graph = metadata
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == resource.workspace_id)
                .map(|workspace| {
                    materialize_workspace_graph_snapshot(successor, metadata, workspace, None, None)
                })
                .transpose()?;
            new_graphs.insert(resource.workspace_id, graph);
        }
        let old_workspace = current
            .metadata()
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == resource.workspace_id)
            .ok_or_else(|| refusal("ENTITY_LEASE_SOURCE_CHANGED"))?;
        let old_hash = verified_hash(
            &old_graphs[&resource.workspace_id],
            old_workspace,
            resource,
            backend,
            &transaction.repository_id,
        )?
        .ok_or_else(|| refusal("ENTITY_LEASE_SOURCE_CHANGED"))?;
        let claim = claims.iter().find(|claim| claim.resource == resource);
        // A stale explicit claim is refused even when this particular operation
        // does not move its body. Removing/reacquiring never revives old fences.
        if let Some(claim) = claim {
            let held = metadata
                .entity_edit_leases
                .entries
                .iter()
                .find(|entry| entry.resource == resource && entry.expires_at_ms > now_ms)
                .ok_or_else(|| refusal("ENTITY_LEASE_STALE"))?;
            if claim.authority_id != held.authority_id
                || context.map(|context| context.session_id) != Some(held.session_id)
                || claim.checked_fence().map_err(refusal)? != held.fence
            {
                return Err(refusal("ENTITY_LEASE_STALE"));
            }
            if old_hash != held.source_hash {
                return Err(refusal("ENTITY_LEASE_SOURCE_CHANGED"));
            }
        }
        let new_workspace = metadata
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == resource.workspace_id);
        let new_hash = match (new_graphs[&resource.workspace_id].as_ref(), new_workspace) {
            (Some(graph), Some(workspace)) => verified_hash(
                graph,
                workspace,
                resource,
                backend,
                &transaction.repository_id,
            )?,
            _ => None,
        };
        if new_hash != Some(old_hash) {
            metadata
                .entity_edit_leases
                .check_body_change(resource, old_hash, new_hash, context, now_ms)
                .map_err(refusal)?;
        }
    }
    metadata.validate_entity_edit_leases()?;
    Ok(context
        .filter(|context| !context.claims.is_empty())
        .map(|context| EntityEditLeaseEvent {
            context: context.clone(),
            lease: None,
        }))
}
