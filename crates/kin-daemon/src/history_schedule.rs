// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Schedule a partial store's history derivation, and attach it at a start.
//!
//! A store `kin init --history background` created holds its checked-out
//! commit and derives its history in a worker process that `kin init` starts.
//! The daemon keeps that promise across restarts. As it starts it attaches a
//! history a worker already staged, before any state is open, because that is
//! the one moment no daemon is serving the partial store. Once it is serving it
//! starts a worker when the store still owes its history and none is running,
//! and again later if a worker failed, backing off between attempts.
//!
//! The worker is a separate `kin` process at background priority with a
//! bounded share of threads and memory (see
//! `kin_cli::commands::history_derive`), so a derivation never runs on the
//! threads that answer queries and never counts against this daemon's memory.

use std::time::Duration;

use kin_core::history_state::{self, DerivationState, HistoryMode, HistoryStanding};
use kin_core::KinLayout;

/// How long a serving daemon waits before its first look, so its own startup
/// work goes first.
const FIRST_LOOK: Duration = Duration::from_secs(60);
/// How often a serving daemon looks again. A look reads two small files, so
/// a worker that died is restarted within minutes.
const LOOK_EVERY: Duration = Duration::from_secs(5 * 60);
/// The shortest wait after a failed attempt; it doubles per attempt.
const RETRY_BASE: Duration = Duration::from_secs(5 * 60);
/// The longest wait after a failed attempt.
const RETRY_CEILING: Duration = Duration::from_secs(6 * 60 * 60);

/// Attach a staged history, or finish an attach a crash interrupted, before
/// this daemon opens the store. The caller holds the runtime authority.
///
/// Never fails the start: a store that cannot be attached stays partial and
/// keeps saying so.
pub fn before_open(layout: &KinLayout) {
    match kin_core::history_attach::recover_interrupted_attach(layout) {
        Ok(true) => {
            tracing::info!(
                "finished attaching this store's complete history after an interruption"
            );
            return;
        }
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(%error, "an interrupted history attach could not be finished");
            return;
        }
    }
    let HistoryStanding::Partial {
        progress: Some(progress),
        ..
    } = history_state::standing(layout)
    else {
        return;
    };
    if progress.state != DerivationState::Staged {
        return;
    }
    let Some(stage) = progress.detail.as_deref().map(std::path::PathBuf::from) else {
        return;
    };
    match kin_core::history_attach::attach_staged_history(layout, &stage) {
        Ok(_) => {
            let _ = history_state::clear_progress(layout);
            tracing::info!(
                stage = %stage.display(),
                "attached this store's complete history before opening it"
            );
        }
        Err(error) => {
            tracing::warn!(%error, "the staged history was not attached; the store stays partial");
            let mut waiting =
                kin_core::history_state::HistoryDerivationProgress::new(DerivationState::Waiting);
            waiting.attempts = progress.attempts;
            waiting.detail = Some(error.to_string());
            let _ = history_state::write_progress(layout, &waiting);
        }
    }
}

/// Whether the store at `layout` owes a derivation a worker should start now.
fn owes_a_worker(layout: &KinLayout, now: chrono::DateTime<chrono::Utc>) -> bool {
    let HistoryStanding::Partial { record, progress } = history_state::standing(layout) else {
        return false;
    };
    if record.mode != HistoryMode::Background || record.underivable.is_some() {
        return false;
    }
    match progress {
        None => true,
        Some(progress) => match progress.state {
            DerivationState::Queued => true,
            // A worker that is running holds the lock and a new one exits at
            // once; one that died left this state behind, and a new one
            // resumes it.
            DerivationState::Running => true,
            // Attached at the next start.
            DerivationState::Staged | DerivationState::Waiting => false,
            DerivationState::Failed => {
                let backoff = RETRY_BASE
                    .saturating_mul(2_u32.saturating_pow(progress.attempts.saturating_sub(1)))
                    .min(RETRY_CEILING);
                let since = now
                    .signed_duration_since(progress.updated_at)
                    .to_std()
                    .unwrap_or_default();
                since >= backoff
            }
        },
    }
}

/// The `kin` binary beside this daemon, which carries the worker.
fn kin_binary() -> Option<std::path::PathBuf> {
    let name = if cfg!(windows) { "kin.exe" } else { "kin" };
    let candidate = std::env::current_exe().ok()?.with_file_name(name);
    candidate.is_file().then_some(candidate)
}

/// Look after the store's history derivation for as long as this daemon runs.
pub fn spawn_scheduler(layout: KinLayout) {
    if !history_state::standing(&layout)
        .mode()
        .is_some_and(|mode| mode == HistoryMode::Background)
    {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_LOOK).await;
        loop {
            if history_state::standing(&layout).is_complete() {
                return;
            }
            if owes_a_worker(&layout, chrono::Utc::now()) {
                match kin_binary() {
                    Some(kin) => {
                        match kin_cli::commands::history_derive::spawn_worker_with(&kin, &layout) {
                            Ok(Some(pid)) => {
                                tracing::info!(pid, "started this store's history derivation")
                            }
                            Ok(None) => {}
                            Err(error) => tracing::warn!(
                                %error,
                                "could not start this store's history derivation"
                            ),
                        }
                    }
                    None => tracing::debug!(
                        "no kin binary beside this daemon, so it starts no history derivation"
                    ),
                }
            }
            tokio::time::sleep(LOOK_EVERY).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_core::history_state::{HistoryDerivationProgress, HistoryStateRecord};

    fn partial(dir: &std::path::Path, mode: HistoryMode) -> KinLayout {
        let root = dir.join(".kin");
        std::fs::create_dir_all(&root).unwrap();
        let layout = KinLayout::new(root);
        history_state::stamp_staged(
            &layout,
            &HistoryStateRecord::new(
                mode,
                "0123456789abcdef0123456789abcdef01234567".into(),
                None,
                "snapshot".into(),
                Vec::new(),
                Some(10),
                None,
            ),
        )
        .unwrap();
        layout
    }

    #[test]
    fn a_background_store_with_no_progress_owes_a_worker() {
        let dir = tempfile::tempdir().unwrap();
        let layout = partial(dir.path(), HistoryMode::Background);
        assert!(owes_a_worker(&layout, chrono::Utc::now()));
    }

    #[test]
    fn a_none_store_never_owes_one() {
        let dir = tempfile::tempdir().unwrap();
        let layout = partial(dir.path(), HistoryMode::None);
        assert!(!owes_a_worker(&layout, chrono::Utc::now()));
    }

    #[test]
    fn a_failed_attempt_backs_off_before_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let layout = partial(dir.path(), HistoryMode::Background);
        let mut failed = HistoryDerivationProgress::new(DerivationState::Failed);
        failed.attempts = 2;
        history_state::write_progress(&layout, &failed).unwrap();
        let now = failed.updated_at;
        assert!(!owes_a_worker(&layout, now + chrono::Duration::minutes(9)));
        assert!(owes_a_worker(&layout, now + chrono::Duration::minutes(11)));
    }

    #[test]
    fn a_staged_history_waits_for_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let layout = partial(dir.path(), HistoryMode::Background);
        history_state::write_progress(
            &layout,
            &HistoryDerivationProgress::new(DerivationState::Staged),
        )
        .unwrap();
        assert!(!owes_a_worker(&layout, chrono::Utc::now()));
    }
}
