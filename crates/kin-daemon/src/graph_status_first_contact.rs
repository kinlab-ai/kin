// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! First contact with `kin_graph_status`, through the surface an agent calls.
//!
//! The status sampler takes `DaemonState::embedding_work` so it can read every
//! counter under one fence, the background worker holds that lock across a whole
//! batch, and the sampler's bounded budget is 75 ms. A call landing inside a
//! batch therefore spends every attempt without sampling and must answer from
//! the settled cache. These tests grade what it answers on a daemon that has
//! never completed a live sample.
//!
//! They drive `POST /mcp/tools/call` through [`crate::api::router`], because
//! what matters is what an MCP client receives rather than what an internal
//! helper returns. They are a separate module from `api.rs`'s own tests on
//! purpose: the behaviour under test is FIRST contact, so the state must never
//! have answered a status call before the one being graded, and a fixture shared
//! with tests that call status first would seed the cache these depend on being
//! empty.
//!
//! Every reply passes the production public-status forwarding validator before
//! its semantic fields are read. That checks the published counters against
//! their cross-counter invariants without reconstructing private inventory:
//! indexed at most total, pending at least the uncovered count, index keys at
//! least indexed, and a staleness disclosure present exactly when the sampling
//! says the reading was replayed.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use kin_db::EntityStore;
use tower::ServiceExt;

use crate::state::DaemonState;

/// Point registry authority at the test process's one scratch registry instead
/// of the developer's real `~/.kin`.
fn install_test_registry_override() {
    kin_core::test_env::install_test_registry();
}

/// The daemon an agent meets: opened over a real store, and never yet asked a
/// status question.
///
/// The tempdir is returned rather than dropped, because dropping it deletes the
/// store out from under the daemon still holding it open.
fn daemon_at_first_contact() -> (tempfile::TempDir, Arc<DaemonState>) {
    install_test_registry_override();
    let dir = tempfile::tempdir().unwrap();
    let layout = kin_core::init(dir.path()).unwrap().layout;
    let state = Arc::new(DaemonState::open(layout).unwrap());
    state
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    (dir, state)
}

fn test_entity(name: &str, path: &str) -> kin_model::Entity {
    kin_model::Entity {
        id: kin_model::EntityId::new(),
        kind: kin_model::EntityKind::Function,
        name: name.to_string(),
        language: kin_model::LanguageId::Python,
        fingerprint: kin_model::SemanticFingerprint {
            algorithm: kin_model::FingerprintAlgorithm::V1TreeSitter,
            ast_hash: kin_model::Hash256::from_bytes([0x01; 32]),
            signature_hash: kin_model::Hash256::from_bytes([0x02; 32]),
            behavior_hash: kin_model::Hash256::from_bytes([0x03; 32]),
            equivalence_hash: kin_model::Hash256::from_bytes([0; 32]),
            stability_score: 1.0,
        },
        file_origin: Some(kin_model::FilePathId::new(path)),
        span: Some(kin_model::SourceSpan {
            file: kin_model::FilePathId::new(path),
            start_byte: 0,
            end_byte: 0,
            start_line: 1,
            start_col: 1,
            end_line: 1,
            end_col: 20,
        }),
        signature: format!("def {name}()"),
        visibility: kin_model::Visibility::Public,
        role: kin_model::EntityRole::Source,
        doc_summary: None,
        metadata: Default::default(),
        lineage_parent: None,
        created_in: None,
        superseded_by: None,
    }
}

/// Put entities into the live query graph, the way admission does.
fn admit_two_entities(state: &Arc<DaemonState>) {
    state
        .graph
        .upsert_entity(&test_entity("first_contact_alpha", "src/alpha.py"))
        .unwrap();
    state
        .graph
        .upsert_entity(&test_entity("first_contact_beta", "src/beta.py"))
        .unwrap();
}

