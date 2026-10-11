// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A working tree, read by ingestion and written by projection.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use crate::confined::Confined;
use crate::{Grant, WorktreeRoot};

/// What sits at a path in a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Regular,
    Directory,
    Link,
    Other,
}

/// Read access to a working tree, for the boundaries that turn it into graph
/// truth: ingestion, import, reconcile and migration.
///
/// Answer paths are never handed one. A semantic answer that needs repository
/// content reads it from the graph.
#[derive(Debug)]
pub struct WorktreeSource {
    inner: Confined,
}

impl WorktreeSource {
    /// Open the working tree at `root`.
    pub fn open(root: &Path, grant: &Grant) -> io::Result<Self> {
        Ok(Self {
            inner: Confined::open(root, grant)?,
        })
    }

    /// A confined view of the tree an unconfined token names, for reads few
    /// enough that per-path resolution costs nothing that matters. The token
    /// already holds the authority this view narrows.
    pub fn from_root(root: &WorktreeRoot) -> io::Result<Self> {
        Ok(Self {
            inner: Confined::open_held(root.path())?,
        })
    }

    /// The directory this was opened at, for messages.
    pub fn root(&self) -> &Path {
        self.inner.root()
    }

    /// Read the entry at `relative`, refusing one longer than `max_bytes`.
    /// `Ok(None)` when nothing exists there.
    pub fn read(&self, relative: impl AsRef<Path>, max_bytes: u64) -> io::Result<Option<Vec<u8>>> {
        self.inner.read_bounded(relative.as_ref(), max_bytes)
    }

    /// What sits at `relative`, without following a link. `Ok(None)` when
    /// nothing does.
    pub fn kind(&self, relative: impl AsRef<Path>) -> io::Result<Option<EntryKind>> {
        self.inner.kind(relative.as_ref())
    }

    /// The names directly inside the directory at `relative`, sorted. An empty
    /// path lists the root.
    pub fn names(&self, relative: impl AsRef<Path>) -> io::Result<Vec<OsString>> {
        self.inner.names(relative.as_ref())
    }

    /// The target the link at `relative` names, exactly as stored and without
    /// following it. `Ok(None)` when nothing exists there.
    pub fn link_target(&self, relative: impl AsRef<Path>) -> io::Result<Option<PathBuf>> {
        self.inner.link_target(relative.as_ref())
    }
}

/// Write access to a working tree, for projection: materializing what the
/// graph holds back into the tree.
#[derive(Debug)]
pub struct WorktreeProjection {
    inner: Confined,
}

impl WorktreeProjection {
    /// Open the working tree at `root`.
    pub fn open(root: &Path, grant: &Grant) -> io::Result<Self> {
        Ok(Self {
            inner: Confined::open(root, grant)?,
        })
    }

    /// The directory this was opened at, for messages.
    pub fn root(&self) -> &Path {
        self.inner.root()
    }

    /// Replace the entry at `relative` with `bytes`, atomically and durably.
    pub fn write(&self, relative: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
        self.inner.replace(relative.as_ref(), bytes)
    }

    /// Create the directory at `relative` and its parents.
    pub fn create_dir_all(&self, relative: impl AsRef<Path>) -> io::Result<()> {
        self.inner.create_dir_all(relative.as_ref())
    }

    /// Remove the entry at `relative`. `Ok(false)` when there was none.
    pub fn remove(&self, relative: impl AsRef<Path>) -> io::Result<bool> {
        self.inner.remove(relative.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_hands_out_its_path_and_a_confined_view_of_it() {
        let outer = tempfile::tempdir().unwrap();
        std::fs::write(outer.path().join("main.rs"), b"fn main() {}\n").unwrap();
        let root = WorktreeRoot::new(outer.path(), &Grant::ambient());
        assert_eq!(root.path(), outer.path());
        assert_eq!(
            WorktreeSource::from_root(&root)
                .unwrap()
                .read("main.rs", 64)
                .unwrap()
                .as_deref(),
            Some(&b"fn main() {}\n"[..])
        );
    }

    #[test]
    fn a_source_reads_and_lists_but_stays_inside_its_tree() {
        let outer = tempfile::tempdir().unwrap();
        let tree = outer.path().join("tree");
        std::fs::create_dir_all(tree.join("pkg")).unwrap();
        std::fs::write(tree.join("pkg/mod.py"), b"x = 1\n").unwrap();
        std::fs::write(outer.path().join("outside"), b"no").unwrap();

        let source = WorktreeSource::open(&tree, &Grant::ambient()).unwrap();
        assert_eq!(source.names("").unwrap(), vec![OsString::from("pkg")]);
        assert_eq!(source.kind("pkg").unwrap(), Some(EntryKind::Directory));
        assert_eq!(source.kind("pkg/mod.py").unwrap(), Some(EntryKind::Regular));
        assert_eq!(source.kind("pkg/absent.py").unwrap(), None);
        assert_eq!(
            source.read("pkg/mod.py", 64).unwrap().as_deref(),
            Some(&b"x = 1\n"[..])
        );
        assert!(source.read("../outside", 64).is_err());
        assert!(source.read("pkg/mod.py", 3).is_err());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("mod.py", tree.join("pkg/alias.py")).unwrap();
            assert_eq!(source.kind("pkg/alias.py").unwrap(), Some(EntryKind::Link));
            assert_eq!(
                source.link_target("pkg/alias.py").unwrap(),
                Some(std::path::PathBuf::from("mod.py"))
            );
            assert_eq!(source.link_target("pkg/absent.py").unwrap(), None);
        }
    }

    #[test]
    fn a_projection_writes_creates_and_removes_inside_its_tree() {
        let outer = tempfile::tempdir().unwrap();
        let tree = outer.path().join("tree");
        std::fs::create_dir(&tree).unwrap();

        let projection = WorktreeProjection::open(&tree, &Grant::ambient()).unwrap();
        projection.create_dir_all("src/app").unwrap();
        projection
            .write("src/app/main.rs", b"fn main() {}\n")
            .unwrap();
        assert_eq!(
            std::fs::read(tree.join("src/app/main.rs")).unwrap(),
            b"fn main() {}\n"
        );
        assert!(projection.remove("src/app/main.rs").unwrap());
        assert!(!projection.remove("src/app/main.rs").unwrap());
        assert!(projection.write("../escaped", b"x").is_err());
        assert!(!outer.path().join("escaped").exists());
    }
}
