// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Prepared stores this machine holds, found by the identity of what they were
//! admitted from rather than by where they are.
//!
//! Admitting a Git repository derives its whole semantic history, and a daemon
//! then enriches the graph and embeds its entities. A second checkout of the
//! same repository at the same state pays all of that again, although every
//! input is identical. This catalog is how a second `kin init` finds the first
//! store without anyone naming its path.
//!
//! ## Three identities, kept apart
//!
//! - The **source** is [`kin_git::ref_identity::GitRefIdentity`]: the exact raw
//!   refs and `HEAD` an admission captures. Git objects are content-addressed,
//!   so equal sources hold equal reachable history. It is the lookup key.
//! - The **repository and workspace** identities travel with the store.
//! - The **location** is a locator. It is recorded so the store can be found,
//!   and it never decides anything: every candidate is reopened and checked
//!   against the source before it is used, so a lookup is discovery and never
//!   permission to trust what the catalog says.
//!
//! ## What makes a store reusable
//!
//! [`PreparedStore::open`] holds the store's authority frozen and states why a
//! store is not reusable rather than guessing. A store is reusable exactly when
//! it still serves its Git admission and nothing else: one workspace, no open
//! merge, every Kin ref exactly the Git ref it was imported from, a workspace at
//! that `HEAD` with no uncommitted source, and no stash. A store that has moved
//! on since, with a native commit or an admitted edit, is not what an admission
//! of the same checkout would produce, so it is never reused.
//!
//! Whether its derivations are this build's is a separate question, answered by
//! [`crate::hydration_semantics`] against the record the store carries. A store
//! another build derived is reused only through `kin upgrade`.
//!
//! ## The catalog file
//!
//! `prepared-stores.toml` beside the registry, under the registry's own
//! ownership, mode, lock and atomic-publication discipline
//! ([`crate::registry::update_private_toml`]). It lives inside the managed Kin
//! home, so `KIN_HOME` and `KIN_REGISTRY_PATH` bound it exactly as they bound
//! the registry.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::hydration_semantics::HydrationSemanticsRead;
use crate::layout::KinLayout;

/// Schema token of the catalog file.
pub const PREPARED_STORE_CATALOG_SCHEMA: &str = "kin.prepared-store-catalog.v1";

/// The catalog's file name, beside `registry.toml`.
pub const PREPARED_STORE_CATALOG_FILE_NAME: &str = "prepared-stores.toml";

/// The environment variable that turns reuse off: `0` admits every checkout
/// from scratch.
pub const PREPARED_STORE_REUSE_ENV: &str = "KIN_PREPARED_STORE_REUSE";

/// How long a lookup waits for a candidate's authority lock before passing it
/// over.
pub const PREPARED_STORE_OPEN_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// One store the catalog knows, as it was when last recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedStoreRecord {
    pub repository_id: String,
    pub workspace_id: String,
    /// The working tree the store serves. A locator only.
    pub location: PathBuf,
    /// The store directory, when it was recorded: a store kept outside its
    /// working tree is not at `location/.kin`. A locator only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<PathBuf>,
    /// The [`kin_git::ref_identity::GitRefIdentity::digest`] of the Git
    /// authority the store admitted.
    pub source: String,
    /// The commit `HEAD` names, for a person reading the catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Digest of the store's replicated roots when recorded.
    pub replicated_roots: String,
    /// The hydration semantics version the served state was derived under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydration_version: Option<u32>,
    /// The derivation identity the served state was derived under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derivation: Option<String>,
    pub recorded_at: String,
    /// The Kin version that recorded this entry.
    pub recorded_by: String,
    /// The store this one was copied from, when it was reused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reused_from: Option<PathBuf>,
}

impl PreparedStoreRecord {
    /// The store this entry names, when one is still there and still serves
    /// the entry's working tree: the recorded store directory, or the store
    /// discovery finds for the working tree when none was recorded.
    ///
    /// A locator only. [`PreparedStore::open`] still decides whether the store
    /// is reusable.
    pub fn located_store(&self) -> Option<KinLayout> {
        if let Some(store) = &self.store {
            if !store.is_dir() {
                return None;
            }
            let layout = KinLayout::new(store.clone());
            return same_location(layout.working_dir(), &self.location).then_some(layout);
        }
        crate::layout::locate_store(&self.location)
    }
}

