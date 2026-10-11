// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Reuse a store this machine already prepared for the same Git state.
//!
//! `kin init` over a Git checkout, and `kin clone` over Git transport, first
//! ask the prepared-store catalog ([`kin_core::prepared_store`]) for a store
//! admitted from exactly this checkout's refs and `HEAD`. The catalog names
//! candidates; nothing it says is trusted. Each candidate is reopened with its
//! authority frozen and must still serve exactly its Git admission, and its
//! derivations are compared with this build's:
//!
//! - derived by this build: the store is copied as it is, with its graph,
//!   language-server enrichment and vectors, and nothing is derived again;
//! - derived by another build, or with no record saying which: the copy is
//!   re-derived by `kin upgrade` before it is published, so it is never served
//!   as if this build had derived it, and the upgrade carries the
//!   language-server edges over;
//! - recorded by a newer build, or with a record this build cannot read: it is
//!   passed over.
//!
//! The copy is the recovery carrier's checked copy
//! ([`super::recovery_carrier::reuse_copy`]): every file hashed, the donor
//! inventoried again, the staged authority reopened against the frozen roots,
//! and published with a no-replace rename. The donor is only read.
//!
//! The reused store keeps the donor's repository and workspace identities, as
//! a restored carrier does. Its location, registry row, daemon and ownership
//! are its own, and the local spine selects one authority per repository, so
//! the two stores never replace each other. Minting a new workspace identity
//! for a reused store needs an authority operation that does not exist yet.
//!
//! When nothing is reusable the reasons are returned, and the caller admits
//! the checkout from scratch.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kin_core::hydration_semantics::{self, HydrationStanding};
use kin_core::prepared_store::{self, PreparedStore};
use serde::Serialize;

use super::upgrade::{UpgradeHooks, UpgradeReport};

/// Schema of the machine-readable reuse report.
pub const REUSE_REPORT_SCHEMA: &str = "kin.init-reuse.v1";

/// How a prepared store was brought to this build.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "route", rename_all = "snake_case")]
pub enum ReuseRoute {
    /// This build derived it, so nothing was derived again.
    Current,
    /// Another build derived it, and `kin upgrade` re-derived the copy under
    /// this build before it was published.
    Rederived {
        /// The standing the donor had against this build.
        standing: &'static str,
        /// Why, in the words every surface uses.
        reason: String,
        upgrade: Box<UpgradeReport>,
    },
}

/// Milliseconds each step of a reuse took.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ReuseTimings {
    /// Reading this checkout's refs and status and the catalog.
    pub lookup_ms: u64,
    /// Reading and checking the donor's authority.
    pub open_ms: u64,
    /// Copying and hashing the donor's files.
    pub copy_ms: u64,
    /// Proving the copy is the donor's authority.
    pub verify_ms: u64,
    /// Rewriting the copy's configuration, and re-deriving it when needed.
    pub finish_ms: u64,
    pub total_ms: u64,
}

/// What a reuse carried from the donor.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReuseCarried {
    pub files: usize,
    pub bytes: u64,
    pub tracked_artifacts: u64,
    pub entities: usize,
    pub relations: usize,
    /// Files whose language-server enrichment the donor recorded finished.
    pub enriched_files: usize,
    /// Vectors in the donor's persisted index, when it had one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vectors: Option<u64>,
}

/// A store reused for this checkout.
#[derive(Debug, Clone, Serialize)]
pub struct ReuseReport {
    pub schema: &'static str,
    pub donor: PathBuf,
    pub repository_id: String,
    pub workspace_id: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(flatten)]
    pub route: ReuseRoute,
    pub carried: ReuseCarried,
    pub timings: ReuseTimings,
    /// Candidates passed over before this one, and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub passed_over: Vec<String>,
}

/// What asking for a reuse came to.
#[derive(Debug)]
pub enum ReuseAttempt {
    Reused(Box<ReuseReport>),
    /// Nothing was reused, for these reasons, and nothing was written.
    /// `candidates` counts the catalog entries that named this checkout's
    /// source, so a caller can tell "nothing prepared" from "passed over".
    Declined {
        reasons: Vec<String>,
        candidates: usize,
    },
}

fn declined(reason: impl Into<String>) -> ReuseAttempt {
    ReuseAttempt::Declined {
        reasons: vec![reason.into()],
        candidates: 0,
    }
}

