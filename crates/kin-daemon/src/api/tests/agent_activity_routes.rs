// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// What an agent does through the daemon, as `/agents`, `/session/{id}/calls`
// and the `ToolCall` stream show it.

/// Open a session the way the MCP server does when its client initializes:
/// named by the client, in a folder, with no process.
async fn activity_open_session(state: &Arc<DaemonState>) -> String {
    let started = router(Arc::clone(state))
        .oneshot(
            Request::post("/session")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "vendor": "claude-code",
                        "client_name": "claude-code",
                        "transport": "mcp",
                        "cwd": state.layout.working_dir(),
                        "capabilities": {
                            "can_read": true,
                            "can_write": true,
                            "can_execute": false,
                            "can_branch": false,
                            "can_commit": true,
                            "max_concurrent_intents": 1,
                        },
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(started.status(), StatusCode::OK);
    let body = axum::body::to_bytes(started.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    body["session_id"].as_str().unwrap().to_string()
}

async fn activity_get(state: &Arc<DaemonState>, route: &str) -> (StatusCode, String) {
    let response = router(Arc::clone(state))
        .oneshot(Request::get(route).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

async fn activity_call(
    state: &Arc<DaemonState>,
    session: Option<&str>,
    name: &str,
    arguments: serde_json::Value,
) -> axum::response::Response {
    let mut request = Request::post("/mcp/tools/call").header("content-type", "application/json");
    if let Some(session) = session {
        request = request.header("X-Kin-Session", session);
    }
    router(Arc::clone(state))
        .oneshot(
            request
                .body(Body::from(
                    serde_json::json!({ "name": name, "arguments": arguments }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
}

fn activity_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => out.push(text.clone()),
        serde_json::Value::Array(items) => {
            for item in items {
                activity_strings(item, out);
            }
        }
        serde_json::Value::Object(fields) => {
            for (key, item) in fields {
                out.push(key.clone());
                activity_strings(item, out);
            }
        }
        _ => {}
    }
}

/// Nothing a reader of `/agents` or `/session/{id}/calls` is served may be a
/// directory, a process id, a path, or what the agent asked.
fn assert_path_free(body: &str, forbidden: &[&str]) {
    for needle in forbidden {
        assert!(!body.contains(needle), "{needle} leaked into {body}");
    }
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    let mut strings = Vec::new();
    activity_strings(&value, &mut strings);
    for text in strings {
        assert!(
            !text.contains('/') && !text.contains('\\'),
            "path-shaped string served: {text}"
        );
        assert!(
            !crate::agent_activity::is_path_like(&text),
            "file-shaped string served: {text}"
        );
    }
}

#[tokio::test]
async fn an_agents_reads_are_shown_by_entity_and_never_by_what_it_asked_or_where_it_runs() {
    let state = test_state();
    state
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let entity = test_entity("handler", "src/secret_module.py");
    state.graph.upsert_entity(&entity).unwrap();
    let session = activity_open_session(&state).await;
    let working_dir = state.layout.working_dir().display().to_string();

    let answered = activity_call(
        &state,
        Some(&session),
        "find_references",
        serde_json::json!({
            "entity_id": entity.id.to_string(),
            "query": "private question text about src/secret_module.py",
        }),
    )
    .await;
    assert_eq!(answered.status(), StatusCode::OK);
    assert!(
        answered.headers().get("x-kin-session-status").is_none(),
        "a held session is not reported unknown"
    );

    let (status, calls) = activity_get(&state, &format!("/session/{session}/calls")).await;
    assert_eq!(status, StatusCode::OK, "{calls}");
    let parsed: serde_json::Value = serde_json::from_str(&calls).unwrap();
    assert_eq!(parsed["schema"], "kin.agent-calls.v1");
    assert_eq!(parsed["calls_total"], 1);
    let call = &parsed["calls"][0];
    assert_eq!(call["seq"], 1);
    assert_eq!(call["tool"], "find_references");
    assert_eq!(call["entity_ids"][0], entity.id.to_string());
    assert!(call["bytes"].as_u64().unwrap() > 0);
    let forbidden = [
        working_dir.as_str(),
        "private question",
        "secret_module",
        "\"cwd\"",
        "\"pid\"",
    ];
    assert_path_free(&calls, &forbidden);

    let (status, agents) = activity_get(&state, "/agents").await;
    assert_eq!(status, StatusCode::OK, "{agents}");
    let parsed: serde_json::Value = serde_json::from_str(&agents).unwrap();
    assert_eq!(parsed["schema"], "kin.agents.v1");
    let agent = parsed["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["session_id"] == session.as_str())
        .expect("the reading agent is shown");
    assert_eq!(agent["client"], "claude-code");
    assert_eq!(agent["transport"], "mcp");
    assert_eq!(agent["state"], "active");
    assert_eq!(agent["calls_total"], 1);
    assert_eq!(agent["recent_calls"][0]["tool"], "find_references");
    assert!(
        parsed["agents"]
            .as_array()
            .unwrap()
            .iter()
            .all(|agent| agent["vendor"] != crate::agent_activity::DAEMON_OWN_VENDOR),
        "the daemon's own reconcile session is not an agent: {agents}"
    );
    assert_path_free(&agents, &forbidden);

    // An ended session is gone from both, and so is its log.
    let ended = router(Arc::clone(&state))
        .oneshot(
            Request::delete(format!("/session/{session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ended.status(), StatusCode::OK);
    let (status, _) = activity_get(&state, &format!("/session/{session}/calls")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, agents) = activity_get(&state, "/agents").await;
    assert!(!agents.contains(&session));
    assert_eq!(state.coordinator.activity().retained_entries(), 0);
}

#[tokio::test]
async fn a_call_naming_a_session_the_daemon_does_not_hold_is_told_so() {
    let state = test_state();
    state
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let stranger = SessionId::new().to_string();
    let answered = activity_call(
        &state,
        Some(&stranger),
        "semantic_search",
        serde_json::json!({"query": "handler"}),
    )
    .await;
    assert_eq!(answered.status(), StatusCode::OK);
    assert_eq!(
        answered
            .headers()
            .get("x-kin-session-status")
            .and_then(|value| value.to_str().ok()),
        Some("unknown")
    );
    assert_eq!(state.coordinator.activity().logged_sessions(), 0);

    // A call that names no session is not told anything.
    let anonymous = activity_call(
        &state,
        None,
        "semantic_search",
        serde_json::json!({"query": "x"}),
    )
    .await;
    assert!(anonymous.headers().get("x-kin-session-status").is_none());
}

#[tokio::test]
async fn a_session_opened_without_a_process_takes_one_when_it_writes() {
    let state = test_state();
    let session = activity_open_session(&state).await;
    let session_id = parse_session_id(&session).unwrap();
    assert_eq!(
        state
            .coordinator
            .get_session(&session_id)
            .unwrap()
            .unwrap()
            .pid,
        None
    );
    let attach = |pid: u32| {
        let state = Arc::clone(&state);
        let session = session.clone();
        async move {
            router(state)
                .oneshot(
                    Request::post(format!("/session/{session}/process"))
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::json!({ "pid": pid }).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap()
        }
    };
    let attached = attach(std::process::id()).await;
    assert_eq!(attached.status(), StatusCode::OK);
    assert_eq!(
        state
            .coordinator
            .get_session(&session_id)
            .unwrap()
            .unwrap()
            .pid,
        Some(std::process::id())
    );
    // A session that names a process keeps it.
    let other = attach(std::process::id() + 1).await;
    assert_eq!(other.status(), StatusCode::OK);
    assert_eq!(
        state
            .coordinator
            .get_session(&session_id)
            .unwrap()
            .unwrap()
            .pid,
        Some(std::process::id())
    );
    let missing = router(Arc::clone(&state))
        .oneshot(
            Request::post(format!("/session/{}/process", SessionId::new()))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({ "pid": 1 }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

/// Read the next `data:` frame off an SSE body, skipping heartbeats.
async fn activity_next_frame<S>(body: &mut S) -> serde_json::Value
where
    S: futures_util::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin,
{
    loop {
        let chunk = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            futures_util::StreamExt::next(body),
        )
        .await
        .expect("a frame arrives")
        .expect("the stream is open")
        .unwrap();
        let text = String::from_utf8(chunk.to_vec()).unwrap();
        if let Some(data) = text.trim().strip_prefix("data: ") {
            return serde_json::from_str(data).unwrap();
        }
    }
}

#[tokio::test]
async fn tool_calls_reach_only_the_subscribers_that_ask_for_them() {
    let state = test_state();
    state
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let entity = test_entity("handler", "src/lib.py");
    state.graph.upsert_entity(&entity).unwrap();
    let session = activity_open_session(&state).await;

    let asked = router(Arc::clone(&state))
        .oneshot(
            Request::get("/graph/events?types=ToolCall")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut asked = asked.into_body().into_data_stream();
    assert_eq!(activity_next_frame(&mut asked).await["type"], "connected");
    let unfiltered = router(Arc::clone(&state))
        .oneshot(Request::get("/graph/events").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let mut unfiltered = unfiltered.into_body().into_data_stream();
    assert_eq!(
        activity_next_frame(&mut unfiltered).await["type"],
        "connected"
    );

    let answered = activity_call(
        &state,
        Some(&session),
        "find_references",
        serde_json::json!({ "entity_id": entity.id.to_string() }),
    )
    .await;
    assert_eq!(answered.status(), StatusCode::OK);

    let frame = activity_next_frame(&mut asked).await;
    assert_eq!(frame["type"], "ToolCall");
    assert_eq!(frame["session_id"], session.as_str());
    assert_eq!(frame["call_seq"], 1);
    assert_eq!(frame["tool"], "find_references");
    assert_eq!(frame["coalesced"], 0);
    assert!(
        frame["seq"].as_u64().is_some(),
        "every frame carries its position"
    );
    assert!(!frame.to_string().contains("src/lib.py"));

    // The unfiltered subscriber's next frame is the graph event emitted after
    // the call, not the call.
    state.emit_event(crate::state::DaemonEvent::GraphDeltaApplied {
        nodes_added: 1,
        nodes_modified: 0,
        nodes_removed: 0,
        relations_added: 0,
        relations_removed: 0,
    });
    assert_eq!(
        activity_next_frame(&mut unfiltered).await["type"],
        "GraphDeltaApplied"
    );
}

/// Read-only calls refresh their session's liveness and fill its call log,
/// and write nothing a save would have to carry: the graph keeps no reason to
/// make its next save a full snapshot, and the session record is unchanged.
#[tokio::test]
async fn read_only_calls_give_the_next_save_nothing_to_carry() {
    let state = test_state();
    state
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let session = activity_open_session(&state).await;
    let session_id = parse_session_id(&session).unwrap();
    let recorded = kin_model::SessionStore::get_session(state.graph.as_ref(), &session_id)
        .unwrap()
        .unwrap()
        .last_heartbeat;
    // Stand in for the save that followed the session's open.
    state.graph.clear_full_snapshot_required();
    let pending_before = state.graph.has_unpersisted_changes();
    let dirty_before = state.is_dirty();

    std::thread::sleep(std::time::Duration::from_millis(20));
    for index in 0..25 {
        let answered = activity_call(
            &state,
            Some(&session),
            "semantic_search",
            serde_json::json!({ "query": format!("handler {index}") }),
        )
        .await;
        assert_eq!(answered.status(), StatusCode::OK);
    }

    assert!(
        !state.graph.full_snapshot_required(),
        "read-only calls must not make the next save a full snapshot"
    );
    assert_eq!(state.graph.has_unpersisted_changes(), pending_before);
    assert_eq!(state.is_dirty(), dirty_before);
    assert_eq!(
        kin_model::SessionStore::get_session(state.graph.as_ref(), &session_id)
            .unwrap()
            .unwrap()
            .last_heartbeat,
        recorded,
        "the session record is not rewritten for liveness"
    );
    let live = state.coordinator.get_session(&session_id).unwrap().unwrap();
    assert!(live.last_heartbeat > recorded, "liveness still advances");
    let (calls_total, _, _) = state.coordinator.activity().recent_calls(&session_id, 1);
    assert_eq!(calls_total, 25);
}

/// A call on the repo-scoped route that names a session is recorded against
/// it the same way, with the repository it named, and a session the daemon
/// does not hold is told so there too.
#[tokio::test]
async fn repo_scoped_calls_are_recorded_against_their_session() {
    let state = test_state();
    state
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let session = activity_open_session(&state).await;
    let repo_id = state.cached_repo_id.clone();
    let call = |session: String| {
        let state = Arc::clone(&state);
        let route = format!("/repos/{repo_id}/mcp/tools/call");
        async move {
            router(state)
                .oneshot(
                    Request::post(route)
                        .header("content-type", "application/json")
                        .header("X-Kin-Session", session)
                        .body(Body::from(
                            serde_json::json!({
                                "schema_version": 1,
                                "name": "semantic_locate",
                                "arguments": {"query": "private question text"},
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap()
        }
    };

    let answered = call(session.clone()).await;
    assert!(answered.headers().get("x-kin-session-status").is_none());
    let status = answered.status();
    let body = axum::body::to_bytes(answered.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert!(
        !body.is_empty(),
        "the route's answer reaches the caller unchanged ({status})"
    );

    let (status, calls) = activity_get(&state, &format!("/session/{session}/calls")).await;
    assert_eq!(status, StatusCode::OK, "{calls}");
    let parsed: serde_json::Value = serde_json::from_str(&calls).unwrap();
    assert_eq!(parsed["calls_total"], 1);
    assert_eq!(parsed["calls"][0]["tool"], "semantic_locate");
    assert_eq!(parsed["calls"][0]["repo_id"], repo_id.as_str());
    assert_path_free(&calls, &["private question", "\"cwd\"", "\"pid\""]);

    let stranger = call(SessionId::new().to_string()).await;
    assert_eq!(
        stranger
            .headers()
            .get("x-kin-session-status")
            .and_then(|value| value.to_str().ok()),
        Some("unknown")
    );
}

#[tokio::test]
async fn refused_names_are_private_and_bounded_on_both_activity_routes() {
    for repo_scoped in [false, true] {
        let state = test_state();
        state
            .is_initialized
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let session = activity_open_session(&state).await;
        let marker = "private-tool-name-marker";
        let name = format!("/{marker}/{}", "x".repeat(64 * 1024));
        let route = if repo_scoped {
            format!("/repos/{}/mcp/tools/call", state.cached_repo_id)
        } else {
            "/mcp/tools/call".to_string()
        };
        let call = |with_session: bool| {
            let state = Arc::clone(&state);
            let route = route.clone();
            let session = session.clone();
            let name = name.clone();
            async move {
                let mut request = Request::post(route).header("content-type", "application/json");
                if with_session {
                    request = request.header("X-Kin-Session", session);
                }
                router(state)
                    .oneshot(
                        request
                            .body(Body::from(
                                serde_json::json!({
                                    "schema_version": 1, "name": name, "arguments": {}
                                })
                                .to_string(),
                            ))
                            .unwrap(),
                    )
                    .await
                    .unwrap()
            }
        };
        let baseline = call(false).await;
        let baseline_status = baseline.status();
        let baseline_body = axum::body::to_bytes(baseline.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let events = router(Arc::clone(&state))
            .oneshot(
                Request::get("/graph/events?types=ToolCall")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut events = events.into_body().into_data_stream();
        assert_eq!(activity_next_frame(&mut events).await["type"], "connected");
        let refused = call(true).await;
        assert_eq!(refused.status(), baseline_status);
        let body = axum::body::to_bytes(refused.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(body, baseline_body, "activity does not alter the refusal");
        let frame = activity_next_frame(&mut events).await;
        assert_eq!(frame["type"], "ToolCall");
        assert_eq!(frame["tool"], "unrecognized_tool");
        assert_eq!(frame["call_seq"], 1);
        assert!(!frame.to_string().contains(marker));
        for route in ["/agents".to_string(), format!("/session/{session}/calls")] {
            let (status, body) = activity_get(&state, &route).await;
            assert_eq!(status, StatusCode::OK);
            assert_path_free(&body, &[marker]);
            assert!(body.contains("unrecognized_tool"));
        }
        let (_, _, calls) = state
            .coordinator
            .activity()
            .recent_calls(&parse_session_id(&session).unwrap(), 1);
        assert_eq!(calls[0].seq, 1);
        let expected_bytes = if repo_scoped {
            body.len()
        } else {
            // The local log counts the dispatch answer before the public
            // presentation boundary removes caller-supplied path text.
            kin_mcp::McpError::ToolNotFound(name).to_string().len()
        };
        assert_eq!(
            calls[0].bytes, expected_bytes,
            "the answer size metric stays unchanged"
        );
    }
}
