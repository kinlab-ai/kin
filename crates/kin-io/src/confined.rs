// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A directory handle that resolves every path beneath itself.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cap_std::fs::{Dir, OpenOptions};

use crate::Grant;

/// One opened directory. Every path given to it is resolved beneath it by
/// `cap-std`, which refuses absolute paths, `..` past the root, and links that
/// resolve outside it.
#[derive(Debug)]
pub(crate) struct Confined {
    dir: Dir,
    root: PathBuf,
}

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

fn too_long(relative: &Path, root: &Path, max_bytes: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{} under {} is longer than the {max_bytes}-byte limit",
            relative.display(),
            root.display()
        ),
    )
}

impl Confined {
    pub(crate) fn from_retained_directory(root: &Path, dir: Dir) -> Self {
        Self {
            dir,
            root: root.to_path_buf(),
        }
    }

    pub(crate) fn open(root: &Path, _grant: &Grant) -> io::Result<Self> {
        Self::open_held(root)
    }

    /// Create `root` and its missing parents, then open it.
    pub(crate) fn create(root: &Path, _grant: &Grant) -> io::Result<Self> {
        Dir::create_ambient_dir_all(root, cap_std::ambient_authority())?;
        Self::open_held(root)
    }

    /// Create or truncate `relative` and hand back the open handle, for a
    /// writer that streams into it rather than replacing it whole.
    pub(crate) fn create_file(&self, relative: &Path) -> io::Result<std::fs::File> {
        Ok(self.dir.create(relative)?.into_std())
    }

    /// Open `root` for a caller that already holds authority over it, through
    /// a token rather than a fresh grant.
    pub(crate) fn open_held(root: &Path) -> io::Result<Self> {
        let dir = Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
        Ok(Self {
            dir,
            root: root.to_path_buf(),
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn sub(&self, relative: &Path) -> io::Result<Self> {
        Ok(Self {
            dir: self.dir.open_dir(relative)?,
            root: self.root.join(relative),
        })
    }

    pub(crate) fn open_read_file(&self, relative: &Path) -> io::Result<cap_std::fs::File> {
        if !self.dir.symlink_metadata(relative)?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "entry is not a regular file",
            ));
        }
        let file = self.dir.open(relative)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "entry is not a regular file",
            ));
        }
        Ok(file)
    }

    pub(crate) fn symlink_metadata(&self, relative: &Path) -> io::Result<cap_std::fs::Metadata> {
        self.dir.symlink_metadata(relative)
    }

    /// Read `relative` whole, refusing anything longer than `max_bytes`.
    /// `Ok(None)` when nothing exists there.
    pub(crate) fn read_bounded(
        &self,
        relative: &Path,
        max_bytes: u64,
    ) -> io::Result<Option<Vec<u8>>> {
        let file = match self.dir.open(relative) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        // Refuse a long entry before reading it; the bounded read below still
        // holds if it grows in between.
        if file.metadata()?.len() > max_bytes {
            return Err(too_long(relative, &self.root, max_bytes));
        }
        let mut bytes = Vec::new();
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_bytes {
            return Err(too_long(relative, &self.root, max_bytes));
        }
        Ok(Some(bytes))
    }

    /// The target a link at `relative` names, read without following it.
    /// `Ok(None)` when nothing exists there.
    pub(crate) fn link_target(&self, relative: &Path) -> io::Result<Option<PathBuf>> {
        match self.dir.read_link_contents(relative) {
            Ok(target) => Ok(Some(target)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Replace `relative` with `bytes` so a reader sees the old bytes or the
    /// new ones and never a torn write: write a fresh temporary beside it,
    /// flush it, rename it over the target, then flush the directory.
    pub(crate) fn replace(&self, relative: &Path, bytes: &[u8]) -> io::Result<()> {
        let name = relative.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} names no entry", relative.display()),
            )
        })?;
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let mut temporary_name = std::ffi::OsString::from(".");
        temporary_name.push(name);
        temporary_name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        let temporary = parent.join(&temporary_name);

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let written = (|| {
            let mut file = self.dir.open_with(&temporary, &options)?;
            file.write_all(bytes)?;
            kin_blobs::durable::sync_all(&file.into_std())?;
            self.dir.rename(&temporary, &self.dir, relative)
        })();
        if let Err(error) = written {
            let _ = self.dir.remove_file(&temporary);
            return Err(error);
        }
        self.sync_parent(parent)
    }

    /// Replace `relative` with `bytes` atomically but without flushing, for a
    /// record that is rebuilt when lost, such as a cache or a paging cursor:
    /// a reader still never sees a torn write, and the caller does not pay for
    /// a flush to the device.
    pub(crate) fn replace_unflushed(&self, relative: &Path, bytes: &[u8]) -> io::Result<()> {
        let name = relative.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} names no entry", relative.display()),
            )
        })?;
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let mut temporary_name = std::ffi::OsString::from(".");
        temporary_name.push(name);
        temporary_name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        let temporary = parent.join(&temporary_name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let written = (|| {
            let mut file = self.dir.open_with(&temporary, &options)?;
            file.write_all(bytes)?;
            drop(file);
            self.dir.rename(&temporary, &self.dir, relative)
        })();
        if written.is_err() {
            let _ = self.dir.remove_file(&temporary);
        }
        written
    }

    /// Remove `relative`. `Ok(false)` when nothing was there.
    pub(crate) fn remove(&self, relative: &Path) -> io::Result<bool> {
        match self.dir.remove_file(relative) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn create_dir_all(&self, relative: &Path) -> io::Result<()> {
        self.dir.create_dir_all(relative)
    }

    pub(crate) fn kind(&self, relative: &Path) -> io::Result<Option<crate::EntryKind>> {
        match self.dir.symlink_metadata(relative) {
            Ok(metadata) => {
                let kind = metadata.file_type();
                Ok(Some(if kind.is_symlink() {
                    crate::EntryKind::Link
                } else if kind.is_dir() {
                    crate::EntryKind::Directory
                } else if kind.is_file() {
                    crate::EntryKind::Regular
                } else {
                    crate::EntryKind::Other
                }))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The names directly inside `relative`, sorted.
    pub(crate) fn names(&self, relative: &Path) -> io::Result<Vec<std::ffi::OsString>> {
        let entries = if relative.as_os_str().is_empty() {
            self.dir.entries()?
        } else {
            self.dir.read_dir(relative)?
        };
        let mut names = entries
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }

    #[cfg(unix)]
    fn sync_parent(&self, parent: &Path) -> io::Result<()> {
        let directory = if parent.as_os_str().is_empty() {
            self.dir.try_clone()?
        } else {
            self.dir.open_dir(parent)?
        };
        // On Linux a capability directory may be an O_PATH descriptor. It
        // pins the directory for traversal but cannot be flushed (EBADF).
        // Reopen "." for reading through that held capability, never through
        // the ambient root path, so renaming the root cannot redirect the flush.
        kin_blobs::durable::sync_all(&directory.open(".")?.into_std())
    }

    #[cfg(not(unix))]
    fn sync_parent(&self, _parent: &Path) -> io::Result<()> {
        // Windows cannot flush a directory handle; the rename itself is the
        // ordering point there.
        Ok(())
    }
}
