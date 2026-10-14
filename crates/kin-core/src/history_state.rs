// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Durable record of how much Git history a store has admitted.
//!
//! A conversion used to admit a repository's whole reachable Git history
//! before the store existed, so every store was complete by construction and
//! nothing needed to say so. A store can now start from the checked-out commit
//! alone: `kin init --history background` publishes it at once and derives the
//! history afterwards, and `kin init --history none` never derives it. Such a
//! store holds one snapshot change for the checked-out tree and no Git history
//! at all, and this record is what says so.
//!
//! ## What the record claims
//!
//! Its presence claims that the store's history is partial: the store admitted
//! the checked-out commit's tree as a single snapshot change, and none of that
//! commit's ancestors. It names the Git commit the snapshot was taken from and
//! the parents that were left out, so every surface can say exactly where the
//! admitted history stops.
//!
//! Its absence means what it has always meant. Every store a build before this
//! record created is a complete exact-reachable admission or a native store,
//! and a complete admission never writes the record.
//!
//! ## Why a record and not a guess
//!
//! A partial store has no Git external authority and one native root change,
//! which a native store also has, so nothing in the authority alone tells a
//! reader which of the two it is looking at. Guessing either way is wrong in a
//! way this repository has paid for before: calling a native store partial
//! would refuse its pushes, and calling a partial store complete would publish
//! a truncated history as whole.
//!
//! ## Reads fail closed
//!
//! An unreadable record reads as partial. A record that cannot be read may
//! still describe a partial store, and the only safe answer to "is this history
//! complete" when the answer is unknown is no. See [`HistoryStanding`].
//!
//! ## Where it is written
//!
//! [`stamp_staged`] writes it into the STAGED layout, before the rename that
//! publishes `.kin`, so a partial store and its record become visible in the
//! same instant and no reader ever sees a partial store without it. It is
//! removed only when a complete admission replaces the store, and the store it
//! is removed with is the partial one.
//!
//! The derivation progress beside it ([`HistoryDerivationProgress`]) is a
//! separate file, rewritten as the background derivation advances. It is a
//! report of work, never a claim about the store: a store is complete only when
//! the record is gone, whatever the progress file says.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::layout::KinLayout;

/// Schema token of the record.
pub const HISTORY_STATE_SCHEMA: &str = "kin.history-state.v1";
/// Schema token of the derivation progress report.
pub const HISTORY_PROGRESS_SCHEMA: &str = "kin.history-derivation.v1";

const HISTORY_STATE_FILE: &str = "history-state.json";
const HISTORY_PROGRESS_FILE: &str = "history-derivation.json";

/// `.kin/history-state.json` for `layout`.
pub fn record_path(layout: &KinLayout) -> PathBuf {
    layout.root().join(HISTORY_STATE_FILE)
}

/// `.kin/history-derivation.json` for `layout`.
pub fn progress_path(layout: &KinLayout) -> PathBuf {
    layout.root().join(HISTORY_PROGRESS_FILE)
}

/// How a store's Git history is admitted.
///
/// `kin init --history <mode>` names one of these, and a partial store records
/// the mode it was created under so the daemon knows whether to derive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryMode {
    /// Admit the complete reachable history before the store is published.
    Full,
    /// Admit the checked-out commit first and derive the history afterwards,
    /// at low priority, while the store answers.
    Background,
    /// Admit the checked-out commit only and never derive history unless
    /// asked to.
    None,
}

impl HistoryMode {
    /// The word `kin init --history` takes for this mode.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Background => "background",
            Self::None => "none",
        }
    }

    /// Parse the word `kin init --history` takes.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "full" => Some(Self::Full),
            "background" => Some(Self::Background),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// Whether a store created under this mode starts partial.
    pub const fn starts_partial(self) -> bool {
        !matches!(self, Self::Full)
    }
}

