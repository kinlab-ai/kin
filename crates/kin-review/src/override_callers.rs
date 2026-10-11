// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Callers that reach a changed method only through one of its overrides.
//!
//! An override declares the method again, so a call the resolver settled on
//! the override is a call to the override and no edge reaches the base from
//! it. The base's callers and the overrides are already in an impact answer,
//! and the callers of the overrides are the third part of what changes with a
//! signature: when the base gains a required parameter, every override gains
//! it too, and every call to an override has to pass it. Asked which functions
//! change when `JSONProvider.dumps` gains one, an answer that listed the base's
//! callers and `DefaultJSONProvider.dumps` left out
//! `DefaultJSONProvider.response`, whose `self.dumps(...)` is settled on the
//! override, and read as complete.
//!
//! Each row names the override it calls and how firmly the call edge settled
//! it. A call matched by name only is a candidate, kept apart from proof.

use std::collections::HashMap;

use kin_index::RelationResolution;
use kin_model::entity::Entity;
use kin_model::ids::EntityId;
use kin_model::relation::{GraphNodeId, RelationKind};
use serde::{Deserialize, Serialize};

use crate::error::ReviewError;
use crate::impact::ImpactGraph;

/// One call from a caller to an override of a changed method.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverrideCall {
    /// The override the call reaches.
    pub override_id: EntityId,
    /// The override's name, as the graph names it.
    pub override_name: String,
    /// The changed method this override belongs to, directly or through a
    /// chain of overrides.
    pub changed_id: EntityId,
    /// How firmly the call edge settled the override, on the ladder every
    /// reference row reports: `type_resolved`, `import_scoped` or `name_only`.
    pub resolution: String,
    /// Whether that resolution is proof, which `name_only` is not.
    pub proven: bool,
}

/// A caller of one or more overrides of a changed method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverrideCaller {
    /// The calling entity.
    pub entity: Entity,
    /// Every override it calls, proven calls first, then in the order the
    /// overrides were found.
    pub calls: Vec<OverrideCall>,
}

impl OverrideCaller {
    /// The call the row is labelled with: its strongest, first found.
    pub fn primary(&self) -> Option<&OverrideCall> {
        self.calls.first()
    }

    /// Whether any of its calls to an override is proven.
    pub fn proven(&self) -> bool {
        self.calls.iter().any(|call| call.proven)
    }
}

/// The callers of every override of a set of changed methods, split by proof.
#[derive(Debug, Clone, Default)]
pub struct OverrideCallers {
    /// Callers with at least one proven call to an override.
    pub proven: Vec<OverrideCaller>,
    /// Callers whose every call to an override was matched by name only.
    pub candidates: Vec<OverrideCaller>,
}

