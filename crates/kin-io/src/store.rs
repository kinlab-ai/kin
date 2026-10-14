// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Kin's own records, and the host's configuration and cache directories.

use std::io;
use std::path::Path;

use crate::confined::Confined;
use crate::Grant;

/// One Kin store's own records: markers, journals, cursors, receipts and the
/// daemon's process records.
///
/// Every record name resolves beneath the store directory, so a `StoreDir`
/// cannot reach repository content. That is what makes it safe to hand to
/// code on an answer path.
#[derive(Debug)]
pub struct StoreDir {
    inner: Confined,
}

impl StoreDir {
    /// Adopt a directory capability the caller already retains, without
    /// reopening the mutable path naming it.
    pub fn from_retained_directory(root: &Path, directory: cap_std::fs::Dir) -> Self {
        Self {
            inner: Confined::from_retained_directory(root, directory),
        }
    }

    /// Open the store directory at `root`.
    pub fn open(root: &Path, grant: &Grant) -> io::Result<Self> {
        Ok(Self {
            inner: Confined::open(root, grant)?,
        })
    }

    /// Create the store directory at `root`, with any missing parents, and
    /// open it.
    pub fn create(root: &Path, grant: &Grant) -> io::Result<Self> {
        Ok(Self {
            inner: Confined::create(root, grant)?,
        })
    }

    /// The directory this store was opened at, for messages.
    pub fn root(&self) -> &Path {
        self.inner.root()
    }

    /// The records beneath `name`, as their own store.
    pub fn sub(&self, name: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            inner: self.inner.sub(name.as_ref())?,
        })
    }

    /// Read the record `name`, refusing one longer than `max_bytes`.
    /// `Ok(None)` when the store holds no such record.
    pub fn read_record(
        &self,
        name: impl AsRef<Path>,
        max_bytes: u64,
    ) -> io::Result<Option<Vec<u8>>> {
        self.inner.read_bounded(name.as_ref(), max_bytes)
    }

    /// Replace the record `name` atomically and durably.
    pub fn write_record(&self, name: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
        self.inner.replace(name.as_ref(), bytes)
    }

    /// Replace the record `name` atomically without flushing it to the device:
    /// for a record that is rebuilt when lost, such as a paging cursor or a
    /// cache. A reader never sees a torn write.
    pub fn write_scratch_record(&self, name: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
        self.inner.replace_unflushed(name.as_ref(), bytes)
    }

    /// Remove the record `name`. `Ok(false)` when there was none.
    pub fn remove_record(&self, name: impl AsRef<Path>) -> io::Result<bool> {
        self.inner.remove(name.as_ref())
    }

    /// Whether the store holds a record named `name`.
    pub fn has_record(&self, name: impl AsRef<Path>) -> io::Result<bool> {
        Ok(matches!(
            self.inner.kind(name.as_ref())?,
            Some(crate::EntryKind::Regular)
        ))
    }

    /// Create the record directory `name` and its parents.
    pub fn create_dir_all(&self, name: impl AsRef<Path>) -> io::Result<()> {
        self.inner.create_dir_all(name.as_ref())
    }
}

/// One configuration or cache directory on the host: the user's Kin
/// configuration, an install root, or a model cache.
#[derive(Debug)]
pub struct HostDir {
    inner: Confined,
}

impl HostDir {
    /// Open the host directory at `root`.
    pub fn open(root: &Path, grant: &Grant) -> io::Result<Self> {
        Ok(Self {
            inner: Confined::open(root, grant)?,
        })
    }

    /// Create the host directory at `root`, with any missing parents, and
    /// open it: an output directory a run is about to fill.
    pub fn create(root: &Path, grant: &Grant) -> io::Result<Self> {
        Ok(Self {
            inner: Confined::create(root, grant)?,
        })
    }

    /// Create or truncate `name` and return it open for writing, for output
    /// that is streamed rather than replaced whole, such as a transcript.
    pub fn create_file(&self, name: impl AsRef<Path>) -> io::Result<std::fs::File> {
        self.inner.create_file(name.as_ref())
    }

    /// The directory this was opened at, for messages.
    pub fn root(&self) -> &Path {
        self.inner.root()
    }

    /// Read `name`, refusing anything longer than `max_bytes`. `Ok(None)` when
    /// nothing exists there.
    pub fn read(&self, name: impl AsRef<Path>, max_bytes: u64) -> io::Result<Option<Vec<u8>>> {
        self.inner.read_bounded(name.as_ref(), max_bytes)
    }

    /// Stream a regular file within this host directory without materializing it.
    pub fn open_read_file(&self, name: impl AsRef<Path>) -> io::Result<cap_std::fs::File> {
        self.inner.open_read_file(name.as_ref())
    }

    /// Inspect an entry without following its final symbolic link.
    pub fn symlink_metadata(&self, name: impl AsRef<Path>) -> io::Result<cap_std::fs::Metadata> {
        self.inner.symlink_metadata(name.as_ref())
    }

    /// Replace `name` atomically and durably.
    pub fn write(&self, name: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
        self.inner.replace(name.as_ref(), bytes)
    }

