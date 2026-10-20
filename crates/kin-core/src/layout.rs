// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::KinError;

/// Current `.kin/` directory schema version.
///
/// Bump this when the layout changes in a way that requires migration.
pub const KIN_LAYOUT_VERSION: u32 = 2;

/// The store directory layout.
///
/// All store paths are computed lazily from a single `root`, the store
/// directory. A new store lives in the central stores directory under the Kin
/// home (see [`stores_dir`] and [`store_root_for`]) and names the working tree
/// it serves in its own [`WORKING_TREE_RECORD`]; the layout carries that path,
/// so nothing that asks for [`Self::working_dir`] has to know where the store
/// lives. A store created by an earlier version, or pinned there on purpose,
/// sits in the working tree as `<working tree>/.kin`, and then the working
/// tree is the root's parent.
#[derive(Debug, Clone)]
pub struct KinLayout {
    root: PathBuf,
    /// The working tree of a store kept outside it. `None` for a store in its
    /// working tree, whose working tree is the root's parent.
    external_working_dir: Option<PathBuf>,
}

impl KinLayout {
    /// Create a layout rooted at the given store directory.
    ///
    /// A store kept outside its working tree is recognized by the record it
    /// carries, so a caller holding only the store's path still gets the
    /// working tree it serves. Any other root is an in-tree `.kin`, and its
    /// working tree is its parent.
    pub fn new(root: PathBuf) -> Self {
        let external_working_dir = read_working_tree_record(&root);
        Self {
            root,
            external_working_dir,
        }
    }

    /// A layout for a store at `root` that serves the working tree at
    /// `working_dir` from outside it. Reads nothing.
    pub fn external(root: PathBuf, working_dir: PathBuf) -> Self {
        Self {
            root,
            external_working_dir: Some(working_dir),
        }
    }

    /// The layout of the store serving exactly the working tree at
    /// `working_dir`, or of the store `kin init` would create for it.
    ///
    /// Never walks up: a caller that holds a working tree asks about that tree.
    /// See [`store_root_for`] for the order a store is looked for in.
    pub fn for_working_dir(working_dir: &Path) -> Self {
        if let Some(layout) = locate_store(working_dir) {
            return layout;
        }
        let root = store_root_for(working_dir);
        if is_in_tree_store_root(working_dir, &root) {
            Self::new(root)
        } else {
            Self::external(root, working_dir.to_path_buf())
        }
    }

    /// Whether this store lives outside the working tree it serves.
    pub fn is_external(&self) -> bool {
        self.external_working_dir.is_some()
    }

    /// Find the store serving the working tree that contains `start`.
    ///
    /// Each directory from `start` upward is asked exactly the question
    /// [`locate_store`] answers, whether a store serves *that* directory as its
    /// working tree: `KIN_STORE` when its store records the directory, the
    /// directory's own in-tree `.kin`, or the store index entry for exactly
    /// that directory. The first answer wins, so running a command in a
    /// subdirectory finds the store of the tree it is in, and a nested tree
    /// with a store of its own finds that one.
    ///
    /// The walk stops at the first repository boundary, a directory carrying a
    /// `.git` file or directory. That directory is a working-tree root: a
    /// clone, a linked worktree or a submodule. When nothing serves it, it has
    /// no store, and nothing above it is consulted or mentioned, because any
    /// store up there belongs to a different repository. That keeps a nested
    /// repository, a worktree checked out under another one, and a checkout
    /// below a stray `.kin` in a home directory from binding some other tree's
    /// graph. `KIN_ALLOW_PARENT_STORE=1` walks past the boundary for a caller
    /// that really means to serve a nested tree from its parent's store.
    ///
    /// A directory inside a store kept outside its tree, such as a session
    /// workspace under the store's `runs/`, finds that store too: the walk
    /// reaches the store directory, and a store directory its own working tree
    /// finds is the answer. An in-tree store's runs sit inside its tree and
    /// were always found that way.
    ///
    /// An in-tree `.kin` counts only when it looks like a store: it carries a
    /// manifest, a layout version, a config or a `kindb` directory. The managed
    /// install root, which holds the cross-repo registry, the binaries, the
    /// shim and the central stores, is never one; see [`is_managed_kin_home`]
    /// for how it is recognized by what it holds rather than by where `$HOME`
    /// points. A bare or registry-only `.kin` is not a store either.
    ///
    /// Repository discovery is always filesystem-rooted. A daemon endpoint is
    /// transport configuration, not evidence that a store exists or belongs to
    /// the endpoint's repository.
    pub fn discover(start: &Path) -> Option<Self> {
        let mut current = start.to_path_buf();
        let explicit = ExplicitStore::from_env();
        let index = StoreIndex::open();
        let cross_boundaries = std::env::var_os("KIN_ALLOW_PARENT_STORE").is_some();
        loop {
            if let Some(layout) = explicit.as_ref().and_then(|store| store.serving(&current)) {
                return Some(layout);
            }
            let in_tree = current.join(".kin");
            if is_in_tree_store(&in_tree) {
                return Some(Self::new(in_tree));
            }
            if let Some(found) = index
                .as_ref()
                .and_then(|index| index.store_serving(&current))
            {
                return Some(found);
            }
            if let Some(found) = store_rooted_at(&current) {
                return Some(found);
            }
            if !cross_boundaries && current.join(".git").exists() {
                return None;
            }
            if !current.pop() {
                return None;
            }
        }
    }

    /// The `.kin/` root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The working tree this store serves: the parent of an in-tree `.kin/`,
    /// or the tree an external store's record names.
    pub fn working_dir(&self) -> &Path {
        if let Some(working_dir) = &self.external_working_dir {
            return working_dir;
        }
        self.root
            .parent()
            .expect(".kin/ always has a parent directory")
    }

    /// `.kin/config.toml`
    pub fn config_path(&self) -> PathBuf {
        self.root().join("config.toml")
    }

    /// `.kin/manifest.json`
    pub fn manifest_path(&self) -> PathBuf {
        self.root().join("manifest.json")
    }

    /// `.kin/kindb/` — KinDB snapshot and index directory.
    pub fn kindb_dir(&self) -> PathBuf {
        self.root().join("kindb")
    }

    /// `.kin/kindb/ingest-cas/` — non-authoritative blob staging for explicit
    /// filesystem reconciliation.
    ///
    /// Reconcile parses exact input bytes from this cache before a repository
    /// transaction copies referenced bodies into repository-owned source CAS.
    /// Runtime history, VFS, and semantic query paths must never treat this
    /// staging directory as committed repository authority.
    pub fn ingest_cas_dir(&self) -> PathBuf {
        self.kindb_dir().join("ingest-cas")
    }

    /// `.kin/kindb/<repository-id>/` — the storage namespace holding one
    /// repository's authority record, deltas, snapshots and source blobs.
    ///
    /// This is where graph truth actually lives, and it is the only rule for
    /// finding it. KinDB's `LocalFileBackend` joins the same component onto its
    /// base path for every artifact it addresses, so a caller that resolves a
    /// namespace by hand and a caller that hands the backend a repository id
    /// must agree by construction rather than by two copies of one convention.
    ///
    /// The repository id comes from `.kin/manifest.json`; see
    /// [`crate::manifest::resolve_repo_id`]. A caller that already holds a
    /// bound repository asks its binding for
    /// [`crate::LocalRepositoryAuthorityBinding::namespace_path`] instead of
    /// re-deriving the id.
    pub fn kindb_namespace_path(&self, repo_id: &str) -> PathBuf {
        kindb_namespace_in(&self.kindb_dir(), repo_id)
    }

    /// `.kin/kindb/graph.kndb` — the anchor the derived sidecars are named
    /// from, and NOT the graph.
    ///
    /// The single-file snapshot this path once addressed is retired: nothing
    /// writes it, and repository-v6 keeps graph truth under
    /// [`Self::kindb_namespace_path`] instead. What still lives at this name is
    /// the derived vector and text sidecar set (`graph.kvec`, `graph.kidx`),
    /// which kin-db addresses by suffixing this path.
    ///
    /// Opening a graph here answers with a valid EMPTY graph rather than an
    /// error, because an absent namespace is legitimately how an uninitialized
    /// one looks from this path's point of view. `kin graph viz` did exactly
    /// that on a 20,298-entity store and drew a blank canvas at exit 0. Resolve
    /// the namespace, not this file, for anything that is an answer.
    pub fn kindb_snapshot_path(&self) -> PathBuf {
        self.kindb_dir().join("graph.kndb")
    }

    /// `.kin/kindb/head-generation` — daemon-published durable authority head.
    ///
    /// This is intentionally separate from `.kin/kindb/generation`, which is
    /// owned by KinDB as the generation of its legacy compatibility projection.
    pub fn kindb_head_generation_path(&self) -> PathBuf {
        self.kindb_dir().join("head-generation")
    }

    /// `.kin/kindb/last-admission` — when a complete exact-tree admission last
    /// succeeded.
    ///
    /// Durable rather than in-process on purpose. The reconcile probes already
    /// record this, but they record it in daemon memory, so a restart erases it
    /// and every freshness surface reports that no admission has ever succeeded.
    /// A store that has not been admitted since March then reads the same as one
    /// admitted a second ago, which is the whole defect: nothing a reader can
    /// consult says how far graph truth has fallen behind the repository.
    ///
    /// Deliberately not written by the generation-publish path. That path also
    /// runs on daemon startup, so stamping it there would refresh the timestamp
    /// on every restart and manufacture exactly the false freshness this marker
    /// exists to prevent. It is written only where an admission actually
    /// succeeded.
    pub fn kindb_last_admission_path(&self) -> PathBuf {
        self.kindb_dir().join("last-admission")
    }

    /// `.kin/kindb/hydration-semantics` — the replay-semantics version this
    /// store was created under.
    ///
    /// Durable rather than in-process because the fact it records is about the
    /// store's creation and not about any daemon's life. A local conversion's
    /// initial historical deltas use one revision of the replay algorithm and no
    /// path re-derives them in place. Native transport can later add history
    /// authored elsewhere, so this records only the creation-time version and
    /// does not claim provenance for everything the store later receives.
    ///
    /// Written into the STAGED layout, before the rename that publishes `.kin`,
    /// so a store and its own creation-time record become visible in the same
    /// instant. An absent record does not establish agreement: the store may
    /// predate this record or the file may have been removed, and admitting a
    /// native transfer pack removes it on purpose because the wire carries no
    /// authoring version for the history it moves. See
    /// [`crate::hydration_semantics`] for the full producer enumeration.
    pub fn kindb_hydration_semantics_path(&self) -> PathBuf {
        self.kindb_dir()
            .join(crate::hydration_semantics::HYDRATION_SEMANTICS_FILE_NAME)
    }

    /// `.kin/kindb/relation-census` — the relation-kind histogram as it stood
    /// when the graph's relations were last settled.
    ///
    /// Durable rather than in-process for the same reason the last-admission
    /// marker beside it is: a census is only interpretable against the census
    /// before it, and an in-memory one dies with the daemon that took it. It is
    /// written where relations actually change, at the end of a completed
    /// enrichment sweep and at the end of a commit, and never on a read path.
    /// Stamping it from `graph status` would make every reading compare against
    /// the reading before it, so a store that lost a whole relation kind would
    /// announce the loss once and then report itself clean forever.
    pub fn kindb_relation_census_path(&self) -> PathBuf {
        self.kindb_dir().join("relation-census")
    }

    /// `.kin/kindb/retained-parse` — the paths whose current bytes did not parse
    /// and whose entities the graph is therefore still answering from an earlier
    /// parse.
    ///
    /// Durable rather than in-process for the same reason the last-admission
    /// marker beside it is: the pass that learns the fact and the command that
    /// has to report it are usually different processes, and a reconcile pass
    /// with nothing to admit returns before it reconciles anything, so a surface
    /// recomputing this per read would report a store whole the moment nothing
    /// changed. Written only by the daemon's reconcile seams, from what they
    /// observed, and read by `kin status`, `kin diff`, `kin commit`,
    /// `kin graph status` and `kin doctor` so the five cannot disagree.
    pub fn kindb_retained_parse_path(&self) -> PathBuf {
        self.kindb_dir().join("retained-parse")
    }

    /// `.kin/kindb/graph.kvec` — persisted vector index aligned with the snapshot.
    pub fn kindb_vector_index_path(&self) -> PathBuf {
        self.kindb_snapshot_path().with_extension("kvec")
    }

    /// `.kin/kindb/embedding-coverage-complete` — daemon-published marker that
    /// this store's embedding coverage has been whole at least once.
    ///
    /// Partial coverage on its own cannot say whether a store is filling for
    /// the first time or lost ground it already held, and only the second is
    /// something to act on. The marker is what separates them.
    pub fn kindb_embedding_coverage_marker_path(&self) -> PathBuf {
        self.kindb_dir().join("embedding-coverage-complete")
    }

    /// `.kin/kindb/text-index/` — Persistent tantivy text index directory.
    pub fn text_index_dir(&self) -> PathBuf {
        self.kindb_dir().join("text-index")
    }

    /// `.kin/kindb/text-index/hosted/<digest>/` — the persistent text index for
    /// one hosted repository's served graph.
    ///
    /// A hosted daemon serves many repositories out of one layout, and a
    /// persistent text index is a directory of segment files under one manifest
    /// that names them. Two graphs pointed at one directory are two writers of
    /// one image: each reclaims the segment files the other's manifest names,
    /// and the load-time reconciliation then refuses an image whose segments
    /// and manifest disagree. So the directory is per repository, and the
    /// repository is what names it.
    ///
    /// The name is a digest and not the repository id, because a repository id
    /// is not a path component. [`kin_model::RepositoryId`] refuses only the
    /// empty string, ids over 255 bytes and control characters, so `../peer`,
    /// `/etc/kin` and `a/b` are all valid ids and all escape or collide as
    /// path components. A digest has none of those shapes: it is a fixed-length
    /// string over `0-9a-f`, one per distinct id byte string, so `Kin` and
    /// `kin` land apart and `a/b` and `a_b` land apart.
    ///
    /// 128 bits of SHA-256, which is a collision bound far beyond a fleet of
    /// repository ids and short enough to read in a directory listing.
    pub fn hosted_text_index_dir(&self, repo_id: &str) -> PathBuf {
        self.text_index_dir()
            .join("hosted")
            .join(hosted_text_index_component(repo_id))
    }