/// Reuse a prepared store for the Git checkout at `dir`, when one is.
///
/// Writes nothing unless it returns [`ReuseAttempt::Reused`], and then only
/// the store for `dir`, published whole: `dir/.kin`, or the location `kin init
/// --store` reserved outside the tree. The copy records `enrichment_enabled`
/// as its language-server enrichment setting before it is published, because
/// that setting is the caller's and not the donor's.
pub fn try_reuse(dir: &Path, enrichment_enabled: bool, progress: &dyn Fn(&str)) -> ReuseAttempt {
    let started = Instant::now();
    if !prepared_store::reuse_enabled() {
        return declined(format!(
            "{}=0 turns reuse off",
            prepared_store::PREPARED_STORE_REUSE_ENV
        ));
    }
    let source = match prepared_store::checkout_source(dir) {
        Ok(source) => source,
        Err(reason) => return declined(reason),
    };
    let catalog = prepared_store::catalog_path();
    let candidates = match prepared_store::candidates_at(&catalog, &source.digest) {
        Ok(candidates) => candidates,
        Err(reason) => return declined(reason),
    };
    // An entry whose location holds no store any more names a store that was
    // moved or removed. It is forgotten here rather than when it happens to be
    // tried, so the catalog does not keep pointing at nothing.
    let candidates = candidates
        .into_iter()
        .filter_map(|candidate| {
            let donor = candidate.located_store();
            if donor.is_none() {
                let _ = prepared_store::forget_at(&catalog, &candidate.location);
            }
            donor.map(|donor| (candidate, donor))
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return declined(
            "no prepared store on this machine was admitted from this checkout's exact Git refs",
        );
    }
    let count = candidates.len();
    // The same refusal an admission makes first. Reuse must not become a way
    // around a blocker the checkout carries.
    if let Err(error) = kin_git::check_git_admission_blockers(&kin_io::WorktreeRoot::new(
        dir,
        &kin_io::Grant::ambient(),
    )) {
        return ReuseAttempt::Declined {
            reasons: vec![format!("this checkout has an admission blocker: {error}")],
            candidates: count,
        };
    }
    let lookup = started.elapsed();
    let here = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut passed_over = Vec::new();
    for (candidate, donor) in candidates {
        let location = candidate
            .location
            .canonicalize()
            .unwrap_or_else(|_| candidate.location.clone());
        if location == here {
            continue;
        }
        progress(&format!(
            "Checking the prepared store at {}",
            location.display()
        ));
        match reuse_from(
            dir,
            &location,
            &donor,
            &source,
            lookup,
            started,
            enrichment_enabled,
            progress,
        ) {
            Ok(mut report) => {
                report.passed_over = passed_over;
                return ReuseAttempt::Reused(Box::new(report));
            }
            Err(reason) => passed_over.push(format!("{}: {reason}", location.display())),
        }
    }
    ReuseAttempt::Declined {
        reasons: passed_over,
        candidates: count,
    }
}

#[allow(clippy::too_many_arguments)]
fn reuse_from(
    dir: &Path,
    location: &Path,
    donor: &kin_core::KinLayout,
    source: &prepared_store::CheckoutSource,
    lookup: Duration,
    started: Instant,
    enrichment_enabled: bool,
    progress: &dyn Fn(&str),
) -> Result<ReuseReport, String> {
    let opening = Instant::now();
    let store = PreparedStore::open(donor, prepared_store::PREPARED_STORE_OPEN_WAIT)?;
    if store.source.digest() != source.digest {
        return Err("it was admitted from other Git refs than its catalog entry says".to_string());
    }
    let standing =
        hydration_semantics::standing_of(&store.hydration, hydration_semantics::binary_version());
    let rederive = match &standing {
        HydrationStanding::Ahead { .. } | HydrationStanding::Unreadable { .. } => {
            return Err(standing.sentence());
        }
        HydrationStanding::Rederived { under, derives, .. } if under > derives => {
            return Err(standing.sentence());
        }
        standing => standing.is_gap(),
    };
    let target = kin_core::layout::reserve_store_root(dir)
        .map_err(|error| format!("its store location could not be reserved: {error}"))?;
    let author = if rederive {
        Some(
            super::require_commit_author_for(&kin_core::KinLayout::for_working_dir(dir)).map_err(
                |error| {
                    format!(
                        "{}, and re-deriving it records a change, which needs a commit author: \
                         {error:#}",
                        standing.sentence()
                    )
                },
            )?,
        )
    } else {
        None
    };
    let vectors = persisted_vector_count(donor);
    let open = opening.elapsed();
    let tracked = store.tracked_artifacts;
    let mut upgrade = None;
    progress(&format!(
        "Copying the prepared store at {}",
        location.display()
    ));
    let copied = super::recovery_carrier::reuse_copy(
        donor.root(),
        &target,
        &store.roots,
        &store.repository_id,
        &store.workspace_id,
        |staged| {
            // First, so every step below reads this checkout as the copy's
            // working tree, never the donor's.
            kin_core::layout::stage_store_placement(staged, &target, dir)?;
            let staged = kin_core::KinLayout::new(staged.to_path_buf());
            kin_core::git_init::refresh_git_coexistence_config(&staged, dir)?;
            kin_core::enrichment_setting::record_unpublished(&staged, enrichment_enabled)?;
            if let Some(author) = author.clone() {
                progress("Re-deriving the copy under this build");
                let quiet = |line: &str| progress(line);
                upgrade = Some(super::upgrade::upgrade_store(
                    &staged,
                    author,
                    &UpgradeHooks::default(),
                    &quiet,
                )?);
            }
            // The checkout was just shown to be exactly the commit the store
            // admitted, so its graph truth matches its files as of now.
            kin_core::last_admission::record(&staged, tracked);
            Ok(())
        },
    )
    .map_err(|error| format!("it could not be copied: {error:#}"))?;
    let route = match upgrade {
        Some(upgrade) => {
            if !upgrade.warnings.is_empty() {
                // The copy is published with the upgrade committed; its
                // follow-up steps are what `kin upgrade` finishes, and the
                // report carries them.
                tracing::warn!(warnings = ?upgrade.warnings, "the reused store's upgrade left follow-up steps");
            }
            ReuseRoute::Rederived {
                standing: standing.label(),
                reason: standing.sentence(),
                upgrade: Box::new(upgrade),
            }
        }
        None => ReuseRoute::Current,
    };
    let report = ReuseReport {
        schema: REUSE_REPORT_SCHEMA,
        donor: location.to_path_buf(),
        repository_id: store.repository_id.clone(),
        workspace_id: store.workspace_id.clone(),
        source: source.digest.clone(),
        head: source.identity.head_object().map(str::to_string),
        route,
        carried: ReuseCarried {
            files: copied.files,
            bytes: copied.bytes,
            tracked_artifacts: tracked,
            entities: copied.entities,
            relations: copied.relations,
            enriched_files: store.enrichment_marks,
            vectors,
        },
        timings: ReuseTimings {
            lookup_ms: millis(lookup),
            open_ms: millis(open),
            copy_ms: millis(copied.copy),
            verify_ms: millis(copied.verify),
            finish_ms: millis(copied.finish),
            total_ms: millis(started.elapsed()),
        },
        passed_over: Vec::new(),
    };
    drop(store);
    Ok(report)
}

/// The vectors the persisted index at `layout` holds, from its metadata.
pub(crate) fn persisted_vector_count(layout: &kin_core::KinLayout) -> Option<u64> {
    let path = layout
        .kindb_vector_index_path()
        .with_extension("kvec.meta.json");
    let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    meta.get("indexed")?.as_u64()
}

/// The persisted vector index's binding, which moves whenever its contents do.
pub(crate) fn persisted_vector_binding(layout: &kin_core::KinLayout) -> Option<String> {
    let path = layout
        .kindb_vector_index_path()
        .with_extension("kvec.meta.json");
    let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    Some(meta.get("index_binding_sha256")?.as_str()?.to_string())
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl ReuseReport {
    /// The lines a person reads, before the enrichment phase reports.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let head = self
            .head
            .as_deref()
            .map(|head| format!(", HEAD {}", &head[..head.len().min(12)]))
            .unwrap_or_default();
        lines.push(format!(
            "Reused the prepared store at {} for this checkout: the same Git history and refs{head}.",
            self.donor.display()
        ));
        let carried = &self.carried;
        lines.push(format!(
            "  Copied {} files ({}) in {} ms and checked the copy against the store's authority \
             in {} ms.",
            carried.files,
            human_bytes(carried.bytes),
            self.timings.copy_ms,
            self.timings.verify_ms
        ));
        let vectors = carried
            .vectors
            .map(|vectors| format!(", {vectors} vectors"))
            .unwrap_or_default();
        lines.push(format!(
            "  Reused: {} tracked files, {} entities, {} relations, language-server enrichment \
             for {} files{vectors}.",
            carried.tracked_artifacts, carried.entities, carried.relations, carried.enriched_files
        ));
        match &self.route {
            ReuseRoute::Current => lines.push(
                "  Recomputed: nothing. This build derived the store, so no history, entity or \
                 relation was derived again."
                    .to_string(),
            ),
            ReuseRoute::Rederived {
                reason, upgrade, ..
            } => {
                let checkpoints = upgrade.heads.iter().filter(|head| head.checkpoint).count();
                let entity_deltas: usize =
                    upgrade.heads.iter().map(|head| head.entity_deltas).sum();
                let relation_deltas: usize =
                    upgrade.heads.iter().map(|head| head.relation_deltas).sum();
                let carried_relations: usize = upgrade
                    .heads
                    .iter()
                    .map(|head| head.carried_relations)
                    .sum();
                lines.push(format!(
                    "  Recomputed: {reason}, so the copy was re-derived under this build before \
                     it was published."
                ));
                lines.push(format!(
                    "    Parsed {} source files in {} ms: {} entity and {} relation changes on {} \
                     head(s), {} language-server relation(s) carried over.",
                    upgrade.source_files,
                    upgrade.elapsed_ms,
                    entity_deltas,
                    relation_deltas,
                    checkpoints,
                    carried_relations
                ));
            }
        }
        lines.push(format!(
            "  Repository {}, workspace {}: the same identities as the prepared store, in a store \
             of this checkout's own.",
            self.repository_id, self.workspace_id
        ));
        lines.push(format!(
            "  Reuse took {} ms: lookup {} ms, checking the prepared store {} ms, copy {} ms, \
             verification {} ms, finishing {} ms.",
            self.timings.total_ms,
            self.timings.lookup_ms,
            self.timings.open_ms,
            self.timings.copy_ms,
            self.timings.verify_ms,
            self.timings.finish_ms
        ));
        lines
    }
}

fn human_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.1} MB", bytes as f64 / MB)
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}
