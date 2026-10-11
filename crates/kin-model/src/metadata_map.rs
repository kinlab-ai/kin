// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The metadata map an entity carries, shared between copies until one of
//! them changes.
//!
//! A repository daemon holds the same entity in several places at once: the
//! decoded store body, the entity revisions beside it, the materialized graph
//! section, the change that admitted it, the workspace graph kept for the
//! next open, the operation record, and the graph that answers queries. Each
//! used to own a deep copy of the entity's metadata, which is the largest
//! part of an entity in memory. Measured on a 6,610-entity Python repository,
//! one copy of every entity's metadata held 38 MB of live heap and the daemon
//! held nine of them: five decoded from the one store body and four cloned
//! from those, 345 of its 643 MB.
//!
//! [`MetadataMap`] keeps the map behind an [`Arc`]. A clone is a pointer copy,
//! and a write through [`DerefMut`] detaches the writer's copy first
//! ([`Arc::make_mut`]), so no holder ever sees another holder's change: the
//! sharing is invisible to everything except the allocator. Decoding goes one
//! step further. Two decodes of equal content return one allocation, found by
//! a content hash and confirmed by equality, so the copies a store body
//! carries of the same entity collapse to one however many sections hold it.
//!
//! None of this changes what is written. [`MetadataMap`] serializes as the
//! plain map it wraps, so stores written before and after it are the same
//! bytes, and an entity's JSON and schema are unchanged.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// The plain map a [`MetadataMap`] holds.
pub type MetadataEntries = HashMap<String, Value>;

/// An entity's metadata: a map from key to JSON value, shared between copies
/// until one of them writes.
///
/// Reads go through [`Deref`] to the map itself, so every map method works
/// unchanged. Writes go through [`DerefMut`], which gives this copy a private
/// map first when another copy shares it.
#[derive(Clone, Default)]
pub struct MetadataMap(Arc<MetadataEntries>);

impl MetadataMap {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `self` and `other` share one allocation.
    ///
    /// Equal maps need not share one; maps that share one are always equal.
    pub fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// The plain map, without a copy when no other holder shares it.
    pub fn into_entries(self) -> MetadataEntries {
        Arc::try_unwrap(self.0).unwrap_or_else(|shared| (*shared).clone())
    }

    /// The map `entries` decoded to, sharing an allocation with any live map
    /// of equal content that an earlier decode produced.
    fn decoded(entries: MetadataEntries) -> Self {
        if entries.is_empty() {
            return Self::default();
        }
        let digest = content_digest(&entries);
        let found = interner()
            .lock()
            .ok()
            .and_then(|table| table.entries.get(&digest).and_then(Weak::upgrade));
        // Equality is checked outside the lock, and it is what makes sharing
        // sound: a digest only says where to look.
        if let Some(shared) = found {
            if *shared == entries {
                return Self(shared);
            }
        }
        let shared = Arc::new(entries);
        if let Ok(mut table) = interner().lock() {
            table.remember(digest, &shared);
        }
        Self(shared)
    }
}

impl Deref for MetadataMap {
    type Target = MetadataEntries;

    fn deref(&self) -> &MetadataEntries {
        &self.0
    }
}

impl DerefMut for MetadataMap {
    fn deref_mut(&mut self) -> &mut MetadataEntries {
        Arc::make_mut(&mut self.0)
    }
}

impl PartialEq for MetadataMap {
    fn eq(&self, other: &Self) -> bool {
        self.shares_storage_with(other) || *self.0 == *other.0
    }
}

impl Eq for MetadataMap {}

/// Formats as the plain map, so nothing that prints an entity changes.
impl std::fmt::Debug for MetadataMap {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl From<MetadataEntries> for MetadataMap {
    fn from(entries: MetadataEntries) -> Self {
        Self(Arc::new(entries))
    }
}

impl From<MetadataMap> for MetadataEntries {
    fn from(map: MetadataMap) -> Self {
        map.into_entries()
    }
}

impl FromIterator<(String, Value)> for MetadataMap {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
        Self(Arc::new(iter.into_iter().collect()))
    }
}

impl Extend<(String, Value)> for MetadataMap {
    fn extend<I: IntoIterator<Item = (String, Value)>>(&mut self, iter: I) {
        Arc::make_mut(&mut self.0).extend(iter);
    }
}

