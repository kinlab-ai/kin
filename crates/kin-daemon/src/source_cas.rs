// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The two stores a publication may find a source body in.
//!
//! Ingestion staging is not authority. Layout documents it as
//! non-authoritative and nothing promises to retain it, while a body the
//! repository has already published is durable in its own CAS. So every
//! publication path that reads a body the current transition did not itself
//! observe reads staging first and repository CAS second, and a store whose
//! staging directory is lost while its authority survives still commits,
//! merges, and admits.
//!
//! The rule lives here rather than at each read because those reads could not
//! miss while every publication was preceded by a scan that rewrote all leaves
//! back into staging. Bounding one tick to what moved removed that accidental
//! standing repair, and the fallback became the only thing keeping the rule
//! true. Spread across separate copies, it was true at the copies that had it
//! and false everywhere else.

use kin_blobs::BlobStore;
use kin_db::{LocalFileBackend, RepositoryAuthorityManager};
use kin_model::Hash256;
use thiserror::Error;

use crate::error::DaemonError;

/// One source body, and which store answered for it.
pub(crate) enum PublishableSource {
    /// Ingestion staging holds the body. A publication that newly references
    /// it must copy it into repository CAS to make it durable.
    Staged(Vec<u8>),
    /// Only repository CAS holds the body. It is already durable there, so no
    /// copy has to move it and staging having dropped it is not a failure.
    AlreadyPublished(Vec<u8>),
}

impl PublishableSource {
    /// The body, wherever it came from. For callers that only measure it.
    pub(crate) fn body(&self) -> &[u8] {
        match self {
            Self::Staged(body) | Self::AlreadyPublished(body) => body,
        }
    }

    /// The body only while a copy still has to move it into repository CAS.
    pub(crate) fn body_to_publish(&self) -> Option<&[u8]> {
        match self {
            Self::Staged(body) => Some(body),
            Self::AlreadyPublished(_) => None,
        }
    }
}

/// Names the store and the size, never the body. These carry user source, and a
/// debug print is the one place a whole file leaks into a log by accident.
impl std::fmt::Debug for PublishableSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (store, body) = match self {
            Self::Staged(body) => ("Staged", body),
            Self::AlreadyPublished(body) => ("AlreadyPublished", body),
        };
        write!(formatter, "{store}({} bytes)", body.len())
    }
}

/// Why a source body a publication needs could not be produced.
#[derive(Debug, Error)]
pub(crate) enum SourceBodyUnavailable {
    /// Repository CAS could not be consulted, so whether it holds the body is
    /// unknown. That is not absence and must never be reported as it: one says
    /// the body is gone, the other says nobody managed to look.
    #[error("read repository source {hash}: {source}")]
    Authority {
        hash: Hash256,
        source: kin_db::KinDbError,
    },

    /// Neither store holds the body. Carries the ingestion refusal so absence
    /// stays matchable as the typed blob error it has always been, rather than
    /// as a sentence a caller would have to parse.
    #[error("source {hash} is absent from both ingestion staging and repository CAS ({source})")]
    Absent {
        hash: Hash256,
        source: kin_blobs::BlobError,
    },
}

impl From<SourceBodyUnavailable> for DaemonError {
    fn from(error: SourceBodyUnavailable) -> Self {
        match error {
            SourceBodyUnavailable::Authority { source, .. } => Self::Graph(source),
            SourceBodyUnavailable::Absent { source, .. } => Self::Blob(source),
        }
    }
}

/// Read one source body from the two stores a publication may find it in.
///
/// Staging answers first because a body observed this tick is there and may
/// not be in repository CAS yet. Repository CAS answers second because a body
/// published by an earlier transition is durable there whether or not its
/// staged copy survived.
pub(crate) fn read_publishable_source(
    blobs: &BlobStore,
    authority: &RepositoryAuthorityManager<LocalFileBackend>,
    hash: Hash256,
) -> Result<PublishableSource, SourceBodyUnavailable> {
    let ingestion_error = match blobs.read(&kin_blobs::Hash256::from_bytes(*hash.as_bytes())) {
        Ok(body) => return Ok(PublishableSource::Staged(body)),
        Err(error) => error,
    };
    match authority
        .load_source_blob(hash)
        .map_err(|source| SourceBodyUnavailable::Authority { hash, source })?
    {
        Some(body) => Ok(PublishableSource::AlreadyPublished(body)),
        None => Err(SourceBodyUnavailable::Absent {
            hash,
            source: ingestion_error,
        }),
    }
}

