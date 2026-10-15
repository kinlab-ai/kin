// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Replace a partial-history store with the complete store derived beside it.
//!
//! A partial store holds one native snapshot of the checked-out commit. Its
//! complete history is derived by [`crate::git_init::derive_complete_history_into_stage`]
//! into a staged store beside the repository, under the partial store's own
//! repository identity, by the same ladder and the same proofs that admit any
//! complete store. This module is the last step: it swaps the staged store into
//! place.
//!
//! ## The rules it keeps
//!
//! **Only with no daemon on the store.** The caller holds the repository's
//! runtime authority, which every daemon holds from before it opens any state
//! until it exits. The authority's lock file is `daemon.lock` inside the store
//! directory, so the directory itself is never renamed: its contents are, and
//! `daemon.lock` stays where it is for the whole swap.
//!
//! **Nothing the partial store holds is lost.** The swap refuses a partial
//! store that has anything the staged store cannot stand for: a change other
//! than its snapshot, or a ref that moved off the snapshot. It waits, moving
//! nothing, while the partial store holds work that is still open
//! ([`OpenWork`]): a merge, a stash, an agent's transaction, an unapplied
//! entity draft or a release being recorded. That check is made here, under
//! the runtime authority and with no daemon serving, so it is exact whoever
//! asks for the attach and whatever a daemon believed a moment earlier. What
//! the partial store accumulated on top of the snapshot without committing,
//! its workspace state and the cross-file enrichment a sweep published into
//! it, is re-derived by the next daemon from the working tree and the
//! language servers, exactly as for a newly converted store.
//!
//! **A crash never strands the store.** A journal written before the first move
//! and removed after the last one names the stage, and every attach, including
//! the one a daemon runs as it starts, rolls an interrupted swap forward before
//! doing anything else. Both moves are renames within one filesystem, and each
//! half of the swap is safe to repeat.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{KinError, Result};
use crate::history_state::{self, HistoryStanding};
use crate::layout::KinLayout;
use crate::manifest::KinManifest;

const ATTACH_JOURNAL_SCHEMA: &str = "kin.history-attach.v1";
const ATTACH_JOURNAL_FILE: &str = "history-attach.json";
/// The directory, inside a stage, the partial store's replaced entries move to.
const PROVISIONAL_DIR: &str = "provisional";

/// Entries of a store directory that stay where they are across the swap.
///
/// The journal is what makes the swap resumable. The rest are the host's and
/// the operator's rather than the authority's: remote sync cursors, installed
/// language-server packages, backups and logs. `config.toml` is not among
/// them: the staged store was sealed with its own, derived from the same Git
/// source.
const KEPT_IN_PLACE: &[&str] = &[
    ATTACH_JOURNAL_FILE,
    crate::layout::WORKING_TREE_RECORD,
    "logs",
    "sync_state.json",
    "packages",
    "backups",
];

/// Whether one store entry stays where it is across the swap.
///
/// Every `daemon*` entry does: `daemon.lock` is the runtime authority's own
/// lock, held by the caller for the whole swap, and the rest are the records a
/// daemon starting on this store has already written about itself before it
/// attaches, which must still be where it and `kin daemon stop` look for them.
fn kept_in_place(name: &str) -> bool {
    name.starts_with("daemon") || KEPT_IN_PLACE.contains(&name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AttachPhase {
    /// Moving the partial store's replaced entries out.
    Evacuating,
    /// Moving the staged store's entries in.
    Installing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachJournal {
    schema: String,
    phase: AttachPhase,
    /// The stage directory; the complete store is its `.kin`.
    stage_parent: PathBuf,
}

/// What an attach did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachOutcome {
    /// The staged complete store replaced the partial one.
    Attached,
    /// The store is already complete; nothing was done.
    AlreadyComplete,
    /// Nothing was moved, because the partial store holds work that is still
    /// open and the swap would drop it. The stage stays staged, and the attach
    /// goes ahead once that work is finished.
    Deferred(OpenWork),
}

/// Work a partial store holds open, which an attach waits for.
///
/// Each of these lives in the store's own records and nowhere else, so the
/// swap would drop it, and each is finished by whoever opened it. A refusal is
/// different: a change committed after the snapshot never goes away, so an
/// attach that meets one fails rather than waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenWork {
    /// A merge is open.
    Merge,
    /// The store holds stashed work.
    Stash,
    /// An agent's transaction is open, or one is being recovered.
    Transaction,
    /// An entity draft has not been applied.
    Draft,
    /// A release request is recorded and not settled yet.
    Release,
}

impl OpenWork {
    /// What is open, as a clause every surface can put after "waiting for".
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Merge => "an open merge to be finished",
            Self::Stash => "stashed work to be restored or dropped",
            Self::Transaction => "an agent's open transaction to be committed or abandoned",
            Self::Draft => "an unapplied entity draft to be applied or discarded",
            Self::Release => "a release being recorded to finish",
        }
    }
}

