// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! What a commit's operations did, entity by entity, for its receipt.
//!
//! A receipt counted entity deltas and listed the files written. Neither
//! says which entity an operation changed, created or removed, so a reader
//! of the receipt could not tell that its edit landed on the function it
//! meant without reading that function again. Each operation that names an
//! entity is answered with a row for that entity: its id, its name, its
//! kind, the module the graph places it in, and what the operation did.

use std::collections::BTreeSet;

use kin_mcp::write_reply::{CHANGED, CREATED, REMOVED};
use kin_mcp::{McpMutationOperation, McpMutationPayload};
use kin_model::graph::GraphStore;
use kin_model::{Entity, EntityDelta, EntityId, EntityKind, SemanticChange};
use serde_json::{json, Value};

/// Most rows one receipt lists.
const CHANGES_MAX: usize = 64;

fn row<G: GraphStore>(graph: &G, entity: &Entity, change: &str) -> Value {
    let mut row = json!({
        "entity_id": entity.id,
        "name": entity.name,
        "kind": entity.kind,
        "change": change,
    });
    if let Some(module) = kin_mcp::semantic_owners::module_label(graph, entity) {
        row["module"] = json!(module);
    }
    // A name its source unit declares more than once: the sentence says
    // which of them by its id.
    kin_mcp::semantic_owners::mark_shared_name(graph, entity, &mut row);
    row
}

/// One row for each entity `operations` changed, created or removed, in the
/// order the operations were given, read from the committed `change` and the
/// graph as the commit left it. An operation that names no entity, a
/// relationship or a unit's imports, adds no row.
pub(crate) fn entity_changes<G: GraphStore>(
    graph: &G,
    change: &SemanticChange,
    operations: &[McpMutationOperation],
) -> Vec<Value> {
    let mut rows = Vec::new();
    let mut listed: BTreeSet<EntityId> = BTreeSet::new();
    let added = || {
        change.entity_deltas.iter().filter_map(|delta| match delta {
            EntityDelta::Added { new }
                if new.file_origin.is_some() && !kin_model::is_file_module_surface(new) =>
            {
                Some(new)
            }
            _ => None,
        })
    };
    for operation in operations {
        let Some(payload) = operation.payload.as_ref() else {
            continue;
        };
        match payload {
            McpMutationPayload::EntitySourceBase(base)
            | McpMutationPayload::EntitySourcePatch(kin_mcp::source_base::EntitySourcePatch {
                source_base: base,
                ..
            }) => {
                if let Ok(Some(entity)) = graph.get_entity(&base.entity_id) {
                    if listed.insert(entity.id) {
                        rows.push(row(graph, &entity, CHANGED));
                    }
                }
            }
            McpMutationPayload::EntityRemove(remove) => {
                let removed = change.entity_deltas.iter().find_map(|delta| match delta {
                    EntityDelta::Removed { old } if old.id == remove.source_base.entity_id => {
                        Some(old)
                    }
                    _ => None,
                });
                if let Some(old) = removed {
                    if listed.insert(old.id) {
                        rows.push(row(graph, old, REMOVED));
                    }
                }
            }
            McpMutationPayload::EntityCreate(create) => {
                // A unit-addressed creation names what its body declares. An
                // anchored one names one identifier, and a method is recorded
                // behind its class: the class the anchor is, or the class the
                // anchor method belongs to.
                let requested =
                    crate::unit_lifecycle::requested_names(std::slice::from_ref(operation));
                let anchor = create
                    .anchor()
                    .and_then(|(base, _)| graph.get_entity(&base.entity_id).ok().flatten());
                let member = anchor.as_ref().and_then(|anchor| {
                    let owner = match anchor.kind {
                        EntityKind::Class => Some(anchor.name.as_str()),
                        EntityKind::Method => anchor.name.rsplit_once('.').map(|(owner, _)| owner),
                        _ => None,
                    }?;
                    Some((
                        format!("{owner}.{}", create.name),
                        anchor.file_origin.clone(),
                    ))
                });
                for new in added() {
                    let named = requested.contains(&new.name)
                        || member.as_ref().is_some_and(|(name, file)| {
                            new.kind == EntityKind::Method
                                && &new.name == name
                                && &new.file_origin == file
                        });
                    if named && listed.insert(new.id) {
                        rows.push(row(graph, new, CREATED));
                    }
                }
            }
            _ => {}
        }
        if rows.len() >= CHANGES_MAX {
            rows.truncate(CHANGES_MAX);
            break;
        }
    }
    rows
}