impl IntoIterator for MetadataMap {
    type Item = (String, Value);
    type IntoIter = std::collections::hash_map::IntoIter<String, Value>;

    fn into_iter(self) -> Self::IntoIter {
        self.into_entries().into_iter()
    }
}

impl<'a> IntoIterator for &'a MetadataMap {
    type Item = (&'a String, &'a Value);
    type IntoIter = std::collections::hash_map::Iter<'a, String, Value>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Written as the plain map, so the bytes on disk and on the wire are the
/// ones the map wrote before it was shared.
impl Serialize for MetadataMap {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

/// Read as the plain map, then shared with any live map of equal content.
impl<'de> Deserialize<'de> for MetadataMap {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        MetadataEntries::deserialize(deserializer).map(Self::decoded)
    }
}

/// The plain map's schema, so published schemas do not change.
impl JsonSchema for MetadataMap {
    fn is_referenceable() -> bool {
        MetadataEntries::is_referenceable()
    }

    fn schema_name() -> String {
        MetadataEntries::schema_name()
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        MetadataEntries::schema_id()
    }

    fn json_schema(generator: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        MetadataEntries::json_schema(generator)
    }
}

/// Live decoded maps by content digest.
///
/// Holds [`Weak`] references only, so it never keeps a map alive: a map is
/// freed when its last holder drops it, and its entry is swept later.
struct Interner {
    entries: HashMap<u64, Weak<MetadataEntries>>,
    /// Table size at which dead entries are next swept.
    sweep_at: usize,
}

/// Fewest entries the table holds before its first sweep.
const FIRST_SWEEP_AT: usize = 16_384;

impl Interner {
    /// File `shared` under `digest`, replacing whatever was there.
    ///
    /// A replaced entry is either dead or a different map whose digest
    /// collided, and losing the second only costs that map its sharing.
    fn remember(&mut self, digest: u64, shared: &Arc<MetadataEntries>) {
        self.entries.insert(digest, Arc::downgrade(shared));
        if self.entries.len() >= self.sweep_at {
            self.entries.retain(|_, entry| entry.strong_count() > 0);
            // Twice the live size, so sweeping stays amortized constant time
            // per decode however the live set grows or shrinks.
            self.sweep_at = (self.entries.len() * 2).max(FIRST_SWEEP_AT);
        }
    }
}

fn interner() -> &'static Mutex<Interner> {
    static INTERNER: OnceLock<Mutex<Interner>> = OnceLock::new();
    INTERNER.get_or_init(|| {
        Mutex::new(Interner {
            entries: HashMap::new(),
            sweep_at: FIRST_SWEEP_AT,
        })
    })
}

/// A digest of a map's content that does not depend on its iteration order.
///
/// Each entry is hashed on its own and the results are added, because two
/// equal hash maps can iterate in different orders.
fn content_digest(entries: &MetadataEntries) -> u64 {
    let mut combined = entries.len() as u64;
    for (key, value) in entries {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        hash_value(value, &mut hasher);
        combined = combined.wrapping_add(hasher.finish());
    }
    combined
}

