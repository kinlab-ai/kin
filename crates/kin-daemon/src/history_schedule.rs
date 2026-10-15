// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Schedule a partial store's history derivation, and attach it when it is
//! ready.
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
//!
//! ## A daemon that is serving when the history arrives
//!
//! A daemon the local app reads every few seconds, or an agent keeps calling,
//! never idles out, so "attached at the next start" used to mean never: the
//! repository showed one change of thousands for as long as anyone used it.
//! A serving daemon now looks for a staged history every few seconds and, at
//! its first quiet point, hands the store over: it stops serving the way a
//! cooperative stop does, keeps the repository's runtime authority until its
//! process is gone, and its process starts a successor on its way out. The
//! successor attaches the history in [`before_open`], as every start does.
//!
//! The attach is not done in place. Everything a serving daemon holds, its
//! graph, its indexes, its open records and every task that reads them, is of
//! the partial store, and the attach replaces that store's contents wholesale.
//! Swapping them under a live daemon would leave each of those holding what
//! was just moved away. A start is the one place that opens all of it from
//! nothing, and the attach it runs is the journaled one a crash is already
//! recovered from.
//!
//! A quiet point is a look that finds no request being answered, no change
//! being published, no pass over the working copy running, no agent's claim on
//! an entity, and no open work in the store
//! ([`kin_core::history_attach::OpenWork`]). It is an estimate, made
//! to avoid leaving clients for nothing. The attach decides again for itself,
//! under the authority and with nothing serving, so work that opens between
//! the look and the stop is never dropped: the successor finds it, leaves the
//! store partial, and serves on until the next quiet point. Whatever the
//! handover is waiting for is written to the store's history progress, where
//! every surface reads it. A crash at any point of the attach is finished by
//! the next process to take the authority ([`recover_before_start`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kin_core::history_attach::{self, AttachOutcome, AttachReadiness, OpenWork};
use kin_core::history_state::{self, DerivationState, HistoryMode, HistoryStanding};
use kin_core::KinLayout;

use crate::state::{DaemonState, RECON_PROCESSING};

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
/// How often a serving daemon looks for a staged history while a derivation
/// is running or staged. A look reads two small records.
const HANDOFF_LOOK: Duration = Duration::from_secs(2);
/// How often it looks while no derivation is running or staged.
const HANDOFF_LOOK_WHEN_NOTHING_DERIVES: Duration = Duration::from_secs(30);

/// Finish a history attach a crash interrupted, before this process reads
/// anything else of the store.
///
/// A swap stopped between moving the partial store out and the complete one
/// in leaves a store with no manifest, no settings and no graph, and a start
/// reads the manifest long before it takes the runtime authority it keeps. So
/// a start that finds the journal takes the authority for this alone, finishes
/// the swap and lets go, and everything after it reads a whole store. Without
/// this, a daemon killed at that point left a store no start could open.
///
/// Never fails the start: when another daemon holds the authority, that
/// daemon is the one finishing the swap, and this start ends on its own once
/// it finds the repository taken.
pub fn recover_before_start(layout: &KinLayout) {
    if !history_attach::attach_is_interrupted(layout) {
        return;
    }
    let authority = match crate::daemon::acquire_daemon_authority(layout.root()) {
        Ok(authority) => authority,
        Err(error) => {
            tracing::warn!(
                %error,
                "an interrupted history attach was found, and the repository's runtime \
                 authority is held elsewhere, so it is left to its holder"
            );
            return;
        }
    };
    match history_attach::recover_interrupted_attach(layout) {
        Ok(true) => {
            tracing::info!("finished attaching this store's complete history after an interruption")
        }
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(%error, "an interrupted history attach could not be finished")
        }
    }
    drop(authority);
}

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
            reap_stages_of_a_complete_store(layout);
            return;
        }
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(%error, "an interrupted history attach could not be finished");
            return;
        }
    }
    let progress = match history_state::standing(layout) {
        HistoryStanding::Partial {
            progress: Some(progress),
            ..
        } => progress,
        HistoryStanding::Complete => {
            reap_stages_of_a_complete_store(layout);
            return;
        }
        _ => return,
    };
    // A stage that is gone is left for the scheduler, which starts the
    // derivation again; nothing here could attach it.
    let Some(stage) = kin_cli::commands::history_derive::finished_stage(Some(&progress)) else {
        return;
    };
    match history_attach::attach_staged_history(layout, &stage) {
        Ok(AttachOutcome::Attached | AttachOutcome::AlreadyComplete) => {
            let _ = history_state::clear_progress(layout);
            tracing::info!(
                stage = %stage.display(),
                "attached this store's complete history before opening it"
            );
        }
        Ok(AttachOutcome::Deferred(work)) => {
            // Nothing was moved and the stage is whole. This daemon serves the
            // partial store, and hands it over once the work is finished.
            tracing::info!(
                waiting_for = work.describe(),
                "the staged history was not attached yet; the store holds open work"
            );
            let _ = history_state::note_staged_wait(layout, Some(work.describe()));
        }
        Err(error) => record_refusal(layout, progress.attempts, &error.to_string()),
    }
}

