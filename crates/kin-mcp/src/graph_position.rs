// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Where an entity sits in the call graph, as a caller of `locate` may sort
//! its leading rows by it.
//!
//! Relevance stays the default order. `sort` re-orders the
//! [`SORT_WINDOW`] most relevant rows by one fact and leaves the rest in
//! relevance order, and each re-sorted row carries the fact it was sorted by:
//!
//! - `callers`: most proven callers first. A proven caller is a non-test
//!   entity with a resolved call to the row. Call resolution is partial, so
//!   the count is a floor: at least that many.
//! - `hops`: nearest a root first, where a root is a callable no non-test
//!   entity is proven to call (a public entry point, a command, a handler, or
//!   code whose callers Kin could not resolve). The count is the fewest calls
//!   from such a root to the row, over resolved calls only.
//!
//! Measured before this was built, on 1,032 descriptions over eight Python
//! repositories that never name the function they describe: re-sorting the 20
//! most relevant rows by callers, always, put the answer in the top five 31.6%
//! of the time against 59.2% for relevance; by hops, 39.9%. Graph position does
//! not tell the answer from the rows around it. A sort helps a question that is
//! about position itself ("which of these is called the most"), not a search
//! for what a description describes. Confining rows to a package helps far
//! more when the package is known: 59.2% to 79.7% at the top five.
//!
//! Every fact is read from the graph alone, never from files.

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::Value;

use kin_model::graph::GraphStore;
use kin_model::relation::{GraphNodeId, RelationKind};
use kin_model::{EntityId, EntityRole};

/// The argument that names the order.
pub const SORT_ARGUMENT: &str = "sort";

/// The orders `sort` takes, the default first.
pub const SORT_KEYS: [&str; 3] = ["relevance", "callers", "hops"];

/// How many of the most relevant rows a sort re-orders. The measured window.
pub const SORT_WINDOW: usize = 20;

/// Where a re-sorted row carries its proven caller count.
pub const CALLERS_KEY: &str = "proven_callers";

/// Where a re-sorted row carries its distance from the nearest root.
pub const HOPS_KEY: &str = "hops_from_root";

/// Where a reply says how its rows were sorted: the key and the window.
pub const SORTED_KEY: &str = "sorted";

/// How many entities a walk toward a root reads before it says it found none.
const HOPS_WALK_MAX: usize = 5_000;

/// An order other than relevance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowSort {
    Callers,
    Hops,
}

impl RowSort {
    /// The order `arguments` ask for, `None` for relevance or none given.
    pub fn from_arguments(arguments: &HashMap<String, Value>) -> Result<Option<Self>, String> {
        let Some(value) = arguments
            .get(SORT_ARGUMENT)
            .filter(|value| !value.is_null())
        else {
            return Ok(None);
        };
        match value.as_str() {
            Some("relevance") => Ok(None),
            Some("callers") => Ok(Some(Self::Callers)),
            Some("hops") => Ok(Some(Self::Hops)),
            _ => Err(format!("sort must be one of {}.", SORT_KEYS.join(", "))),
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Callers => "callers",
            Self::Hops => "hops",
        }
    }
}

/// Where a row re-sorted by `sort` carries its fact.
pub fn fact_key(sort: RowSort) -> &'static str {
    match sort {
        RowSort::Callers => CALLERS_KEY,
        RowSort::Hops => HOPS_KEY,
    }
}

/// The non-test entities proven to call `id`, distinct, excluding itself.
fn callers_of<G: GraphStore + ?Sized>(store: &G, id: &EntityId) -> Vec<EntityId> {
    let target = GraphNodeId::Entity(*id);
    let mut seen = HashSet::new();
    // Incoming relations are read with the outgoing ones; a call to `id` is
    // one whose target it is.
    store
        .get_all_relations_for_entity(id)
        .unwrap_or_default()
        .into_iter()
        .filter(|relation| relation.kind == RelationKind::Calls && relation.dst == target)
        .filter_map(|relation| match relation.src {
            GraphNodeId::Entity(caller) if caller != *id => Some(caller),
            _ => None,
        })
        .filter(|caller| seen.insert(*caller))
        .filter(|caller| {
            store
                .get_entity(caller)
                .ok()
                .flatten()
                .is_some_and(|entity| entity.role != EntityRole::Test)
        })
        .collect()
}

/// How many non-test entities are proven to call `id`.
pub fn proven_callers<G: GraphStore + ?Sized>(store: &G, id: &EntityId) -> u64 {
    callers_of(store, id).len() as u64
}