/// The catalog file's contents.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PreparedStoreCatalog {
    #[serde(default)]
    pub schema: String,
    #[serde(default)]
    pub stores: Vec<PreparedStoreRecord>,
}

/// `prepared-stores.toml` beside the registry this process is bounded by.
pub fn catalog_path() -> PathBuf {
    crate::registry::registry_path().with_file_name(PREPARED_STORE_CATALOG_FILE_NAME)
}

/// Whether reuse is on for this process: on unless the variable is falsy.
pub fn reuse_enabled() -> bool {
    std::env::var(PREPARED_STORE_REUSE_ENV).map_or(true, |value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

/// Record `record`, replacing whatever the catalog held for its location.
pub fn record(record: PreparedStoreRecord) -> Result<(), String> {
    record_at(&catalog_path(), record)
}

/// [`record`] into an explicit catalog file.
pub fn record_at(path: &Path, record: PreparedStoreRecord) -> Result<(), String> {
    crate::registry::update_private_toml(path, |catalog: &mut PreparedStoreCatalog| {
        let mut record = record;
        catalog.schema = PREPARED_STORE_CATALOG_SCHEMA.to_string();
        catalog.stores.retain(|held| {
            if !same_location(&held.location, &record.location) {
                return true;
            }
            // Where a store came from stays known when it is recorded again.
            if record.reused_from.is_none() {
                record.reused_from = held.reused_from.clone();
            }
            false
        });
        catalog.stores.push(record);
    })
    .map_err(|error| format!("could not update the prepared-store catalog: {error}"))
}

/// Remove the entry for `location`, when there is one.
pub fn forget_at(path: &Path, location: &Path) -> Result<(), String> {
    crate::registry::update_private_toml(path, |catalog: &mut PreparedStoreCatalog| {
        catalog
            .stores
            .retain(|held| !same_location(&held.location, location));
    })
    .map_err(|error| format!("could not update the prepared-store catalog: {error}"))
}

/// Every entry recorded for `source`, newest first.
pub fn candidates_at(path: &Path, source: &str) -> Result<Vec<PreparedStoreRecord>, String> {
    let catalog: PreparedStoreCatalog = crate::registry::load_private_toml(path)
        .map_err(|error| format!("could not read the prepared-store catalog: {error}"))?;
    let mut found = catalog
        .stores
        .into_iter()
        .filter(|record| record.source == source)
        .collect::<Vec<_>>();
    found.sort_by(|left, right| right.recorded_at.cmp(&left.recorded_at));
    Ok(found)
}

fn same_location(left: &Path, right: &Path) -> bool {
    left == right
        || matches!(
            (left.canonicalize(), right.canonicalize()),
            (Ok(left), Ok(right)) if left == right
        )
}

/// Digest of the replicated roots: everything but the receiver-local
/// generation, ref log and local state.
pub fn replicated_roots_digest(roots: &kin_model::RootBundle) -> String {
    use sha2::{Digest, Sha256};
    let replicated = serde_json::json!({
        "version": roots.version,
        "history": roots.history,
        "ref_state": roots.ref_state,
        "collaboration": roots.collaboration,
        "replication": roots.replication,
    });
    let digest: [u8; 32] = Sha256::digest(replicated.to_string().as_bytes()).into();
    format!(
        "sha256:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

/// The source a Git checkout would be admitted from, when it can be reused.
#[derive(Debug, Clone)]
pub struct CheckoutSource {
    pub identity: kin_git::ref_identity::GitRefIdentity,
    pub digest: String,
}

/// The source of the checkout at `working_dir`, or why it cannot be matched
/// against a prepared store.
///
/// A checkout with uncommitted or untracked files is refused: an admission
/// takes the committed state and discloses the rest, so the store it would
/// produce is still the committed one, but the working tree beside it would
/// not match what the reused store's workspace holds. Admitting it from
/// scratch is what discloses that divergence.
pub fn checkout_source(working_dir: &Path) -> Result<CheckoutSource, String> {
    let marker = working_dir.join(".git");
    match std::fs::symlink_metadata(&marker) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err("the Git repository marker is a link".to_string())
        }
        Ok(_) => {}
        Err(_) => return Err("this directory is not a Git checkout".to_string()),
    }
    let identity = kin_git::ref_identity::GitRefIdentity::of_checkout(&kin_io::WorktreeRoot::new(
        working_dir,
        &kin_io::Grant::ambient(),
    ))
    .map_err(|error| format!("the Git refs could not be read: {error}"))?;
    if let Some(dirty) = uncommitted_paths(working_dir)? {
        return Err(format!(
            "the working tree differs from the commit it holds ({dirty} uncommitted or \
             untracked path(s))"
        ));
    }
    let digest = identity.digest();
    Ok(CheckoutSource { identity, digest })
}

/// How many paths `git status` reports, or `None` when the working tree is
/// exactly the commit it holds.
fn uncommitted_paths(working_dir: &Path) -> Result<Option<usize>, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(working_dir)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        // A status that refreshed the index would write inside the checkout.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|error| format!("`git status` could not run: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`git status` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let entries = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .count();
    Ok((entries > 0).then_some(entries))
}

/// A store checked for reuse: what it was admitted from and what it serves.
///
/// Read from the store's persisted authority envelope, which is digest-checked
/// under the repository lock, and from a full read-only open when the envelope
/// cannot answer. Holding nothing afterwards is deliberate: the copy that
/// follows inventories the store twice and proves the copied authority carries
/// exactly [`Self::roots`], so a store that changes in between is refused
/// there, and a whole-history validation is paid once, on the copy, rather
/// than on both sides.
pub struct PreparedStore {
    pub layout: KinLayout,
    pub repository_id: String,
    pub workspace_id: String,
    pub workspace: kin_model::WorkspaceId,
    pub source: kin_git::ref_identity::GitRefIdentity,
    pub roots: kin_model::RootBundle,
    pub hydration: HydrationSemanticsRead,
    /// Tracked files in the workspace tree.
    pub tracked_artifacts: u64,
    /// Files whose language-server enrichment the workspace records finished.
    pub enrichment_marks: usize,
}

impl PreparedStore {
    /// Check the store at `layout`: it must still serve exactly its Git
    /// admission, or this says why it does not.
    pub fn open(layout: &KinLayout, wait: std::time::Duration) -> Result<Self, String> {
        let manifest = crate::manifest::KinManifest::load(&layout.manifest_path())
            .map_err(|error| format!("its manifest could not be read: {error}"))?;
        let binding = crate::LocalRepositoryAuthorityBinding::from_layout(layout)
            .map_err(|error| format!("its authority could not be bound: {error}"))?;
        if layout.merge_state_path().exists() {
            return Err("it holds an open merge".to_string());
        }
        if directory_holds_anything(&layout.stashes_dir()) {
            return Err("it holds a stash".to_string());
        }
        let envelope = binding
            .read_authority_metadata_read_only(wait)
            .map_err(|error| format!("its authority could not be read: {error}"))?;
        let facts = match &envelope {
            Some(envelope) => Facts::of(envelope.metadata(), &binding)?,
            None => {
                let freeze = binding
                    .freeze_existing_read_only(wait)
                    .map_err(|error| format!("its authority could not be read: {error}"))?;
                Facts::of(freeze.authority().metadata(), &binding)?
            }
        };
        Ok(Self {
            layout: layout.clone(),
            repository_id: manifest.repo_id,
            workspace_id: manifest.workspace_id,
            workspace: binding.workspace_id(),
            source: facts.source,
            roots: facts.roots,
            hydration: crate::hydration_semantics::read(layout),
            tracked_artifacts: facts.tracked_artifacts,
            enrichment_marks: facts.enrichment_marks,
        })
    }

    /// The catalog entry that describes this store serving the working tree at
    /// `location`.
    pub fn record(&self, location: &Path, reused_from: Option<PathBuf>) -> PreparedStoreRecord {
        let (hydration_version, derivation) = match &self.hydration {
            HydrationSemanticsRead::Recorded(stamp) => {
                let (version, derivation) = stamp.served();
                (version, derivation.map(str::to_string))
            }
            _ => (None, None),
        };
        PreparedStoreRecord {
            repository_id: self.repository_id.clone(),
            workspace_id: self.workspace_id.clone(),
            location: location.to_path_buf(),
            store: Some(self.layout.root().to_path_buf()),
            source: self.source.digest(),
            head: self.source.head_object().map(str::to_string),
            replicated_roots: replicated_roots_digest(&self.roots),
            hydration_version,
            derivation,
            recorded_at: chrono::Utc::now().to_rfc3339(),
            recorded_by: env!("CARGO_PKG_VERSION").to_string(),
            reused_from,
        }
    }
}

/// What a store's authority says about reuse.
struct Facts {
    source: kin_git::ref_identity::GitRefIdentity,
    roots: kin_model::RootBundle,
    tracked_artifacts: u64,
    enrichment_marks: usize,
}

impl Facts {
    fn of(
        metadata: &kin_db::PersistedRepositoryAuthority,
        binding: &crate::LocalRepositoryAuthorityBinding,
    ) -> Result<Self, String> {
        let workspace = match metadata.workspaces.as_slice() {
            [only] if only.workspace_id == binding.workspace_id() => only,
            workspaces => {
                return Err(format!(
                    "its authority holds {} workspace(s), and a reusable store holds only its own",
                    workspaces.len()
                ))
            }
        };
        if !metadata.merge_transactions.is_empty() {
            return Err("a merge is open in it".to_string());
        }
        let git = metadata
            .git_external_authority
            .as_ref()
            .ok_or_else(|| "it was not admitted from Git".to_string())?;
        refs_serve_git_admission(metadata, git)?;
        workspace_serves_git_head(workspace, git)?;
        Ok(Self {
            source: kin_git::ref_identity::GitRefIdentity::of_authority(git),
            roots: metadata.roots.clone(),
            tracked_artifacts: workspace.tree.artifacts().count() as u64,
            enrichment_marks: workspace.enrichment_marks.len(),
        })
    }
}

/// Record the store at `layout` in the catalog when it serves exactly its Git
/// admission, and say why not otherwise.
///
/// Called when a conversion or a reuse has finished with a store, and when a
/// daemon opens one, so a store an earlier build prepared becomes discoverable
/// the first time this build serves it. Recording is cheap and claims nothing:
/// a lookup checks every candidate again before it copies anything.
pub fn record_store(layout: &KinLayout, reused_from: Option<PathBuf>) -> Result<(), String> {
    let store = PreparedStore::open(layout, PREPARED_STORE_OPEN_WAIT)?;
    let record = store.record(layout.working_dir(), reused_from);
    drop(store);
    self::record(record)
}

fn directory_holds_anything(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_some())
}

