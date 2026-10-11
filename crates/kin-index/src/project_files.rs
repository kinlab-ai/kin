// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The project manifests of the tree a unit is indexed from.
//!
//! An adapter that names a unit from its project, as Rust names a crate root
//! by its crate, asks for the manifests above the unit by path
//! ([`kin_parser::project_manifest::ProjectFiles`]). Each reader here answers
//! from graph-owned truth: a graph's tree or one exact resolved tree, with the
//! bodies read from a content store. Every surface that derives a unit and
//! every one that checks a derivation reads the same tree the same way, so a
//! manifest is never read one way when a unit is named and another way when
//! the name is checked.
//!
//! A reader answers for a manifest path alone
//! ([`kin_parser::project_manifest::is_manifest_path`]), in three ways: the
//! tree holds no manifest there, it holds this text, or it holds a manifest
//! whose text cannot be produced. The last refuses the units beneath it rather
//! than naming them as if the manifest were absent: a body missing from the
//! store, or bytes that are not UTF-8. A link at a manifest path is followed
//! to its target inside the tree, as Cargo follows it, and one that leads
//! nowhere in the tree reads as no manifest.
//!
//! A reader keeps what it read, so the sources of one operation share the
//! few manifests above them. It is built for one operation over one tree and
//! dropped with it. A manifest body read from a content store is also kept
//! for the process by its digest, which names those exact bytes in any tree,
//! because a commit reads each of its sources through a reader of its own.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use kin_blobs::BlobStore;
use kin_model::{
    EntityFilter, EntityKind, FilePathId, GraphStore, RepoPath, ResolvedTree, TreeEntry,
};
use kin_parser::project_manifest::{is_manifest_path, ProjectFiles, UnreadableManifest};

use crate::error::{IndexError, Result};

/// What a reader answers for one manifest path.
type Answer = std::result::Result<Option<Arc<str>>, UnreadableManifest>;

/// Manifests already read, by path, including the paths that held none.
#[derive(Default)]
struct Kept(Mutex<HashMap<String, Answer>>);

impl Kept {
    fn read(&self, path: &str, read: impl FnOnce() -> Answer) -> Answer {
        if !is_manifest_path(path) {
            return Ok(None);
        }
        if let Some(kept) = self.0.lock().ok().and_then(|kept| kept.get(path).cloned()) {
            return kept;
        }
        let answer = read();
        if let Ok(mut kept) = self.0.lock() {
            kept.insert(path.to_string(), answer.clone());
        }
        answer
    }
}

fn unreadable(path: &str, reason: impl Into<String>) -> UnreadableManifest {
    UnreadableManifest {
        path: path.to_string(),
        reason: reason.into(),
    }
}

/// Links followed from a manifest path before it reads as absent.
const LINK_HOPS: usize = 8;

/// Where the entry the tree holds at a manifest path leads.
///
/// A link is followed to its target inside the tree, the way Cargo follows a
/// symlinked `Cargo.toml` as Bazel and cargo-raze lay them out, through
/// `lookup`, which answers the tree's entry at a repository-relative path. A
/// link whose target leaves the tree, is missing, or loops leads nowhere.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Followed {
    /// The body of every link followed, in order.
    pub(crate) links: Vec<kin_blobs::Hash256>,
    /// Each path a followed link names, in order, whether or not the tree
    /// holds anything there.
    pub(crate) reads: Vec<String>,
    /// The path and body the manifest path reads, or `None` where it leads
    /// nowhere in the tree.
    pub(crate) body: Option<(String, kin_blobs::Hash256)>,
}