impl std::fmt::Display for HistoryMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The record of a store whose history is partial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryStateRecord {
    pub schema: String,
    /// `background` or `none`. Never `full`: a complete store has no record.
    pub mode: HistoryMode,
    /// The Git commit whose tree the snapshot change admitted, as hex.
    pub source_commit: String,
    /// The branch `HEAD` named when the snapshot was taken, or `None` for a
    /// detached `HEAD`.
    pub source_ref: Option<String>,
    /// The snapshot change's semantic identity.
    pub snapshot_change: String,
    /// Parents of `source_commit` that were not admitted, as hex. Empty when
    /// the source commit is a root commit, in which case the admitted history
    /// is the commit's whole first-parent line but other refs are still not
    /// admitted.
    pub boundary_parents: Vec<String>,
    /// Commits admitted: the snapshot, so one.
    pub admitted_commits: u64,
    /// Commits reachable from `HEAD` when the snapshot was taken, when that
    /// could be counted cheaply. The denominator every "how far" answer uses.
    pub reachable_commits: Option<u64>,
    /// Why the history cannot be derived here, when it cannot: a shallow
    /// clone has no ancestors to derive from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underivable: Option<String>,
    pub recorded_at: DateTime<Utc>,
}

impl HistoryStateRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mode: HistoryMode,
        source_commit: String,
        source_ref: Option<String>,
        snapshot_change: String,
        boundary_parents: Vec<String>,
        reachable_commits: Option<u64>,
        underivable: Option<String>,
    ) -> Self {
        Self {
            schema: HISTORY_STATE_SCHEMA.to_string(),
            mode,
            source_commit,
            source_ref,
            snapshot_change,
            boundary_parents,
            admitted_commits: 1,
            reachable_commits,
            underivable,
            recorded_at: Utc::now(),
        }
    }
}

/// Where a background derivation stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivationState {
    /// Owed and not started yet.
    Queued,
    /// Running now.
    Running,
    /// The complete history is admitted in a staged store that has not
    /// replaced this one yet.
    Staged,
    /// The last attempt failed; it is retried later.
    Failed,
    /// Held back, for the reason in `detail`, and not failed.
    Waiting,
}

impl DerivationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Staged => "staged",
            Self::Failed => "failed",
            Self::Waiting => "waiting",
        }
    }
}

/// A report of how far the history derivation got. Never a claim about the
/// store: see the module documentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryDerivationProgress {
    pub schema: String,
    pub state: DerivationState,
    /// The admission step a running derivation is on, as the conversion ladder
    /// names it. For a staged derivation, what the switch to the full history
    /// is waiting for, when a daemon has found it has to wait; see
    /// [`Self::staged_waiting_for`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    /// Commits the derivation is admitting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commits_total: Option<u64>,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

impl HistoryDerivationProgress {
    /// Whether the worker this progress names is still running. A report with
    /// no pid, or on a platform this cannot ask, reads as alive, so a
    /// derivation is never described as stopped on a guess.
    pub fn worker_is_alive(&self) -> bool {
        let Some(pid) = self.pid else {
            return true;
        };
        #[cfg(unix)]
        {
            let Ok(pid) = libc::pid_t::try_from(pid) else {
                return true;
            };
            // SAFETY: signal 0 checks for existence and delivers nothing.
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            alive || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            true
        }
    }

    /// The stage a staged derivation left its complete store in.
    pub fn staged_at(&self) -> Option<PathBuf> {
        (self.state == DerivationState::Staged)
            .then(|| self.detail.as_deref().map(PathBuf::from))
            .flatten()
    }

    /// What the switch to a staged history is waiting for, as a clause that
    /// follows "waiting for". `None` when the history is not staged, or when
    /// nothing is known to hold the switch back.
    pub fn staged_waiting_for(&self) -> Option<&str> {
        (self.state == DerivationState::Staged)
            .then_some(self.phase.as_deref())
            .flatten()
    }

    pub fn new(state: DerivationState) -> Self {
        Self {
            schema: HISTORY_PROGRESS_SCHEMA.to_string(),
            state,
            phase: None,
            step: None,
            steps: None,
            commits_total: None,
            attempts: 0,
            pid: None,
            detail: None,
            started_at: None,
            updated_at: Utc::now(),
        }
    }
}

/// What a store's history is, as every surface should state it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryStanding {
    /// No record: the store holds its complete history, or is native.
    Complete,
    /// The store admitted the checked-out commit only.
    Partial {
        record: HistoryStateRecord,
        progress: Option<HistoryDerivationProgress>,
    },
    /// A record is present and cannot be read. Treated as partial.
    Unreadable { reason: String },
}