    /// `.kin/stashes/` — Named overlay snapshots.
    pub fn stashes_dir(&self) -> PathBuf {
        self.root().join("stashes")
    }

    /// `.kin/backups/` — Graph snapshot backups.
    pub fn backups_dir(&self) -> PathBuf {
        self.root().join("backups")
    }

    /// `.kin/projections/` — Generated file/doc projections.
    pub fn projections_dir(&self) -> PathBuf {
        self.root().join("projections")
    }

    /// `.kin/docs/` — Living docs (AGENTS.md, ARCHITECTURE.md, etc.).
    pub fn docs_dir(&self) -> PathBuf {
        self.root().join("docs")
    }

    /// `.kin/bench/` — Benchmark traces and reports.
    pub fn bench_dir(&self) -> PathBuf {
        self.root().join("bench")
    }

    /// `.kin/runs/` — Validation run evidence.
    pub fn runs_dir(&self) -> PathBuf {
        self.root().join("runs")
    }

    /// `.kin/logs/` — Daemon logs.
    pub fn logs_dir(&self) -> PathBuf {
        self.root().join("logs")
    }

    /// `.kin/adapters/` — Assistant adapter config.
    pub fn adapters_dir(&self) -> PathBuf {
        self.root().join("adapters")
    }

    /// `.kin/shallow/` — Persisted C2 shallow-syntax metadata.
    pub fn shallow_dir(&self) -> PathBuf {
        self.root().join("shallow")
    }

    /// `.kin/version` — layout schema version marker.
    pub fn version_path(&self) -> PathBuf {
        self.root().join("version")
    }

    /// Read the schema version from the `.kin/version` file.
    ///
    /// Returns version 1 if the file does not exist (pre-versioning repos).
    pub fn read_version(&self) -> Result<u32, KinError> {
        let path = self.version_path();
        if !path.exists() {
            return Ok(1);
        }
        let text = std::fs::read_to_string(&path).map_err(|e| KinError::io(&path, e))?;
        text.trim().parse::<u32>().map_err(|_| {
            KinError::Config(format!("invalid version in {}: {text:?}", path.display()))
        })
    }

    /// Verify that this binary can read the `.kin/` directory.
    ///
    /// Returns an error unless the on-disk version is exactly current.
    ///
    /// Version 2 is the clean-slate repository-authority layout. Older
    /// file/branch-authority layouts must not be served as if they carried v6
    /// refs and workspaces.
    pub fn check_version(&self) -> Result<(), KinError> {
        let current = self.read_version()?;
        if current != KIN_LAYOUT_VERSION {
            return Err(KinError::IncompatibleVersion {
                found: current,
                supported: KIN_LAYOUT_VERSION,
            });
        }
        Ok(())
    }

    /// Write the `.kin/version` marker, stamping the layout at `version`.
    fn write_version(&self, version: u32) -> Result<(), KinError> {
        let path = self.version_path();
        std::fs::write(&path, version.to_string()).map_err(|e| KinError::io(&path, e))
    }

    /// Bring the on-disk `.kin/` layout up to the version this binary writes.
    ///
    /// This is never a silent no-op:
    /// - a newer-than-supported layout is refused with [`KinError::IncompatibleVersion`];
    /// - a pre-versioning repo (no `.kin/version`) is treated as version 1 and
    ///   refused because no compatibility migration can manufacture v6
    ///   repository authority;
    /// - an older layout is walked forward one step at a time via [`Self::migrate_step`],
    ///   which transforms `v -> v+1` on disk or **loudly refuses** when no step is
    ///   registered for that version. An older layout is never accepted as-is.
    ///
    /// When the on-disk version already equals [`KIN_LAYOUT_VERSION`] there is
    /// genuinely nothing to do and `Ok(())` is returned without touching disk.
    pub fn migrate(&self) -> Result<(), KinError> {
        let mut current = self.read_version()?;
        if current > KIN_LAYOUT_VERSION {
            return Err(KinError::IncompatibleVersion {
                found: current,
                supported: KIN_LAYOUT_VERSION,
            });
        }

        while current < KIN_LAYOUT_VERSION {
            self.migrate_step(current)?;
            current += 1;
            self.write_version(current)?;
        }
        Ok(())
    }

    /// Apply the single migration that upgrades the on-disk layout from `from`
    /// to `from + 1`.
    ///
    /// Each future breaking layout change registers its step in the `match`
    /// below (e.g. `1 => self.migrate_v1_to_v2(),`). A version with no
    /// registered step is refused loudly rather than skipped, so an old `.kin/`
    /// can never be served against a binary that does not know how to upgrade
    /// it.
    fn migrate_step(&self, from: u32) -> Result<(), KinError> {
        // Register future migration steps here, lowest version first. Until
        // one exists, every old authority is deliberately refused.
        Err(KinError::Config(format!(
            "no migration path for .kin/ layout version {from} -> {next}: \
             this repository predates a breaking layout change that this kin \
             build cannot auto-upgrade. Re-run `kin init` in a fresh checkout \
             or restore from a backup made with a matching kin version.",
            next = from + 1,
        )))
    }

    /// `.kin/sync_state.json` — persisted sync state per remote.
    pub fn sync_state_path(&self) -> PathBuf {
        self.root().join("sync_state.json")
    }

    /// `.kin/merge_state.json` — persisted merge conflict state.
    ///
    /// Written by `kin merge` when conflicts are detected, read by
    /// `kin conflicts` and `kin resolve`, cleared on resolution or abort.
    pub fn merge_state_path(&self) -> PathBuf {
        self.root().join("merge_state.json")
    }

    /// `.kin/pending-release.json` — exact serialized daemon release request
    /// retained until the daemon reports durable success or definitive
    /// rejection. This lets a later CLI process resume an uncertain timeout or
    /// transport failure without constructing a second release marker.
    pub fn pending_release_path(&self) -> PathBuf {
        self.root().join("pending-release.json")
    }

    /// `.kin/pending-release.lock` — cross-process serialization for release
    /// recovery, policy preflight, and marker construction.
    pub fn pending_release_lock_path(&self) -> PathBuf {
        self.root().join("pending-release.lock")
    }

    /// All directories that must exist inside `.kin/`.
    ///
    pub fn all_dirs(&self) -> Vec<PathBuf> {
        vec![
            self.kindb_dir(),
            self.ingest_cas_dir(),
            self.stashes_dir(),
            self.backups_dir(),
            self.projections_dir(),
            self.docs_dir(),
            self.bench_dir(),
            self.runs_dir(),
            self.logs_dir(),
            self.adapters_dir(),
            self.shallow_dir(),
        ]
    }
}

// ---------------------------------------------------------------------------
// Where a store lives
// ---------------------------------------------------------------------------

/// Names the store directory for the working tree a command runs in, the way
/// `GIT_DIR` names a Git directory.
///
/// `kin init` creates the store there instead of in the central stores
/// directory, and records it in the store index, so every later process finds
/// it from the working tree whether or not it carries this variable. Discovery
/// honours it only for the working tree the store's own record names, so a
/// shell that exports it cannot bind that store to some other repository.
pub const KIN_STORE_ENV: &str = "KIN_STORE";

/// Chooses where `kin init` puts a new store when neither `--store` nor
/// `KIN_STORE` names one: `central` (the default) or `in-tree`.
///
/// `in-tree` keeps the earlier layout, `<working tree>/.kin`, for a setup that
/// needs the store to travel with the checkout, such as a container whose Kin
/// home is thrown away while the repository is a bind mount. Existing stores
/// are found wherever they are, whatever this says.
pub const KIN_STORE_LAYOUT_ENV: &str = "KIN_STORE_LAYOUT";

/// The directory beside the registry that holds central stores, one
/// directory per working tree.
///
/// Like the store index it lives beside `registry.toml`, so `KIN_HOME` and
/// `KIN_REGISTRY_PATH` bound it exactly as they bound the registry.
pub const STORES_DIR_NAME: &str = "stores";

/// Where `kin init` puts a new store when nothing names a location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorePlacement {
    /// In the central stores directory, outside the working tree.
    Central,
    /// In the working tree, as `.kin`.
    InTree,
}

/// The placement `KIN_STORE_LAYOUT` asks for, central unless it says
/// `in-tree`.
pub fn default_store_placement() -> StorePlacement {
    match std::env::var(KIN_STORE_LAYOUT_ENV) {
        Ok(value) if value.trim().eq_ignore_ascii_case("in-tree") => StorePlacement::InTree,
        _ => StorePlacement::Central,
    }
}

/// The file a store kept outside its working tree carries to name that tree.
///
/// An in-tree `.kin` never has one: its working tree is its parent. The record
/// is what lets a process holding only the store's path, such as a daemon
/// handed `--store`, a supervisor reading a worker's arguments, or a copy
/// being checked, answer which tree the store serves without a lookup.
pub const WORKING_TREE_RECORD: &str = "working-tree.json";

const WORKING_TREE_RECORD_SCHEMA: &str = "kin.working-tree.v1";

/// The directory beside the registry that indexes stores kept outside their
/// working trees, one entry per working tree.
///
/// It lives beside `registry.toml`, so `KIN_HOME` and `KIN_REGISTRY_PATH`
/// bound it exactly as they bound the registry and the prepared-store catalog.
/// The working tree carries nothing: no pointer file, no link, no ignore rule,
/// which is what lets a store serve a read-only checkout and keeps it out of
/// that checkout's diff.
pub const STORE_INDEX_DIR_NAME: &str = "store-index";

const STORE_INDEX_ENTRY_SCHEMA: &str = "kin.store-index-entry.v1";

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct WorkingTreeRecord {
    schema: String,
    working_dir: PathBuf,
    /// The Git commit the working tree had checked out when this record was
    /// written, when it is a Git checkout with a commit.
    ///
    /// It is the store's repository identity that survives a move. The path
    /// changes when a checkout is moved or renamed, and its history does not,
    /// so a checkout whose object database holds this commit is the same
    /// repository, and `kin init` there can take the store back instead of
    /// building a second one. Added after the first records were written, so
    /// a record without it still parses and simply cannot be matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    git_anchor: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoreIndexEntry {
    schema: String,
    working_dir: PathBuf,
    store: PathBuf,
}

fn read_working_tree_record_file(root: &Path) -> Option<WorkingTreeRecord> {
    let bytes = std::fs::read(root.join(WORKING_TREE_RECORD)).ok()?;
    let record: WorkingTreeRecord = serde_json::from_slice(&bytes).ok()?;
    (record.schema == WORKING_TREE_RECORD_SCHEMA && record.working_dir.is_absolute())
        .then_some(record)
}

/// The working tree the record in the store at `root` names, or `None` when
/// `root` carries no readable record, which is every in-tree `.kin`.
pub fn read_working_tree_record(root: &Path) -> Option<PathBuf> {
    read_working_tree_record_file(root).map(|record| record.working_dir)
}

/// The Git commit the store at `root` recorded as its repository identity,
/// when its record carries one.
pub fn read_store_git_anchor(root: &Path) -> Option<String> {
    read_working_tree_record_file(root).and_then(|record| record.git_anchor)
}

/// The commit the Git checkout at `working_dir` has checked out, as hex.
///
/// Opened isolated from ambient Git configuration, the way admission opens a
/// source, so a machine's replace refs or includes cannot change the answer.
/// `None` for a directory that is not a Git checkout or has no commit yet.
fn git_head_commit(working_dir: &Path) -> Option<String> {
    if !working_dir.join(".git").exists() {
        return None;
    }
    let options = gix::open::Options::isolated();
    let repo = gix::open_opts(working_dir, options).ok()?;
    let head = repo.head_id().ok()?;
    Some(head.detach().to_string())
}

/// Whether the Git checkout at `working_dir` holds the commit `anchor`, which
/// is what makes it the repository a store recorded.
fn git_checkout_holds(working_dir: &Path, anchor: &str) -> bool {
    if !working_dir.join(".git").exists() {
        return false;
    }
    let Ok(id) = gix::ObjectId::from_hex(anchor.as_bytes()) else {
        return false;
    };
    let options = gix::open::Options::isolated();
    let Ok(repo) = gix::open_opts(working_dir, options) else {
        return false;
    };
    repo.has_object(id)
}

/// Record in the store at `root` that it serves the working tree at
/// `working_dir`, replacing any record it carried.
///
/// The record also carries the commit a Git checkout has out, which is how a
/// store is recognized again after its checkout moves; see
/// [`reattach_candidates`].
///
/// Written through a temporary file and a rename, so a reader sees the old
/// record or the new one and never a torn one.
pub fn write_working_tree_record(root: &Path, working_dir: &Path) -> Result<(), KinError> {
    let working_dir = working_dir
        .canonicalize()
        .map_err(|error| KinError::io(working_dir, error))?;
    let git_anchor = git_head_commit(&working_dir);
    let record = WorkingTreeRecord {
        schema: WORKING_TREE_RECORD_SCHEMA.to_string(),
        working_dir,
        git_anchor,
    };
    let body = serde_json::to_vec_pretty(&record)
        .map_err(|error| KinError::Other(format!("encode the working-tree record: {error}")))?;
    write_file_atomically(&root.join(WORKING_TREE_RECORD), &body)
}

/// Remove the working-tree record from the store at `root`, when it has one.
///
/// A store copied into `<working tree>/.kin` from a store that lived outside
/// its tree would otherwise keep naming the tree it was copied from.
pub fn remove_working_tree_record(root: &Path) -> Result<(), KinError> {
    let path = root.join(WORKING_TREE_RECORD);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(KinError::io(&path, error)),
    }
}

/// Make the store staged at `staged` serve `working_dir` from where it will be
/// published, `final_root`: a record naming the tree when `final_root` is
/// outside it, and none when it is the tree's own `.kin`.
///
/// Called on a staged store before its publishing rename, so a published store
/// and the record saying which tree it serves appear in the same instant.
pub fn stage_store_placement(
    staged: &Path,
    final_root: &Path,
    working_dir: &Path,
) -> Result<(), KinError> {
    if is_in_tree_store_root(working_dir, final_root) {
        remove_working_tree_record(staged)
    } else {
        write_working_tree_record(staged, working_dir)
    }
}

