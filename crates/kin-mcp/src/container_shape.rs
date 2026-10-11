// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A container entity served as its shape rather than its whole body.
//!
//! A class, a module or an object literal holds other entities, and its body
//! is theirs as much as its own. Serving it whole hands a reader every member
//! it holds in one read: on a real web framework one class body was 37 KB,
//! most of its file, which is a whole-file read by another name. The members
//! are entities in their own right, each readable by id, so the container is
//! served as what only it can say: its own header (the declaration line, its
//! docstring or leading comment, and whatever it declares before its first
//! member) and its members by id and signature. A reader drills into the one
//! member it needs. Module containers always serve a member inventory, even
//! when small or missing member coverage. They expose no aggregate body.
//!
//! Language agnostic by construction: a container is an entity that is not
//! itself callable code and holds at least one member through a `Contains`
//! edge, which every adapter that nests declarations records. The header is
//! cut at the first member's recorded span, so no language's syntax is read
//! here.

use kin_model::{Entity, EntityKind, GraphNodeId, GraphStore, RelationKind};
use serde_json::{json, Map, Value};

/// The `body_unavailable` reason a context row carries while it is served as
/// its shape. A row carrying it is rendered as a shape, never as a missing
/// body.
pub const CONTAINER_SHAPE_REASON: &str = "container_shape";

/// The `projection` a shape row publishes.
pub const SHAPE_PROJECTION: &str = "Shape";

/// A container whose whole body is at most this many bytes is served whole:
/// reading its members one by one would cost more round trips than the bytes
/// it saves.
pub const CONTAINER_INLINE_MAX_BYTES: usize = 4_096;

/// The most header bytes a shape carries. A longer header is cut at a line
/// boundary and says so in `header_complete`.
pub const CONTAINER_HEADER_MAX_BYTES: usize = 2_048;

/// The most members a shape lists with their signatures. The rest are listed
/// by id and name alone, so every member stays addressable.
pub const CONTAINER_MEMBERS_WITH_SIGNATURE_MAX: usize = 80;

/// The fields a shape adds to a row, all of which leave together when the row
/// is not the one the shape was served on.
pub const SHAPE_KEYS: [&str; 14] = [
    "projection",
    "header",
    "header_complete",
    "members",
    "body_bytes",
    "body_omitted",
    "member_coverage",
    "semantic_gap",
    "source_base_unavailable",
    "member_coverage_reason",
    "member_coverage_scope",
    "member_counts",
    "members_unavailable",
    "body_bytes_basis",
];

/// The largest container body `full_body` serves whole: the bound every body
/// has ([`crate::source_pages::FULL_BODY_MAX_BYTES`]). A larger one is served
/// as its shape even when its whole body is asked for, with a note that says
/// so: its members are entities, each readable by id, and one reply that holds
/// all of them is a whole-file read by another name.
pub const CONTAINER_FULL_BODY_MAX_BYTES: usize = crate::source_pages::FULL_BODY_MAX_BYTES;

/// How a shape tells its reader to get more.
pub const SHAPE_NOTE: &str = "Modules are served as their recorded member inventory, which does \
     not establish that every declaration was parsed. A class \
     or object literal over 4 KB is served as its outline: its header and its members by id \
     and signature. Read the member you need by its id.";

pub const MODULE_MEMBER_GAP: &str = "module_member_coverage_unavailable";

/// An inventory has no aggregate source or edit base by policy. This does not
/// say whether its underlying source bytes can be resolved for a narrow site.
pub const MODULE_INVENTORY_ONLY: &str = "module_inventory_only";

struct MemberInventory {
    members: Vec<Entity>,
    recorded: Option<usize>,
    unavailable: Vec<Value>,
    missing: usize,
    unreadable: usize,
    invalid_targets: usize,
}

impl MemberInventory {
    fn reason(&self) -> &'static str {
        if self.recorded.is_none() {
            "contains_read_failed"
        } else if self.invalid_targets > 0 {
            "contains_invalid_targets"
        } else if !self.unavailable.is_empty() {
            "contains_targets_unavailable"
        } else if self.recorded == Some(0) {
            "no_recorded_contains_members"
        } else {
            "recorded_contains_members"
        }
    }

    fn coverage_gap(&self) -> bool {
        self.members.is_empty()
            || self.recorded != Some(self.members.len())
            || self.invalid_targets > 0
    }
}

