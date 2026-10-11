// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Where entities are projected in the working copy, for editors.
//!
//! The graph is the authority and the working copy is a projection of it. An
//! editor still has to put a cursor somewhere, so this module answers one
//! question at that boundary: for each entity id, the path its declaration is
//! projected to, relative to the repository root, and the declaration's span
//! there. It reads the graph and nothing else. An id the graph cannot place is
//! answered as not projected, with the reason, and is never resolved by
//! searching the working copy.
//!
//! This is a materialization surface. The daemon serves it as
//! `POST /projection/places` and the CLI as `kin places`, and it is absent from
//! the MCP tool list and from every agent reply on purpose: agents address
//! entities by id and relationship, never by path.

use kin_model::{EntityId, EntityKind, EntityStore, RepoPath};
use serde::{Deserialize, Serialize};

/// The most entity ids one call answers. A caller with more splits them.
pub const MAX_PLACES_PER_REQUEST: usize = 512;

/// How a place counts columns: bytes of the line's UTF-8 text, as the parser
/// records a declaration's start and end. An editor that counts in characters
/// converts against the line it opened.
pub const COLUMN_UNIT: &str = "utf8_byte";

/// One call: the entities to place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacesRequest {
    /// Entity ids, bare or spelled `entity:<uuid>`. Answered in this order,
    /// each distinct id once.
    pub entity_ids: Vec<String>,
}

/// The answer to a [`PlacesRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacesResponse {
    /// The working-copy directory every `path` is relative to, when the
    /// answering daemon knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_copy_root: Option<String>,
    /// Always [`COLUMN_UNIT`].
    pub column_unit: String,
    /// One place per distinct requested id, in request order.
    pub places: Vec<EntityPlace>,
}

/// Where one entity is projected, or why it is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EntityPlace {
    /// The entity's declaration is in the working copy at this span.
    Projected {
        /// The id exactly as it was asked for.
        entity_id: String,
        name: String,
        kind: EntityKind,
        /// Relative to the repository root, `/`-separated.
        path: String,
        /// 1-based line the declaration starts on.
        start_line: u32,
        /// 1-based column the declaration starts at, in [`COLUMN_UNIT`].
        start_column: u32,
        /// 1-based line the declaration ends on.
        end_line: u32,
        /// 1-based column just past the declaration's last byte.
        end_column: u32,
    },
    /// The graph does not place this entity in the working copy.
    NotProjected {
        /// The id exactly as it was asked for.
        entity_id: String,
        reason: NotProjectedReason,
        /// The reason as a sentence a reader can be shown.
        detail: String,
        /// The entity that replaced this one, for [`NotProjectedReason::Superseded`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        superseded_by: Option<String>,
    },
}

impl EntityPlace {
    /// The id this place answers, exactly as it was asked for.
    pub fn entity_id(&self) -> &str {
        match self {
            EntityPlace::Projected { entity_id, .. }
            | EntityPlace::NotProjected { entity_id, .. } => entity_id,
        }
    }

    pub fn is_projected(&self) -> bool {
        matches!(self, EntityPlace::Projected { .. })
    }
}

/// Why an entity has no place in the working copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotProjectedReason {
    /// The request named something that is not an entity id.
    InvalidId,
    /// The graph holds no entity with this id.
    UnknownEntity,
    /// A newer version of the entity replaced this one.
    Superseded,
    /// The entity has no declaration of its own, as a derived entity or one
    /// the graph has not placed yet does not.
    NoDeclaration,
    /// The entity's declaration is not part of the working copy the graph
    /// projects.
    NotInWorkingCopy,
}

impl NotProjectedReason {
    pub fn detail(self) -> &'static str {
        match self {
            NotProjectedReason::InvalidId => "This is not an entity id.",
            NotProjectedReason::UnknownEntity => "The graph holds no entity with this id.",
            NotProjectedReason::Superseded => {
                "A newer version of this entity replaced it. Ask for that entity instead."
            }
            NotProjectedReason::NoDeclaration => {
                "This entity has no declaration of its own in the working copy."
            }
            NotProjectedReason::NotInWorkingCopy => {
                "This entity's declaration is not part of the working copy Kin projects."
            }
        }
    }
}

fn not_projected(entity_id: &str, reason: NotProjectedReason) -> EntityPlace {
    EntityPlace::NotProjected {
        entity_id: entity_id.to_string(),
        reason,
        detail: reason.detail().to_string(),
        superseded_by: None,
    }
}

/// An entity id, bare or spelled `entity:<uuid>` as a call-site row names one.
fn parse_entity_id(raw: &str) -> Option<EntityId> {
    let trimmed = raw.trim();
    let bare = trimmed.strip_prefix("entity:").unwrap_or(trimmed);
    serde_json::from_value(serde_json::Value::String(bare.to_string())).ok()
}

