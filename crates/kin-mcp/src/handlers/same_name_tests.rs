// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A name several distinct entities carry is never answered for one of them.
//!
//! The head-to-head measured three silent picks from one rule: `find_references`
//! on `commitTextUpdate` answered for the ART copy of three renderer
//! implementations, `find_references` on `_make_timedelta` answered for
//! `sansio/app.py`'s copy when the question was about `app.py`'s, and
//! `trace_data_flow` on `advanceTimers` walked the scheduler mock's copy. Each
//! name is the whole name of several entities, and the resolver ranked one in.
//!
//! These tests hold the replacement: `find_references` sections every distinct
//! candidate and grades each section as its own answer, the single-focal
//! walkers refuse with every candidate and the spelling that names each one
//! alone, a module-qualified name pins one through the Module entities the
//! graph holds, and the twins of one declaration still count once. The
//! fixtures are JavaScript, Python and C shaped, and nothing asserted here
//! depends on the language.

use std::collections::HashMap;

use kin_db::InMemoryGraph;
use kin_model::ids::RelationId;
use kin_model::relation::{Relation, RelationOrigin};
use kin_model::{
    Entity, EntityId, EntityKind, EntityMetadata, EntityRole, EntityStore, FilePathId,
    FingerprintAlgorithm, GraphNodeId, Hash256, LanguageId, RelationKind, SemanticFingerprint,
    Visibility,
};
use serde_json::Value;

use super::entities::{handle_find_references, handle_trace_data_flow, AMBIGUOUS_CANDIDATES_KEY};
use super::path::handle_trace_path;
use crate::types::{ContentBlock, ToolCallResult};

/// An entity the way the unit fixtures elsewhere in this crate build one: no
/// source span, so no reply reaches for caller text a repository authority
/// would have to serve.
fn entity(language: LanguageId, kind: EntityKind, name: &str, file: &str) -> Entity {
    Entity {
        id: EntityId::new(),
        kind,
        name: name.to_string(),
        language,
        fingerprint: SemanticFingerprint {
            algorithm: FingerprintAlgorithm::V1TreeSitter,
            ast_hash: Hash256::from_bytes([0; 32]),
            signature_hash: Hash256::from_bytes([0; 32]),
            behavior_hash: Hash256::from_bytes([0; 32]),
            equivalence_hash: Hash256::from_bytes([0; 32]),
            stability_score: 1.0,
        },
        file_origin: Some(FilePathId::new(file)),
        span: None,
        signature: format!("function {name}()"),
        visibility: Visibility::Public,
        role: EntityRole::Source,
        doc_summary: None,
        metadata: EntityMetadata::default(),
        lineage_parent: None,
        created_in: None,
        superseded_by: None,
    }
}

fn function(language: LanguageId, name: &str, file: &str) -> Entity {
    entity(language, EntityKind::Function, name, file)
}

/// The module surface an adapter mints for a file: kind `Module`, named by the
/// language's own coordinate.
fn module(language: LanguageId, name: &str, file: &str) -> Entity {
    let mut module = entity(language, EntityKind::Module, name, file);
    module.signature = format!("module {file}");
    module
}

fn calls(src: &Entity, dst: &Entity) -> Relation {
    Relation {
        id: RelationId::new(),
        kind: RelationKind::Calls,
        src: GraphNodeId::Entity(src.id),
        dst: GraphNodeId::Entity(dst.id),
        confidence: 1.0,
        origin: RelationOrigin::Parsed,
        created_in: None,
        import_source: None,
        evidence: Vec::new(),
    }
}

fn store_of(entities: &[&Entity], relations: &[Relation]) -> InMemoryGraph {
    let store = InMemoryGraph::new();
    for entity in entities {
        store.upsert_entity(entity).unwrap();
    }
    for relation in relations {
        store.upsert_relation(relation).unwrap();
    }
    store
}

fn body(result: &ToolCallResult) -> Value {
    let ContentBlock::Text { text } = result.content.first().expect("a content block");
    serde_json::from_str(text).unwrap_or_else(|_| panic!("JSON: {text}"))
}

fn args(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.clone()))
        .collect()
}

async fn references(store: &InMemoryGraph, query: &str) -> Value {
    body(
        &handle_find_references(&args(&[("query", serde_json::json!(query))]), store, None)
            .await
            .unwrap(),
    )
}

