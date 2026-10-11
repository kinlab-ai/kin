// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The two questions an impact answer holds, each under its own verdict.
//!
//! "What must change with X" is answered by its direct callers and its
//! overrides, read the way `find_references` reads the callers of one focal:
//! every call site in the store that could call it, proven or listed. "What
//! else could this reach" is the transitive walk out to dependents, tests and
//! contract consumers, bounded by the call-site evidence across that much
//! wider reach. One verdict over both let a gap in the second, often in code
//! nowhere near the change, refuse an answer to the first that was complete.
//!
//! Every array an impact answer carries stays where it was. `sections` names
//! which arrays answer which question, the evidence the direct verdict was
//! read from, and, once the envelope grades them, each section's verdict.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use kin_model::entity::Entity;
use kin_model::graph::GraphStore;
use kin_model::{EntityId, EntityKind, LanguageId, RelationKind};
use serde_json::{json, Value};

use crate::call_sites::FocalScan;

/// The key an impact answer carries its sections under.
pub const SECTIONS_KEY: &str = "sections";
/// The section that answers what must change with the changed entities.
pub const DIRECT: &str = "direct";
/// The section that answers what else the change could reach.
pub const DEPENDENTS: &str = "dependents";
/// The field the top-level verdict of an impact answer names its section by.
pub const SCOPE_KEY: &str = "scope";

/// Declarations in JavaScript or TypeScript that carry the changed function's
/// name and that no edge links to it.
pub const SAME_NAME_KEY: &str = "same_name_implementations";
/// The one note that says what [`SAME_NAME_KEY`] rows are and are not.
pub const SAME_NAME_NOTE_KEY: &str = "same_name_implementations_note";
/// How many same-name rows one answer lists; the rest are counted.
pub const SAME_NAME_ROWS_MAX: usize = 50;
/// How many same-name rows the store held past [`SAME_NAME_ROWS_MAX`].
pub const SAME_NAME_WITHHELD_KEY: &str = "same_name_implementations_withheld";

/// Methods whose receiver types satisfy the interface a changed Go interface
/// method belongs to. Go satisfaction is structural, so no edge records these
/// and `affected_overrides` cannot hold them; each must change with the
/// contract's signature to keep satisfying it.
pub const INTERFACE_IMPLEMENTATIONS_KEY: &str = "affected_implementations";
/// The one note that says what [`INTERFACE_IMPLEMENTATIONS_KEY`] rows are.
pub const INTERFACE_IMPLEMENTATIONS_NOTE_KEY: &str = "affected_implementations_note";
/// The note every non-empty [`INTERFACE_IMPLEMENTATIONS_KEY`] list carries.
pub const INTERFACE_IMPLEMENTATIONS_NOTE: &str = "These methods belong to types whose method \
    sets satisfy the changed interface method's interface, so a call through the interface can \
    run each of them. Go records no edge for that: satisfaction is structural, so each row is a \
    possible implementation read from the method sets, not a recorded one. A signature change to \
    the interface method has to be made in each of them for its type to keep satisfying the \
    interface.";

/// The note every non-empty [`SAME_NAME_KEY`] list carries.
pub const SAME_NAME_NOTE: &str = "These are declarations with the same name as the changed \
    function that no edge links to it. They are name matches, not proven links, and they do not \
    change this section's verdict. A module-swapped implementation, such as a host config or a \
    platform fork, may also need a signature change; inspect its role before treating it as affected.";

/// The arrays the direct section answers with, in the order a reader acts on
/// them: who calls it, who holds it as a value, what overrides it, who calls
/// an override, who may, and what shares its name.
pub const DIRECT_ROWS: [&str; 7] = [
    "affected_callers",
    crate::handlers::review::IMPACT_VALUE_USES_KEY,
    "affected_overrides",
    INTERFACE_IMPLEMENTATIONS_KEY,
    crate::handlers::implementation_family::OVERRIDE_CALLERS_KEY,
    crate::handlers::implementation_family::OVERRIDE_CALLER_CANDIDATES_KEY,
    SAME_NAME_KEY,
];
/// The arrays the dependents section answers with.
pub const DEPENDENT_ROWS: [&str; 3] = [
    "affected_dependents",
    "affected_contract_consumers",
    "affected_tests",
];