/// Every Kin ref is exactly the Git ref it was imported from.
///
/// A Git admission gives every raw ref a Kin ref of the same name: an imported
/// branch names the change its commit was admitted as, through the commit's
/// alias, and a ref the import did not convert names the raw object. A native
/// commit, a new branch or a moved ref leaves some Kin ref naming something
/// else, and then the store is no longer what an admission of the checkout
/// would produce.
fn refs_serve_git_admission(
    metadata: &kin_db::PersistedRepositoryAuthority,
    git: &kin_model::GitExternalAuthority,
) -> Result<(), String> {
    use kin_model::{GitRawTarget, RefTarget};

    let head_ref = match &git.raw_head {
        GitRawTarget::Symbolic { target } => Some(target),
        GitRawTarget::Direct { .. } => None,
    };
    if metadata.ref_state.default_ref.as_ref() != head_ref {
        return Err("its default ref is not the Git HEAD it was admitted with".to_string());
    }
    if metadata.ref_state.refs.len() != git.raw_refs.len() {
        return Err(format!(
            "it holds {} ref(s) and its Git admission {}",
            metadata.ref_state.refs.len(),
            git.raw_refs.len()
        ));
    }
    let aliased = metadata
        .aliases
        .iter()
        .map(|alias| (alias.oid, alias.change_id))
        .collect::<std::collections::HashMap<_, _>>();
    for kin_ref in &metadata.ref_state.refs {
        let raw = git
            .raw_refs
            .iter()
            .find(|raw| raw.name == kin_ref.name)
            .ok_or_else(|| {
                format!(
                    "its ref {} is not a Git ref it was admitted with",
                    kin_ref.name
                )
            })?;
        let imported = match (&kin_ref.target, &raw.target) {
            (RefTarget::Symbolic { target }, GitRawTarget::Symbolic { target: git }) => {
                target == git
            }
            (RefTarget::ExternalObject { object }, GitRawTarget::Direct { object: git }) => {
                object == git
            }
            (RefTarget::Change { change_id }, GitRawTarget::Direct { object }) => {
                aliased.get(&object.oid) == Some(change_id)
            }
            _ => false,
        };
        if !imported {
            return Err(format!(
                "its ref {} has moved since its Git admission",
                kin_ref.name
            ));
        }
    }
    Ok(())
}

