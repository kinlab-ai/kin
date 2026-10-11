// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The last full status reading of a store, kept for the publication it
//! describes.
//!
//! A status with no daemon running opens repository authority in-process,
//! which decodes the whole snapshot and re-verifies every persisted body, so
//! it costs whatever the whole store is worth. Right after a conversion that
//! is the common case: `kin init` stops the daemon it started, and the first
//! `kin status` paid 13 seconds on a 678-commit store and 153 seconds on a
//! 5,562-commit one, to report counts the conversion had just read.
//!
//! So the process that pays for an open records what it read, keyed by the
//! exact durable publication it opened, and a later reader that finds the same
//! publication answers from the record. The key is the SHA-256 of the store's
//! publication record. KinDB replaces that one small record atomically as its
//! commit point, and the record content-binds the snapshot digest and every
//! acknowledged frame digest, so identical record bytes mean identical durable
//! state. A record keyed by any other publication is never served. The daemon
//! revalidates the authority it holds open against the same record for the
//! same reason.
//!
//! The reading itself is opaque here: its shape belongs to `kin status`.
//!
//! It is a cache of derived counts, rebuilt by the next open whenever it is
//! missing or names another publication, so it is written as a scratch record.

use std::io;

use chrono::{DateTime, Utc};
use kin_core::{KinLayout, LocalRepositoryAuthorityBinding};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The record's name in the store, beside the store's other records.
const RECORD: &str = "status-reading.json";

/// The longest record read back. A reading carries the admitted tree its
/// working-copy walk compares against, so it grows with the repository.
const MAX_RECORD_BYTES: u64 = 256 * 1024 * 1024;

/// KinDB reads its own publication record under a 1 MiB bound, and so does
/// this.
const MAX_PUBLICATION_RECORD_BYTES: u64 = 1024 * 1024;

const SCHEMA: &str = "kin.status-reading.v1";

/// KinDB's commit record for one local repository's durable publication.
const PUBLICATION_RECORD: &str = "authority.json";

/// One durable publication of a store, named by the SHA-256 of its record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication(String);

#[cfg(test)]
impl Publication {
    /// The hex digest that names it.
    fn as_str(&self) -> &str {
        &self.0
    }
}

/// A reading recorded for the publication it was looked up by.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedReading {
    /// When the reading was taken.
    pub recorded_at: DateTime<Utc>,
    /// The command that took it, such as `kin init`.
    pub recorded_by: String,
    /// The reading, in its recorder's own shape.
    pub reading: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
struct RecordFile {
    schema: String,
    publication: String,
    recorded_at: DateTime<Utc>,
    recorded_by: String,
    reading: serde_json::Value,
}

/// The store's own records. A reading is a record of this store, never
/// repository content, so the capability that holds it reaches the store and
/// nothing outside it.
fn reading_store(layout: &KinLayout) -> io::Result<kin_io::StoreDir> {
    kin_io::StoreDir::open(layout.root(), &kin_io::Grant::ambient())
}

/// Which durable publication the store holds, read without loading it.
///
/// `Ok(None)` when the store has published nothing yet. One small record is
/// read, so this costs the same on any store.
pub fn current_publication(
    layout: &KinLayout,
    binding: &LocalRepositoryAuthorityBinding,
) -> io::Result<Option<Publication>> {
    let path = binding.namespace_path().join(PUBLICATION_RECORD);
    let relative = path.strip_prefix(layout.root()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the publication record {} is outside the store at {}",
                path.display(),
                layout.root().display()
            ),
        )
    })?;
    Ok(reading_store(layout)?
        .read_record(relative, MAX_PUBLICATION_RECORD_BYTES)?
        .map(|bytes| {
            Publication(
                Sha256::digest(&bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            )
        }))
}

