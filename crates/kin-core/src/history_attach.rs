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
//! than its snapshot, a ref that moved off the snapshot, a stash or an open
//! merge. What the partial store accumulated on top of the snapshot without
//! committing, its workspace state and the cross-file enrichment a sweep
//! published into it, is re-derived by the next daemon from the working tree
//! and the language servers, exactly as for a newly converted store.
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
    let record = match history_state::standing(layout) {
        HistoryStanding::Complete => return Ok(AttachOutcome::AlreadyComplete),
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
    verify_nothing_would_be_lost(layout, &record)?;

    let journal = AttachJournal {
        schema: ATTACH_JOURNAL_SCHEMA.to_string(),
        phase: AttachPhase::Evacuating,
        stage_parent: stage_parent.to_path_buf(),
    };
    write_journal(layout, &journal)?;
    roll_forward(layout, journal)?;
    Ok(AttachOutcome::Attached)
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
        std::fs::rename(&from, &to).map_err(|error| KinError::io(&from, error))?;
    }
    carry_enrichment_setting(&provisional, root)?;
    sync_directory(root)?;
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
    if ours.repo_id != theirs.repo_id {
        return Err(KinError::Other(format!(
            "the store staged at {} belongs to repository {}, not {}",
            staged.root().display(),
            theirs.repo_id,
            ours.repo_id
        )));
    }
    if !history_state::standing(staged).is_complete() {
        return Err(KinError::Other(format!(
            "the store staged at {} does not hold complete history",
            staged.root().display()
        )));
    }
    Ok(())
}

/// Refuse a swap that would drop anything the partial store holds beyond its
/// snapshot.
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
    if std::fs::symlink_metadata(layout.merge_state_path()).is_ok() {
        return refuse("a merge is open in this store".to_string());
    }
    if std::fs::read_dir(layout.stashes_dir()).is_ok_and(|mut entries| entries.next().is_some()) {
        return refuse("this store holds stashes".to_string());
    }
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