/// Read the callers of each family's overrides off incoming `Calls` edges.
///
/// `families` pairs each changed method with its overrides, in the order the
/// override walk found them. One caller of several overrides, or of overrides
/// in several families, is one row carrying every call. `skip(caller,
/// proven)` leaves a caller out: the family members themselves, and whatever
/// the reader of these rows already lists, which can depend on whether the
/// caller's calls to an override are proven. Rows are ordered by file, then
/// name, then id, so two reads of one graph list one answer the same way.
pub fn override_callers_at<I: ImpactGraph>(
    graph: &I,
    families: &[(EntityId, Vec<Entity>)],
    skip: impl Fn(&Entity, bool) -> bool,
) -> Result<OverrideCallers, ReviewError> {
    let mut order: Vec<EntityId> = Vec::new();
    let mut found: HashMap<EntityId, OverrideCaller> = HashMap::new();
    let mut rank = 0usize;
    let mut ranks: HashMap<EntityId, usize> = HashMap::new();
    for (changed_id, overrides) in families {
        for overrider in overrides {
            ranks.entry(overrider.id).or_insert_with(|| {
                rank += 1;
                rank
            });
            for relation in graph.get_all_relations_for_entity(&overrider.id)? {
                if relation.kind != RelationKind::Calls
                    || relation.dst != GraphNodeId::Entity(overrider.id)
                {
                    continue;
                }
                let Some(caller_id) = relation.src.as_entity() else {
                    continue;
                };
                if caller_id == overrider.id {
                    continue;
                }
                let resolution = RelationResolution::of(&relation);
                let call = OverrideCall {
                    override_id: overrider.id,
                    override_name: overrider.name.clone(),
                    changed_id: *changed_id,
                    resolution: resolution.as_str().to_string(),
                    proven: resolution.is_proven(),
                };
                if let Some(row) = found.get_mut(&caller_id) {
                    match row
                        .calls
                        .iter_mut()
                        .find(|known| known.override_id == call.override_id)
                    {
                        // Two edges to one override keep the firmer reading.
                        Some(known) if call.proven && !known.proven => *known = call,
                        Some(_) => {}
                        None => row.calls.push(call),
                    }
                    continue;
                }
                let Some(entity) = graph.get_entity(&caller_id)? else {
                    continue;
                };
                order.push(caller_id);
                found.insert(
                    caller_id,
                    OverrideCaller {
                        entity,
                        calls: vec![call],
                    },
                );
            }
        }
    }
    let mut out = OverrideCallers::default();
    for id in order {
        let Some(mut row) = found.remove(&id) else {
            continue;
        };
        let proven = row.proven();
        if skip(&row.entity, proven) {
            continue;
        }
        row.calls.sort_by_key(|call| {
            (
                !call.proven,
                ranks.get(&call.override_id).copied().unwrap_or(usize::MAX),
            )
        });
        if proven {
            out.proven.push(row);
        } else {
            out.candidates.push(row);
        }
    }
    for rows in [&mut out.proven, &mut out.candidates] {
        rows.sort_by(|left, right| row_key(left).cmp(&row_key(right)));
    }
    Ok(out)
}

/// [`override_callers_at`] over the live graph, read through the same view
/// an impact answer reads, so a reference answer and an impact answer about
/// one method list one set of override callers.
pub fn override_callers<G: kin_model::graph::GraphStore>(
    store: &G,
    families: &[(EntityId, Vec<Entity>)],
    skip: impl Fn(&Entity, bool) -> bool,
) -> Result<OverrideCallers, ReviewError> {
    override_callers_at(&crate::impact::LiveGraph(store), families, skip)
}

fn row_key(row: &OverrideCaller) -> (Option<String>, String, EntityId) {
    (
        row.entity
            .file_origin
            .as_ref()
            .map(|file| file.0.clone())
            .or_else(|| row.entity.span.as_ref().map(|span| span.file.0.clone())),
        row.entity.name.clone(),
        row.entity.id,
    )
}

#[cfg(test)]
mod tests {
    use kin_model::entity::{
        EntityKind, EntityMetadata, EntityRole, FingerprintAlgorithm, SemanticFingerprint,
        SourceSpan, Visibility,
    };
    use kin_model::ids::{FilePathId, Hash256, RelationId};
    use kin_model::relation::{Relation, RelationOrigin};
    use kin_model::{EntityStore, LanguageId};

    use super::*;
    use crate::diff::{EntityChange, EntityChangeKind, SemanticDiff};
    use crate::impact::analyze_impact;