/// Fields the direct section's verdict is read from beside its rows: the
/// per-entity consumer counts an absence is claimed off, the arrival reading
/// that qualifies a zero, and the override disclosure, with what each changed
/// method's family obliges.
const DIRECT_EVIDENCE: [&str; 9] = [
    "entity_impacts",
    "changed_ids",
    crate::caller_arrival::CALLER_ARRIVAL_KEY,
    "overrides_not_admitted",
    SAME_NAME_NOTE_KEY,
    INTERFACE_IMPLEMENTATIONS_NOTE_KEY,
    crate::handlers::implementation_family::FAMILY_KEY,
    crate::handlers::implementation_family::FAMILY_NOTE_KEY,
    crate::handlers::implementation_family::FAMILY_WITHHELD_KEY,
];
/// Fields the dependents section's verdict is read from beside its rows.
const DEPENDENT_EVIDENCE: [&str; 1] = ["enrichment"];
/// Fields both sections are read under: the edge classes both walks read,
/// and what the answer reports about itself.
const SHARED_EVIDENCE: [&str; 5] = [
    crate::edge_coverage::EDGE_COVERAGE_KEY,
    "degradations",
    "cross_repo_impact",
    "cross_repo_impact_status",
    "truncated",
];

/// The methods that may implement each changed Go interface method, as
/// impact rows, leaving out what the answer already lists as a changed
/// entity or an override, and the changed entities whose implementations
/// could not be read. Which types satisfy which contract is decided by
/// `kin_index::dispatch`, the authority `find_references` reads, so the two
/// answers cannot disagree. Each row carries its entity's kind, role and
/// module, as every other impact row does, so a reader who leaves test code
/// out drops a test fake and a row is placed in its package.
pub fn interface_implementation_rows<G: GraphStore>(
    store: &G,
    changed: &[Entity],
    overrides: &[Entity],
) -> InterfaceImplementationRows {
    let listed: HashSet<String> = changed
        .iter()
        .chain(overrides)
        .map(|entity| entity.id.to_string())
        .collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut found = InterfaceImplementationRows::default();
    for focal in changed {
        let candidates =
            match crate::handlers::common::collect_interface_implementations(store, focal) {
                Ok(Some(crate::handlers::common::InterfaceImplementations::Candidates {
                    rows,
                    ..
                })) => rows,
                Ok(
                    None
                    | Some(crate::handlers::common::InterfaceImplementations::NoImplementation {
                        ..
                    }),
                ) => continue,
                // A walk that failed is no implementation-free answer.
                Ok(Some(crate::handlers::common::InterfaceImplementations::Unavailable {
                    ..
                }))
                | Err(_) => {
                    found.unread.push(focal.name.clone());
                    continue;
                }
            };
        for candidate in candidates {
            if listed.contains(&candidate.entity_id) || !seen.insert(candidate.entity_id.clone()) {
                continue;
            }
            let entity = uuid::Uuid::parse_str(&candidate.entity_id)
                .ok()
                .and_then(|id| store.get_entity(&EntityId(id)).ok().flatten());
            let mut row = json!({
                "id": candidate.entity_id,
                "name": candidate.name,
                "kind": entity.as_ref().map(|entity| json!(entity.kind)),
                "role": entity.as_ref().map(|entity| json!(entity.role)),
                "receiver": candidate.receiver,
                "signature": candidate.signature,
                "start_line": candidate.line,
                "resolution": crate::handlers::entities::IMPLEMENTS_INTERFACE_CANDIDATE,
            });
            if let Some(entity) = &entity {
                if let Some(module) = crate::semantic_owners::entity_module(store, entity) {
                    row["module"] = json!(module);
                }
                crate::semantic_owners::mark_shared_name(store, entity, &mut row);
            }
            found.rows.push(row);
        }
    }
    found
}

