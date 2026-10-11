// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! `kin store`: where a working tree's store lives and the stores this machine
//! keeps.
//!
//! A new store lives in the central stores directory under the Kin home, one
//! directory per working tree, and the working tree carries nothing. A store
//! created by an earlier version sits in its tree as `.kin` and keeps working
//! there. `kin init --store <dir>` or `KIN_STORE` puts a store anywhere else.
//!
//! - `kin store path` answers which store serves a directory.
//! - `kin store attach` makes a copied or moved store serve a working tree.
//! - `kin store list` lists every store kept outside its tree and how it
//!   stands: serving its tree, detached from it, or left behind by a tree that
//!   is gone.
//! - `kin store prune` deletes the central stores whose working tree is gone.
//!   It only reports unless `--yes` is given, because a store holds native Kin
//!   history that Git cannot rebuild.
//!
//! These commands read a few small files per store and start nothing.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

/// Schema of the machine-readable answer.
pub const STORE_PATH_SCHEMA: &str = "kin.store-path.v1";

#[derive(Debug, Serialize)]
struct StorePath {
    schema: &'static str,
    /// The working tree the store serves.
    working_dir: PathBuf,
    /// The store directory.
    store: PathBuf,
    /// Whether the store is kept outside the working tree.
    external: bool,
}

/// Print the store serving `path` (default: the current directory), found by
/// walking up exactly as every other command does. Exits through the shared
/// "not a Kin repository" refusal when there is none.
pub fn path(path: Option<PathBuf>, json: bool) -> Result<()> {
    let start = match path {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    let Some(layout) = kin_core::KinLayout::discover(&start) else {
        return Err(crate::commands::not_a_kin_repository());
    };
    let answer = StorePath {
        schema: STORE_PATH_SCHEMA,
        working_dir: layout.working_dir().to_path_buf(),
        store: layout.root().to_path_buf(),
        external: layout.is_external(),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&answer)?);
    } else {
        println!("{}", answer.store.display());
    }
    Ok(())
}

/// Schema of the machine-readable attach answer.
pub const STORE_ATTACH_SCHEMA: &str = "kin.store-attach.v1";

/// Make the store at `store`, kept outside any working tree, serve the working
/// tree at `path` (default: the current directory).
///
/// This is how a copy of a prepared store, such as an APFS clone taken for one
/// task, is given to a checkout of its own, and how a store left behind by a
/// moved checkout is given back to it. Attaching changes where the store is
/// found and which tree it serves; it never changes what the store holds. The
/// daemon that first serves the tree reconciles any difference between the
/// tree and the store's workspace exactly as it does after any stop, and a
/// detached switch moves the store to the commit the checkout is at without
/// parsing anything.
pub fn attach(store: PathBuf, path: Option<PathBuf>, json: bool) -> Result<()> {
    let tree = match path {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    let tree = tree
        .canonicalize()
        .with_context(|| format!("resolve the working tree {}", tree.display()))?;
    let layout = attach_store(&store, &tree)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": STORE_ATTACH_SCHEMA,
                "working_dir": layout.working_dir(),
                "store": layout.root(),
            }))?
        );
    } else {
        println!(
            "{} now serves {}",
            layout.root().display(),
            layout.working_dir().display()
        );
    }
    Ok(())
}