/// The working tree the store at `root` serves: the tree its record names, or
/// its parent for an in-tree `.kin`.
pub fn working_dir_of_store(root: &Path) -> Option<PathBuf> {
    read_working_tree_record(root).or_else(|| root.parent().map(Path::to_path_buf))
}

/// Whether `root` is the in-tree store position of `working_dir`, its `.kin`.
pub fn is_in_tree_store_root(working_dir: &Path, root: &Path) -> bool {
    if root.file_name() != Some(std::ffi::OsStr::new(".kin")) {
        return false;
    }
    match root.parent() {
        Some(parent) => same_dir(parent, working_dir) || parent == working_dir,
        None => false,
    }
}

/// The directory a new store at `final_root` for `working_dir` is staged in
/// before its publishing rename.
///
/// An in-tree store stages beside its working tree, never inside it, so a
/// watcher on the tree never sees a half-built store. A store outside its
/// tree stages beside itself, which keeps the rename on one filesystem.
pub fn store_staging_parent(working_dir: &Path, final_root: &Path) -> Option<PathBuf> {
    if is_in_tree_store_root(working_dir, final_root) {
        working_dir.parent().map(Path::to_path_buf)
    } else {
        final_root.parent().map(Path::to_path_buf)
    }
}

/// The store index directory this process is bounded by.
pub fn store_index_dir() -> PathBuf {
    crate::registry::registry_path().with_file_name(STORE_INDEX_DIR_NAME)
}

/// The central stores directory this process is bounded by: `stores` beside
/// the registry, `~/.kin/stores` by default.
pub fn stores_dir() -> PathBuf {
    crate::registry::registry_path().with_file_name(STORES_DIR_NAME)
}

/// Whether the in-tree position `in_tree` (a working tree's `.kin`) holds a
/// store this process may serve.
///
/// The managed install root carries the registry, the binaries and the
/// central stores and is never a store. Neither is a `.kin` that carries any
/// of the install root's entries and nothing a store holds, such as the stray
/// home directory holding only a registry that a Linux runner once bound as a
/// repository: one install-root entry is not enough for
/// [`is_managed_kin_home`], which errs toward a real repository, but with no
/// manifest, layout version or `kindb` beside it there is no repository to
/// err toward.
fn is_in_tree_store(in_tree: &Path) -> bool {
    in_tree.is_dir() && !is_managed_kin_home(in_tree) && !is_a_kin_home_without_a_store(in_tree)
}

/// Entries only a Kin home carries, the install root's markers and the store
/// directories that sit beside its registry.
const KIN_HOME_ENTRIES: [&str; 6] = [
    "registry.toml",
    "bin",
    "lib",
    "shell",
    STORES_DIR_NAME,
    STORE_INDEX_DIR_NAME,
];

fn is_a_kin_home_without_a_store(dir: &Path) -> bool {
    let home_entry = KIN_HOME_ENTRIES.iter().any(|name| dir.join(name).exists());
    let store_entry = ["manifest.json", "version", "kindb"]
        .iter()
        .any(|name| dir.join(name).exists());
    home_entry && !store_entry
}

/// The store serving exactly the working tree at `working_dir`, never one
/// found by walking up from it.
///
/// Looked for in this order:
///
/// 1. `KIN_STORE`, when the store it names records this working tree;
/// 2. the in-tree store, `<working_dir>/.kin`, when it is a store and not the
///    managed Kin install root;
/// 3. the store index entry for this working tree, when the store it names
///    exists, records this working tree, and is a location a store may have.
///
/// A store is only ever accepted when its own record agrees with the index,
/// so a stale entry, a store moved to serve another tree, or a copy that kept
/// its donor's record, binds nothing.
pub fn locate_store(working_dir: &Path) -> Option<KinLayout> {
    if let Some(layout) = ExplicitStore::from_env().and_then(|store| store.serving(working_dir)) {
        return Some(layout);
    }
    let in_tree = working_dir.join(".kin");
    if is_in_tree_store(&in_tree) {
        return Some(KinLayout::new(in_tree));
    }
    StoreIndex::open()?.store_serving(working_dir)
}

/// The store whose directory is exactly `dir`, when `dir` is a store kept
/// outside its working tree and that tree finds it.
///
/// This is how a command run inside a store, such as an agent launched into a
/// session workspace under the store's `runs/`, finds the repository it
/// belongs to. The tree's own lookup has to name this directory, so a copy,
/// an orphan, or a planted record binds nothing.
fn store_rooted_at(dir: &Path) -> Option<KinLayout> {
    let recorded = read_working_tree_record(dir)?;
    let layout = locate_store(&recorded)?;
    let same = layout.root() == dir
        || layout
            .root()
            .canonicalize()
            .ok()
            .zip(dir.canonicalize().ok())
            .is_some_and(|(left, right)| left == right);
    same.then_some(layout)
}

/// Whether a store serves exactly the working tree at `working_dir`.
pub fn store_exists_for(working_dir: &Path) -> bool {
    locate_store(working_dir).is_some()
}

/// Where the store for the working tree at `working_dir` is, or where `kin
/// init` creates it.
///
/// An existing store wins ([`locate_store`]). Otherwise the store goes where
/// `KIN_STORE` names, when that is not already some other tree's store; then
/// where the store index reserved for this tree, which `kin init` writes
/// before it admits; then where [`KIN_STORE_LAYOUT_ENV`] places it: the
/// working tree's slot in the central stores directory by default, or the
/// tree's own `.kin`. A tree whose central slot cannot be named, because the
/// Kin home is not an absolute path, gets its `.kin`.
///
/// This only answers. [`reserve_store_root`] answers the same and records a
/// central location in the store index, which is what a command that is about
/// to create the store calls.
pub fn store_root_for(working_dir: &Path) -> PathBuf {
    if let Some(layout) = locate_store(working_dir) {
        return layout.root().to_path_buf();
    }
    if let Some(explicit) = ExplicitStore::from_env() {
        if explicit.is_unclaimed() {
            return explicit.root;
        }
    }
    if let Some(reserved) =
        StoreIndex::open().and_then(|index| index.reserved(working_dir).ok().flatten())
    {
        return reserved;
    }
    default_store_root(working_dir)
}

/// The store root [`default_store_placement`] gives a tree with no store and
/// no reservation.
fn default_store_root(working_dir: &Path) -> PathBuf {
    let in_tree = working_dir.join(".kin");
    match default_store_placement() {
        StorePlacement::InTree => in_tree,
        StorePlacement::Central => central_store_root(working_dir).unwrap_or(in_tree),
    }
}

/// The location in the central stores directory for the working tree at
/// `working_dir`, or `None` when the stores directory is not an absolute path.
///
/// The name is the tree's directory name, made safe for a path component, and
/// 64 bits of the SHA-256 of its canonical path: readable in a listing, and
/// one per working tree. It is not the repository id. That id is minted inside
/// the admission that publishes into this location, so it does not exist yet
/// when the location is chosen; it can be adopted from another store, so two
/// checkouts of one repository may share it; and it is not guaranteed to be a
/// path component at all.
///
/// A slot already holding something that is not this tree's store, such as
/// the store of a tree that used to live at this path and moved away without
/// it, is passed over for the next free suffix rather than reused, so a new
/// store never lands on another one.
pub fn central_store_root(working_dir: &Path) -> Option<PathBuf> {
    let stores = stores_dir();
    if !stores.is_absolute() {
        return None;
    }
    // Spelled the way the store index records it, so this and a reservation
    // name the same location byte for byte.
    let stores = absolute_store_path(&stores).unwrap_or(stores);
    let canonical = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());
    let base = format!("{}-{}", store_label(&canonical), hex_prefix(&canonical, 8));
    for attempt in 1..=64u32 {
        let name = if attempt == 1 {
            base.clone()
        } else {
            format!("{base}-{attempt}")
        };
        let candidate = stores.join(name);
        match std::fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(candidate),
            Ok(metadata)
                if metadata.is_dir()
                    && read_working_tree_record(&candidate).as_deref()
                        == Some(canonical.as_path()) =>
            {
                return Some(candidate);
            }
            _ => continue,
        }
    }
    None
}

/// A working tree's directory name made into a safe, visible path component.
fn store_label(canonical_working_dir: &Path) -> String {
    let leaf = canonical_working_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let label: String = leaf
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let label = label.trim_matches(|c| c == '.' || c == '-');
    let label: String = label.chars().take(48).collect();
    if label.is_empty() {
        "repository".to_string()
    } else {
        label
    }
}

fn hex_prefix(path: &Path, bytes: usize) -> String {
    let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
    let mut text = String::with_capacity(bytes * 2);
    for byte in &digest[..bytes] {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// [`store_root_for`], and when that is a location outside the working tree
/// that the store index does not hold yet, record it there before anything is
/// created.
///
/// Every command that creates a store calls this first, so the store is found
/// from its working tree from the instant its publishing rename lands, and a
/// crash between the two cannot leave a store nothing finds. A reservation
/// whose store never appears binds nothing.
///
/// The default central location falls back to the tree's own `.kin` when it
/// cannot be used, for example because the Kin home is read-only or lies
/// inside the working tree, so a missing or unwritable home never stops a
/// repository from being initialized. A location `KIN_STORE` names is never
/// replaced that way: a refusal there is the caller's to see.
pub fn reserve_store_root(working_dir: &Path) -> Result<PathBuf, KinError> {
    if let Some(layout) = locate_store(working_dir) {
        return Ok(layout.root().to_path_buf());
    }
    if let Some(explicit) = ExplicitStore::from_env() {
        if explicit.is_unclaimed() {
            return register_external_store(working_dir, &explicit.root);
        }
    }
    if let Some(index) = StoreIndex::open() {
        if let Some(reserved) = index.reserved(working_dir)? {
            return Ok(reserved);
        }
    }
    let canonical = working_dir
        .canonicalize()
        .map_err(|error| KinError::io(working_dir, error))?;
    let root = default_store_root(&canonical);
    if is_in_tree_store_root(&canonical, &root) {
        return Ok(root);
    }
    match register_external_store(&canonical, &root) {
        Ok(root) => Ok(root),
        Err(error) => {
            tracing::warn!(
                working_dir = %canonical.display(),
                central = %root.display(),
                error = %error,
                "the central store location cannot be used; keeping the store in the working tree"
            );
            Ok(canonical.join(".kin"))
        }
    }
}

/// Refuse a location that a store kept outside `working_dir` must not have.
///
/// Both paths are absolute and normalized. Refused: the tree's own `.kin`
/// (that is the in-tree store position and needs no entry), anywhere else
/// inside the tree (the tree's watcher and every admission would read the
/// store as source), a directory containing the tree, the managed Kin install
/// root or a directory containing it, the central stores directory itself,
/// and the store index. The storage capability KinDB pins at `<store>/kindb`
/// assumes a store directory that belongs to one repository and nothing
/// else, and each of these would break that.
fn validate_store_location(working_dir: &Path, store_root: &Path) -> Result<(), KinError> {
    if is_in_tree_store_root(working_dir, store_root) {
        return Err(KinError::Other(format!(
            "{} is this working tree's own store position, not a location outside it",
            store_root.display()
        )));
    }
    if store_root.starts_with(working_dir) || working_dir.starts_with(store_root) {
        return Err(KinError::Other(format!(
            "a store kept outside its working tree cannot be inside it or contain it: {} and {} \
             overlap. Choose a directory outside {}",
            store_root.display(),
            working_dir.display(),
            working_dir.display()
        )));
    }
    let home = crate::registry::managed_kin_home();
    let home = home.canonicalize().unwrap_or(home);
    if is_managed_kin_home(store_root) || home.starts_with(store_root) {
        return Err(KinError::Other(format!(
            "{} is the Kin installation, not a store location",
            store_root.display()
        )));
    }
    for (reserved, what) in [
        (stores_dir(), "the directory that holds central stores"),
        (store_index_dir(), "the store index"),
    ] {
        let reserved = absolute_store_path(&reserved).unwrap_or(reserved);
        if reserved.starts_with(store_root)
            || (store_root.starts_with(&reserved) && what == "the store index")
        {
            return Err(KinError::Other(format!(
                "{} is {what}, not a store location",
                store_root.display()
            )));
        }
    }
    Ok(())
}

/// Create the central stores directory, private to this user, and keep
/// Spotlight out of it.
///
/// Private because a store holds a copy of every source body it admitted.
/// Spotlight is kept out because every write the daemon makes would
/// otherwise be indexed: `.metadata_never_index` is what macOS reads, and the
/// marker is harmless elsewhere.
fn ensure_stores_dir(stores: &Path) -> Result<(), KinError> {
    if !stores.is_dir() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder
                .create(stores)
                .map_err(|error| KinError::io(stores, error))?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(stores).map_err(|error| KinError::io(stores, error))?;
    }
    #[cfg(target_os = "macos")]
    {
        let marker = stores.join(".metadata_never_index");
        if !marker.exists() {
            // A marker that cannot be written costs indexing, never the store.
            let _ = std::fs::write(&marker, b"");
        }
    }
    Ok(())
}

/// Record that the store for the working tree at `working_dir` lives at
/// `store_root`, outside that tree.
///
/// This is the index entry discovery reads. It may be written before the
/// store exists, which is how `kin init` reserves the location its admission
/// then publishes to; discovery ignores an entry until the store it names
/// exists and records the same tree.
///
/// Refused for every location [`validate_store_location`] refuses. A location
/// inside the central stores directory makes sure that directory exists,
/// private and kept out of Spotlight.
pub fn register_external_store(working_dir: &Path, store_root: &Path) -> Result<PathBuf, KinError> {
    let working_dir = working_dir
        .canonicalize()
        .map_err(|error| KinError::io(working_dir, error))?;
    let store_root = absolute_store_path(store_root)?;
    if is_in_tree_store_root(&working_dir, &store_root) {
        return Err(KinError::Other(format!(
            "{} is this working tree's own store position; omit --store to keep the store there",
            store_root.display()
        )));
    }
    validate_store_location(&working_dir, &store_root)?;
    // The admission stages beside the store and publishes it with one rename
    // into this directory, so the reservation makes sure it exists.
    if let Some(parent) = store_root.parent() {
        let stores = stores_dir();
        if parent == absolute_store_path(&stores).unwrap_or(stores) {
            ensure_stores_dir(parent)?;
        } else {
            std::fs::create_dir_all(parent).map_err(|error| KinError::io(parent, error))?;
        }
    }
    write_store_index_entry(&working_dir, &store_root)?;
    Ok(store_root)
}

fn write_store_index_entry(
    canonical_working_dir: &Path,
    store_root: &Path,
) -> Result<(), KinError> {
    let index = store_index_dir();
    crate::registry::create_kin_dir_all(&index).map_err(|error| KinError::io(&index, error))?;
    let entry = StoreIndexEntry {
        schema: STORE_INDEX_ENTRY_SCHEMA.to_string(),
        working_dir: canonical_working_dir.to_path_buf(),
        store: store_root.to_path_buf(),
    };
    let body = serde_json::to_vec_pretty(&entry)
        .map_err(|error| KinError::Other(format!("encode the store index entry: {error}")))?;
    write_file_atomically(
        &index.join(store_index_file_name(canonical_working_dir)),
        &body,
    )
}

/// Remove the store index entry for the working tree at `working_dir`, when
/// there is one. The store itself is not touched.
pub fn forget_external_store(working_dir: &Path) -> Result<(), KinError> {
    let Ok(working_dir) = working_dir.canonicalize() else {
        return Ok(());
    };
    forget_store_index_entry(&working_dir)
}

/// Remove the store index entry recorded for `recorded_working_dir`, spelled
/// exactly as the entry records it.
///
/// [`forget_external_store`] canonicalizes first, which a working tree that no
/// longer exists cannot do, so the entry for a moved or deleted tree is
/// removed through this instead.
pub fn forget_store_index_entry(recorded_working_dir: &Path) -> Result<(), KinError> {
    let path = store_index_dir().join(store_index_file_name(recorded_working_dir));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(KinError::io(&path, error)),
    }
}