/// A body read from the two stores a publication may find it in, or from
/// staging alone when no authority is at hand, as `None` where neither holds
/// it.
pub(crate) fn source_reader<'a>(
    blobs: &'a BlobStore,
    authority: Option<&'a RepositoryAuthorityManager<LocalFileBackend>>,
) -> impl Fn(Hash256) -> Option<Vec<u8>> + 'a {
    move |hash| match authority {
        Some(authority) => read_publishable_source(blobs, authority, hash)
            .ok()
            .map(|source| source.body().to_vec()),
        None => blobs.read(&hash).ok(),
    }
}

/// Copy into staging every body the manifests `tree` holds that an adapter
/// names units from read, wherever staging lacks it, from repository CAS: a
/// manifest's own body, and for one held as a link, each link in its chain and
/// the body it leads to, whatever that target is named
/// ([`kin_index::naming_manifest_bodies`]).
///
/// The units beneath such a manifest are named from it wherever they are
/// parsed, and the daemon's own passes read every body from staging. Restoring
/// the few naming manifests before such a pass makes a manifest staging lost
/// fall back to authority, as a commit's source reads do, rather than refuse
/// the units beneath it. Authority is opened only when staging lacks one, and
/// a manifest neither store yields is left for the pass to refuse by name.
/// Returns how many were restored.
pub(crate) fn restage_naming_manifests(
    state: &crate::state::DaemonState,
    tree: &kin_model::ResolvedTree,
) -> usize {
    let staged = |hash: Hash256| state.blobs.read(&hash).ok();
    let missing = |state: &crate::state::DaemonState| -> Vec<Hash256> {
        kin_index::naming_manifest_bodies(tree, &staged)
            .into_iter()
            .filter(|hash| !state.blobs.exists(hash).unwrap_or(false))
            .collect()
    };
    let mut wanted = missing(state);
    if wanted.is_empty() {
        return 0;
    }
    let authority =
        match crate::local_repository_authority::LocalRepositoryAuthorityContext::from_state(state)
            .map_err(|error| error.to_string())
            .and_then(|context| context.open().map_err(|error| error.to_string()))
        {
            Ok(authority) => authority,
            Err(error) => {
                tracing::warn!(%error, "a naming manifest is missing from staging and repository authority could not be opened to restore it");
                return 0;
            }
        };
    let mut restored = 0;
    let mut tried = std::collections::HashSet::new();
    // A restored link can lead to a body staging also lacks, so the chains
    // are followed again until nothing new is wanted.
    while !wanted.is_empty() {
        for hash in wanted {
            if !tried.insert(hash) {
                continue;
            }
            match authority.load_source_blob(hash) {
                Ok(Some(body)) if state.blobs.write(&body).is_ok() => restored += 1,
                Ok(_) | Err(_) => {
                    tracing::warn!(%hash, "a naming manifest is in neither staging nor repository authority")
                }
            }
        }
        wanted = missing(state)
            .into_iter()
            .filter(|hash| !tried.contains(hash))
            .collect();
    }
    restored
}

/// [`source_reader`] for a caller holding repository authority unopened:
/// staging answers first, and authority is opened once, only when staging
/// lacks a body, since opening it reads the whole persisted authority.
pub(crate) fn source_reader_opening<'a>(
    blobs: &'a BlobStore,
    context: &'a crate::local_repository_authority::LocalRepositoryAuthorityContext,
) -> impl Fn(Hash256) -> Option<Vec<u8>> + 'a {
    let opened = std::cell::OnceCell::new();
    move |hash| {
        blobs.read(&hash).ok().or_else(|| {
            opened
                .get_or_init(|| context.open().ok())
                .as_ref()
                .and_then(|authority: &RepositoryAuthorityManager<LocalFileBackend>| {
                    authority.load_source_blob(hash).ok().flatten()
                })
        })
    }
}