/// Remove what is left of stages derived for a store that is now complete.
///
/// An attach removes its stage last, after the journal that would finish it is
/// gone, so a crash during that removal leaves the stage, the partial store it
/// replaced, behind with nothing to say so. A complete store has no use for a
/// stage, and no derivation starts for one, so whatever is left is reclaimed
/// here. Only stages named for this repository and holding no other
/// repository's store are touched.
fn reap_stages_of_a_complete_store(layout: &KinLayout) {
    let Ok(manifest) = kin_core::KinManifest::load(&layout.manifest_path()) else {
        return;
    };
    let reaped =
        history_attach::reap_abandoned_stages(layout.working_dir(), &manifest.repo_id, None);
    if reaped > 0 {
        tracing::info!(
            reaped,
            "removed what was left of history stages this complete store no longer needs"
        );
    }
}

/// Record that the staged history cannot be attached, for a reason waiting
/// does not cure. The store stays partial and says why.
fn record_refusal(layout: &KinLayout, attempts: u32, reason: &str) {
    tracing::warn!(
        error = reason,
        "the staged history was not attached; the store stays partial"
    );
    let mut waiting =
        kin_core::history_state::HistoryDerivationProgress::new(DerivationState::Waiting);
    waiting.attempts = attempts;
    waiting.detail = Some(reason.to_string());
    let _ = history_state::write_progress(layout, &waiting);
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
            // Handed over at the next quiet point, unless the stage it
            // names is gone, which only a new derivation repairs.
            DerivationState::Staged => {
                kin_cli::commands::history_derive::finished_stage(Some(&progress)).is_none()
            }
            DerivationState::Waiting => false,
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

/// What keeps a serving daemon from handing its store over right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuietBlocker {
    /// A request is being answered.
    Request,
    /// A change is being published.
    Publication,
    /// The first pass over the working copy has not finished.
    FirstScan,
    /// Changes to the working copy are being taken in.
    Reconciliation,
    /// A command is waiting to read a language-server pass's result.
    SweepWaiter,
    /// An agent holds a claim on an entity it is about to change.
    Claim,
    /// The store holds open work the attach would drop.
    Open(OpenWork),
}

impl QuietBlocker {
    /// What the handover waits for, as a clause that follows "waiting for".
    pub(crate) const fn describe(self) -> &'static str {
        match self {
            Self::Request => "the requests being answered to finish",
            Self::Publication => "a change being published to land",
            Self::FirstScan => "the first read of the working copy to finish",
            Self::Reconciliation => "changes to the working copy to be taken in",
            Self::SweepWaiter => "a command reading language-server results to finish",
            Self::Claim => "an agent's claim on an entity to be released",
            Self::Open(work) => work.describe(),
        }
    }
}

/// The first thing that keeps `state` from being at a quiet point, if anything
/// does.
///
/// Embedding and language-server enrichment do not: both are derived from the
/// partial store, the attach replaces it, and the successor derives them again
/// for the complete one. An attached session or an event subscriber does not
/// either: neither holds work, and both find the successor the way they find
/// any daemon that restarted. A session that holds a claim does, because the
/// claim is what keeps another agent off the entity it names, and the store
/// that replaces this one knows nothing of it.
pub(crate) fn quiet_blocker(state: &DaemonState) -> Option<QuietBlocker> {
    if state.active_request_count() > 0 {
        return Some(QuietBlocker::Request);
    }
    if state.prepared_publication.pending() {
        return Some(QuietBlocker::Publication);
    }
    if !state.is_initialized.load(Ordering::Relaxed) {
        return Some(QuietBlocker::FirstScan);
    }
    if state.reconciliation_status.load(Ordering::Relaxed) == RECON_PROCESSING {
        return Some(QuietBlocker::Reconciliation);
    }
    if state
        .sweep_waiters
        .any_waiting(crate::sweep_waiters::process_alive)
    {
        return Some(QuietBlocker::SweepWaiter);
    }
    // Unknown counts as held: a claim that cannot be read may still stand.
    let holds_a_claim = state.coordinator.list_sessions().map_or(true, |sessions| {
        sessions
            .iter()
            .filter(|session| session.vendor != crate::agent_activity::DAEMON_OWN_VENDOR)
            .any(|session| {
                state
                    .coordinator
                    .list_intents(&session.session_id)
                    .map_or(true, |claims| !claims.is_empty())
            })
    });
    if holds_a_claim {
        return Some(QuietBlocker::Claim);
    }
    // The transactions this daemon holds, ahead of their durable mirror, which
    // the store's own records answer for below.
    let holds_a_transaction = state
        .mcp_transactions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .any(|transaction| !matches!(transaction.state.as_str(), "committed" | "aborted"));
    if holds_a_transaction {
        return Some(QuietBlocker::Open(OpenWork::Transaction));
    }
    history_attach::open_work(&state.layout).map(QuietBlocker::Open)
}