fn ready_envelope() -> crate::Envelope {
    crate::Envelope::daemon().with_health(&serde_json::json!({
        "initialized": true,
        "graph_loaded": true,
        "graph_generation": 12,
        "graph_entity_count": 12,
    }))
}

/// The react shape: three renderers' host configs each define
/// `commitTextUpdate`, and the reconciler calls the DOM one.
struct Renderers {
    store: InMemoryGraph,
    dom: Entity,
    art: Entity,
    test_host: Entity,
    caller: Entity,
}

fn renderers() -> Renderers {
    let js = LanguageId::JavaScript;
    let dom_file = "packages/react-dom-bindings/src/client/ReactFiberConfigDOM.js";
    let art_file = "packages/react-art/src/ReactFiberConfigART.js";
    let test_file = "packages/react-test-renderer/src/ReactFiberConfigTestHost.js";
    let caller_file = "packages/react-reconciler/src/ReactFiberCommitHostEffects.js";
    let dom = function(js, "commitTextUpdate", dom_file);
    let art = function(js, "commitTextUpdate", art_file);
    let test_host = function(js, "commitTextUpdate", test_file);
    let caller = function(js, "commitHostTextUpdate", caller_file);
    let modules = [
        module(js, "ReactFiberConfigDOM", dom_file),
        module(js, "ReactFiberConfigART", art_file),
        module(js, "ReactFiberConfigTestHost", test_file),
        module(js, "ReactFiberCommitHostEffects", caller_file),
    ];
    let mut all: Vec<&Entity> = vec![&dom, &art, &test_host, &caller];
    all.extend(modules.iter());
    let store = store_of(&all, &[calls(&caller, &dom)]);
    Renderers {
        store,
        dom,
        art,
        test_host,
        caller,
    }
}

#[tokio::test]
async fn a_whole_name_three_renderers_define_is_answered_in_a_section_each() {
    let fixture = renderers();
    let reply = references(&fixture.store, "commitTextUpdate").await;

    assert_eq!(reply["ambiguous_focal"], true, "{reply}");
    assert_eq!(reply["resolution"], "same_name", "{reply}");
    assert_eq!(reply["candidate_count"], 3, "{reply}");
    assert!(
        reply.get("focal_entity").is_none() && reply.get("references").is_none(),
        "a sectioned answer names no single winner at the top level: {reply}"
    );
    let sections = reply[AMBIGUOUS_CANDIDATES_KEY]
        .as_array()
        .expect("sections");
    let mut by_id: HashMap<String, &Value> = HashMap::new();
    for section in sections {
        by_id.insert(section["entity_id"].as_str().unwrap().to_string(), section);
    }
    for (expected, qualified) in [
        (&fixture.dom, "ReactFiberConfigDOM.commitTextUpdate"),
        (&fixture.art, "ReactFiberConfigART.commitTextUpdate"),
        (
            &fixture.test_host,
            "ReactFiberConfigTestHost.commitTextUpdate",
        ),
    ] {
        let section = by_id
            .get(&expected.id.to_string())
            .unwrap_or_else(|| panic!("every renderer gets its own section: {reply}"));
        assert_eq!(section["qualified_name"], qualified, "{section}");
        assert_eq!(section["focal_entity"]["name"], "commitTextUpdate");
        assert!(section["references"].is_array(), "{section}");
    }
    let dom_rows = by_id[&fixture.dom.id.to_string()]["references"]
        .as_array()
        .unwrap();
    assert!(
        dom_rows
            .iter()
            .any(|row| row.to_string().contains(&fixture.caller.id.to_string())),
        "the DOM section carries its caller: {dom_rows:?}"
    );
    assert_eq!(
        by_id[&fixture.art.id.to_string()]["total_upstream"],
        0,
        "the ART section carries only its own callers"
    );

    // Every candidate was answered for, so the resolution is settled rather
    // than a choice, and nothing is reported as degraded.
    let resolution = &reply["focal_resolution"];
    assert_eq!(resolution["addressed_by"], "name");
    assert_eq!(resolution["matched"], "exact_name");
    assert_eq!(resolution["same_name_candidates"], 3);
    assert_eq!(resolution["sectioned"], 3);
    assert_eq!(resolution["other_candidates"], serde_json::json!([]));
    assert!(reply.get("degradations").is_none(), "{reply}");
}

