// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! What changes with a method's signature, read as its implementation family.
//!
//! A method's signature is shared by every member of its family: the method,
//! each override or member implementation of it, and the callers of every one
//! of them. A call the resolver settled on an override is a call to the
//! override, so the edge it left reaches no other member. An impact answer
//! that listed the method's callers and its overrides, and called the
//! overrides "not consumers", answered "which functions must change if
//! `JSONProvider.dumps` gains a required parameter" without
//! `DefaultJSONProvider.response`, whose `self.dumps(...)` is settled on
//! `DefaultJSONProvider.dumps`, and read as the whole answer. An agent told
//! to make fewer calls never asked the override who called it.
//!
//! So the answer carries the obligation itself. The callers of each override
//! are rows of their own, labelled with the override each calls, a caller
//! matched by name only is a candidate kept apart from proof, and one
//! `implementation_family` entry per changed method states what each part of
//! the family obliges and where its rows are, with how far Kin measured that
//! no other override exists.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use kin_index::RelationResolution;
use kin_model::entity::{Entity, EntityKind};
use kin_model::graph::GraphStore;
use kin_model::ids::EntityId;
use kin_model::relation::{GraphNodeId, RelationKind};
use kin_review::override_callers::OverrideCaller;
use serde_json::{json, Map, Value};

use crate::call_sites::FocalScan;
use crate::error::{McpError, Result};

/// One entry per changed method: its family and what changing its signature
/// obliges.
pub const FAMILY_KEY: &str = "implementation_family";
/// The one note that says how the family's rows read.
pub const FAMILY_NOTE_KEY: &str = "implementation_family_note";
/// Changed methods past [`FAMILY_FOCALS_MAX`], counted rather than read.
pub const FAMILY_WITHHELD_KEY: &str = "implementation_family_withheld";
/// Entities proven to call an override of a changed method.
pub const OVERRIDE_CALLERS_KEY: &str = "affected_override_callers";
/// Entities whose every call to an override was matched by name only.
pub const OVERRIDE_CALLER_CANDIDATES_KEY: &str = "override_caller_candidates";
/// Both override-caller arrays, in the order a reader acts on them.
pub const OVERRIDE_CALLER_BUCKETS: [&str; 2] =
    [OVERRIDE_CALLERS_KEY, OVERRIDE_CALLER_CANDIDATES_KEY];
/// The fields an override-caller row carries beside its identity, kept when
/// the row is compacted.
pub const OVERRIDE_CALLER_LABELS: [&str; 5] = [
    "calls_override",
    "calls_override_id",
    "also_calls_overrides",
    "resolution",
    "proven",
];

/// How many changed methods one answer reads a family for. Each reading
/// walks the method's overrides and checks the repository's recorded classes
/// for one the walk could have missed, so an answer about a whole file stops
/// here and counts the rest.
pub const FAMILY_FOCALS_MAX: usize = 8;
/// How many members one family entry lists; the rest are counted, and every
/// one is a row of `affected_overrides`.
pub const FAMILY_MEMBERS_LISTED_MAX: usize = 32;

/// The note every answer with a family carries.
pub const FAMILY_NOTE: &str = "Every member of a method's implementation family changes \
    with its signature: the method, each override or implementation in affected_overrides, \
    each caller of the method in affected_callers, and each caller of an override in \
    affected_override_callers, which names the override it calls under calls_override. A \
    row with proven false, each row of override_caller_candidates and each unresolved call \
    site in sections.direct.call_sites was matched by name only: check it, it is not proof. \
    coverage.repository_completeness says whether Kin confirmed that no other override exists.";

/// The labels of one override-caller row: the override it calls, how firmly
/// the call settled it, and any other override it also calls.
fn labels(row: &OverrideCaller) -> Map<String, Value> {
    let mut labels = Map::new();
    let Some(primary) = row.primary() else {
        return labels;
    };
    labels.insert("calls_override".into(), json!(primary.override_name));
    labels.insert(
        "calls_override_id".into(),
        json!(primary.override_id.to_string()),
    );
    labels.insert("resolution".into(), json!(primary.resolution));
    labels.insert("proven".into(), json!(row.proven()));
    let others: Vec<&str> = row
        .calls
        .iter()
        .skip(1)
        .map(|call| call.override_name.as_str())
        .collect();
    if !others.is_empty() {
        labels.insert("also_calls_overrides".into(), json!(others));
    }
    labels
}

/// Rewrite each override-caller row, serialized as the caller entity beside
/// its calls, as the entity row every other bucket serves, with its labels.
pub fn flatten_rows(result: &mut Value, impact: &kin_review::ImpactReport) {
    for (key, rows) in OVERRIDE_CALLER_BUCKETS.iter().zip([
        &impact.affected_override_callers,
        &impact.override_caller_candidates,
    ]) {
        let Some(Value::Array(served)) = result.get_mut(*key) else {
            continue;
        };
        for (value, row) in served.iter_mut().zip(rows) {
            let mut entity = match value.get_mut("entity").map(Value::take) {
                Some(Value::Object(entity)) => entity,
                _ => continue,
            };
            entity.extend(labels(row));
            *value = Value::Object(entity);
        }
    }
}