/// Remove the store index entry recorded for `recorded_working_dir` when, and
/// only when, it names `store`. Answers whether an entry was removed.
///
/// A store that moved or was attached elsewhere leaves the entry of the tree it
/// used to serve naming it. A copy taken from a store that still serves its
/// tree carries that tree in its record too, and that tree's entry names the
/// original, which must stay.
pub fn forget_store_index_entry_naming(
    recorded_working_dir: &Path,
    store: &Path,
) -> Result<bool, KinError> {
    let Some(index) = StoreIndex::open() else {
        return Ok(false);
    };
    let names_store = index
        .entries()
        .into_iter()
        .find(|(tree, _)| tree == recorded_working_dir)
        .is_some_and(|(_, indexed)| {
            indexed == store
                || indexed
                    .canonicalize()
                    .ok()
                    .zip(store.canonicalize().ok())
                    .is_some_and(|(left, right)| left == right)
        });
    if names_store {
        forget_store_index_entry(recorded_working_dir)?;
    }
    Ok(names_store)
}

/// An absolute, normalized spelling of a store path that may not exist yet:
/// its nearest existing ancestor canonicalized, with the rest appended.
fn absolute_store_path(path: &Path) -> Result<PathBuf, KinError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| KinError::io(path, error))?
            .join(path)
    };
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            let mut resolved = canonical;
            for component in rest.iter().rev() {
                resolved.push(component);
            }
            return Ok(resolved);
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return Ok(absolute),
        }
    }
}

fn store_index_file_name(canonical_working_dir: &Path) -> String {
    let mut name = hex_prefix(canonical_working_dir, 16);
    name.push_str(".json");
    name
}

fn write_file_atomically(path: &Path, body: &[u8]) -> Result<(), KinError> {
    let parent = path
        .parent()
        .ok_or_else(|| KinError::Other(format!("{} has no parent directory", path.display())))?;
    let mut staged =
        tempfile::NamedTempFile::new_in(parent).map_err(|error| KinError::io(parent, error))?;
    std::io::Write::write_all(&mut staged, body).map_err(|error| KinError::io(path, error))?;
    kin_blobs::durable::sync_all(staged.as_file()).map_err(|error| KinError::io(path, error))?;
    staged
        .persist(path)
        .map_err(|error| KinError::io(path, error.error))?;
    Ok(())
}

/// The store index as this process sees it, when it exists at all.
struct StoreIndex {
    dir: PathBuf,
}

impl StoreIndex {
    fn open() -> Option<Self> {
        let dir = store_index_dir();
        dir.is_dir().then_some(Self { dir })
    }

    fn entry(&self, working_dir: &Path) -> Option<(PathBuf, StoreIndexEntry)> {
        let canonical = working_dir.canonicalize().ok()?;
        let bytes = std::fs::read(self.dir.join(store_index_file_name(&canonical))).ok()?;
        let entry: StoreIndexEntry = serde_json::from_slice(&bytes).ok()?;
        (entry.schema == STORE_INDEX_ENTRY_SCHEMA && entry.working_dir == canonical)
            .then_some((canonical, entry))
    }

    /// The valid location the entry for `working_dir` names, whether or not a
    /// store is there yet. A recognized entry with an unsafe location refuses
    /// creation before an admission or a prepared-state restore writes there.
    fn reserved(&self, working_dir: &Path) -> Result<Option<PathBuf>, KinError> {
        let Some((canonical, entry)) = self.entry(working_dir) else {
            return Ok(None);
        };
        if !entry.store.is_absolute()
            || entry
                .store
                .components()
                .any(|component| component == std::path::Component::ParentDir)
        {
            return Err(KinError::Other(format!(
                "the indexed store location is not an absolute normalized path: {}",
                entry.store.display()
            )));
        }
        let store = absolute_store_path(&entry.store)?;
        validate_store_location(&canonical, &store)?;
        Ok(Some(store))
    }

    /// The existing store the entry for `working_dir` names, when that store
    /// records the same tree and sits where a store may sit. An entry naming
    /// a location the index would refuse to write, however it got there, binds
    /// nothing.
    fn store_serving(&self, working_dir: &Path) -> Option<KinLayout> {
        // Discovery and creation must validate the same resolved location.
        // Testing the index's raw spelling would let an external symlink or
        // a parent component hide a location inside the working tree.
        let store = self.reserved(working_dir).ok()??;
        let canonical = working_dir.canonicalize().ok()?;
        let recorded = read_working_tree_record(&store)?;
        (recorded == canonical).then(|| KinLayout::external(store, working_dir.to_path_buf()))
    }

    /// Every readable entry, as the working tree it names and its store.
    fn entries(&self) -> Vec<(PathBuf, PathBuf)> {
        let Ok(listing) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut entries = Vec::new();
        for item in listing.flatten() {
            let path = item.path();
            if path.extension() != Some(std::ffi::OsStr::new("json")) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(entry) = serde_json::from_slice::<StoreIndexEntry>(&bytes) else {
                continue;
            };
            if entry.schema != STORE_INDEX_ENTRY_SCHEMA
                || path.file_name().and_then(|name| name.to_str())
                    != Some(store_index_file_name(&entry.working_dir).as_str())
            {
                continue;
            }
            entries.push((entry.working_dir, entry.store));
        }
        entries.sort();
        entries
    }
}

/// The store `KIN_STORE` names, and the tree its record names if it has one.
struct ExplicitStore {
    root: PathBuf,
    working_dir: Option<PathBuf>,
}

impl ExplicitStore {
    fn from_env() -> Option<Self> {
        let raw = std::env::var_os(KIN_STORE_ENV).filter(|value| !value.is_empty())?;
        let root = absolute_store_path(Path::new(&raw)).ok()?;
        let working_dir = read_working_tree_record(&root);
        Some(Self { root, working_dir })
    }

    fn serving(&self, working_dir: &Path) -> Option<KinLayout> {
        let recorded = self.working_dir.as_ref()?;
        let canonical = working_dir.canonicalize().ok()?;
        (&canonical == recorded && validate_store_location(&canonical, &self.root).is_ok())
            .then(|| KinLayout::external(self.root.clone(), working_dir.to_path_buf()))
    }

    /// Whether the named location is free for a new store: absent, or an empty
    /// directory. A store already there belongs to the tree its record names.
    fn is_unclaimed(&self) -> bool {
        match std::fs::read_dir(&self.root) {
            Ok(mut entries) => entries.next().is_none(),
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        }
    }
}

// ---------------------------------------------------------------------------
// Every store this machine knows, and the ones nothing serves any more
// ---------------------------------------------------------------------------

/// How a store kept outside its working tree stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoreStanding {
    /// Its working tree exists and finds it.
    Serving,
    /// Its working tree exists and finds another store or none, so nothing
    /// reaches this one. `kin store attach` gives it back to a tree.
    Detached,
    /// Its working tree is gone and the directory that held it is still
    /// there, so the tree was moved or deleted.
    Gone,
    /// Neither its working tree nor the directory that held it exists. The
    /// tree may be on a volume that is not mounted, so this is never treated
    /// as gone without being asked.
    Unreachable,
    /// The store index reserves the location, and no store is there.
    Reserved,
}

impl StoreStanding {
    /// The machine-readable name, as `kin store list --json` prints it.
    pub fn id(self) -> &'static str {
        match self {
            Self::Serving => "serving",
            Self::Detached => "detached",
            Self::Gone => "gone",
            Self::Unreachable => "unreachable",
            Self::Reserved => "reserved",
        }
    }

    /// Whether no working tree reaches the store because its tree is not
    /// where the store recorded it.
    pub fn is_orphaned(self) -> bool {
        matches!(self, Self::Gone | Self::Unreachable)
    }
}

/// One store kept outside its working tree, as the store index and the
/// central stores directory know it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownStore {
    /// The store directory.
    pub store: PathBuf,
    /// The working tree the store's own record names, or the index entry's
    /// when the store has no readable record.
    pub working_dir: PathBuf,
    /// Whether the store lives in the central stores directory, which is the
    /// only place `kin store prune` deletes from.
    pub central: bool,
    /// How it stands.
    pub standing: StoreStanding,
    /// The Git commit its record carries as its repository identity.
    pub git_anchor: Option<String>,
}

/// Every store kept outside its working tree that this process can see: each
/// store index entry, and each store in the central stores directory whether
/// or not an entry names it. In-tree stores are not listed; they live and die
/// with their trees.
///
/// Reads two small files per store and starts nothing.
pub fn known_external_stores() -> Vec<KnownStore> {
    let stores = stores_dir();
    let stores = absolute_store_path(&stores).unwrap_or(stores);
    let mut seen = std::collections::BTreeSet::new();
    let mut known = Vec::new();
    let index_entries = StoreIndex::open()
        .map(|index| index.entries())
        .unwrap_or_default();
    for (indexed_tree, store) in index_entries {
        if !seen.insert(store.clone()) {
            continue;
        }
        known.push(describe_store(&store, Some(&indexed_tree), &stores));
    }
    if let Ok(listing) = std::fs::read_dir(&stores) {
        let mut paths: Vec<PathBuf> = listing
            .flatten()
            .map(|item| item.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| !name.starts_with('.'))
            })
            .collect();
        paths.sort();
        for store in paths {
            if !store.is_dir() || read_working_tree_record(&store).is_none() {
                continue;
            }
            if !seen.insert(store.clone()) {
                continue;
            }
            known.push(describe_store(&store, None, &stores));
        }
    }
    known
}

fn describe_store(store: &Path, indexed_tree: Option<&Path>, stores: &Path) -> KnownStore {
    let record = read_working_tree_record_file(store);
    let working_dir = record
        .as_ref()
        .map(|record| record.working_dir.clone())
        .or_else(|| indexed_tree.map(Path::to_path_buf))
        .unwrap_or_default();
    let git_anchor = record.as_ref().and_then(|record| record.git_anchor.clone());
    let central = store.parent() == Some(stores);
    let standing = if !store.is_dir() {
        StoreStanding::Reserved
    } else if working_dir.is_dir() {
        let serves = locate_store(&working_dir).is_some_and(|layout| {
            layout.root() == store
                || layout
                    .root()
                    .canonicalize()
                    .ok()
                    .zip(store.canonicalize().ok())
                    .is_some_and(|(left, right)| left == right)
        });
        if serves {
            StoreStanding::Serving
        } else {
            StoreStanding::Detached
        }
    } else if working_dir
        .parent()
        .is_some_and(|parent| parent.as_os_str().is_empty() || parent.is_dir())
    {
        StoreStanding::Gone
    } else {
        StoreStanding::Unreachable
    };
    KnownStore {
        store: store.to_path_buf(),
        working_dir,
        central,
        standing,
        git_anchor,
    }
}

/// The stores whose working tree is gone or unreachable and whose recorded
/// repository identity is in the Git checkout at `working_dir`: the stores a
/// moved or renamed copy of this repository left behind.
///
/// A match means this checkout's object database holds the commit the store
/// recorded, so it is the same repository's history. The store's tree must
/// be missing, so a clone or worktree that still exists beside this one never
/// loses its store to it. A directory that is not a Git checkout matches
/// nothing, because nothing else identifies a tree once its path has changed.
pub fn reattach_candidates(working_dir: &Path) -> Vec<KnownStore> {
    let Ok(canonical) = working_dir.canonicalize() else {
        return Vec::new();
    };
    if !canonical.join(".git").exists() {
        return Vec::new();
    }
    known_external_stores()
        .into_iter()
        .filter(|known| known.standing.is_orphaned() && known.working_dir != canonical)
        .filter(|known| {
            known
                .git_anchor
                .as_deref()
                .is_some_and(|anchor| git_checkout_holds(&canonical, anchor))
        })
        .collect()
}

/// Why no store serves a Git checkout, when the stores this process can see
/// say why. See [`explain_store_miss`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreMiss {
    /// One store this checkout left behind when it moved: its working tree is
    /// gone or unreachable, and its recorded commit is in this checkout's
    /// history.
    Moved(KnownStore),
    /// Several stores this repository's moved copies left behind, and nothing
    /// says which one this checkout continues.
    Ambiguous(Vec<KnownStore>),
    /// A store that still serves another checkout of the same repository:
    /// this checkout is a copy of that one, and the store stays with it.
    CopyOf(KnownStore),
}

