// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

/// Private expanded-base recovery controls preserve assertions about the
/// exact retained payload. Public ticket replay is covered independently below.
async fn private_source_base_mutate(
    state: &Arc<DaemonState>,
    _name: &str,
    arguments: serde_json::Value,
    session: &str,
) -> kin_mcp::ToolCallResult {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("X-Kin-Session", session.parse().unwrap());
    crate::mcp_mutate::call(
        Arc::clone(state),
        headers,
        serde_json::from_value(arguments).unwrap(),
        true,
    )
    .await
    .unwrap()
}

#[tokio::test]
#[serial_test::serial]
async fn mcp_mutate_source_base_replays_old_success_but_retains_new_stale_work() {
    let (_dir, state, source) = source_base_fixture().await;
    let session = mcp_test_session(&state);
    let request = serde_json::json!({
        "session_id": session,
        "request_id": "guarded-edit",
        "operations": [source_base_operation(&source)],
        "summary": "Update the value through its exact source read"
    });
    let first = private_source_base_mutate(&state, "kin_mutate", request.clone(), &session).await;
    let original = mutation_receipt(&first);
    assert_eq!(tool_result_payload(&first)["already_applied"], false);
    assert_eq!(original["ops_applied"], 1);

    let later_body = "pub fn value() -> u8 { 3 }";
    source_base_commit_fresh(
        &state,
        &source["id"],
        later_body,
        "Intervening authored work",
    )
    .await;
    let later_roots = source_base_roots(&state);
    let layout = state.layout.clone();
    drop(state);
    let reopened = Arc::new(DaemonState::open(layout).unwrap());
    reopened
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let replay =
        private_source_base_mutate(&reopened, "kin_mutate", request.clone(), &session).await;
    assert_eq!(mutation_receipt(&replay), original);
    assert_eq!(tool_result_payload(&replay)["already_applied"], true);
    assert_eq!(source_base_roots(&reopened), later_roots);

    let current_session = mcp_test_session(&reopened);
    let mut stale = request.clone();
    stale["session_id"] = serde_json::json!(current_session);
    stale["request_id"] = serde_json::json!("new-stale-edit");
    let before = source_base_roots(&reopened);
    let refused =
        private_source_base_mutate(&reopened, "kin_mutate", stale.clone(), &current_session).await;
    assert_eq!(refused.is_error, Some(true));
    assert!(
        kin_mcp::source_base::is_source_base_conflict(&mcp_result_text(&refused)),
        "{}",
        mcp_result_text(&refused)
    );
    assert_eq!(source_base_roots(&reopened), before);
    let conflict = tool_result_payload(&refused);
    let tx = conflict["transaction_id"].as_str().unwrap();
    let persisted =
        crate::state::load_persisted_mcp_transactions_checked(&reopened.layout).unwrap();
    assert_eq!(persisted[tx].state, "active");
    assert_eq!(
        persisted[tx].staged_operations[0].body.as_deref(),
        Some(SOURCE_BASE_EDIT)
    );
    assert_eq!(
        serde_json::to_value(&persisted[tx].staged_operations[0]).unwrap()["payload"],
        stale["operations"][0]["payload"]
    );
    let (_, binding) = mutation_record(&reopened, "new-stale-edit").unwrap();
    assert_eq!(binding["transaction_id"], tx);
    assert_eq!(binding["request"]["operations"], stale["operations"]);

    let current = mcp_call(
        router(Arc::clone(&reopened)),
        "get_entity_source",
        serde_json::json!({"entity_id": source["id"]}),
    )
    .await;
    assert_eq!(tool_result_payload(&current)["body"], later_body);
    assert_eq!(source_base_roots(&reopened), before);
}

/// The public source ticket survives publication, later work and a daemon
/// restart because replay matches the original request before rereading source.
#[tokio::test]
#[serial_test::serial]
async fn public_source_ticket_durable_mutation_replays_after_source_changes() {
    let (_dir, state, witness) = source_base_fixture().await;
    let public = mcp_call(
        router(Arc::clone(&state)),
        "get_entity_source",
        serde_json::json!({"entity_id":witness["id"]}),
    )
    .await;
    let source = tool_result_payload(&public);
    assert_ne!(public.is_error, Some(true), "{}", mcp_result_text(&public));
    assert!(
        kin_mcp::source_ticket::is_ticket(&source["source_base"]),
        "{source}"
    );
    assert_eq!(source["body"], witness["body"]);
    assert_eq!(
        source["source_base"],
        kin_mcp::source_ticket::compact(&witness["source_base"]).unwrap()
    );
    let session = mcp_test_session(&state);
    let request = serde_json::json!({"session_id":session,"request_id":"public-ticket",
        "operations":[source_base_operation(&source)]});
    let first = mutate_http(&state, "kin_mutate", request.clone(), &session).await;
    let receipt = mutation_receipt(&first);
    source_base_commit_fresh(
        &state,
        &source["id"],
        "pub fn value() -> u8 { 3 }",
        "later work",
    )
    .await;
    let roots = source_base_roots(&state);
    let layout = state.layout.clone();
    drop(state);
    let reopened = Arc::new(DaemonState::open(layout).unwrap());
    reopened
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let replay = mutate_http(&reopened, "kin_mutate", request.clone(), &session).await;
    assert_eq!(mutation_receipt(&replay), receipt);
    assert_eq!(
        tool_result_payload(&replay)["publication_accounting"],
        tool_result_payload(&first)["publication_accounting"]
    );
    assert_eq!(tool_result_payload(&replay)["already_applied"], true);
    assert_eq!(source_base_roots(&reopened), roots);
    let mut changed = request.clone();
    changed["operations"][0]["body"] = serde_json::json!("pub fn value() -> u8 { 9 }");
    let mismatch = mutate_http(&reopened, "kin_mutate", changed, &session).await;
    assert_eq!(mismatch.is_error, Some(true));
    assert!(mcp_result_text(&mismatch).contains("request_id_payload_mismatch"));
    let new_session = mcp_test_session(&reopened);
    let mut stale = request;
    stale["session_id"] = serde_json::json!(new_session);
    stale["request_id"] = serde_json::json!("new-stale-ticket");
    let refused = mutate_http(&reopened, "kin_mutate", stale, &new_session).await;
    assert_eq!(
        refused.is_error,
        Some(true),
        "{}",
        mcp_result_text(&refused)
    );
    assert_eq!(source_base_roots(&reopened), roots);
}
