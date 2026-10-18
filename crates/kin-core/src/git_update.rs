// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Exact incremental Git ingestion for an already bound Git-origin branch.

use std::collections::BTreeMap;
use std::path::PathBuf;

use kin_blobs::BlobStore;
use kin_db::{
    LocalFileBackend, PersistedRepositoryAuthority, RepositoryAuthorityManager,
    RepositoryAuthorityState,
};
use kin_git::{AdmittedSemanticGitImportPlan, SealedContentObservation};
use kin_model::{
    AuthorId, ChangeOrigin, ExternalObjectId, ExternalObjectKind, GitExternalAuthority,
    GitExternalAuthorityDelta, GitObjectBodyLoader, GitObjectId, GitRawRef, GitRawTarget, Hash256,
    OperationId, RefExpectation, RefMutation, RefName, RefTarget, RefUpdatePolicy,
    RepositoryTransaction, SealedObservationBinding, SemanticChangeId, WorkspaceHead, WorkspaceId,
    WorkspaceState, REPOSITORY_TRANSACTION_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};

use crate::{KinError, Result};

#[cfg(all(test, unix))]
mod tests;

/// A fetched, immutable commit observation and the semantic head it updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitUpdateRequest {
    pub operation_id: OperationId,
    pub source_dir: PathBuf,
    pub source_ref: RefName,
    pub source_oid: GitObjectId,
    pub destination_ref: RefName,
    pub expected_head: SemanticChangeId,
}

/// Eligibility from persisted local authority. Source projection is verified
/// separately at the update's publication boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitUpdateStatus {
    pub can_update: bool,
    pub reason: Option<String>,
    pub branch: Option<RefName>,
    pub head: Option<SemanticChangeId>,
    pub git_oid: Option<GitObjectId>,
}

pub struct PreparedGitUpdate {
    pub transaction: Option<RepositoryTransaction>,
    pub previous_head: SemanticChangeId,
    pub head: SemanticChangeId,
    pub admitted_changes: usize,
    pub workspace: WorkspaceState,
    admitted: AdmittedSemanticGitImportPlan,
    sealed: Option<SealedContentObservation>,
}

pub fn git_update_status(
    authority: &RepositoryAuthorityState,
    workspace_id: WorkspaceId,
) -> GitUpdateStatus {
    let mut status = GitUpdateStatus {
        can_update: false,
        reason: None,
        branch: None,
        head: None,
        git_oid: None,
    };
    let checked = (|| -> Result<()> {
        let workspace = find_workspace(authority, workspace_id)?;
        let WorkspaceHead::Symbolic { target: branch } = &workspace.head else {
            return Err(refusal("Git updates require a current branch"));
        };
        status.branch = Some(branch.clone());
        let target = workspace
            .base_target
            .as_ref()
            .ok_or_else(|| refusal("Git updates require a born branch"))?;
        let head = authority
            .resolve_target_change_id(target)
            .map_err(graph_error)?;
        status.head = Some(head);
        let oid = require_git_update_base(authority, workspace, branch, head)?;
        status.git_oid = Some(oid);
        Ok(())
    })();
    match checked {
        Ok(()) => status.can_update = true,
        Err(error) => status.reason = Some(error.to_string()),
    }
    status
}