/// Place one entity from what `store` holds.
pub fn entity_place<S: EntityStore + ?Sized>(
    store: &S,
    requested: &str,
) -> Result<EntityPlace, S::Error> {
    let Some(id) = parse_entity_id(requested) else {
        return Ok(not_projected(requested, NotProjectedReason::InvalidId));
    };
    let Some(entity) = store.get_entity(&id)? else {
        return Ok(not_projected(requested, NotProjectedReason::UnknownEntity));
    };
    if let Some(successor) = entity.superseded_by {
        return Ok(EntityPlace::NotProjected {
            entity_id: requested.to_string(),
            reason: NotProjectedReason::Superseded,
            detail: NotProjectedReason::Superseded.detail().to_string(),
            superseded_by: Some(successor.to_string()),
        });
    }
    let Some(span) = entity.span else {
        return Ok(not_projected(requested, NotProjectedReason::NoDeclaration));
    };
    // A place is a path the graph's own repository tree tracks. A span whose
    // path the tree does not hold names nothing the working copy projects, so
    // it is reported rather than handed to an editor to open.
    let tracked = RepoPath::from_utf8(span.file.0.clone())
        .ok()
        .and_then(|path| store.artifact_id_at_path(&path))
        .is_some();
    if !tracked {
        return Ok(not_projected(
            requested,
            NotProjectedReason::NotInWorkingCopy,
        ));
    }
    Ok(EntityPlace::Projected {
        entity_id: requested.to_string(),
        name: entity.name,
        kind: entity.kind,
        path: span.file.0,
        start_line: span.start_line.saturating_add(1),
        start_column: span.start_col.saturating_add(1),
        end_line: span.end_line.saturating_add(1),
        end_column: span.end_col.saturating_add(1),
    })
}

/// Place every distinct requested id, in request order.
///
/// The caller bounds the batch at [`MAX_PLACES_PER_REQUEST`]. A graph read
/// that fails fails the whole call, because a read error is not an answer
/// about any one entity.
pub fn entity_places<S: EntityStore + ?Sized>(
    store: &S,
    entity_ids: &[String],
) -> Result<Vec<EntityPlace>, S::Error> {
    let mut seen = std::collections::HashSet::new();
    let mut places = Vec::with_capacity(entity_ids.len());
    for requested in entity_ids {
        if seen.insert(requested.as_str()) {
            places.push(entity_place(store, requested)?);
        }
    }
    Ok(places)
}