/// What one look at a serving daemon's store found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Look {
    /// No history is staged. `deriving` says whether one may be soon.
    Nothing { deriving: bool },
    /// A history is staged and the handover waits for this.
    Wait(QuietBlocker),
    /// A history is staged and this is a quiet point.
    HandOff,
    /// The history is complete, or can never be attached: nothing to watch.
    Over,
}

/// Look at the store `state` serves and decide what a staged history calls
/// for. Blocking: the last check before a handover reads the store's
/// persisted authority.
pub(crate) fn look(state: &DaemonState) -> Look {
    let layout = &state.layout;
    let progress = match history_state::standing(layout) {
        HistoryStanding::Complete => return Look::Over,
        HistoryStanding::Partial {
            progress: Some(progress),
            ..
        } => progress,
        // A background store whose worker has reported nothing yet is owed a
        // derivation that is about to start, and a small repository's is over
        // in seconds.
        HistoryStanding::Partial {
            record,
            progress: None,
        } => {
            return Look::Nothing {
                deriving: record.mode == HistoryMode::Background && record.underivable.is_none(),
            }
        }
        HistoryStanding::Unreadable { .. } => return Look::Nothing { deriving: false },
    };
    let Some(stage) = kin_cli::commands::history_derive::finished_stage(Some(&progress)) else {
        return Look::Nothing {
            deriving: matches!(
                progress.state,
                DerivationState::Queued | DerivationState::Running
            ),
        };
    };
    if let Some(blocker) = quiet_blocker(state) {
        return Look::Wait(blocker);
    }
    // Asked here so this daemon does not leave its clients for an attach that
    // would wait or be refused. The attach asks again for itself.
    match history_attach::attach_readiness(layout, &stage) {
        Ok(AttachReadiness::Ready) => {}
        Ok(AttachReadiness::AlreadyComplete) => return Look::Over,
        Ok(AttachReadiness::Waiting(work)) => return Look::Wait(QuietBlocker::Open(work)),
        Err(error) => {
            record_refusal(layout, progress.attempts, &error.to_string());
            return Look::Over;
        }
    }
    // That read takes as long as the partial store is large, and a request
    // may have arrived during it.
    match quiet_blocker(state) {
        Some(blocker) => Look::Wait(blocker),
        None => Look::HandOff,
    }
}

/// Watch for a staged history for as long as this daemon serves a partial
/// store, and hand the store over at the first quiet point after one arrives.
pub(crate) fn spawn_handoff_watch(
    state: Arc<DaemonState>,
    cancel: tokio::sync::watch::Receiver<bool>,
) {
    spawn_handoff_watch_every(
        state,
        cancel,
        HANDOFF_LOOK,
        HANDOFF_LOOK_WHEN_NOTHING_DERIVES,
    );
}