/// What [`interface_implementation_rows`] read: the rows, and the name of
/// every changed entity whose implementations walk failed, so the answer says
/// so rather than reading as one no method implements.
#[derive(Debug, Default)]
pub struct InterfaceImplementationRows {
    pub rows: Vec<Value>,
    pub unread: Vec<String>,
}

/// How many changed entities one impact answer reads every possible caller
/// of. Each reading walks the callers of the focal's calling languages, so an
/// answer about a whole file stops here and says so rather than walking the
/// store once per declaration in it.
pub const DIRECT_SCAN_FOCALS_MAX: usize = 64;

/// The store-wide readings of every possible caller of each changed entity,
/// taken the way `find_references` takes one.
#[derive(Default)]
pub struct DirectScans {
    /// The changed entities that were read, in the answer's order.
    pub focals: Vec<Entity>,
    pub scans: HashMap<EntityId, Arc<FocalScan>>,
    /// Changed entities the selected graph does not hold.
    pub missing: usize,
    /// Changed entities whose reading could not be taken from the store.
    pub unreadable: Vec<String>,
    /// Changed entities past [`DIRECT_SCAN_FOCALS_MAX`].
    pub unscanned: usize,
    /// Changed interface methods whose implementations could not be read
    /// (see [`interface_implementation_rows`]).
    pub implementations_unread: Vec<String>,
    /// Every changed entity the answer names.
    pub changed: usize,
}

impl DirectScans {
    /// The `call_sites` block the direct section's verdict is read from: the
    /// block `find_references` serves for one focal, or one per focal under
    /// `focals` with their clauses joined, plus a clause for every changed
    /// entity no reading covers. Settled only when every one is.
    pub fn call_sites_block(&self) -> Value {
        let mut extra: Vec<String> = Vec::new();
        if self.missing > 0 {
            extra.push(format!(
                "focal_not_in_graph: {} of the {} changed entities are not in the selected graph, \
                 so no caller of them could be read",
                self.missing, self.changed
            ));
        }
        if !self.unreadable.is_empty() {
            extra.push(format!(
                "retrieval_degraded: the possible callers of {} could not be read from the \
                 store, so callers of them may be missing from this section",
                self.unreadable.join(", ")
            ));
        }
        if !self.implementations_unread.is_empty() {
            extra.push(format!(
                "retrieval_degraded: the methods that implement {} could not be read from the \
                 store, so implementations of them may be missing from this section",
                self.implementations_unread.join(", ")
            ));
        }
        if self.unscanned > 0 {
            extra.push(format!(
                "walk_bounded: the possible callers of {} of the {} changed entities were not \
                 read, past the bound of {DIRECT_SCAN_FOCALS_MAX} entities one answer reads them \
                 for, so callers of those entities are a lower bound",
                self.unscanned, self.changed
            ));
        }
        let blocks: Vec<Value> = self
            .focals
            .iter()
            .filter_map(|focal| {
                let scan = self.scans.get(&focal.id)?;
                let mut block = crate::call_sites::named_block(scan);
                block["entity_id"] = json!(focal.id.to_string());
                block["name"] = json!(focal.name);
                Some(block)
            })
            .collect();
        if blocks.len() == 1 && extra.is_empty() {
            return blocks.into_iter().next().expect("one block");
        }
        let mut clauses: Vec<String> = Vec::new();
        for clause in blocks
            .iter()
            .flat_map(|block| block["clauses"].as_array().cloned().unwrap_or_default())
            .filter_map(|clause| clause.as_str().map(str::to_string))
            .chain(extra.iter().cloned())
        {
            if !clauses.contains(&clause) {
                clauses.push(clause);
            }
        }
        let settled = extra.is_empty()
            && !blocks.is_empty()
            && blocks
                .iter()
                .all(|block| block["settled"].as_bool() == Some(true));
        let mut block = json!({
            "scope": crate::call_sites::NAMED_SCOPE,
            "settled": settled,
            "clauses": clauses,
            "focals": blocks,
        });
        if blocks_is_empty_without_reason(&block, &extra) {
            // Nothing was read and nothing says why: never a certification.
            block["clauses"] = json!([format!(
                "{}: no changed entity's callers were read, so this section cannot say who calls \
                 it",
                crate::verdict::UNLISTED_CLAUSE_CODE
            )]);
        }
        block
    }
}

