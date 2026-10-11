// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A pass that asks its questions concurrently proves what the sequential
//! pass proves from the same answers, whatever order they arrive in, and
//! whatever load does to them.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use crate::enrichment::{EntityIndex, EntityRef};
use crate::file_enrichment::{enrich_file_definitions, FileEnrichmentResult};
use crate::lifecycle::LspServer;
use kin_model::EntityId;

const PEER: &str = include_str!("concurrency_test_peer.py");
const DEFINITION: &str = "textDocument/definition";
const TYPES: &str = "textDocument/typeDefinition";

/// Four callers in `source.py`, each making eight calls at columns no other
/// call shares, into four functions in `lib.py`. The definition answers mix
/// every kind the fold reads: a declaration in the graph, no answer, a
/// declaration outside the repository, a server error, a binding in the
/// caller, and an alias hop into a file only its binding holds.
struct Fixture {
    root: PathBuf,
    text: String,
    lib: String,
    alias: String,
    index: EntityIndex,
    callers: Vec<EntityRef>,
    responses: Value,
}

fn entity(name: &str, file: &str, start: u32, end: u32, name_col: u32) -> EntityRef {
    EntityRef {
        id: EntityId::new(),
        name: name.into(),
        file_path: file.into(),
        start_line: start,
        start_col: 0,
        end_line: end,
        name_line: start,
        name_col,
        declares_name: true,
        declares_only: false,
        kind: kin_model::EntityKind::Function,
    }
}