impl std::fmt::Display for OpenWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.describe())
    }
}

/// Whether an attach of the store staged at `stage_parent` would go ahead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachReadiness {
    /// Nothing stands in the way.
    Ready,
    /// The store is already complete.
    AlreadyComplete,
    /// The partial store holds open work the attach waits for.
    Waiting(OpenWork),
}

/// The daemon's durable mirror of its agents' transactions, and the record it
/// leaves while one of their lifecycle writes is being recovered. The daemon
/// writes both and takes their names from here, because the attach has to
/// find them from every process that can run it.
pub const AGENT_TRANSACTIONS_RECORD: &str = "mcp_transactions.json";
pub const AGENT_TRANSACTIONS_RECOVERY_RECORD: &str = "mcp_transactions.lifecycle.json";
/// The directory the daemon keeps entity drafts in, and the lock inside it,
/// which is not a draft. Named here for the same reason.
pub const ENTITY_DRAFTS_DIR: &str = "entity-drafts-v1";
pub const ENTITY_DRAFTS_LOCK: &str = "store.lock";

/// The open work the store at `layout` holds, if it holds any.
///
/// Reads the store's own records, so it answers the same from a serving
/// daemon deciding whether now is a quiet point and from the attach itself.
/// A record that cannot be read counts as open work: the only safe answer to
/// "would the swap drop anything" when the answer is unknown is yes.
pub fn open_work(layout: &KinLayout) -> Option<OpenWork> {
    let root = layout.root();
    let present = |path: &Path| !matches!(crate::store_records::read(root, path), Ok(None));
    if present(&layout.merge_state_path()) {
        return Some(OpenWork::Merge);
    }
    if holds_an_entry(&layout.stashes_dir(), None) {
        return Some(OpenWork::Stash);
    }
    if present(&root.join(AGENT_TRANSACTIONS_RECOVERY_RECORD))
        || holds_an_open_transaction(root, &root.join(AGENT_TRANSACTIONS_RECORD))
    {
        return Some(OpenWork::Transaction);
    }
    if holds_an_entry(&root.join(ENTITY_DRAFTS_DIR), Some(ENTITY_DRAFTS_LOCK)) {
        return Some(OpenWork::Draft);
    }
    if present(&layout.pending_release_path()) {
        return Some(OpenWork::Release);
    }
    None
}

/// Whether `dir` holds any entry other than `ignored`. A directory that is
/// not there holds none; one that cannot be listed is taken to hold one.
fn holds_an_entry(dir: &Path, ignored: Option<&str>) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => entries.any(|entry| match entry {
            Ok(entry) => ignored.is_none_or(|ignored| entry.file_name() != ignored),
            Err(_) => true,
        }),
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Whether the transaction mirror at `path` holds a transaction that is
/// neither committed nor abandoned. A mirror that cannot be read as the map it
/// is counts as holding one.
fn holds_an_open_transaction(root: &Path, path: &Path) -> bool {
    let raw = match crate::store_records::read(root, path) {
        Ok(None) => return false,
        Ok(Some(raw)) => raw,
        Err(_) => return true,
    };
    let Ok(serde_json::Value::Object(transactions)) = serde_json::from_slice(&raw) else {
        return true;
    };
    transactions.values().any(|transaction| {
        !matches!(
            transaction.get("state").and_then(serde_json::Value::as_str),
            Some("committed" | "aborted")
        )
    })
}

/// Where a stage's complete store lives.
pub fn staged_store(stage_parent: &Path) -> PathBuf {
    stage_parent.join(".kin")
}