    /// Remove `name`. `Ok(false)` when there was nothing.
    pub fn remove(&self, name: impl AsRef<Path>) -> io::Result<bool> {
        self.inner.remove(name.as_ref())
    }

    /// Whether `name` exists as a regular entry.
    pub fn contains(&self, name: impl AsRef<Path>) -> io::Result<bool> {
        Ok(matches!(
            self.inner.kind(name.as_ref())?,
            Some(crate::EntryKind::Regular)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, StoreDir) {
        let root = tempfile::tempdir().unwrap();
        let store = StoreDir::open(root.path(), &Grant::ambient()).unwrap();
        (root, store)
    }

    #[test]
    fn a_record_round_trips_and_is_removed() {
        let (_root, store) = store();
        assert_eq!(store.read_record("daemon.port", 64).unwrap(), None);
        store.write_record("daemon.port", b"4219").unwrap();
        assert_eq!(
            store.read_record("daemon.port", 64).unwrap().as_deref(),
            Some(&b"4219"[..])
        );
        assert!(store.has_record("daemon.port").unwrap());
        store.write_record("daemon.port", b"4220").unwrap();
        assert_eq!(
            store.read_record("daemon.port", 64).unwrap().as_deref(),
            Some(&b"4220"[..])
        );
        assert!(store.remove_record("daemon.port").unwrap());
        assert!(!store.remove_record("daemon.port").unwrap());
        assert!(!store.has_record("daemon.port").unwrap());
    }

    #[test]
    fn a_scratch_record_round_trips() {
        let (_root, store) = store();
        store
            .write_scratch_record("locate-cursor", b"page-2")
            .unwrap();
        store
            .write_scratch_record("locate-cursor", b"page-3")
            .unwrap();
        assert_eq!(
            store.read_record("locate-cursor", 64).unwrap().as_deref(),
            Some(&b"page-3"[..])
        );
        assert!(store.write_scratch_record("../escaped", b"x").is_err());
    }

    #[test]
    fn a_replace_leaves_no_temporary_behind() {
        let (root, store) = store();
        store.create_dir_all("cursors").unwrap();
        store.write_record("cursors/locate", b"next").unwrap();
        let mut names: Vec<_> = std::fs::read_dir(root.path().join("cursors"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, vec![std::ffi::OsString::from("locate")]);
    }

    #[test]
    fn durable_replacements_flush_the_held_directory_after_its_path_moves() {
        let parent = tempfile::tempdir().unwrap();
        let original = parent.path().join("store");
        let moved = parent.path().join("moved");
        std::fs::create_dir(&original).unwrap();
        let store = StoreDir::open(&original, &Grant::ambient()).unwrap();
        store.create_dir_all("nested").unwrap();
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        for name in ["manifest.json", "nested/state.json"] {
            store.write_record(name, b"first").unwrap();
            store.write_record(name, b"second").unwrap();
            assert_eq!(std::fs::read(moved.join(name)).unwrap(), b"second");
            assert!(!original.join(name).exists());
        }
    }

    #[test]
    fn a_record_longer_than_its_limit_is_refused() {
        let (_root, store) = store();
        store.write_record("journal", &[7u8; 16]).unwrap();
        let error = store.read_record("journal", 15).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(store.read_record("journal", 16).unwrap().unwrap().len(), 16);
    }

    #[test]
    fn a_store_cannot_reach_outside_itself() {
        let repository = tempfile::tempdir().unwrap();
        std::fs::write(repository.path().join("source.py"), b"secret = 1\n").unwrap();
        let kin = repository.path().join(".kin");
        std::fs::create_dir(&kin).unwrap();
        let store = StoreDir::open(&kin, &Grant::ambient()).unwrap();

        assert!(store.read_record("../source.py", 1024).is_err());
        let absolute = repository.path().join("source.py");
        assert!(store.read_record(&absolute, 1024).is_err());
        assert!(store.write_record("../escaped", b"x").is_err());
        assert!(!repository.path().join("escaped").exists());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(repository.path().join("source.py"), kin.join("link"))
                .unwrap();
            assert!(store.read_record("link", 1024).is_err());
        }
    }

    #[test]
    fn a_host_directory_reads_and_writes_beneath_itself() {
        let root = tempfile::tempdir().unwrap();
        let host = HostDir::open(root.path(), &Grant::ambient()).unwrap();
        assert!(!host.contains("telemetry-consent").unwrap());
        host.write("telemetry-consent", b"yes").unwrap();
        assert!(host.contains("telemetry-consent").unwrap());
        assert_eq!(
            host.read("telemetry-consent", 8).unwrap().as_deref(),
            Some(&b"yes"[..])
        );
        assert!(host.read("../outside", 8).is_err());
        assert!(host.remove("telemetry-consent").unwrap());
    }

    #[test]
    fn a_created_host_directory_takes_streamed_output() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let out = root.path().join("runs/one");
        let host = HostDir::create(&out, &Grant::ambient()).unwrap();
        let mut file = host.create_file("transcript.jsonl").unwrap();
        writeln!(file, "{{}}").unwrap();
        drop(file);
        assert_eq!(
            std::fs::read(out.join("transcript.jsonl")).unwrap(),
            b"{}\n"
        );
        assert!(host.create_file("../escaped").is_err());
    }
}