/// The fewest resolved calls from a root to `id`: 0 when nothing proven
/// calls it. `None` when every walk back runs into a cycle of callers or
/// past [`HOPS_WALK_MAX`] entities.
pub fn hops_from_root<G: GraphStore + ?Sized>(store: &G, id: &EntityId) -> Option<u64> {
    let mut seen: HashSet<EntityId> = HashSet::from([*id]);
    let mut queue: VecDeque<(EntityId, u64)> = VecDeque::from([(*id, 0)]);
    while let Some((entity, depth)) = queue.pop_front() {
        let callers = callers_of(store, &entity);
        if callers.is_empty() {
            return Some(depth);
        }
        for caller in callers {
            if seen.insert(caller) {
                if seen.len() > HOPS_WALK_MAX {
                    return None;
                }
                queue.push_back((caller, depth + 1));
            }
        }
    }
    None
}

/// The fact `sort` orders by, for `id`.
pub fn fact<G: GraphStore + ?Sized>(store: &G, id: &EntityId, sort: RowSort) -> Option<u64> {
    match sort {
        RowSort::Callers => Some(proven_callers(store, id)),
        RowSort::Hops => hops_from_root(store, id),
    }
}

/// The new order of rows given in relevance order, by their entity ids: the
/// leading [`SORT_WINDOW`] re-sorted by `sort`, the rest as they were. Ties
/// and rows with no fact keep their relevance order, a row with no fact after
/// every row with one. Returns the order as indices into `ids` and each
/// re-sorted index's fact.
pub fn sort_window<G: GraphStore + ?Sized>(
    store: &G,
    ids: &[Option<EntityId>],
    sort: RowSort,
) -> (Vec<usize>, HashMap<usize, Option<u64>>) {
    let window = ids.len().min(SORT_WINDOW);
    let facts: HashMap<usize, Option<u64>> = (0..window)
        .map(|index| (index, ids[index].and_then(|id| fact(store, &id, sort))))
        .collect();
    let mut order: Vec<usize> = (0..window).collect();
    order.sort_by_key(|index| {
        let rank = match (sort, facts[index]) {
            (_, None) => (1, 0),
            (RowSort::Callers, Some(count)) => (0, u64::MAX - count),
            (RowSort::Hops, Some(hops)) => (0, hops),
        };
        (rank, *index)
    });
    order.extend(window..ids.len());
    (order, facts)
}

/// Re-order JSON rows that carry an entity id under `id_key`, and write the
/// fact on each re-sorted row. Returns how many rows were re-sorted.
pub fn sort_rows<G: GraphStore + ?Sized>(
    store: &G,
    rows: &mut Vec<Value>,
    id_key: &str,
    sort: RowSort,
) -> usize {
    let ids: Vec<Option<EntityId>> = rows
        .iter()
        .map(|row| {
            row.get(id_key)
                .and_then(Value::as_str)
                .and_then(|id| uuid::Uuid::parse_str(id).ok())
                .map(EntityId)
        })
        .collect();
    let (order, facts) = sort_window(store, &ids, sort);
    let mut taken: Vec<Option<Value>> = std::mem::take(rows).into_iter().map(Some).collect();
    for index in order {
        let Some(mut row) = taken[index].take() else {
            continue;
        };
        if let Some(fact) = facts.get(&index) {
            row[fact_key(sort)] = match fact {
                Some(value) => Value::from(*value),
                None => Value::Null,
            };
        }
        rows.push(row);
    }
    facts.len()
}