/// Derive the selected fetched commit through the same semantic admission path
/// as complete Git import, and compare all held ancestor records byte for byte.
/// No source working-copy state or ambient Git branch enters the derivation.
pub fn prepare_git_update(
    manager: &RepositoryAuthorityManager<LocalFileBackend>,
    workspace_id: WorkspaceId,
    request: &GitUpdateRequest,
    capture_store: &BlobStore,
) -> Result<PreparedGitUpdate> {
    let authority = manager.read_authority();
    let workspace = find_workspace(&authority, workspace_id)?.clone();
    let current_oid = require_git_update_base(
        &authority,
        &workspace,
        &request.destination_ref,
        request.expected_head,
    )?;
    let held = repository_metadata(&authority);
    let held_aliases = held
        .aliases
        .iter()
        .map(|alias| (alias.oid, alias))
        .collect::<BTreeMap<_, _>>();
    let repository_id = held.repository_id.clone();
    let snapshot = kin_git::capture_lossless_git_ref(
        &kin_io::WorktreeRoot::new(&request.source_dir, &kin_io::Grant::ambient()),
        repository_id.clone(),
        &request.source_ref,
        request.source_oid,
        capture_store,
    )
    .map_err(git_error)?;
    for record in &snapshot.objects {
        if record.object.kind != ExternalObjectKind::Commit
            || held_aliases.contains_key(&record.object.oid)
        {
            continue;
        }
        let body = capture_store
            .read(&kin_blobs::Hash256::from_bytes(
                *record.body_hash.as_bytes(),
            ))
            .map_err(|error| refusal(error.to_string()))?;
        if body
            .split(|byte| *byte == b'\n')
            .take_while(|line| !line.is_empty())
            .any(|line| line.starts_with(b"kin-change-id "))
        {
            return Err(refusal(
                "fetched history contains a native Kin export without verified native export bindings",
            ));
        }
    }
    let plan = kin_git::plan_semantic_git_import(&snapshot, capture_store).map_err(git_error)?;
    let plan = crate::git_init::bind_historical_semantics(plan, capture_store)?;
    let admitted = kin_git::admit_semantic_git_import(&plan, capture_store).map_err(git_error)?;
    let mut aliases = Vec::new();
    let mut changes = Vec::new();
    let mut contains_current = false;
    for alias in &admitted.aliases {
        let change = admitted
            .changes
            .read_by_oid(&alias.oid)
            .map_err(git_error)?
            .ok_or_else(|| refusal("fetched Git alias has no admitted semantic change"))?;
        if alias.oid == current_oid {
            contains_current = true;
        }
        if let Some(existing_alias) = held_aliases.get(&alias.oid) {
            if *existing_alias != alias {
                return Err(refusal(format!(
                    "Git commit {} derives a different semantic identity; its existing alias cannot be changed",
                    alias.oid
                )));
            }
            let existing = authority
                .snapshot()
                .changes
                .read_change(&existing_alias.change_id)
                .map_err(graph_error)?
                .ok_or_else(|| refusal("held Git alias has no immutable semantic change"))?;
            if existing != change {
                return Err(refusal(format!(
                    "Git commit {} no longer derives byte-identical held semantic records",
                    alias.oid
                )));
            }
        } else {
            if authority.snapshot().changes.contains_change(&change.id) {
                return Err(refusal(
                    "a fetched Git change already exists without its exact alias",
                ));
            }
            aliases.push(alias.clone());
            changes.push(change);
        }
    }
    let mut fresh_changes = changes
        .into_iter()
        .map(|change| (change.id, change))
        .collect::<BTreeMap<_, _>>();
    let changes = admitted
        .changes
        .ids()
        .filter_map(|id| fresh_changes.remove(&id))
        .collect::<Vec<_>>();
    if !contains_current {
        return Err(refusal(
            "the fetched Git commit does not descend from the current Git-origin head",
        ));
    }
    let head = admitted
        .workspace_base_change_id()
        .ok_or_else(|| refusal("fetched Git commit has no admitted semantic head"))?;
    if request.source_oid == current_oid {
        if head != request.expected_head || !changes.is_empty() || !aliases.is_empty() {
            return Err(refusal(
                "the current Git commit no longer derives its exact held head",
            ));
        }
        return Ok(PreparedGitUpdate {
            transaction: None,
            previous_head: request.expected_head,
            head,
            admitted_changes: 0,
            workspace,
            admitted,
            sealed: None,
        });
    }
    let mut external_objects = Vec::new();
    for record in &admitted.external_objects {
        match held
            .external_objects
            .binary_search_by_key(&record.object, |held| held.object)
        {
            Ok(index) if held.external_objects[index] == *record => {}
            Ok(_) => {
                return Err(refusal(
                    "fetched Git object disagrees with its held descriptor",
                ))
            }
            Err(_) => external_objects.push(record.clone()),
        }
    }
    let old_git_authority = held
        .git_external_authority
        .as_ref()
        .ok_or_else(|| refusal("Git updates require existing exact Git external authority"))?;
    let new_git_authority = union_git_authority(
        old_git_authority,
        &snapshot,
        &request.destination_ref,
        request.source_oid,
        capture_store,
        manager,
    )?;
    let target = authority
        .resolve_ref_target(&request.destination_ref)
        .map_err(graph_error)?
        .ok_or_else(|| refusal("the selected current branch no longer exists"))?;
    let transaction = RepositoryTransaction {
        schema_version: REPOSITORY_TRANSACTION_SCHEMA_VERSION,
        operation_id: request.operation_id,
        repository_id,
        expected_generation: authority.roots().generation,
        expected_roots: authority.roots().clone(),
        actor: AuthorId::new("kin-git-update"),
        reason: "admit exact fetched Git descendants".to_string(),
        external_objects,
        git_authority_delta: Some(GitExternalAuthorityDelta::update(
            old_git_authority.clone(),
            new_git_authority,
        )),
        changes,
        aliases,
        ref_mutations: vec![RefMutation {
            name: request.destination_ref.clone(),
            expected: RefExpectation::MustEqual { target },
            new_target: Some(RefTarget::external_object(ExternalObjectId::new(
                ExternalObjectKind::Commit,
                request.source_oid,
            ))),
            policy: RefUpdatePolicy::FastForwardOnly,
        }],
        default_ref_mutation: None,
        // The workspace stays on its old base until the daemon follows the
        // admitted ref. A workspace mutation that changes nothing is not one
        // the model admits, so none rides here: the expected roots stop a
        // concurrent workspace change, and the publication seam proves the
        // tracked bytes unchanged before it commits.
        workspace_mutation: None,
        local_overlay_delta: None,
        merge_transaction_delta: None,
        sealed_observation: None,
        collaboration_delta: None,
        git_snapshot_proofs: Vec::new(),
        entity_edit_lease: None,
    };
    transaction.validate().map_err(model_error)?;
    Ok(PreparedGitUpdate {
        admitted_changes: transaction.changes.len(),
        transaction: Some(transaction),
        previous_head: request.expected_head,
        head,
        workspace,
        admitted,
        sealed: None,
    })
}