/// Why no store serves the Git checkout enclosing `start`, with that
/// checkout's root, or `None` when `start` is in no Git checkout or no store
/// this process can see is this repository's.
///
/// A store left behind by a move is matched by history, as
/// [`reattach_candidates`] matches it. Of several, the one whose tree had
/// this checkout's directory name is the move when exactly one did, which is
/// what moving a checkout to a new parent looks like. A store still serving a
/// checkout whose history this one shares, at another path, makes this one a
/// copy.
pub fn explain_store_miss(start: &Path) -> Option<(PathBuf, StoreMiss)> {
    let mut canonical = start.canonicalize().ok()?;
    while !canonical.join(".git").exists() {
        if !canonical.pop() {
            return None;
        }
    }
    let miss = explain_checkout_miss(&canonical)?;
    Some((canonical, miss))
}

/// [`explain_store_miss`] for the checkout root `canonical`.
fn explain_checkout_miss(canonical: &Path) -> Option<StoreMiss> {
    let canonical = canonical.to_path_buf();
    let candidates = reattach_candidates(&canonical);
    match candidates.len() {
        0 => {}
        1 => return candidates.into_iter().next().map(StoreMiss::Moved),
        _ => {
            let same_name: Vec<&KnownStore> = candidates
                .iter()
                .filter(|known| known.working_dir.file_name() == canonical.file_name())
                .collect();
            return Some(match same_name.as_slice() {
                [chosen] => StoreMiss::Moved((*chosen).clone()),
                _ => StoreMiss::Ambiguous(candidates),
            });
        }
    }
    known_external_stores()
        .into_iter()
        .filter(|known| known.standing == StoreStanding::Serving && known.working_dir != canonical)
        .find(|known| {
            known
                .git_anchor
                .as_deref()
                .is_some_and(|anchor| git_checkout_holds(&canonical, anchor))
        })
        .map(StoreMiss::CopyOf)
}

/// The process id a store's daemon published, when it published one.
pub fn store_daemon_pid(store: &Path) -> Option<u32> {
    std::fs::read_to_string(store.join("daemon.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Whether a daemon is serving the store at `store` right now.
///
/// A running daemon holds an exclusive lock on the store's `daemon.lock` for
/// its whole life, so a lock this process cannot take is a live daemon. The
/// published pid is not used: a store copied from a live one carries the
/// donor daemon's pid, and that daemon serves the donor, not the copy, whose
/// `daemon.lock` is a different file nobody holds. A store whose daemon still
/// runs is not attached elsewhere or removed.
pub fn store_daemon_is_running(store: &Path) -> bool {
    use fs2::FileExt as _;
    let Ok(lock) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(store.join("daemon.lock"))
    else {
        return false;
    };
    match lock.try_lock_exclusive() {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&lock);
            false
        }
        Err(_) => true,
    }
}

/// How many bytes the store at `store` occupies on disk, as `du` counts them.
pub fn store_disk_bytes(store: &Path) -> u64 {
    crate::init_attempt::directory_bytes(store)
}

/// Delete a store kept in the central stores directory and the index entry
/// that names it, answering how many bytes it occupied.
///
/// Refused for anything that is not a direct child of the central stores
/// directory carrying a store's working-tree record, so a location a person
/// chose with `--store`, an in-tree store, or the Kin home itself can never
/// be removed through this. The store is renamed aside before it is deleted,
/// so a process that finds the store path mid-removal finds nothing rather
/// than half a store.
pub fn remove_central_store(store: &Path) -> Result<u64, KinError> {
    let stores = stores_dir();
    let stores = absolute_store_path(&stores).unwrap_or(stores);
    let store = absolute_store_path(store)?;
    let recorded = read_working_tree_record(&store);
    if store.parent() != Some(stores.as_path()) || recorded.is_none() {
        return Err(KinError::Other(format!(
            "{} is not a store in the central stores directory {}",
            store.display(),
            stores.display()
        )));
    }
    if store_daemon_is_running(&store) {
        return Err(KinError::Other(format!(
            "a daemon is still serving {}, so it is not removed",
            store.display()
        )));
    }
    let bytes = store_disk_bytes(&store);
    let name = store
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let doomed = stores.join(format!(
        ".removing-{name}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::rename(&store, &doomed).map_err(|error| KinError::io(&store, error))?;
    if let Some(recorded) = recorded {
        forget_store_index_entry_naming(&recorded, &store)?;
    }
    std::fs::remove_dir_all(&doomed).map_err(|error| KinError::io(&doomed, error))?;
    Ok(bytes)
}

/// Remove the store index entries that can never bind a store: a reservation
/// whose store does not exist, or an entry whose store now records another
/// tree. Answers the working trees whose entries were removed.
///
/// A reservation younger than an hour is left alone, because `kin init`
/// reserves its location before an admission that can take that long, and
/// removing the entry under it would publish a store nothing finds.
pub fn remove_stale_index_entries() -> Result<Vec<PathBuf>, KinError> {
    const RESERVATION_GRACE: std::time::Duration = std::time::Duration::from_secs(3600);
    let Some(index) = StoreIndex::open() else {
        return Ok(Vec::new());
    };
    let mut removed = Vec::new();
    for (tree, store) in index.entries() {
        let reserved_recently = std::fs::metadata(index.dir.join(store_index_file_name(&tree)))
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_none_or(|age| age < RESERVATION_GRACE);
        let stale = match std::fs::symlink_metadata(&store) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => !reserved_recently,
            Ok(_) => read_working_tree_record(&store).is_some_and(|recorded| recorded != tree),
            Err(_) => false,
        };
        if stale {
            forget_store_index_entry(&tree)?;
            removed.push(tree);
        }
    }
    Ok(removed)
}

/// Join a repository id onto a KinDB base directory to name that repository's
/// storage namespace.
///
/// The one place the `<base>/<repository-id>` rule is written down. A caller
/// holding a [`KinLayout`] goes through [`KinLayout::kindb_namespace_path`];
/// this exists for the callers that hold a backend's base path instead of a
/// layout, so that both spell the namespace the same way and a change to the
/// rule reaches both. Two independent copies of it are what let the CLI look
/// for graph truth at a path the daemon never writes.
pub fn kindb_namespace_in(kindb_dir: &Path, repo_id: &str) -> PathBuf {
    kindb_dir.join(repo_id)
}

/// The real home's `.kin`, which is an install-root identity rather than a
/// servable repo. Returns `None` when no home directory is resolvable.
///
/// It is not where the cross-repo registry necessarily lives. That file follows
/// [`crate::registry::managed_kin_home`], so this directory holds it only when
/// `KIN_HOME` is unset. Callers here are asking what a directory *is*, not
/// where to read the registry.
pub fn global_home_kin_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".kin"))
}

/// Entries a managed Kin install root carries and a repository store never
/// does: the cross-repo registry, the installed binaries, the injected shim,
/// and the shell hooks.
///
/// [`KinLayout`] has no accessor for any of these names, and that is the point.
/// Finding them inside a `.kin` is evidence about what the directory *is*,
/// which is an answer no environment variable can contradict.
const MANAGED_HOME_MARKERS: [&str; 4] = ["registry.toml", "bin", "lib", "shell"];

/// How many markers settle it.
///
/// One could be an accident, say a stray `bin/` somebody dropped inside a
/// repository store. Two together are the installer's fingerprint. A store has
/// no reason to grow even one, so a quorum of two errs toward still resolving a
/// real repository, which is the direction this guard must fail in: refusing a
/// user's own store is worse than binding an install root, and binding an
/// install root is what this exists to stop.
const MANAGED_HOME_MARKER_QUORUM: usize = 2;

/// Whether `candidate` holds enough toolchain-owned entries to be an install
/// root. Pure over the path, so it is decided by the directory rather than by
/// the environment the caller happens to have.
fn carries_managed_home_markers(candidate: &Path) -> bool {
    MANAGED_HOME_MARKERS
        .iter()
        .filter(|name| candidate.join(name).exists())
        .count()
        >= MANAGED_HOME_MARKER_QUORUM
}

/// Whether two paths name the same directory, comparing canonical forms too so
/// symlinked or non-normalized paths (a `/var` vs `/private/var` home on macOS)
/// still match.
fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize().ok(), b.canonicalize().ok()) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Whether `candidate` is a directory the Kin toolchain owns — the managed
/// install root holding the registry, the binaries, the shim and the models —
/// rather than a repository store that can be served.
///
/// Two tests, and the order is the fix. The first asks what the directory *is*,
/// by the entries only an install root carries. The second is the older
/// question of whether the path *is where this process's install root would
/// be*, kept because a freshly created home carrying one marker or none is
/// still not a repository.
///
/// Identity alone was the defect. It resolved the install root from the running
/// process's `$HOME`, so a process whose home was not the home that owns the
/// `.kin` being walked into compared two different paths, found them unequal,
/// and bound the toolchain directory as a repository. Every `kin` command in
/// that process then addressed the wrong root, not just `kin doctor`. A CI
/// runner with a redirected home, a `sudo` or service context, a container
/// mounting a host home, and every rig that isolates `HOME` all land there.
/// Asking what the directory holds is an answer none of them can change.
///
/// The manifest escape stays on the identity arm only. A `.kin` that is where
/// this process's install root would be, and carries a `manifest.json`, and
/// carries no toolchain markers, is somebody's repository sitting at that path,
/// and it is still bound. A directory carrying the markers is the install root
/// whatever else appears in it, so planting a `manifest.json` there does not
/// turn it into a repository.
///
/// Public because [`discover`](KinLayout::discover) is not the only caller that
/// has to tell the two directories apart. `kin init` refuses a directory that
/// already holds a `.kin`, and the remedy it should name depends on which of
/// them this is; asking here keeps that answer in one place, so a command
/// cannot decide the install root is a repository while discovery decides it is
/// not.
pub fn is_managed_kin_home(candidate: &Path) -> bool {
    if carries_managed_home_markers(candidate) {
        return true;
    }
    let identities = [
        global_home_kin_dir(),
        Some(crate::registry::managed_kin_home()),
    ];
    for home in identities.into_iter().flatten() {
        if same_dir(candidate, &home) {
            return !candidate.join("manifest.json").exists();
        }
    }
    false
}