/// The workspace sits at the Git `HEAD` with no uncommitted source.
fn workspace_serves_git_head(
    workspace: &kin_model::WorkspaceState,
    git: &kin_model::GitExternalAuthority,
) -> Result<(), String> {
    use kin_model::{GitRawTarget, WorkspaceHead};

    match (&workspace.head, &git.raw_head) {
        (WorkspaceHead::Symbolic { target }, GitRawTarget::Symbolic { target: git })
            if target == git => {}
        (WorkspaceHead::Detached { .. }, GitRawTarget::Direct { .. }) => {}
        _ => return Err("its workspace is not at the Git HEAD it was admitted with".to_string()),
    }
    if workspace.base_tree_hash != Some(workspace.tree_hash) {
        return Err("its workspace holds uncommitted source".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(location: &Path, source: &str, at: &str) -> PreparedStoreRecord {
        PreparedStoreRecord {
            repository_id: "r".to_string(),
            workspace_id: "w".to_string(),
            location: location.to_path_buf(),
            store: None,
            source: source.to_string(),
            head: None,
            replicated_roots: "sha256:00".to_string(),
            hydration_version: Some(32),
            derivation: None,
            recorded_at: at.to_string(),
            recorded_by: "test".to_string(),
            reused_from: None,
        }
    }

    /// The catalog keeps one entry per location, answers by source newest
    /// first, and forgets a location on request.
    #[test]
    fn the_catalog_keys_entries_by_location_and_answers_by_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PREPARED_STORE_CATALOG_FILE_NAME);
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        record_at(&path, record(&a, "s1", "2026-09-28T01:00:00Z")).unwrap();
        record_at(&path, record(&b, "s1", "2026-09-28T02:00:00Z")).unwrap();
        record_at(&path, record(&a, "s1", "2026-09-28T03:00:00Z")).unwrap();
        record_at(
            &path,
            record(&dir.path().join("c"), "s2", "2026-09-28T04:00:00Z"),
        )
        .unwrap();
        let found = candidates_at(&path, "s1").unwrap();
        assert_eq!(
            found.iter().map(|r| r.location.clone()).collect::<Vec<_>>(),
            vec![a.clone(), b.clone()],
            "one entry per location, newest first"
        );
        forget_at(&path, &a).unwrap();
        assert_eq!(candidates_at(&path, "s1").unwrap().len(), 1);
        assert!(candidates_at(&path, "missing").unwrap().is_empty());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains(PREPARED_STORE_CATALOG_SCHEMA), "{raw}");
    }

    /// An absent catalog is an empty one, never an error.
    #[test]
    fn an_absent_catalog_has_no_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("nested")
            .join(PREPARED_STORE_CATALOG_FILE_NAME);
        assert!(candidates_at(&path, "s").unwrap().is_empty());
    }

    #[test]
    fn replicated_roots_digest_ignores_receiver_local_roots() {
        let root =
            |byte: u8| kin_model::AuthorityRoot::new(1, kin_model::Hash256::from_bytes([byte; 32]));
        let mut roots = kin_model::RootBundle {
            version: kin_model::REPOSITORY_ROOT_SCHEMA_VERSION,
            generation: 1,
            history: root(1),
            ref_state: root(2),
            ref_log: root(3),
            collaboration: root(4),
            replication: root(5),
            local_state: root(6),
        };
        let digest = replicated_roots_digest(&roots);
        roots.generation = 9;
        roots.ref_log = root(7);
        roots.local_state = root(8);
        assert_eq!(replicated_roots_digest(&roots), digest);
        roots.history = root(9);
        assert_ne!(replicated_roots_digest(&roots), digest);
    }
}
