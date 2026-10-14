// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Records in the directory that holds them, reached through a capability
//! confined to that directory.
//!
//! The markers and ledgers this crate keeps beside a store's graph (the last
//! admission, the relation census and its hold, the retained parse record, the
//! history state, the sync state, the manifest) and the config document are
//! records under one directory. They are read and written here through a
//! kin-io `StoreDir`, which resolves every record name beneath that directory
//! and cannot reach anything outside it, so these reads are safe on any path,
//! answer paths included. This module is the one place the crate claims
//! authority for them.

use std::io;
use std::path::Path;

use kin_io::{Grant, StoreDir};

/// The longest record read here. The records are small JSON documents.
const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;

/// The store at `root`, created first when `create` is set.
fn store(root: &Path, create: bool) -> io::Result<StoreDir> {
    let grant = Grant::ambient();
    if create {
        StoreDir::create(root, &grant)
    } else {
        StoreDir::open(root, &grant)
    }
}

/// `path`'s place inside the store at `root`.
fn relative<'a>(root: &Path, path: &'a Path) -> io::Result<&'a Path> {
    path.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is not a record of the store at {}",
                path.display(),
                root.display()
            ),
        )
    })
}

/// Read the record at `path` in the store at `root`. `Ok(None)` when the store
/// or the record does not exist.
pub(crate) fn read(root: &Path, path: &Path) -> io::Result<Option<Vec<u8>>> {
    let store = match store(root, false) {
        Ok(store) => store,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    store.read_record(relative(root, path)?, MAX_RECORD_BYTES)
}

#[cfg(any(unix, windows))]
pub(crate) fn read_retained(
    root: &Path,
    directory: cap_std::fs::Dir,
    name: &Path,
) -> io::Result<Option<Vec<u8>>> {
    StoreDir::from_retained_directory(root, directory).read_record(name, MAX_RECORD_BYTES)
}

/// Replace the record at `path` in the store at `root` atomically and
/// durably, creating the store and the record's directory when they are
/// missing. A reader sees the previous record or this one, never a torn one.
pub(crate) fn write(root: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let store = store(root, true)?;
    let name = relative(root, path)?;
    if let Some(parent) = name
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        store.create_dir_all(parent)?;
    }
    store.write_record(name, bytes)
}

/// Remove the record at `path` in the store at `root`. `Ok(false)` when there
/// was none.
pub(crate) fn remove(root: &Path, path: &Path) -> io::Result<bool> {
    match store(root, false) {
        Ok(store) => store.remove_record(relative(root, path)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_and_stays_inside_its_store() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join(".kin");
        let record = root.join("kindb").join("last-admission");
        assert_eq!(read(&root, &record).unwrap(), None);
        write(&root, &record, b"{}").unwrap();
        assert_eq!(read(&root, &record).unwrap().as_deref(), Some(&b"{}"[..]));
        assert!(remove(&root, &record).unwrap());
        assert!(!remove(&root, &record).unwrap());
        assert!(write(&root, &outer.path().join("elsewhere"), b"x").is_err());
    }
}