/// The path component naming one hosted repository's text index.
///
/// Free-standing so a caller can name the component without a layout, and so
/// the mapping has one definition: every hosted site derives its directory from
/// this, and a second derivation elsewhere would be a second answer.
fn hosted_text_index_component(repo_id: &str) -> String {
    let digest = Sha256::digest(repo_id.as_bytes());
    let mut component = String::with_capacity(32);
    for byte in &digest[..16] {
        // `write!` into a `String` cannot fail; the result is consumed so the
        // formatting contract stays visible rather than being swallowed.
        let _ = write!(component, "{byte:02x}");
    }
    component
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository id is not a path component, and this is where that is
    /// enforced.
    ///
    /// `kin_model::RepositoryId::new` refuses only the empty string, ids over
    /// 255 bytes and control characters, so every id below is a legal
    /// repository id. Joined raw, the first three escape the directory the
    /// hosted daemon owns and the last two collide with a sibling. Every one of
    /// them has to land inside the hosted root, under a name made only of hex
    /// digits.
    #[test]
    fn hosted_text_index_component_admits_no_path_shape() {
        let layout = KinLayout::new(PathBuf::from("/repo/.kin"));
        let hosted_root = layout.text_index_dir().join("hosted");
        for repo_id in [
            "../peer", "..", "/etc/kin", "a/b", "a_b", "kin", "Kin", "kin.", " kin",
        ] {
            let dir = layout.hosted_text_index_dir(repo_id);
            let component = dir
                .strip_prefix(&hosted_root)
                .unwrap_or_else(|_| panic!("{repo_id:?} escaped the hosted text-index root"));
            assert_eq!(
                component.components().count(),
                1,
                "{repo_id:?} produced more than one path component"
            );
            let name = component.to_string_lossy();
            assert_eq!(name.len(), 32, "{repo_id:?} produced {name:?}");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{repo_id:?} produced {name:?}"
            );
        }
    }

    /// Ids that a filesystem or a careless sanitizer would fold together stay
    /// apart.
    ///
    /// Case and delimiter folding are the two ways a per-repository directory
    /// quietly becomes a shared one: `Kin` and `kin` are distinct repositories
    /// on a case-sensitive store, and `a/b` and `a_b` are distinct
    /// repositories that a slash-to-underscore rewrite would merge. A digest
    /// over the exact id bytes folds neither.
    #[test]
    fn hosted_text_index_directories_separate_ids_a_sanitizer_would_fold() {
        let layout = KinLayout::new(PathBuf::from("/repo/.kin"));
        let ids = ["kin", "Kin", "KIN", "a/b", "a_b", "a-b", "../kin", "kin/"];
        let directories: std::collections::HashSet<PathBuf> = ids
            .iter()
            .map(|repo_id| layout.hosted_text_index_dir(repo_id))
            .collect();
        assert_eq!(
            directories.len(),
            ids.len(),
            "distinct repository ids must get distinct directories"
        );
    }

    /// The same id names the same directory every time, in this process and in
    /// the next one.
    ///
    /// A hosted pod reopens its store across restarts and a rebuilt index is
    /// only reusable if the name is stable, so the mapping is pinned to a
    /// literal rather than recomputed by the test. A change to the digest, its
    /// truncation or the namespace fails here.
    #[test]
    fn hosted_text_index_directory_is_stable_for_an_id() {
        let layout = KinLayout::new(PathBuf::from("/repo/.kin"));
        assert_eq!(
            layout.hosted_text_index_dir("kin"),
            PathBuf::from("/repo/.kin/kindb/text-index/hosted/c1d6b0ba57d78fec8689f0877d805355")
        );
    }

    #[test]
    fn layout_paths() {
        let layout = KinLayout::new(PathBuf::from("/repo/.kin"));
        assert_eq!(
            layout.config_path(),
            PathBuf::from("/repo/.kin/config.toml")
        );
        assert_eq!(
            layout.manifest_path(),
            PathBuf::from("/repo/.kin/manifest.json")
        );
        assert_eq!(layout.kindb_dir(), PathBuf::from("/repo/.kin/kindb"));
        assert_eq!(
            layout.ingest_cas_dir(),
            PathBuf::from("/repo/.kin/kindb/ingest-cas")
        );
        assert_eq!(
            layout.kindb_snapshot_path(),
            PathBuf::from("/repo/.kin/kindb/graph.kndb")
        );
        assert_eq!(
            layout.kindb_head_generation_path(),
            PathBuf::from("/repo/.kin/kindb/head-generation")
        );
        assert_eq!(layout.working_dir(), Path::new("/repo"));
    }

    /// The namespace resolver names the repository-id directory, and it agrees
    /// with the free function the backend-holding callers use.
    ///
    /// Pinned as an exact path rather than as "somewhere under kindb" because
    /// the defect this replaced was a second, plausible-looking path under the
    /// same directory that nothing writes.
    #[test]
    fn kindb_namespace_is_the_repository_id_directory() {
        let layout = KinLayout::new(PathBuf::from("/repo/.kin"));
        let repo_id = "c2fd2519-1d5a-4292-8511-7e9196accade";
        assert_eq!(
            layout.kindb_namespace_path(repo_id),
            PathBuf::from("/repo/.kin/kindb/c2fd2519-1d5a-4292-8511-7e9196accade")
        );
        assert_eq!(
            layout.kindb_namespace_path(repo_id),
            kindb_namespace_in(&layout.kindb_dir(), repo_id)
        );
        assert_ne!(
            layout.kindb_namespace_path(repo_id),
            layout.kindb_snapshot_path()
        );
    }

    #[test]
    fn all_dirs_count() {
        let layout = KinLayout::new(PathBuf::from("/repo/.kin"));
        // kindb, ingest-cas, stashes, backups, projections, docs, bench, runs,
        // logs, adapters, shallow
        assert_eq!(layout.all_dirs().len(), 11);
    }

    /// A scratch machine: its own registry, and so its own store index, with
    /// no `KIN_STORE` leaking in from the environment running the tests.
    fn isolated_machine() -> (tempfile::TempDir, crate::test_env::EnvVarGuard) {
        let machine = tempfile::tempdir().unwrap();
        let guard = crate::test_env::EnvVarGuard::set(
            "KIN_REGISTRY_PATH",
            machine.path().join("registry.toml"),
        )
        .without(KIN_STORE_ENV)
        .without(KIN_STORE_LAYOUT_ENV)
        .without("KIN_ALLOW_PARENT_STORE");
        (machine, guard)
    }

    /// A store directory kept outside `tree`, registered and recording it.
    fn external_store(tree: &Path, store: &Path) -> PathBuf {
        let store = register_external_store(tree, store).unwrap();
        std::fs::create_dir_all(&store).unwrap();
        write_working_tree_record(&store, tree).unwrap();
        store
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn a_store_kept_outside_its_tree_is_found_from_anywhere_inside_it() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tree.path().join("src/pkg")).unwrap();
        let store = external_store(tree.path(), &machine.path().join("stores/repo"));

        let found = KinLayout::discover(&tree.path().join("src/pkg")).expect("discovered");
        assert!(found.is_external());
        assert_eq!(found.root(), store);
        assert_eq!(
            found.working_dir().canonicalize().unwrap(),
            tree.path().canonicalize().unwrap()
        );
        assert_eq!(
            entries(tree.path()),
            vec!["src"],
            "the tree carries nothing"
        );

        // A caller holding only the store's path learns the tree from the record.
        let from_path = KinLayout::new(store.clone());
        assert_eq!(from_path.working_dir(), tree.path().canonicalize().unwrap());
        assert_eq!(
            working_dir_of_store(&store).unwrap(),
            tree.path().canonicalize().unwrap()
        );
        assert!(store_exists_for(tree.path()));
        assert_eq!(store_root_for(tree.path()), store);
    }

    #[test]
    fn an_index_entry_whose_store_serves_another_tree_binds_nothing() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let store = external_store(tree.path(), &machine.path().join("stores/repo"));
        // The store was copied or moved to serve another tree; the old entry
        // still names it.
        write_working_tree_record(&store, other.path()).unwrap();

        assert!(KinLayout::discover(tree.path()).is_none());
        assert!(locate_store(tree.path()).is_none());
    }

    #[test]
    fn kin_store_binds_only_the_tree_its_store_records() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        let store = machine.path().join("explicit");
        std::fs::create_dir_all(&store).unwrap();
        write_working_tree_record(&store, tree.path()).unwrap();
        let _env = env.with(KIN_STORE_ENV, &store);

        let found = KinLayout::discover(tree.path()).expect("KIN_STORE serves its tree");
        assert_eq!(
            found.root().canonicalize().unwrap(),
            store.canonicalize().unwrap()
        );
        assert!(
            KinLayout::discover(unrelated.path()).is_none(),
            "an exported KIN_STORE must not bind some other repository"
        );
    }

    #[test]
    fn a_reserved_location_is_where_the_store_goes_but_not_a_store_yet() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        let reserved =
            register_external_store(tree.path(), &machine.path().join("stores/repo")).unwrap();

        assert_eq!(store_root_for(tree.path()), reserved);
        assert!(!store_exists_for(tree.path()));
        assert!(KinLayout::discover(tree.path()).is_none());
        let planned = KinLayout::for_working_dir(tree.path());
        assert!(planned.is_external());
        assert_eq!(planned.root(), reserved);

        forget_external_store(tree.path()).unwrap();
        let central = store_root_for(tree.path());
        assert_eq!(Some(central.clone()), central_store_root(tree.path()));
        assert!(
            central.starts_with(machine.path().canonicalize().unwrap().join(STORES_DIR_NAME)),
            "{} is the default once the reservation is gone",
            central.display()
        );
    }

    #[test]
    fn a_store_location_inside_or_around_the_tree_is_refused() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (_machine, _env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        for location in [
            tree.path().join(".kin"),
            tree.path().join("store"),
            tree.path().to_path_buf(),
            tree.path().parent().unwrap().to_path_buf(),
        ] {
            assert!(
                register_external_store(tree.path(), &location).is_err(),
                "{} must be refused",
                location.display()
            );
        }
    }

    /// A reservation is read from an untrusted index, just like a serving
    /// store. Native init must refuse unsafe destinations before staging any
    /// store, and must leave the index and the working tree as it found them.
    #[test]
    fn an_unsafe_index_reservation_refuses_init_before_any_mutation() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        let canonical = tree.path().canonicalize().unwrap();
        let index = store_index_dir();
        for refused in [
            canonical.join("vendor/store"),
            canonical.join(".kin"),
            index.join("nested-store"),
            machine.path().join(STORES_DIR_NAME),
            PathBuf::from("relative-store"),
            machine.path().join("missing/../store"),
        ] {
            write_store_index_entry(&canonical, &refused).unwrap();
            let entry = index.join(store_index_file_name(&canonical));
            let before = std::fs::read(&entry).unwrap();
            let machine_before = entries(machine.path());
            let index_before = entries(&index);

            assert!(
                reserve_store_root(&canonical).is_err(),
                "{} must not be reserved from the index",
                refused.display()
            );
            assert!(
                crate::init(&canonical).is_err(),
                "{} must refuse admission",
                refused.display()
            );
            assert_eq!(entries(&canonical), Vec::<String>::new());
            assert_eq!(entries(machine.path()), machine_before);
            assert_eq!(entries(&index), index_before);
            assert_eq!(std::fs::read(&entry).unwrap(), before);
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_index_reservation_through_a_link_into_the_tree_refuses_init() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        let canonical = tree.path().canonicalize().unwrap();
        let link = machine.path().join("external");
        std::os::unix::fs::symlink(&canonical, &link).unwrap();
        write_store_index_entry(&canonical, &link.join("store")).unwrap();
        let entry = store_index_dir().join(store_index_file_name(&canonical));
        let before = std::fs::read(&entry).unwrap();

        assert!(reserve_store_root(&canonical).is_err());
        assert!(crate::init(&canonical).is_err());
        assert_eq!(entries(&canonical), Vec::<String>::new());
        assert_eq!(std::fs::read(&entry).unwrap(), before);
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
    }

    #[test]
    fn a_valid_absent_central_or_external_reservation_is_kept_without_mutation() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        let canonical = tree.path().canonicalize().unwrap();
        for location in [
            machine.path().join("stores/repo"),
            machine.path().join("external/repo"),
        ] {
            let reserved = register_external_store(&canonical, &location).unwrap();
            let entry = store_index_dir().join(store_index_file_name(&canonical));
            let before = std::fs::read(&entry).unwrap();
            let parent = reserved.parent().unwrap();
            let parent_before = entries(parent);

            assert_eq!(reserve_store_root(&canonical).unwrap(), reserved);
            assert_eq!(store_root_for(&canonical), reserved);
            assert!(!reserved.exists());
            assert!(!store_exists_for(&canonical));
            assert_eq!(entries(&canonical), Vec::<String>::new());
            assert_eq!(entries(parent), parent_before);
            assert_eq!(std::fs::read(&entry).unwrap(), before);
        }
    }

    #[test]
    fn native_init_publishes_a_reserved_store_outside_the_tree_and_writes_nothing_into_it() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        let reserved =
            register_external_store(tree.path(), &machine.path().join("stores/repo")).unwrap();

        let initialized = crate::init(tree.path()).expect("initialize with the store outside");
        assert_eq!(initialized.layout.root(), reserved);
        assert!(initialized.layout.is_external());
        assert!(
            entries(tree.path()).is_empty(),
            "{:?}",
            entries(tree.path())
        );
        assert!(reserved.join("manifest.json").is_file());
        assert!(!reserved.join(".kin").exists());

        let found = KinLayout::discover(tree.path()).expect("found after init");
        assert_eq!(found.root(), reserved);
        crate::LocalRepositoryAuthorityBinding::from_layout(&found)
            .expect("the published store binds as a repository");
        assert!(
            crate::init(tree.path()).is_err(),
            "a second init sees the store kept outside the tree"
        );
    }

    /// An in-tree store as an earlier version left it: a `.kin` carrying its
    /// layout version.
    fn in_tree_store(tree: &Path) -> PathBuf {
        let kin_dir = tree.join(".kin");
        std::fs::create_dir_all(&kin_dir).unwrap();
        std::fs::write(kin_dir.join("version"), KIN_LAYOUT_VERSION.to_string()).unwrap();
        kin_dir
    }

    /// `kin init` with nothing naming a location: the store goes to the
    /// tree's slot in the central stores directory, the store index records
    /// it, and the tree carries nothing at all.
    #[test]
    fn a_new_store_goes_to_the_central_directory_and_the_tree_carries_nothing() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tree.path().join("src")).unwrap();

        let initialized = crate::init(tree.path()).expect("initialize");
        let root = initialized.layout.root().to_path_buf();
        assert!(initialized.layout.is_external());
        assert_eq!(
            root.parent(),
            Some(
                machine
                    .path()
                    .join(STORES_DIR_NAME)
                    .canonicalize()
                    .unwrap()
                    .as_path()
            ),
            "{} is not in the central stores directory",
            root.display()
        );
        let name = root.file_name().unwrap().to_string_lossy().into_owned();
        let leaf = tree
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            name.starts_with(leaf.trim_start_matches('.')),
            "{name} is named after the tree {leaf}"
        );
        assert_eq!(
            entries(tree.path()),
            vec!["src"],
            "the tree carries nothing"
        );
        assert!(root.join("manifest.json").is_file());
        assert_eq!(
            read_working_tree_record(&root).unwrap(),
            tree.path().canonicalize().unwrap()
        );

        let found = KinLayout::discover(&tree.path().join("src")).expect("found from inside");
        assert_eq!(found.root(), root);
        assert_eq!(locate_store(tree.path()).unwrap().root(), root);
        crate::LocalRepositoryAuthorityBinding::from_layout(&found)
            .expect("the central store binds as a repository");
        assert!(
            crate::init(tree.path()).is_err(),
            "a second init sees the central store"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_central_stores_directory_is_private_and_kept_out_of_spotlight() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let (machine, _env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        reserve_store_root(tree.path()).unwrap();
        let stores = machine.path().join(STORES_DIR_NAME);
        let mode = std::fs::metadata(&stores).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "the stores directory is {mode:o}");
        if cfg!(target_os = "macos") {
            assert!(stores.join(".metadata_never_index").is_file());
        }
    }

    #[test]
    fn the_in_tree_layout_keeps_a_new_store_in_its_tree() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, env) = isolated_machine();
        let _env = env.with(KIN_STORE_LAYOUT_ENV, "in-tree");
        let tree = tempfile::tempdir().unwrap();

        let initialized = crate::init(tree.path()).expect("initialize in the tree");
        assert!(!initialized.layout.is_external());
        assert_eq!(
            initialized.layout.root(),
            tree.path().canonicalize().unwrap().join(".kin")
        );
        assert!(!machine.path().join(STORES_DIR_NAME).exists());
        assert!(!machine.path().join(STORE_INDEX_DIR_NAME).exists());
    }

    /// A store an earlier version created in its tree keeps serving it, from
    /// the tree and from below it, and no central location is reserved.
    #[test]
    fn an_existing_in_tree_store_keeps_serving_its_tree() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        let kin_dir = in_tree_store(tree.path());
        std::fs::create_dir_all(tree.path().join("src")).unwrap();

        assert_eq!(store_root_for(tree.path()), kin_dir);
        assert_eq!(reserve_store_root(tree.path()).unwrap(), kin_dir);
        assert_eq!(
            KinLayout::discover(&tree.path().join("src"))
                .unwrap()
                .root(),
            kin_dir
        );
        assert!(!machine.path().join(STORE_INDEX_DIR_NAME).exists());
    }

    /// A slot holding another tree's store, such as one a tree that used to
    /// live at this path left behind, is never reused.
    #[test]
    fn an_occupied_central_slot_is_passed_over() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (_machine, _env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let first = central_store_root(tree.path()).unwrap();
        std::fs::create_dir_all(&first).unwrap();
        write_working_tree_record(&first, other.path()).unwrap();

        let second = central_store_root(tree.path()).unwrap();
        assert_ne!(second, first);
        assert_eq!(
            second.file_name().unwrap().to_string_lossy(),
            format!("{}-2", first.file_name().unwrap().to_string_lossy())
        );

        // The slot this tree's own store holds is its own, and reserving it
        // again puts the index entry back.
        write_working_tree_record(&first, tree.path()).unwrap();
        assert_eq!(central_store_root(tree.path()).unwrap(), first);
        assert_eq!(reserve_store_root(tree.path()).unwrap(), first);
        assert_eq!(locate_store(tree.path()).unwrap().root(), first);
    }

    /// A command run inside a central store, as an agent launched into a
    /// session workspace under its `runs/` is, finds that store's repository.
    #[test]
    fn a_directory_inside_a_central_store_finds_its_repository() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        let store = external_store(tree.path(), &machine.path().join("stores/repo"));
        let session = store.join("runs/session-1/src");
        std::fs::create_dir_all(&session).unwrap();

        let found = KinLayout::discover(&session).expect("found from inside the store");
        assert_eq!(found.root(), store);
        assert_eq!(
            found.working_dir().canonicalize().unwrap(),
            tree.path().canonicalize().unwrap()
        );

        // A copy of the store is not the store its tree finds.
        let copy = machine.path().join("stores/copy");
        std::fs::create_dir_all(copy.join("runs/session-2")).unwrap();
        write_working_tree_record(&copy, tree.path()).unwrap();
        assert!(KinLayout::discover(&copy.join("runs/session-2")).is_none());
    }

    /// A Kin home that cannot hold stores does not stop an init: the store
    /// stays in the tree.
    #[test]
    fn an_unusable_central_directory_keeps_the_store_in_the_tree() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        std::fs::write(machine.path().join(STORES_DIR_NAME), "not a directory").unwrap();
        let tree = tempfile::tempdir().unwrap();

        let root = reserve_store_root(tree.path()).unwrap();
        assert_eq!(root, tree.path().canonicalize().unwrap().join(".kin"));
    }

    /// The index is read as untrusted: an entry naming a location a store may
    /// not have binds nothing, even when the location carries a record that
    /// agrees with it.
    #[test]
    fn the_index_never_binds_a_location_a_store_may_not_have() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        let canonical = tree.path().canonicalize().unwrap();

        let inside = canonical.join("vendor/store");
        std::fs::create_dir_all(&inside).unwrap();
        write_working_tree_record(&inside, &canonical).unwrap();
        write_store_index_entry(&canonical, &inside).unwrap();
        assert!(
            locate_store(&canonical).is_none(),
            "a store inside its tree"
        );
        assert!(KinLayout::discover(&canonical).is_none());

        let home = machine.path().join("kin-home");
        managed_install_root(&home);
        let install_root = home.join(".kin").canonicalize().unwrap();
        let _env = env.with("KIN_HOME", &install_root);
        write_working_tree_record(&install_root, &canonical).unwrap();
        write_store_index_entry(&canonical, &install_root).unwrap();
        assert!(locate_store(&canonical).is_none(), "the install root");

        let stores = machine.path().join(STORES_DIR_NAME);
        std::fs::create_dir_all(&stores).unwrap();
        write_working_tree_record(&stores, &canonical).unwrap();
        write_store_index_entry(&canonical, &stores.canonicalize().unwrap()).unwrap();
        assert!(locate_store(&canonical).is_none(), "the stores directory");

        for refused in [inside, install_root, stores] {
            assert!(
                register_external_store(&canonical, &refused).is_err(),
                "{} must be refused",
                refused.display()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn indexed_discovery_resolves_aliases_before_testing_containment() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        let canonical = tree.path().canonicalize().unwrap();
        let inside = canonical.join("vendor/store");
        std::fs::create_dir_all(&inside).unwrap();
        write_working_tree_record(&inside, &canonical).unwrap();
        let alias = machine.path().join("outside-alias");
        std::os::unix::fs::symlink(&inside, &alias).unwrap();
        write_store_index_entry(&canonical, &alias).unwrap();
        let entry = store_index_dir().join(store_index_file_name(&canonical));
        let entry_before = std::fs::read(&entry).unwrap();
        let record_before = std::fs::read(inside.join(WORKING_TREE_RECORD)).unwrap();

        assert!(locate_store(&canonical).is_none());
        assert!(KinLayout::discover(&canonical).is_none());
        assert!(reserve_store_root(&canonical).is_err());
        assert_eq!(std::fs::read(&entry).unwrap(), entry_before);
        assert_eq!(
            std::fs::read(inside.join(WORKING_TREE_RECORD)).unwrap(),
            record_before
        );
    }

    #[test]
    fn indexed_discovery_rejects_parent_components_and_relative_locations() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        let canonical = tree.path().canonicalize().unwrap();
        let store = external_store(&canonical, &machine.path().join("custom-store"));
        let sibling = machine.path().join("sibling");
        std::fs::create_dir_all(&sibling).unwrap();
        for malformed in [
            sibling.join("../custom-store"),
            PathBuf::from("custom-store"),
        ] {
            write_store_index_entry(&canonical, &malformed).unwrap();
            assert!(
                locate_store(&canonical).is_none(),
                "{}",
                malformed.display()
            );
            assert!(KinLayout::discover(&canonical).is_none());
            assert!(reserve_store_root(&canonical).is_err());
            assert!(store.join(WORKING_TREE_RECORD).is_file());
        }
    }

    #[cfg(unix)]
    #[test]
    fn indexed_discovery_keeps_valid_external_aliases_and_checks_the_record() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        let tree = tempfile::tempdir().unwrap();
        let canonical = tree.path().canonicalize().unwrap();
        let store = external_store(&canonical, &machine.path().join("custom-store"));
        let alias = machine.path().join("custom-store-alias");
        std::os::unix::fs::symlink(&store, &alias).unwrap();
        write_store_index_entry(&canonical, &alias).unwrap();

        let layout = locate_store(&canonical).expect("a valid external alias still binds");
        assert_eq!(layout.root(), store.canonicalize().unwrap());
        assert_eq!(layout.working_dir(), canonical);
        assert_eq!(reserve_store_root(&canonical).unwrap(), layout.root());

        let other = tempfile::tempdir().unwrap();
        write_working_tree_record(&store, other.path()).unwrap();
        assert!(locate_store(&canonical).is_none(), "the record must agree");
        std::fs::write(store.join(WORKING_TREE_RECORD), b"invalid json").unwrap();
        assert!(
            locate_store(&canonical).is_none(),
            "a corrupt record cannot bind"
        );
        assert!(KinLayout::discover(&canonical).is_none());
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Kin Test")
            .env("GIT_AUTHOR_EMAIL", "kin@example.invalid")
            .env("GIT_COMMITTER_NAME", "Kin Test")
            .env("GIT_COMMITTER_EMAIL", "kin@example.invalid")
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// Copy the regular fixture tree, including its Git history, on every host.
    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            let destination = to.join(entry.file_name());
            if kind.is_dir() {
                copy_tree(&entry.path(), &destination);
            } else {
                assert!(kind.is_file(), "fixtures contain only regular files");
                std::fs::copy(entry.path(), destination).unwrap();
            }
        }
    }

    fn git_repository(dir: &Path, file: &str) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "--initial-branch=main"]);
        std::fs::write(dir.join(file), "content\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "seed"]);
    }

    /// A moved checkout is the same repository at a new path. Its history
    /// still holds the commit the store recorded, so the store is offered
    /// back; a different repository's checkout, and a store whose tree still
    /// exists, are not.
    #[test]
    fn a_store_left_behind_by_a_move_is_matched_by_history_not_by_path() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        let scratch = tempfile::tempdir().unwrap();
        let original = scratch.path().join("project");
        git_repository(&original, "a.txt");
        let store = external_store(&original, &machine.path().join("stores/project"));
        assert!(read_store_git_anchor(&store).is_some());

        let unrelated = scratch.path().join("unrelated");
        git_repository(&unrelated, "b.txt");
        assert!(
            reattach_candidates(&unrelated).is_empty(),
            "the original still exists, and its store is not offered to anyone"
        );

        let moved = scratch.path().join("moved/project-renamed");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::rename(&original, &moved).unwrap();
        assert!(locate_store(&moved).is_none());

        let offered = reattach_candidates(&moved);
        assert_eq!(offered.len(), 1, "{offered:?}");
        assert_eq!(offered[0].store, store.canonicalize().unwrap());
        assert_eq!(offered[0].standing, StoreStanding::Gone);
        assert!(
            reattach_candidates(&unrelated).is_empty(),
            "another repository's history does not hold the recorded commit"
        );

        let plain = scratch.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(
            reattach_candidates(&plain).is_empty(),
            "a directory that is not a Git checkout cannot be identified"
        );
    }

    /// A checkout no store serves says why when the stores can: one moved
    /// away from its store, a copy beside a checkout that keeps its store, or
    /// one of several moved copies, where the directory name decides only when
    /// exactly one shares it.
    #[test]
    fn a_miss_names_the_move_the_copy_or_the_ambiguity() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        let scratch = tempfile::tempdir().unwrap();
        let original = scratch.path().join("project");
        git_repository(&original, "a.txt");
        let store = external_store(&original, &machine.path().join("stores/project"));

        // A copy beside the original: the store stays with the original.
        let copy = scratch.path().join("project-copy");
        copy_tree(&original, &copy);
        assert!(locate_store(&copy).is_none());
        match explain_store_miss(&copy).map(|(_, miss)| miss) {
            Some(StoreMiss::CopyOf(known)) => {
                assert_eq!(known.store, store.canonicalize().unwrap());
                assert_eq!(known.working_dir, original.canonicalize().unwrap());
            }
            other => panic!("a copy, not {other:?}"),
        }

        // The original moves: the copy is now one of two checkouts that could
        // continue it, and only the one with its old name is the move.
        let moved = scratch.path().join("elsewhere/project");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::rename(&original, &moved).unwrap();
        // Asked from inside the checkout, it answers for the checkout's root.
        std::fs::create_dir_all(moved.join("nested")).unwrap();
        let (checkout, miss) = explain_store_miss(&moved.join("nested")).unwrap();
        assert_eq!(checkout, moved.canonicalize().unwrap());
        match Some(miss) {
            Some(StoreMiss::Moved(known)) => {
                assert_eq!(known.store, store.canonicalize().unwrap());
                assert_eq!(known.standing, StoreStanding::Gone);
            }
            other => panic!("the move, not {other:?}"),
        }

        // A second store for the same history, left behind by another move,
        // with neither tree named like this checkout: nothing to choose by.
        let other = scratch.path().join("other");
        copy_tree(&moved, &other);
        let second = external_store(&other, &machine.path().join("stores/other"));
        std::fs::remove_dir_all(&other).unwrap();
        let renamed = scratch.path().join("renamed");
        std::fs::rename(&moved, &renamed).unwrap();
        match explain_store_miss(&renamed).map(|(_, miss)| miss) {
            Some(StoreMiss::Ambiguous(stores)) => {
                let mut found: Vec<_> = stores.into_iter().map(|known| known.store).collect();
                found.sort();
                let mut expected = vec![
                    store.canonicalize().unwrap(),
                    second.canonicalize().unwrap(),
                ];
                expected.sort();
                assert_eq!(found, expected);
            }
            other => panic!("ambiguous, not {other:?}"),
        }

        let plain = scratch.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(explain_store_miss(&plain), None);
    }

    #[test]
    fn known_stores_say_how_each_stands() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        let scratch = tempfile::tempdir().unwrap();
        let stores = machine.path().join(STORES_DIR_NAME);

        let serving_tree = scratch.path().join("serving");
        std::fs::create_dir_all(&serving_tree).unwrap();
        let serving = external_store(&serving_tree, &stores.join("serving"));

        let gone_tree = scratch.path().join("gone");
        std::fs::create_dir_all(&gone_tree).unwrap();
        let gone = external_store(&gone_tree, &stores.join("gone"));
        std::fs::remove_dir(&gone_tree).unwrap();

        let unreachable_tree = scratch.path().join("volume/unreachable");
        std::fs::create_dir_all(&unreachable_tree).unwrap();
        let unreachable = external_store(&unreachable_tree, &stores.join("unreachable"));
        std::fs::remove_dir_all(scratch.path().join("volume")).unwrap();

        let detached_tree = scratch.path().join("detached");
        std::fs::create_dir_all(&detached_tree).unwrap();
        let detached = external_store(&detached_tree, &stores.join("detached"));
        forget_external_store(&detached_tree).unwrap();

        let reserved_tree = scratch.path().join("reserved");
        std::fs::create_dir_all(&reserved_tree).unwrap();
        let reserved = register_external_store(&reserved_tree, &stores.join("reserved")).unwrap();

        let known = known_external_stores();
        let standing = |store: &Path| {
            known
                .iter()
                .find(|known| known.store == store)
                .unwrap_or_else(|| panic!("{} is not listed: {known:?}", store.display()))
                .standing
        };
        assert_eq!(standing(&serving), StoreStanding::Serving);
        assert_eq!(standing(&gone), StoreStanding::Gone);
        assert_eq!(standing(&unreachable), StoreStanding::Unreachable);
        assert_eq!(standing(&detached), StoreStanding::Detached);
        assert_eq!(standing(&reserved), StoreStanding::Reserved);
        assert!(known.iter().all(|known| known.central));
        assert_eq!(known.len(), 5);
    }

    #[test]
    fn only_a_central_store_is_ever_removed() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, _env) = isolated_machine();
        let scratch = tempfile::tempdir().unwrap();
        let tree = scratch.path().join("tree");
        std::fs::create_dir_all(&tree).unwrap();

        let chosen = external_store(&tree, &scratch.path().join("chosen"));
        assert!(remove_central_store(&chosen).is_err(), "a --store location");
        assert!(chosen.is_dir());
        forget_external_store(&tree).unwrap();

        let in_tree = in_tree_store(&tree);
        assert!(remove_central_store(&in_tree).is_err(), "an in-tree store");
        std::fs::remove_dir_all(&in_tree).unwrap();

        let central = external_store(&tree, &machine.path().join(STORES_DIR_NAME).join("tree"));
        std::fs::write(central.join("payload"), vec![7u8; 4096]).unwrap();
        let bytes = remove_central_store(&central).expect("remove the central store");
        assert!(bytes >= 4096);
        assert!(!central.exists());
        assert!(locate_store(&tree).is_none());
        assert!(
            StoreIndex::open().unwrap().entries().is_empty(),
            "its index entry goes with it"
        );
    }

    #[test]
    fn discover_finds_kin_dir() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (_machine, _env) = isolated_machine();
        let dir = tempfile::tempdir().unwrap();
        let kin_dir = in_tree_store(dir.path());

        let found = KinLayout::discover(dir.path()).unwrap();
        assert_eq!(found.root(), kin_dir);
        assert!(!found.is_external());
    }

    #[test]
    fn discover_walks_up() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (_machine, _env) = isolated_machine();
        let dir = tempfile::tempdir().unwrap();
        let kin_dir = in_tree_store(dir.path());
        let sub = dir.path().join("src").join("deep");
        std::fs::create_dir_all(&sub).unwrap();

        let found = KinLayout::discover(&sub).unwrap();
        assert_eq!(found.root(), kin_dir);
    }

    #[test]
    fn discover_returns_none_when_missing() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (_machine, _env) = isolated_machine();
        let dir = tempfile::tempdir().unwrap();
        assert!(KinLayout::discover(dir.path()).is_none());
    }

    /// A directory named `.kin` that holds a Kin home's entries and nothing a
    /// store holds is not a store. The one behind the Linux CI failure held
    /// only a registry.
    #[test]
    fn discover_requires_a_real_kin_directory() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (_machine, _env) = isolated_machine();
        let dir = tempfile::tempdir().unwrap();
        assert!(KinLayout::discover(dir.path()).is_none());

        let kin_dir = dir.path().join(".kin");
        std::fs::create_dir(&kin_dir).unwrap();
        let found = KinLayout::discover(dir.path()).unwrap();
        assert_eq!(found.root(), kin_dir);

        std::fs::write(kin_dir.join("registry.toml"), "").unwrap();
        assert!(
            KinLayout::discover(dir.path()).is_none(),
            "a .kin holding only a registry is a Kin home, not a store"
        );
        assert!(locate_store(dir.path()).is_none());
        std::fs::remove_file(kin_dir.join("registry.toml")).unwrap();
        std::fs::create_dir(kin_dir.join(STORES_DIR_NAME)).unwrap();
        assert!(
            KinLayout::discover(dir.path()).is_none(),
            "a .kin holding central stores is a Kin home, not a store"
        );

        std::fs::write(kin_dir.join("version"), KIN_LAYOUT_VERSION.to_string()).unwrap();
        let found = KinLayout::discover(dir.path()).unwrap();
        assert_eq!(found.root(), kin_dir);
    }

    #[test]
    fn discover_refuses_parent_store_across_nested_repo_boundary() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (_machine, _env) = isolated_machine();
        let dir = tempfile::tempdir().unwrap();
        in_tree_store(dir.path());
        let nested = dir.path().join("nested");
        std::fs::create_dir_all(nested.join(".git")).unwrap();
        let deep = nested.join("src");
        std::fs::create_dir_all(&deep).unwrap();

        assert!(
            KinLayout::discover(&deep).is_none(),
            "must refuse to bind a parent .kin across a nested-repo (.git) boundary"
        );

        let nested_store = in_tree_store(&nested);
        let found = KinLayout::discover(&deep).unwrap();
        assert_eq!(
            found.root(),
            nested_store,
            "a nested repo with its own .kin binds that store, not the parent"
        );
    }

    /// The walk ends at the first working-tree root, so nothing above it is
    /// bound, whether the store up there is in its tree or central. Only an
    /// explicit opt-in walks past it.
    #[test]
    fn discovery_ends_at_the_first_repository_boundary() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let (machine, env) = isolated_machine();
        let outer = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outer.path().join(".git")).unwrap();
        let outer_store = external_store(outer.path(), &machine.path().join("stores/outer"));

        // A linked worktree marks its root with a `.git` file.
        let worktree = outer.path().join("worktrees/feature");
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: elsewhere\n").unwrap();
        assert!(
            KinLayout::discover(&worktree.join("src")).is_none(),
            "a worktree without a store of its own must not bind the outer tree's store"
        );

        // A plain subdirectory of the outer tree is part of it.
        let plain = outer.path().join("docs/guides");
        std::fs::create_dir_all(&plain).unwrap();
        let found = KinLayout::discover(&plain).expect("a subdirectory finds its tree's store");
        assert_eq!(found.root(), outer_store);

        // The worktree's own store wins inside it.
        let worktree_store = external_store(&worktree, &machine.path().join("stores/feature"));
        let found = KinLayout::discover(&worktree.join("src")).unwrap();
        assert_eq!(found.root(), worktree_store);
        forget_external_store(&worktree).unwrap();

        let _env = env.with("KIN_ALLOW_PARENT_STORE", "1");
        let found =
            KinLayout::discover(&worktree.join("src")).expect("the opt-in walks past the boundary");
        assert_eq!(found.root(), outer_store);
    }

    #[test]
    fn is_managed_kin_home_rejects_unrelated_and_repo_paths() {
        // A normal repo `.kin` (carrying a manifest) is never the install root.
        let dir = tempfile::tempdir().unwrap();
        let kin_dir = dir.path().join(".kin");
        std::fs::create_dir(&kin_dir).unwrap();
        std::fs::write(kin_dir.join("manifest.json"), "{}").unwrap();
        assert!(!is_managed_kin_home(&kin_dir));

        // An unrelated `.kin` path that is not the install root is never the
        // install root either, regardless of manifest presence.
        let bare = dir.path().join("sub").join(".kin");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(!is_managed_kin_home(&bare));
    }

    /// Build a `.kin` that looks exactly like what the installer leaves behind:
    /// the cross-repo registry, the installed binaries, the shim and the hooks.
    fn managed_install_root(at: &Path) -> PathBuf {
        let managed = at.join(".kin");
        std::fs::create_dir_all(managed.join("bin")).unwrap();
        std::fs::create_dir_all(managed.join("lib")).unwrap();
        std::fs::create_dir_all(managed.join("shell")).unwrap();
        std::fs::write(managed.join("registry.toml"), "").unwrap();
        managed
    }

    /// The defect FIR-2018 and FIR-2014 both describe, from the side that
    /// produced it: a process whose `$HOME` is not the home owning the `.kin`
    /// the walk reaches. The identity comparison cannot save this case, so what
    /// the directory holds has to.
    #[test]
    fn discover_refuses_the_install_root_when_home_is_elsewhere() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let scratch = tempfile::tempdir().unwrap();
        let toolchain = scratch.path().join("toolchain");
        let managed = managed_install_root(&toolchain);
        let cwd = toolchain.join("sub");
        std::fs::create_dir_all(&cwd).unwrap();

        let elsewhere = scratch.path().join("otherhome");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let _home = crate::test_env::EnvVarGuard::set("HOME", &elsewhere);
        let _kin_home = crate::test_env::EnvVarGuard::unset("KIN_HOME");
        let _kin_dir = crate::test_env::EnvVarGuard::unset("KIN_DIR");

        let found = KinLayout::discover(&cwd);
        assert!(
            found.is_none(),
            "discover bound the managed install root at {:?} because $HOME pointed elsewhere",
            found.map(|layout| layout.root().to_path_buf())
        );
        // The install root really was there to be found, so the assertion above
        // is about the guard rather than about an empty walk.
        assert!(managed.is_dir(), "the fixture install root must exist");
    }

    /// The same directory, with `$HOME` set to the home that owns it. This is
    /// the arm the old identity comparison already handled, and it must keep
    /// working: a fix that only moved the failure to the correct-home case
    /// would pass the test above and still be wrong.
    #[test]
    fn discover_refuses_the_install_root_when_home_is_its_own() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let scratch = tempfile::tempdir().unwrap();
        let toolchain = scratch.path().join("toolchain");
        managed_install_root(&toolchain);
        let cwd = toolchain.join("sub");
        std::fs::create_dir_all(&cwd).unwrap();

        let _home = crate::test_env::EnvVarGuard::set("HOME", &toolchain);
        let _kin_home = crate::test_env::EnvVarGuard::unset("KIN_HOME");
        let _kin_dir = crate::test_env::EnvVarGuard::unset("KIN_DIR");

        let found = KinLayout::discover(&cwd);
        assert!(
            found.is_none(),
            "discover bound the managed install root at {:?} under its own home",
            found.map(|layout| layout.root().to_path_buf())
        );
    }

    /// A `manifest.json` planted in the install root does not promote it to a
    /// repository. FIR-2014 names this as its own acceptance line, because the
    /// old guard read the manifest as proof of a real repo and would have been
    /// flipped by one file.
    #[test]
    fn a_planted_manifest_does_not_make_the_install_root_a_repository() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let scratch = tempfile::tempdir().unwrap();
        let toolchain = scratch.path().join("toolchain");
        let managed = managed_install_root(&toolchain);
        std::fs::write(managed.join("manifest.json"), "{}").unwrap();
        let cwd = toolchain.join("sub");
        std::fs::create_dir_all(&cwd).unwrap();

        let _home = crate::test_env::EnvVarGuard::set("HOME", &toolchain);
        let _kin_home = crate::test_env::EnvVarGuard::unset("KIN_HOME");
        let _kin_dir = crate::test_env::EnvVarGuard::unset("KIN_DIR");

        assert!(
            KinLayout::discover(&cwd).is_none(),
            "a manifest planted in the install root turned it into a repository"
        );
    }

    /// The blast radius the ticket named, held open on purpose. A repository
    /// part way through `kin init`, before its manifest exists, still resolves,
    /// because it carries none of the toolchain's entries. A guard written as
    /// "a `.kin` with no manifest is not a repository" would fail here, which is
    /// exactly why this fix does not do that.
    #[test]
    fn a_repository_without_its_manifest_yet_still_resolves() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let scratch = tempfile::tempdir().unwrap();
        let repo = scratch.path().join("repo");
        let store = repo.join(".kin");
        std::fs::create_dir_all(store.join("kindb")).unwrap();
        let cwd = repo.join("src");
        std::fs::create_dir_all(&cwd).unwrap();

        let elsewhere = scratch.path().join("otherhome");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let _home = crate::test_env::EnvVarGuard::set("HOME", &elsewhere);
        let _kin_home = crate::test_env::EnvVarGuard::unset("KIN_HOME");
        let _kin_dir = crate::test_env::EnvVarGuard::unset("KIN_DIR");

        let found = KinLayout::discover(&cwd).expect("a store mid-init must still be discovered");
        assert_eq!(found.root(), store);
    }

    /// `KIN_HOME` relocates the install root, and a relocated root that has not
    /// yet grown two markers is still not a repository.
    #[test]
    fn discover_refuses_a_relocated_install_root_named_by_kin_home() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let scratch = tempfile::tempdir().unwrap();
        let relocated = scratch.path().join("relocated").join(".kin");
        std::fs::create_dir_all(&relocated).unwrap();
        let cwd = scratch.path().join("relocated").join("sub");
        std::fs::create_dir_all(&cwd).unwrap();

        let elsewhere = scratch.path().join("otherhome");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let _home = crate::test_env::EnvVarGuard::set("HOME", &elsewhere);
        let _kin_home = crate::test_env::EnvVarGuard::set("KIN_HOME", &relocated);
        let _kin_dir = crate::test_env::EnvVarGuard::unset("KIN_DIR");

        assert!(
            KinLayout::discover(&cwd).is_none(),
            "the install root named by KIN_HOME was bound as a repository"
        );
    }

    /// One marker is not a quorum. A repository store that happens to carry a
    /// `bin/` is still a repository, which is the direction this guard is
    /// deliberately built to fail in.
    #[test]
    fn one_toolchain_marker_does_not_condemn_a_repository() {
        if !crate::test_env::alone_in_process() {
            return;
        }
        let scratch = tempfile::tempdir().unwrap();
        let repo = scratch.path().join("repo");
        let store = repo.join(".kin");
        std::fs::create_dir_all(store.join("bin")).unwrap();
        std::fs::write(store.join("manifest.json"), "{}").unwrap();
        let cwd = repo.join("src");
        std::fs::create_dir_all(&cwd).unwrap();

        let elsewhere = scratch.path().join("otherhome");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let _home = crate::test_env::EnvVarGuard::set("HOME", &elsewhere);
        let _kin_home = crate::test_env::EnvVarGuard::unset("KIN_HOME");
        let _kin_dir = crate::test_env::EnvVarGuard::unset("KIN_DIR");

        let found = KinLayout::discover(&cwd).expect("one stray marker must not hide a repository");
        assert_eq!(found.root(), store);
    }

    #[test]
    fn global_home_kin_dir_targets_home_dot_kin() {
        if let Some(home_kin) = global_home_kin_dir() {
            assert_eq!(home_kin.file_name().and_then(|n| n.to_str()), Some(".kin"));
        }
    }

    fn empty_kin_layout() -> (tempfile::TempDir, KinLayout) {
        let dir = tempfile::tempdir().unwrap();
        let kin_dir = dir.path().join(".kin");
        std::fs::create_dir(&kin_dir).unwrap();
        let layout = KinLayout::new(kin_dir);
        (dir, layout)
    }

    #[test]
    fn migrate_refuses_pre_versioning_repo_without_authority() {
        let (_dir, layout) = empty_kin_layout();
        assert!(!layout.version_path().exists());
        let error = layout.migrate().unwrap_err();
        assert!(error.to_string().contains("no migration path"));
        assert!(!layout.version_path().exists());
    }

    #[test]
    fn migrate_is_noop_on_current_version() {
        let (_dir, layout) = empty_kin_layout();
        layout.write_version(KIN_LAYOUT_VERSION).unwrap();
        let before = std::fs::metadata(layout.version_path())
            .unwrap()
            .modified()
            .unwrap();
        layout.migrate().unwrap();
        // Current version: nothing rewritten.
        let after = std::fs::metadata(layout.version_path())
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn migrate_refuses_newer_layout() {
        let (_dir, layout) = empty_kin_layout();
        layout.write_version(KIN_LAYOUT_VERSION + 7).unwrap();
        match layout.migrate() {
            Err(KinError::IncompatibleVersion { found, supported }) => {
                assert_eq!(found, KIN_LAYOUT_VERSION + 7);
                assert_eq!(supported, KIN_LAYOUT_VERSION);
            }
            other => panic!("expected IncompatibleVersion, got {other:?}"),
        }
    }

    #[test]
    fn migrate_loudly_refuses_old_unmigratable_layout() {
        // Synthesize an older layout (version 0) with no registered upgrade
        // step. migrate() must refuse loudly, never silently accept it.
        let (_dir, layout) = empty_kin_layout();
        layout.write_version(0).unwrap();
        match layout.migrate() {
            Err(KinError::Config(msg)) => {
                assert!(msg.contains("no migration path"), "got: {msg}");
            }
            other => panic!("expected loud Config refusal, got {other:?}"),
        }
        // The on-disk version is left untouched by a refused migration.
        assert_eq!(layout.read_version().unwrap(), 0);
    }
}
