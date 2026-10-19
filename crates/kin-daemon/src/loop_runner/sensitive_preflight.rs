// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The sensitive-content verdict, reached before an admission derives anything.
//!
//! Repository authority scans every artifact an admission introduces and
//! refuses the whole admission for untracked content that looks like a
//! credential and carries no approval. It does that inside the publishing
//! transaction, after every changed source has been parsed and linked. On a
//! large delta that derivation takes most of an hour, and a refusal at its end
//! discards all of it. A fast-forward that brought nine such files cost nine
//! complete derivations, one approval at a time.
//!
//! The verdict needs none of the derivation. It depends on each introduced
//! artifact's bytes and on the approvals the published tree carries, and both
//! are in hand once the tree is planned. An added artifact has a fresh
//! identity, so authority cannot already track it. The approvals are derived
//! from this same desired tree by the same policy derivation publication uses.
//! So the same scan runs here first and names every blocked artifact at once.
//!
//! It only ever refuses what publication would refuse. An artifact it passes is
//! still judged at publication, and when the approvals cannot be derived here
//! exactly as publication derives them, nothing is decided here at all.

use kin_model::{ResolvedTree, SensitiveArtifactKind, SharedAdmissionPolicy, TreeDelta, TreeEntry};
use tracing::debug;

use crate::error::{DaemonError, Result};
use crate::state::DaemonState;

/// Refuse, before any derivation, every introduced artifact the publishing
/// transaction's sensitive-content scan would refuse, naming them all.
pub(super) fn refuse_blocked_sensitive_content(
    state: &DaemonState,
    previous: &ResolvedTree,
    deltas: &[TreeDelta],
) -> Result<()> {
    let introduced = deltas
        .iter()
        .filter_map(|delta| match delta {
            TreeDelta::Added { new, .. } => Some(new),
            TreeDelta::Updated { .. } | TreeDelta::Removed { .. } => None,
        })
        .collect::<Vec<_>>();
    if introduced.is_empty() {
        return Ok(());
    }
    let desired = previous
        .apply(deltas)
        .map_err(super::invalid_tree_transition)?;
    let read = |hash: kin_model::Hash256| {
        state
            .blobs
            .read(&kin_blobs::Hash256(hash.0))
            .map_err(|error| kin_model::ModelError::InvalidOperation(error.to_string()))
    };
    let allowances = match SharedAdmissionPolicy::derive_from_tree_with_allowances(
        None,
        &desired,
        |hash| Ok(read(hash)?.len() as u64),
        read,
    ) {
        Ok((policy, _)) => policy.sensitive_allowances,
        Err(error) => {
            debug!(
                %error,
                "left the sensitive-content verdict to publication: the approvals this tree \
                 carries do not derive here"
            );
            return Ok(());
        }
    };
    let mut blocked = Vec::new();
    for artifact in introduced {
        let (hash, kind) = match artifact.entry {
            TreeEntry::Blob { hash, executable } => {
                (hash, SensitiveArtifactKind::Blob { executable })
            }
            TreeEntry::Symlink { target_blob } => (target_blob, SensitiveArtifactKind::Symlink),
            TreeEntry::Gitlink { .. } => continue,
        };
        let Ok(body) = read(hash) else {
            // Publication reads the same body and refuses for itself.
            continue;
        };
        if let Err(error @ kin_db::admission::SensitiveAdmissionError::Blocked { .. }) =
            kin_db::admission::enforce_sensitive_admission(
                &artifact.path,
                hash,
                kind,
                &body,
                false,
                &allowances,
            )
        {
            blocked.push(error);
        }
    }
    match kin_db::admission::sensitive_refusal(&blocked) {
        Some(refusal) => Err(DaemonError::Graph(kin_db::KinDbError::Model(
            kin_model::ModelError::InvalidOperation(refusal),
        ))),
        None => Ok(()),
    }
}