impl HistoryStanding {
    /// Whether this store's history may be treated as complete. False for an
    /// unreadable record, which fails closed.
    pub const fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }

    /// The machine word for this standing: `complete`, `partial` or
    /// `unreadable`.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial { .. } => "partial",
            Self::Unreadable { .. } => "unreadable",
        }
    }

    /// The mode the store records, when it records one.
    pub fn mode(&self) -> Option<HistoryMode> {
        match self {
            Self::Partial { record, .. } => Some(record.mode),
            _ => None,
        }
    }

    /// One sentence for a person, or `None` for a complete store.
    ///
    /// Says how much history is admitted and how far the derivation got, and
    /// what to run to finish it. Every history-dependent answer carries it.
    pub fn sentence(&self) -> Option<String> {
        match self {
            Self::Complete => None,
            Self::Unreadable { reason } => Some(format!(
                "History is partial: this store's history record cannot be read ({reason}), so \
                 its history is treated as incomplete. `kin init --history full` derives it."
            )),
            Self::Partial { record, progress } => {
                let commit = short_oid(&record.source_commit);
                let admitted = match record.reachable_commits {
                    Some(total) if total > 1 => format!(
                        "only the checked-out commit {commit} is admitted, 1 of {total} commits"
                    ),
                    _ => format!("only the checked-out commit {commit} is admitted"),
                };
                let derivation = match &record.underivable {
                    Some(reason) => format!("its history cannot be derived here: {reason}"),
                    None => derivation_clause(record.mode, progress.as_ref()),
                };
                Some(format!("History is partial: {admitted}; {derivation}."))
            }
        }
    }
}

/// The machine half of a partial-history qualifier, carried beside every
/// history-dependent answer so a program reads what a person reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialHistoryNotice {
    /// `partial` or `unreadable`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admitted_commits: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reachable_commits: Option<u64>,
    /// Where the derivation stands: `queued`, `running`, `stopped` (a running
    /// worker that is gone), `staged`, `failed` or `waiting`, when one has
    /// reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derivation: Option<String>,
    /// `step/steps` of a running derivation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derivation_step: Option<String>,
    /// The sentence the answer's text carries.
    pub sentence: String,
}

impl HistoryStanding {
    /// The machine-readable qualifier for a history-dependent answer, or
    /// `None` for a complete store.
    pub fn notice(&self) -> Option<PartialHistoryNotice> {
        let sentence = self.sentence()?;
        let mut notice = PartialHistoryNotice {
            state: self.code().to_string(),
            mode: None,
            source_commit: None,
            admitted_commits: None,
            reachable_commits: None,
            derivation: None,
            derivation_step: None,
            sentence,
        };
        if let Self::Partial { record, progress } = self {
            notice.mode = Some(record.mode.as_str().to_string());
            notice.source_commit = Some(record.source_commit.clone());
            notice.admitted_commits = Some(record.admitted_commits);
            notice.reachable_commits = record.reachable_commits;
            if let Some(progress) = progress {
                let state =
                    if progress.state == DerivationState::Running && !progress.worker_is_alive() {
                        "stopped"
                    } else {
                        progress.state.as_str()
                    };
                notice.derivation = Some(state.to_string());
                notice.derivation_step = match (progress.step, progress.steps) {
                    (Some(step), Some(steps)) => Some(format!("{step}/{steps}")),
                    _ => None,
                };
            }
        }
        Some(notice)
    }
}

/// How far the history derivation got, as the second half of the sentence.
fn derivation_clause(mode: HistoryMode, progress: Option<&HistoryDerivationProgress>) -> String {
    let detail = |progress: &HistoryDerivationProgress| {
        progress
            .detail
            .as_deref()
            .map(|detail| format!(" ({detail})"))
            .unwrap_or_default()
    };
    match progress.map(|progress| (progress.state, progress)) {
        Some((DerivationState::Running, progress)) if !progress.worker_is_alive() => {
            "the last background derivation stopped part way and starts again when the \
             repository's daemon next looks"
                .to_string()
        }
        Some((DerivationState::Running, progress)) => {
            match (&progress.phase, progress.step, progress.steps) {
                (Some(phase), Some(step), Some(steps)) => format!(
                    "the full history is deriving in the background (step {step} of {steps}, \
                     {phase})"
                ),
                _ => "the full history is deriving in the background".to_string(),
            }
        }
        Some((DerivationState::Staged, progress)) => match progress.staged_waiting_for() {
            Some(waiting_for) => format!(
                "the full history is derived, and this repository switches to it at its next \
                 quiet point (waiting for {waiting_for})"
            ),
            None => "the full history is derived, and this repository switches to it at its \
                     next quiet point"
                .to_string(),
        },
        Some((DerivationState::Failed, progress)) => format!(
            "the last background derivation failed{} and is retried later",
            detail(progress)
        ),
        Some((DerivationState::Waiting, progress)) => {
            format!("the background derivation is waiting{}", detail(progress))
        }
        _ if mode == HistoryMode::None => {
            "history is not derived in this mode; `kin init --history full` derives it".to_string()
        }
        _ => "the full history derives in the background".to_string(),
    }
}

