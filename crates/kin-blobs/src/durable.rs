// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! File flushes for Kin stores.
//!
//! Every place Kin makes a write durable calls [`sync_all`] or [`sync_data`]
//! here instead of the methods on [`std::fs::File`]. In a release build, and
//! by default in every build, they are exactly those methods.
//!
//! This module exists for test speed. On Apple platforms the standard library
//! flushes with `fcntl(F_FULLFSYNC)`, which empties the drive's whole write
//! cache. On an Apple SSD that costs about 5 ms a file, and the drive serves
//! roughly 600 of them a second for the whole machine, so a test suite that
//! runs many store-heavy test processes at once spends most of its time queued
//! behind other processes' flushes. A plain `fsync(2)` costs about 0.04 ms.
//! No test can tell the two apart: crash tests kill processes rather than
//! the machine, and the operating system's cache survives a killed process.
//!
//! A debug build that finds `KIN_TEST_FLUSH=fsync` in its environment
//! therefore issues a plain `fsync(2)` on Apple platforms. Other platforms
//! already flush with `fsync` or `fdatasync`, so there both modes issue the
//! same calls. `KIN_TEST_FLUSH=full`, an empty value or no value keeps the
//! standard flush. Any other value keeps it too and prints one warning. A
//! release build never reads the variable.
//!
//! The switch is an environment variable rather than a cargo feature so that
//! it reaches the `kin` and `kin-daemon` processes that tests start, and so
//! the same test binary can measure both modes.

use std::fs::File;
use std::io;
use std::sync::OnceLock;

/// The environment variable a debug build reads to choose its flush.
pub const TEST_FLUSH_ENV: &str = "KIN_TEST_FLUSH";

/// Which flush [`sync_all`] and [`sync_data`] issue in this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushMode {
    /// The standard library's flush: `F_FULLFSYNC` on Apple platforms.
    Full,
    /// A plain `fsync(2)` on Apple platforms, chosen by a debug build that
    /// was started with `KIN_TEST_FLUSH=fsync`. Elsewhere it issues the same
    /// calls as [`FlushMode::Full`].
    Fsync,
}

impl FlushMode {
    /// The mode's name, as a receipt or log line records it.
    pub fn as_str(self) -> &'static str {
        match self {
            FlushMode::Full => "full",
            FlushMode::Fsync => "fsync",
        }
    }
}

/// The flush mode this process uses, decided once on first use.
pub fn flush_mode() -> FlushMode {
    static MODE: OnceLock<FlushMode> = OnceLock::new();
    *MODE.get_or_init(|| {
        let debug_assertions = cfg!(debug_assertions);
        let value = if debug_assertions {
            std::env::var_os(TEST_FLUSH_ENV).map(|value| value.to_string_lossy().into_owned())
        } else {
            None
        };
        let decision = decide(debug_assertions, value.as_deref());
        if decision.unrecognized {
            eprintln!(
                "warning: ignoring {TEST_FLUSH_ENV}={value:?}: expected \"fsync\" or \"full\"; \
                 using full flushes",
                value = value.as_deref().unwrap_or_default(),
            );
        }
        decision.mode
    })
}

/// Flush `file`'s data and metadata, as [`File::sync_all`] does.
pub fn sync_all(file: &File) -> io::Result<()> {
    match flush_mode() {
        FlushMode::Full => standard_sync_all(file),
        FlushMode::Fsync => relaxed_sync(file, standard_sync_all),
    }
}

/// Flush `file`'s data, as [`File::sync_data`] does.
pub fn sync_data(file: &File) -> io::Result<()> {
    match flush_mode() {
        FlushMode::Full => standard_sync_data(file),
        FlushMode::Fsync => relaxed_sync(file, standard_sync_data),
    }
}

#[allow(clippy::disallowed_methods)]
fn standard_sync_all(file: &File) -> io::Result<()> {
    file.sync_all()
}

#[allow(clippy::disallowed_methods)]
fn standard_sync_data(file: &File) -> io::Result<()> {
    file.sync_data()
}

/// A plain `fsync(2)`, retried on `EINTR` as the standard library retries.
#[cfg(target_vendor = "apple")]
fn relaxed_sync(file: &File, _standard: fn(&File) -> io::Result<()>) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    loop {
        // SAFETY: the descriptor belongs to `file`, which outlives the call.
        if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Other platforms have no costlier flush to relax.
#[cfg(not(target_vendor = "apple"))]
fn relaxed_sync(file: &File, standard: fn(&File) -> io::Result<()>) -> io::Result<()> {
    standard(file)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Decision {
    mode: FlushMode,
    /// The variable held a value this module does not know, which is worth
    /// one warning because it was set on purpose and is being ignored.
    unrecognized: bool,
}

/// The flush a build chooses from its profile and the variable's value.
fn decide(debug_assertions: bool, value: Option<&str>) -> Decision {
    if !debug_assertions {
        return Decision {
            mode: FlushMode::Full,
            unrecognized: false,
        };
    }
    match value {
        Some("fsync") => Decision {
            mode: FlushMode::Fsync,
            unrecognized: false,
        },
        None | Some("") | Some("full") => Decision {
            mode: FlushMode::Full,
            unrecognized: false,
        },
        Some(_) => Decision {
            mode: FlushMode::Full,
            unrecognized: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    fn full() -> Decision {
        Decision {
            mode: FlushMode::Full,
            unrecognized: false,
        }
    }

    #[test]
    fn a_debug_build_relaxes_only_on_exactly_fsync() {
        assert_eq!(
            decide(true, Some("fsync")),
            Decision {
                mode: FlushMode::Fsync,
                unrecognized: false,
            }
        );
        assert_eq!(decide(true, Some("full")), full());
        assert_eq!(decide(true, None), full());
        assert_eq!(decide(true, Some("")), full());
    }

    #[test]
    fn an_unknown_value_keeps_full_flushes_and_asks_for_a_warning() {
        for value in ["FSYNC", "fsync ", "relaxed", "1", "0", "\u{fffd}"] {
            assert_eq!(
                decide(true, Some(value)),
                Decision {
                    mode: FlushMode::Full,
                    unrecognized: true,
                },
                "{value:?}"
            );
        }
    }

    #[test]
    fn a_release_build_always_flushes_fully_and_never_warns() {
        for value in [None, Some("fsync"), Some("full"), Some(""), Some("bogus")] {
            assert_eq!(decide(false, value), full(), "{value:?}");
        }
    }

    #[test]
    fn mode_names_are_stable_for_receipts() {
        assert_eq!(FlushMode::Full.as_str(), "full");
        assert_eq!(FlushMode::Fsync.as_str(), "fsync");
    }

    #[test]
    fn both_flushes_succeed_on_a_file_and_keep_its_contents() {
        let mut file = scratch_file();
        file.write_all(b"durable").unwrap();
        sync_data(&file).unwrap();
        sync_all(&file).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "durable");
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_handle_can_be_flushed() {
        let directory = File::open(std::env::temp_dir()).unwrap();
        sync_all(&directory).unwrap();
    }

    fn scratch_file() -> File {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("kin-blobs-durable-{}-{n}", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        file
    }
}
