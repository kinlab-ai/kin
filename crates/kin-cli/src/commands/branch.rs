// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Wire types, parsing, and CLI transport for daemon-owned repository branches.

use anyhow::{Context, Result};
use kin_model::{
    AuthorId, ExternalObjectKind, Hash256, OperationId, RefName, RefTarget, RepositoryId,
    RootBundle, SemanticChangeId, WorkspaceHead, WorkspaceId,
};
use serde::{Deserialize, Serialize};

use super::repository_authority::parse_ref_name;

pub const BRANCH_LIST_SCHEMA: &str = "kin.branch-list.v1";

/// Recorded state a human confirmed. RefName's byte encoding is authority;
/// display labels never enter this expectation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BranchSwitchExpectation {
    pub repository_id: RepositoryId,
    pub workspace_id: WorkspaceId,
    pub roots: RootBundle,
    pub workspace_generation: u64,
    pub workspace_head: WorkspaceHead,
    pub workspace_base: Option<RefTarget>,
    pub workspace_tree: Hash256,
    pub source_ref_head: Option<RefTarget>,
    pub target_ref: RefName,
    pub target_head: RefTarget,
    pub target_change: SemanticChangeId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchSwitchPreviewRequest {
    pub name: RefName,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchSwitchConfirmRequest {
    pub expected: BranchSwitchExpectation,
    pub operation_id: OperationId,
    pub actor: AuthorId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchSwitchSource {
    pub display: String,
    pub workspace_head: WorkspaceHead,
    pub base: Option<RefTarget>,
    pub change: Option<SemanticChangeId>,
    pub ref_head: Option<RefTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchSwitchTarget {
    pub display: String,
    pub name: RefName,
    pub head: RefTarget,
    pub change: SemanticChangeId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchSwitchPendingWork {
    pub present: bool,
    pub carried_count: usize,
    pub absorbed_count: usize,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchSwitchPreviewResponse {
    pub schema: String,
    pub expected: BranchSwitchExpectation,
    pub source: BranchSwitchSource,
    pub target: BranchSwitchTarget,
    pub pending_work: BranchSwitchPendingWork,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum BranchRequest {
    List,
    Create {
        name: RefName,
        operation_id: OperationId,
        actor: AuthorId,
    },
    Delete {
        name: RefName,
        operation_id: OperationId,
        actor: AuthorId,
    },
    Switch {
        name: RefName,
        operation_id: OperationId,
        actor: AuthorId,
    },
    /// Move the workspace onto one admitted point in history with a detached
    /// head, by the same exact transition a branch switch makes.
    SwitchDetached {
        /// Any selector the shared ref grammar accepts, resolved by the daemon
        /// under the authority lease the switch commits against: `git:<oid>`
        /// or a unique prefix of one, a semantic change id or prefix, `HEAD~N`,
        /// or a ref, whose current target is taken.
        target: String,
        operation_id: OperationId,
        actor: AuthorId,
    },
}

impl BranchRequest {
    pub fn is_mutation(&self) -> bool {
        !matches!(self, Self::List)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BranchListEntry {
    pub name: RefName,
    pub target: RefTarget,
    /// The semantic change `target` resolves to. A branch imported from Git
    /// targets its Git commit, which resolves through the commit's alias; a
    /// native branch targets its change directly. Absent when the authority
    /// cannot resolve the target to an admitted change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_id: Option<SemanticChangeId>,
    pub active: bool,
    pub default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BranchListReport {
    pub schema: String,
    pub authority: String,
    pub repository_id: RepositoryId,
    pub authority_generation: u64,
    pub roots: RootBundle,
    pub workspace_id: WorkspaceId,
    pub workspace_generation: u64,
    pub workspace_head: WorkspaceHead,
    pub repository_ref_count: usize,
    pub branch_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_ref: Option<RefName>,
    pub branches: Vec<BranchListEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchResponse {
    #[serde(default)]
    pub lines: Vec<String>,
    #[serde(default)]
    pub mutated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<BranchListReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority_generation: Option<u64>,
    #[serde(default)]
    pub idempotent: bool,
}

pub async fn list(json: bool) -> Result<()> {
    let response = execute(BranchRequest::List).await?;
    if json {
        let report = response
            .report
            .ok_or_else(|| anyhow::anyhow!("daemon branch list response omitted its report"))?;
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_lines(response);
    }
    Ok(())
}

pub async fn create(name: RefName) -> Result<()> {
    execute_and_print(mutation_request(name, BranchMutation::Create)?).await
}

pub async fn delete(name: RefName) -> Result<()> {
    execute_and_print(mutation_request(name, BranchMutation::Delete)?).await
}

pub async fn switch(name: RefName) -> Result<()> {
    execute_and_print(mutation_request(name, BranchMutation::Switch)?).await
}

/// Move the workspace onto `target` with a detached head.
pub async fn switch_detached(target: String) -> Result<()> {
    if target.trim().is_empty() {
        anyhow::bail!("provide the point in history to detach at, such as git:<sha>");
    }
    execute_and_print(BranchRequest::SwitchDetached {
        target,
        operation_id: OperationId::new(),
        actor: crate::commands::require_commit_author()?,
    })
    .await
}

/// The exact point in history a detached switch lands on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedDestination {
    /// What the workspace head and base will name. A Git commit keeps the
    /// external-object form `kin init` gives a detached checkout, so a store
    /// switched onto a commit records the same head as one admitted there.
    pub target: RefTarget,
    pub change_id: SemanticChangeId,
}

/// Resolve a detached switch's selector through the one shared ref grammar.
///
/// Takes the lease and history the caller already holds, so the destination is
/// read from the same authority the switch then commits against.
pub fn resolve_detached_destination(
    lease: &kin_db::AuthorityReadLease<kin_db::RepositoryAuthorityState>,
    workspace_id: &WorkspaceId,
    history: &kin_db::InMemoryGraph,
    selector: &str,
) -> Result<DetachedDestination> {
    let authority = super::ref_grammar::Authority::held(lease, workspace_id);
    let resolved = super::ref_grammar::resolve(&authority, history, selector)?;
    // A detached head must bind a resolved target. A commit object is kept as
    // named, and so is a change named outright with `kin:`. Anything else (a
    // tag object, a ref, or a parent hop such as `HEAD~1`) binds the one Git
    // commit the change was imported as, which is the head `kin init` records
    // for a detached checkout of that commit, and the change itself only when
    // no single commit names it.
    // A change named outright with `kin:` is kept as the change.
    let names_change = selector.trim_start().starts_with("kin:");
    let target = match resolved.target {
        Some(target @ RefTarget::Change { .. }) if names_change => target,
        Some(RefTarget::ExternalObject { object }) if object.kind == ExternalObjectKind::Commit => {
            RefTarget::external_object(object)
        }
        _ => imported_commit_of(lease, &resolved.change_id)
            .unwrap_or_else(|| RefTarget::change(resolved.change_id)),
    };
    Ok(DetachedDestination {
        target,
        change_id: resolved.change_id,
    })
}

/// The one Git commit `change_id` was imported as, when exactly one alias
/// names it.
fn imported_commit_of(
    lease: &kin_db::AuthorityReadLease<kin_db::RepositoryAuthorityState>,
    change_id: &SemanticChangeId,
) -> Option<RefTarget> {
    let mut commits = lease
        .metadata()
        .aliases
        .iter()
        .filter(|alias| &alias.change_id == change_id)
        .map(|alias| alias.oid);
    let only = commits.next()?;
    if commits.any(|other| other != only) {
        return None;
    }
    Some(RefTarget::external_object(kin_model::ExternalObjectId {
        kind: ExternalObjectKind::Commit,
        oid: only,
    }))
}

pub fn parse_branch_ref(name: Option<&str>, ref_hex: Option<&str>) -> Result<RefName> {
    let name = match (name, ref_hex) {
        (Some(name), None) => parse_ref_name(name)?,
        (None, Some(encoded)) => {
            let bytes = hex::decode(encoded)
                .with_context(|| format!("invalid repository ref hex '{encoded}'"))?;
            if hex::encode(&bytes) != encoded {
                anyhow::bail!(
                    "repository ref hex must use canonical lowercase hexadecimal encoding"
                );
            }
            RefName::from_bytes(bytes)
                .map_err(|error| anyhow::anyhow!("invalid byte-exact repository ref: {error}"))?
        }
        (Some(_), Some(_)) => {
            anyhow::bail!("provide either a branch name or --ref-hex, not both")
        }
        (None, None) => anyhow::bail!("provide a branch name or --ref-hex"),
    };
    require_branch_ref(&name)?;
    Ok(name)
}

enum BranchMutation {
    Create,
    Delete,
    Switch,
}

fn mutation_request(name: RefName, mutation: BranchMutation) -> Result<BranchRequest> {
    let operation_id = OperationId::new();
    let actor = crate::commands::require_commit_author()?;
    Ok(match mutation {
        BranchMutation::Create => BranchRequest::Create {
            name,
            operation_id,
            actor,
        },
        BranchMutation::Delete => BranchRequest::Delete {
            name,
            operation_id,
            actor,
        },
        BranchMutation::Switch => BranchRequest::Switch {
            name,
            operation_id,
            actor,
        },
    })
}

async fn execute(request: BranchRequest) -> Result<BranchResponse> {
    let layout = crate::commands::require_repository_layout()?;
    let daemon_url = crate::daemon_client::resolve_daemon_url(&layout)
        .await?
        .ok_or_else(|| crate::daemon_client::daemon_required_error("branch operations", &layout))?;
    let daemon = crate::daemon_client::DaemonClient::from_base_url_for_layout(daemon_url, &layout)?;
    daemon.branch(&request).await
}

async fn execute_and_print(request: BranchRequest) -> Result<()> {
    print_lines(execute(request).await?);
    Ok(())
}

fn require_branch_ref(name: &RefName) -> Result<()> {
    if !name.is_branch() {
        anyhow::bail!("branch command requires a ref below refs/heads/, found {name}");
    }
    Ok(())
}

fn print_lines(response: BranchResponse) {
    for line in response.lines {
        println!("{line}");
    }
}