/// The block a sorted reply carries: which order, over how many rows.
pub fn sorted_block(sort: RowSort, rows: usize) -> Value {
    serde_json::json!({ "by": sort.key(), "rows": rows, "window": SORT_WINDOW })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_db::InMemoryGraph;
    use kin_model::graph::EntityStore;
    use kin_model::relation::Relation;
    use kin_model::RelationId;
    use serde_json::json;

    fn entity(name: &str, role: EntityRole) -> kin_model::Entity {
        use kin_model::{
            EntityKind, EntityMetadata, FilePathId, FingerprintAlgorithm, Hash256, LanguageId,
            SemanticFingerprint, Visibility,
        };
        kin_model::Entity {
            id: EntityId::new(),
            kind: EntityKind::Function,
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
            file_origin: Some(FilePathId::new("src/app.py")),
            span: None,
            signature: format!("def {name}()"),
            visibility: Visibility::Public,
            role,
            doc_summary: None,
            metadata: EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    fn call(graph: &InMemoryGraph, from: &kin_model::Entity, to: &kin_model::Entity) {
        graph
            .upsert_relation(&Relation {
                id: RelationId::new(),
                kind: RelationKind::Calls,
                src: GraphNodeId::Entity(from.id),
                dst: GraphNodeId::Entity(to.id),
                confidence: 1.0,
                origin: kin_model::RelationOrigin::Parsed,
                created_in: None,
                import_source: None,
                evidence: Vec::new(),
            })
            .unwrap();
    }

    /// `main` calls `serve`, which calls `handle` and `log`; `handle` calls
    /// `log` too, and a test calls `log`. Callers count non-test callers;
    /// hops count calls from the nearest root.
    #[test]
    fn callers_and_hops_are_read_from_resolved_non_test_calls() {
        let graph = InMemoryGraph::new();
        let [main, serve, handle, log] =
            ["main", "serve", "handle", "log"].map(|name| entity(name, EntityRole::Source));
        let test = entity("test_log", EntityRole::Test);
        for entity in [&main, &serve, &handle, &log, &test] {
            graph.upsert_entity(entity).unwrap();
        }
        call(&graph, &main, &serve);
        call(&graph, &serve, &handle);
        call(&graph, &serve, &log);
        call(&graph, &handle, &log);
        call(&graph, &test, &log);

        assert_eq!(proven_callers(&graph, &log.id), 2, "the test is no caller");
        assert_eq!(proven_callers(&graph, &main.id), 0);
        assert_eq!(hops_from_root(&graph, &main.id), Some(0));
        assert_eq!(hops_from_root(&graph, &serve.id), Some(1));
        assert_eq!(hops_from_root(&graph, &log.id), Some(2));

        // Rows in relevance order: handle, log, main. Sorted by callers, log
        // (2) leads, then handle (1), then main (0); by hops, main (a root)
        // leads.
        let mut rows: Vec<Value> = [&handle, &log, &main]
            .iter()
            .map(|entity| json!({"entity_id": entity.id.to_string(), "name": entity.name}))
            .collect();
        assert_eq!(
            sort_rows(&graph, &mut rows, "entity_id", RowSort::Callers),
            3
        );
        let names: Vec<&str> = rows
            .iter()
            .map(|row| row["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["log", "handle", "main"]);
        assert_eq!(rows[0][CALLERS_KEY], json!(2));
        sort_rows(&graph, &mut rows, "entity_id", RowSort::Hops);
        let names: Vec<&str> = rows
            .iter()
            .map(|row| row["name"].as_str().unwrap())
            .collect();
        // `handle` and `log` are both two calls from `main`; a tie keeps the
        // order the rows came in.
        assert_eq!(names, ["main", "log", "handle"]);
        assert_eq!(rows[0][HOPS_KEY], json!(0));
        assert_eq!(rows[2][HOPS_KEY], json!(2));
    }

    /// Only the leading window is re-sorted; the rest keep relevance order,
    /// and relevance is no sort at all.
    #[test]
    fn a_sort_reorders_only_the_leading_window_and_relevance_is_none() {
        let graph = InMemoryGraph::new();
        let ids: Vec<Option<EntityId>> = (0..SORT_WINDOW + 5).map(|_| None).collect();
        let (order, facts) = sort_window(&graph, &ids, RowSort::Callers);
        assert_eq!(order, (0..SORT_WINDOW + 5).collect::<Vec<_>>());
        assert_eq!(facts.len(), SORT_WINDOW);
        let none = HashMap::from([(SORT_ARGUMENT.to_string(), json!("relevance"))]);
        assert_eq!(RowSort::from_arguments(&none), Ok(None));
        let bad = HashMap::from([(SORT_ARGUMENT.to_string(), json!("size"))]);
        assert!(RowSort::from_arguments(&bad).is_err());
    }
}

/// The options as a caller reaches them: through `locate`'s routing and the
/// handlers each mode answers from.
#[cfg(test)]
mod route_tests {
    use std::collections::HashMap;

    use kin_db::InMemoryGraph;
    use kin_model::graph::EntityStore;
    use kin_model::relation::{GraphNodeId, Relation, RelationKind};
    use kin_model::{RelationId, RelationOrigin};
    use serde_json::{json, Value};

    fn ids_named(graph: &InMemoryGraph, name: &str) -> Vec<kin_model::EntityId> {
        graph
            .list_all_entities()
            .unwrap()
            .into_iter()
            .filter(|entity| entity.name == name)
            .map(|entity| entity.id)
            .collect()
    }

    fn call(graph: &InMemoryGraph, from: kin_model::EntityId, to: kin_model::EntityId) {
        graph
            .upsert_relation(&Relation {
                id: RelationId::new(),
                kind: RelationKind::Calls,
                src: GraphNodeId::Entity(from),
                dst: GraphNodeId::Entity(to),
                confidence: 1.0,
                origin: RelationOrigin::Parsed,
                created_in: None,
                import_source: None,
                evidence: Vec::new(),
            })
            .unwrap();
    }

    fn name_search(graph: &InMemoryGraph, args: Value) -> Value {
        let args: HashMap<String, Value> = serde_json::from_value(args).unwrap();
        let result = crate::handlers::entities::handle_semantic_search(&args, graph).unwrap();
        let crate::types::ContentBlock::Text { text } = result.content.first().unwrap();
        serde_json::from_str(text).unwrap()
    }

    fn graph() -> InMemoryGraph {
        let graph = InMemoryGraph::default();
        crate::declaration_coverage::fixtures::admit(
            &graph,
            "src/app/core.py",
            "def run():\n    return 1\n\ndef main():\n    return 2\n\ndef helper():\n    return 3\n",
            false,
        );
        crate::declaration_coverage::fixtures::admit(
            &graph,
            "src/tools/jobs.py",
            "def run():\n    return 4\n",
            false,
        );
        graph
    }

    /// A name lookup confined to a module path lists the declarations inside
    /// it and counts the ones left out, and a scoped lookup claims no closure
    /// over the name.
    #[test]
    fn a_name_lookup_keeps_the_rows_inside_its_scope() {
        let graph = graph();
        let whole = name_search(&graph, json!({"query": "run"}));
        assert_eq!(whole["total_matches"], json!(2), "{whole}");
        let scoped = name_search(&graph, json!({"query": "run", "scope": "jobs"}));
        assert_eq!(scoped["total_matches"], json!(1), "{scoped}");
        assert_eq!(scoped["row_scope"]["outside"], json!(1), "{scoped}");
        assert!(scoped["row_scope"]["module"]
            .as_str()
            .unwrap()
            .ends_with("jobs"));
        assert!(scoped.get("own_name_rows_complete").is_none(), "{scoped}");
        let refused = crate::trace_scope::RowScope::from_arguments(
            &graph,
            &HashMap::from([("scope".to_string(), json!("no_such_module"))]),
        );
        assert!(refused.is_err(), "a scope that names no module is refused");
    }

    /// Sorted by callers, the declaration more code is proven to call leads,
    /// each re-sorted row says how many, and the reply says how it sorted.
    #[test]
    fn a_name_lookup_sorted_by_callers_puts_the_most_called_first() {
        let graph = graph();
        let runs = ids_named(&graph, "run");
        let (core_run, jobs_run) = {
            let core = graph.get_entity(&runs[0]).unwrap().unwrap();
            if core.file_origin.as_ref().unwrap().0.contains("core") {
                (runs[0], runs[1])
            } else {
                (runs[1], runs[0])
            }
        };
        let main = ids_named(&graph, "main")[0];
        let helper = ids_named(&graph, "helper")[0];
        call(&graph, main, jobs_run);
        call(&graph, helper, jobs_run);
        call(&graph, main, core_run);

        let sorted = name_search(&graph, json!({"query": "run", "sort": "callers"}));
        let rows = sorted["results"].as_array().unwrap();
        assert_eq!(rows[0][super::CALLERS_KEY], json!(2), "{sorted}");
        assert_eq!(rows[1][super::CALLERS_KEY], json!(1), "{sorted}");
        assert_eq!(sorted[super::SORTED_KEY]["by"], json!("callers"));

        let by_hops = name_search(&graph, json!({"query": "run", "sort": "hops"}));
        let rows = by_hops["results"].as_array().unwrap();
        assert_eq!(rows[0][super::HOPS_KEY], json!(1), "{by_hops}");
    }

    /// `locate` refuses a sort on a text search, whose rows are every match,
    /// and a scope or sort sent beside a cursor, which keeps its own.
    #[test]
    fn locate_refuses_a_sort_on_text_and_a_new_scope_on_a_continuation() {
        let route = |arguments: Value| {
            let mut name = "locate".to_string();
            let mut arguments: HashMap<String, Value> = serde_json::from_value(arguments).unwrap();
            crate::verbs::resolve(&mut name, &mut arguments).map(|()| name)
        };
        assert!(route(json!({"query": "retry", "mode": "text", "sort": "callers"})).is_err());
        assert!(route(json!({"query": "retry", "mode": "text", "scope": "flask"})).is_ok());
        assert!(route(json!({"cursor": "lit.1.2.abc", "scope": "flask"})).is_err());
        assert!(route(json!({"query": "run", "mode": "name", "sort": "hops"})).is_ok());
        assert!(route(
            json!({"query": "where retries happen", "scope": "flask", "sort": "callers"})
        )
        .is_ok());
    }
}