/// Attach the store at `store` to the canonical working tree `tree`, for
/// `kin store attach` and for `kin init` taking back a moved store.
///
/// Refused when the tree already has a store, when `store` is not a store,
/// when it lies inside the tree, when a daemon is still serving it, and when
/// it still serves another tree that finds it through the index: that store
/// is live somewhere, and taking it would leave that tree without one. The
/// copy's daemon runtime files, which name the process that served the store
/// it was copied from, are removed, and so is the index entry of the tree it
/// used to serve when that entry names this store, which a moved store's does
/// and a copy's never does.
pub(crate) fn attach_store(store: &Path, tree: &Path) -> Result<kin_core::KinLayout> {
    if let Some(existing) = kin_core::layout::locate_store(tree) {
        bail!(
            "{} already has a Kin store at {}",
            tree.display(),
            existing.root().display()
        );
    }
    let store = store
        .canonicalize()
        .with_context(|| format!("resolve the store {}", store.display()))?;
    if !store.join("manifest.json").is_file() {
        bail!(
            "{} is not a Kin store: it has no manifest.json",
            store.display()
        );
    }
    let previous = kin_core::layout::read_working_tree_record(&store);
    if let Some(previous) = &previous {
        let still_serves = kin_core::layout::locate_store(previous)
            .is_some_and(|layout| same_path(layout.root(), &store));
        if still_serves && !same_path(previous, tree) {
            bail!(
                "{} still serves {}. Copy it and attach the copy, or remove that tree's \
                 store first",
                store.display(),
                previous.display()
            );
        }
    }
    if kin_core::layout::store_daemon_is_running(&store) {
        bail!(
            "a Kin daemon is still serving {}. Stop it first: {}",
            store.display(),
            kin_core::layout::store_daemon_pid(&store).map_or_else(
                || "end the kin-daemon process that holds it".to_string(),
                |pid| format!("`kill {pid}`")
            )
        );
    }
    for runtime in super::recovery_carrier::RUNTIME_FILES {
        let path = store.join(runtime);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("remove {}", path.display()));
            }
        }
    }
    kin_core::layout::register_external_store(tree, &store).with_context(|| {
        format!(
            "record {} as the store for {}",
            store.display(),
            tree.display()
        )
    })?;
    kin_core::layout::write_working_tree_record(&store, tree)
        .with_context(|| format!("record {} in {}", tree.display(), store.display()))?;
    if let Some(previous) = previous.filter(|previous| !same_path(previous, tree)) {
        kin_core::layout::forget_store_index_entry_naming(&previous, &store)
            .with_context(|| format!("forget the store index entry for {}", previous.display()))?;
    }
    let layout = kin_core::KinLayout::discover(tree)
        .context("the attached store is not found from its working tree")?;
    kin_core::LocalRepositoryAuthorityBinding::from_layout(&layout)
        .with_context(|| format!("bind the repository authority at {}", store.display()))?;
    Ok(layout)
}

/// Schema of `kin store list --json`.
pub const STORE_LIST_SCHEMA: &str = "kin.store-list.v1";

#[derive(Debug, Serialize)]
struct ListedStore {
    store: PathBuf,
    working_dir: PathBuf,
    standing: &'static str,
    central: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
    /// The repository's owner, name and host on its Git remote, when Kin has
    /// a remote recorded for it. Absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
}

#[derive(Debug, Serialize)]
struct StoreList {
    schema: &'static str,
    stores_dir: PathBuf,
    stores: Vec<ListedStore>,
}

fn listed(known: &kin_core::layout::KnownStore, size: bool) -> ListedStore {
    let origin = origin_of(known);
    ListedStore {
        store: known.store.clone(),
        working_dir: known.working_dir.clone(),
        standing: known.standing.id(),
        central: known.central,
        bytes: (size && known.standing != kin_core::layout::StoreStanding::Reserved)
            .then(|| kin_core::layout::store_disk_bytes(&known.store)),
        owner: origin.as_ref().and_then(|origin| origin.owner.clone()),
        name: origin.as_ref().map(|origin| origin.name.clone()),
        host: origin.map(|origin| origin.host),
    }
}

/// Where a listed store's repository lives on its Git remote, read through
/// the one parser the workspace page also uses. A reserved store holds no
/// configuration yet and has none.
fn origin_of(
    known: &kin_core::layout::KnownStore,
) -> Option<kin_core::repository_origin::RepositoryOrigin> {
    if known.standing == kin_core::layout::StoreStanding::Reserved {
        return None;
    }
    let layout = kin_core::KinLayout::external(known.store.clone(), known.working_dir.clone());
    kin_core::repository_origin::repository_origin(&layout)
}