/// A fresh stage directory for a derivation of the store at `layout`.
///
/// Beside the repository rather than inside it, because the source proofs walk
/// the worktree and a stage inside it would be an untracked directory that
/// changes under them. Named after the store's repository so a reaper can tell
/// its own stages from another repository's in the same parent.
pub fn new_stage_parent(working_dir: &Path, repository_id: &str) -> Result<PathBuf> {
    let root = crate::layout::store_root_for(working_dir);
    let parent = crate::layout::store_staging_parent(working_dir, &root).ok_or_else(|| {
        KinError::Other(format!(
            "repository root has no parent to stage its history beside: {}",
            working_dir.display()
        ))
    })?;
    let stage = parent.join(format!(
        ".kin-history-{}-{}",
        short_identity(repository_id),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir(&stage).map_err(|error| KinError::io(&stage, error))?;
    Ok(stage)
}

fn short_identity(repository_id: &str) -> String {
    repository_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(12)
        .collect()
}

/// Remove this repository's stages other than `keep`.
///
/// A derivation killed part way leaves its stage behind. Only stages named for
/// this repository are considered, and of those only ones whose store, when
/// present, carries this repository's identity, so a stage another repository
/// in the same parent owns is never touched. Answers how many were removed.
pub fn reap_abandoned_stages(
    working_dir: &Path,
    repository_id: &str,
    keep: Option<&Path>,
) -> usize {
    let root = crate::layout::store_root_for(working_dir);
    let Some(parent) = crate::layout::store_staging_parent(working_dir, &root) else {
        return 0;
    };
    let prefix = format!(".kin-history-{}-", short_identity(repository_id));
    let Ok(entries) = std::fs::read_dir(parent) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(&prefix) {
            continue;
        }
        let path = entry.path();
        if keep.is_some_and(|keep| keep == path) {
            continue;
        }
        let manifest = KinManifest::load(&staged_store(&path).join("manifest.json"));
        let ours = match manifest {
            Ok(manifest) => manifest.repo_id == repository_id,
            // A stage the ladder had not published into yet holds no store.
            Err(_) => !staged_store(&path).exists(),
        };
        if ours && std::fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Replace the partial store at `layout` with the complete store staged at
/// `stage_parent`, or finish a replacement a crash interrupted.
///
/// The caller must hold the repository's runtime authority.
pub fn attach_staged_history(layout: &KinLayout, stage_parent: &Path) -> Result<AttachOutcome> {
    if recover_interrupted_attach(layout)? {
        return Ok(AttachOutcome::Attached);
    }
    match attach_readiness(layout, stage_parent)? {
        AttachReadiness::Ready => {}
        AttachReadiness::AlreadyComplete => return Ok(AttachOutcome::AlreadyComplete),
        AttachReadiness::Waiting(work) => return Ok(AttachOutcome::Deferred(work)),
    }

    let journal = AttachJournal {
        schema: ATTACH_JOURNAL_SCHEMA.to_string(),
        phase: AttachPhase::Evacuating,
        stage_parent: stage_parent.to_path_buf(),
    };
    write_journal(layout, &journal)?;
    roll_forward(layout, journal)?;
    Ok(AttachOutcome::Attached)
}

/// Whether attaching the store staged at `stage_parent` would go ahead now,
/// without moving anything.
///
/// The attach asks this itself, with the runtime authority held and no daemon
/// serving, and that answer is the one that decides. A serving daemon asks it
/// earlier, only to avoid leaving its clients for an attach that would wait or
/// be refused anyway. An error is a refusal that waiting does not cure.
pub fn attach_readiness(layout: &KinLayout, stage_parent: &Path) -> Result<AttachReadiness> {
    let record = match history_state::standing(layout) {
        HistoryStanding::Complete => return Ok(AttachReadiness::AlreadyComplete),
        HistoryStanding::Unreadable { reason } => {
            return Err(KinError::Other(format!(
                "the store's history record cannot be read ({reason}), so its history cannot be \
                 replaced safely"
            )))
        }
        HistoryStanding::Partial { record, .. } => record,
    };
    let staged = KinLayout::new(staged_store(stage_parent));
    verify_staged_store(layout, &staged)?;
    if let Some(work) = open_work(layout) {
        return Ok(AttachReadiness::Waiting(work));
    }
    verify_nothing_would_be_lost(layout, &record)?;
    Ok(AttachReadiness::Ready)
}

/// Whether a swap of the store at `layout` was interrupted and still has to
/// be finished. A journal that cannot be read counts: something is there.
///
/// Until it is finished the store may hold neither its old records nor its
/// new ones, its manifest included, so a process that would read them has to
/// ask this first and finish the swap under the runtime authority.
pub fn attach_is_interrupted(layout: &KinLayout) -> bool {
    let root = layout.root();
    !matches!(
        crate::store_records::read(root, &root.join(ATTACH_JOURNAL_FILE)),
        Ok(None)
    )
}

/// Finish a swap a crash interrupted. Answers whether there was one.
///
/// Called ahead of every attach and by a daemon as it starts, with the runtime
/// authority held, so a store whose swap stopped half way is whole again before
/// anything reads it.
pub fn recover_interrupted_attach(layout: &KinLayout) -> Result<bool> {
    let path = layout.root().join(ATTACH_JOURNAL_FILE);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(KinError::io(&path, error)),
    };
    let journal: AttachJournal = serde_json::from_slice(&raw).map_err(|error| {
        KinError::Other(format!(
            "history attach journal {} cannot be read: {error}",
            path.display()
        ))
    })?;
    if journal.schema != ATTACH_JOURNAL_SCHEMA {
        return Err(KinError::Other(format!(
            "history attach journal {} has schema {}, not {ATTACH_JOURNAL_SCHEMA}",
            path.display(),
            journal.schema
        )));
    }
    roll_forward(layout, journal)?;
    Ok(true)
}

/// Stop a swap after a number of its steps, as a crash would, so a test can
/// prove every step is one recovery resumes from. Compiled for tests only:
/// this crate's own, and those of a crate that takes its `test-support`
/// feature from a dev-dependency.
#[cfg(any(test, feature = "test-support"))]
pub mod interruption {
    use std::cell::Cell;

    thread_local! {
        static STEPS_LEFT: Cell<Option<usize>> = const { Cell::new(None) };
    }

    /// Let this thread's swaps take `steps` more steps and then stop, or run
    /// to their end with `None`.
    pub fn after(steps: Option<usize>) {
        STEPS_LEFT.with(|left| left.set(steps));
    }

    pub(super) fn step() -> crate::error::Result<()> {
        STEPS_LEFT.with(|left| match left.get() {
            None => Ok(()),
            Some(0) => Err(crate::error::KinError::Other(
                "history attach interrupted by its test".to_string(),
            )),
            Some(steps) => {
                left.set(Some(steps - 1));
                Ok(())
            }
        })
    }
}

/// One step of a swap is about to change the store. Stops the swap here when a
/// test asked for an interruption, and is nothing otherwise.
fn before_step() -> Result<()> {
    #[cfg(any(test, feature = "test-support"))]
    interruption::step()?;
    Ok(())
}

fn roll_forward(layout: &KinLayout, mut journal: AttachJournal) -> Result<()> {
    let root = layout.root();
    let stage_store = staged_store(&journal.stage_parent);
    let provisional = journal.stage_parent.join(PROVISIONAL_DIR);
    if journal.phase == AttachPhase::Evacuating {
        std::fs::create_dir_all(&provisional).map_err(|error| KinError::io(&provisional, error))?;
        for entry in read_entries(root)? {
            if kept_in_place(&entry) {
                continue;
            }
            let from = root.join(&entry);
            let to = provisional.join(&entry);
            before_step()?;
            if to.exists() {
                // Moved by an earlier attempt that died before the journal
                // advanced, and something recreated the name since. The
                // provisional copy is the partial store's; this one is newer
                // runtime litter, and nothing reads it.
                remove_entry(&from)?;
                continue;
            }
            std::fs::rename(&from, &to).map_err(|error| KinError::io(&from, error))?;
        }
        sync_directory(root)?;
        sync_directory(&provisional)?;
        before_step()?;
        journal.phase = AttachPhase::Installing;
        write_journal(layout, &journal)?;
    }
    for entry in read_entries(&stage_store)? {
        let to = root.join(&entry);
        if to.exists() {
            // Kept in place, or installed by an earlier attempt.
            continue;
        }
        let from = stage_store.join(&entry);
        before_step()?;
        std::fs::rename(&from, &to).map_err(|error| KinError::io(&from, error))?;
    }
    // Not a step of its own: it is repeated by every recovery that reaches
    // it, and writes the same setting each time.
    carry_enrichment_setting(&provisional, root)?;
    sync_directory(root)?;
    before_step()?;
    let journal_path = root.join(ATTACH_JOURNAL_FILE);
    match std::fs::remove_file(&journal_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(KinError::io(&journal_path, error)),
    }
    sync_directory(root)?;
    // The replaced partial store and the emptied stage. A failure here leaves
    // disk behind and no store in doubt, so it is logged rather than raised.
    if let Err(error) = std::fs::remove_dir_all(&journal.stage_parent) {
        tracing::warn!(
            stage = %journal.stage_parent.display(),
            %error,
            "the complete history is attached, but its stage directory could not be removed"
        );
    }
    Ok(())
}

/// Carry the repository's language-server enrichment setting across the swap.
///
/// The staged store's settings are its own, derived from the same Git source,
/// and they replace the partial store's. Its `lsp` section is not derived from
/// Git at all: `kin init --no-enrich` records it as the partial store is
/// created, and `kin daemon sweep --enable` can record it while the history is
/// still deriving behind the store. Installing the staged section as it stands
/// would switch enrichment back on for a repository that switched it off, so
/// the partial store's section is written over the staged one.
///
/// Read from the evacuated copy, so a swap that recovery finishes carries it
/// too, and repeating it after a crash writes the same value again. A side
/// that cannot be read leaves the installed settings as they are: the swap is
/// already complete, and refusing it here would strand a whole store over one
/// section.
fn carry_enrichment_setting(provisional: &Path, root: &Path) -> Result<()> {
    let Ok(partial) = crate::config::KinConfig::load(&provisional.join("config.toml")) else {
        return Ok(());
    };
    let installed_path = root.join("config.toml");
    let Ok(mut installed) = crate::config::KinConfig::load(&installed_path) else {
        return Ok(());
    };
    let unchanged = toml::to_string(&partial.lsp)
        .ok()
        .zip(toml::to_string(&installed.lsp).ok())
        .is_some_and(|(partial, installed)| partial == installed);
    if unchanged {
        return Ok(());
    }
    installed.lsp = partial.lsp;
    installed.save(&installed_path)
}

/// The staged store must be a complete store of this same repository.
fn verify_staged_store(layout: &KinLayout, staged: &KinLayout) -> Result<()> {
    let ours = KinManifest::load(&layout.manifest_path())?;
    let theirs = KinManifest::load(&staged.manifest_path()).map_err(|error| {
        KinError::Other(format!(
            "no complete store is staged at {}: {error}",
            staged.root().display()
        ))
    })?;
    // Neither refusal names where the stage is kept: both are recorded in the
    // store's history progress, which people read.
    if ours.repo_id != theirs.repo_id {
        return Err(KinError::Other(format!(
            "the staged history belongs to repository {}, not {}",
            theirs.repo_id, ours.repo_id
        )));
    }
    if !history_state::standing(staged).is_complete() {
        return Err(KinError::Other(
            "the staged store does not hold complete history".to_string(),
        ));
    }
    Ok(())
}

/// Refuse a swap that would drop anything the partial store committed beyond
/// its snapshot. Work that is still open is [`open_work`]'s to find, and the
/// attach waits for it instead.
fn verify_nothing_would_be_lost(
    layout: &KinLayout,
    record: &history_state::HistoryStateRecord,
) -> Result<()> {
    let refuse = |what: String| {
        Err(KinError::Other(format!(
            "the complete history was derived but not attached: {what}. Attaching would lose \
             it, so this store stays partial; its history keeps answering as partial"
        )))
    };
    let manifest = KinManifest::load(&layout.manifest_path())?;
    let repository_id = kin_model::RepositoryId::new(manifest.repo_id.clone())
        .map_err(|error| KinError::Other(format!("invalid repository identity: {error}")))?;
    let (manager, _) = crate::repository_authority::open_persisted_local_repository_authority(
        repository_id,
        Arc::new(kin_db::LocalFileBackend::new(layout.kindb_dir())),
    )
    .map_err(|error| KinError::Graph(error.to_string()))?;
    let lease = manager.read_authority();
    let changes = lease.snapshot().changes.len();
    if changes != 1 {
        return refuse(format!(
            "{} change(s) were committed in it after its snapshot",
            changes.saturating_sub(1)
        ));
    }
    let snapshot = lease
        .snapshot()
        .changes
        .change_ids()
        .map_err(|error| KinError::Graph(error.to_string()))?
        .first()
        .map(ToString::to_string)
        .unwrap_or_default();
    if snapshot != record.snapshot_change {
        return refuse("its only change is not the snapshot it recorded".to_string());
    }
    for repository_ref in &lease.metadata().ref_state.refs {
        let on_snapshot =
            matches!(
                &repository_ref.target,
                kin_model::RefTarget::Change { change_id } if change_id.to_string() == snapshot
            ) || matches!(repository_ref.target, kin_model::RefTarget::Symbolic { .. });
        if !on_snapshot {
            return refuse(format!(
                "ref {} was moved off the snapshot",
                repository_ref.name
            ));
        }
    }
    Ok(())
}

fn read_entries(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|error| KinError::io(dir, error))? {
        let entry = entry.map_err(|error| KinError::io(dir, error))?;
        let name = entry.file_name().into_string().map_err(|name| {
            KinError::Other(format!(
                "store entry {} is not UTF-8",
                Path::new(&name).display()
            ))
        })?;
        names.push(name);
    }
    names.sort();
    Ok(names)
}

fn remove_entry(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| KinError::io(path, error))?;
    if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
    .map_err(|error| KinError::io(path, error))
}