/// Observe one outgoing Contains read. A missing/unreadable target stays a
/// named gap rather than silently disappearing from the inventory. Incoming
/// edges and repeated edges do not invent additional members.
fn observe_members<E>(
    entity: &Entity,
    relations: std::result::Result<Vec<kin_model::Relation>, E>,
    mut get_entity: impl FnMut(&kin_model::EntityId) -> std::result::Result<Option<Entity>, E>,
) -> MemberInventory {
    let mut inventory = MemberInventory {
        members: Vec::new(),
        recorded: None,
        unavailable: Vec::new(),
        missing: 0,
        unreadable: 0,
        invalid_targets: 0,
    };
    let Ok(relations) = relations else {
        return inventory;
    };
    let mut ids = std::collections::BTreeSet::new();
    for relation in relations {
        if relation.kind != RelationKind::Contains || relation.src != GraphNodeId::Entity(entity.id)
        {
            continue;
        }
        match relation.dst.as_entity() {
            Some(id) if id != entity.id => {
                ids.insert(id);
            }
            _ => inventory.invalid_targets += 1,
        }
    }
    inventory.recorded = Some(ids.len());
    for id in ids {
        match get_entity(&id) {
            Ok(Some(member)) if member.id == id => inventory.members.push(member),
            Ok(Some(_)) => {
                inventory.unreadable += 1;
                inventory
                    .unavailable
                    .push(json!({"id": id, "reason": "member_entity_identity_mismatch"}));
            }
            Ok(None) => {
                inventory.missing += 1;
                inventory
                    .unavailable
                    .push(json!({"id": id, "reason": "member_entity_missing"}));
            }
            Err(_) => {
                inventory.unreadable += 1;
                inventory
                    .unavailable
                    .push(json!({"id": id, "reason": "member_entity_read_failed"}));
            }
        }
    }
    inventory.members.sort_by(|left, right| {
        let start = |entity: &Entity| entity.span.as_ref().map(|span| span.start_byte);
        start(left)
            .cmp(&start(right))
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });
    inventory
}

/// The graph's inventory of a module, without loading or slicing source bytes.
/// An empty inventory is not evidence that its file contains no declarations.
pub fn module_inventory<G: GraphStore>(store: &G, entity: &Entity) -> Option<Map<String, Value>> {
    if !kin_model::is_module_container(entity) {
        return None;
    }
    let inventory = observe_members(
        entity,
        store.get_relations(&entity.id, &[RelationKind::Contains]),
        |id| store.get_entity(id),
    );
    let coverage_gap = inventory.coverage_gap();
    let mut fields = shape_from(entity, "", &inventory.members);
    fields.insert(
        "body_bytes".into(),
        json!(entity
            .span
            .as_ref()
            .map(|span| span.end_byte.saturating_sub(span.start_byte))),
    );
    fields.insert(
        "source_base_unavailable".into(),
        json!(MODULE_INVENTORY_ONLY),
    );
    fields.insert("body_bytes_basis".into(), json!("recorded_span"));
    fields.insert("member_coverage_scope".into(), json!("recorded_contains"));
    fields.insert("member_coverage_reason".into(), json!(inventory.reason()));
    let mut counts = json!({
        "recorded": inventory.recorded,
        "returned": inventory.members.len(),
    });
    for (key, count) in [
        ("missing", inventory.missing),
        ("unreadable", inventory.unreadable),
        ("invalid_targets", inventory.invalid_targets),
    ] {
        if count > 0 {
            counts[key] = json!(count);
        }
    }
    fields.insert("member_counts".into(), counts);
    if !inventory.unavailable.is_empty() {
        fields.insert("members_unavailable".into(), json!(inventory.unavailable));
    }
    fields.insert(
        "member_coverage".into(),
        json!(if inventory.members.is_empty() {
            "unavailable"
        } else if coverage_gap {
            "partial"
        } else {
            "recorded"
        }),
    );
    if coverage_gap {
        fields.insert("semantic_gap".into(), json!(MODULE_MEMBER_GAP));
    }
    Some(fields)
}

pub fn module_source_reply<G: GraphStore>(store: &G, entity: &Entity) -> Option<Value> {
    let fields = module_inventory(store, entity)?;
    let mut reply = json!({
        "id": entity.id, "name": entity.name, "kind": entity.kind,
        "language": entity.language, "signature": entity.signature,
        "file_path": entity.file_origin, "body": null, "source_base": null,
        "body_complete": false, "note": SHAPE_NOTE,
    });
    reply.as_object_mut().expect("module object").extend(fields);
    Some(reply)
}

/// The note a shape carries when its whole body was asked for and is larger
/// than [`CONTAINER_FULL_BODY_MAX_BYTES`].
fn full_body_refused_note(bytes: usize) -> String {
    format!(
        "full_body serves a container of at most {} KB whole, and this one is {} KB, so it is \
         served as its outline: its header, and its members by id and signature. Read the member \
         you need by its id.",
        CONTAINER_FULL_BODY_MAX_BYTES / 1024,
        bytes.div_ceil(1024)
    )
}

