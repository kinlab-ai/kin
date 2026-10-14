// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The one writer for verification runs.
//!
//! A run is repository authority, as a review is: it says what was proved,
//! against which fingerprints, and by whom. Every run this daemon records,
//! for `kin verify run` or for the tests that reach a piece of work, is
//! committed here as one repository transaction, the way the review writer
//! commits a review event, and shown to the live graph only once authority
//! holds it.
//!
//! The order is what keeps the repository whole. A run reaches the graph with
//! relations from it to the entities it proves, and a working-copy admission
//! checks every relation's ends against the records authority holds. A run
//! written to the graph alone left those relations pointing at a run
//! authority had never seen, and every admission after it was refused, so the
//! repository stopped following its own working copy. A daemon that reopens
//! the store also serves exactly the runs it answered with.
//!
//! The writer takes `persist_lock` itself. Its caller holds the coordination
//! gate and a graph-authority mutation guard, the pair every authority writer
//! in this daemon holds.

use kin_cli::commands::verify::VerificationRecord;
use kin_model::AuthorId;

use crate::local_repository_authority::ActiveLocalRepositoryAuthority;
use crate::review_write::ReviewWriteRefusal;
use crate::state::{DaemonEvent, DaemonState};

/// Commit `record` to repository authority, then show it to the live graph.
///
/// Nothing is written when the commit is refused. Once it commits, a failure
/// to follow it in this process is reported as exactly that, with the
/// generation authority now holds.
pub(crate) fn commit_verification_record(
    state: &DaemonState,
    record: &VerificationRecord,
) -> Result<(), String> {
    if state.storage_backend.is_some() {
        return Err(
            "a hosted daemon takes verification runs through the repository transfer seam, not \
             through local runs"
                .to_string(),
        );
    }
    let _persistence = state
        .persist_lock
        .lock()
        .map_err(|_| "daemon persistence lock poisoned".to_string())?;
    let authority = ActiveLocalRepositoryAuthority::open_bound(state).map_err(|refusal| {
        format!(
            "a verification run is repository authority: {:#}",
            refusal.into_error()
        )
    })?;
    let delta = record.to_delta().map_err(|error| format!("{error:#}"))?;
    let receipt = crate::review_write::commit_collaboration::<std::convert::Infallible>(
        state,
        &authority,
        delta,
        &record.reason(),
        &AuthorId::new(record.actor_label().to_string()),
    )
    .map_err(|refusal| match refusal {
        ReviewWriteRefusal::Commit { detail, .. } => {
            format!("the verification run was not committed to repository authority: {detail}")
        }
        other => other.to_string(),
    })?;

    // Durable from here. The steps below make this process agree with what
    // authority now holds.
    let diverged = |detail: String| {
        format!(
            "the verification run committed to repository authority at generation {}, but the \
             daemon's live graph could not follow it: {detail}; restart the daemon to serve it \
             from authority again",
            receipt.generation
        )
    };
    state
        .record_repository_authority_commit(receipt.generation)
        .map_err(|error| diverged(error.to_string()))?;
    record
        .apply_to(state.graph.as_ref())
        .map_err(|error| diverged(format!("{error:#}")))?;
    crate::binding_history::restore_review_publication(state, &authority)
        .map_err(|error| diverged(error.to_string()))?;

    state.bump_version();
    state.mark_dirty();
    state.emit_event(DaemonEvent::GraphRootChanged {
        old_root_hash: None,
        new_root_hash: "verification-state".to_string(),
    });
    state.emit_event(DaemonEvent::RepositoryAuthorityChanged {
        repository_id: receipt.repository_id.to_string(),
        operation_id: receipt.operation_id,
        previous_generation: receipt.roots_before.generation,
        new_generation: receipt.generation,
    });
    Ok(())
}