/// Hash a JSON value so that equal values hash equally.
fn hash_value(value: &Value, hasher: &mut impl Hasher) {
    match value {
        Value::Null => 0u8.hash(hasher),
        Value::Bool(flag) => {
            1u8.hash(hasher);
            flag.hash(hasher);
        }
        Value::Number(number) => {
            2u8.hash(hasher);
            if let Some(signed) = number.as_i64() {
                signed.hash(hasher);
            } else if let Some(unsigned) = number.as_u64() {
                unsigned.hash(hasher);
            } else {
                number.as_f64().map(f64::to_bits).hash(hasher);
            }
        }
        Value::String(text) => {
            3u8.hash(hasher);
            text.hash(hasher);
        }
        Value::Array(items) => {
            4u8.hash(hasher);
            items.len().hash(hasher);
            for item in items {
                hash_value(item, hasher);
            }
        }
        Value::Object(fields) => {
            // A JSON object's own iteration order is its key order, so an
            // ordered walk is stable for equal objects.
            5u8.hash(hasher);
            fields.len().hash(hasher);
            for (key, field) in fields {
                key.hash(hasher);
                hash_value(field, hasher);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entries(pairs: &[(&str, Value)]) -> MetadataEntries {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect()
    }

    /// A clone is a pointer copy, and a write detaches only the writer.
    #[test]
    fn a_write_to_one_copy_never_reaches_another() {
        let original: MetadataMap = entries(&[("blob_hash", json!("abc"))]).into();
        let mut copy = original.clone();
        assert!(copy.shares_storage_with(&original));

        copy.insert("declaration_line".to_string(), json!(7));
        assert!(!copy.shares_storage_with(&original));
        assert_eq!(original.len(), 1);
        assert_eq!(copy.len(), 2);
        assert_eq!(original.get("declaration_line"), None);
    }

    /// Two decodes of equal content share one allocation, and a decode of
    /// different content does not.
    #[test]
    fn equal_decodes_share_one_allocation() {
        let body = entries(&[
            ("embedding_body_preview", json!("def render(self): ...")),
            (
                "file_import_context",
                json!({"modules": ["rich.console", "rich.text"], "aliases": {"Text": "rich.text.Text"}}),
            ),
            ("declaration_line", json!(42)),
            ("ratio", json!(0.5)),
        ]);
        let bytes = rmp_serde::to_vec(&MetadataMap::from(body.clone())).unwrap();

        let first: MetadataMap = rmp_serde::from_slice(&bytes).unwrap();
        let second: MetadataMap = rmp_serde::from_slice(&bytes).unwrap();
        assert!(first.shares_storage_with(&second));
        assert_eq!(*first, body);

        let mut other = body;
        other.insert("declaration_line".to_string(), json!(43));
        let other_bytes = rmp_serde::to_vec(&MetadataMap::from(other)).unwrap();
        let third: MetadataMap = rmp_serde::from_slice(&other_bytes).unwrap();
        assert!(!third.shares_storage_with(&first));
        assert_ne!(third, first);
    }

    /// A map freed by every holder is not kept alive by the interner, and a
    /// later decode of the same content still decodes it.
    #[test]
    fn the_interner_keeps_nothing_alive() {
        let body = entries(&[("only_this_test_writes_this_key", json!([1, 2, 3]))]);
        let bytes = serde_json::to_vec(&MetadataMap::from(body.clone())).unwrap();
        let first: MetadataMap = serde_json::from_slice(&bytes).unwrap();
        let weak = Arc::downgrade(&first.0);
        drop(first);
        assert!(weak.upgrade().is_none());

        let again: MetadataMap = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(*again, body);
    }

    /// Metadata written by a build that held the plain map reads back the
    /// same and writes the same bytes, so no store needs migrating.
    #[test]
    fn metadata_written_before_sharing_reads_and_writes_unchanged() {
        #[derive(Serialize)]
        struct PlainEntityMetadata {
            #[serde(flatten)]
            extra: MetadataEntries,
        }
        let plain = PlainEntityMetadata {
            extra: entries(&[(
                "file_surface_context",
                json!({"exports": ["Console", "Group"], "dunder_all": true}),
            )]),
        };
        let written_before = rmp_serde::to_vec(&plain).unwrap();

        let read: crate::EntityMetadata = rmp_serde::from_slice(&written_before).unwrap();
        assert_eq!(*read.extra, plain.extra);
        assert_eq!(rmp_serde::to_vec(&read).unwrap(), written_before);
        assert_eq!(
            serde_json::to_vec(&read).unwrap(),
            serde_json::to_vec(&plain).unwrap()
        );
    }

    /// Equal maps digest equally whatever order they were built in.
    #[test]
    fn the_digest_ignores_insertion_order() {
        let mut forward = MetadataEntries::new();
        let mut backward = MetadataEntries::new();
        let keys: Vec<String> = (0..64).map(|index| format!("key_{index}")).collect();
        for (index, key) in keys.iter().enumerate() {
            forward.insert(key.clone(), json!(index));
        }
        for (index, key) in keys.iter().enumerate().rev() {
            backward.insert(key.clone(), json!(index));
        }
        assert_eq!(forward, backward);
        assert_eq!(content_digest(&forward), content_digest(&backward));
    }
}