/// Follow the entry `entry` the tree holds at `path`.
pub(crate) fn follow_manifest(
    path: &str,
    entry: &TreeEntry,
    lookup: &dyn Fn(&str) -> Option<TreeEntry>,
    read: &dyn Fn(kin_blobs::Hash256) -> Option<Vec<u8>>,
) -> Followed {
    let mut followed = Followed::default();
    let mut at = path.to_string();
    let mut entry = *entry;
    for _ in 0..LINK_HOPS {
        match entry {
            TreeEntry::Blob { hash, .. } => {
                followed.body = Some((at, hash));
                return followed;
            }
            TreeEntry::Gitlink { .. } => return followed,
            TreeEntry::Symlink { target_blob } => {
                followed.links.push(target_blob);
                let Some(target) =
                    read(target_blob).and_then(|bytes| String::from_utf8(bytes).ok())
                else {
                    return followed;
                };
                let Some(resolved) = link_target(&at, &target) else {
                    return followed;
                };
                followed.reads.push(resolved.clone());
                let Some(next) = lookup(&resolved) else {
                    return followed;
                };
                at = resolved;
                entry = next;
            }
        }
    }
    followed
}

/// Every body the naming manifests of `tree` read
/// ([`kin_parser::project_manifest::is_naming_manifest_path`]), once each: the
/// body of one held as a body, and for one held as a link, the body of each
/// link in its chain and the body it leads to inside the tree. `read` reads a
/// link's body to follow it, so a chain stops at the first link `read` cannot
/// produce, which is still listed.
pub fn naming_manifest_bodies(
    tree: &ResolvedTree,
    read: &dyn Fn(kin_blobs::Hash256) -> Option<Vec<u8>>,
) -> Vec<kin_blobs::Hash256> {
    let lookup = |at: &str| {
        let at = RepoPath::from_utf8(at.to_string()).ok()?;
        Some(tree.artifact_at_path(&at)?.entry)
    };
    let mut bodies = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for artifact in tree.artifacts_by_path() {
        let Some(path) = artifact.path.as_utf8() else {
            continue;
        };
        if !kin_parser::project_manifest::is_naming_manifest_path(path) {
            continue;
        }
        let followed = follow_manifest(path, &artifact.entry, &lookup, read);
        for hash in followed
            .links
            .into_iter()
            .chain(followed.body.map(|(_, hash)| hash))
        {
            if seen.insert(hash) {
                bodies.push(hash);
            }
        }
    }
    bodies
}

/// The text of the manifest the tree holds at `path` under `entry`, following
/// links ([`follow_manifest`]). A link that leads nowhere in the tree reads as
/// no manifest, so the units beneath it are named from their paths. Only a
/// body the tree names that cannot be read is unreadable.
fn entry_text(
    path: &str,
    entry: &TreeEntry,
    lookup: &dyn Fn(&str) -> Option<TreeEntry>,
    read: &dyn Fn(kin_blobs::Hash256) -> Option<Vec<u8>>,
) -> Answer {
    match follow_manifest(path, entry, lookup, read).body {
        Some((_, hash)) => body_text(path, hash, read),
        None => Ok(None),
    }
}

/// The repository-relative path a link at `link` naming `target` points to,
/// or `None` for one that leaves the repository.
fn link_target(link: &str, target: &str) -> Option<String> {
    if target.is_empty() || target.starts_with('/') || target.contains('\\') {
        return None;
    }
    let mut parts: Vec<&str> = link.split('/').collect();
    parts.pop();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The UTF-8 text of the body stored under `hash`, read through `read` only
/// when this process has not read those bytes already.
fn body_text(
    path: &str,
    hash: kin_blobs::Hash256,
    read: &dyn Fn(kin_blobs::Hash256) -> Option<Vec<u8>>,
) -> Answer {
    static TEXTS: OnceLock<Mutex<HashMap<kin_blobs::Hash256, Arc<str>>>> = OnceLock::new();
    const KEPT: usize = 512;
    let texts = TEXTS.get_or_init(Default::default);
    if let Some(kept) = texts
        .lock()
        .ok()
        .and_then(|texts| texts.get(&hash).cloned())
    {
        return Ok(Some(kept));
    }
    let bytes = read(hash)
        .ok_or_else(|| unreadable(path, format!("its body {hash} is not in the store")))?;
    // A body that is not the digest's never stands for it.
    if kin_blobs::digest(&bytes) != hash {
        return Err(unreadable(path, format!("the store's body is not {hash}")));
    }
    let text: Arc<str> = String::from_utf8(bytes)
        .map_err(|_| unreadable(path, "its bytes are not UTF-8"))?
        .into();
    if let Ok(mut texts) = texts.lock() {
        if texts.len() >= KEPT {
            texts.clear();
        }
        texts.insert(hash, text.clone());
    }
    Ok(Some(text))
}

/// A content-store read that answers `None` for a body it cannot produce.
pub type BlobReader<'a> = Box<dyn Fn(kin_blobs::Hash256) -> Option<Vec<u8>> + Send + Sync + 'a>;

fn blob_reader(blobs: &BlobStore) -> BlobReader<'_> {
    Box::new(move |hash| blobs.read(&hash).ok())
}