#[tokio::test]
async fn a_sectioned_answer_grades_each_section_and_stays_small_under_answer_only() {
    let fixture = renderers();
    let raw = handle_find_references(
        &args(&[("query", serde_json::json!("commitTextUpdate"))]),
        &fixture.store,
        None,
    )
    .await
    .unwrap();

    let whole = body(&crate::finalize_with_envelope(
        raw.clone(),
        ready_envelope(),
        "find_references",
    ));
    let sections = whole[AMBIGUOUS_CANDIDATES_KEY].as_array().unwrap();
    let mut worst_inconclusive = false;
    for section in sections {
        let state = section["_kin"]["verdict"]["state"]
            .as_str()
            .unwrap_or_else(|| panic!("every section carries its own verdict: {section}"));
        worst_inconclusive |= state == "inconclusive";
    }
    let top = &whole["_kin"]["verdict"];
    assert_eq!(
        top["state"] == "inconclusive",
        worst_inconclusive,
        "the response's verdict is the worst of its sections': {top}"
    );
    assert_eq!(top["absence_claim"], "not_applicable", "{top}");
    let factor = top["limiting_factor"].as_str().unwrap_or_default();
    assert!(
        !factor.contains("edge_coverage_unreported") && !factor.contains("focal_resolution"),
        "the top level is not graded as a reference list it never had: {factor}"
    );
    for section in sections {
        for code in section["_kin"]["verdict"]["limiting_factor"]
            .as_str()
            .unwrap_or_default()
            .split("; ")
            .filter(|code| !code.is_empty())
        {
            assert!(factor.contains(code), "{code} is composed into {factor}");
        }
    }

    let narrow = body(&crate::finalize_with_envelope_bounded(
        raw,
        ready_envelope(),
        "find_references",
        &crate::budget::ResponseBudget {
            answer_only: true,
            ..crate::budget::ResponseBudget::default()
        },
    ));
    let narrow_sections = narrow[AMBIGUOUS_CANDIDATES_KEY].as_array().unwrap();
    assert_eq!(narrow_sections.len(), 3);
    for section in narrow_sections {
        assert!(section["entity_id"].is_string() && section["qualified_name"].is_string());
        assert!(section["references"].is_array(), "{section}");
        assert!(section["_kin"]["verdict"]["state"].is_string(), "{section}");
        for dropped in [
            "edge_coverage",
            "caller_arrival",
            "negative",
            "focal_resolution",
        ] {
            assert!(
                section.get(dropped).is_none(),
                "{dropped} rides a narrowed section"
            );
        }
    }
    assert_eq!(narrow["_kin"]["shape"], "answer_only", "{narrow}");
    let narrow_bytes = serde_json::to_string(&narrow).unwrap().len();
    let whole_bytes = serde_json::to_string(&whole).unwrap().len();
    assert!(
        narrow_bytes * 2 < whole_bytes,
        "answer_only narrows each section: {narrow_bytes} of {whole_bytes} bytes"
    );
}

/// The flask shape: `_make_timedelta` in `flask/app.py` and in
/// `flask/sansio/app.py`, only the second one used.
struct Timedeltas {
    store: InMemoryGraph,
    top: Entity,
    sansio: Entity,
}

fn timedeltas() -> Timedeltas {
    let py = LanguageId::Python;
    let top = function(py, "_make_timedelta", "src/flask/app.py");
    let sansio = function(py, "_make_timedelta", "src/flask/sansio/app.py");
    let user = function(py, "App.__init__", "src/flask/sansio/app.py");
    let modules = [
        module(py, "flask", "src/flask/__init__.py"),
        module(py, "app", "src/flask/app.py"),
        module(py, "sansio", "src/flask/sansio/__init__.py"),
        module(py, "app", "src/flask/sansio/app.py"),
    ];
    let mut all: Vec<&Entity> = vec![&top, &sansio, &user];
    all.extend(modules.iter());
    let store = store_of(&all, &[calls(&user, &sansio)]);
    Timedeltas { store, top, sansio }
}