/// [`spawn_handoff_watch`] at a cadence its caller names: `look_every` while
/// a derivation is running or staged, `idle_every` otherwise.
fn spawn_handoff_watch_every(
    state: Arc<DaemonState>,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    look_every: Duration,
    idle_every: Duration,
) -> Option<tokio::task::JoinHandle<()>> {
    if history_state::standing(&state.layout).is_complete() {
        return None;
    }
    Some(tokio::spawn(async move {
        let mut wait = look_every;
        // The blocker the looks keep meeting, and since when. One that has
        // stood for a whole look interval is worth telling people about; one
        // met once is usually a request that has been answered before anyone
        // could read the note.
        let mut met: Option<(QuietBlocker, tokio::time::Instant)> = None;
        loop {
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                _ = cancel.changed() => return,
            }
            if *cancel.borrow() {
                return;
            }
            let looking = Arc::clone(&state);
            let Ok(found) = tokio::task::spawn_blocking(move || look(&looking)).await else {
                continue;
            };
            wait = look_every;
            match found {
                Look::Over => return,
                Look::Nothing { deriving } => {
                    met = None;
                    if !deriving {
                        wait = idle_every;
                    }
                }
                Look::Wait(blocker) => {
                    let since = match met {
                        Some((standing, since)) if standing == blocker => since,
                        _ => tokio::time::Instant::now(),
                    };
                    met = Some((blocker, since));
                    if since.elapsed() >= look_every {
                        note_wait(&state.layout, Some(blocker));
                    }
                    // Requests come and go in milliseconds, so the gap
                    // between two of them is looked for more often than
                    // anything else is waited on.
                    if blocker == QuietBlocker::Request {
                        wait = look_every / 10;
                    }
                }
                Look::HandOff => {
                    note_wait(&state.layout, None);
                    if state.hand_off_for_history() {
                        tracing::info!(
                            "this store's complete history is staged and nothing is open; \
                             stopping so the next start attaches it"
                        );
                        return;
                    }
                }
            }
        }
    }))
}

fn note_wait(layout: &KinLayout, blocker: Option<QuietBlocker>) {
    match history_state::note_staged_wait(layout, blocker.map(QuietBlocker::describe)) {
        Ok(noted) => {
            if let (true, Some(blocker)) = (noted, blocker) {
                tracing::info!(
                    waiting_for = blocker.describe(),
                    "this store's complete history is staged; waiting for a quiet point"
                );
            }
        }
        Err(error) => {
            tracing::warn!(%error, "could not record what the staged history is waiting for")
        }
    }
}

/// Set once this process has stopped serving to hand its store over, so its
/// entrypoint starts the successor as the process ends.
static HANDED_OFF: AtomicBool = AtomicBool::new(false);

/// Hold the repository's runtime authority until this process is gone, and
/// mark the process as one that handed its store over.
///
/// A daemon's process outlives its run: the entrypoint gives the runtime's
/// blocking threads a grace period afterwards, and one of them may still be
/// writing a record by name. Released here, the authority would let the
/// successor attach the history under that thread, and the write would land in
/// the store the successor installed. Never released, it ends with the
/// process, which is the first moment nothing of this daemon can write.
pub(crate) fn keep_authority_until_exit(authority: crate::lifecycle::DaemonLock) {
    std::mem::forget(authority);
    HANDED_OFF.store(true, Ordering::SeqCst);
}

/// Whether this process stopped serving to hand its store over.
pub fn handed_off() -> bool {
    HANDED_OFF.load(Ordering::SeqCst)
}

/// The environment this process was started with, before it adopted anything
/// of its own.
static STARTED_WITH: std::sync::OnceLock<Vec<(std::ffi::OsString, std::ffi::OsString)>> =
    std::sync::OnceLock::new();

/// Remember the environment this process was started with. Called first
/// thing, while the process is single-threaded and before it writes the
/// resource profile, retrieval arm and tool directories it resolves into its
/// own environment.
///
/// A successor that inherited those would read them as an operator's explicit
/// settings, which outrank a store's own, and the store it opens is not the
/// one they were resolved from.
pub fn remember_start_environment() {
    let _ = STARTED_WITH.set(std::env::vars_os().collect());
}