fn blocks_is_empty_without_reason(block: &Value, extra: &[String]) -> bool {
    extra.is_empty()
        && block["focals"]
            .as_array()
            .is_none_or(|focals| focals.is_empty())
}

/// The `sections` object a handler attaches before the envelope grades it:
/// which arrays answer which question, and the direct section's call-site
/// evidence. Only arrays the answer holds are named.
pub fn sections_json(payload: &Value, direct_call_sites: Value) -> Value {
    let named = |rows: &[&'static str]| -> Vec<&'static str> {
        rows.iter()
            .copied()
            .filter(|key| payload.get(*key).is_some_and(Value::is_array))
            .collect()
    };
    json!({
        DIRECT: {
            "rows": named(&DIRECT_ROWS),
            "call_sites": direct_call_sites,
        },
        DEPENDENTS: {
            "rows": named(&DEPENDENT_ROWS),
        },
    })
}

/// The payloads each section is graded as: its rows, its own evidence and the
/// evidence both share, read by the same verdict every impact answer is read
/// by. The direct one carries the call-site reading of its focals as
/// `call_sites`, the way a reference answer does, and the dependents one the
/// enrichment observation over the wider reach. `None` for an answer with no
/// sections.
pub(crate) fn section_views(payload: &Value) -> Option<(Value, Value)> {
    let sections = payload.get(SECTIONS_KEY)?.as_object()?;
    let view = |keys: &[&[&str]]| -> serde_json::Map<String, Value> {
        keys.iter()
            .flat_map(|keys| keys.iter())
            .filter_map(|key| {
                payload
                    .get(*key)
                    .map(|value| ((*key).to_string(), value.clone()))
            })
            .collect()
    };
    let mut direct = view(&[&DIRECT_ROWS, &DIRECT_EVIDENCE, &SHARED_EVIDENCE]);
    if let Some(block) = sections
        .get(DIRECT)
        .and_then(|section| section.get(crate::call_sites::CALL_SITES_KEY))
    {
        direct.insert(crate::call_sites::CALL_SITES_KEY.to_string(), block.clone());
    }
    let dependents = view(&[&DEPENDENT_ROWS, &DEPENDENT_EVIDENCE, &SHARED_EVIDENCE]);
    Some((Value::Object(direct), Value::Object(dependents)))
}

/// The compact form of a section's verdict every page of the answer carries.
pub(crate) fn section_state(verdict: &Value) -> Value {
    json!({
        "state": verdict.get("state").cloned().unwrap_or(Value::Null),
        "limiting_factor": verdict.get("limiting_factor").cloned().unwrap_or(Value::Null),
    })
}

/// Whether `entity` is a JavaScript or TypeScript function or method.
fn is_script_callable(entity: &Entity) -> bool {
    matches!(
        entity.language,
        LanguageId::JavaScript | LanguageId::TypeScript
    ) && matches!(entity.kind, EntityKind::Function | EntityKind::Method)
}

/// The last segment of a qualified name: `commitTextUpdate` for both
/// `commitTextUpdate` and `createReactNoop.<locals>.mutationHostConfig.commitTextUpdate`.
fn short_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// Held JavaScript and TypeScript functions and methods whose short name is a
/// changed function's, when that function has no override or implementation
/// edge to answer for it: a module-swapped implementation, such as a
/// renderer's host config, is linked to its siblings by nothing but its name
/// and its signature. Each row is shaped like an override row. The changed
/// entities and their overrides are left out, and so is every row past
/// [`SAME_NAME_ROWS_MAX`], which the second value counts.
///
/// A name match proves no link, so these rows never enter a verdict.
pub fn same_name_implementations<G: GraphStore + ?Sized>(
    store: &G,
    changed: &[Entity],
    overrides: &[Entity],
) -> (Vec<Value>, usize) {
    let mut excluded: HashSet<EntityId> = changed.iter().map(|entity| entity.id).collect();
    excluded.extend(overrides.iter().map(|entity| entity.id));
    let mut rows: Vec<Entity> = Vec::new();
    let mut seen: HashSet<EntityId> = HashSet::new();
    for focal in changed.iter().filter(|focal| is_script_callable(focal)) {
        let linked = store
            .get_all_relations_for_entity(&focal.id)
            .map(|relations| {
                relations.iter().any(|relation| {
                    matches!(
                        relation.kind,
                        RelationKind::Overrides | RelationKind::Implements
                    )
                })
            })
            .unwrap_or(true);
        if linked {
            continue;
        }
        let name = short_name(&focal.name);
        if name.is_empty() {
            continue;
        }
        let Ok(held) = store.query_entities(&kin_model::graph::EntityFilter {
            kinds: Some(vec![EntityKind::Function, EntityKind::Method]),
            languages: Some(vec![LanguageId::JavaScript, LanguageId::TypeScript]),
            // Names ending in it, any case: the short-name rule below decides.
            name_pattern: Some(format!("*{name}")),
            ..Default::default()
        }) else {
            continue;
        };
        for entity in held {
            if short_name(&entity.name) != name
                || !is_script_callable(&entity)
                || excluded.contains(&entity.id)
                || !seen.insert(entity.id)
            {
                continue;
            }
            rows.push(entity);
        }
    }
    rows.sort_by(|left, right| {
        (
            left.file_origin.as_ref().map(|file| file.0.as_str()),
            left.span.as_ref().map(|span| span.start_byte),
            &left.name,
        )
            .cmp(&(
                right.file_origin.as_ref().map(|file| file.0.as_str()),
                right.span.as_ref().map(|span| span.start_byte),
                &right.name,
            ))
    });
    let withheld = rows.len().saturating_sub(SAME_NAME_ROWS_MAX);
    let rows = rows
        .iter()
        .take(SAME_NAME_ROWS_MAX)
        .map(|entity| {
            let mut row = json!({
                "id": entity.id.to_string(),
                "name": entity.name,
                "kind": entity.kind,
                "role": entity.role,
                "signature": entity.signature,
                "projection": {
                    "path": entity.file_origin.as_ref().map(|file| file.0.clone()),
                },
                "start_line": crate::handlers::common::entity_presentation_start_line(entity),
                "end_line": crate::handlers::common::entity_presentation_end_line(entity),
                "resolution": "same_name",
            });
            // Siblings share their name, so the module is what tells them
            // apart: one renderer's host config from another's.
            if let Some(module) = crate::semantic_owners::entity_module(store, entity) {
                row["module"] = json!(module);
            }
            crate::semantic_owners::mark_shared_name(store, entity, &mut row);
            row
        })
        .collect();
    (rows, withheld)
}

/// Where an impact answer counts the declarations that share a changed
/// function's name and that the graph holds no entity for: a variable bound
/// to a function, or a re-export. They are siblings a reader of a
/// module-swapped implementation has to change too, and no row can name them.
pub const SAME_NAME_NOT_HELD_KEY: &str = "same_name_not_held";

/// How many declarations in JavaScript and TypeScript files share the short
/// name of a changed, module-swapped function and are not entities, read
/// from the declaration census the way a name search reads it. Zero when no
/// changed entity is such a function or the census cannot say.
pub fn same_name_not_held<S: kin_model::graph::EntityStore>(store: &S, changed: &[Entity]) -> u64 {
    let mut names: Vec<&str> = changed
        .iter()
        .filter(|focal| is_script_callable(focal))
        .map(|focal| short_name(&focal.name))
        .filter(|name| !name.is_empty())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
        .into_iter()
        .map(|name| {
            let reading = crate::declaration_coverage::observe(
                store,
                Some(&[LanguageId::JavaScript, LanguageId::TypeScript]),
                &crate::declaration_coverage::NameQuery::new(name, None),
                &[],
            );
            let listed = reading
                .get("matching_unadmitted")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            // The census matches a name pattern; only the name itself is a
            // sibling. Rows past the ones it lists are counted as they are.
            let exact = listed
                .iter()
                .filter(|row| {
                    row.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|held| short_name(held) == name)
                })
                .count() as u64;
            let total = reading
                .get("matching_unadmitted_total")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            exact + total.saturating_sub(listed.len() as u64)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use kin_db::InMemoryGraph;
    use kin_model::graph::EntityStore as _;
    use kin_model::relation::{Relation, RelationOrigin};
    use kin_model::{EntityKind, GraphNodeId, LanguageId, RelationId, RelationKind};
    use serde_json::{json, Value};

    use super::Entity;
    use crate::call_sites::fixture::spanned_entity;
    use crate::session::SessionRegistry;
    use crate::types::ContentBlock;

    fn script(name: &str, file: &str, kind: EntityKind, language: LanguageId) -> Entity {
        let mut entity = spanned_entity(
            name,
            file,
            language,
            0,
            "function commitTextUpdate(textInstance, oldText, newText) {}\n",
        );
        entity.kind = kind;
        entity.signature = format!("{name}(textInstance, oldText, newText)");
        entity
    }

    async fn impact(store: &InMemoryGraph, focal: &Entity) -> Value {
        let sessions = SessionRegistry::empty_for_test();
        let result = crate::handlers::review::handle_impact_analysis(
            &HashMap::from([
                ("entity_ids".to_string(), json!([focal.id.to_string()])),
                ("include_traffic".to_string(), json!(false)),
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
        serde_json::from_str(text).unwrap()
    }

    /// An interface method whose implementations could not be read is said in
    /// the direct section's reading, so the section is not settled and does
    /// not read as one no method implements.
    #[test]
    fn an_unread_implementations_walk_unsettles_the_direct_reading() {
        let scans = super::DirectScans {
            implementations_unread: vec!["Writer.Write".to_string()],
            ..super::DirectScans::default()
        };
        let block = scans.call_sites_block();
        assert_eq!(block["settled"], false, "{block}");
        let clauses = block["clauses"].as_array().unwrap();
        assert!(
            clauses
                .iter()
                .any(|clause| clause.as_str().is_some_and(|clause| {
                    clause.starts_with("retrieval_degraded:")
                        && clause
                            .contains("the methods that implement Writer.Write could not be read")
                })),
            "{block}"
        );
        assert!(
            !super::DirectScans::default()
                .call_sites_block()
                .to_string()
                .contains("methods that implement"),
            "a walk that was read adds no clause"
        );
    }

    /// Every list the direct section answers with is paged in its reading
    /// order, and the methods that may implement an interface method are also
    /// trimmed by the reply budget like the rest of the direct answer.
    #[test]
    fn every_direct_row_list_is_paged_and_the_implementations_are_budgeted() {
        let paged = crate::trace_pages::PageKind::Impact.collections();
        for key in super::DIRECT_ROWS {
            assert!(paged.contains(&key), "{key} is not paged");
        }
        let at = |key: &str| paged.iter().position(|held| *held == key).unwrap();
        assert!(at("affected_overrides") < at(super::INTERFACE_IMPLEMENTATIONS_KEY));
        assert!(at(super::INTERFACE_IMPLEMENTATIONS_KEY) < at(super::SAME_NAME_KEY));
        assert!(crate::budget::governed_collections("impact_analysis")
            .contains(&super::INTERFACE_IMPLEMENTATIONS_KEY));
    }

    /// A renderer's host config is swapped in by module: each renderer
    /// declares its own `commitTextUpdate`, and no edge links them. Changing
    /// one lists the others beside the direct answer, as name matches that
    /// leave its verdict as it was.
    #[tokio::test]
    async fn a_module_swapped_script_function_lists_its_same_name_siblings_ungraded() {
        let dom = script(
            "commitTextUpdate",
            "packages/react-dom-bindings/src/client/ReactFiberConfigDOM.js",
            EntityKind::Function,
            LanguageId::JavaScript,
        );
        let art = script(
            "commitTextUpdate",
            "packages/react-art/src/ReactFiberConfigART.js",
            EntityKind::Function,
            LanguageId::JavaScript,
        );
        let test_host = script(
            "commitTextUpdate",
            "packages/react-test-renderer/src/ReactFiberConfigTestHost.js",
            EntityKind::Function,
            LanguageId::JavaScript,
        );
        let noop = script(
            "createReactNoop.<locals>.mutationHostConfig.commitTextUpdate",
            "packages/react-noop-renderer/src/createReactNoop.js",
            EntityKind::Method,
            LanguageId::JavaScript,
        );
        let typed = script(
            "commitTextUpdate",
            "packages/react-native-renderer/src/ReactFiberConfigNative.ts",
            EntityKind::Function,
            LanguageId::TypeScript,
        );
        // Not the same short name, not a callable, and not a script.
        let longer = script(
            "commitTextUpdateLater",
            "packages/shared/later.js",
            EntityKind::Function,
            LanguageId::JavaScript,
        );
        let class = script(
            "commitTextUpdate",
            "packages/shared/klass.js",
            EntityKind::Class,
            LanguageId::JavaScript,
        );
        let python = script(
            "commitTextUpdate",
            "tools/commit.py",
            EntityKind::Function,
            LanguageId::Python,
        );

        let alone = InMemoryGraph::new();
        alone.upsert_entity(&dom).unwrap();
        let without = impact(&alone, &dom).await;
        assert!(without.get(super::SAME_NAME_KEY).is_none(), "{without}");

        let store = InMemoryGraph::new();
        for entity in [
            &dom, &art, &test_host, &noop, &typed, &longer, &class, &python,
        ] {
            store.upsert_entity(entity).unwrap();
        }
        let with = impact(&store, &dom).await;
        let rows = with[super::SAME_NAME_KEY].as_array().expect("rows");
        let mut names: Vec<(&str, &str)> = rows
            .iter()
            .map(|row| {
                (
                    row["name"].as_str().unwrap(),
                    row["projection"]["path"].as_str().unwrap(),
                )
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                (
                    "commitTextUpdate",
                    "packages/react-art/src/ReactFiberConfigART.js"
                ),
                (
                    "commitTextUpdate",
                    "packages/react-native-renderer/src/ReactFiberConfigNative.ts"
                ),
                (
                    "commitTextUpdate",
                    "packages/react-test-renderer/src/ReactFiberConfigTestHost.js"
                ),
                (
                    "createReactNoop.<locals>.mutationHostConfig.commitTextUpdate",
                    "packages/react-noop-renderer/src/createReactNoop.js"
                ),
            ],
            "{with}"
        );
        for row in rows {
            for field in ["id", "name", "kind", "signature", "start_line"] {
                assert!(!row[field].is_null(), "{field}: {row}");
            }
            assert_eq!(row["resolution"], "same_name", "{row}");
        }
        assert_eq!(with[super::SAME_NAME_NOTE_KEY], super::SAME_NAME_NOTE);
        assert_eq!(
            with["sections"]["direct"]["rows"],
            json!([
                "affected_callers",
                "affected_overrides",
                super::SAME_NAME_KEY
            ])
        );
        // Name matches never raise or lower the direct verdict.
        assert_eq!(
            with["_kin"]["verdict"]["state"], without["_kin"]["verdict"]["state"],
            "{with}"
        );
        assert_eq!(
            with["_kin"]["verdict"]["limiting_factor"],
            without["_kin"]["verdict"]["limiting_factor"]
        );

        // A function an override edge already answers for gets no list.
        store
            .upsert_relation(&Relation {
                id: RelationId::new(),
                kind: RelationKind::Overrides,
                src: GraphNodeId::Entity(art.id),
                dst: GraphNodeId::Entity(dom.id),
                confidence: 1.0,
                origin: RelationOrigin::Parsed,
                created_in: None,
                import_source: None,
                evidence: Vec::new(),
            })
            .unwrap();
        let linked = impact(&store, &dom).await;
        assert!(linked.get(super::SAME_NAME_KEY).is_none(), "{linked}");
    }
}
