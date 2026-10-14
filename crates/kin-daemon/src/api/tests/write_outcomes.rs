// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// What a write leaves in its session's calls when its caller stops waiting
// before it ends. Included into `api::tests`.

/// The commit an agent's client gave up on. The client's patience ran out,
/// it closed its connection and its session ended, and the commit ran on
/// and landed. The session's calls name the begin, the stage and the
/// commit, and the commit says the change it published and that nobody
/// was waiting for it.
#[tokio::test]
async fn a_commit_that_lands_after_its_caller_left_is_kept_on_its_session() {
    let state = test_state();
    let mut caller = test_entity("caller", "src/lib.rs");
    caller.file_origin = None;
    caller.span = None;
    let mut callee = test_entity("callee", "src/lib.rs");
    callee.file_origin = None;
    callee.span = None;
    install_repository_entities(&state, vec![caller.clone(), callee.clone()]);
    state
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let session_id = mcp_test_session(&state);
    let session = SessionId(Uuid::parse_str(&session_id).unwrap());

    let begin = mcp_call_as(
        router(Arc::clone(&state)),
        "kin_transaction_begin",
        serde_json::json!({ "session_id": session_id, "scope": "file:src/lib.rs" }),
        session,
    )
    .await;
    let begin_json: serde_json::Value = serde_json::from_str(&mcp_result_text(&begin)).unwrap();
    let tx_id = begin_json["transaction_id"].as_str().unwrap().to_string();
    let stage = mcp_call_as(
        router(Arc::clone(&state)),
        "kin_transaction_stage",
        serde_json::json!({
            "transaction_id": tx_id,
            "operations": [{
                "verb": "add",
                "target": "",
                "payload": { "Relation": {
                    "from": caller.id, "to": callee.id, "kind": RelationKind::Calls }},
                "description": ""
            }]
        }),
        session,
    )
    .await;
    assert_ne!(stage.is_error, Some(true), "{}", mcp_result_text(&stage));

    // The commit's answer is held back from its record long enough for
    // the caller to give up and the session to end first.
    let owner = Arc::as_ptr(&state) as usize;
    *CALL_RECORD_PAUSE_FOR_TEST.lock().unwrap() = Some((owner, Duration::from_millis(800)));
    let commit = router(Arc::clone(&state)).oneshot(
        Request::post("/mcp/tools/call")
            .header("content-type", "application/json")
            .header("X-Kin-Session", session.to_string())
            .body(Body::from(
                serde_json::json!({
                    "name": "kin_transaction_commit",
                    "arguments": { "transaction_id": tx_id, "message": "Call the callee" }
                })
                .to_string(),
            ))
            .unwrap(),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(150), commit)
            .await
            .is_err(),
        "the commit answered before its caller gave up"
    );
    state.coordinator.deregister_session(&session).unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let kept = loop {
        let kept = state
            .agent_authorship
            .ended_session(&session_id, &Timestamp::now())
            .expect("the session that did work is kept once it ends");
        if kept
            .calls
            .iter()
            .any(|call| call.tool == "kin_transaction_commit")
        {
            break kept;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the commit never reached the session's calls: {kept:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    *CALL_RECORD_PAUSE_FOR_TEST.lock().unwrap() = None;

    let tools: Vec<&str> = kept.calls.iter().map(|call| call.tool.as_str()).collect();
    assert_eq!(
        tools,
        [
            "kin_transaction_begin",
            "kin_transaction_stage",
            "kin_transaction_commit"
        ]
    );
    assert_eq!(kept.calls_total, 3);
    let committed = kept.calls.last().unwrap();
    assert_eq!(committed.seq, 3);
    assert!(!committed.is_error);
    assert!(committed.caller_left, "{committed:?}");
    let change = committed
        .change_id
        .clone()
        .expect("the commit names the change it published");
    assert!(kept.change_ids.contains(&change), "{kept:?}");
    // What the begin and the stage were told reached their caller.
    assert!(kept.calls[..2].iter().all(|call| !call.caller_left));
}

/// A commit whose caller stopped waiting while its session lives is on
/// the live log, saying so, and a write its caller waited for says the
/// change it made and nothing about anybody leaving.
#[tokio::test]
async fn a_live_session_shows_a_commit_its_caller_stopped_waiting_for() {
    let state = test_state();
    let mut caller = test_entity("caller", "src/lib.rs");
    caller.file_origin = None;
    caller.span = None;
    let mut callee = test_entity("callee", "src/lib.rs");
    callee.file_origin = None;
    callee.span = None;
    install_repository_entities(&state, vec![caller.clone(), callee.clone()]);
    state
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let session_id = mcp_test_session(&state);
    let session = SessionId(Uuid::parse_str(&session_id).unwrap());
    let operations = serde_json::json!([{
        "verb": "add",
        "target": "",
        "payload": { "Relation": {
            "from": caller.id, "to": callee.id, "kind": RelationKind::Calls }},
        "description": ""
    }]);
    let begin = mcp_call_as(
        router(Arc::clone(&state)),
        "kin_transaction_begin",
        serde_json::json!({ "session_id": session_id, "scope": "file:src/lib.rs" }),
        session,
    )
    .await;
    let begin_json: serde_json::Value = serde_json::from_str(&mcp_result_text(&begin)).unwrap();
    let tx_id = begin_json["transaction_id"].as_str().unwrap().to_string();

    let owner = Arc::as_ptr(&state) as usize;
    *CALL_RECORD_PAUSE_FOR_TEST.lock().unwrap() = Some((owner, Duration::from_millis(500)));
    let commit = router(Arc::clone(&state)).oneshot(
        Request::post("/mcp/tools/call")
            .header("content-type", "application/json")
            .header("X-Kin-Session", session.to_string())
            .body(Body::from(
                serde_json::json!({
                    "name": "kin_transaction_commit",
                    "arguments": { "transaction_id": tx_id, "operations": operations }
                })
                .to_string(),
            ))
            .unwrap(),
    );
    assert!(tokio::time::timeout(Duration::from_millis(100), commit)
        .await
        .is_err());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let committed = loop {
        let (_, _, calls) = state.coordinator.activity().recent_calls(&session, 10);
        if let Some(call) = calls
            .iter()
            .find(|call| call.tool == "kin_transaction_commit")
        {
            break call.clone();
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    *CALL_RECORD_PAUSE_FOR_TEST.lock().unwrap() = None;
    assert!(!committed.is_error, "{committed:?}");
    assert!(committed.caller_left);
    let change = committed.change_id.clone().unwrap();
    assert_eq!(
        state.coordinator.activity().recent_changes(&session),
        vec![change]
    );
    let (_, _, calls) = state.coordinator.activity().recent_calls(&session, 10);
    let begun = calls
        .iter()
        .find(|call| call.tool == "kin_transaction_begin")
        .unwrap();
    assert!(!begun.caller_left && begun.change_id.is_none());
}