fn range(line: u32, start: u32, end: u32) -> Value {
    json!({"start": {"line": line, "character": start}, "end": {"line": line, "character": end}})
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("kin-lsp-concurrency-{}", EntityId::new()));
        std::fs::create_dir_all(&root).unwrap();
        let uri = crate::protocol::path_to_uri(&root.join("source.py"));
        let lib_uri = crate::protocol::path_to_uri(&root.join("lib.py"));
        let alias_uri = crate::protocol::path_to_uri(&root.join("alias.py"));
        let lib =
            "def f0():\n    pass\ndef f1():\n    pass\ndef f2():\n    pass\ndef f3():\n    pass\n";
        let alias = "f = lambda: 0\n";
        let mut text = String::new();
        let mut entities = Vec::new();
        let mut callers = Vec::new();
        let mut responses = json!({});
        let mut line = 0u32;
        for caller in 0..4u32 {
            let start = line;
            text.push_str(&format!(
                "{}def run{caller}():\n",
                " ".repeat(caller as usize)
            ));
            line += 1;
            for call in 0..8u32 {
                let col = 4 + caller * 8 + call;
                let target = (caller + call) % 4;
                text.push_str(&format!("{}f{target}(x{call})\n", " ".repeat(col as usize)));
                line += 1;
                let answer = match col % 7 {
                    0 | 4 => {
                        json!({"result": [{"uri": lib_uri, "range": range(target * 2, 4, 6)}]})
                    }
                    1 => json!({"result": null}),
                    2 => {
                        json!({"result": [{"uri": "file:///opt/python/lib/os.py", "range": range(10, 4, 8)}]})
                    }
                    3 => json!({"error": {"code": -32603, "message": "internal error"}}),
                    5 => json!({"result": [{"uri": uri, "range": range(start, 0, 3)}]}),
                    _ => json!({"result": [{"uri": alias_uri, "range": range(0, 0, 1)}]}),
                };
                responses[format!("{DEFINITION}@{uri}#{col}")] = answer;
            }
            let caller_ref = entity(
                &format!("run{caller}"),
                "source.py",
                start,
                line - 1,
                caller + 4,
            );
            responses[format!("textDocument/prepareCallHierarchy@{uri}#{}", caller + 4)] = json!({
                "result": [{
                    "name": caller_ref.name, "kind": 12, "uri": uri,
                    "range": range(start, 0, caller + 11),
                    "selectionRange": range(start, caller + 4, caller + 8),
                }]
            });
            entities.push(caller_ref.clone());
            callers.push(caller_ref);
        }
        for target in 0..4u32 {
            entities.push(entity(
                &format!("f{target}"),
                "lib.py",
                target * 2,
                target * 2 + 1,
                4,
            ));
        }
        // The alias hop's second question, asked in `alias.py`.
        responses[format!("{DEFINITION}@{alias_uri}#0")] =
            json!({"result": [{"uri": lib_uri, "range": range(0, 4, 6)}]});
        responses["callHierarchy/outgoingCalls"] = json!({"result": [{
            "to": {"name": "f0", "kind": 12, "uri": lib_uri, "range": range(0, 0, 8), "selectionRange": range(0, 4, 6)},
            "fromRanges": [range(1, 4, 6)],
        }]});
        let index = EntityIndex::new(entities, &root);
        Self {
            root,
            text,
            lib: lib.into(),
            alias: alias.into(),
            index,
            callers,
            responses,
        }
    }

    fn uri(&self) -> String {
        crate::protocol::path_to_uri(&self.root.join("source.py"))
    }

    fn document(&self, file: &str) -> Option<String> {
        match file {
            "source.py" => Some(self.text.clone()),
            "lib.py" => Some(self.lib.clone()),
            "alias.py" => Some(self.alias.clone()),
            _ => None,
        }
    }

    fn server(&self, options: Value, ceiling: usize) -> LspServer {
        let mut responses = self.responses.clone();
        responses["__options__"] = options;
        let server = LspServer::scripted_for_tests(PEER, responses);
        server.in_flight().set_ceiling(ceiling);
        server
    }

    async fn pass(&self, server: &LspServer) -> FileEnrichmentResult {
        let provider = |file: &str| self.document(file);
        enrich_file_definitions(
            server,
            &self.root.join("source.py"),
            &self.text,
            &self.index,
            &self.root,
            Some(&provider),
        )
        .await
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// What the peer saw: every message, every cancellation, and the most
/// questions it held at once.
async fn peer_seen(server: &LspServer) -> Value {
    server
        .client
        .request("test/seen", Value::Null)
        .await
        .unwrap()
}

/// Every definition asked in `document`, by column, in asking order.
fn definitions_at(seen: &Value, document: &str) -> Vec<u64> {
    seen["seen"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| {
            message["method"] == DEFINITION && message["params"]["textDocument"]["uri"] == document
        })
        .map(|message| message["params"]["position"]["character"].as_u64().unwrap())
        .collect()
}

/// Everything a pass proved and counted that its caller records is the same.
#[track_caller]
fn assert_same(answer: &FileEnrichmentResult, expected: &FileEnrichmentResult, case: &str) {
    if answer.relations != expected.relations {
        let only = |a: &[kin_model::Relation], b: &[kin_model::Relation]| {
            a.iter()
                .filter(|relation| !b.contains(relation))
                .map(|relation| {
                    format!(
                        "{:?} {:?}",
                        relation.kind,
                        relation
                            .evidence
                            .iter()
                            .map(|e| e.source_span.as_ref().map(|s| (s.start_line, s.start_col)))
                            .collect::<Vec<_>>()
                    )
                })
                .collect::<Vec<_>>()
        };
        panic!(
            "relations differ ({case}): {} vs {}; only in the answer {:?}; only expected {:?}",
            answer.relations.len(),
            expected.relations.len(),
            only(&answer.relations, &expected.relations),
            only(&expected.relations, &answer.relations)
        );
    }
    assert_eq!(
        answer.site_answers, expected.site_answers,
        "site answers: {case}"
    );
    assert_eq!(
        answer.unproven_sites, expected.unproven_sites,
        "unproven sites: {case}"
    );
    assert_eq!(answer.unprovable, expected.unprovable, "unprovable: {case}");
    assert_eq!(
        answer.external_names, expected.external_names,
        "external names: {case}"
    );
    assert_eq!(
        answer.first_failure, expected.first_failure,
        "first failure: {case}"
    );
    let counts = |result: &FileEnrichmentResult| {
        [
            result.definitions_resolved,
            result.positions_queried,
            result.failed_queries,
            result.transient_queries,
            result.retryable_protocol_queries,
            result.refused_queries,
            result.declined_queries,
            result.unproven_calls,
            result.definition_queries,
            result.definition_queries_saved,
            result.call_site_queries,
            result.alias_hops,
            result.overload_answers,
        ]
    };
    assert_eq!(counts(answer), counts(expected), "counts: {case}");
    assert_eq!(
        (answer.call_hierarchy_complete, answer.stopped_early),
        (expected.call_hierarchy_complete, expected.stopped_early),
        "completion: {case}"
    );
}

/// The same answers, delivered in a scrambled order to eight questions at a
/// time, prove exactly what one question at a time proves.
#[tokio::test]
async fn a_pass_proves_the_same_whatever_order_its_answers_arrive_in() {
    let fixture = Fixture::new();
    let sequential = fixture.server(json!({}), 1);
    let expected = fixture.pass(&sequential).await;
    let seen_sequential = peer_seen(&sequential).await;
    sequential.shutdown().await.unwrap();
    assert_eq!(seen_sequential["most_pending"], 1);
    assert!(expected.failed_queries > 0, "{expected:?}");
    assert!(expected.alias_hops > 0, "{expected:?}");
    assert!(!expected.site_answers.is_empty(), "{expected:?}");
    assert!(!expected.unproven_sites.is_empty(), "{expected:?}");
    assert!(
        expected
            .relations
            .iter()
            .any(|relation| relation.kind == kin_model::RelationKind::Calls),
        "{:?}",
        expected.unprovable
    );

    for seed in [1, 2, 3] {
        let concurrent = fixture.server(json!({"shuffle_ms": 15, "seed": seed}), 8);
        let answer = fixture.pass(&concurrent).await;
        let seen = peer_seen(&concurrent).await;
        concurrent.shutdown().await.unwrap();
        assert!(
            seen["most_pending"].as_u64().unwrap() > 1,
            "the questions went out together: {}",
            seen["most_pending"]
        );
        assert_same(&answer, &expected, &format!("seed {seed}"));
    }
}

/// A server that turns questions away with ServerCancelled whenever another
/// is waiting gets fewer at once, and every question it turned away is asked
/// again alone: the pass fails nothing the sequential pass did not.
#[tokio::test]
async fn a_throttling_server_is_backed_off_and_fails_no_question_for_it() {
    let fixture = Fixture::new();
    let sequential = fixture.server(json!({}), 1);
    let expected = fixture.pass(&sequential).await;
    sequential.shutdown().await.unwrap();

    let throttled = fixture.server(json!({"throttle": 1, "shuffle_ms": 5}), 8);
    let answer = fixture.pass(&throttled).await;
    assert!(
        throttled.in_flight().current() < 8,
        "the window backed off: {}",
        throttled.in_flight().current()
    );
    throttled.shutdown().await.unwrap();
    assert_same(&answer, &expected, "");
    assert_eq!(answer.retryable_protocol_queries, 0, "{answer:?}");
}

/// A server that answers slower than the pass's two-second bound whenever it
/// holds several questions times none out when asked one at a time, so the
/// concurrent pass records no timeout the sequential pass would not.
#[tokio::test]
async fn a_timeout_under_load_is_asked_again_alone_before_it_counts() {
    let fixture = Fixture::new();
    let sequential = fixture.server(json!({}), 1);
    let expected = fixture.pass(&sequential).await;
    let seen_sequential = peer_seen(&sequential).await;
    sequential.shutdown().await.unwrap();
    assert_eq!(expected.transient_queries, 0, "{expected:?}");

    let slow = fixture.server(
        json!({"busy_at": 3, "busy_delay_ms": 2500, "shuffle_ms": 30, "controlled_clock": true}),
        8,
    );
    let answer =
        crate::enrichment_integrity_tests::with_held_request_clock(&slow, fixture.pass(&slow))
            .await;
    let asked = definitions_at(&peer_seen(&slow).await, &fixture.uri()).len();
    slow.shutdown().await.unwrap();
    assert_same(&answer, &expected, "");
    assert_eq!(answer.transient_queries, 0, "{answer:?}");
    assert!(
        asked > definitions_at(&seen_sequential, &fixture.uri()).len(),
        "questions that timed out beside others were asked again"
    );
}

/// A question that went out before the fold opened a document is asked again
/// once it is open, as the sequential pass would have asked it.
#[tokio::test]
async fn an_answer_given_before_a_document_opened_is_asked_again() {
    let fixture = Fixture::new();
    let sequential = fixture.server(json!({}), 1);
    let expected = fixture.pass(&sequential).await;
    let asked_sequentially = definitions_at(&peer_seen(&sequential).await, &fixture.uri());
    sequential.shutdown().await.unwrap();

    let concurrent = fixture.server(json!({"shuffle_ms": 5}), 8);
    let answer = fixture.pass(&concurrent).await;
    let seen = peer_seen(&concurrent).await;
    concurrent.shutdown().await.unwrap();
    assert_same(&answer, &expected, "");

    let messages = seen["seen"].as_array().unwrap();
    let opened_at = messages
        .iter()
        .position(|message| message["method"] == "textDocument/didOpen")
        .expect("the alias hop opened alias.py");
    let asked_again = definitions_at(&seen, &fixture.uri()).len() - asked_sequentially.len();
    assert!(
        asked_again > 0,
        "questions in flight at the open were asked again"
    );
    // Every question the fold read after the open went out after it.
    let asked_before_open: Vec<u64> = messages[..opened_at]
        .iter()
        .filter(|message| message["method"] == DEFINITION)
        .map(|message| message["params"]["position"]["character"].as_u64().unwrap())
        .collect();
    let asked_after_open: Vec<u64> = messages[opened_at..]
        .iter()
        .filter(|message| message["method"] == DEFINITION)
        .map(|message| message["params"]["position"]["character"].as_u64().unwrap())
        .collect();
    assert!(
        asked_before_open
            .iter()
            .any(|column| asked_after_open.contains(column)),
        "before {asked_before_open:?}, after {asked_after_open:?}"
    );
}

/// Dropping a pass, as its file budget does, cancels every question it still
/// had in flight with the server.
#[tokio::test]
async fn dropping_a_pass_cancels_every_question_in_flight() {
    let fixture = Fixture::new();
    let mut responses = fixture.responses.clone();
    for (key, answer) in responses.as_object_mut().unwrap().iter_mut() {
        if key.starts_with(DEFINITION) {
            *answer = json!({"hold": true});
        }
    }
    responses["__options__"] = json!({});
    let server = LspServer::scripted_for_tests(PEER, responses);
    server.in_flight().set_ceiling(8);
    let provider = |file: &str| fixture.document(file);
    let source = fixture.root.join("source.py");
    let pass = enrich_file_definitions(
        &server,
        &source,
        &fixture.text,
        &fixture.index,
        &fixture.root,
        Some(&provider),
    );
    assert!(tokio::time::timeout(Duration::from_millis(500), pass)
        .await
        .is_err());
    let seen = peer_seen(&server).await;
    server.shutdown().await.unwrap();
    let sent: Vec<u64> = seen["seen"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["method"] == DEFINITION)
        .map(|message| message["id"].as_u64().unwrap())
        .collect();
    let cancelled: Vec<u64> = seen["cancelled"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_u64().unwrap())
        .collect();
    assert_eq!(sent.len(), 8, "the window's worth went out: {sent:?}");
    assert!(
        sent.iter().all(|id| cancelled.contains(id)),
        "sent {sent:?}, cancelled {cancelled:?}"
    );
}

/// The uses-type arm, like the definitions pass, proves the same from
/// scrambled answers to eight questions at a time.
#[tokio::test]
async fn the_uses_type_arm_proves_the_same_whatever_order_its_answers_arrive_in() {
    let fixture = Fixture::new();
    let uri = fixture.uri();
    let lib_uri = crate::protocol::path_to_uri(&fixture.root.join("lib.py"));
    let mut responses = fixture.responses.clone();
    for col in 0..40u32 {
        responses[format!("{TYPES}@{uri}#{col}")] = match col % 4 {
            0 | 1 => json!({"result": [{"uri": lib_uri, "range": range((col % 4) * 2, 4, 6)}]}),
            2 => json!({"result": null}),
            _ => json!({"result": [{"uri": lib_uri, "range": range(6, 4, 6)}]}),
        };
    }
    let run = |options: Value, ceiling: usize| {
        let mut responses = responses.clone();
        responses["__options__"] = options;
        let fixture = &fixture;
        async move {
            let server = LspServer::scripted_for_tests(PEER, responses);
            server.in_flight().set_ceiling(ceiling);
            let provider = |file: &str| fixture.document(file);
            let mut relations = Vec::new();
            for caller in &fixture.callers {
                relations.push(
                    crate::enrichment::enrich_entity_uses_type(
                        &server,
                        caller,
                        &fixture.index,
                        &fixture.root,
                        Some(&provider),
                    )
                    .await
                    .map_err(|error| error.to_string()),
                );
            }
            let seen = peer_seen(&server).await;
            server.shutdown().await.unwrap();
            (relations, seen["most_pending"].as_u64().unwrap())
        }
    };
    let (expected, one) = run(json!({}), 1).await;
    assert_eq!(one, 1);
    assert!(expected.iter().any(|relations| relations
        .as_ref()
        .is_ok_and(|relations| !relations.is_empty())));
    for seed in [4, 5] {
        let (answer, most) = run(json!({"shuffle_ms": 10, "seed": seed}), 8).await;
        assert!(most > 1, "{most}");
        assert_eq!(answer, expected, "seed {seed}");
    }
}
