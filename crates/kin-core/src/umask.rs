// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! This process's file-creation mask, and the one place Kin narrows it.
//!
//! Distributions that set `USERGROUPS_ENAB`, Ubuntu and Debian among them,
//! give every login a umask of 0002. Rust's `create_dir_all`, `File::create`
//! and `fs::write` ask for 0777 or 0666 and leave the rest to that mask, so
//! under it every directory and file Kin made came out group writable: the
//! managed home, its config and shell files, the supervisor's records, and a
//! repository's store. The registry then refused the directory it lives in,
//! and everything else was writable by the user's whole group.
//!
//! So each Kin binary adds group and world write to its own umask as the first
//! thing it does, before any thread starts or any file is created. The mask
//! only ever gains bits: a stricter one such as 077 is kept as it was. Child
//! processes Kin starts inherit the narrowed mask, and files Kin writes into a
//! working tree are created without group write too, which a checkout shared
//! through group permissions has to grant explicitly. Paths whose mode is a
//! security boundary, such as the registry and `~/.kin` itself, still set it
//! explicitly rather than relying on this.
//!
//! Windows has no umask, so this is a no-op there.

/// The bits Kin always keeps in its umask: write for group and for others.
pub const WITHHELD_BITS: u32 = 0o022;

/// Add [`WITHHELD_BITS`] to this process's umask, keeping every bit already
/// set, and return the mask now in force. Returns `None` off Unix.
///
/// Reading a umask means setting one, so there is a moment between the two
/// calls when the mask is exactly 022. Call this while the process is still
/// single-threaded, as the first line of `main`.
pub fn withhold_shared_write() -> Option<u32> {
    #[cfg(unix)]
    {
        // SAFETY: `umask` has no preconditions and cannot fail.
        let previous = unsafe { libc::umask(WITHHELD_BITS as libc::mode_t) };
        let narrowed = previous | WITHHELD_BITS as libc::mode_t;
        // SAFETY: as above.
        unsafe { libc::umask(narrowed) };
        Some(u32::from(narrowed))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The umask is process-wide, so the cases run in a child process where
    /// no other test can observe or race the change.
    #[test]
    fn withholding_shared_write_keeps_a_stricter_mask() {
        if let Some(case) = std::env::var_os("UMASK_SUBPROCESS_START") {
            let start = u32::from_str_radix(case.to_str().unwrap(), 8).unwrap();
            // SAFETY: this child runs one test on one thread.
            unsafe { libc::umask(start as libc::mode_t) };
            let narrowed = withhold_shared_write().unwrap();
            // SAFETY: as above; reading the mask means setting it.
            let observed = u32::from(unsafe { libc::umask(0o022) });
            assert_eq!(narrowed, observed);
            println!("UMASK {narrowed:04o}");
            return;
        }
        for (start, expected) in [
            ("0002", "0022"),
            ("0000", "0022"),
            ("0022", "0022"),
            ("0077", "0077"),
            ("0027", "0027"),
            ("0007", "0027"),
        ] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "umask::tests::withholding_shared_write_keeps_a_stricter_mask",
                    "--exact",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("UMASK_SUBPROCESS_START", start)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "umask {start}: {stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                stdout.contains(&format!("UMASK {expected}")),
                "umask {start} should narrow to {expected}: {stdout}"
            );
        }
    }
}