fn short_oid(hex: &str) -> &str {
    hex.get(..12).unwrap_or(hex)
}

/// Refuse `operation` when it would carry this store's history anywhere as if
/// it were whole.
///
/// Exporting, ejecting, pushing, serving a clone, publishing a prepared state
/// and hosted publication all hand a store's history to something that will
/// read it as the repository's history. A partial store's history is one
/// snapshot, so each of them refuses here, says how far the derivation got,
/// and names the command that completes it. An unreadable record refuses too.
pub fn require_complete_for(layout: &KinLayout, operation: &str) -> Result<(), String> {
    require_complete_standing_for(&standing(layout), operation)
}

/// [`require_complete_for`] over a standing already read.
pub fn require_complete_standing_for(
    standing: &HistoryStanding,
    operation: &str,
) -> Result<(), String> {
    let Some(sentence) = standing.sentence() else {
        return Ok(());
    };
    Err(format!(
        "{operation} refused: {sentence} It would treat that snapshot as this repository's \
         whole history. Run `kin init --history full` to derive and attach the full history, then \
         run {operation} again. Nothing was changed."
    ))
}

/// Qualify one history-dependent answer: append the partial-history sentence
/// to its text and hand back the machine half, for a store whose history is
/// partial. A complete store's answer is left exactly as it was.
pub fn qualify_answer(layout: &KinLayout, lines: &mut Vec<String>) -> Option<PartialHistoryNotice> {
    let notice = standing(layout).notice()?;
    lines.push(notice.sentence.clone());
    Some(notice)
}

/// Read the record and the progress beside it.
pub fn standing(layout: &KinLayout) -> HistoryStanding {
    standing_at(layout.root())
}

/// [`standing`] for a store root that is not wrapped in a layout.
pub fn standing_at(kin_root: &Path) -> HistoryStanding {
    let raw = match crate::store_records::read(kin_root, &kin_root.join(HISTORY_STATE_FILE)) {
        Ok(Some(raw)) => raw,
        Ok(None) => return HistoryStanding::Complete,
        Err(error) => {
            return HistoryStanding::Unreadable {
                reason: error.to_string(),
            }
        }
    };
    let record = match serde_json::from_slice::<HistoryStateRecord>(&raw) {
        Ok(record) if record.schema == HISTORY_STATE_SCHEMA => record,
        Ok(record) => {
            return HistoryStanding::Unreadable {
                reason: format!("schema {} is not {HISTORY_STATE_SCHEMA}", record.schema),
            }
        }
        Err(error) => {
            return HistoryStanding::Unreadable {
                reason: error.to_string(),
            }
        }
    };
    let progress = crate::store_records::read(kin_root, &kin_root.join(HISTORY_PROGRESS_FILE))
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_slice::<HistoryDerivationProgress>(&raw).ok())
        .filter(|progress| progress.schema == HISTORY_PROGRESS_SCHEMA);
    HistoryStanding::Partial { record, progress }
}

/// Write the record into a staged layout, before publication.
pub fn stamp_staged(staged: &KinLayout, record: &HistoryStateRecord) -> std::io::Result<()> {
    write_json_atomically(staged.root(), &record_path(staged), record)
}