/// The manifests of a graph's current tree, with their bytes read through
/// `read`.
pub struct GraphProjectFiles<'a, G: ?Sized, R> {
    graph: &'a G,
    read: R,
    kept: Kept,
}

impl<'a, G: GraphStore + ?Sized> GraphProjectFiles<'a, G, BlobReader<'a>> {
    /// The manifests of `graph`'s tree, read from the content store `blobs`.
    pub fn new(graph: &'a G, blobs: &'a BlobStore) -> Self {
        Self::with_reader(graph, blob_reader(blobs))
    }
}

impl<'a, G: GraphStore + ?Sized, R: Fn(kin_blobs::Hash256) -> Option<Vec<u8>>>
    GraphProjectFiles<'a, G, R>
{
    /// The manifests of `graph`'s tree, read through `read`.
    pub fn with_reader(graph: &'a G, read: R) -> Self {
        Self {
            graph,
            read,
            kept: Kept::default(),
        }
    }
}

impl<G: GraphStore + ?Sized, R: Fn(kin_blobs::Hash256) -> Option<Vec<u8>>> ProjectFiles
    for GraphProjectFiles<'_, G, R>
{
    fn manifest_reads(&self, path: &str) -> Vec<String> {
        if !is_manifest_path(path) {
            return Vec::new();
        }
        let Some(entry) = self
            .graph
            .get_tree_entry(&FilePathId::new(path))
            .ok()
            .flatten()
        else {
            return Vec::new();
        };
        let lookup = |at: &str| {
            self.graph
                .get_tree_entry(&FilePathId::new(at))
                .ok()
                .flatten()
        };
        follow_manifest(path, &entry, &lookup, &self.read).reads
    }

    fn manifest(&self, path: &str) -> Answer {
        self.kept.read(path, || {
            let entry = self
                .graph
                .get_tree_entry(&FilePathId::new(path))
                .map_err(|error| unreadable(path, format!("the graph's tree: {error}")))?;
            let lookup = |at: &str| {
                self.graph
                    .get_tree_entry(&FilePathId::new(at))
                    .ok()
                    .flatten()
            };
            match entry {
                Some(entry) => entry_text(path, &entry, &lookup, &self.read),
                None => Ok(None),
            }
        })
    }
}

/// The manifests of one exact resolved tree, with their bytes read through
/// `read`.
pub struct TreeProjectFiles<'a, R> {
    tree: &'a ResolvedTree,
    read: R,
    kept: Kept,
}

impl<'a> TreeProjectFiles<'a, BlobReader<'a>> {
    /// The manifests of `tree`, read from the content store `blobs`.
    pub fn from_blobs(tree: &'a ResolvedTree, blobs: &'a BlobStore) -> Self {
        Self::new(tree, blob_reader(blobs))
    }
}

impl<'a, R: Fn(kin_blobs::Hash256) -> Option<Vec<u8>>> TreeProjectFiles<'a, R> {
    pub fn new(tree: &'a ResolvedTree, read: R) -> Self {
        Self {
            tree,
            read,
            kept: Kept::default(),
        }
    }
}