/// Start the daemon that takes this one's place, exactly as this one was
/// started: the same program, arguments, environment and output.
///
/// Called as the process ends, after the runtime is gone. The successor waits
/// on the runtime authority this process still holds, takes it the moment the
/// process is gone, and attaches the staged history before it opens the
/// store. A successor that cannot be started costs nothing but time: the next
/// client to call starts a daemon as it always has, and that start attaches.
pub fn start_successor() -> std::io::Result<u32> {
    let program = std::env::current_exe()?;
    let mut command = std::process::Command::new(program);
    command
        .args(std::env::args_os().skip(1))
        .stdin(std::process::Stdio::null());
    if let Some(environment) = STARTED_WITH.get() {
        command.env_clear().envs(environment.iter().cloned());
    }
    kin_daemon_spawn::detach_from_caller(&mut command);
    command.spawn().map(|child| child.id())
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

    /// A Git repository of two commits, converted with only its checked-out
    /// commit, with its complete history derived and staged beside it: what a
    /// background worker leaves for a daemon to find.
    struct StagedHistory {
        _repository: tempfile::TempDir,
        layout: KinLayout,
        repository_id: kin_model::RepositoryId,
        stage: std::path::PathBuf,
    }

    impl StagedHistory {
        fn progress(&self) -> Option<HistoryDerivationProgress> {
            match history_state::standing(&self.layout) {
                HistoryStanding::Partial { progress, .. } => progress,
                _ => None,
            }
        }

        fn is_complete(&self) -> bool {
            history_state::standing(&self.layout).is_complete()
        }

        fn stage_is_whole(&self) -> bool {
            history_attach::staged_store(&self.stage)
                .join("manifest.json")
                .is_file()
        }

        /// How many changes the store's persisted authority holds.
        fn changes(&self) -> usize {
            let (authority, _) =
                kin_core::repository_authority::open_persisted_local_repository_authority(
                    self.repository_id.clone(),
                    Arc::new(kin_db::LocalFileBackend::new(self.layout.kindb_dir())),
                )
                .unwrap();
            let lease = authority.read_authority();
            lease.snapshot().changes.len()
        }

        /// The store as a start finds it after the attach: complete, with
        /// both commits, no journal, no stage, and a daemon state that opens.
        fn assert_attached(&self) {
            assert!(
                self.is_complete(),
                "the store still records partial history"
            );
            assert!(!self.layout.root().join("history-attach.json").exists());
            assert!(!self.stage.exists(), "the stage was left behind");
            assert_eq!(self.changes(), 2, "both commits are in the attached store");
            DaemonState::open(self.layout.clone()).expect("the attached store opens");
        }
    }

    fn staged_history() -> StagedHistory {
        let repository = tempfile::tempdir().unwrap();
        let root = repository.path();
        let git = |args: &[&str]| {
            let output = kin_git::test_support::fixture_git_in(root)
                .args([
                    "-c",
                    "user.name=History Test",
                    "-c",
                    "user.email=history@example.test",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q", "--initial-branch=main"]);
        // The conversion resolves these in this process too. Pin both to the
        // temporary repository rather than the developer's global scope.
        let hooks = root.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        git(&["config", "core.hooksPath", hooks.to_str().unwrap()]);
        let excludes = root.join(".git/fixture-excludes");
        std::fs::write(&excludes, b"").unwrap();
        git(&["config", "core.excludesFile", excludes.to_str().unwrap()]);
        std::fs::write(root.join("a.py"), b"def a():\n    return 1\n").unwrap();
        git(&["add", "a.py"]);
        git(&["commit", "-q", "-s", "-m", "Add a"]);
        std::fs::write(
            root.join("b.py"),
            b"from a import a\n\ndef b():\n    return a()\n",
        )
        .unwrap();
        git(&["add", "b.py"]);
        git(&["commit", "-q", "-s", "-m", "Add b"]);

        let partial =
            kin_core::init_current_commit_from_git(root, None, HistoryMode::Background).unwrap();
        let stage = history_attach::new_stage_parent(root, partial.repository_id.as_str()).unwrap();
        kin_core::derive_complete_history_into_stage(
            root,
            partial.layout.root(),
            &partial.repository_id,
            &stage,
        )
        .unwrap();
        let mut staged = HistoryDerivationProgress::new(DerivationState::Staged);
        staged.attempts = 1;
        staged.detail = Some(stage.display().to_string());
        history_state::write_progress(&partial.layout, &staged).unwrap();
        StagedHistory {
            _repository: repository,
            layout: partial.layout,
            repository_id: partial.repository_id,
            stage,
        }
    }

    fn transaction(state: &str) -> kin_mcp::McpTransaction {
        kin_mcp::McpTransaction {
            transaction_id: "transaction".into(),
            session_id: "session".into(),
            scope: "entity".into(),
            state: state.into(),
            staged_operations: Vec::new(),
            created_at: None,
            commit_payload_hash: None,
            last_activity_at: kin_model::Timestamp::now(),
        }
    }

    /// A serving daemon that finds its history staged waits while anything is
    /// in flight or open, names what it waits for, moves nothing, and reaches
    /// the handover only once all of it is finished.
    #[test]
    fn a_staged_history_waits_for_every_kind_of_work_before_the_handover() {
        let staged = staged_history();
        let state = DaemonState::open(staged.layout.clone()).unwrap();

        // Until a history is staged there is nothing to hand over, and the
        // daemon keeps looking often only while one is on its way.
        let worker_left = staged.progress().unwrap();
        history_state::clear_progress(&staged.layout).unwrap();
        assert_eq!(look(&state), Look::Nothing { deriving: true });
        let mut running = HistoryDerivationProgress::new(DerivationState::Running);
        running.pid = Some(std::process::id());
        history_state::write_progress(&staged.layout, &running).unwrap();
        assert_eq!(look(&state), Look::Nothing { deriving: true });
        history_state::write_progress(
            &staged.layout,
            &HistoryDerivationProgress::new(DerivationState::Failed),
        )
        .unwrap();
        assert_eq!(look(&state), Look::Nothing { deriving: false });
        history_state::write_progress(&staged.layout, &worker_left).unwrap();

        state.is_initialized.store(false, Ordering::Relaxed);
        assert_eq!(look(&state), Look::Wait(QuietBlocker::FirstScan));
        state.is_initialized.store(true, Ordering::Relaxed);

        state.begin_request();
        assert_eq!(look(&state), Look::Wait(QuietBlocker::Request));
        state.end_request();

        state
            .reconciliation_status
            .store(RECON_PROCESSING, Ordering::Relaxed);
        assert_eq!(look(&state), Look::Wait(QuietBlocker::Reconciliation));
        state
            .reconciliation_status
            .store(crate::state::RECON_IDLE, Ordering::Relaxed);

        // An attached session holds nothing by being attached. One that
        // claims an entity does, until it lets go.
        let session = state
            .coordinator
            .register_session(
                "claude-code",
                "test",
                kin_model::session::SessionTransport::Mcp,
                None,
                std::path::PathBuf::from("/"),
                kin_model::session::SessionCapabilities::default(),
            )
            .unwrap();
        assert_eq!(look(&state), Look::HandOff);
        let claim = match state
            .coordinator
            .register_intent(
                &session,
                vec![kin_model::session::IntentScope::Entity(
                    kin_model::ids::EntityId::new(),
                )],
                kin_model::session::LockType::Soft,
                "rename an entity",
                None,
            )
            .unwrap()
        {
            crate::session_registry::IntentRegistrationResult::Registered { intent_id, .. } => {
                intent_id
            }
            other => panic!("the claim was not registered: {other:?}"),
        };
        assert_eq!(look(&state), Look::Wait(QuietBlocker::Claim));
        state.coordinator.release_intent(&session, &claim).unwrap();
        assert_eq!(look(&state), Look::HandOff);

        // A transaction this daemon holds, before and after it is settled.
        state
            .mcp_transactions
            .lock()
            .unwrap()
            .insert("transaction".into(), transaction("active"));
        assert_eq!(
            look(&state),
            Look::Wait(QuietBlocker::Open(OpenWork::Transaction))
        );
        state
            .mcp_transactions
            .lock()
            .unwrap()
            .insert("transaction".into(), transaction("committed"));
        assert_eq!(look(&state), Look::HandOff);
        state.mcp_transactions.lock().unwrap().clear();

        // Work the store's own records hold, which outlives this daemon.
        std::fs::write(staged.layout.merge_state_path(), b"{}").unwrap();
        assert_eq!(
            look(&state),
            Look::Wait(QuietBlocker::Open(OpenWork::Merge))
        );
        std::fs::remove_file(staged.layout.merge_state_path()).unwrap();

        assert_eq!(look(&state), Look::HandOff);
        // Looking changes nothing: the store is still partial, its history
        // still staged where the worker left it.
        assert!(!staged.is_complete());
        assert!(staged.stage_is_whole());
        assert_eq!(
            staged.progress().unwrap().staged_at(),
            Some(staged.stage.clone())
        );
        assert_eq!(staged.changes(), 1);

        // A state no daemon loop runs has nothing to stop, and is not marked
        // as handing off for it.
        assert!(!state.hand_off_for_history());
        assert!(!state.handing_off_for_history());
    }

    /// A staged history that can never be attached is recorded as such once,
    /// and the daemon stops watching rather than leaving its clients for it.
    #[test]
    fn a_stage_that_cannot_be_attached_is_recorded_and_never_handed_off() {
        let staged = staged_history();
        let state = DaemonState::open(staged.layout.clone()).unwrap();
        state.is_initialized.store(true, Ordering::Relaxed);
        // A staged store that itself records partial history is not complete.
        let HistoryStanding::Partial { record, .. } = history_state::standing(&staged.layout)
        else {
            panic!("the fixture store is partial");
        };
        history_state::stamp_staged(
            &KinLayout::new(history_attach::staged_store(&staged.stage)),
            &record,
        )
        .unwrap();

        assert_eq!(look(&state), Look::Over);
        let progress = staged.progress().unwrap();
        assert_eq!(progress.state, DerivationState::Waiting);
        assert!(
            progress
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("does not hold complete history")),
            "{progress:?}"
        );
        assert!(!staged.is_complete());
        assert_eq!(staged.changes(), 1);
    }

    /// The watch a serving daemon runs: while a request is held it records
    /// what it waits for and stops nothing; once the request ends it stops the
    /// daemon for the handover, and the next start attaches the history.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_watch_waits_while_busy_then_hands_off_and_the_next_start_attaches() {
        let staged = staged_history();
        let state = Arc::new(DaemonState::open(staged.layout.clone()).unwrap());
        state.is_initialized.store(true, Ordering::Relaxed);
        let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
        state.stop_trigger.set(cancel_tx).unwrap();

        state.begin_request();
        let watch = spawn_handoff_watch_every(
            Arc::clone(&state),
            cancel_rx.clone(),
            Duration::from_millis(20),
            Duration::from_millis(20),
        )
        .expect("a partial store is watched");
        let waiting_for = QuietBlocker::Request.describe();
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while staged
            .progress()
            .and_then(|progress| progress.staged_waiting_for().map(str::to_string))
            .as_deref()
            != Some(waiting_for)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the watch never recorded what it waits for: {:?}",
                staged.progress()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Several looks later it is still serving and has moved nothing.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !*cancel_rx.borrow(),
            "the daemon was stopped under a request"
        );
        assert!(!state.handing_off_for_history());
        assert!(!staged.is_complete());
        assert!(staged.stage_is_whole());
        let sentence = history_state::standing(&staged.layout).sentence().unwrap();
        assert!(
            sentence.contains(&format!("(waiting for {waiting_for})")),
            "{sentence}"
        );

        state.end_request();
        tokio::time::timeout(Duration::from_secs(60), cancel_rx.changed())
            .await
            .expect("with nothing in flight the daemon is stopped for the handover")
            .unwrap();
        assert!(*cancel_rx.borrow());
        assert!(state.handing_off_for_history());
        tokio::time::timeout(Duration::from_secs(60), watch)
            .await
            .expect("the watch ends with the handover")
            .unwrap();
        // The handover itself attaches nothing: that is the next start's.
        let progress = staged.progress().unwrap();
        assert_eq!(progress.staged_at(), Some(staged.stage.clone()));
        assert_eq!(progress.staged_waiting_for(), None);
        assert!(!staged.is_complete());
        drop(state);

        before_open(&staged.layout);
        staged.assert_attached();
        assert!(staged.progress().is_none());
    }

    /// A start that finds open work attaches nothing and keeps the history
    /// staged, saying what it waits for. A start after the work is finished
    /// attaches it.
    #[test]
    fn a_start_waits_for_open_work_and_attaches_once_it_is_finished() {
        let staged = staged_history();
        let mirror = staged
            .layout
            .root()
            .join(history_attach::AGENT_TRANSACTIONS_RECORD);
        let open: std::collections::HashMap<_, _> =
            [("transaction".to_string(), transaction("active"))].into();
        std::fs::write(&mirror, serde_json::to_vec(&open).unwrap()).unwrap();

        before_open(&staged.layout);
        assert!(!staged.is_complete());
        assert!(staged.stage_is_whole());
        let progress = staged.progress().unwrap();
        assert_eq!(progress.state, DerivationState::Staged);
        assert_eq!(progress.staged_at(), Some(staged.stage.clone()));
        assert_eq!(
            progress.staged_waiting_for(),
            Some(OpenWork::Transaction.describe())
        );
        // The transaction is exactly where its agent left it.
        assert_eq!(
            crate::state::load_persisted_mcp_transactions_checked(&staged.layout)
                .unwrap()
                .get("transaction")
                .map(|transaction| transaction.state.clone()),
            Some("active".to_string())
        );
        assert_eq!(staged.changes(), 1);

        let settled: std::collections::HashMap<_, _> =
            [("transaction".to_string(), transaction("aborted"))].into();
        std::fs::write(&mirror, serde_json::to_vec(&settled).unwrap()).unwrap();
        before_open(&staged.layout);
        staged.assert_attached();
    }

    /// An attach stopped part way, as a crash stops it, is finished by the
    /// next start before anything opens the store, whichever half of the swap
    /// it was stopped in.
    #[test]
    fn a_start_finishes_an_attach_a_crash_interrupted() {
        let journal_phase = |layout: &KinLayout| {
            let raw = std::fs::read(layout.root().join("history-attach.json")).unwrap();
            let journal: serde_json::Value = serde_json::from_slice(&raw).unwrap();
            journal["phase"].as_str().unwrap().to_string()
        };
        // Stopped before the first move, part way through moving the partial
        // store out, and part way through moving the complete one in.
        for stop in ["before the first move", "moving out", "moving in"] {
            let staged = staged_history();
            history_attach::interruption::after(Some(match stop {
                "before the first move" => 0,
                _ => 2,
            }));
            let stopped =
                history_attach::attach_staged_history(&staged.layout, &staged.stage).unwrap_err();
            assert!(stopped.to_string().contains("interrupted"), "{stopped}");
            assert_eq!(journal_phase(&staged.layout), "evacuating", "{stop}");
            if stop == "moving in" {
                while journal_phase(&staged.layout) != "installing" {
                    history_attach::interruption::after(Some(1));
                    history_attach::recover_interrupted_attach(&staged.layout).unwrap_err();
                }
                // Two entries of the complete store are in place, the rest
                // are still staged.
                history_attach::interruption::after(Some(2));
                history_attach::recover_interrupted_attach(&staged.layout).unwrap_err();
                assert!(staged.stage_is_whole() || staged.layout.manifest_path().is_file());
            }
            history_attach::interruption::after(None);

            before_open(&staged.layout);
            staged.assert_attached();
        }
    }

    /// A daemon killed after the partial store was moved out and before the
    /// complete one was moved in leaves a store with no manifest. A start
    /// finishes the attach before it reads the manifest, so that store opens.
    #[test]
    fn a_start_recovers_a_store_whose_manifest_was_already_moved_out() {
        let staged = staged_history();
        // Stop the swap one step at a time until the store's manifest has
        // been moved out, which is how a kill at that point leaves it.
        history_attach::interruption::after(Some(0));
        history_attach::attach_staged_history(&staged.layout, &staged.stage).unwrap_err();
        while staged.layout.manifest_path().exists() {
            history_attach::interruption::after(Some(1));
            history_attach::recover_interrupted_attach(&staged.layout).unwrap_err();
        }
        history_attach::interruption::after(None);
        assert!(history_attach::attach_is_interrupted(&staged.layout));
        assert!(
            kin_core::manifest::resolve_repo_id(&staged.layout, None).is_err(),
            "the half-moved store still answers for its identity"
        );

        recover_before_start(&staged.layout);
        assert!(!history_attach::attach_is_interrupted(&staged.layout));
        assert_eq!(
            kin_core::manifest::resolve_repo_id(&staged.layout, None).unwrap(),
            staged.repository_id.as_str()
        );
        staged.assert_attached();
        // Nothing is left to recover, and asking again changes nothing.
        recover_before_start(&staged.layout);
        staged.assert_attached();
    }

    /// A crash while an attach removes its stage, after the journal is gone,
    /// leaves the stage behind. The next start of the complete store removes
    /// it, and never a stage of another repository.
    #[test]
    fn a_start_removes_a_stage_an_attach_left_behind() {
        let staged = staged_history();
        before_open(&staged.layout);
        staged.assert_attached();
        let working_dir = staged.layout.working_dir().to_path_buf();
        // What a crash part way through removing the stage leaves: some of
        // the replaced partial store, and no staged store.
        let left =
            history_attach::new_stage_parent(&working_dir, staged.repository_id.as_str()).unwrap();
        std::fs::create_dir_all(left.join("provisional/kindb")).unwrap();
        std::fs::write(left.join("provisional/kindb/left"), b"replaced").unwrap();
        let theirs = history_attach::new_stage_parent(&working_dir, "another-repository").unwrap();

        before_open(&staged.layout);
        assert!(!left.exists(), "the leftover stage was kept");
        assert!(theirs.exists(), "another repository's stage was removed");
        staged.assert_attached();
        std::fs::remove_dir_all(theirs).unwrap();
    }

    /// A staged history is handed over, not derived again, for as long as
    /// its stage is there. A stage that is gone is derived again.
    #[test]
    fn a_staged_history_owes_a_worker_only_when_its_stage_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let layout = partial(dir.path(), HistoryMode::Background);
        let stage = dir.path().join("stage");
        let staged_store = history_attach::staged_store(&stage);
        std::fs::create_dir_all(&staged_store).unwrap();
        std::fs::write(staged_store.join("manifest.json"), b"{}").unwrap();
        let mut staged = HistoryDerivationProgress::new(DerivationState::Staged);
        staged.detail = Some(stage.display().to_string());
        history_state::write_progress(&layout, &staged).unwrap();
        assert!(!owes_a_worker(&layout, chrono::Utc::now()));
        std::fs::remove_dir_all(&stage).unwrap();
        assert!(owes_a_worker(&layout, chrono::Utc::now()));
    }
}
