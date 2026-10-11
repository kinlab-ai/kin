// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! One index of the same bytes at the same path, shared across a process.
//!
//! Indexing a source is a function of its path and its bytes. A commit that
//! changed one function asked for that function again for every other source
//! in the repository: its cross-file linker is built for the commit and
//! restores what each source imports and leaves unresolved from a fresh
//! parse of each. It also parsed the edited source's previous bytes, to
//! compare what other sources bind against, although those bytes were parsed
//! when they were written. In a repository of 83 Python sources, every
//! one-line commit parsed all 83.
//!
//! [`index_source`] keeps the result under the path and the content digest
//! and hands each caller a copy, so bytes are parsed once in a process while
//! their result is kept.
//!
//! # What a kept result can be answered for
//!
//! Only the bytes it was parsed from, at the path it was parsed at, under
//! what its project declared about it (a Rust crate root's crate name), by
//! the one pipeline this build has. The digest is checked against the bytes on
//! every call, kept or not, as the parse checks it. The path is part of the
//! key, and the path alone selects the language adapter. The pipeline is
//! not the caller's: [`index_source`] builds [`IndexPipeline::new`] itself,
//! which has no options, reads no environment and holds the registry of
//! adapters compiled into this build. A pipeline configured any other way,
//! should one ever exist, cannot reach what is kept here, and a process
//! cannot change builds.
//!
//! # How much is kept
//!
//! Two bounds, both held on every insert. A path keeps its three most
//! recently used versions. An edit keeps the bytes it writes and then reads
//! the ones before them, to compare what other sources bind against, so the
//! version read longest ago is always the one from two edits back: with
//! three kept, the one an edit needs is never the one that leaves. A long
//! session editing one source holds three of it, not one per edit. And the
//! source bytes all kept results stand for
//! never exceed [`SHARED_SOURCE_BYTES_MAX`]; past it the results used
//! longest ago leave first. A source larger than that bound is indexed for
//! its caller and not kept. What a kept result holds in memory is a multiple
//! of its source, so the bound is on the bytes that can be counted exactly.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use kin_model::FilePathId;
use kin_parser::project_manifest::{ProjectFiles, UnitContext};

use crate::classifier::{FileClassification, FileClassifier};
use crate::error::{IndexError, Result};
use crate::pipeline::{IndexPipeline, IndexedAny, IndexedFile};

/// Most source bytes whose index is kept at once. A source larger than this
/// is indexed for its caller and not kept.
pub const SHARED_SOURCE_BYTES_MAX: usize = 32 * 1024 * 1024;

/// Versions of one path kept at once. An edit keeps its new bytes and then
/// reads the previous ones, so the least recently used of three is the
/// version from two edits back. With two, keeping the new bytes would push
/// out the previous ones just before they are read.
const VERSIONS_PER_PATH: usize = 3;

type Key = (String, kin_blobs::Hash256, UnitContext);

struct Kept {
    indexed: Arc<IndexedFile>,
    source_bytes: usize,
    used: u64,
}

#[derive(Default)]
struct Shared {
    kept: HashMap<Key, Kept>,
    source_bytes: usize,
    clock: u64,
}

impl Shared {
    fn get(&mut self, key: &Key) -> Option<Arc<IndexedFile>> {
        self.clock += 1;
        let clock = self.clock;
        self.kept.get_mut(key).map(|kept| {
            kept.used = clock;
            Arc::clone(&kept.indexed)
        })
    }

    fn keep(&mut self, key: Key, source_bytes: usize, indexed: Arc<IndexedFile>, limit: usize) {
        if source_bytes > limit {
            return;
        }
        self.clock += 1;
        let path = key.0.clone();
        if let Some(replaced) = self.kept.insert(
            key,
            Kept {
                indexed,
                source_bytes,
                used: self.clock,
            },
        ) {
            self.source_bytes -= replaced.source_bytes;
        }
        self.source_bytes += source_bytes;
        // Versions of this path past the most recently used few leave now.
        loop {
            let mut versions: Vec<(&Key, u64)> = self
                .kept
                .iter()
                .filter(|(other, _)| other.0 == path)
                .map(|(other, kept)| (other, kept.used))
                .collect();
            if versions.len() <= VERSIONS_PER_PATH {
                break;
            }
            versions.sort_by_key(|(_, used)| *used);
            let oldest = versions[0].0.clone();
            if let Some(left) = self.kept.remove(&oldest) {
                self.source_bytes -= left.source_bytes;
            }
        }
        while self.source_bytes > limit {
            let Some(oldest) = self
                .kept
                .iter()
                .min_by_key(|(_, kept)| kept.used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(left) = self.kept.remove(&oldest) {
                self.source_bytes -= left.source_bytes;
            }
        }
    }
}

fn shared() -> &'static Mutex<Shared> {
    static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
    SHARED.get_or_init(Mutex::default)
}

