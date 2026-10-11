// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Every retrieval arm, end to end through the locate pipeline, on one
//! fixture graph: the default arm, cards and graph without embeddings, and
//! each with the scorer on.

use super::cards::tests::{card_entity, edge};
use super::scorer::{test_scorer, CandidateScorer, ScorerUnavailable, ScoringCandidate};
use super::*;
use crate::retrieval_arm::{ARM_ENV, SCORER_ENV};
use kin_core::test_env::EnvVarGuard;

const QUERY: &str = "how are tokens topped up over time";

/// A small store where the answer to [`QUERY`] is reachable by its doc words
/// and its neighbours only through the graph.
fn arm_fixture() -> kin_db::InMemoryGraph {
    let graph = kin_db::InMemoryGraph::new();
    let owner = card_entity(
        "RateLimiter",
        "net/throttle.py",
        Some("Guards a socket budget."),
    );
    let target = card_entity(
        "refill",
        "net/throttle.py",
        Some("Top up the bucket with tokens as time passes."),
    );
    let helper = card_entity(
        "monotonic_clock",
        "net/clock.py",
        Some("Seconds since boot."),
    );
    let page = card_entity("render_page", "web/view.py", Some("Draw the HTML page."));
    let form = card_entity("submit_form", "web/form.py", Some("Post the form fields."));
    for entity in [&owner, &target, &helper, &page, &form] {
        graph.upsert_entity(entity).unwrap();
    }
    graph
        .upsert_relation(&edge(owner.id, target.id, RelationKind::Contains))
        .unwrap();
    graph
        .upsert_relation(&edge(target.id, helper.id, RelationKind::Calls))
        .unwrap();
    graph
        .upsert_relation(&edge(page.id, form.id, RelationKind::Calls))
        .unwrap();
    graph.flush_text_index().unwrap();
    graph
}

fn locate(graph: &kin_db::InMemoryGraph, query: &str) -> LocateResult {
    run_with_graph_capture_budgeted(
        graph,
        None,
        query,
        false,
        10,
        true,
        Vec::new(),
        None,
        SnippetOptions::enabled(None),
        None,
        kin_mcp::handlers::common::EntitySourceScope::WorkspaceHead,
        LocateScope::SOURCE_ONLY,
        LocateBudget::unbounded(),
    )
    .unwrap()
}

/// The environment a test states in full: the arm, the scorer, and a model
/// cache root that holds nothing, so no arm can load a model by accident.
fn arm_env(arm: Option<&str>, scorer: Option<&str>) -> (EnvVarGuard, tempfile::TempDir) {
    let cache = tempfile::tempdir().unwrap();
    let mut guard = EnvVarGuard::unset(ARM_ENV);
    guard.apply(ARM_ENV, arm);
    guard.apply(SCORER_ENV, scorer);
    guard.apply("HF_HOME", Some(cache.path()));
    guard.apply::<_, &str>("KIN_PROFILE", None);
    guard.apply::<_, &str>("KIN_LOCATE_CROSS_ENCODER_ENABLED", None);
    guard.apply::<_, &str>("KIN_LOCATE_SCORER_MODEL", None);
    guard.apply("KIN_EMBED_BACKEND", Some("cpu"));
    (guard, cache)
}

fn names(result: &LocateResult) -> Vec<&str> {
    result.entities.iter().map(|e| e.name.as_str()).collect()
}

fn ledger(result: &LocateResult) -> Vec<(String, String)> {
    result
        .degradations
        .iter()
        .map(|entry| (entry.component.clone(), entry.reason.clone()))
        .collect()
}