/// The entities each override-caller bucket holds, in the order its rows are
/// served.
pub fn bucket_entities(impact: &kin_review::ImpactReport) -> [Vec<&Entity>; 2] {
    [
        impact
            .affected_override_callers
            .iter()
            .map(|row| &row.entity)
            .collect(),
        impact
            .override_caller_candidates
            .iter()
            .map(|row| &row.entity)
            .collect(),
    ]
}

/// Whether a changed entity gets a family entry: a method always, and any
/// other callable when something overrides it, which a language that models
/// methods as functions can still record.
fn has_family(entity: &Entity, overriders: &super::overrides::Overriders) -> bool {
    entity.kind == EntityKind::Method
        || (entity.kind == EntityKind::Function && !overriders.rows.is_empty())
}

/// The callers a `Calls` edge reaches `id` from, outside `excluded`, each with
/// the firmest resolution of its edges, which says whether it is proven.
fn callers_of<G: GraphStore>(
    store: &G,
    id: &EntityId,
    excluded: &HashSet<EntityId>,
) -> Result<HashMap<EntityId, RelationResolution>> {
    let mut found: HashMap<EntityId, RelationResolution> = HashMap::new();
    for relation in
        kin_index::relation_read::relations_for_read(store, id).map_err(McpError::graph)?
    {
        if relation.kind != RelationKind::Calls || relation.dst != GraphNodeId::Entity(*id) {
            continue;
        }
        let Some(caller) = relation.src.as_entity() else {
            continue;
        };
        if excluded.contains(&caller) {
            continue;
        }
        let resolution = RelationResolution::of(&relation);
        found
            .entry(caller)
            .and_modify(|known| *known = (*known).max(resolution))
            .or_insert(resolution);
    }
    Ok(found)
}

fn split(callers: &HashMap<EntityId, RelationResolution>) -> (usize, usize) {
    let proven = callers
        .values()
        .filter(|resolution| resolution.is_proven())
        .count();
    (proven, callers.len() - proven)
}

/// What one answer's families read, for the rows and the plain reply.
#[derive(Default)]
pub struct Families {
    /// One entry per changed method read, in the answer's order.
    pub entries: Vec<Value>,
    /// Changed methods past [`FAMILY_FOCALS_MAX`].
    pub withheld: usize,
    /// Each family focal's direct callers, with their firmest resolution.
    direct: Vec<(Entity, HashMap<EntityId, RelationResolution>)>,
}

impl Families {
    /// Attach the entries, the note and the count of methods not read.
    pub fn attach(&self, result: &mut Value) {
        if self.entries.is_empty() {
            return;
        }
        result[FAMILY_KEY] = json!(self.entries);
        // The note says how the override rows read, so it rides only an
        // answer that has some.
        let overridden = self.entries.iter().any(|entry| {
            entry
                .pointer("/obligations/change_signature/count")
                .and_then(Value::as_u64)
                .is_some_and(|members| members > 1)
        });
        if overridden {
            result[FAMILY_NOTE_KEY] = json!(FAMILY_NOTE);
        }
        if self.withheld > 0 {
            result[FAMILY_WITHHELD_KEY] = json!(self.withheld);
        }
    }

    /// Label each `affected_callers` row that calls a family's method with
    /// the method it calls, how firmly, and whether that is proof, so the
    /// family's callers read proven or not in one list. A row calling no
    /// family method is left as it was.
    pub fn label_direct_callers(&self, result: &mut Value) {
        let Some(Value::Array(rows)) = result.get_mut("affected_callers") else {
            return;
        };
        // Keyed by the id as a row serializes it, which is how a row is found.
        let mut by_caller: HashMap<String, Vec<(&str, RelationResolution)>> = HashMap::new();
        for (focal, callers) in &self.direct {
            for (caller, resolution) in callers {
                by_caller
                    .entry(caller.to_string())
                    .or_default()
                    .push((focal.name.as_str(), *resolution));
            }
        }
        for row in rows.iter_mut() {
            let Some(object) = row.as_object_mut() else {
                continue;
            };
            let Some(mut calls) = object
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| by_caller.get(id))
                .cloned()
            else {
                continue;
            };
            // The firmest call labels the row; any other method it calls is
            // named beside it.
            calls.sort_by_key(|(_, resolution)| std::cmp::Reverse(*resolution));
            let (name, resolution) = calls[0];
            object.insert("calls".into(), json!(name));
            object.insert("resolution".into(), json!(resolution.as_str()));
            object.insert("proven".into(), json!(resolution.is_proven()));
            if calls.len() > 1 {
                object.insert(
                    "also_calls".into(),
                    json!(calls[1..].iter().map(|(name, _)| *name).collect::<Vec<_>>()),
                );
            }
        }
    }
}

