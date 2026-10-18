// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

async fn entity_lease_test_call(
    state: &Arc<DaemonState>,
    session: &str,
    args: serde_json::Value,
    authenticated: bool,
    write: bool,
) -> kin_mcp::ToolCallResult {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("X-Kin-Session", session.parse().unwrap());
    crate::mcp_entity_leases::call(
        Arc::clone(state),
        headers,
        serde_json::from_value(args).unwrap(),
        authenticated,
        write,
    )
    .await
}

#[tokio::test]
#[serial_test::serial]
async fn entity_lease_authenticated_control_and_claimed_edit_preserve_source_cas_and_replay() {
    let (_dir, state, source) = source_base_fixture().await;
    let owner = mcp_test_session(&state);
    let foreign = mcp_test_session(&state);
    let preflight = mutate_http(
        &state,
        "kin_entity_leases",
        serde_json::json!({"entity_ids":[source["id"]]}),
        &owner,
    )
    .await;
    assert_ne!(
        preflight.is_error,
        Some(true),
        "{}",
        mcp_result_text(&preflight)
    );
    let preflight = tool_result_payload(&preflight);
    assert_eq!(
        preflight["leases"][0]["source_hash"],
        source["source_base"]["body_hash"]
    );
    assert!(preflight["leases"][0]["lease"].is_null());
    let acquire = serde_json::json!({"request_id":"lease-value", "mutation":{
        "operation":"acquire","entity_id":source["id"],
        "expected_source_hash":source["source_base"]["body_hash"]}});
    let denied = entity_lease_test_call(&state, &owner, acquire.clone(), false, true).await;
    assert_eq!(denied.is_error, Some(true));
    let issued = mutate_http(&state, "kin_entity_lease", acquire.clone(), &owner).await;
    assert_ne!(issued.is_error, Some(true), "{}", mcp_result_text(&issued));
    let issued = tool_result_payload(&issued);
    let claim = issued["lease"]["claim"].clone();
    assert!(claim["fence"].is_string(), "{issued}");
    assert_eq!(issued["lease"]["session_id"], owner);
    let replay = entity_lease_test_call(&state, &owner, acquire.clone(), true, true).await;
    let replay = tool_result_payload(&replay);
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["lease"], issued["lease"]);
    assert_eq!(replay["receipt"], issued["receipt"]);

    let before = source_base_roots(&state);
    for (session, request, claims) in [
        (&owner, "unclaimed", serde_json::Value::Null),
        (&foreign, "foreign", serde_json::json!([claim.clone()])),
    ] {
        let mut request = serde_json::json!({"session_id":session,"request_id":request,
            "operations":[source_base_operation(&source)]});
        if !claims.is_null() {
            request["entity_edit_lease_claims"] = claims;
        }
        let denied = private_source_base_mutate(&state, "kin_mutate", request, session).await;
        assert_eq!(denied.is_error, Some(true), "{}", mcp_result_text(&denied));
        assert_eq!(source_base_roots(&state), before);
    }
    let public = mcp_call(
        router(Arc::clone(&state)),
        "get_entity_source",
        serde_json::json!({"entity_id":source["id"]}),
    )
    .await;
    assert_ne!(public.is_error, Some(true), "{}", mcp_result_text(&public));
    let public_source = tool_result_payload(&public);
    let request = serde_json::json!({"session_id":owner,"request_id":"claimed",
        "entity_edit_lease_claims":[claim.clone()],"operations":[source_base_operation(&public_source)]});
    let edited = mutate_http(&state, "kin_mutate", request.clone(), &owner).await;
    assert_ne!(edited.is_error, Some(true), "{}", mcp_result_text(&edited));
    let receipt = mutation_receipt(&edited);
    let current = source_base_read(&state, &source["id"]).await;
    assert_eq!(current["body"], SOURCE_BASE_EDIT);
    let status = entity_lease_test_call(
        &state,
        &owner,
        serde_json::json!({"entity_ids":[source["id"]]}),
        true,
        false,
    )
    .await;
    assert_ne!(status.is_error, Some(true), "{}", mcp_result_text(&status));

    let release = serde_json::json!({"request_id":"release-value","mutation":{
        "operation":"release","entity_id":source["id"],"authority_id":claim["authority_id"],"fence":claim["fence"]}});
    let released = entity_lease_test_call(&state, &owner, release, true, true).await;
    assert_ne!(
        released.is_error,
        Some(true),
        "{}",
        mcp_result_text(&released)
    );
    let roots = source_base_roots(&state);
    let old = mutate_http(&state, "kin_mutate", request, &owner).await;
    assert_eq!(mutation_receipt(&old), receipt);
    assert_eq!(source_base_roots(&state), roots);
    let stale = serde_json::json!({"session_id":owner,"request_id":"stale-claim",
        "entity_edit_lease_claims":[claim],"operations":[source_base_operation(&current)]});
    let stale = private_source_base_mutate(&state, "kin_mutate", stale, &owner).await;
    assert_eq!(stale.is_error, Some(true), "{}", mcp_result_text(&stale));
    assert_eq!(source_base_roots(&state), roots);
}