/// [`IndexPipeline::index_file_content_in`]'s indexed file for `source` at
/// `file_id` in the tree whose manifests `project` holds, parsed once in this
/// process while its result is kept.
pub fn index_source(
    file_id: &FilePathId,
    source: &[u8],
    blob_hash: kin_blobs::Hash256,
    project: &dyn ProjectFiles,
) -> Result<IndexedFile> {
    // The parse refuses bytes that are not the digest's, so a kept result is
    // never answered for bytes it was not parsed from.
    let actual = kin_blobs::digest(source);
    if actual != blob_hash {
        return Err(IndexError::SourceDigestMismatch {
            path: file_id.0.clone(),
            expected: blob_hash,
            actual,
        });
    }
    // This build's one pipeline, never a caller's: see the module note.
    let pipeline = IndexPipeline::new();
    let unit = pipeline.unit_context(file_id, project)?;
    let key = (file_id.0.clone(), blob_hash, unit);
    let kept = shared()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key);
    if let Some(kept) = kept {
        return Ok(IndexedFile::clone(&kept));
    }
    let indexed = pipeline
        .index_file_content_with_unit(file_id, source, blob_hash, &key.2)?
        .indexed_file;
    shared()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .keep(
            key,
            source.len(),
            Arc::new(indexed.clone()),
            SHARED_SOURCE_BYTES_MAX,
        );
    Ok(indexed)
}

/// [`IndexPipeline::index_any_content`], with an entity source's index
/// shared the way [`index_source`] shares it. Anything that is not an entity
/// source is indexed for its caller as before.
pub fn index_any(
    file_id: &FilePathId,
    content: &[u8],
    blob_hash: kin_blobs::Hash256,
    project: &dyn ProjectFiles,
) -> Result<IndexedAny> {
    match FileClassifier::classify_with_content(Path::new(&file_id.0), content) {
        FileClassification::EntitySource => Ok(IndexedAny::EntitySource(index_source(
            file_id, content, blob_hash, project,
        )?)),
        _ => IndexPipeline::new().index_any_content_in(file_id, content, blob_hash, project),
    }
}

/// How many times this process has parsed a source at `path`, kept or not:
/// what a test reads to prove that unchanged bytes were not parsed again.
#[cfg(any(test, feature = "test-support"))]
pub fn parses_of(path: &str) -> u64 {
    parses()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(path)
        .copied()
        .unwrap_or(0)
}

#[cfg(any(test, feature = "test-support"))]
fn parses() -> &'static Mutex<HashMap<String, u64>> {
    static PARSES: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    PARSES.get_or_init(Mutex::default)
}