/// Whether this kind is callable code, whose body is the answer itself. A
/// function that holds nested functions is still read as its body.
fn is_code_kind(kind: EntityKind) -> bool {
    matches!(
        kind,
        EntityKind::Function | EntityKind::Method | EntityKind::Test | EntityKind::Macro
    )
}

/// The entities this one contains, in source order. Empty for callable code
/// and for any entity that holds nothing.
pub fn container_members<G: GraphStore>(store: &G, entity: &Entity) -> Vec<Entity> {
    if is_code_kind(entity.kind) {
        return Vec::new();
    }
    observe_members(
        entity,
        store.get_relations(&entity.id, &[RelationKind::Contains]),
        |id| store.get_entity(id),
    )
    .members
}

/// The fields a row or a source reply carries in place of `body` when `entity`
/// is a container over [`CONTAINER_INLINE_MAX_BYTES`], or `None` when the body
/// is to be served as it is.
///
/// `body` must be the entity's exact recorded span: the header is cut from it
/// at the first member's recorded offset.
pub fn shape_fields<G: GraphStore>(
    store: &G,
    entity: &Entity,
    body: &str,
) -> Option<Map<String, Value>> {
    if let Some(fields) = module_inventory(store, entity) {
        return Some(fields);
    }
    if body.len() <= CONTAINER_INLINE_MAX_BYTES {
        return None;
    }
    let members = container_members(store, entity);
    if members.is_empty() {
        return None;
    }
    Some(shape_from(entity, body, &members))
}

fn shape_from(entity: &Entity, body: &str, members: &[Entity]) -> Map<String, Value> {
    // The header ends where the first member recorded in this file begins.
    let first_member = entity.span.as_ref().and_then(|span| {
        members
            .iter()
            .filter_map(|member| member.span.as_ref())
            .filter(|member| {
                member.file == span.file
                    && member.start_byte > span.start_byte
                    && member.start_byte < span.end_byte
            })
            .map(|member| member.start_byte - span.start_byte)
            .min()
    });
    let prelude_end = floor_char_boundary(body, first_member.unwrap_or(body.len()));
    let prelude = body[..prelude_end].trim_end();
    let (header, header_complete) = if prelude.len() <= CONTAINER_HEADER_MAX_BYTES {
        (prelude, true)
    } else {
        let cut = floor_char_boundary(prelude, CONTAINER_HEADER_MAX_BYTES);
        let cut = prelude[..cut].rfind('\n').unwrap_or(cut);
        (prelude[..cut].trim_end(), false)
    };

    let member_rows: Vec<Value> = members
        .iter()
        .enumerate()
        .map(|(index, member)| {
            if index < CONTAINER_MEMBERS_WITH_SIGNATURE_MAX {
                json!({
                    "id": member.id,
                    "name": member.name,
                    "signature": member.signature,
                })
            } else {
                json!({ "id": member.id, "name": member.name })
            }
        })
        .collect();

    let mut fields = Map::new();
    fields.insert("projection".into(), json!(SHAPE_PROJECTION));
    fields.insert("header".into(), json!(header));
    fields.insert("header_complete".into(), json!(header_complete));
    fields.insert("members".into(), Value::Array(member_rows));
    fields.insert("body_bytes".into(), json!(body.len()));
    fields.insert("body_omitted".into(), json!(CONTAINER_SHAPE_REASON));
    fields
}

/// The largest char boundary at or below `index`.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// The tokens a shape row's content costs: its header and every member
/// signature it lists.
pub fn shape_tokens(row: &Map<String, Value>) -> usize {
    let header = row
        .get("header")
        .and_then(Value::as_str)
        .map_or(0, kin_context::estimate_tokens);
    let members: usize = row
        .get("members")
        .and_then(Value::as_array)
        .map_or(0, |members| {
            members
                .iter()
                .map(|member| {
                    member
                        .get("signature")
                        .or_else(|| member.get("name"))
                        .and_then(Value::as_str)
                        .map_or(0, kin_context::estimate_tokens)
                })
                .sum()
        });
    header + members
}