impl PreparedGitUpdate {
    /// Copy capture bodies into the retained repository source CAS and seal
    /// their exact tree/content closure before it can become authority.
    pub fn seal_in_repository(
        &mut self,
        manager: &RepositoryAuthorityManager<LocalFileBackend>,
        capture_store: &BlobStore,
    ) -> Result<()> {
        if self.transaction.is_none() {
            return Ok(());
        }
        manager
            .with_source_blob_write_batch(&mut |batch| {
                for record in &self.admitted.external_objects {
                    let body = capture_store
                        .read(&kin_blobs::Hash256::from_bytes(
                            *record.body_hash.as_bytes(),
                        ))
                        .map_err(|error| kin_db::KinDbError::StorageError(error.to_string()))?;
                    batch.save(*record.body_hash.as_bytes(), &body)?;
                }
                for hash in self.admitted.content.identities.keys() {
                    let body = capture_store
                        .read(&kin_blobs::Hash256::from_bytes(*hash.as_bytes()))
                        .map_err(|error| kin_db::KinDbError::StorageError(error.to_string()))?;
                    batch.save(*hash.as_bytes(), &body)?;
                }
                Ok(())
            })
            .map_err(graph_error)?;
        let sealed = kin_git::seal_all_content_observation(
            &self.admitted,
            &crate::init::GraphOwnedContent::new(manager),
        )
        .map_err(git_error)?;
        let binding = SealedObservationBinding::new(
            sealed.fingerprint,
            sealed.observed_trees as u64,
            sealed.observed_entries as u64,
            sealed.coverage.sealed_bodies as u64,
            sealed.coverage.sealed_body_bytes,
            sealed.coverage.opaque_bodies as u64,
            sealed.exclusions.len() as u64,
        )
        .map_err(model_error)?;
        self.transaction
            .as_mut()
            .expect("a mutation was checked above")
            .sealed_observation = Some(binding);
        self.sealed = Some(sealed);
        Ok(())
    }

    pub fn verify_published_seal(
        &self,
        manager: &RepositoryAuthorityManager<LocalFileBackend>,
    ) -> Result<()> {
        let Some(before) = &self.sealed else {
            return Ok(());
        };
        let after = kin_git::seal_all_content_observation(
            &self.admitted,
            &crate::init::GraphOwnedContent::new(manager),
        )
        .map_err(git_error)?;
        if before != &after {
            return Err(refusal(
                "published Git update content does not match its admission seal",
            ));
        }
        Ok(())
    }
}

fn find_workspace(
    authority: &RepositoryAuthorityState,
    workspace_id: WorkspaceId,
) -> Result<&WorkspaceState> {
    repository_metadata(authority)
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == workspace_id)
        .ok_or_else(|| refusal("the current repository workspace is absent"))
}