#[tokio::test]
async fn both_modules_copies_of_a_function_are_sectioned_with_names_that_pin_each() {
    let fixture = timedeltas();
    let reply = references(&fixture.store, "_make_timedelta").await;
    let sections = reply[AMBIGUOUS_CANDIDATES_KEY]
        .as_array()
        .expect("sections");
    let named: HashMap<String, String> = sections
        .iter()
        .map(|section| {
            (
                section["entity_id"].as_str().unwrap().to_string(),
                section["qualified_name"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        named[&fixture.top.id.to_string()],
        "flask.app._make_timedelta"
    );
    assert_eq!(
        named[&fixture.sansio.id.to_string()],
        "sansio.app._make_timedelta"
    );

    // Pasted back, each name answers for its own copy alone.
    for (qualified, expected) in [
        ("flask.app._make_timedelta", &fixture.top),
        ("sansio.app._make_timedelta", &fixture.sansio),
        ("flask.sansio.app._make_timedelta", &fixture.sansio),
    ] {
        let reply = references(&fixture.store, qualified).await;
        assert!(reply.get(AMBIGUOUS_CANDIDATES_KEY).is_none(), "{reply}");
        assert_eq!(
            reply["focal_entity"]["id"],
            expected.id.to_string(),
            "{qualified}: {reply}"
        );
        assert_eq!(reply["focal_resolution"]["matched"], "qualified_name");
        assert_eq!(reply["focal_resolution"]["same_name_candidates"], 1);
    }

    // A qualifier that names both modules still names both.
    let reply = references(&fixture.store, "app._make_timedelta").await;
    assert_eq!(reply["resolution"], "same_qualified_name", "{reply}");
    assert_eq!(reply[AMBIGUOUS_CANDIDATES_KEY].as_array().unwrap().len(), 2);
    // A qualifier that names no module holding the name reaches nothing.
    let reply = handle_find_references(
        &args(&[("query", serde_json::json!("cli._make_timedelta"))]),
        &fixture.store,
        None,
    )
    .await
    .unwrap();
    assert_eq!(reply.is_error, Some(true), "{:?}", body(&reply));
}

#[tokio::test]
async fn a_prototype_and_its_definition_are_one_answer_that_names_the_twin() {
    let c = LanguageId::C;
    let mut prototype = function(c, "buffer_grow", "src/buffer.h");
    prototype.signature = "int buffer_grow(buffer *b, size_t n);".to_string();
    let mut definition = function(c, "buffer_grow", "src/buffer.c");
    definition.signature = "int buffer_grow(buffer *b, size_t n)".to_string();
    let store = store_of(&[&prototype, &definition], &[]);

    let reply = references(&store, "buffer_grow").await;
    assert!(reply.get(AMBIGUOUS_CANDIDATES_KEY).is_none(), "{reply}");
    assert_eq!(
        reply["focal_entity"]["id"],
        definition.id.to_string(),
        "{reply}"
    );
    let resolution = &reply["focal_resolution"];
    assert_eq!(resolution["matched"], "exact_name");
    assert_eq!(resolution["same_name_candidates"], 2);
    let others = resolution["other_candidates"].as_array().unwrap();
    assert_eq!(others.len(), 1);
    assert_eq!(others[0]["id"], prototype.id.to_string());
}

/// The scheduler shape: `advanceTimers` in the scheduler and in its mock.
struct Schedulers {
    store: InMemoryGraph,
    real: Entity,
    handle_timeout: Entity,
    mock_caller: Entity,
}

fn schedulers() -> Schedulers {
    let js = LanguageId::JavaScript;
    let real_file = "packages/scheduler/src/forks/Scheduler.js";
    let mock_file = "packages/scheduler/src/forks/SchedulerMock.js";
    let real = function(js, "advanceTimers", real_file);
    let mock = function(js, "advanceTimers", mock_file);
    let handle_timeout = function(js, "handleTimeout", real_file);
    let mock_caller = function(js, "unstable_advanceTime", mock_file);
    let modules = [
        module(js, "scheduler", "packages/scheduler/index.js"),
        module(js, "Scheduler", real_file),
        module(js, "SchedulerMock", mock_file),
    ];
    let mut all: Vec<&Entity> = vec![&real, &mock, &handle_timeout, &mock_caller];
    all.extend(modules.iter());
    let store = store_of(
        &all,
        &[calls(&handle_timeout, &real), calls(&mock_caller, &mock)],
    );
    Schedulers {
        store,
        real,
        handle_timeout,
        mock_caller,
    }
}

fn trace(store: &InMemoryGraph, focal: &str) -> Value {
    body(
        &handle_trace_data_flow(
            &args(&[
                ("focal", serde_json::json!(focal)),
                ("direction", serde_json::json!("callers")),
            ]),
            store,
        )
        .unwrap(),
    )
}

#[test]
fn a_walk_from_a_name_two_modules_define_is_refused_with_both_candidates() {
    let fixture = schedulers();
    let reply = trace(&fixture.store, "advanceTimers");
    assert_eq!(reply["ambiguous_focal"], true, "{reply}");
    assert_eq!(reply["resolution"], "same_name", "{reply}");
    assert!(reply.get("chain").is_none(), "nothing was walked: {reply}");
    let mut named: Vec<&str> = reply["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["qualified_name"].as_str().unwrap())
        .collect();
    named.sort_unstable();
    assert_eq!(
        named,
        vec!["Scheduler.advanceTimers", "SchedulerMock.advanceTimers"]
    );
    for row in reply["candidates"].as_array().unwrap() {
        for key in ["entity_id", "name", "kind", "file_path"] {
            assert!(row.get(key).is_some(), "{key} in {row}");
        }
    }
}

#[test]
fn a_walk_re_asked_by_id_or_qualified_name_follows_that_copy_and_reports_its_resolution() {
    let fixture = schedulers();
    for focal in [
        fixture.real.id.to_string(),
        "Scheduler.advanceTimers".to_string(),
        "scheduler.Scheduler.advanceTimers".to_string(),
    ] {
        let reply = trace(&fixture.store, &focal);
        assert_eq!(reply["focal_id"], fixture.real.id.to_string(), "{reply}");
        let chain = reply["chain"].to_string();
        assert!(
            chain.contains(&fixture.handle_timeout.id.to_string()),
            "{chain}"
        );
        assert!(
            !chain.contains(&fixture.mock_caller.id.to_string()),
            "{chain}"
        );
        assert!(
            reply["focal_resolution"]["same_name_candidates"].is_u64(),
            "every walk reports its resolution: {reply}"
        );
    }

    // Pinned by id, the resolution is reported as settled, the way a pinned
    // `find_references` reports it, so the verdict neither calls the
    // resolution unreported nor calls it ambiguous.
    let annotated = body(&crate::finalize_with_envelope(
        handle_trace_data_flow(
            &args(&[
                ("focal", serde_json::json!(fixture.real.id.to_string())),
                ("direction", serde_json::json!("callers")),
            ]),
            &fixture.store,
        )
        .unwrap(),
        ready_envelope(),
        "trace_data_flow",
    ));
    let resolution = &annotated["focal_resolution"];
    assert_eq!(resolution["addressed_by"], "entity_id");
    assert_eq!(resolution["other_candidates"], serde_json::json!([]));
    let factor = annotated["_kin"]["verdict"]["limiting_factor"]
        .as_str()
        .unwrap_or_default();
    assert!(!factor.contains("focal_resolution"), "{factor}");
}

#[test]
fn a_path_end_two_modules_define_is_refused_and_a_prototype_takes_its_definition() {
    let fixture = schedulers();
    let reply = body(
        &handle_trace_path(
            &args(&[
                ("from", serde_json::json!("handleTimeout")),
                ("to", serde_json::json!("advanceTimers")),
            ]),
            &fixture.store,
        )
        .unwrap(),
    );
    assert_eq!(reply["ambiguous_focal"], true, "{reply}");
    assert_eq!(reply["end"], "to", "{reply}");
    assert_eq!(reply["candidate_count"], 2, "{reply}");

    let routed = body(
        &handle_trace_path(
            &args(&[
                ("from", serde_json::json!("handleTimeout")),
                ("to", serde_json::json!("Scheduler.advanceTimers")),
            ]),
            &fixture.store,
        )
        .unwrap(),
    );
    assert!(routed.get("ambiguous_focal").is_none(), "{routed}");
    assert_eq!(
        routed["to"]["entity_id"],
        fixture.real.id.to_string(),
        "{routed}"
    );

    let c = LanguageId::C;
    let mut prototype = function(c, "buffer_grow", "src/buffer.h");
    prototype.signature = "int buffer_grow(buffer *b, size_t n);".to_string();
    let mut definition = function(c, "buffer_grow", "src/buffer.c");
    definition.signature = "int buffer_grow(buffer *b, size_t n)".to_string();
    let caller = function(c, "buffer_append", "src/buffer.c");
    let store = store_of(
        &[&prototype, &definition, &caller],
        &[calls(&caller, &definition)],
    );
    let reply = body(
        &handle_trace_path(
            &args(&[
                ("from", serde_json::json!("buffer_append")),
                ("to", serde_json::json!("buffer_grow")),
            ]),
            &store,
        )
        .unwrap(),
    );
    assert!(reply.get("ambiguous_focal").is_none(), "{reply}");
    assert_eq!(
        reply["to"]["entity_id"],
        definition.id.to_string(),
        "{reply}"
    );
}
