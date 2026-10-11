// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Authority tokens for filesystem and process access.
//!
//! Kin answers semantic questions from its graph. Raw filesystem and process
//! access belongs at declared boundaries: ingestion, import, reconcile,
//! migration, projection, configuration, a store's own records, and the
//! processes Kin supervises. A boundary proves it is one by holding a token
//! from this module, and a function that reaches the filesystem says so by
//! taking one.
//!
//! [`Grant::ambient`] is the only way to make a token. Kin's filesystem
//! boundary lint lists it beside `std::fs::read` and the other raw entry
//! points, so a call to it is accepted only at a registered boundary, and code
//! on an answer path cannot mint authority it was not handed.
//!
//! The tokens carry authority without confinement: a holder uses its own
//! primitives beneath the root it names. That is deliberate for the
//! boundaries whose volume makes per-path confinement too slow, such as a walk
//! over a whole working tree. The confined capabilities, which resolve every
//! path beneath a directory handle, live in the `kin-io` crate and are built
//! from these tokens.
//!
//! Why the tokens live in kin-blobs: this crate is published to the Kin
//! registry and sits beneath every other crate that is, so each of them can
//! take a token without a new published crate. The tokens add no dependency:
//! they are plain values over `std::path`. The durable flush switch in
//! [`crate::durable`] is here for the same reason.

use std::path::{Path, PathBuf};

/// Authority to reach the host's filesystem and processes.
///
/// A `Grant` is the root every token and capability is built from. It carries
/// no state; holding one is the claim that the caller is a declared boundary.
/// Cloning a grant confers nothing its holder did not already have.
#[derive(Clone, Debug)]
pub struct Grant {
    _private: (),
}

impl Grant {
    /// Claim ambient authority.
    ///
    /// The filesystem boundary lint refuses this call outside a registered
    /// boundary, the same way it refuses `std::fs::read`.
    pub fn ambient() -> Grant {
        Grant { _private: () }
    }
}

/// Authority over one working tree, without confinement.
///
/// An ingestion boundary that walks or reads a whole tree, such as membership
/// discovery, a file watcher or a coverage report, holds one of these and uses
/// its own primitives beneath [`WorktreeRoot::path`]. A function that reads the
/// tree takes `&WorktreeRoot`, and only a [`Grant`] mints one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeRoot {
    root: PathBuf,
}

impl WorktreeRoot {
    /// Authority over the working tree at `root`.
    pub fn new(root: impl Into<PathBuf>, _grant: &Grant) -> Self {
        Self { root: root.into() }
    }

    /// The tree's directory, for the boundary's own primitives.
    pub fn path(&self) -> &Path {
        &self.root
    }
}

/// Authority over one Kin store's own records, without confinement.
///
/// A store boundary that persists or maps its own records, such as a search or
/// vector index, holds one of these and uses its own primitives beneath
/// [`StoreRoot::path`]. A function that touches the store takes `&StoreRoot`,
/// and only a [`Grant`] mints one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreRoot {
    root: PathBuf,
}

impl StoreRoot {
    /// Authority over the store at `root`.
    pub fn new(root: impl Into<PathBuf>, _grant: &Grant) -> Self {
        Self { root: root.into() }
    }

    /// The store's directory, for the boundary's own primitives.
    pub fn path(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_names_the_root_it_was_minted_for() {
        let grant = Grant::ambient();
        let tree = WorktreeRoot::new("/repo", &grant);
        let store = StoreRoot::new("/repo/.kin", &grant.clone());
        assert_eq!(tree.path(), Path::new("/repo"));
        assert_eq!(store.path(), Path::new("/repo/.kin"));
        assert_eq!(tree.clone(), tree);
    }
}