/// The project manifests of `graph`'s tree, each read through
/// [`source_reader`]. A manifest the tree holds whose body neither store
/// yields cannot be read, and refuses the units beneath it rather than naming
/// them as if it were absent; a link at a manifest path is followed inside the
/// tree ([`kin_index::GraphProjectFiles`]).
pub(crate) fn graph_manifests<'a, G: kin_model::GraphStore + ?Sized>(
    graph: &'a G,
    blobs: &'a BlobStore,
    authority: Option<&'a RepositoryAuthorityManager<LocalFileBackend>>,
) -> kin_index::GraphProjectFiles<'a, G, impl Fn(Hash256) -> Option<Vec<u8>> + 'a> {
    kin_index::GraphProjectFiles::with_reader(graph, source_reader(blobs, authority))
}

/// The project manifests of one exact tree, each read through
/// [`source_reader`].
pub(crate) fn tree_manifests<'a>(
    tree: &'a kin_model::ResolvedTree,
    blobs: &'a BlobStore,
    authority: Option<&'a RepositoryAuthorityManager<LocalFileBackend>>,
) -> kin_index::TreeProjectFiles<'a, impl Fn(Hash256) -> Option<Vec<u8>> + 'a> {
    kin_index::TreeProjectFiles::new(tree, source_reader(blobs, authority))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn authority(init: &kin_core::InitResult) -> RepositoryAuthorityManager<LocalFileBackend> {
        RepositoryAuthorityManager::open(
            init.repository_id.clone(),
            Arc::new(LocalFileBackend::new(init.layout.kindb_dir())),
        )
        .unwrap()
    }

    /// A body the current transition just observed is in staging and not yet in
    /// repository CAS, so it has to be reported as still needing a copy.
    #[test]
    fn a_staged_body_is_reported_as_still_needing_publication() {
        let root = tempfile::tempdir().unwrap();
        let init = kin_core::init(root.path()).unwrap();
        let blobs = BlobStore::new(init.layout.ingest_cas_dir()).unwrap();
        let authority = authority(&init);

        let digest = blobs.write(b"target/\n").unwrap();
        let hash = Hash256::from_bytes(digest.0);

        let source = read_publishable_source(&blobs, &authority, hash).unwrap();
        assert_eq!(source.body(), b"target/\n");
        assert_eq!(source.body_to_publish(), Some(b"target/\n".as_slice()));
    }

    /// The read this whole module exists for: staging is gone, authority is
    /// intact, and the body is still produced, with no copy asked for because
    /// repository CAS already owns it.
    #[test]
    fn a_published_body_survives_losing_ingestion_staging() {
        let root = tempfile::tempdir().unwrap();
        let init = kin_core::init(root.path()).unwrap();
        let blobs = BlobStore::new(init.layout.ingest_cas_dir()).unwrap();
        let authority = authority(&init);

        let digest = blobs.write(b"target/\n").unwrap();
        let hash = Hash256::from_bytes(digest.0);
        authority.save_source_blob(hash, b"target/\n").unwrap();

        std::fs::remove_dir_all(init.layout.ingest_cas_dir()).unwrap();
        let blobs = BlobStore::new(init.layout.ingest_cas_dir()).unwrap();
        assert!(
            blobs
                .read(&kin_blobs::Hash256::from_bytes(*hash.as_bytes()))
                .is_err(),
            "the fixture only proves anything while the staged body is genuinely gone"
        );

        let source = read_publishable_source(&blobs, &authority, hash).unwrap();
        assert_eq!(source.body(), b"target/\n");
        assert_eq!(
            source.body_to_publish(),
            None,
            "a body repository CAS already owns needs no copy"
        );
    }

    /// The control that keeps the fallback from swallowing a real loss. A body
    /// neither store holds still fails, and fails as the typed blob absence a
    /// caller can match on rather than as a sentence.
    #[test]
    fn a_body_neither_store_holds_fails_as_the_typed_blob_absence() {
        let root = tempfile::tempdir().unwrap();
        let init = kin_core::init(root.path()).unwrap();
        let blobs = BlobStore::new(init.layout.ingest_cas_dir()).unwrap();
        let authority = authority(&init);

        let hash = Hash256::from_bytes([7; 32]);
        let error = read_publishable_source(&blobs, &authority, hash)
            .expect_err("a body no store holds cannot be produced");
        assert!(
            matches!(error, SourceBodyUnavailable::Absent { .. }),
            "absence is absence, not an authority read failure: {error}"
        );
        assert!(
            matches!(
                DaemonError::from(error),
                DaemonError::Blob(kin_blobs::BlobError::NotFound { .. })
            ),
            "a caller matching on the typed blob absence still sees it"
        );
    }
}
