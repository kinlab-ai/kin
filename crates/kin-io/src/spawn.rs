// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Starting processes.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Command;

use crate::Grant;

/// Authority to start processes: the daemon, language servers, Git, and the
/// commands Kin runs in a materialized workspace.
///
/// An answer path is never handed one, so it cannot run `rg`, `grep` or `find`
/// in place of the graph.
#[derive(Debug)]
pub struct Spawner {
    _private: (),
}

impl Spawner {
    /// A spawner, built from a grant.
    pub fn new(_grant: &Grant) -> Self {
        Self { _private: () }
    }

    /// A command for `program`, ready to configure and start.
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        Command::new(program)
    }

    /// The first of `candidates` that exists, in order: how a program is
    /// found beside an executable or along a search path before it is started.
    pub fn locate_program(&self, candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
        candidates.into_iter().find(|candidate| candidate.exists())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_spawner_locates_the_first_program_that_exists() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("kin-daemon");
        std::fs::write(&present, b"").unwrap();
        let spawner = Spawner::new(&Grant::ambient());
        assert_eq!(
            spawner.locate_program([dir.path().join("absent"), present.clone()]),
            Some(present)
        );
        assert_eq!(spawner.locate_program([dir.path().join("absent")]), None);
    }

    #[test]
    fn a_spawner_starts_a_process() {
        let spawner = Spawner::new(&Grant::ambient());
        let status = spawner.command("true").status().unwrap();
        assert!(status.success());
    }
}