fn write_journal(layout: &KinLayout, journal: &AttachJournal) -> Result<()> {
    use std::io::Write as _;

    let path = layout.root().join(ATTACH_JOURNAL_FILE);
    let staged = path.with_extension(format!("tmp-{}", std::process::id()));
    let body =
        serde_json::to_vec_pretty(journal).map_err(|error| KinError::Other(error.to_string()))?;
    {
        let mut file =
            std::fs::File::create(&staged).map_err(|error| KinError::io(&staged, error))?;
        file.write_all(&body)
            .and_then(|()| kin_blobs::durable::sync_all(&file))
            .map_err(|error| KinError::io(&staged, error))?;
    }
    std::fs::rename(&staged, &path).map_err(|error| KinError::io(&path, error))?;
    sync_directory(layout.root())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    std::fs::File::open(path)
        .and_then(|directory| kin_blobs::durable::sync_all(&directory))
        .map_err(|error| KinError::io(path, error))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(root: &Path, entries: &[&str]) {
        std::fs::create_dir_all(root).unwrap();
        for entry in entries {
            if entry.ends_with('/') {
                std::fs::create_dir_all(root.join(entry)).unwrap();
            } else {
                std::fs::write(root.join(entry), entry.as_bytes()).unwrap();
            }
        }
    }

    #[test]
    fn a_swap_interrupted_after_evacuation_finishes_on_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo/.kin");
        store(
            &root,
            &[
                "daemon.lock",
                "config.toml",
                "manifest.json",
                "kindb/",
                "history-state.json",
            ],
        );
        std::fs::write(root.join("kindb/partial"), b"partial").unwrap();
        let stage = dir.path().join(".kin-history-x");
        store(
            &staged_store(&stage),
            &["manifest.json", "kindb/", "config.toml", "version"],
        );
        std::fs::write(staged_store(&stage).join("kindb/complete"), b"complete").unwrap();
        std::fs::write(staged_store(&stage).join("config.toml"), b"staged").unwrap();
        let layout = KinLayout::new(root.clone());

        // Evacuate, then stop as a crash would, with the journal advanced.
        let journal = AttachJournal {
            schema: ATTACH_JOURNAL_SCHEMA.to_string(),
            phase: AttachPhase::Evacuating,
            stage_parent: stage.clone(),
        };
        write_journal(&layout, &journal).unwrap();
        std::fs::create_dir_all(stage.join(PROVISIONAL_DIR)).unwrap();
        for entry in [
            "config.toml",
            "manifest.json",
            "kindb",
            "history-state.json",
        ] {
            std::fs::rename(root.join(entry), stage.join(PROVISIONAL_DIR).join(entry)).unwrap();
        }
        write_journal(
            &layout,
            &AttachJournal {
                phase: AttachPhase::Installing,
                ..journal
            },
        )
        .unwrap();

        assert!(recover_interrupted_attach(&layout).unwrap());
        assert!(root.join("kindb/complete").exists());
        assert!(!root.join("kindb/partial").exists());
        assert!(!root.join("history-state.json").exists());
        assert_eq!(std::fs::read(root.join("config.toml")).unwrap(), b"staged");
        assert!(root.join("daemon.lock").exists());
        assert!(root.join("version").exists());
        assert!(!root.join(ATTACH_JOURNAL_FILE).exists());
        assert!(!stage.exists());
        assert!(!recover_interrupted_attach(&layout).unwrap());
    }

    /// A repository that switched enrichment off keeps it off once its full
    /// history attaches, although the staged store carries settings of its
    /// own in which it is on.
    #[test]
    fn the_enrichment_setting_survives_the_swap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo/.kin");
        store(&root, &["daemon.lock", "manifest.json", "kindb/"]);
        std::fs::write(root.join("config.toml"), "[lsp]\nenabled = false\n").unwrap();
        let stage = dir.path().join(".kin-history-z");
        store(&staged_store(&stage), &["manifest.json", "kindb/"]);
        let staged_settings = toml::to_string_pretty(&crate::config::KinConfig::default()).unwrap();
        assert!(
            staged_settings.contains("enabled = true"),
            "{staged_settings}"
        );
        std::fs::write(staged_store(&stage).join("config.toml"), staged_settings).unwrap();
        let layout = KinLayout::new(root.clone());
        let journal = AttachJournal {
            schema: ATTACH_JOURNAL_SCHEMA.to_string(),
            phase: AttachPhase::Evacuating,
            stage_parent: stage.clone(),
        };
        write_journal(&layout, &journal).unwrap();
        roll_forward(&layout, journal).unwrap();

        let installed = crate::config::KinConfig::load(&root.join("config.toml")).unwrap();
        assert!(
            !installed.lsp.enabled,
            "the attached store must keep the repository's enrichment switched off"
        );
        assert!(root.join("daemon.lock").exists());
        assert!(!stage.exists());
    }

    #[test]
    fn a_swap_interrupted_during_evacuation_finishes_on_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo/.kin");
        store(
            &root,
            &[
                "daemon.lock",
                "manifest.json",
                "kindb/",
                "history-state.json",
            ],
        );
        let stage = dir.path().join(".kin-history-y");
        store(&staged_store(&stage), &["manifest.json", "kindb/"]);
        std::fs::write(staged_store(&stage).join("kindb/complete"), b"complete").unwrap();
        let layout = KinLayout::new(root.clone());
        write_journal(
            &layout,
            &AttachJournal {
                schema: ATTACH_JOURNAL_SCHEMA.to_string(),
                phase: AttachPhase::Evacuating,
                stage_parent: stage.clone(),
            },
        )
        .unwrap();
        std::fs::create_dir_all(stage.join(PROVISIONAL_DIR)).unwrap();
        std::fs::rename(
            root.join("kindb"),
            stage.join(PROVISIONAL_DIR).join("kindb"),
        )
        .unwrap();

        assert!(recover_interrupted_attach(&layout).unwrap());
        assert!(root.join("kindb/complete").exists());
        assert!(!root.join("history-state.json").exists());
        assert!(root.join("daemon.lock").exists());
    }

    #[test]
    fn stages_of_other_repositories_are_never_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let store =
            crate::layout::register_external_store(&repo, &dir.path().join("stores/repo")).unwrap();
        let initialized = crate::init(&repo).unwrap();
        assert_eq!(initialized.layout.root(), store);
        let repository_id = initialized.repository_id.as_str();
        let ours = new_stage_parent(&repo, repository_id).unwrap();
        let theirs = new_stage_parent(&repo, "repo-two").unwrap();
        let kept = new_stage_parent(&repo, repository_id).unwrap();
        for stage in [&ours, &theirs, &kept] {
            assert_eq!(stage.parent(), store.parent());
        }
        assert_eq!(reap_abandoned_stages(&repo, repository_id, Some(&kept)), 1);
        assert!(!ours.exists());
        assert!(theirs.exists());
        assert!(kept.exists());
        crate::layout::forget_external_store(&repo).unwrap();
    }
}