/// List every store kept outside its working tree: each one the store index
/// names and each one in the central stores directory, with how it stands.
///
/// `--size` adds what each occupies on disk, which walks every store and so is
/// not the default.
pub fn list(json: bool, size: bool) -> Result<()> {
    let known = kin_core::layout::known_external_stores();
    let answer = StoreList {
        schema: STORE_LIST_SCHEMA,
        stores_dir: kin_core::layout::stores_dir(),
        stores: known.iter().map(|known| listed(known, size)).collect(),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&answer)?);
        return Ok(());
    }
    if answer.stores.is_empty() {
        println!(
            "No stores kept outside a working tree. Central stores live in {}",
            answer.stores_dir.display()
        );
        return Ok(());
    }
    for store in &answer.stores {
        let size = store
            .bytes
            .map(|bytes| format!("  {}", super::byte_fmt::human_bytes(bytes)))
            .unwrap_or_default();
        println!(
            "{:<11} {}{size}\n            serves {}",
            store.standing,
            store.store.display(),
            store.working_dir.display()
        );
    }
    let orphaned = answer
        .stores
        .iter()
        .filter(|store| store.standing == "gone")
        .count();
    if orphaned > 0 {
        println!(
            "{orphaned} store(s) belong to a working tree that is gone. `kin store prune` lists \
             what it would remove, and `kin store attach <store>` gives one to a checkout."
        );
    }
    Ok(())
}

/// Schema of `kin store prune --json`.
pub const STORE_PRUNE_SCHEMA: &str = "kin.store-prune.v1";

#[derive(Debug, Serialize)]
struct PrunedStore {
    store: PathBuf,
    working_dir: PathBuf,
    standing: &'static str,
    bytes: u64,
    removed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    kept_because: Option<String>,
}

#[derive(Debug, Serialize)]
struct PruneReport {
    schema: &'static str,
    applied: bool,
    stores: Vec<PrunedStore>,
    stale_index_entries: Vec<PathBuf>,
    bytes_reclaimed: u64,
}