/// Record `reading` as the store's reading at `publication`, replacing any
/// earlier one. Atomic: a reader sees the previous record or this one. Not
/// flushed to the device: a reading lost to a crash is rebuilt by the next
/// open.
pub fn record(
    layout: &KinLayout,
    publication: &Publication,
    recorded_by: &str,
    reading: serde_json::Value,
) -> io::Result<()> {
    let body = serde_json::to_vec(&RecordFile {
        schema: SCHEMA.to_string(),
        publication: publication.0.clone(),
        recorded_at: Utc::now(),
        recorded_by: recorded_by.to_string(),
        reading,
    })
    .map_err(io::Error::other)?;
    reading_store(layout)?.write_scratch_record(RECORD, &body)
}

/// The reading recorded for exactly `publication`, if there is one.
///
/// `None` for a missing, unreadable or foreign record, and for one recorded
/// at any other publication: a reader then opens the store as it would have
/// without a record. Nothing here can make a reader serve a reading of
/// different durable bytes.
pub fn recorded_for(layout: &KinLayout, publication: &Publication) -> Option<RecordedReading> {
    let bytes = reading_store(layout)
        .ok()?
        .read_record(RECORD, MAX_RECORD_BYTES)
        .ok()??;
    let record: RecordFile = serde_json::from_slice(&bytes).ok()?;
    if record.schema != SCHEMA || record.publication != publication.0 {
        return None;
    }
    Some(RecordedReading {
        recorded_at: record.recorded_at,
        recorded_by: record.recorded_by,
        reading: record.reading,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_in(dir: &std::path::Path) -> KinLayout {
        let root = dir.join(".kin");
        std::fs::create_dir_all(&root).unwrap();
        KinLayout::new(root)
    }

    /// A reading is served only for the exact publication it was recorded at.
    #[test]
    fn a_reading_is_served_only_for_its_own_publication() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout_in(dir.path());
        let first = Publication("a".repeat(64));
        let second = Publication("b".repeat(64));
        assert_eq!(recorded_for(&layout, &first), None, "nothing recorded yet");

        record(
            &layout,
            &first,
            "kin init",
            serde_json::json!({"entities": 216}),
        )
        .unwrap();
        let read = recorded_for(&layout, &first).expect("the same publication reads back");
        assert_eq!(read.recorded_by, "kin init");
        assert_eq!(read.reading, serde_json::json!({"entities": 216}));
        assert_eq!(
            recorded_for(&layout, &second),
            None,
            "a reading of other durable bytes is never served"
        );

        record(
            &layout,
            &second,
            "kin status",
            serde_json::json!({"entities": 217}),
        )
        .unwrap();
        assert_eq!(
            recorded_for(&layout, &first),
            None,
            "replaced, not appended"
        );
        assert_eq!(
            recorded_for(&layout, &second).map(|read| read.reading),
            Some(serde_json::json!({"entities": 217}))
        );
    }

    /// The publication is the digest of the store's own publication record,
    /// read without loading the store.
    #[test]
    fn the_publication_names_the_store_publication_record() {
        let dir = tempfile::tempdir().unwrap();
        let layout = kin_core::init(dir.path()).unwrap().layout;
        let binding = LocalRepositoryAuthorityBinding::from_layout(&layout).unwrap();
        let publication = current_publication(&layout, &binding)
            .unwrap()
            .expect("an initialized store has published");
        let bytes = std::fs::read(binding.namespace_path().join(PUBLICATION_RECORD)).unwrap();
        let expected: String = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(publication.as_str(), expected);
        assert_eq!(
            current_publication(&layout, &binding).unwrap(),
            Some(publication),
            "an unmoved store reads as the same publication"
        );
    }

    /// A record this build cannot read is no record at all.
    #[test]
    fn an_unreadable_or_foreign_record_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout_in(dir.path());
        let publication = Publication("c".repeat(64));
        std::fs::write(layout.root().join(RECORD), b"{ not json").unwrap();
        assert_eq!(recorded_for(&layout, &publication), None);
        std::fs::write(
            layout.root().join(RECORD),
            serde_json::to_vec(&serde_json::json!({
                "schema": "kin.status-reading.v0",
                "publication": publication.as_str(),
                "recorded_at": "2026-10-10T00:00:00Z",
                "recorded_by": "kin init",
                "reading": {},
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(recorded_for(&layout, &publication), None);
    }
}