/// Serve an entity source reply as its container shape, in place, unless the
/// caller asked for the whole body of a container no larger than
/// [`CONTAINER_FULL_BODY_MAX_BYTES`]. Returns whether the reply was reshaped.
///
/// `reply` is a `get_entity_source` object carrying `id` and `body`. Every
/// other field it carries (identity, provenance, `source_base`) stays: the
/// shape changes what is served, not what the read is.
pub fn shape_source_reply<G: GraphStore>(store: &G, reply: &mut Value, full_body: bool) -> bool {
    let Some(object) = reply.as_object_mut() else {
        return false;
    };
    let Some(body) = object.get("body").and_then(Value::as_str) else {
        return false;
    };
    let Some(id) = object
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| crate::handlers::common::parse_entity_id(id).ok())
    else {
        return false;
    };
    let Ok(Some(entity)) = store.get_entity(&id) else {
        return false;
    };
    if let Some(fields) = module_inventory(store, &entity) {
        object.remove("body");
        object.remove("source_base");
        object.extend(fields);
        object.insert("note".into(), json!(SHAPE_NOTE));
        return true;
    }
    if full_body && body.len() <= CONTAINER_FULL_BODY_MAX_BYTES {
        return false;
    }
    let Some(fields) = shape_fields(store, &entity, body) else {
        return false;
    };
    let bytes = body.len();
    object.remove("body");
    object.extend(fields);
    let note = if full_body {
        full_body_refused_note(bytes)
    } else {
        SHAPE_NOTE.to_string()
    };
    object.insert("note".into(), json!(note));
    // A class served as its own code carries a base that guards that code.
    // Where that code could not be served and this outline stands in for it,
    // the base read here guards every member's bytes instead, which a write
    // to such a class does not accept, so the outline carries none and says
    // why.
    if crate::module_code::serves_class_code(store, &entity) {
        object.remove("source_base");
        object.insert(
            "source_base_unavailable".into(),
            json!(format!(
                "{}: the class-level code of {} cannot be served now, so this reply is its \
                 outline and carries no source_base. Read and change a member by its own id.",
                crate::module_code::CLASS_CODE_UNAVAILABLE,
                entity.name
            )),
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(file: &kin_model::FilePathId, start: usize, end: usize) -> kin_model::SourceSpan {
        kin_model::SourceSpan {
            file: file.clone(),
            start_byte: start,
            end_byte: end,
            start_line: 1,
            start_col: 0,
            end_line: 1,
            end_col: 0,
        }
    }

    fn entity(name: &str, kind: EntityKind, span: Option<kin_model::SourceSpan>) -> Entity {
        use kin_model::{
            EntityMetadata, EntityRole, FingerprintAlgorithm, Hash256, SemanticFingerprint,
            Visibility,
        };
        Entity {
            id: kin_model::EntityId::new(),
            kind,
            name: name.to_string(),
            language: kin_model::LanguageId::Python,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([0; 32]),
                signature_hash: Hash256::from_bytes([0; 32]),
                behavior_hash: Hash256::from_bytes([0; 32]),
                equivalence_hash: Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: span.as_ref().map(|span| span.file.clone()),
            span,
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

    fn contains(src: kin_model::EntityId, dst: kin_model::EntityId) -> kin_model::Relation {
        kin_model::Relation {
            id: kin_model::RelationId::new(),
            kind: RelationKind::Contains,
            src: GraphNodeId::Entity(src),
            dst: GraphNodeId::Entity(dst),
            confidence: 1.0,
            origin: kin_model::RelationOrigin::Parsed,
            created_in: None,
            import_source: None,
            evidence: Vec::new(),
        }
    }

    fn presented_module_inventory(with_member: bool, with_missing: bool) -> Value {
        use kin_model::EntityStore;
        let graph = kin_db::InMemoryGraph::new();
        let file = kin_model::FilePathId::new("pkg/app.py");
        let module = entity("app", EntityKind::Module, Some(span(&file, 0, 100)));
        graph.upsert_entity(&module).unwrap();
        if with_member {
            let member = entity("run", EntityKind::Function, Some(span(&file, 20, 40)));
            graph.upsert_entity(&member).unwrap();
            graph
                .upsert_relation(&contains(module.id, member.id))
                .unwrap();
        }
        if with_missing {
            graph
                .upsert_relation(&contains(module.id, kin_model::EntityId::new()))
                .unwrap();
        }
        let mut reply = module_source_reply(&graph, &module).unwrap();
        reply["_kin"] = json!({"verdict":{"state":"certified"}});
        crate::plain_reply::plain_answer(reply.as_object().unwrap().clone(), "get_entity_source")
    }

    #[test]
    fn source_verdict_keeps_unmeasured_module_membership_partial() {
        let reply = presented_module_inventory(false, false);
        assert_eq!(reply["member_counts"], json!({"recorded":0,"returned":0}));
        assert_eq!(reply["member_coverage"], "unavailable");
        assert_eq!(reply["semantic_gap"], MODULE_MEMBER_GAP);
        assert_eq!(reply["_kin"]["verdict"], "partial", "{reply}");
        assert!(reply["body"].is_null());
        assert!(reply["source_base"].is_null());
    }

    #[test]
    fn source_verdict_keeps_partial_members_and_their_gap() {
        let reply = presented_module_inventory(true, true);
        assert_eq!(
            reply["member_counts"],
            json!({"recorded":2,"returned":1,"missing":1})
        );
        assert_eq!(reply["members"][0]["name"], "run");
        assert_eq!(
            reply["members_unavailable"][0]["reason"],
            "member_entity_missing"
        );
        assert_eq!(reply["member_coverage_scope"], "recorded_contains");
        assert_eq!(reply["_kin"]["verdict"], "partial", "{reply}");
        assert!(
            !reply["_kin"]["meaning"]
                .as_str()
                .unwrap()
                .contains("records no members"),
            "{reply}"
        );
    }

    #[test]
    fn source_verdict_keeps_readable_recorded_members_complete() {
        let reply = presented_module_inventory(true, false);
        assert_eq!(reply["member_counts"], json!({"recorded":1,"returned":1}));
        assert_eq!(reply["member_coverage_scope"], "recorded_contains");
        assert!(reply.get("semantic_gap").is_none());
        assert_eq!(reply["_kin"]["verdict"], "complete", "{reply}");
        assert_eq!(reply["members"][0]["name"], "run");
        assert!(reply["body"].is_null());
    }

    #[test]
    fn module_inventory_uses_only_recorded_contains_not_same_file_or_previews() {
        use kin_model::EntityStore;
        let graph = kin_db::InMemoryGraph::new();
        let file = kin_model::FilePathId::new("pkg/app.py");
        let module = entity("app", EntityKind::Module, Some(span(&file, 0, 200)));
        let early = entity("early", EntityKind::Function, Some(span(&file, 20, 40)));
        let late = entity("late", EntityKind::Function, Some(span(&file, 70, 90)));
        let mut unrelated = entity("unrelated", EntityKind::Function, Some(span(&file, 40, 60)));
        unrelated.metadata.extra.insert(
            "embedding_body_preview".into(),
            json!("unrelated_file_text()"),
        );
        for entity in [&module, &late, &early, &unrelated] {
            graph.upsert_entity(entity).unwrap();
        }
        // A same-file declaration is not a recorded member until a Contains
        // edge says so. Repeated edge records still address one member.
        for relation in [
            contains(module.id, late.id),
            contains(module.id, early.id),
            contains(module.id, late.id),
            contains(unrelated.id, module.id),
        ] {
            graph.upsert_relation(&relation).unwrap();
        }
        let inventory = module_inventory(&graph, &module).unwrap();
        let members = inventory["members"].as_array().unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0]["id"], json!(early.id));
        assert_eq!(members[1]["id"], json!(late.id));
        assert_eq!(
            inventory["member_counts"],
            json!({"recorded":2,"returned":2})
        );
        assert_eq!(inventory["member_coverage"], "recorded");
        assert_eq!(inventory["member_coverage_scope"], "recorded_contains");
        assert_eq!(inventory["source_base_unavailable"], MODULE_INVENTORY_ONLY);
        assert_eq!(inventory["body_bytes_basis"], "recorded_span");
        assert!(!inventory.contains_key("semantic_gap"));
        let serialized = serde_json::to_string(&inventory).unwrap();
        assert!(!serialized.contains(&unrelated.id.to_string()));
        assert!(!serialized.contains("unrelated_file_text"));
        assert!(!serialized.contains("repository_complete"));
    }

    #[test]
    fn module_inventory_keeps_dangling_contains_targets_as_named_gaps() {
        use kin_model::EntityStore;
        let graph = kin_db::InMemoryGraph::new();
        let file = kin_model::FilePathId::new("pkg/app.py");
        let module = entity("app", EntityKind::Module, Some(span(&file, 0, 100)));
        let member = entity("run", EntityKind::Function, Some(span(&file, 20, 40)));
        let missing = kin_model::EntityId::new();
        graph.upsert_entity(&module).unwrap();
        graph.upsert_entity(&member).unwrap();
        graph
            .upsert_relation(&contains(module.id, member.id))
            .unwrap();
        graph
            .upsert_relation(&contains(module.id, missing))
            .unwrap();
        let inventory = module_inventory(&graph, &module).unwrap();
        assert_eq!(inventory["member_coverage"], "partial");
        assert_eq!(
            inventory["member_coverage_reason"],
            "contains_targets_unavailable"
        );
        assert_eq!(inventory["semantic_gap"], MODULE_MEMBER_GAP);
        assert_eq!(
            inventory["member_counts"],
            json!({"recorded":2,"returned":1,"missing":1})
        );
        assert_eq!(
            inventory["members_unavailable"],
            json!([{"id":missing,"reason":"member_entity_missing"}])
        );
        assert_eq!(inventory["members"][0]["id"], json!(member.id));
    }

    #[test]
    fn member_read_failures_do_not_turn_a_nonempty_inventory_into_complete_coverage() {
        let module = entity("app", EntityKind::Module, None);
        let member = entity("run", EntityKind::Function, None);
        let unreadable = kin_model::EntityId::new();
        let wrong_identity = kin_model::EntityId::new();
        let relations = vec![
            contains(module.id, member.id),
            contains(module.id, unreadable),
            contains(module.id, wrong_identity),
        ];
        let inventory = observe_members(&module, Ok(relations), |id| {
            if *id == unreadable {
                Err("entity store unavailable")
            } else {
                Ok(Some(member.clone()))
            }
        });
        assert!(inventory.coverage_gap());
        assert_eq!(inventory.reason(), "contains_targets_unavailable");
        assert_eq!(inventory.recorded, Some(3));
        assert_eq!(inventory.members.len(), 1);
        assert_eq!(inventory.unreadable, 2);
        assert!(inventory
            .unavailable
            .contains(&json!({"id":unreadable,"reason":"member_entity_read_failed"})));
        assert!(inventory
            .unavailable
            .contains(&json!({"id":wrong_identity,"reason":"member_entity_identity_mismatch"})));
    }

    #[test]
    fn failed_or_empty_contains_reads_never_certify_no_declarations() {
        let module = entity("app", EntityKind::Module, None);
        let failed = observe_members::<&str>(&module, Err("relation store unavailable"), |_| {
            panic!("no member lookup is possible after a failed Contains read")
        });
        assert_eq!(failed.reason(), "contains_read_failed");
        assert_eq!(failed.recorded, None);
        assert!(failed.coverage_gap());
        let empty = observe_members::<&str>(&module, Ok(Vec::new()), |_| {
            panic!("an empty Contains observation names no member")
        });
        assert_eq!(empty.reason(), "no_recorded_contains_members");
        assert_eq!(empty.recorded, Some(0));
        assert!(empty.coverage_gap());
        let malformed =
            observe_members::<&str>(&module, Ok(vec![contains(module.id, module.id)]), |_| {
                panic!("a self edge is not a member")
            });
        assert_eq!(malformed.reason(), "contains_invalid_targets");
        assert_eq!(malformed.invalid_targets, 1);
        assert!(malformed.coverage_gap());
    }

    #[test]
    fn the_header_ends_at_the_first_member_and_members_keep_source_order() {
        let file = kin_model::FilePathId::new("pkg/app.py");
        let body = format!(
            "class App:\n    \"\"\"The app.\"\"\"\n\n    def run(self):\n        pass\n{}\n    def stop(self):\n        pass\n",
            "#".repeat(10)
        );
        let run_at = body.find("def run").unwrap();
        let stop_at = body.find("def stop").unwrap();
        let class = entity(
            "App",
            EntityKind::Class,
            Some(span(&file, 100, 100 + body.len())),
        );
        let stop = entity(
            "App.stop",
            EntityKind::Method,
            Some(span(&file, 100 + stop_at, 100 + body.len())),
        );
        let run = entity(
            "App.run",
            EntityKind::Method,
            Some(span(&file, 100 + run_at, 100 + stop_at)),
        );
        let fields = shape_from(&class, &body, &[run.clone(), stop.clone()]);
        assert_eq!(
            fields["header"],
            json!("class App:\n    \"\"\"The app.\"\"\"")
        );
        assert_eq!(fields["header_complete"], json!(true));
        let members = fields["members"].as_array().unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0]["name"], json!("App.run"));
        assert_eq!(members[1]["name"], json!("App.stop"));
        assert_eq!(fields["projection"], json!(SHAPE_PROJECTION));
        assert_eq!(fields["body_bytes"], json!(body.len()));
    }

    #[test]
    fn a_long_header_is_cut_at_a_line_and_says_so() {
        let file = kin_model::FilePathId::new("pkg/app.py");
        let doc: String = (0..400).map(|line| format!("    line {line}\n")).collect();
        let body = format!("class App:\n{doc}    def run(self):\n        pass\n");
        let run_at = body.find("def run").unwrap();
        let class = entity("App", EntityKind::Class, Some(span(&file, 0, body.len())));
        let run = entity(
            "App.run",
            EntityKind::Method,
            Some(span(&file, run_at, body.len())),
        );
        let fields = shape_from(&class, &body, &[run]);
        let header = fields["header"].as_str().unwrap();
        assert!(header.len() <= CONTAINER_HEADER_MAX_BYTES);
        assert!(header.starts_with("class App:"));
        assert!(!header.ends_with('\n'));
        assert_eq!(fields["header_complete"], json!(false));
    }

    #[test]
    fn callable_code_is_never_a_container() {
        for kind in [
            EntityKind::Function,
            EntityKind::Method,
            EntityKind::Test,
            EntityKind::Macro,
        ] {
            assert!(is_code_kind(kind));
        }
        for kind in [
            EntityKind::Class,
            EntityKind::Module,
            EntityKind::Interface,
            EntityKind::Constant,
        ] {
            assert!(!is_code_kind(kind));
        }
    }

    #[test]
    fn modules_always_serve_inventory_even_when_small_or_without_members() {
        use kin_model::EntityStore;
        for kind in [EntityKind::Module, EntityKind::Package, EntityKind::File] {
            for bytes in [30, 4_096, 20_000] {
                let graph = kin_db::InMemoryGraph::new();
                let file = kin_model::FilePathId::new("pkg/app.py");
                let module = entity("app", kind, Some(span(&file, 0, bytes)));
                graph.upsert_entity(&module).unwrap();
                let fields = shape_fields(&graph, &module, "unrelated_module_code()").unwrap();
                assert_eq!(fields["header"], "");
                assert_eq!(fields["semantic_gap"], MODULE_MEMBER_GAP);
                assert_eq!(fields["member_coverage"], "unavailable");
                assert_eq!(fields["body_bytes"], bytes);
                for full_body in [false, true] {
                    let mut reply = json!({"id": module.id, "body": "unrelated_module_code()", "source_base": {"ticket":"forged"}});
                    assert!(shape_source_reply(&graph, &mut reply, full_body));
                    assert!(reply.get("body").is_none());
                    assert!(reply.get("source_base").is_none());
                    assert!(!reply.to_string().contains("unrelated_module_code"));
                }
            }
        }
    }

    #[test]
    fn module_inventory_is_shared_by_source_batch_context_and_metadata_without_source_io() {
        use crate::handlers::common::{
            attach_context_body, derived_generator_source_at, entity_response_json,
            read_entity_source_exact_at, read_entity_source_excerpt_detailed_held,
            ContextBodyBudget, ContextSourceProvider, EntitySourceScope, HeldSourceAuthority,
        };
        use kin_context::ContextProjectionProvider;
        use kin_model::EntityStore;
        let graph = kin_db::InMemoryGraph::new();
        let file = kin_model::FilePathId::new("pkg/app.py");
        let mut module = entity("app", EntityKind::Module, Some(span(&file, 0, 40)));
        module.signature = "module pkg/app.py".into();
        module.metadata.extra.insert(
            "embedding_body_preview".into(),
            json!("unrelated_module_code()"),
        );
        graph.upsert_entity(&module).unwrap();
        let held = HeldSourceAuthority::new(&graph, None);
        let scopes = [
            EntitySourceScope::WorkspaceHead,
            EntitySourceScope::At(kin_model::SemanticChangeId::from_hash(
                kin_model::Hash256::from_bytes([1; 32]),
            )),
        ];
        for scope in scopes {
            assert!(
                read_entity_source_exact_at(&held, &module, 1_000_000, scope)
                    .unwrap()
                    .is_none()
            );
            assert!(
                read_entity_source_excerpt_detailed_held(&held, &module, 30, 8_000, scope)
                    .unwrap()
                    .is_none()
            );
            assert!(derived_generator_source_at(&held, &module, 8_000, scope)
                .unwrap_err()
                .to_string()
                .contains("module_body_unavailable"));
        }
        let args = std::collections::HashMap::from([
            ("entity_id".into(), json!(module.id)),
            ("full_body".into(), json!(true)),
        ]);
        let source =
            crate::handlers::entities::handle_get_entity_source(&args, &graph, None).unwrap();
        let crate::ContentBlock::Text { text } = &source.content[0];
        let value: Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["semantic_gap"], MODULE_MEMBER_GAP);
        assert!(value["body"].is_null());
        assert!(value["source_base"].is_null());

        let metadata = entity_response_json(&graph, &module, None).unwrap();
        assert!(metadata.get("source_excerpt").is_none());
        assert!(!metadata.to_string().contains("unrelated_module_code"));
        assert_eq!(metadata["semantic_gap"], MODULE_MEMBER_GAP);

        let mut row = json!({"id":module.id});
        attach_context_body(&held, &module, &mut row, &mut ContextBodyBudget::new(4_000)).unwrap();
        assert_eq!(row["projection"], SHAPE_PROJECTION);
        assert!(row["body"].is_null());
        let mut provider = ContextSourceProvider {
            held: &held,
            fields: Default::default(),
        };
        assert!(matches!(
            provider.full_body(&module, Default::default()).unwrap(),
            kin_context::BodyCandidate::Unavailable { .. }
        ));
        assert_eq!(
            provider.fields.borrow()[&module.id]["semantic_gap"],
            MODULE_MEMBER_GAP
        );

        let batch_args =
            std::collections::HashMap::from([("entity_ids".into(), json!([module.id]))]);
        let batch = crate::handlers::entities::handle_get_entity_sources(&batch_args, &graph, None)
            .unwrap();
        let crate::ContentBlock::Text { text } = &batch.content[0];
        let batch: Value = serde_json::from_str(text).unwrap();
        assert_eq!(batch["results"][0]["semantic_gap"], MODULE_MEMBER_GAP);
        assert_eq!(batch["returned"], 1);
        assert_eq!(batch["truncated"], false);
        assert!(!text.contains("unrelated_module_code"));

        let member = entity("run", EntityKind::Function, Some(span(&file, 0, 40)));
        graph.upsert_entity(&member).unwrap();
        graph
            .upsert_relation(&kin_model::Relation {
                id: kin_model::RelationId::new(),
                kind: RelationKind::Contains,
                src: GraphNodeId::Entity(module.id),
                dst: GraphNodeId::Entity(member.id),
                confidence: 1.0,
                origin: kin_model::RelationOrigin::Parsed,
                created_in: None,
                import_source: None,
                evidence: vec![],
            })
            .unwrap();
        let inventory = module_inventory(&graph, &module).unwrap();
        assert_eq!(inventory["members"][0]["id"], json!(member.id));
        assert_eq!(inventory["member_coverage"], "recorded");
        assert!(!inventory.contains_key("semantic_gap"));
        assert!(kin_model::require_targeted_source(&member).is_ok());
        assert!(shape_fields(&graph, &member, "def run(): return 42").is_none());
    }

    /// `full_body` serves a container whole only up to its bound; past it the
    /// container is served as its outline with a note that says why, and the
    /// default serves any container over 4 KB as its outline.
    #[test]
    fn full_body_serves_a_container_whole_only_up_to_its_bound() {
        use kin_model::graph::EntityStore as _;
        let file = kin_model::FilePathId::new("pkg/app.py");
        let body_of = |pad: usize| {
            format!(
                "class App:\n    def run(self):\n        pass\n{}",
                "    # pad\n".repeat(pad)
            )
        };
        for (pad, whole) in [(600, true), (2_000, false)] {
            let graph = kin_db::InMemoryGraph::new();
            let body = body_of(pad);
            let class = entity("App", EntityKind::Class, Some(span(&file, 0, body.len())));
            let run_at = body.find("def run").unwrap();
            let run = entity(
                "App.run",
                EntityKind::Method,
                Some(span(&file, run_at, run_at + 30)),
            );
            graph.upsert_entity(&class).unwrap();
            graph.upsert_entity(&run).unwrap();
            graph
                .upsert_relation(&kin_model::relation::Relation {
                    id: kin_model::RelationId::new(),
                    kind: RelationKind::Contains,
                    src: GraphNodeId::Entity(class.id),
                    dst: GraphNodeId::Entity(run.id),
                    confidence: 1.0,
                    origin: kin_model::relation::RelationOrigin::Parsed,
                    created_in: None,
                    import_source: None,
                    evidence: vec![],
                })
                .unwrap();

            let mut asked = json!({"id": class.id.to_string(), "body": body});
            assert_eq!(
                shape_source_reply(&graph, &mut asked, true),
                !whole,
                "{pad}"
            );
            if whole {
                assert!(asked.get("body").is_some(), "{asked}");
            } else {
                assert!(asked.get("body").is_none(), "{asked}");
                assert_eq!(asked["members"][0]["name"], "App.run");
                let note = asked["note"].as_str().unwrap();
                assert!(note.contains("at most 16 KB"), "{note}");
                assert!(note.contains("this one is 20 KB"), "{note}");
            }

            let mut default = json!({"id": class.id.to_string(), "body": body_of(pad)});
            assert!(shape_source_reply(&graph, &mut default, false), "{pad}");
            assert_eq!(default["note"], SHAPE_NOTE);
            assert!(!SHAPE_NOTE.contains("get_entity_source"));
        }
    }
}