/// Read the family of every changed method, up to [`FAMILY_FOCALS_MAX`]:
/// its members with how each was found and how many callers each has, what
/// a signature change obliges, and how far Kin measured that no other
/// override exists. `scans` are the readings of each changed entity's
/// possible callers, whose unresolved sites name the family's call name and
/// so could call any member.
pub fn read<G: GraphStore>(
    store: &G,
    changed: &[Entity],
    impact: &kin_review::ImpactReport,
    scans: &HashMap<EntityId, Arc<FocalScan>>,
) -> Result<Families> {
    let changed_ids: HashSet<EntityId> = changed.iter().map(|entity| entity.id).collect();
    let mut families = Families::default();
    for focal in changed {
        if !matches!(focal.kind, EntityKind::Method | EntityKind::Function) {
            continue;
        }
        if families.entries.len() >= FAMILY_FOCALS_MAX {
            if focal.kind == EntityKind::Method {
                families.withheld += 1;
            }
            continue;
        }
        let found = super::overrides::overriders_for_target(store, focal)?;
        if !has_family(focal, &found) {
            continue;
        }
        let mut members: HashSet<EntityId> = changed_ids.clone();
        members.extend(found.rows.iter().map(|row| row.entity.id));

        let direct = callers_of(store, &focal.id, &members)?;
        let (direct_proven, direct_by_name) = split(&direct);
        let mut listed = vec![json!({
            "entity_id": focal.id.to_string(),
            "name": focal.name,
            "member": "changed",
            "callers": direct_proven,
            "callers_by_name": direct_by_name,
        })];
        for row in found
            .rows
            .iter()
            .take(FAMILY_MEMBERS_LISTED_MAX.saturating_sub(1))
        {
            let (proven, by_name) = split(&callers_of(store, &row.entity.id, &members)?);
            let mut member = json!({
                "entity_id": row.entity.id.to_string(),
                "name": row.entity.name,
                "member": if row.implements { "implementation" } else { "override" },
                "resolution": row.resolution.as_str(),
                "proven": row.resolution.is_proven(),
                "callers": proven,
                "callers_by_name": by_name,
            });
            if let Some(base) = &row.overrides {
                member["overrides"] = json!(base);
            }
            listed.push(member);
        }
        let in_family =
            |row: &&OverrideCaller| row.calls.iter().any(|call| call.changed_id == focal.id);
        let override_callers = impact
            .affected_override_callers
            .iter()
            .filter(in_family)
            .count();
        let candidates = impact
            .override_caller_candidates
            .iter()
            .filter(in_family)
            .count();
        let unresolved = scans
            .get(&focal.id)
            .map(|scan| json!(scan.named.len() + scan.promoted.len()))
            .unwrap_or(Value::Null);
        let block = super::overrides::overridden_by_json(&found);
        let mut entry = json!({
            "entity_id": focal.id.to_string(),
            "name": focal.name,
            "members": listed,
            "obligations": {
                "change_signature": {
                    "count": 1 + found.rows.len(),
                    "rows": ["changed_ids", "affected_overrides"],
                },
                "update_calls": {
                    "count": direct_proven + override_callers,
                    "of_changed": direct_proven,
                    "of_overrides": override_callers,
                    "rows": ["affected_callers", OVERRIDE_CALLERS_KEY],
                },
                "check_calls": {
                    "count": direct_by_name + candidates,
                    "of_changed": direct_by_name,
                    "of_overrides": candidates,
                    "rows": ["affected_callers", OVERRIDE_CALLER_CANDIDATES_KEY],
                },
                "check_sites": {
                    "count": unresolved,
                    "rows": ["sections.direct.call_sites"],
                },
            },
            "coverage": block["coverage"].clone(),
        });
        if found.rows.len() + 1 > FAMILY_MEMBERS_LISTED_MAX {
            entry["members_withheld"] = json!(found.rows.len() + 1 - FAMILY_MEMBERS_LISTED_MAX);
        }
        for key in [
            "bounded",
            "unlisted_same_name_methods",
            "classes_spelling_base_without_edge",
        ] {
            if let Some(value) = block.get(key) {
                entry[key] = value.clone();
            }
        }
        families.entries.push(entry);
        families.direct.push((focal.clone(), direct));
    }
    Ok(families)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use kin_db::InMemoryGraph;
    use kin_model::entity::Entity;
    use kin_model::graph::EntityStore as _;
    use kin_model::relation::{Relation, RelationOrigin};
    use kin_model::{EntityKind, GraphNodeId, LanguageId, RelationId, RelationKind};
    use serde_json::{json, Value};

    use crate::call_sites::fixture::spanned_entity;
    use crate::session::SessionRegistry;
    use crate::types::ContentBlock;

    fn declared(name: &str, file: &str, kind: EntityKind, signature: &str) -> Entity {
        let mut entity =
            spanned_entity(name, file, LanguageId::Python, 0, "def body():\n    pass\n");
        entity.kind = kind;
        entity.signature = signature.to_string();
        entity
    }

    fn method(name: &str, file: &str) -> Entity {
        let leaf = name.rsplit('.').next().unwrap();
        declared(
            name,
            file,
            EntityKind::Method,
            &format!("def {leaf}(self, obj)"),
        )
    }

    fn function(name: &str, file: &str) -> Entity {
        declared(name, file, EntityKind::Function, &format!("def {name}()"))
    }

    fn class(name: &str, file: &str, header: &str) -> Entity {
        declared(name, file, EntityKind::Class, header)
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
            evidence: Vec::new(),
        }
    }

    fn calls(src: &Entity, dst: &Entity) -> Relation {
        edge(RelationKind::Calls, src, dst, 1.0)
    }

    /// `x.dumps()` with nothing settling `x`: the fan-out tier links it to
    /// every method of the name.
    fn guessed_call(src: &Entity, dst: &Entity) -> Relation {
        edge(
            RelationKind::Calls,
            src,
            dst,
            kin_index::resolution::RECEIVER_NAME_FANOUT_CONFIDENCE,
        )
    }

    /// The Flask shape the question was asked on, with an unrelated
    /// serializer whose `dumps` shares the name and nothing else.
    struct Flask {
        store: InMemoryGraph,
        base: Entity,
        default: Entity,
        response: Entity,
        module_dumps: Entity,
        encode_any: Entity,
        provider: Entity,
    }

    fn flask() -> Flask {
        let store = InMemoryGraph::new();
        let provider = class(
            "JSONProvider",
            "src/flask/json/provider.py",
            "class JSONProvider:",
        );
        let default_class = class(
            "DefaultJSONProvider",
            "src/flask/json/provider.py",
            "class DefaultJSONProvider(JSONProvider):",
        );
        let tagged = class(
            "TaggedJSONSerializer",
            "src/flask/json/tag.py",
            "class TaggedJSONSerializer:",
        );
        let mut base = method("JSONProvider.dumps", "src/flask/json/provider.py");
        // The focal's file was read for unadmitted nested classes, so only
        // the repository-wide reading decides whether the family is complete.
        base.metadata.extra.insert(
            kin_parser::FILE_NESTED_DEFINITIONS_NOT_ADMITTED_KEY.into(),
            json!(0),
        );
        let default = method("DefaultJSONProvider.dumps", "src/flask/json/provider.py");
        let response = method("DefaultJSONProvider.response", "src/flask/json/provider.py");
        let tagged_dumps = method("TaggedJSONSerializer.dumps", "src/flask/json/tag.py");
        let module_dumps = function("dumps", "src/flask/json/__init__.py");
        let session_save = method(
            "SecureCookieSessionInterface.get_signing_serializer",
            "src/flask/sessions.py",
        );
        let encode_any = function("encode_any", "src/extension.py");
        for entity in [
            &provider,
            &default_class,
            &tagged,
            &base,
            &default,
            &response,
            &tagged_dumps,
            &module_dumps,
            &session_save,
            &encode_any,
        ] {
            store.upsert_entity(entity).unwrap();
        }
        for relation in [
            edge(RelationKind::Contains, &provider, &base, 1.0),
            edge(RelationKind::Contains, &default_class, &default, 1.0),
            edge(RelationKind::Contains, &default_class, &response, 1.0),
            edge(RelationKind::Contains, &tagged, &tagged_dumps, 1.0),
            edge(RelationKind::Extends, &default_class, &provider, 1.0),
            edge(RelationKind::Overrides, &default, &base, 1.0),
            calls(&module_dumps, &base),
            // `self.dumps(...)` inside the subclass, settled on the override.
            calls(&response, &default),
            // A same-named method in an unrelated hierarchy, and its caller.
            calls(&session_save, &tagged_dumps),
            // A call whose receiver nothing settles, linked by name to the
            // override and to the unrelated namesake, never to the base.
            guessed_call(&encode_any, &default),
            guessed_call(&encode_any, &tagged_dumps),
        ] {
            store.upsert_relation(&relation).unwrap();
        }
        Flask {
            store,
            base,
            default,
            response,
            module_dumps,
            encode_any,
            provider,
        }
    }

    async fn impact_text(store: &InMemoryGraph, focal: &Entity, compact: bool) -> String {
        let sessions = SessionRegistry::empty_for_test();
        let result = crate::handlers::review::handle_impact_analysis(
            &HashMap::from([
                ("entity_ids".to_string(), json!([focal.id.to_string()])),
                ("include_traffic".to_string(), json!(false)),
                ("compact".to_string(), json!(compact)),
            ]),
            store,
            &sessions,
        )
        .await
        .unwrap();
        let result = crate::finalize_with_envelope(
            result,
            crate::Envelope::daemon().with_health(&json!({
                "initialized": true, "graph_loaded": true, "graph_generation": 3,
            })),
            "impact_analysis",
        );
        let ContentBlock::Text { text } = result.content.first().unwrap();
        text.clone()
    }

    async fn impact(store: &InMemoryGraph, focal: &Entity) -> Value {
        serde_json::from_str(&impact_text(store, focal, true).await).unwrap()
    }

    fn plain(text: &str, tool: &str) -> Value {
        let mut result = json!({"content": [{"type": "text", "text": text}]});
        crate::plain_reply::present(&mut result, tool);
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    fn names(rows: &Value) -> Vec<&str> {
        rows.as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| row["name"].as_str())
            .collect()
    }

    /// Asked what changes when `JSONProvider.dumps` gains a required
    /// parameter, the answer lists the base's caller and the override, and
    /// the caller that reaches the family only through the override, named
    /// with the override it calls. Each obligation is stated with where its
    /// rows are, proof apart from name matches.
    #[tokio::test]
    async fn impact_on_a_method_lists_the_callers_of_its_overrides_with_their_obligations() {
        let flask = flask();
        let text = impact_text(&flask.store, &flask.base, true).await;
        let answer: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(names(&answer["affected_callers"]), ["dumps"], "{answer:#}");
        let caller = &answer["affected_callers"][0];
        assert_eq!(caller["calls"], "JSONProvider.dumps");
        assert_eq!(caller["proven"], true);
        assert_eq!(caller["resolution"], "type_resolved");
        assert_eq!(
            names(&answer["affected_overrides"]),
            ["DefaultJSONProvider.dumps"]
        );

        let through = answer[super::OVERRIDE_CALLERS_KEY].as_array().unwrap();
        assert_eq!(
            names(&answer[super::OVERRIDE_CALLERS_KEY]),
            ["DefaultJSONProvider.response"]
        );
        assert_eq!(through[0]["calls_override"], "DefaultJSONProvider.dumps");
        assert_eq!(
            through[0]["calls_override_id"],
            flask.default.id.to_string()
        );
        assert_eq!(through[0]["proven"], true);
        assert_eq!(through[0]["resolution"], "type_resolved");
        assert_eq!(through[0]["id"], flask.response.id.to_string());
        assert!(through[0]["start_line"].is_u64(), "{}", through[0]);
        for bulk in ["fingerprint", "metadata", "span"] {
            assert!(through[0].get(bulk).is_none(), "{bulk}: {}", through[0]);
        }

        // The call whose receiver nothing settles is a site to check.
        let candidates = answer[super::OVERRIDE_CALLER_CANDIDATES_KEY]
            .as_array()
            .unwrap();
        assert_eq!(
            names(&answer[super::OVERRIDE_CALLER_CANDIDATES_KEY]),
            ["encode_any"]
        );
        assert_eq!(candidates[0]["id"], flask.encode_any.id.to_string());
        assert_eq!(candidates[0]["proven"], false);
        assert_eq!(candidates[0]["resolution"], "name_only");
        assert_eq!(candidates[0]["calls_override"], "DefaultJSONProvider.dumps");

        // The unrelated namesake and its caller are in no row of the direct
        // answer and no family. (The store-wide reading of pending entities
        // under `enrichment` names every entity, and is not an answer row.)
        let direct = json!([
            answer["affected_callers"],
            answer["affected_overrides"],
            answer[super::OVERRIDE_CALLERS_KEY],
            answer[super::OVERRIDE_CALLER_CANDIDATES_KEY],
            answer["affected_dependents"],
            answer[super::FAMILY_KEY],
        ])
        .to_string();
        for absent in [
            "TaggedJSONSerializer.dumps",
            "SecureCookieSessionInterface.get_signing_serializer",
        ] {
            assert!(!direct.contains(absent), "{absent} in {answer:#}");
        }

        let family = &answer[super::FAMILY_KEY];
        assert_eq!(family.as_array().unwrap().len(), 1, "{family:#}");
        let entry = &family[0];
        assert_eq!(entry["name"], "JSONProvider.dumps");
        assert_eq!(
            entry["members"],
            json!([
                {"entity_id": flask.base.id.to_string(), "name": "JSONProvider.dumps",
                 "member": "changed", "callers": 1, "callers_by_name": 0},
                {"entity_id": flask.default.id.to_string(), "name": "DefaultJSONProvider.dumps",
                 "member": "override", "resolution": "type_resolved", "proven": true,
                 "callers": 1, "callers_by_name": 1},
            ])
        );
        let obligations = &entry["obligations"];
        assert_eq!(obligations["change_signature"]["count"], 2);
        assert_eq!(obligations["update_calls"]["count"], 2);
        assert_eq!(obligations["update_calls"]["of_overrides"], 1);
        assert_eq!(obligations["check_calls"]["count"], 1);
        assert_eq!(
            obligations["update_calls"]["rows"],
            json!(["affected_callers", "affected_override_callers"])
        );
        assert_eq!(entry["coverage"]["repository_completeness"], "complete");
        assert_eq!(answer[super::FAMILY_NOTE_KEY], super::FAMILY_NOTE);
        assert_eq!(
            answer["sections"]["direct"]["rows"],
            json!([
                "affected_callers",
                "affected_overrides",
                "affected_override_callers",
                "override_caller_candidates"
            ])
        );

        // The plain answer says the obligation in its meaning.
        let reply = plain(&text, "impact_analysis");
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert!(
            meaning.contains(
                "A signature change also changes 1 override in affected_overrides and 1 caller in affected_override_callers that calls an override, named under calls_override."
            ),
            "{meaning}"
        );
        assert!(
            meaning.contains("1 more caller or call site was matched by name only"),
            "{meaning}"
        );
        assert!(
            !meaning.contains("Another override could exist"),
            "{meaning}"
        );
        // What reaches an agent stays in plain words.
        for text in [meaning, super::FAMILY_NOTE] {
            for jargon in [
                "census",
                "focal",
                "upstream",
                "owed",
                "escape",
                "certified",
                "inconclusive",
                "enrichment",
                "residue",
                "\u{2014}",
            ] {
                assert!(!text.to_lowercase().contains(jargon), "{jargon}: {text}");
            }
        }
        assert_eq!(
            names(&reply[super::OVERRIDE_CALLERS_KEY]),
            ["DefaultJSONProvider.response"]
        );
        assert!(reply[super::FAMILY_KEY].is_array(), "{reply:#}");

        // Raw rows keep the whole entity and the same labels.
        let raw: Value =
            serde_json::from_str(&impact_text(&flask.store, &flask.base, false).await).unwrap();
        let row = &raw[super::OVERRIDE_CALLERS_KEY][0];
        assert!(row.get("span").is_some(), "{row}");
        assert_eq!(row["calls_override"], "DefaultJSONProvider.dumps");
        assert!(
            row.get("entity").is_none() && row.get("calls").is_none(),
            "{row}"
        );
    }

    /// A subclass declaring a method of the name with no override edge could
    /// be an override the walk missed, and the family says so rather than
    /// reading complete.
    #[tokio::test]
    async fn a_missed_override_keeps_the_family_incomplete_and_the_answer_says_so() {
        let flask = flask();
        let compact = class(
            "CompactProvider",
            "src/compact.py",
            "class CompactProvider(JSONProvider):",
        );
        let compact_dumps = method("CompactProvider.dumps", "src/compact.py");
        for entity in [&compact, &compact_dumps] {
            flask.store.upsert_entity(entity).unwrap();
        }
        for relation in [
            edge(RelationKind::Contains, &compact, &compact_dumps, 1.0),
            edge(RelationKind::Extends, &compact, &flask.provider, 1.0),
        ] {
            flask.store.upsert_relation(&relation).unwrap();
        }
        let text = impact_text(&flask.store, &flask.base, true).await;
        let answer: Value = serde_json::from_str(&text).unwrap();
        let entry = &answer[super::FAMILY_KEY][0];
        assert_eq!(entry["coverage"]["repository_completeness"], "incomplete");
        assert_eq!(
            entry["unlisted_same_name_methods"][0]["name"],
            "CompactProvider.dumps"
        );
        let meaning = plain(&text, "impact_analysis")["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            meaning.contains("Another override could exist"),
            "{meaning}"
        );
    }

    /// An override of an override is three edges from the base, and the
    /// caller of the deepest one is still listed with the override it calls.
    #[tokio::test]
    async fn impact_lists_the_caller_of_an_override_of_an_override() {
        let store = InMemoryGraph::new();
        let base = method("Base.render", "pkg/base.py");
        let middle = method("Middle.render", "pkg/middle.py");
        let leaf = method("Leaf.render", "pkg/leaf.py");
        let paint = function("paint", "pkg/paint.py");
        for entity in [&base, &middle, &leaf, &paint] {
            store.upsert_entity(entity).unwrap();
        }
        for relation in [
            edge(RelationKind::Overrides, &middle, &base, 1.0),
            edge(RelationKind::Overrides, &leaf, &middle, 1.0),
            calls(&paint, &leaf),
        ] {
            store.upsert_relation(&relation).unwrap();
        }
        let answer = impact(&store, &base).await;
        assert_eq!(names(&answer[super::OVERRIDE_CALLERS_KEY]), ["paint"]);
        assert_eq!(
            answer[super::OVERRIDE_CALLERS_KEY][0]["calls_override"],
            "Leaf.render"
        );
        let members = &answer[super::FAMILY_KEY][0]["members"];
        assert_eq!(members[2]["name"], "Leaf.render");
        assert_eq!(members[2]["overrides"], "Middle.render");
        assert_eq!(members[2]["callers"], 1);
    }

    /// A member implementing a declared contract member is in the family,
    /// labelled as an implementation, and its caller is listed.
    #[tokio::test]
    async fn impact_lists_the_caller_of_an_implementation_of_a_contract_member() {
        let store = InMemoryGraph::new();
        let contract = method("Encoder.encode", "src/codec.py");
        let implementation = method("Utf8Encoder.encode", "src/utf8.py");
        let implementer = class("Utf8Encoder", "src/utf8.py", "class Utf8Encoder(Encoder):");
        let send = function("send_packet", "src/net.py");
        for entity in [&contract, &implementation, &implementer, &send] {
            store.upsert_entity(entity).unwrap();
        }
        for relation in [
            edge(RelationKind::Implements, &implementation, &contract, 1.0),
            // A class implementing the contract names no member.
            edge(RelationKind::Implements, &implementer, &contract, 1.0),
            calls(&send, &implementation),
        ] {
            store.upsert_relation(&relation).unwrap();
        }
        let answer = impact(&store, &contract).await;
        assert_eq!(names(&answer[super::OVERRIDE_CALLERS_KEY]), ["send_packet"]);
        assert_eq!(
            answer[super::OVERRIDE_CALLERS_KEY][0]["calls_override"],
            "Utf8Encoder.encode"
        );
        let members = &answer[super::FAMILY_KEY][0]["members"];
        assert_eq!(members.as_array().unwrap().len(), 2, "{members:#}");
        assert_eq!(members[1]["member"], "implementation");
    }

    /// A function nothing overrides answers as it did: no family, no
    /// override rows, and its callers unlabelled.
    #[tokio::test]
    async fn impact_on_a_function_is_unchanged() {
        let flask = flask();
        let jsonify = function("jsonify", "src/flask/json/__init__.py");
        flask.store.upsert_entity(&jsonify).unwrap();
        flask
            .store
            .upsert_relation(&calls(&jsonify, &flask.module_dumps))
            .unwrap();
        let answer = impact(&flask.store, &flask.module_dumps).await;
        assert_eq!(names(&answer["affected_callers"]), ["jsonify"]);
        for key in [
            super::FAMILY_KEY,
            super::FAMILY_NOTE_KEY,
            super::OVERRIDE_CALLERS_KEY,
            super::OVERRIDE_CALLER_CANDIDATES_KEY,
        ] {
            assert!(answer.get(key).is_none(), "{key}: {answer:#}");
        }
        for label in ["calls", "proven", "resolution"] {
            assert!(
                answer["affected_callers"][0].get(label).is_none(),
                "{label}: {answer:#}"
            );
        }
    }

    /// A long answer pages every override-caller row, and its family entry
    /// leads the direct rows, so the first page states the obligation.
    #[test]
    fn impact_pages_lead_with_the_family_and_reach_every_override_caller() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let arguments = HashMap::from([(
            "entity_ids".to_string(),
            json!(["00000000-0000-4000-8000-000000000001"]),
        )]);
        let authority = json!({"repo": "example", "truth": 11});
        let context = crate::impact_pages::context(&arguments, &authority);
        let row = |bucket: &str, index: usize| {
            json!({
                "id": format!("00000000-0000-4000-8000-{index:012}"),
                "name": format!("{bucket}_{index}"), "kind": "method",
                "signature": "x".repeat(120),
                "calls_override": "Child.run", "proven": bucket == "through",
            })
        };
        let through: Vec<Value> = (0..20).map(|index| row("through", index)).collect();
        let by_name: Vec<Value> = (0..6).map(|index| row("by_name", index)).collect();
        let family = json!([{"name": "Base.run", "obligations": {
            "change_signature": {"count": 2}, "update_calls": {"count": 21}}}]);
        let original = json!({
            "affected_callers": [row("caller", 0)],
            "affected_overrides": [row("override", 0)],
            super::OVERRIDE_CALLERS_KEY: through,
            super::OVERRIDE_CALLER_CANDIDATES_KEY: by_name,
            super::FAMILY_KEY: family,
            super::FAMILY_NOTE_KEY: super::FAMILY_NOTE,
            "negative": {"safe_to_conclude_absent": false},
            "_kin": {"verdict": {"state": "inconclusive"},
                "repository": {"root": "/example/project"},
                "graph_as_of": {"generation": 4, "graph_root": "captured-root"}},
        });
        let mut page = crate::trace_pages::start_kind(
            original,
            context.clone(),
            3000,
            crate::trace_pages::PageKind::Impact,
        )
        .unwrap();
        let first_readings: Vec<String> = page["readings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|reading| reading["key"].as_str().map(str::to_string))
            .collect();
        assert!(
            first_readings.iter().any(|key| key == super::FAMILY_KEY),
            "{page:#}"
        );
        let mut seen: HashMap<&str, Vec<Value>> = HashMap::new();
        let mut pages = 0;
        loop {
            pages += 1;
            assert!(pages < 100);
            assert!(serde_json::to_vec(&page).unwrap().len() <= 3000);
            for bucket in super::OVERRIDE_CALLER_BUCKETS {
                seen.entry(bucket)
                    .or_default()
                    .extend(page[bucket].as_array().into_iter().flatten().cloned());
            }
            let Some(cursor) = page["next_cursor"].as_str() else {
                break;
            };
            page = crate::impact_pages::resume(cursor, &context, 3000).unwrap();
        }
        assert!(pages > 1, "the fixture must page");
        assert_eq!(seen[super::OVERRIDE_CALLERS_KEY], through);
        assert_eq!(seen[super::OVERRIDE_CALLER_CANDIDATES_KEY], by_name);
    }

    /// A reference answer on the method lists the same override callers
    /// under its `overridden_by` block, apart from its references, and says
    /// where they are.
    #[tokio::test]
    async fn references_on_a_method_list_the_callers_of_its_overrides() {
        let flask = flask();
        // Reference rows read each caller's own text for its sites, which
        // needs a repository this unit test has none of; spanless entities
        // are answered from their edges alone, as the reference tests do.
        for mut entity in flask.store.list_all_entities().unwrap() {
            entity.span = None;
            flask.store.upsert_entity(&entity).unwrap();
        }
        let args = HashMap::from([("entity_id".to_string(), json!(flask.base.id.to_string()))]);
        let result = crate::handlers::entities::handle_find_references(&args, &flask.store, None)
            .await
            .unwrap();
        let ContentBlock::Text { text } = result.content.first().unwrap();
        let answer: Value = serde_json::from_str(text).unwrap();
        let block = &answer[super::super::overrides::OVERRIDDEN_BY_KEY];
        assert_eq!(names(&block["overriders"]), ["DefaultJSONProvider.dumps"]);
        assert_eq!(
            names(&block[super::super::overrides::OVERRIDE_CALLERS_KEY]),
            ["DefaultJSONProvider.response"],
            "{block:#}"
        );
        let row = &block[super::super::overrides::OVERRIDE_CALLERS_KEY][0];
        assert_eq!(row["calls_override"], "DefaultJSONProvider.dumps");
        assert_eq!(row["proven"], true);
        assert_eq!(
            names(&block[super::super::overrides::OVERRIDE_CALLER_CANDIDATES_KEY]),
            ["encode_any"]
        );
        assert_eq!(
            block[super::super::overrides::OVERRIDE_CALLER_CANDIDATES_KEY][0]["proven"],
            false
        );
        assert!(block["detail"]
            .as_str()
            .unwrap()
            .contains("So does each caller in override_callers"));
        // The references are the base's callers alone, and the count is theirs.
        assert_eq!(names(&answer["references"]), ["dumps"], "{answer:#}");
        assert_eq!(answer["total_upstream"], 1);
        assert!(
            !block.to_string().contains("TaggedJSONSerializer"),
            "{block:#}"
        );

        let finalized = crate::finalize_with_envelope(
            result,
            crate::Envelope::daemon().with_health(&json!({
                "initialized": true, "graph_loaded": true, "graph_generation": 3,
            })),
            "find_references",
        );
        let ContentBlock::Text { text } = finalized.content.first().unwrap();
        let reply = plain(text, "find_references");
        let overrides = &reply["overridden_by"];
        assert_eq!(
            names(&overrides["override_callers"]),
            ["DefaultJSONProvider.response"],
            "{reply:#}"
        );
        assert_eq!(
            overrides["override_callers"][0]["calls_override"],
            "DefaultJSONProvider.dumps"
        );
        assert_eq!(
            names(&overrides["override_caller_candidates"]),
            ["encode_any"]
        );
        assert!(overrides["note"]
            .as_str()
            .unwrap()
            .contains("each caller in override_callers"));
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert!(
            meaning.contains("1 more caller calls an override of it rather than it: it is under overridden_by.override_callers"),
            "{meaning}"
        );

        // A focused override answer still reads no callers.
        let focused =
            super::super::overrides::focused_overrides(&flask.store, &flask.base).unwrap();
        let block = &focused[super::super::overrides::OVERRIDDEN_BY_KEY];
        assert!(block
            .get(super::super::overrides::OVERRIDE_CALLERS_KEY)
            .is_none());
    }
}