/// Delete the central stores whose working tree is gone.
///
/// Without `--yes` this only reports what it would remove. A store holds the
/// native Kin history of its tree, which Git cannot rebuild, so nothing is
/// deleted by default. A tree whose parent directory is missing too counts as
/// unreachable rather than gone, because it may sit on a volume that is not
/// mounted; `--unreachable` includes those. Only stores in the central stores
/// directory are deleted: a store a person placed with `--store` is reported
/// and left. Index entries that can never bind a store are removed with
/// `--yes` as well.
pub fn prune(yes: bool, unreachable: bool, json: bool) -> Result<()> {
    let known = kin_core::layout::known_external_stores();
    let mut stores = Vec::new();
    let mut reclaimed = 0_u64;
    for known in known.iter().filter(|known| {
        known.standing == kin_core::layout::StoreStanding::Gone
            || (unreachable && known.standing == kin_core::layout::StoreStanding::Unreachable)
    }) {
        let mut entry = PrunedStore {
            store: known.store.clone(),
            working_dir: known.working_dir.clone(),
            standing: known.standing.id(),
            bytes: kin_core::layout::store_disk_bytes(&known.store),
            removed: false,
            kept_because: None,
        };
        if !known.central {
            entry.kept_because =
                Some("it is outside the central stores directory; remove it by hand".to_string());
        } else if kin_core::layout::store_daemon_is_running(&known.store) {
            entry.kept_because = Some("a daemon is still serving it".to_string());
        } else if yes {
            match kin_core::layout::remove_central_store(&known.store) {
                Ok(bytes) => {
                    entry.removed = true;
                    entry.bytes = bytes;
                    reclaimed = reclaimed.saturating_add(bytes);
                }
                Err(error) => entry.kept_because = Some(error.to_string()),
            }
        }
        stores.push(entry);
    }
    let stale_index_entries = if yes {
        kin_core::layout::remove_stale_index_entries()
            .context("remove store index entries that bind nothing")?
    } else {
        Vec::new()
    };
    let unreachable_left = if unreachable {
        0
    } else {
        known
            .iter()
            .filter(|known| known.standing == kin_core::layout::StoreStanding::Unreachable)
            .count()
    };
    let report = PruneReport {
        schema: STORE_PRUNE_SCHEMA,
        applied: yes,
        stores,
        stale_index_entries,
        bytes_reclaimed: reclaimed,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if report.stores.is_empty() {
        println!("No store belongs to a working tree that is gone.");
    }
    for store in &report.stores {
        let verb = match (&store.kept_because, store.removed) {
            (Some(_), _) => "kept",
            (None, true) => "removed",
            (None, false) => "would remove",
        };
        println!(
            "{verb} {} ({}), which served {}{}",
            store.store.display(),
            super::byte_fmt::human_bytes(store.bytes),
            store.working_dir.display(),
            store
                .kept_because
                .as_ref()
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default()
        );
    }
    if !yes
        && report
            .stores
            .iter()
            .any(|store| store.kept_because.is_none())
    {
        println!(
            "Nothing was removed. Run `kin store prune --yes` to delete these stores and the \
             native Kin history they hold."
        );
    }
    if yes && report.bytes_reclaimed > 0 {
        println!(
            "Reclaimed {}.",
            super::byte_fmt::human_bytes(report.bytes_reclaimed)
        );
    }
    if unreachable_left > 0 {
        println!(
            "{unreachable_left} store(s) belong to a working tree whose parent directory is \
             missing too, which may be a volume that is not mounted. `kin store prune \
             --unreachable` includes them."
        );
    }
    Ok(())
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(store: PathBuf, working_dir: PathBuf) -> kin_core::layout::KnownStore {
        kin_core::layout::KnownStore {
            store,
            working_dir,
            central: true,
            standing: kin_core::layout::StoreStanding::Serving,
            git_anchor: None,
        }
    }

    /// The Mac app lists repositories from `kin store list --json`, so each
    /// entry names its remote's owner, name and host when Kin recorded one,
    /// and leaves them out when it did not. The fields are additive.
    #[test]
    fn listed_stores_name_the_owner_and_repository_their_remote_records() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("store");
        let tree = temp.path().join("tree");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&tree).unwrap();
        // A store outside its tree carries the record that names the tree.
        kin_core::layout::write_working_tree_record(&store, &tree).unwrap();
        let mut config = kin_core::KinConfig::default();
        config.git.remotes = vec![kin_core::config::GitRemoteTransportConfig {
            name: "origin".into(),
            fetch_urls: vec!["https://gitlab.com/group/sub/repo.git".into()],
            push_urls: Vec::new(),
            fetch_refspecs: Vec::new(),
            push_refspecs: Vec::new(),
        }];
        config.save(&store.join("config.toml")).unwrap();
        let entry =
            serde_json::to_value(listed(&known(store.clone(), tree.clone()), false)).unwrap();
        assert_eq!(entry["owner"], "group/sub");
        assert_eq!(entry["name"], "repo");
        assert_eq!(entry["host"], "gitlab.com");
        assert_eq!(
            entry["standing"], "serving",
            "the existing fields are unchanged"
        );

        // No remote recorded and no Git repository to read: nothing is guessed.
        kin_core::KinConfig::default()
            .save(&store.join("config.toml"))
            .unwrap();
        let entry = serde_json::to_value(listed(&known(store, tree), false)).unwrap();
        for field in ["owner", "name", "host"] {
            assert!(entry.get(field).is_none(), "{field} in {entry}");
        }
    }
}