/// Rewrite the mode an existing partial store records.
pub fn set_mode(layout: &KinLayout, mode: HistoryMode) -> std::io::Result<Option<HistoryMode>> {
    let HistoryStanding::Partial { mut record, .. } = standing(layout) else {
        return Ok(None);
    };
    if mode == HistoryMode::Full {
        return Err(std::io::Error::other(
            "a partial store cannot record the full mode; derive its history instead",
        ));
    }
    let previous = record.mode;
    record.mode = mode;
    write_json_atomically(layout.root(), &record_path(layout), &record)?;
    Ok(Some(previous))
}

/// Write the derivation progress for `layout`.
pub fn write_progress(
    layout: &KinLayout,
    progress: &HistoryDerivationProgress,
) -> std::io::Result<()> {
    write_json_atomically(layout.root(), &progress_path(layout), progress)
}

/// Record what the switch to a staged history is waiting for, or with `None`
/// that nothing is known to hold it back.
///
/// Changes nothing but that note and the time, and only on a progress that is
/// staged, so it can never turn a running or failed derivation into a staged
/// one. Answers whether it wrote: a note that already says this is left alone,
/// so a daemon that looks every few seconds does not rewrite the record every
/// few seconds.
pub fn note_staged_wait(layout: &KinLayout, waiting_for: Option<&str>) -> std::io::Result<bool> {
    let HistoryStanding::Partial {
        progress: Some(mut progress),
        ..
    } = standing(layout)
    else {
        return Ok(false);
    };
    if progress.state != DerivationState::Staged || progress.phase.as_deref() == waiting_for {
        return Ok(false);
    }
    progress.phase = waiting_for.map(str::to_string);
    progress.updated_at = Utc::now();
    write_progress(layout, &progress)?;
    Ok(true)
}

/// Remove the derivation progress, for a store whose derivation is over.
pub fn clear_progress(layout: &KinLayout) -> std::io::Result<()> {
    crate::store_records::remove(layout.root(), &progress_path(layout)).map(|_| ())
}

