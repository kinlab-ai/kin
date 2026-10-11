// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Capabilities for filesystem and process access.
//!
//! Kin answers semantic questions from its graph. Raw filesystem and process
//! access belongs at declared boundaries: ingestion, import, reconcile,
//! migration, projection, configuration, the store's own records, and the
//! processes Kin supervises. This crate is how that access is handed to code
//! that needs it.
//!
//! Every capability here is built from a [`Grant`], and [`Grant::ambient`] is
//! the only way to make one. The grant and the unconfined tokens built from it
//! ([`WorktreeRoot`], [`StoreRoot`]) are defined in `kin_blobs::authority` and
//! re-exported here. The filesystem boundary lint
//! (`scripts/io-lint/clippy.toml`) lists `Grant::ambient` beside the raw
//! `std::fs`, `std::path::Path` and `std::process` entry points, so a call to
//! it is accepted only at a registered boundary. Code on an answer path never
//! mints authority; it can only use a capability it was handed, and the type
//! of that capability says what it reaches.
//!
//! - [`StoreDir`] reaches one Kin store's own records and nothing outside it.
//!   It is safe to hand to answer code: a record name that is absolute, climbs
//!   out with `..`, or resolves through a link outside the store is refused,
//!   so a `StoreDir` cannot read repository content.
//! - [`HostDir`] reaches one configuration or cache directory on the host.
//! - [`WorktreeSource`] reads a working tree, for ingestion, import, reconcile
//!   and migration.
//! - [`WorktreeRoot`] is authority over a working tree without confinement, for
//!   a boundary whose volume makes per-path resolution too slow; it uses its
//!   own primitives beneath the root.
//! - [`WorktreeProjection`] writes a working tree, for projection.
//! - [`Spawner`] starts processes.
//!
//! Directory confinement comes from `cap-std`, which resolves every path
//! beneath an opened directory handle rather than from the process's ambient
//! view of the filesystem.

mod confined;
mod spawn;
mod store;
mod worktree;

/// The authority tokens live in kin-blobs, which every published Kin crate
/// already depends on; see [`kin_blobs::authority`] for why.
pub use kin_blobs::authority::{Grant, StoreRoot, WorktreeRoot};
pub use spawn::Spawner;
pub use store::{HostDir, StoreDir};
pub use worktree::{EntryKind, WorktreeProjection, WorktreeSource};
