// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

pub use kin_core::git_update::{GitUpdateRequest, GitUpdateStatus};
use kin_model::{OperationId, SemanticChangeId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitUpdateResponse {
    pub operation_id: OperationId,
    pub previous_head: SemanticChangeId,
    pub head: SemanticChangeId,
    pub admitted_changes: usize,
    pub authority_generation: u64,
    pub workspace_follow: crate::commands::transfer::WorkspaceFollow,
    pub derived_views: crate::commands::transfer::DerivedViewRefresh,
}

#[derive(Debug)]
pub enum GitUpdateDispatch {
    Answered(Box<GitUpdateResponse>),
    Refused { status: u16, body: String },
    Lost(anyhow::Error),
}