fn require_git_update_base(
    authority: &RepositoryAuthorityState,
    workspace: &WorkspaceState,
    branch: &RefName,
    expected_head: SemanticChangeId,
) -> Result<GitObjectId> {
    let metadata = repository_metadata(authority);
    if metadata.merge_transactions.iter().any(|record| {
        record.workspace_id == workspace.workspace_id && record.state.is_in_progress()
    }) {
        return Err(refusal(
            "complete or abandon the current merge before a Git update",
        ));
    }
    if workspace.holds_uncommitted_work() {
        return Err(refusal(
            "commit or preserve current graph-owned changes before a Git update",
        ));
    }
    if workspace.head
        != (WorkspaceHead::Symbolic {
            target: branch.clone(),
        })
    {
        return Err(refusal(
            "the workspace no longer tracks the selected branch",
        ));
    }
    let base = workspace
        .base_target
        .as_ref()
        .ok_or_else(|| refusal("Git updates require a born branch"))?;
    let base_id = authority
        .resolve_target_change_id(base)
        .map_err(graph_error)?;
    let target = authority
        .resolve_ref_target(branch)
        .map_err(graph_error)?
        .ok_or_else(|| refusal("the current branch is absent"))?;
    if base_id != expected_head
        || authority
            .resolve_target_change_id(&target)
            .map_err(graph_error)?
            != expected_head
    {
        return Err(refusal(
            "the current branch or workspace moved after the Git update was selected",
        ));
    }
    let change = authority
        .snapshot()
        .changes
        .read_change(&expected_head)
        .map_err(graph_error)?
        .ok_or_else(|| refusal("the current semantic head is absent"))?;
    let ChangeOrigin::GitCommit { oid } = change.origin else {
        return Err(refusal(
            "Git updates after native Kin changes require verified native export bindings",
        ));
    };
    if !metadata.aliases.iter().any(|alias| {
        alias.oid == oid
            && alias.change_id == expected_head
            && alias.repository_id == metadata.repository_id
    }) {
        return Err(refusal(
            "the current Git-origin head has no exact repository alias",
        ));
    }
    if metadata.git_external_authority.is_none() {
        return Err(refusal(
            "the current head has no exact Git external authority",
        ));
    }
    Ok(oid)
}

fn union_git_authority(
    old: &GitExternalAuthority,
    fetched: &kin_git::LosslessGitRepository,
    destination: &RefName,
    oid: GitObjectId,
    store: &BlobStore,
    manager: &RepositoryAuthorityManager<LocalFileBackend>,
) -> Result<GitExternalAuthority> {
    let mut records = old
        .closure
        .objects
        .iter()
        .map(|entry| (entry.record.object, entry.record.clone()))
        .collect::<BTreeMap<_, _>>();
    for record in &fetched.objects {
        if let Some(existing) = records.insert(record.object, record.clone()) {
            if existing != *record {
                return Err(refusal(
                    "fetched closure differs from the retained Git authority",
                ));
            }
        }
    }
    let mut refs = old
        .raw_refs
        .iter()
        .map(|raw| (raw.name.clone(), raw.clone()))
        .collect::<BTreeMap<_, _>>();
    refs.insert(
        destination.clone(),
        GitRawRef {
            name: destination.clone(),
            target: GitRawTarget::Direct {
                object: ExternalObjectId::new(ExternalObjectKind::Commit, oid),
            },
        },
    );
    // Every old root remains reachable: the sole replaced ref advances to a
    // descendant, and all other refs and the raw HEAD remain exact.
    GitExternalAuthority::from_raw_parts(
        old.repository_id.clone(),
        old.object_format,
        refs.into_values().collect(),
        old.raw_head.clone(),
        records.into_values().collect(),
        &mut StoreBodyLoader { store, manager },
    )
    .map_err(|error| refusal(format!("verify the exact updated Git authority: {error}")))
}

struct StoreBodyLoader<'a> {
    store: &'a BlobStore,
    manager: &'a RepositoryAuthorityManager<LocalFileBackend>,
}

impl GitObjectBodyLoader for StoreBodyLoader<'_> {
    type Error = String;
    fn load_body(&mut self, hash: &Hash256) -> std::result::Result<Option<Vec<u8>>, String> {
        match self
            .store
            .read(&kin_blobs::Hash256::from_bytes(*hash.as_bytes()))
        {
            Ok(body) => Ok(Some(body)),
            Err(kin_blobs::BlobError::NotFound { .. }) => self
                .manager
                .load_source_blob(*hash)
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        }
    }
}

fn refusal(message: impl Into<String>) -> KinError {
    KinError::Other(message.into())
}

fn repository_metadata(authority: &RepositoryAuthorityState) -> &PersistedRepositoryAuthority {
    authority.metadata()
}
fn graph_error(error: kin_db::KinDbError) -> KinError {
    KinError::Graph(error.to_string())
}
fn model_error(error: kin_model::ModelError) -> KinError {
    refusal(error.to_string())
}
fn git_error(error: kin_git::GitError) -> KinError {
    refusal(format!("exact Git update refused: {error}"))
}