/// Whether any place in an answer says the graph holds no such entity, which
/// is the one answer a read spanned by a graph writer may not certify.
pub fn claims_absence(places: &[EntityPlace]) -> bool {
    places.iter().any(|place| {
        matches!(
            place,
            EntityPlace::NotProjected {
                reason: NotProjectedReason::UnknownEntity,
                ..
            }
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_db::InMemoryGraph;
    use kin_model::{
        Entity, EntityMetadata, EntityRole, FilePathId, FingerprintAlgorithm, LanguageId,
        SemanticFingerprint, SourceSpan, Visibility,
    };

    fn track(graph: &InMemoryGraph, path: &str) {
        graph
            .apply_transaction_delta(&kin_model::TransactionDelta {
                entity_deltas: vec![],
                relation_deltas: vec![],
                tree_deltas: vec![kin_model::TreeDelta::Added {
                    artifact_id: kin_model::ArtifactId::new(),
                    new: kin_model::LocatedEntry::new(
                        RepoPath::from_utf8(path).unwrap(),
                        kin_model::TreeEntry::blob(kin_model::Hash256::from_bytes([7; 32]), false),
                    ),
                }],
                admission_policy_delta: None,
                external_reference_deltas: Vec::new(),
                resolution_record_deltas: Vec::new(),
            })
            .unwrap();
    }

    fn entity(name: &str, file: &str, span: Option<(u32, u32, u32, u32)>) -> Entity {
        Entity {
            id: EntityId::new(),
            kind: EntityKind::Function,
            name: name.to_string(),
            language: LanguageId::Python,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: kin_model::Hash256::from_bytes([1; 32]),
                signature_hash: kin_model::Hash256::from_bytes([2; 32]),
                behavior_hash: kin_model::Hash256::from_bytes([3; 32]),
                equivalence_hash: kin_model::Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(FilePathId::new(file)),
            span: span.map(|(start_line, start_col, end_line, end_col)| SourceSpan {
                file: FilePathId::new(file),
                start_byte: 0,
                end_byte: 0,
                start_line,
                start_col,
                end_line,
                end_col,
            }),
            signature: format!("def {name}()"),
            visibility: Visibility::Public,
            role: EntityRole::Source,
            doc_summary: None,
            metadata: EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    #[test]
    fn a_tracked_declaration_is_placed_with_one_based_lines_and_columns() {
        let graph = InMemoryGraph::new();
        track(&graph, "billing/invoice.py");
        let total = entity("invoice_total", "billing/invoice.py", Some((32, 0, 38, 59)));
        graph.upsert_entity(&total).unwrap();

        let place = entity_place(&graph, &total.id.to_string()).unwrap();

        assert_eq!(
            place,
            EntityPlace::Projected {
                entity_id: total.id.to_string(),
                name: "invoice_total".to_string(),
                kind: EntityKind::Function,
                path: "billing/invoice.py".to_string(),
                start_line: 33,
                start_column: 1,
                end_line: 39,
                end_column: 60,
            }
        );
    }

    #[test]
    fn an_id_spelled_as_a_call_site_names_it_is_placed_and_echoed_as_asked() {
        let graph = InMemoryGraph::new();
        track(&graph, "billing/tax.py");
        let tax = entity("tax_for", "billing/tax.py", Some((16, 4, 18, 0)));
        graph.upsert_entity(&tax).unwrap();
        let asked = format!("entity:{}", tax.id);

        let place = entity_place(&graph, &asked).unwrap();

        assert!(place.is_projected(), "{place:?}");
        assert_eq!(place.entity_id(), asked);
    }

    #[test]
    fn every_miss_is_answered_not_projected_with_its_reason() {
        let graph = InMemoryGraph::new();
        track(&graph, "billing/money.py");
        let derived = entity("derived_member", "billing/money.py", None);
        graph.upsert_entity(&derived).unwrap();
        let untracked = entity("stray", "scratch/stray.py", Some((0, 0, 1, 0)));
        graph.upsert_entity(&untracked).unwrap();
        let mut replaced = entity("old_total", "billing/money.py", Some((3, 0, 5, 0)));
        let successor = EntityId::new();
        replaced.superseded_by = Some(successor);
        graph.upsert_entity(&replaced).unwrap();
        let unknown = EntityId::new().to_string();

        let places = entity_places(
            &graph,
            &[
                "not-an-entity".to_string(),
                unknown.clone(),
                derived.id.to_string(),
                untracked.id.to_string(),
                replaced.id.to_string(),
            ],
        )
        .unwrap();

        let reasons: Vec<_> = places
            .iter()
            .map(|place| match place {
                EntityPlace::NotProjected { reason, .. } => *reason,
                EntityPlace::Projected { .. } => panic!("placed a miss: {place:?}"),
            })
            .collect();
        assert_eq!(
            reasons,
            vec![
                NotProjectedReason::InvalidId,
                NotProjectedReason::UnknownEntity,
                NotProjectedReason::NoDeclaration,
                NotProjectedReason::NotInWorkingCopy,
                NotProjectedReason::Superseded,
            ]
        );
        match &places[4] {
            EntityPlace::NotProjected { superseded_by, .. } => {
                assert_eq!(
                    superseded_by.as_deref(),
                    Some(successor.to_string().as_str())
                )
            }
            other => panic!("{other:?}"),
        }
        assert!(claims_absence(&places));
        assert!(!claims_absence(&places[2..]));
    }

    #[test]
    fn a_batch_answers_each_distinct_id_once_in_request_order() {
        let graph = InMemoryGraph::new();
        track(&graph, "billing/report.py");
        let report = entity("monthly_report", "billing/report.py", Some((6, 0, 15, 16)));
        let largest = entity(
            "largest_invoice",
            "billing/report.py",
            Some((18, 0, 22, 70)),
        );
        graph.upsert_entity(&report).unwrap();
        graph.upsert_entity(&largest).unwrap();

        let places = entity_places(
            &graph,
            &[
                largest.id.to_string(),
                report.id.to_string(),
                largest.id.to_string(),
            ],
        )
        .unwrap();

        let ids: Vec<_> = places.iter().map(EntityPlace::entity_id).collect();
        assert_eq!(ids, vec![largest.id.to_string(), report.id.to_string()]);
    }

    #[test]
    fn the_wire_shape_tags_each_place_with_its_status() {
        let response = PlacesResponse {
            working_copy_root: None,
            column_unit: COLUMN_UNIT.to_string(),
            places: vec![
                EntityPlace::Projected {
                    entity_id: "a".to_string(),
                    name: "run".to_string(),
                    kind: EntityKind::Function,
                    path: "caller.py".to_string(),
                    start_line: 3,
                    start_column: 1,
                    end_line: 4,
                    end_column: 25,
                },
                not_projected("b", NotProjectedReason::UnknownEntity),
            ],
        };

        let wire = serde_json::to_value(&response).unwrap();

        assert_eq!(
            wire,
            serde_json::json!({
                "column_unit": "utf8_byte",
                "places": [
                    {"status": "projected", "entity_id": "a", "name": "run", "kind": "function",
                     "path": "caller.py", "start_line": 3, "start_column": 1, "end_line": 4,
                     "end_column": 25},
                    {"status": "not_projected", "entity_id": "b", "reason": "unknown_entity",
                     "detail": "The graph holds no entity with this id."}
                ]
            })
        );
        let read: PlacesResponse = serde_json::from_value(wire).unwrap();
        assert_eq!(read, response);
    }
}