/// Count one real parse of `path`.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn count_parse(path: &str) {
    *parses()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(path.to_string())
        .or_default() += 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    use kin_parser::project_manifest::NoProjectFiles;

    fn indexed(path: &str, source: &str) -> IndexedFile {
        index_source(
            &FilePathId::new(path),
            source.as_bytes(),
            kin_blobs::digest(source.as_bytes()),
            &NoProjectFiles,
        )
        .unwrap()
    }

    /// The same bytes at the same path are parsed once, and every caller gets
    /// a result of its own that equals a fresh parse.
    #[test]
    fn the_same_bytes_at_the_same_path_are_parsed_once() {
        let path = "shared_index_probe/once.py";
        let source = "def first():\n    return 1\n\n\ndef second():\n    return first()\n";
        let one = indexed(path, source);
        assert_eq!(parses_of(path), 1);
        let mut two = indexed(path, source);
        assert_eq!(parses_of(path), 1, "kept bytes were parsed again");
        let names = |file: &IndexedFile| {
            file.entities
                .iter()
                .map(|entity| (entity.id, entity.name.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&one), names(&two));
        assert_eq!(one.relations.len(), two.relations.len());
        assert_eq!(one.blob_hash, two.blob_hash);
        // A caller may change its copy; the next caller's is as parsed.
        two.entities.clear();
        assert_eq!(names(&indexed(path, source)), names(&one));

        // Other bytes at the path, and the same bytes at another path, are
        // each their own parse.
        indexed(path, "def first():\n    return 2\n");
        assert_eq!(parses_of(path), 2);
        let elsewhere = "shared_index_probe/elsewhere.py";
        indexed(elsewhere, source);
        assert_eq!(parses_of(elsewhere), 1);
    }

    /// Bytes that are not the digest's are refused whether or not the
    /// digest's own bytes are kept.
    #[test]
    fn bytes_that_are_not_the_digests_are_refused_even_when_it_is_kept() {
        let path = FilePathId::new("shared_index_probe/digest.py");
        let source = b"def kept():\n    return 1\n";
        let digest = kin_blobs::digest(source);
        index_source(&path, source, digest, &NoProjectFiles).unwrap();
        let refused = index_source(
            &path,
            b"def other():\n    return 2\n",
            digest,
            &NoProjectFiles,
        )
        .unwrap_err();
        assert!(
            matches!(refused, IndexError::SourceDigestMismatch { .. }),
            "{refused}"
        );
    }

    /// A path keeps its latest versions only, so a long session editing one
    /// source does not hold one result per edit. And through any number of
    /// edits, the version an edit reads back, the one before the bytes it
    /// has just kept, is still there.
    #[test]
    fn a_path_keeps_its_latest_versions_and_an_edit_finds_the_one_before() {
        let file = Arc::new(indexed(
            "shared_index_probe/versions.py",
            "def f():\n    return 1\n",
        ));
        let version = |at: u8| {
            (
                "edited.py".to_string(),
                kin_blobs::digest(&[at]),
                UnitContext::default(),
            )
        };
        let other = (
            "other.py".to_string(),
            kin_blobs::digest(b"o"),
            UnitContext::default(),
        );
        let mut shared = Shared::default();
        shared.keep(other.clone(), 10, Arc::clone(&file), usize::MAX);
        // The first edit reads bytes nobody kept, and keeps them.
        shared.keep(version(1), 10, Arc::clone(&file), usize::MAX);
        assert!(shared.get(&version(0)).is_none());
        shared.keep(version(0), 10, Arc::clone(&file), usize::MAX);
        for edit in 2..50 {
            shared.keep(version(edit), 10, Arc::clone(&file), usize::MAX);
            assert!(
                shared.get(&version(edit - 1)).is_some(),
                "edit {edit} lost the bytes before it"
            );
            let of_path = shared
                .kept
                .keys()
                .filter(|key| key.0 == "edited.py")
                .count();
            assert!(of_path <= VERSIONS_PER_PATH, "{of_path} versions kept");
        }
        // Another path's one version is untouched by this path's edits.
        assert!(shared.get(&other).is_some());
        assert_eq!(shared.kept.len(), VERSIONS_PER_PATH + 1);
        assert_eq!(shared.source_bytes, 10 * (VERSIONS_PER_PATH + 1));
        assert!(shared.get(&version(49)).is_some() && shared.get(&version(48)).is_some());
        assert!(shared.get(&version(45)).is_none());
    }

    /// What is kept is bounded by the source bytes it stands for, and the
    /// result used longest ago leaves first.
    #[test]
    fn the_results_used_longest_ago_leave_first() {
        let file = |name: &str| IndexedFile::clone(&indexed(name, "def f():\n    return 1\n"));
        let key = |name: &str| {
            (
                name.to_string(),
                kin_blobs::digest(name.as_bytes()),
                UnitContext::default(),
            )
        };
        let mut shared = Shared::default();
        let limit = 100;
        for name in ["a", "b", "c"] {
            shared.keep(
                key(name),
                40,
                Arc::new(file("shared_index_probe/lru.py")),
                limit,
            );
            // `a` is read again after `b` is kept, so `b` is the oldest.
            if name == "b" {
                assert!(shared.get(&key("a")).is_some());
            }
        }
        assert!(shared.get(&key("b")).is_none());
        assert!(shared.get(&key("a")).is_some() && shared.get(&key("c")).is_some());
        assert_eq!(shared.source_bytes, 80);
        // A source larger than the whole bound is not kept.
        shared.keep(
            key("huge"),
            101,
            Arc::new(file("shared_index_probe/lru.py")),
            limit,
        );
        assert!(shared.get(&key("huge")).is_none());
        assert_eq!(shared.source_bytes, 80);
    }
}