/// Call `kin_graph_status` the way an MCP client does.
async fn graph_status_over_mcp(state: Arc<DaemonState>) -> kin_mcp::ToolCallResult {
    let response = crate::api::router(state)
        .oneshot(
            Request::post("/mcp/tools/call")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "name": "kin_graph_status", "arguments": {} }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn result_text(result: &kin_mcp::ToolCallResult) -> String {
    match result.content.first().unwrap() {
        kin_mcp::ContentBlock::Text { text } => text.clone(),
    }
}

/// Validate the actual public reply through the stdio forwarding boundary.
fn parse_report(result: &kin_mcp::ToolCallResult) -> serde_json::Value {
    let report: serde_json::Value = serde_json::from_str(&result_text(result))
        .unwrap_or_else(|error| panic!("status is not valid JSON: {error}: {result:?}"));
    assert_eq!(report["_kin"]["semantic_surface"], 1);
    let forwarded = kin_mcp::server::finalize_daemon_graph_status(
        result.clone(),
        kin_mcp::Envelope::daemon(),
        &[],
        0,
    );
    assert_ne!(
        forwarded.is_error,
        Some(true),
        "invalid public status: {forwarded:?}"
    );
    assert_eq!(
        serde_json::to_value(forwarded).unwrap(),
        serde_json::to_value(result).unwrap(),
        "forwarding preserves the producer's selected observation and verdict"
    );
    assert!(report.get("source_derivation").is_none());
    assert!(report["enrichment"].get("files").is_none());
    assert!(report["_kin"]["answered_by"].get("repo_root").is_none());
    assert!(report["_kin"]["repository"].get("root").is_none());
    report
}

/// The defect, at the surface an agent sees.
///
/// A daemon that has never completed a status sample, with an embedding pass
/// holding embedding-work serialization, must still answer. Today this returns
/// `isError: true` with "holds no settled reading of this graph", because the
/// settled cache's only writer is downstream of the lock this call cannot take.
#[tokio::test]
async fn first_contact_graph_status_answers_while_an_embedding_pass_holds_the_lock() {
    let (_dir, state) = daemon_at_first_contact();
    admit_two_entities(&state);

    let _embedding_guard = state.embedding_work.lock().unwrap();
    let result = graph_status_over_mcp(Arc::clone(&state)).await;

    assert_ne!(
        result.is_error,
        Some(true),
        "the first tool an agent calls must not fail for the length of an embedding pass: {}",
        result_text(&result)
    );
}

/// The answer must be a report, and it must name where it came from.
///
/// Decoding runs the public forwarding validator, so reaching the assertions below at
/// all proves the published counters are internally consistent. The assertions
/// then pin the three facts a caller needs to know what it is holding: which
/// graph was read, which authority answered, and at which observation fence.
#[tokio::test]
async fn the_first_contact_answer_names_its_scope_authority_and_epoch() {
    let (_dir, state) = daemon_at_first_contact();
    admit_two_entities(&state);
    let epoch = state
        .stable_graph_authority_epoch()
        .expect("a quiescent graph publishes a stable authority epoch");

    let _embedding_guard = state.embedding_work.lock().unwrap();
    let result = graph_status_over_mcp(Arc::clone(&state)).await;
    assert_ne!(result.is_error, Some(true), "{}", result_text(&result));

    let report = parse_report(&result);
    assert_eq!(
        report["scope"], "head",
        "an unscoped call reports the HEAD view"
    );
    assert_eq!(
        report["authority"], "repo-daemon",
        "the answer names the authority that produced it"
    );
    // Nothing has moved this graph's authority since it opened, so the reading's
    // epoch must be the one the daemon is on. A reading carrying some other
    // epoch would be describing a fence this daemon never stood at.
    assert_eq!(
        report["authority_epoch"], epoch,
        "the reading carries the epoch it was taken at"
    );
}

/// A reading taken at an earlier instant says which instant.
///
/// The whole point of answering rather than erroring is that a caller can act on
/// the answer, and it can only do that if it knows how old the answer is and
/// what stopped a live one.
#[tokio::test]
async fn a_replayed_first_contact_reading_states_its_age_and_what_blocked_the_live_sample() {
    let (_dir, state) = daemon_at_first_contact();
    admit_two_entities(&state);

    let _embedding_guard = state.embedding_work.lock().unwrap();
    let result = graph_status_over_mcp(Arc::clone(&state)).await;
    assert_ne!(result.is_error, Some(true), "{}", result_text(&result));

    let report = parse_report(&result);
    assert_eq!(
        report["sampling"], "last_settled_selected_graph",
        "a reading taken before this call is not a live point-in-time sample"
    );
    let stale = report
        .get("stale")
        .expect("a replayed reading discloses itself");
    assert_eq!(
        stale["reason"], "embedding_coverage_changing",
        "the disclosure names the state that blocked the live sample"
    );
    let note = stale["note"]
        .as_str()
        .expect("a stale reading states its cause");
    assert!(
        note.contains("in-flight embedding pass"),
        "the note states the blocking state in words: {}",
        note
    );
    assert!(
        note.contains("kin embed"),
        "the note names what would change the outcome: {}",
        note
    );
}

/// The boundary reading carries the graph the batch actually found.
///
/// `run_background_embedding_batch` calls
/// [`DaemonState::seed_settled_head_graph_status`] at the top of every batch,
/// from inside the guard it already holds. Driving that same production seam
/// here is what pins the counters to a real observation: a reading recorded
/// after an import must carry the import.
///
/// This is the arm that makes "no invented zeros" falsifiable. A fix that seeded
/// only at open, when this store was still empty, satisfies every other arm in
/// this module and fails this one, because it would publish two entities as
/// zero.
#[tokio::test]
async fn a_boundary_reading_carries_the_graph_the_batch_actually_found() {
    let (_dir, state) = daemon_at_first_contact();
    admit_two_entities(&state);
    let live_entities = state.graph.entity_count();
    let live_relations = state.graph.relation_count();
    assert!(
        live_entities > 0,
        "the fixture must hold entities for a published zero to be falsifiable"
    );

    // What the embedding worker does at the top of each batch.
    state.seed_settled_head_graph_status();

    let _embedding_guard = state.embedding_work.lock().unwrap();
    let result = graph_status_over_mcp(Arc::clone(&state)).await;
    assert_ne!(result.is_error, Some(true), "{}", result_text(&result));

    let report = parse_report(&result);
    assert_eq!(
        report["sampling"], "last_settled_selected_graph",
        "the lock is held, so this is the boundary reading and not a live sample"
    );
    assert_eq!(
        report["entity_count"], live_entities,
        "the boundary reading carries the entities the batch found, not the empty graph the open saw"
    );
    assert_eq!(
        report["relation_count"], live_relations,
        "and the relations beside them"
    );
}

/// Control, and it passes on the unmodified source.
///
/// An uncontended first contact already answers live. It is here so the arms
/// above cannot be satisfied by making every answer stale, and so the shape of a
/// healthy reading stays pinned beside the shape of a contended one.
#[tokio::test]
async fn an_uncontended_first_contact_is_a_live_point_in_time_sample() {
    let (_dir, state) = daemon_at_first_contact();
    admit_two_entities(&state);

    let result = graph_status_over_mcp(Arc::clone(&state)).await;
    assert_ne!(result.is_error, Some(true), "{}", result_text(&result));

    let report = parse_report(&result);
    assert_eq!(
        report["sampling"], "point_in_time_selected_graph",
        "an uncontended sample is live"
    );
    assert!(
        report.get("stale").is_none(),
        "a live sample carries no staleness disclosure: {report:?}"
    );
    assert_eq!(
        report["entity_count"],
        state.graph.entity_count(),
        "a live sample carries this graph's own counters"
    );
}

/// A status call made while the first embedding batch builds its embedder
/// samples live.
///
/// The first batch a daemon runs builds the embedder, and on a machine that has
/// never embedded that fetches the model from Hugging Face. The fetch used to
/// run inside the batch, under the lock this call samples under, so for as long
/// as it took every status call replayed the reading settled before it began,
/// and a commit made in that window still read as uncommitted work. A local
/// run of `live_graph_durability_disclosure` sampled its daemon in exactly that
/// state: the batch was inside the model download, and every status read after
/// the commit, for over a minute, was a replay.
///
/// Driven through the production batch seam with the build held open, so the
/// assertion is about where the build runs relative to the lock and not about
/// how long a real fetch takes.
#[tokio::test]
async fn status_samples_live_while_the_first_batch_builds_its_embedder() {
    let (_dir, state) = daemon_at_first_contact();
    admit_two_entities(&state);

    let (building_tx, building_rx) = std::sync::mpsc::sync_channel::<()>(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(0);
    let batch_state = Arc::clone(&state);
    let batch = std::thread::spawn(move || {
        crate::daemon::run_background_embedding_batch(
            &batch_state,
            true,
            move |_| {
                // Stands in for the model fetch: held until the status call
                // below has answered.
                building_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            },
            |_| Ok(0),
        )
    });
    building_rx.recv().unwrap();

    let result = graph_status_over_mcp(Arc::clone(&state)).await;
    release_tx.send(()).unwrap();
    let outcome = batch.join().unwrap();

    assert_ne!(result.is_error, Some(true), "{}", result_text(&result));
    let report = parse_report(&result);
    assert_eq!(
        report["sampling"], "point_in_time_selected_graph",
        "building the embedder holds nothing status samples under, so the answer is live: \
         {report:?}"
    );
    assert!(
        report.get("stale").is_none(),
        "a live sample carries no staleness disclosure: {report:?}"
    );
    assert_eq!(report["entity_count"], state.graph.entity_count());
    assert!(
        matches!(
            outcome,
            crate::daemon::BackgroundEmbeddingBatchOutcome::Completed(0)
        ),
        "the batch still runs once the embedder is built: {outcome:?}"
    );
}
