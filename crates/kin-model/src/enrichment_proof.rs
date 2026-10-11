// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The full proof inputs recorded by version-eight and later enrichment marks.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use crate::identity::{append_canonical_value, CanonicalSink, HashingSink};
use crate::{Entity, Hash256, Relation, ResolutionRecord, ResolutionRecordId, Result};

// Version nine reasks files whose batched writes could have discarded an
// earlier offer's sites for the same relation identity, and files whose
// shared implementation sites could have been assigned to an arbitrary
// owner while another callable was incorrectly marked empty. External
// union-method answers now require the same unique receiver authority as
// repository methods; the old constituent-order proofs must be reasked.
// Version ten binds each file's owned evidence of a shared relation. A peer
// reference query may add evidence to the same edge after this file finishes;
// that new proof belongs to the peer's completion, not this one's. Older
// whole-relation fingerprints are reasked, never reinterpreted as this proof.
// Version eleven reasks Python files whose type answers were read by line
// alone. An answer on a parameter of the function its line declares was
// recorded as a use of that function as a type, so every nested function that
// read a parameter of its parent held a `UsesType` edge to the parent.
pub const ENRICHMENT_PROOF_MARK_VERSION: u32 = 11;

/// Hash complete selected proof inputs once per requested file. The caller
/// supplies one held graph, never a mixture of live and durable records.
///
/// Includes full relation headers, owned evidence, caller payloads, ledgers,
/// selected context validation (including its absence), and transitively referenced
/// resolution records. Canonical serialization sorts map keys; record and
/// entity identities sort the collections. No source bytes are read or copied.
/// A digest is a binding to an observation, not evidence that it completed:
/// only the successful publication path may write the corresponding mark.
pub fn enrichment_proof_inputs_by_file<'a, 'p>(
    paths: impl IntoIterator<Item = &'p str>,
    entities: impl IntoIterator<Item = &'a Entity>,
    relations: impl IntoIterator<Item = &'a Relation>,
    records: impl IntoIterator<Item = &'a ResolutionRecord>,
) -> Result<BTreeMap<String, Hash256>> {
    let paths = paths.into_iter().collect::<BTreeSet<_>>();
    if paths.is_empty() {
        return Ok(BTreeMap::new());
    }
    let entities: BTreeMap<_, _> = entities
        .into_iter()
        .map(|entity| (entity.id, entity))
        .collect();
    let entity_file = |id: &crate::EntityId| {
        entities
            .get(id)
            .and_then(|entity| entity.file_origin.as_ref())
            .map(|file| file.0.as_str())
    };
    let relations = crate::enrichment_relations_by_owner_file(relations, entity_file);
    let records: BTreeMap<_, _> = records
        .into_iter()
        .map(|record| (record.id(), record))
        .collect();
    let ledgers = crate::enrichment_ledgers_by_file(records.values().copied(), entity_file);
    let mut by_file: BTreeMap<&str, Vec<&Entity>> = BTreeMap::new();
    for entity in entities.values() {
        if let Some(file) = entity.file_origin.as_ref() {
            by_file.entry(&file.0).or_default().push(entity);
        }
    }
    let mut result = BTreeMap::new();
    for path in paths {
        let callers = by_file.get(path).map(Vec::as_slice).unwrap_or(&[]);
        let mut owned: Vec<Cow<'_, Relation>> = relations
            .get(path)
            .into_iter()
            .flatten()
            .map(|relation| {
                if relation.origin != crate::RelationOrigin::Lsp {
                    // Parsed call certificates, including spanless ones, are
                    // complete proof inputs and are never narrowed here.
                    return Cow::Borrowed(*relation);
                }
                let keep = |evidence: &crate::RelationEvidence| {
                    let mut assigned = false;
                    let mut own = false;
                    crate::repository::for_each_enrichment_evidence_owner(
                        relation,
                        evidence,
                        &entity_file,
                        |file| {
                            assigned = true;
                            own |= file == path;
                        },
                    );
                    // Only positively peer-owned evidence may be omitted.
                    !assigned || own
                };
                if relation.evidence.iter().all(keep) {
                    Cow::Borrowed(*relation)
                } else {
                    let mut projected = (**relation).clone();
                    projected.evidence.retain(keep);
                    Cow::Owned(projected)
                }
            })
            .collect();
        owned.sort_by_key(|relation| relation.id);
        owned.dedup_by_key(|relation| relation.id);
        let mut required = ledgers.get(path).cloned().unwrap_or_default();
        let mut languages = callers
            .iter()
            .map(|entity| entity.language)
            .collect::<Vec<_>>();
        if let Some(language) = crate::call_site_reading::language_of_path(path) {
            languages.push(language);
        }
        languages.sort_by_key(ToString::to_string);
        languages.dedup();
        required.extend(
            languages
                .into_iter()
                .map(ResolutionRecordId::context_validation),
        );
        required.extend(
            owned
                .iter()
                .flat_map(|relation| &relation.evidence)
                .filter_map(|evidence| {
                    evidence
                        .token
                        .as_deref()
                        .and_then(ResolutionRecordId::from_context_token)
                }),
        );
        let mut proof_records = BTreeMap::new();
        while let Some(id) = required.pop() {
            if proof_records.contains_key(&id) {
                continue;
            }
            let record = records.get(&id).copied();
            if let Some(record) = record {
                required.extend(record.referenced_records());
            }
            // Missing input is explicit, never omitted from the binding.
            proof_records.insert(id, record);
        }
        let mut sink = HashingSink::new();
        sink.write_bytes(b"kin.enrichment-proof-inputs.v10\0");
        append_canonical_value(&mut sink, &(path, callers, owned, proof_records))?;
        result.insert(path.to_owned(), Hash256::from_bytes(sink.finish()));
    }
    Ok(result)
}
