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

    // An ended session is no longer listed as here, and its live log is
    // gone. What it did is kept past its end, as path-free as the live log,
    // and it outlives the daemon.
    let ended = router(Arc::clone(&state))
        .oneshot(
            Request::delete(format!("/session/{session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ended.status(), StatusCode::OK);
    let (_, agents) = activity_get(&state, "/agents").await;
    assert!(!agents.contains(&session));
    assert_eq!(state.coordinator.activity().retained_entries(), 0);
    let (status, kept) = activity_get(&state, &format!("/session/{session}/calls")).await;
    assert_eq!(status, StatusCode::OK, "{kept}");
    let kept_calls: serde_json::Value = serde_json::from_str(&kept).unwrap();
    assert_eq!(kept_calls["calls_total"], 1);
    assert_eq!(kept_calls["calls"][0]["tool"], "find_references");
    assert!(kept_calls["ended_at"].is_string(), "{kept}");
    assert_path_free(&kept, &forbidden);
    let gone = activity_ended(&state, &session)
        .await
        .expect("the agent that ended is kept");
    assert_eq!(gone["client"], "claude-code");
    assert_eq!(gone["transport"], "mcp");
    assert_eq!(gone["calls_kept"], true);
    assert_eq!(gone["calls_total"], 1);
    assert_eq!(gone["recent_calls"][0]["tool"], "find_references");
    assert_eq!(gone["read_entity_ids"][0], entity.id.to_string());
    let (_, listed) = activity_get(&state, "/agents/ended").await;
    let listed_json: serde_json::Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed_json["schema"], "kin.agents-ended.v1");
    assert_eq!(
        listed_json["kept_days"],
        crate::ended_sessions::ENDED_KEPT_DAYS
    );
    assert_eq!(
        listed_json["kept_sessions"],
        crate::ended_sessions::MAX_ENDED_SESSIONS
    );
    assert_path_free(&listed, &forbidden);

    // Written to repository authority once the session has ended and the
    // quiet window has elapsed, and read back from there by a daemon that
    // reopens the store.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(crate::agent_authorship::settle(&state, false).await >= 1);
    let layout = state.layout.clone();
    drop(state);
    let reopened = Arc::new(DaemonState::open(layout).unwrap());
    let again = activity_ended(&reopened, &session)
        .await
        .expect("the agent that ended outlives the daemon");
    assert_eq!(again["calls_total"], 1);
    assert_eq!(again["read_entity_ids"][0], entity.id.to_string());
}

/// The entry `/agents/ended` serves for `session`, if it serves one.
async fn activity_ended(state: &Arc<DaemonState>, session: &str) -> Option<serde_json::Value> {
    let (status, body) = activity_get(state, "/agents/ended").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    parsed["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["session_id"] == session.to_ascii_lowercase())
        .cloned()
}

/// An agent that worked here and left is still shown with what it did: one
/// whose process was killed, before the sweep reaps it and after, and one
/// that ended before its calls were kept, by what it wrote. A write older
/// than the window ended sessions are kept for is not shown.
#[tokio::test]
async fn agents_that_worked_here_and_left_are_kept_with_what_they_read_and_wrote() {
    let state = test_state();
    state
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let entity = test_entity("handler", "src/secret_module.py");
    state.graph.upsert_entity(&entity).unwrap();
    let working_dir = state.layout.working_dir().display().to_string();
    let started = router(Arc::clone(&state))
        .oneshot(
            Request::post("/session")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "vendor": "codex",
                        "client_name": "codex",
                        "transport": "mcp",
                        "cwd": state.layout.working_dir(),
                        // A process id no process holds, as a killed server's is.
                        "watch_pid": 999_999_999u32,
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
    let killed = body["session_id"].as_str().unwrap().to_string();
    let answered = activity_call(
        &state,
        Some(&killed),
        "find_references",
        serde_json::json!({ "entity_id": entity.id.to_string() }),
    )
    .await;
    assert_eq!(answered.status(), StatusCode::OK);

    let (_, here) = activity_get(&state, "/agents").await;
    assert!(!here.contains(&killed), "a killed agent is not shown as here");
    let before_sweep = activity_ended(&state, &killed)
        .await
        .expect("a killed agent is shown as gone before the sweep reaps it");
    assert_eq!(before_sweep["calls_total"], 1);
    assert_eq!(before_sweep["read_entity_ids"][0], entity.id.to_string());
    let reaped = state.coordinator.sweep_stale_sessions_detailed().unwrap();
    assert_eq!(reaped.len(), 1);
    let after_sweep = activity_ended(&state, &killed)
        .await
        .expect("and after it");
    assert_eq!(after_sweep["calls_kept"], true);
    assert_eq!(after_sweep["calls_total"], 1);
    assert_eq!(after_sweep["client"], "codex");

    let writer = SessionId::new().to_string();
    let change = "c".repeat(64);
    state
        .agent_authorship
        .record(kin_cli::commands::review_guide::AgentWrite {
            change_id: change.clone(),
            session: writer.clone(),
            vendor: "claude-code".into(),
            client: "claude-code".into(),
            at: kin_model::timestamp::Timestamp::now().to_string(),
            entities: vec![entity.id],
        });
    let old_writer = SessionId::new().to_string();
    state
        .agent_authorship
        .record(kin_cli::commands::review_guide::AgentWrite {
            change_id: "d".repeat(64),
            session: old_writer.clone(),
            vendor: "claude-code".into(),
            client: "claude-code".into(),
            at: kin_model::timestamp::Timestamp(
                chrono::Utc::now()
                    - chrono::Duration::days(
                        i64::try_from(crate::ended_sessions::ENDED_KEPT_DAYS).unwrap() + 1,
                    ),
            )
            .to_string(),
            entities: vec![entity.id],
        });
    let wrote = activity_ended(&state, &writer)
        .await
        .expect("an agent known only by what it wrote is shown");
    assert_eq!(wrote["calls_kept"], false);
    assert_eq!(wrote["client"], "claude-code");
    assert_eq!(wrote["edited_entity_ids"][0], entity.id.to_string());
    assert_eq!(wrote["change_ids"][0], change);
    assert!(
        activity_ended(&state, &old_writer).await.is_none(),
        "a write past the window is not shown"
    );
    let (_, listed) = activity_get(&state, "/agents/ended").await;
    assert_path_free(
        &listed,
        &[working_dir.as_str(), "secret_module", "\"cwd\"", "\"pid\""],
    );
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

/// A connection that opens its session to read names its server's process to
/// watch. The record still names no process, so a live idle connection ages
/// out on the idle rule; once the process is gone the agent is no longer
/// listed, and the next sweep reaps the session.
#[tokio::test]
async fn a_read_session_whose_server_was_killed_is_not_listed_and_is_reaped() {
    let state = test_state();
    let open = |watch_pid: u32, client: &'static str| {
        let state = Arc::clone(&state);
        async move {
            let started = router(Arc::clone(&state))
                .oneshot(
                    Request::post("/session")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "vendor": client,
                                "client_name": client,
                                "transport": "mcp",
                                "cwd": state.layout.working_dir(),
                                "watch_pid": watch_pid,
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
            parse_session_id(body["session_id"].as_str().unwrap()).unwrap()
        }
    };
    // A process id no process holds, as a killed server's is.
    let killed = open(999_999_999, "codex").await;
    let running = open(std::process::id(), "claude-code").await;
    for session in [killed, running] {
        assert_eq!(
            state.coordinator.get_session(&session).unwrap().unwrap().pid,
            None,
            "a watched process is not one the session lives by"
        );
    }
    let (status, body) = activity_get(&state, "/agents").await;
    assert_eq!(status, StatusCode::OK);
    let listed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let sessions: Vec<&str> = listed["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|agent| agent["session_id"].as_str())
        .collect();
    assert_eq!(sessions, vec![running.to_string().as_str()]);
    let reaped = state.coordinator.sweep_stale_sessions_detailed().unwrap();
    assert_eq!(
        reaped
            .iter()
            .map(|(session, _)| session.session_id)
            .collect::<Vec<_>>(),
        vec![killed]
    );
    assert!(state.coordinator.get_session(&running).unwrap().is_some());
}

async fn activity_report_edit(
    state: &Arc<DaemonState>,
    unit: &std::path::Path,
    old_text: &str,
    new_text: &str,
) -> serde_json::Value {
    activity_edit_hook(state, unit, old_text, new_text, "after").await
}

async fn activity_edit_hook(
    state: &Arc<DaemonState>,
    unit: &std::path::Path,
    old_text: &str,
    new_text: &str,
    phase: &str,
) -> serde_json::Value {
    let response = router(Arc::clone(state))
        .oneshot(
            Request::post("/agents/edits?wait=true")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "client": "claude-code",
                        "session": "client-session-1",
                        "tool_use_id": "tool-1",
                        "phase": phase,
                        "unit": unit,
                        "edits": [{"old_text": old_text, "new_text": new_text}],
                    })
                    .to_string(),
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

/// An agent edits one function with its own tool and its client reports the
/// edit. A person then edits the function beside it and nobody reports that.
/// The agent is named on the one entity its edit changed, a report that is not
/// what happened confirms nothing, and the guide of uncommitted work says so
/// step by step, with no place on disk in anything kept or answered.
#[tokio::test]
#[serial_test::serial]
async fn an_edit_a_client_reports_names_the_agent_on_the_entity_it_changed_and_no_other() {
    let (dir, state, source) = source_base_fixture().await;
    state
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    // The agent is connected, so admission steps are noted from the start.
    let connected = activity_open_session(&state).await;
    let unit = dir.path().join("src/value.rs");
    let value = source["id"].as_str().unwrap().to_string();

    let edited = SOURCE_BASE_ORIGINAL.replacen("{ 1 }", "{ 2 }", 1);
    let prepared = activity_edit_hook(&state, &unit, "{ 1 }", "{ 2 }", "before").await;
    assert_eq!(prepared["outcome"], "prepared", "{prepared}");
    std::fs::write(&unit, &edited).unwrap();
    crate::loop_runner::sync_filesystem_with_graph(&state)
        .await
        .unwrap();
    let outcome = activity_report_edit(&state, &unit, "{ 1 }", "{ 2 }").await;
    assert_eq!(outcome["outcome"], "attributed", "{outcome}");
    assert_eq!(outcome["entities"], 1, "{outcome}");
    let edits = state.agent_authorship.edits();
    assert_eq!(edits.len(), 1);
    assert_eq!(
        edits[0]
            .entities
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec![value.clone()],
        "the function it edited, and not the one beside it or their module"
    );
    assert_eq!(edits[0].client, "claude-code");
    assert_eq!(edits[0].session, "client-session-1");
    assert_eq!(edits[0].kin_session.as_deref(), Some(connected.as_str()));
    assert!(edits[0].base.is_some() && edits[0].change_id.is_none());

    // The same report again finds its step already claimed.
    let again = activity_report_edit(&state, &unit, "{ 1 }", "{ 2 }").await;
    assert_eq!(again["outcome"], "unconfirmed", "{again}");

    // A person edits the neighbour. A report that is not that edit confirms
    // nothing, and nobody is named on it.
    let prepared = activity_edit_hook(&state, &unit, "{ 8 }", "{ 9 }", "before").await;
    assert_eq!(prepared["outcome"], "prepared", "{prepared}");
    std::fs::write(&unit, edited.replacen("{ 8 }", "{ 9 }", 1)).unwrap();
    crate::loop_runner::sync_filesystem_with_graph(&state)
        .await
        .unwrap();
    let wrong = activity_report_edit(&state, &unit, "{ 8 }", "{ 7 }").await;
    assert_eq!(wrong["outcome"], "unconfirmed", "{wrong}");
    assert_eq!(state.agent_authorship.edits().len(), 1);

    let elsewhere = activity_report_edit(
        &state,
        std::path::Path::new("/nowhere/kin/holds/value.rs"),
        "a",
        "b",
    )
    .await;
    assert_eq!(elsewhere["outcome"], "not_this_repository", "{elsewhere}");

    let response = router(Arc::clone(&state))
        .oneshot(
            Request::post("/review")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"op": "guide", "json": true, "working": true}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let answered: kin_cli::commands::review::ReviewResponse =
        serde_json::from_slice(&body).unwrap();
    let guide: serde_json::Value = serde_json::from_str(&answered.json.unwrap()).unwrap();
    let steps: Vec<&serde_json::Value> = guide["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|chapter| chapter["steps"].as_array().unwrap())
        .collect();
    let step = |name: &str| {
        *steps
            .iter()
            .find(|step| step["name"] == name)
            .unwrap_or_else(|| panic!("a step for {name} in {guide}"))
    };
    assert_eq!(
        step("value")["authored_by"],
        serde_json::json!([{
            "name": "Claude Code",
            "session": "client-session-1",
            "at": edits[0].at,
            "through": "client",
            "kin_session": connected,
        }])
    );
    assert!(
        step("sibling").get("authored_by").is_none(),
        "changed by a person: nothing is said"
    );
    assert_eq!(
        guide["summary"]["authors"],
        serde_json::json!([{
            "name": "Claude Code",
            "session": "client-session-1",
            "entities": 1,
            "through": "client",
            "kin_session": connected,
        }])
    );
    // What authority keeps of the edit, once it is written there.
    use kin_model::ProvenanceStore as _;
    assert!(crate::agent_authorship::settle(&state, true).await >= 1);
    let kept: Vec<kin_model::AgentSessionRecord> = state
        .graph
        .list_actors()
        .unwrap()
        .into_iter()
        .filter_map(|actor| actor.agent_session)
        .collect();
    assert_eq!(kept.len(), 1, "one record, for the client's session");
    let kept = serde_json::to_string(&kept).unwrap();
    assert!(
        !kept.contains("value.rs") && !kept.contains("{ 2 }"),
        "neither the place nor the text of the edit is kept: {kept}"
    );
}

/// A paired Write that keeps the current content cannot take ownership of
/// an earlier person's identical resulting content, even while that human
/// admission step is still retained.
#[tokio::test]
#[serial_test::serial]
async fn a_no_op_client_write_never_claims_an_earlier_human_edit() {
    let (dir, state, _) = source_base_fixture().await;
    state.is_initialized.store(true, std::sync::atomic::Ordering::Relaxed);
    let unit = dir.path().join("src/value.rs");
    let content = SOURCE_BASE_ORIGINAL.replacen("{ 1 }", "{ 3 }", 1);
    std::fs::write(&unit, &content).unwrap();
    crate::loop_runner::sync_filesystem_with_graph(&state).await.unwrap();
    let mut claim = crate::client_edits::ClientEditClaim {
        client: "claude-code".into(), session: "client-session-write".into(),
        tool_use_id: Some("write-1".into()), phase: "before".into(),
        unit: unit.to_str().unwrap().into(), edits: Vec::new(), content: Some(content.clone()),
    };
    let prepared = crate::client_edits::resolve(&state, claim.clone()).await;
    assert_eq!(prepared.outcome, "prepared");
    std::fs::write(&unit, &content).unwrap();
    crate::loop_runner::sync_filesystem_with_graph(&state).await.unwrap();
    claim.phase = "after".into();
    let outcome = crate::client_edits::resolve(&state, claim).await;
    assert_eq!(outcome.outcome, "unconfirmed");
    assert!(state.agent_authorship.edits().is_empty());
}

/// A post hook delivered after a commit keeps the admission's original
/// base and points at the actual published change, rather than attributing
/// already committed work to the successor's uncommitted overlay.
#[tokio::test]
#[serial_test::serial]
async fn a_delayed_client_post_hook_attributes_the_change_that_recorded_it() {
    let (dir, state, _) = source_base_fixture().await;
    state.is_initialized.store(true, std::sync::atomic::Ordering::Relaxed);
    let unit = dir.path().join("src/value.rs");
    let original_base = crate::unit_lifecycle::workspace_base_change(&state).unwrap();
    let prepared = activity_edit_hook(&state, &unit, "{ 1 }", "{ 4 }", "before").await;
    assert_eq!(prepared["outcome"], "prepared", "{prepared}");
    std::fs::write(&unit, SOURCE_BASE_ORIGINAL.replacen("{ 1 }", "{ 4 }", 1)).unwrap();
    crate::loop_runner::sync_filesystem_with_graph(&state).await.unwrap();
    let change = commit_through_api(&router(Arc::clone(&state)), kin_model::OperationId::new(), "Record the paired edit").await;
    let outcome = activity_report_edit(&state, &unit, "{ 1 }", "{ 4 }").await;
    assert_eq!(outcome["outcome"], "attributed", "{outcome}");
    let edits = state.agent_authorship.edits();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].base, Some(original_base.to_string()));
    assert_eq!(edits[0].change_id, Some(change.to_string()));
}

/// An amend preserves the replaced change's parents, so attribution seals
/// against the publication receipt's previous workspace base instead.
#[tokio::test]
#[serial_test::serial]
async fn a_client_edit_seals_against_the_replaced_head_when_amended() {
    let (dir, state, _) = source_base_fixture().await;
    state.is_initialized.store(true, std::sync::atomic::Ordering::Relaxed);
    let unit = dir.path().join("src/value.rs");
    let replaced = crate::unit_lifecycle::workspace_base_change(&state).unwrap();
    let prepared = activity_edit_hook(&state, &unit, "{ 1 }", "{ 6 }", "before").await;
    assert_eq!(prepared["outcome"], "prepared", "{prepared}");
    std::fs::write(&unit, SOURCE_BASE_ORIGINAL.replacen("{ 1 }", "{ 6 }", 1)).unwrap();
    crate::loop_runner::sync_filesystem_with_graph(&state).await.unwrap();
    let outcome = activity_report_edit(&state, &unit, "{ 1 }", "{ 6 }").await;
    assert_eq!(outcome["outcome"], "attributed", "{outcome}");
    let (status, body) = amend_request_through_api(&router(Arc::clone(&state)), replaced, Some("Amend the paired edit")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let amended = crate::unit_lifecycle::workspace_base_change(&state).unwrap();
    assert_ne!(amended, replaced);
    assert_eq!(state.agent_authorship.edits()[0].base, Some(replaced.to_string()));
    assert_eq!(state.agent_authorship.edits()[0].change_id, Some(amended.to_string()));
}

/// The absent head of a new native repository is a real base. Its first
/// commit seals paired edits exactly like a commit whose parent exists.
#[tokio::test]
#[serial_test::serial]
async fn a_client_edit_before_the_root_commit_keeps_its_authorship() {
    let dir = tempfile::tempdir().unwrap();
    let initialized = kin_core::init(dir.path()).unwrap();
    let state = Arc::new(DaemonState::open(initialized.layout).unwrap());
    state.is_initialized.store(true, std::sync::atomic::Ordering::Relaxed);
    let unit = dir.path().join("value.rs");
    let mut claim = crate::client_edits::ClientEditClaim {
        client: "claude-code".into(), session: "client-session-root".into(),
        tool_use_id: Some("write-root".into()), phase: "before".into(),
        unit: unit.to_str().unwrap().into(), edits: Vec::new(),
        content: Some("pub fn root_value() -> u32 { 5 }\n".into()),
    };
    let prepared = crate::client_edits::resolve(&state, claim.clone()).await;
    assert_eq!(prepared.outcome, "prepared");
    std::fs::write(&unit, claim.content.as_ref().unwrap()).unwrap();
    crate::loop_runner::sync_filesystem_with_graph(&state).await.unwrap();
    claim.phase = "after".into();
    let outcome = crate::client_edits::resolve(&state, claim).await;
    assert_eq!(outcome.outcome, "attributed");
    // The Write made the whole unit: its module and the function in it.
    assert_eq!(outcome.entities, 2);
    assert!(state.agent_authorship.edits()[0].base.is_none());
    let change = commit_through_api(&router(Arc::clone(&state)), kin_model::OperationId::new(), "Record the first client edit").await;
    assert_eq!(state.agent_authorship.edits()[0].change_id, Some(change.to_string()));
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