/// Scores rows by name from a table; unknown names get a floor.
struct NameScorer(Vec<(&'static str, f32)>);

impl CandidateScorer for NameScorer {
    fn name(&self) -> &'static str {
        "cross-encoder"
    }

    fn score(
        &self,
        _query: &str,
        candidates: &[ScoringCandidate],
    ) -> Result<Vec<Option<f32>>, ScorerUnavailable> {
        Ok(candidates
            .iter()
            .map(|candidate| {
                Some(
                    self.0
                        .iter()
                        .find(|(name, _)| *name == candidate.name)
                        .map_or(0.01, |(_, p)| *p),
                )
            })
            .collect())
    }
}

/// An explicit `fused` arm answers as the pipeline did before the switch
/// existed, even on a store with no search vectors: it carries none of the
/// fields the other arms add, still reports the missing vector index, and never
/// touches the card index. An explicit setting is never second-guessed.
#[test]
#[serial_test::serial]
fn an_explicit_fused_arm_answers_as_it_did_before_the_switch() {
    let graph = arm_fixture();
    let fused = {
        let _env = arm_env(Some("fused"), None);
        locate(&graph, QUERY)
    };
    let wire = serde_json::to_value(&fused).unwrap();
    assert!(wire.get("retrieval_arm").is_none(), "{wire}");
    assert!(wire.get("scorer").is_none(), "{wire}");
    assert!(fused
        .entities
        .iter()
        .all(|e| e.scorer_probability.is_none()));
    assert!(
        ledger(&fused)
            .iter()
            .all(|(component, _)| component != "retrieval_arm" && component != "candidate_scorer"),
        "{:?}",
        ledger(&fused)
    );
    assert!(
        ledger(&fused)
            .iter()
            .any(|(component, _)| component == "vector_index"),
        "the fused arm still reports the store's missing vector index: {:?}",
        ledger(&fused)
    );
    assert_ne!(
        fused.semantic_coverage.as_ref().unwrap().embedding_state,
        EmbeddingState::NotUsed
    );
    assert!(
        super::cards::card_index_is_held_for_test(&graph).is_none(),
        "the fused arm built entity cards"
    );
}

/// With nothing naming an arm, a query with no usable search vectors (a store
/// never embedded, or a build without embeddings, which is how this runs under
/// `--no-default-features`) ranks on entity cards and the graph, names the
/// cards arm on its answer, and reports no index gap: it ranks exactly as the
/// explicit `cards` arm does, its coverage says vectors were not used, and the
/// MCP reply built from it carries no coverage shortfall.
#[test]
#[serial_test::serial]
fn a_query_without_usable_vectors_serves_the_cards_arm_and_names_it() {
    let graph = arm_fixture();
    let automatic = {
        let _env = arm_env(None, None);
        locate(&graph, QUERY)
    };
    let explicit = {
        let _env = arm_env(Some("cards"), None);
        locate(&graph, QUERY)
    };
    assert_eq!(automatic.retrieval_arm.as_deref(), Some("cards"));
    assert_eq!(names(&automatic), names(&explicit), "one arm, one ranking");
    assert!(
        names(&automatic).contains(&"refill"),
        "the described entity is on the answer: {:?}",
        names(&automatic)
    );
    assert!(
        ledger(&automatic)
            .iter()
            .all(|(component, _)| component != "vector_index"),
        "no vector gap on an answer that read no vector: {:?}",
        ledger(&automatic)
    );
    let coverage = automatic.semantic_coverage.as_ref().unwrap();
    assert_eq!(coverage.embedding_state, EmbeddingState::NotUsed);
    assert!(
        coverage
            .limited_by
            .iter()
            .all(|limit| limit != COVERAGE_LIMIT_VECTOR_SUPPORT_DISABLED
                && limit != COVERAGE_LIMIT_VECTOR_INDEX_ABSENT
                && limit != COVERAGE_LIMIT_EMBEDDINGS_INCOMPLETE),
        "{:?}",
        coverage.limited_by
    );

    let compact = crate::commands::locate_compact::project_for_mcp(&automatic);
    assert_eq!(
        compact.ranked_by,
        crate::commands::locate_compact::NO_VECTORS_CLAUSE
    );
    assert_eq!(compact.retrieval_arm.as_deref(), Some("cards"));

    // The reply an agent reads, finalized exactly as the MCP server does.
    let mut payload = match serde_json::to_value(&compact).unwrap() {
        serde_json::Value::Object(map) => map,
        other => panic!("compact locate is an object: {other}"),
    };
    payload.insert("query".into(), serde_json::json!(QUERY));
    payload.insert("granularity".into(), serde_json::json!("entity"));
    payload.insert("routing".into(), serde_json::json!("fused-v1"));
    payload.insert("surface".into(), serde_json::json!("compact"));
    let served = kin_mcp::envelope::finalize(
        kin_mcp::ToolCallResult::text(serde_json::Value::Object(payload).to_string()),
        kin_mcp::Envelope::daemon().with_health(&serde_json::json!({
            "initialized": true,
            "graph_loaded": true,
            "reconciliation_status": "clean",
        })),
        "semantic_locate",
    );
    let kin_mcp::ContentBlock::Text { text } = served.content.first().unwrap();
    let machine: serde_json::Map<String, serde_json::Value> = serde_json::from_str(text).unwrap();
    let envelope = &machine["_kin"];
    assert_eq!(envelope["completeness"]["substrate"], "graph", "{envelope}");
    assert_eq!(
        envelope["completeness"]["classes"]["embeddings"], "not_used",
        "{envelope}"
    );
    let factor = envelope["verdict"]["limiting_factor"]
        .as_str()
        .unwrap_or_default();
    // The fixture's entities carry no source bodies, which is a real gap in
    // what was ranked and stays reported. Missing vectors are not one.
    for vector_gap in [
        "coverage_unknown",
        "coverage_absent",
        "coverage_partial",
        "embeddings_",
    ] {
        assert!(
            !factor.contains(vector_gap),
            "missing vectors are not a gap in this answer: {factor}"
        );
    }
    let plain = kin_mcp::plain_reply::plain_answer(machine, "semantic_locate");
    let meaning = plain["_kin"]["meaning"].as_str().unwrap_or_default();
    assert!(
        !meaning.contains("search index") && !meaning.contains("search vectors yet"),
        "{meaning}"
    );
}

/// The cards arm answers the description from entity cards and the graph,
/// reads no embedding, says so as a note rather than a gap, and names itself.
#[test]
#[serial_test::serial]
fn the_cards_arm_answers_a_description_without_embeddings() {
    let graph = arm_fixture();
    let _env = arm_env(Some("cards"), None);
    let result = locate(&graph, QUERY);

    assert_eq!(result.retrieval_arm.as_deref(), Some("cards"));
    assert!(
        names(&result).contains(&"refill"),
        "the described entity is on the answer: {:?}",
        names(&result)
    );
    let entries = ledger(&result);
    assert!(
        entries
            .iter()
            .all(|(component, _)| component != "vector_index"),
        "an arm that reads no embedding reports no vector gap: {entries:?}"
    );
    let note = result
        .degradations
        .iter()
        .find(|entry| entry.component == "retrieval_arm")
        .expect("the arm says it read no embedding");
    assert_eq!(note.reason, "embeddings_not_used");
    assert_eq!(note.severity(), DegradationSeverity::Note);
    assert!(note.detail.contains("card hits"), "{}", note.detail);
    assert!(result.scorer.is_none(), "no scorer on this arm");
    assert!(
        super::cards::card_index_is_held_for_test(&graph).is_some(),
        "the cards arm holds its card index for the next query"
    );

    let compact = crate::commands::locate_compact::project_for_mcp(&result);
    assert_eq!(
        compact.ranked_by,
        crate::commands::locate_compact::NO_VECTORS_CLAUSE
    );
    assert_eq!(compact.retrieval_arm.as_deref(), Some("cards"));
}

/// With the scorer on, the leading rows are reordered by calibrated
/// probability, each carries its probability, and the answer reports what the
/// scorer did at its top level.
#[test]
#[serial_test::serial]
fn the_cards_arm_with_the_scorer_reorders_by_calibrated_probability() {
    let graph = arm_fixture();
    let _env = arm_env(Some("cards+scorer"), None);
    let unscored = {
        let _control = arm_env(Some("cards"), None);
        locate(&graph, QUERY)
    };
    assert!(unscored.entities.len() >= 2, "{:?}", names(&unscored));
    let last = unscored.entities.last().unwrap().name.clone();
    let last: &'static str = Box::leak(last.into_boxed_str());

    test_scorer::set(Box::new(NameScorer(vec![(last, 0.93)])));
    let result = locate(&graph, QUERY);

    assert_eq!(result.retrieval_arm.as_deref(), Some("cards+cross-encoder"));
    assert_eq!(
        result.entities.first().map(|e| e.name.as_str()),
        Some(last),
        "the row the scorer rated highest leads: {:?}",
        names(&result)
    );
    assert_eq!(result.entities[0].scorer_probability, Some(0.93));
    let report = result
        .scorer
        .as_ref()
        .expect("the answer reports its scorer");
    assert_eq!(report.top_probability, Some(0.93));
    assert!(!report.abstained);
    assert_eq!(report.scored, result.entities.len().min(40));
}

/// When the scorer rates every row below its threshold, the answer keeps its
/// rows and its verdict lists the gap: `_kin.verdict` is inconclusive with the
/// `none_of_these` factor, and the scorer's report rides the verdict.
#[test]
#[serial_test::serial]
fn a_scorer_that_rejects_every_row_abstains_through_the_verdict() {
    let graph = arm_fixture();
    let _env = arm_env(Some("fused+scorer"), None);
    test_scorer::set(Box::new(NameScorer(Vec::new())));
    let result = locate(&graph, QUERY);

    let report = result
        .scorer
        .as_ref()
        .expect("the answer reports its scorer");
    assert!(report.abstained, "{report:?}");
    assert!(!result.entities.is_empty(), "abstention keeps the rows");

    let payload =
        serde_json::to_value(crate::commands::locate_compact::project_for_mcp(&result)).unwrap();
    let verdict = kin_mcp::Verdict::compute(
        "semantic_locate",
        &payload,
        &kin_mcp::Envelope::daemon(),
        None,
    )
    .expect("a retrieval answer carries a verdict")
    .to_value();
    assert_eq!(verdict["state"], "inconclusive", "{verdict}");
    assert!(
        verdict["limiting_factor"]
            .as_str()
            .unwrap_or_default()
            .contains("none_of_these"),
        "{verdict}"
    );
    assert_eq!(verdict["scorer"]["abstained"], true, "{verdict}");
}

/// The fused arm with the scorer on and no reranker in the model cache answers
/// with the fused ranking and a refusing ledger entry naming the missing model.
/// It downloads nothing: the cache root is an empty directory.
#[test]
#[serial_test::serial]
fn the_scorer_without_a_cached_model_reports_the_gap_and_keeps_the_ranking() {
    let graph = arm_fixture();
    let fused = {
        let _env = arm_env(Some("fused"), None);
        locate(&graph, QUERY)
    };
    let _env = arm_env(Some("fused+scorer"), Some("cross-encoder"));
    let result = locate(&graph, QUERY);

    assert_eq!(
        names(&result),
        names(&fused),
        "no scorer ran, so no row moved"
    );
    let report = result
        .scorer
        .as_ref()
        .expect("the answer reports its scorer");
    assert!(
        !report.abstained,
        "a scorer that did not run cannot abstain"
    );
    let expected = if cfg!(feature = "embeddings") {
        "model_not_cached"
    } else {
        "feature_disabled"
    };
    assert_eq!(report.unavailable.as_deref(), Some(expected));
    assert!(
        ledger(&result).contains(&("candidate_scorer".to_string(), expected.to_string())),
        "{:?}",
        ledger(&result)
    );
}

/// The decision-engine slot is selectable, and a model missing from the local
/// cache is reported by name and never fetched: the rows keep the cards order.
#[test]
#[serial_test::serial]
fn the_decision_engine_slot_names_a_model_it_cannot_load() {
    let graph = arm_fixture();
    let plain = {
        let _env = arm_env(Some("cards"), None);
        locate(&graph, QUERY)
    };
    let _env = arm_env(Some("cards+decision-engine"), None);
    let result = locate(&graph, QUERY);
    assert_eq!(
        result.retrieval_arm.as_deref(),
        Some("cards+decision-engine")
    );
    let report = result.scorer.as_ref().expect("the stage reports itself");
    let expected = if cfg!(feature = "decision") {
        "model_not_cached"
    } else {
        "not_in_build"
    };
    assert_eq!(report.unavailable.as_deref(), Some(expected), "{report:?}");
    assert_eq!(
        report.model.as_deref(),
        Some(super::scorer::DEFAULT_DECISION_SCORER_MODEL)
    );
    assert!(
        !report.abstained,
        "a scorer that did not run cannot abstain"
    );
    assert_eq!(names(&result), names(&plain));
    assert!(ledger(&result).iter().any(|(component, reason)| component
        == super::scorer::SCORER_COMPONENT
        && reason == expected));
}

/// The `none` scorer is the control: the stage runs, reports itself, and moves
/// nothing relative to the same candidates with no scorer at all.
#[test]
#[serial_test::serial]
fn the_none_scorer_is_a_control_that_changes_no_row() {
    let graph = arm_fixture();
    let plain = {
        let _env = arm_env(Some("cards"), None);
        locate(&graph, QUERY)
    };
    let _env = arm_env(Some("cards+none"), None);
    let controlled = locate(&graph, QUERY);
    assert_eq!(names(&controlled), names(&plain));
    let report = controlled.scorer.as_ref().unwrap();
    assert_eq!(report.name, "none");
    assert!(!report.abstained);
    assert!(report.top_probability.is_none());
}

/// A store where the function a description is about says so only in its body,
/// beside a class whose NAME shares a word with the description and a test that
/// repeats the description nearly word for word.
fn evidence_fixture() -> (kin_db::InMemoryGraph, [kin_model::EntityId; 3]) {
    let with_body = |mut entity: kin_model::Entity, body: &str| {
        entity.metadata.extra.insert(
            kin_parser::extract::EMBEDDING_BODY_PREVIEW_KEY.to_string(),
            serde_json::Value::String(body.to_string()),
        );
        entity
    };
    let graph = kin_db::InMemoryGraph::new();
    let answer = with_body(
        card_entity("get_signing_serializer", "src/sessions.py", None),
        "def get_signing_serializer(self, app): keys = [] if fallbacks := \
         app.config[\"SECRET_KEY_FALLBACKS\"]: keys.extend(fallbacks) keys.append(app.secret_key) \
         return URLSafeTimedSerializer(keys, salt=self.salt)",
    );
    let mut named_alike = with_body(
        card_entity(
            "DebugFilesKeyError",
            "src/debughelpers.py",
            Some("Raised from request.files during debugging."),
        ),
        "class DebugFilesKeyError(KeyError, AssertionError): pass",
    );
    named_alike.kind = EntityKind::Class;
    let mut test = with_body(
        card_entity("test_secret_key_rotation", "tests/test_basic.py", None),
        "def test_secret_key_rotation(app, client): app.config[\"SECRET_KEY_FALLBACKS\"] = [\"old\"] \
         enable secret key rotation config",
    );
    test.role = kin_model::EntityRole::Test;
    let bystander = with_body(
        card_entity(
            "render_template",
            "src/templating.py",
            Some("Render a template by name."),
        ),
        "def render_template(name, **context): return _render(app, template, context)",
    );
    for entity in [&answer, &named_alike, &test, &bystander] {
        graph.upsert_entity(entity).unwrap();
    }
    graph.flush_text_index().unwrap();
    (graph, [answer.id, named_alike.id, test.id])
}

const BODY_QUERY: &str = "enable secret key rotation with the SECRET_KEY_FALLBACKS config";

fn locate_scoped(graph: &kin_db::InMemoryGraph, query: &str, scope: LocateScope) -> LocateResult {
    run_with_graph_capture_budgeted(
        graph,
        None,
        query,
        false,
        10,
        true,
        Vec::new(),
        None,
        SnippetOptions::enabled(None).without_bodies(),
        None,
        kin_mcp::handlers::common::EntitySourceScope::WorkspaceHead,
        scope,
        LocateBudget::unbounded(),
    )
    .unwrap()
}

/// A description is ranked over entities by what they hold. The function whose
/// body carries the description's words leads, ahead of a class that merely
/// has one of them in its name, and no row is lifted for spelling a name.
#[test]
#[serial_test::serial]
fn a_description_is_ranked_over_entities_by_what_they_hold() {
    let (graph, [answer, named_alike, test]) = evidence_fixture();
    let _env = arm_env(Some("cards"), None);
    let result = locate_scoped(&graph, BODY_QUERY, LocateScope::SOURCE_ONLY);

    let ids: Vec<&str> = result
        .entities
        .iter()
        .map(|e| e.entity_id.as_str())
        .collect();
    assert_eq!(
        ids.first().copied(),
        Some(answer.to_string().as_str()),
        "the function whose body holds the words leads: {:?}",
        names(&result)
    );
    assert!(
        ids.contains(&named_alike.to_string().as_str()),
        "a row that shares one word stays on the page, lower: {:?}",
        names(&result)
    );
    assert!(
        !ids.contains(&test.to_string().as_str()),
        "test code is left out of a description's ranking unless asked for: {:?}",
        names(&result)
    );
    assert!(
        result
            .entities
            .windows(2)
            .all(|pair| pair[0].score >= pair[1].score),
        "rows are in score order, with no tier above it"
    );
    assert_eq!(result.total_ranked, result.entities.len());
    assert!(
        result.all_fallback,
        "the description spelled no entity's name"
    );

    // The same words, with tests asked for: the test that repeats them is ranked.
    let with_tests = locate_scoped(&graph, BODY_QUERY, LocateScope::WITH_TESTS);
    assert!(
        with_tests
            .entities
            .iter()
            .any(|e| e.entity_id == test.to_string()),
        "{:?}",
        names(&with_tests)
    );

    // The switch restores the projection of the file ranking.
    assert_eq!(
        result.score_composition,
        Some(ScoreComposition::EntityFirst)
    );
    let _projection = EnvVarGuard::set("KIN_LOCATE_ENTITY_FIRST", "0");
    let projected = locate_scoped(&graph, BODY_QUERY, LocateScope::SOURCE_ONLY);
    assert_ne!(
        projected.score_composition,
        Some(ScoreComposition::EntityFirst)
    );
}

/// A query that IS a name is a lookup and keeps everything about one: the
/// declaration it names leads and is reported as a name match.
#[test]
#[serial_test::serial]
fn a_name_keeps_its_lookup_whatever_a_description_gets() {
    let (graph, [_, named_alike, _]) = evidence_fixture();
    let _env = arm_env(Some("cards"), None);
    let result = locate_scoped(&graph, "DebugFilesKeyError", LocateScope::SOURCE_ONLY);
    let first = result
        .entities
        .first()
        .expect("a name finds its declaration");
    assert_eq!(first.entity_id, named_alike.to_string());
    assert_eq!(first.match_kind, Some(LocateMatchKind::Name));
    assert!(!result.all_fallback);
}

/// The same description ranks the same way on every call: identical rows, in
/// identical order, with identical scores.
#[test]
#[serial_test::serial]
fn the_same_description_gets_the_same_answer_on_every_call() {
    let (graph, _) = evidence_fixture();
    let _env = arm_env(Some("cards"), None);
    let rows = |result: &LocateResult| -> Vec<(String, u32)> {
        result
            .entities
            .iter()
            .map(|e| (e.entity_id.clone(), e.score.to_bits()))
            .collect()
    };
    let first = locate_scoped(&graph, BODY_QUERY, LocateScope::SOURCE_ONLY);
    assert!(!first.entities.is_empty());
    for _ in 0..4 {
        let again = locate_scoped(&graph, BODY_QUERY, LocateScope::SOURCE_ONLY);
        assert_eq!(rows(&again), rows(&first));
        assert_eq!(again.total_ranked, first.total_ranked);
    }
}

/// A question handed to `context` resolves to the entities `locate` ranks
/// first for the same words. Two rankings for one description is a defect: the
/// focals of a context pack are the head of the locate ranking, nothing else.
#[test]
#[serial_test::serial]
fn a_context_question_resolves_to_the_head_of_the_locate_ranking() {
    let (graph, _) = evidence_fixture();
    let _env = arm_env(Some("cards"), None);
    let located = locate_scoped(&graph, BODY_QUERY, LocateScope::SOURCE_ONLY);
    let (focals, _) = crate::commands::context::question_focals(&graph, BODY_QUERY, 5).unwrap();
    let head: Vec<&str> = located
        .entities
        .iter()
        .filter(|e| !e.entity_id.is_empty())
        .take(5)
        .map(|e| e.entity_id.as_str())
        .collect();
    let focal_ids: Vec<&str> = focals
        .iter()
        .map(|focal| focal.entity_id.as_str())
        .collect();
    assert!(!head.is_empty());
    assert_eq!(focal_ids, head);
}

/// A description asks about tests by its subject, never by a mention. The
/// three questions a model asked while looking for a session test, measured in
/// a task run, are the fixtures.
#[test]
fn a_description_asks_about_tests_by_its_subject_not_by_a_mention() {
    let nothing = |_: &str| NameHolders::default();
    for asks in [
        "test session cleared vary cookie header",
        "tests session_interface vary cookie",
        "test that session access sets vary cookie on response",
        "find the tests for the session interface",
        "unit tests of the cookie serializer",
        "spec for retry backoff",
    ] {
        assert!(description_asks_for_tests(asks, nothing), "{asks}");
    }
    for mentions in [
        "the session cookie is not sent when the session is cleared",
        "When using follow_redirects in the test client, the final state of session is correct.",
        "update test cookie handling for Werkzeug 2.3",
        "Let json.* work even without app on the stack and added tests",
        "Require that cookies are enabled in the test client for session transactions",
        "requests to http://abc.example.test are routed to the wrong host",
    ] {
        assert!(!description_asks_for_tests(mentions, nothing), "{mentions}");
    }

    // `test client` at the head of a bug report names library code.
    let flask = |name: &str| NameHolders {
        library: matches!(name, "test_client" | "test_cli_runner"),
        test_callable: name == "test_session_vary_cookie",
    };
    assert!(!description_asks_for_tests(
        "test client drops the session cookie after a redirect",
        flask
    ));
    assert!(!description_asks_for_tests(
        "remove deprecated as_tuple parameter from test_client",
        flask
    ));
    assert!(description_asks_for_tests(
        "why does test_session_vary_cookie fail on a cleared session",
        flask
    ));
    assert!(description_asks_for_tests(
        "test session cleared vary cookie header",
        flask
    ));

    // Which names are test code is the adapters' to say, by the test role
    // they give an entity, never a naming rule of one language: a camel-case
    // test helper only test code declares asks about tests, a library
    // identifier does not, and `testClient` at the head is library code as
    // `test_client` is.
    let react = |name: &str| NameHolders {
        library: matches!(name, "testClient" | "createRoot"),
        test_callable: matches!(name, "renderIntoDocumentTwice"),
    };
    assert!(description_asks_for_tests(
        "why does renderIntoDocumentTwice leave a detached node",
        react
    ));
    assert!(!description_asks_for_tests(
        "createRoot throws when the container is a comment node",
        react
    ));
    assert!(!description_asks_for_tests(
        "test client loses the root after an error",
        react
    ));

    // What the caller said about tests stands; the question decides only
    // when the caller said nothing.
    let question = "tests session_interface vary cookie";
    assert_eq!(
        LocateScope::for_description(Some(false), question, nothing),
        (LocateScope::SOURCE_ONLY, false)
    );
    assert_eq!(
        LocateScope::for_description(Some(true), "session cookie", nothing),
        (LocateScope::WITH_TESTS, false)
    );
    assert_eq!(
        LocateScope::for_description(None, question, nothing),
        (LocateScope::WITH_TESTS, true)
    );
    assert_eq!(
        LocateScope::for_description(None, "session cookie", nothing),
        (LocateScope::SOURCE_ONLY, false)
    );
}

/// What declares a name is read from the graph by the last segment of an
/// entity's name: library code, or a test function.
#[test]
fn the_graph_says_whether_a_name_is_library_code_or_a_test() {
    let (graph, _) = evidence_fixture();
    let mut method = card_entity("Flask.test_client", "src/app.py", None);
    method.kind = EntityKind::Method;
    graph.upsert_entity(&method).unwrap();
    graph.flush_text_index().unwrap();
    assert_eq!(
        name_holders(&graph, "test_client"),
        NameHolders {
            library: true,
            test_callable: false
        }
    );
    assert_eq!(
        name_holders(&graph, "test_secret_key_rotation"),
        NameHolders {
            library: false,
            test_callable: true
        }
    );
    assert_eq!(name_holders(&graph, "test_nothing"), NameHolders::default());
}