#[tokio::test]
#[serial_test::serial]
async fn entity_lease_control_refuses_owner_clock_and_request_key_substitution() {
    let (_dir, state, source) = source_base_fixture().await;
    let session = mcp_test_session(&state);
    let args = serde_json::json!({"request_id":"lease-identity","mutation":{
        "operation":"acquire","entity_id":source["id"],
        "expected_source_hash":source["source_base"]["body_hash"]}});
    for field in ["session_id", "expires_at_ms", "ttl_ms", "workspace_id"] {
        let mut bad = args.clone();
        bad["mutation"][field] = serde_json::json!("forged");
        let result = entity_lease_test_call(&state, &session, bad, true, true).await;
        assert_eq!(
            result.is_error,
            Some(true),
            "{field}: {}",
            mcp_result_text(&result)
        );
    }
    let first = entity_lease_test_call(&state, &session, args.clone(), true, true).await;
    assert_ne!(first.is_error, Some(true), "{}", mcp_result_text(&first));
    let roots = source_base_roots(&state);
    let mut changed = args;
    changed["mutation"]["expected_source_hash"] = serde_json::json!("00".repeat(32));
    let changed = entity_lease_test_call(&state, &session, changed, true, true).await;
    assert_eq!(
        changed.is_error,
        Some(true),
        "{}",
        mcp_result_text(&changed)
    );
    assert_eq!(source_base_roots(&state), roots);
}

#[tokio::test]
#[serial_test::serial]
async fn entity_lease_replay_repairs_cursor_without_paying_pending_derivation() {
    let (_dir, state, source) = source_base_fixture().await;
    let owner = mcp_test_session(&state);
    let reader = state
        .coordinator
        .register_session(
            "test",
            "read-only-lease-client",
            SessionTransport::Mcp,
            None,
            state.layout.working_dir().to_path_buf(),
            SessionCapabilities {
                can_read: true,
                ..Default::default()
            },
        )
        .unwrap()
        .to_string();
    let args = serde_json::json!({"request_id":"repair-cursor","mutation":{
        "operation":"acquire","entity_id":source["id"],"expected_source_hash":source["source_base"]["body_hash"]}});
    let before = source_base_roots(&state);
    let denied = entity_lease_test_call(&state, &reader, args.clone(), true, true).await;
    assert_eq!(denied.is_error, Some(true));
    assert_eq!(source_base_roots(&state), before);
    state
        .lsp_evidence_unrecorded
        .lock()
        .unwrap()
        .insert("src/pending.rs".into());
    let first = mutate_http(&state, "kin_entity_lease", args.clone(), &owner).await;
    assert_ne!(first.is_error, Some(true), "{}", mcp_result_text(&first));
    let first = tool_result_payload(&first);
    assert!(state
        .lsp_evidence_unrecorded
        .lock()
        .unwrap()
        .contains("src/pending.rs"));
    let old_generation = first["receipt"]["roots_before"]["generation"]
        .as_u64()
        .unwrap();
    let generation: u64 = first["receipt"]["generation"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    state
        .snapshot_generation
        .store(old_generation, std::sync::atomic::Ordering::SeqCst);
    let replay = mutate_http(&state, "kin_entity_lease", args, &owner).await;
    assert_ne!(replay.is_error, Some(true), "{}", mcp_result_text(&replay));
    let replay = tool_result_payload(&replay);
    assert_eq!(replay["lease"], first["lease"]);
    assert_eq!(replay["receipt"], first["receipt"]);
    assert_eq!(
        state
            .snapshot_generation
            .load(std::sync::atomic::Ordering::SeqCst),
        generation
    );
    assert!(state
        .lsp_evidence_unrecorded
        .lock()
        .unwrap()
        .contains("src/pending.rs"));
    let status = mutate_http(
        &state,
        "kin_entity_leases",
        serde_json::json!({"entity_ids":[source["id"]]}),
        &reader,
    )
    .await;
    assert_ne!(status.is_error, Some(true), "{}", mcp_result_text(&status));
}