/// Staged beside the target and renamed into place after an fsync, so a crash
/// leaves the previous record or the new one and never a torn one.
fn write_json_atomically<T: Serialize>(
    kin_root: &Path,
    path: &Path,
    value: &T,
) -> std::io::Result<()> {
    let body = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    crate::store_records::write(kin_root, path, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_in(dir: &Path) -> KinLayout {
        let root = dir.join(".kin");
        std::fs::create_dir_all(&root).unwrap();
        KinLayout::new(root)
    }

    fn record(mode: HistoryMode) -> HistoryStateRecord {
        HistoryStateRecord::new(
            mode,
            "0123456789abcdef0123456789abcdef01234567".to_string(),
            Some("refs/heads/main".to_string()),
            "change".to_string(),
            vec!["89abcdef0123456789abcdef0123456789abcdef".to_string()],
            Some(6_499),
            None,
        )
    }

    #[test]
    fn an_absent_record_is_a_complete_store() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout_in(dir.path());
        assert_eq!(standing(&layout), HistoryStanding::Complete);
        assert!(standing(&layout).is_complete());
        assert_eq!(standing(&layout).sentence(), None);
    }

    #[test]
    fn a_stamped_record_reads_back_partial_with_its_denominator() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout_in(dir.path());
        stamp_staged(&layout, &record(HistoryMode::Background)).unwrap();
        let read = standing(&layout);
        assert!(!read.is_complete());
        assert_eq!(read.code(), "partial");
        assert_eq!(read.mode(), Some(HistoryMode::Background));
        let sentence = read.sentence().unwrap();
        assert!(sentence.contains("1 of 6499 commits"), "{sentence}");
        assert!(sentence.contains("0123456789ab"), "{sentence}");
    }

    #[test]
    fn an_unreadable_record_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout_in(dir.path());
        std::fs::write(record_path(&layout), b"{not json").unwrap();
        let read = standing(&layout);
        assert!(!read.is_complete());
        assert_eq!(read.code(), "unreadable");
        assert!(read.sentence().unwrap().starts_with("History is partial"));
    }

    #[test]
    fn progress_is_reported_and_never_completes_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout_in(dir.path());
        stamp_staged(&layout, &record(HistoryMode::Background)).unwrap();
        let mut progress = HistoryDerivationProgress::new(DerivationState::Staged);
        progress.attempts = 1;
        write_progress(&layout, &progress).unwrap();
        let read = standing(&layout);
        assert!(!read.is_complete());
        let sentence = read.sentence().unwrap();
        assert!(
            sentence.ends_with("switches to it at its next quiet point."),
            "{sentence}"
        );
        clear_progress(&layout).unwrap();
        clear_progress(&layout).unwrap();
    }

    /// A staged history that has to wait says what it waits for, in the
    /// sentence and to a program, and names its stage only while it is staged.
    #[test]
    fn a_staged_history_that_waits_says_what_for() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout_in(dir.path());
        stamp_staged(&layout, &record(HistoryMode::Background)).unwrap();
        let mut progress = HistoryDerivationProgress::new(DerivationState::Staged);
        progress.detail = Some("/stages/one".to_string());
        assert_eq!(progress.staged_at(), Some(PathBuf::from("/stages/one")));
        assert_eq!(progress.staged_waiting_for(), None);
        progress.phase = Some("an agent's open transaction to be committed".to_string());
        write_progress(&layout, &progress).unwrap();
        let HistoryStanding::Partial {
            progress: Some(read),
            ..
        } = standing(&layout)
        else {
            panic!("the store reads back partial with its progress");
        };
        assert_eq!(
            read.staged_waiting_for(),
            Some("an agent's open transaction to be committed")
        );
        let sentence = standing(&layout).sentence().unwrap();
        assert!(
            sentence.contains("(waiting for an agent's open transaction to be committed)"),
            "{sentence}"
        );

        // The note is written once, cleared once, and never onto a
        // derivation that is not staged.
        assert!(!note_staged_wait(&layout, read.staged_waiting_for()).unwrap());
        assert!(note_staged_wait(&layout, Some("an open merge to be finished")).unwrap());
        assert!(note_staged_wait(&layout, None).unwrap());
        assert!(!note_staged_wait(&layout, None).unwrap());
        let HistoryStanding::Partial {
            progress: Some(cleared),
            ..
        } = standing(&layout)
        else {
            panic!("the store reads back partial with its progress");
        };
        assert_eq!(cleared.staged_waiting_for(), None);
        assert_eq!(cleared.staged_at(), Some(PathBuf::from("/stages/one")));
        write_progress(
            &layout,
            &HistoryDerivationProgress::new(DerivationState::Failed),
        )
        .unwrap();
        assert!(!note_staged_wait(&layout, Some("anything")).unwrap());

        // A running derivation's phase is its admission step, never a wait.
        let mut running = HistoryDerivationProgress::new(DerivationState::Running);
        running.phase = Some("admit history".to_string());
        running.detail = Some("/stages/one".to_string());
        assert_eq!(running.staged_waiting_for(), None);
        assert_eq!(running.staged_at(), None);
    }

    #[test]
    fn the_mode_can_change_but_never_to_full() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout_in(dir.path());
        assert_eq!(set_mode(&layout, HistoryMode::None).unwrap(), None);
        stamp_staged(&layout, &record(HistoryMode::Background)).unwrap();
        assert_eq!(
            set_mode(&layout, HistoryMode::None).unwrap(),
            Some(HistoryMode::Background)
        );
        assert_eq!(standing(&layout).mode(), Some(HistoryMode::None));
        assert!(set_mode(&layout, HistoryMode::Full).is_err());
        assert!(standing(&layout)
            .sentence()
            .unwrap()
            .contains("not derived in this mode"));
    }

    #[test]
    fn only_a_complete_store_may_publish_its_history() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout_in(dir.path());
        assert_eq!(require_complete_for(&layout, "kin push"), Ok(()));
        stamp_staged(&layout, &record(HistoryMode::None)).unwrap();
        let refusal = require_complete_for(&layout, "kin push").unwrap_err();
        assert!(
            refusal.starts_with("kin push refused: History is partial"),
            "{refusal}"
        );
        assert!(refusal.contains("kin init --history full"), "{refusal}");
        std::fs::write(record_path(&layout), b"[]").unwrap();
        assert!(require_complete_for(&layout, "kin push").is_err());
    }

    #[test]
    fn modes_round_trip_their_words() {
        for mode in [
            HistoryMode::Full,
            HistoryMode::Background,
            HistoryMode::None,
        ] {
            assert_eq!(HistoryMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(HistoryMode::parse("partial"), None);
        assert!(!HistoryMode::Full.starts_partial());
        assert!(HistoryMode::None.starts_partial());
    }
}