impl<R: Fn(kin_blobs::Hash256) -> Option<Vec<u8>>> ProjectFiles for TreeProjectFiles<'_, R> {
    fn manifest_reads(&self, path: &str) -> Vec<String> {
        if !is_manifest_path(path) {
            return Vec::new();
        }
        let lookup = |at: &str| {
            let at = RepoPath::from_utf8(at.to_string()).ok()?;
            Some(self.tree.artifact_at_path(&at)?.entry)
        };
        let Some(entry) = lookup(path) else {
            return Vec::new();
        };
        follow_manifest(path, &entry, &lookup, &self.read).reads
    }

    fn manifest(&self, path: &str) -> Answer {
        self.kept.read(path, || {
            let lookup = |at: &str| {
                let at = RepoPath::from_utf8(at.to_string()).ok()?;
                Some(self.tree.artifact_at_path(&at)?.entry)
            };
            let Ok(repo_path) = RepoPath::from_utf8(path.to_string()) else {
                return Ok(None);
            };
            match self.tree.artifact_at_path(&repo_path) {
                Some(artifact) => entry_text(path, &artifact.entry, &lookup, &self.read),
                None => Ok(None),
            }
        })
    }
}

/// The entity sources under the directories of the manifests `changed` names
/// whose own module a parse under `project` would name otherwise than `graph`
/// holds it. Only a manifest an adapter names units from is read for this
/// ([`kin_parser::project_manifest::is_naming_manifest_path`]), and a change to
/// a path that is no such manifest and could not hold one's text reaches for
/// nothing. A manifest held as a link reads through its target, so a change at
/// any path a link's chain reads ([`ProjectFiles::manifest_reads`]) sweeps the
/// directory of that link as well.
///
/// A language can name a unit from its project, as Rust names a crate root by
/// the crate its Cargo manifest declares, so a manifest edit can rename units
/// whose own bytes did not change: renaming `[package] name` renames the
/// crate's root, and moving `[lib] path` makes one source the root and leaves
/// the old root a plain module. Each source the change can reach is named
/// under `project` without parsing it ([`crate::IndexPipeline::unit_context`])
/// and compared with the module the graph holds for it. A source whose
/// language names its units some other way, or that the graph holds no module
/// for, is not returned. A manifest `project` cannot read refuses the answer.
pub fn sources_renamed_by_manifests<G: GraphStore + ?Sized>(
    graph: &G,
    tree: &ResolvedTree,
    project: &dyn ProjectFiles,
    changed: &[&str],
) -> Result<Vec<FilePathId>> {
    let parent = |path: &str| -> String {
        path.rsplit_once('/')
            .map_or("", |(directory, _)| directory)
            .to_string()
    };
    let changed: std::collections::HashSet<&str> = changed
        .iter()
        .copied()
        .filter(|path| kin_parser::project_manifest::could_hold_naming_manifest(path))
        .collect();
    if changed.is_empty() {
        return Ok(Vec::new());
    }
    let mut directories: Vec<String> = changed
        .iter()
        .filter(|path| kin_parser::project_manifest::is_naming_manifest_path(path))
        .map(|path| parent(path))
        .collect();
    for artifact in tree.artifacts_by_path() {
        let Some(path) = artifact.path.as_utf8() else {
            continue;
        };
        if matches!(artifact.entry, TreeEntry::Symlink { .. })
            && kin_parser::project_manifest::is_naming_manifest_path(path)
            && project
                .manifest_reads(path)
                .iter()
                .any(|read| changed.contains(read.as_str()))
        {
            directories.push(parent(path));
        }
    }
    if directories.is_empty() {
        return Ok(Vec::new());
    }
    let pipeline = crate::IndexPipeline::new();
    let mut renamed = Vec::new();
    for artifact in tree.artifacts_by_path() {
        let Some(path) = artifact.path.as_utf8() else {
            continue;
        };
        let under = directories.iter().any(|directory| {
            directory.is_empty()
                || path
                    .strip_prefix(directory)
                    .is_some_and(|rest| rest.starts_with('/'))
        });
        if !under || !matches!(artifact.entry, TreeEntry::Blob { .. }) {
            continue;
        }
        let file = FilePathId::new(path);
        let Some(name) = pipeline.unit_context(&file, project)?.module_name else {
            continue;
        };
        let held = graph
            .query_entities(&EntityFilter {
                file_path: Some(file.clone()),
                kinds: Some(vec![EntityKind::Module]),
                ..Default::default()
            })
            .map_err(|error| IndexError::Graph(error.to_string()))?;
        if held
            .iter()
            .find(|entity| kin_model::is_file_module_surface(entity))
            .is_some_and(|surface| surface.name != name)
        {
            renamed.push(file);
        }
    }
    Ok(renamed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::{ArtifactId, Hash256, ResolvedArtifact};

    fn tree(entries: &[(&str, TreeEntry)]) -> ResolvedTree {
        ResolvedTree::from_artifacts(entries.iter().map(|(path, entry)| {
            ResolvedArtifact::new(
                ArtifactId::new(),
                RepoPath::from_utf8(path.to_string()).unwrap(),
                *entry,
            )
        }))
        .unwrap()
    }

    /// A tree answers three ways for a manifest path, and nothing for any
    /// other path. A link is followed to its target inside the tree, and one
    /// that leaves the tree, dangles or loops reads as no manifest.
    #[test]
    fn a_tree_answers_for_its_manifests_three_ways() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStore::new(dir.path().join("cas")).unwrap();
        let manifest = blobs.write(b"[package]\nname = \"kin-db\"\n").unwrap();
        let missing = Hash256::from_bytes([7; 32]);
        let link = |target: &str| TreeEntry::Symlink {
            target_blob: blobs.write(target.as_bytes()).unwrap(),
        };
        let tree = tree(&[
            ("crates/db/Cargo.toml", TreeEntry::blob(manifest, false)),
            ("crates/db/src/lib.rs", TreeEntry::blob(manifest, false)),
            ("crates/lost/Cargo.toml", TreeEntry::blob(missing, false)),
            ("crates/link/Cargo.toml", link("../db/Cargo.toml")),
            (
                "crates/out/Cargo.toml",
                link("../../../elsewhere/Cargo.toml"),
            ),
            ("crates/dangling/Cargo.toml", link("../nowhere/Cargo.toml")),
            ("crates/loop/Cargo.toml", link("Cargo.toml")),
        ]);
        let project = TreeProjectFiles::from_blobs(&tree, &blobs);
        let kin_db = |path: &str| {
            project
                .manifest(path)
                .unwrap()
                .is_some_and(|text| text.contains("kin-db"))
        };
        assert!(kin_db("crates/db/Cargo.toml"));
        assert!(kin_db("crates/link/Cargo.toml"));
        assert_eq!(project.manifest("crates/db/src/lib.rs"), Ok(None));
        assert_eq!(project.manifest("Cargo.toml"), Ok(None));
        for absent in [
            "crates/out/Cargo.toml",
            "crates/dangling/Cargo.toml",
            "crates/loop/Cargo.toml",
        ] {
            assert_eq!(project.manifest(absent), Ok(None), "{absent}");
        }
        assert_eq!(
            project.manifest("crates/lost/Cargo.toml").unwrap_err().path,
            "crates/lost/Cargo.toml"
        );

        // A link reads through the path its target names, present or not.
        assert_eq!(
            project.manifest_reads("crates/link/Cargo.toml"),
            vec!["crates/db/Cargo.toml".to_string()]
        );
        assert_eq!(
            project.manifest_reads("crates/dangling/Cargo.toml"),
            vec!["crates/nowhere/Cargo.toml".to_string()]
        );
        assert!(project.manifest_reads("crates/db/Cargo.toml").is_empty());
        assert!(project.manifest_reads("crates/out/Cargo.toml").is_empty());
        // Every body the naming manifests read: their own, each link's, and
        // the body a link leads to.
        let read = |hash: Hash256| blobs.read(&hash).ok();
        let bodies = naming_manifest_bodies(&tree, &read);
        assert!(bodies.contains(&manifest));
        assert!(bodies.contains(&missing));
        let TreeEntry::Symlink { target_blob } = link("../db/Cargo.toml") else {
            unreachable!()
        };
        assert!(bodies.contains(&target_blob));
        assert_eq!(
            bodies.len(),
            bodies
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
        );
    }
}