    fn entity(name: &str, file: &str, kind: EntityKind) -> Entity {
        Entity {
            id: EntityId::new(),
            kind,
            name: name.to_string(),
            language: LanguageId::Python,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([0; 32]),
                signature_hash: Hash256::from_bytes([0; 32]),
                behavior_hash: Hash256::from_bytes([0; 32]),
                equivalence_hash: Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(FilePathId::new(file)),
            span: Some(SourceSpan {
                file: FilePathId::new(file),
                start_byte: 0,
                end_byte: 10,
                start_line: 1,
                start_col: 0,
                end_line: 2,
                end_col: 0,
            }),
            signature: format!("def {name}(self, obj)"),
            visibility: Visibility::Public,
            role: EntityRole::Source,
            doc_summary: None,
            metadata: EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    fn method(name: &str, file: &str) -> Entity {
        entity(name, file, EntityKind::Method)
    }

    fn edge(kind: RelationKind, src: &Entity, dst: &Entity, confidence: f32) -> Relation {
        Relation {
            id: RelationId::new(),
            kind,
            src: GraphNodeId::Entity(src.id),
            dst: GraphNodeId::Entity(dst.id),
            confidence,
            origin: RelationOrigin::Parsed,
            created_in: None,
            import_source: None,
            evidence: vec![],
        }
    }

    fn calls(src: &Entity, dst: &Entity) -> Relation {
        edge(RelationKind::Calls, src, dst, 1.0)
    }

    /// The receiver-method fan-out tier: `x.dumps()` with nothing settling
    /// `x`, linked to every method of that name.
    fn guessed_call(src: &Entity, dst: &Entity) -> Relation {
        edge(
            RelationKind::Calls,
            src,
            dst,
            kin_index::resolution::RECEIVER_NAME_FANOUT_CONFIDENCE,
        )
    }

    fn impact_of(graph: &kin_db::InMemoryGraph, changed: &Entity) -> crate::ImpactReport {
        let diff = SemanticDiff {
            entity_changes: vec![EntityChange {
                entity_id: changed.id,
                kind: EntityChangeKind::Modified {
                    old: changed.clone(),
                    new: changed.clone(),
                },
            }],
            ..Default::default()
        };
        analyze_impact(graph, &diff).unwrap()
    }

    fn names(rows: &[OverrideCaller]) -> Vec<(&str, &str, bool)> {
        rows.iter()
            .map(|row| {
                let call = row.primary().unwrap();
                (
                    row.entity.name.as_str(),
                    call.override_name.as_str(),
                    call.proven,
                )
            })
            .collect()
    }

    /// The Flask shape: `flask.json.dumps` calls the base through
    /// `current_app.json`, and `DefaultJSONProvider.response` calls
    /// `self.dumps(...)`, which the resolver settled on the override. A
    /// signature change to the base reaches both, and the second is labelled
    /// with the override it calls.
    #[test]
    fn a_caller_of_the_override_is_listed_beside_the_callers_of_the_base() {
        let graph = kin_db::InMemoryGraph::new();
        let base = method("JSONProvider.dumps", "src/flask/json/provider.py");
        let default = method("DefaultJSONProvider.dumps", "src/flask/json/provider.py");
        let response = method("DefaultJSONProvider.response", "src/flask/json/provider.py");
        let module_dumps = entity("dumps", "src/flask/json/__init__.py", EntityKind::Function);
        // Calls both the base and the override, proven: listed once, as a
        // caller of the base.
        let both = entity("roundtrip", "src/flask/json/tag.py", EntityKind::Function);
        for entity in [&base, &default, &response, &module_dumps, &both] {
            graph.upsert_entity(entity).unwrap();
        }
        for relation in [
            edge(RelationKind::Overrides, &default, &base, 1.0),
            calls(&module_dumps, &base),
            calls(&response, &default),
            calls(&both, &base),
            calls(&both, &default),
        ] {
            graph.upsert_relation(&relation).unwrap();
        }

        let report = impact_of(&graph, &base);
        let mut callers: Vec<_> = report
            .affected_callers
            .iter()
            .map(|entity| entity.name.as_str())
            .collect();
        callers.sort();
        assert_eq!(callers, ["dumps", "roundtrip"]);
        assert_eq!(
            names(&report.affected_override_callers),
            [(
                "DefaultJSONProvider.response",
                "DefaultJSONProvider.dumps",
                true
            )]
        );
        let call = report.affected_override_callers[0].primary().unwrap();
        assert_eq!(call.override_id, default.id);
        assert_eq!(call.changed_id, base.id);
        assert_eq!(call.resolution, "type_resolved");
        assert!(report.override_caller_candidates.is_empty());
        // The override caller is affected; the base's own counts, which match
        // what references report for it, are not widened by it.
        assert_eq!(report.entity_impact(&base.id).unwrap().consumer_count, 2);
        assert!(report.total_affected() >= 4);

        // The text report names the override each caller reaches it through.
        let text = crate::format::format_impact(&report);
        assert!(
            text.contains("DefaultJSONProvider.response (Method), calls DefaultJSONProvider.dumps (type_resolved)"),
            "{text}"
        );
    }

    /// An override of an override is three edges from the base, past the
    /// two-hop walk, and its caller is still listed, labelled with the
    /// deepest override and attributed to the changed base.
    #[test]
    fn a_caller_of_an_override_of_an_override_is_listed() {
        let graph = kin_db::InMemoryGraph::new();
        let base = method("Base.render", "pkg/base.py");
        let middle = method("Middle.render", "pkg/middle.py");
        let leaf = method("Leaf.render", "pkg/leaf.py");
        let deep_caller = entity("paint", "pkg/paint.py", EntityKind::Function);
        let middle_caller = entity("draw", "pkg/draw.py", EntityKind::Function);
        for entity in [&base, &middle, &leaf, &deep_caller, &middle_caller] {
            graph.upsert_entity(entity).unwrap();
        }
        for relation in [
            edge(RelationKind::Overrides, &middle, &base, 1.0),
            edge(RelationKind::Overrides, &leaf, &middle, 1.0),
            calls(&deep_caller, &leaf),
            calls(&middle_caller, &middle),
            // An override calling the method it overrides is a family member,
            // never a caller row.
            calls(&leaf, &middle),
        ] {
            graph.upsert_relation(&relation).unwrap();
        }
        let report = impact_of(&graph, &base);
        assert_eq!(
            names(&report.affected_override_callers),
            [
                ("draw", "Middle.render", true),
                ("paint", "Leaf.render", true)
            ]
        );
        assert!(report.affected_override_callers.iter().all(|row| row
            .primary()
            .unwrap()
            .changed_id
            == base.id));
        // The deep caller was out of the two-hop reach entirely before.
        assert!(!report
            .affected_dependents
            .iter()
            .any(|entity| entity.id == deep_caller.id));
    }

    /// A member implementing a declared contract member is in the family; a
    /// class implementing the interface names no member and is not.
    #[test]
    fn a_caller_of_an_implementation_is_listed_and_a_type_level_implements_is_not() {
        let graph = kin_db::InMemoryGraph::new();
        let contract = method("Encoder.encode", "src/codec.ts");
        let implementation = method("Utf8Encoder.encode", "src/utf8.ts");
        let class = entity("Utf8Encoder", "src/utf8.ts", EntityKind::Class);
        let sender = entity("send_packet", "src/net.ts", EntityKind::Function);
        let builder = entity("build", "src/build.ts", EntityKind::Function);
        for entity in [&contract, &implementation, &class, &sender, &builder] {
            graph.upsert_entity(entity).unwrap();
        }
        for relation in [
            edge(RelationKind::Implements, &implementation, &contract, 1.0),
            edge(RelationKind::Implements, &class, &contract, 1.0),
            calls(&sender, &implementation),
            // A caller of the class is not a caller of a family member.
            calls(&builder, &class),
        ] {
            graph.upsert_relation(&relation).unwrap();
        }
        let report = impact_of(&graph, &contract);
        assert_eq!(
            report
                .affected_overrides
                .iter()
                .map(|entity| entity.name.as_str())
                .collect::<Vec<_>>(),
            ["Utf8Encoder.encode"]
        );
        assert_eq!(
            names(&report.affected_override_callers),
            [("send_packet", "Utf8Encoder.encode", true)]
        );
    }

    /// A same-named method in an unrelated hierarchy shares no edge with the
    /// family, so neither it nor its callers are in the answer. A call whose
    /// receiver nothing settles was linked to every method of the name: where
    /// that guess reaches an override and not the base, it is a candidate to
    /// check, never proof, and where it reaches the base too it is already a
    /// listed caller of the base and is not repeated.
    #[test]
    fn an_unrelated_namesake_is_left_out_and_a_name_matched_call_is_a_candidate() {
        let graph = kin_db::InMemoryGraph::new();
        let base = method("JSONProvider.dumps", "src/flask/json/provider.py");
        let default = method("DefaultJSONProvider.dumps", "src/flask/json/provider.py");
        let namesake = method("TaggedJSONSerializer.dumps", "src/flask/json/tag.py");
        let namesake_caller = entity(
            "SecureCookieSessionInterface.save",
            "src/flask/sessions.py",
            EntityKind::Method,
        );
        let guessed = entity("encode_any", "src/extension.py", EntityKind::Function);
        let guessed_everywhere = entity("encode_all", "src/extension.py", EntityKind::Function);
        // Proven on one override, guessed on another: one row, proven.
        let mixed = entity("mixed", "src/mixed.py", EntityKind::Function);
        let other_override = method("CompactJSONProvider.dumps", "src/compact.py");
        for entity in [
            &base,
            &default,
            &namesake,
            &namesake_caller,
            &guessed,
            &guessed_everywhere,
            &mixed,
            &other_override,
        ] {
            graph.upsert_entity(entity).unwrap();
        }
        for relation in [
            edge(RelationKind::Overrides, &default, &base, 1.0),
            edge(RelationKind::Overrides, &other_override, &base, 1.0),
            calls(&namesake_caller, &namesake),
            guessed_call(&guessed, &default),
            guessed_call(&guessed, &namesake),
            guessed_call(&guessed_everywhere, &base),
            guessed_call(&guessed_everywhere, &default),
            guessed_call(&mixed, &default),
            calls(&mixed, &other_override),
        ] {
            graph.upsert_relation(&relation).unwrap();
        }
        let report = impact_of(&graph, &base);
        let listed: Vec<&str> = report
            .affected_callers
            .iter()
            .chain(&report.affected_overrides)
            .map(|entity| entity.name.as_str())
            .chain(
                report
                    .affected_override_callers
                    .iter()
                    .chain(&report.override_caller_candidates)
                    .map(|row| row.entity.name.as_str()),
            )
            .collect();
        for absent in [
            "TaggedJSONSerializer.dumps",
            "SecureCookieSessionInterface.save",
        ] {
            assert!(!listed.contains(&absent), "{absent} in {listed:?}");
        }
        assert_eq!(
            names(&report.override_caller_candidates),
            [("encode_any", "DefaultJSONProvider.dumps", false)]
        );
        assert_eq!(
            report.override_caller_candidates[0]
                .primary()
                .unwrap()
                .resolution,
            "name_only"
        );
        let mixed_row = &report.affected_override_callers[0];
        assert_eq!(
            names(&report.affected_override_callers),
            [("mixed", "CompactJSONProvider.dumps", true)]
        );
        assert_eq!(mixed_row.calls.len(), 2);
        assert!(!mixed_row.calls[1].proven);
        // The guess that also reached the base is a listed caller of the base.
        assert!(report
            .affected_callers
            .iter()
            .any(|entity| entity.id == guessed_everywhere.id));
        // A candidate is not an affected entity.
        let mut proven = crate::ImpactReport {
            override_caller_candidates: report.override_caller_candidates.clone(),
            ..Default::default()
        };
        assert_eq!(proven.total_affected(), 0);
        assert!(!proven.is_empty());
        proven.override_caller_candidates.clear();
        assert!(proven.is_empty());
    }
}
