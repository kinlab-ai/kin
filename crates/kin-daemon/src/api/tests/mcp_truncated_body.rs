// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

#[tokio::test]
async fn mcp_inline_commit_truncation_refuses_every_body_shape_before_staging() {
    const QUEUED: &str = "pub fn queued() -> u8 { 0 }\npub fn side() -> u8 { 0 }\n";
    // One seeded store, the original source and the prior-work import, and a
    // fresh copy of it for each body shape.
    let template = SeededStore::cached("inline-truncation", || async {
        let (dir, state) = source_base_seeded().await;
        source_tree_conversion_fixture(&state, serde_json::json!({
            "verb":"create","target":"src/queued.rs","body":QUEUED,"description":"import prior-work fixture"
        })).await;
        (dir, state)
    })
    .await;
    for shape in ["replace", "create", "update", "source_base", "entity"] {
        let (dir, state) = template.open_copy();
        let original_source = source_base_original_read(&state).await;
        let source_read = mcp_call(
            router(Arc::clone(&state)),
            "get_entity_source",
            serde_json::json!({"entity_id":original_source["id"]}),
        )
        .await;
        assert_ne!(
            source_read.is_error,
            Some(true),
            "{}",
            mcp_result_text(&source_read)
        );
        let source = tool_result_payload(&source_read);
        assert!(
            kin_mcp::source_ticket::is_ticket(&source["source_base"]),
            "{source}"
        );
        // The prior work and the valid earlier operation replace whole
        // entities too, so each carries the base of the version it replaces.
        let queued_source = source_base_read_named(&state, "queued").await;
        let side_source = source_base_read_named(&state, "side").await;
        let session = mcp_test_session(&state);
        let owner = SessionId(uuid::Uuid::parse_str(&session).unwrap());
        let tx = mcp_lifecycle_begin(&state, &session).await;
        let queued = source_base_replacement(
            &queued_source,
            "pub fn queued() -> u8 { 1 }",
            "previously acknowledged work",
        );
        let staged = mcp_call_as(
            router(Arc::clone(&state)),
            "kin_transaction_stage",
            serde_json::json!({"transaction_id":tx,"operations":[ticketed_source_operation(queued.clone())]}),
            owner,
        )
        .await;
        assert_ne!(staged.is_error, Some(true), "{}", mcp_result_text(&staged));
        let before_roots = source_base_roots(&state);
        let before = state.mcp_transactions.lock().unwrap()[&tx].clone();
        assert_eq!(
            serde_json::to_value(&before.staged_operations[0]).unwrap()["payload"],
            queued["payload"],
            "public staging must retain the full private source-base witness"
        );
        // This is valid Rust with a clipping marker in a comment. Parsing alone
        // cannot establish that the rest of the requested source was retained.
        let clipped = "pub fn value() -> u8 { 2 }\n// … [truncated] \n\t";
        let mut operation = serde_json::json!({"verb":"update", "target":source["id"],
            "body":clipped,"description":"inline clipped source"});
        match shape {
            "replace" => {
                operation["verb"] = "replace".into();
                operation["target"] = "src/value.rs".into();
            }
            "create" => {
                operation["verb"] = "create".into();
                operation["target"] = "src/clipped.rs".into();
            }
            "source_base" => {
                operation["payload"] =
                    serde_json::json!({"EntitySourceBase":source["source_base"]});
            }
            "entity" => {
                let id = kin_model::EntityId(
                    uuid::Uuid::parse_str(source["id"].as_str().unwrap()).unwrap(),
                );
                let entity = state.graph.get_entity(&id).unwrap().unwrap();
                operation["payload"] = serde_json::json!({"Entity":entity});
            }
            "update" => {}
            _ => unreachable!(),
        }
        let side_private = source_base_replacement(
            &side_source,
            "pub fn side() -> u8 { 1 }",
            "valid earlier inline operation",
        );
        if matches!(shape, "create" | "replace") {
            // Legacy physical targets are intentionally invalid public work.
            // Prove their exact truncation classification privately without
            // requiring public diagnostics to expose the target coordinate.
            let private_operations = vec![
                serde_json::from_value(side_private.clone()).unwrap(),
                serde_json::from_value(operation.clone()).unwrap(),
            ];
            let truncated =
                kin_mcp::handlers::sessions::reject_truncated_bodies(&private_operations)
                    .expect_err("clipped legacy source must fail before any staging");
            assert!(
                truncated.contains("operation #1")
                    && truncated.contains("[truncated]")
                    && truncated.contains("get_entity_source"),
                "{shape}: {truncated}"
            );
        }
        let side = ticketed_source_operation(side_private);
        let result = mcp_call_as(
            router(Arc::clone(&state)),
            "kin_transaction_commit",
            serde_json::json!({"transaction_id":tx,"operations":[side,operation]}),
            owner,
        )
        .await;
        let text = mcp_result_text(&result);
        let current_roots = source_base_roots(&state);
        let pending = state.mcp_transactions.lock().unwrap().get(&tx).cloned();
        assert_eq!(result.is_error, Some(true), "{shape}: {text}");
        if matches!(shape, "create" | "replace") {
            assert!(
                !text.contains(operation["target"].as_str().unwrap()),
                "{shape}: {text}"
            );
        } else {
            assert!(
                text.contains("operation #1")
                    && text.contains("[truncated]")
                    && text.contains("get_entity_source"),
                "{shape}: {text}"
            );
        }
        assert_eq!(current_roots, before_roots, "{shape}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/value.rs")).unwrap(),
            SOURCE_BASE_ORIGINAL
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/queued.rs")).unwrap(),
            QUEUED
        );
        for path in ["src/side.rs", "src/clipped.rs"] {
            assert!(!dir.path().join(path).exists(), "{shape}: published {path}");
        }
        let pending = pending.unwrap();
        assert_eq!(pending.state, before.state);
        assert!(pending.commit_payload_hash.is_none());
        assert_eq!(
            serde_json::to_value(&pending.staged_operations).unwrap(),
            serde_json::to_value(&before.staged_operations).unwrap()
        );
        let reopened = DaemonState::open(state.layout.clone()).unwrap();
        assert_eq!(source_base_roots(&reopened), before_roots);
        assert_eq!(
            serde_json::to_value(&reopened.mcp_transactions.lock().unwrap()[&tx].staged_operations)
                .unwrap(),
            serde_json::to_value(&before.staged_operations).unwrap()
        );
        drop(reopened);
        // A corrected retry of the same transaction retains prior work and
        // stages each new operation once; the rejected batch added nothing.
        operation["body"] = "pub fn value() -> u8 { 2 }\n".into();
        // Removing the clipping marker never grants file CRUD authority.
        if matches!(shape, "create" | "replace") {
            let refused = mcp_call_as(
                router(Arc::clone(&state)),
                "kin_transaction_commit",
                serde_json::json!({"transaction_id":tx,"operations":[side,operation]}),
                owner,
            )
            .await;
            assert_eq!(refused.is_error, Some(true));
            assert!(mcp_result_text(&refused).contains("semantic_operation_required"));
            assert_eq!(source_base_roots(&state), before_roots);
            operation["verb"] = "update".into();
            operation["target"] = source["id"].clone();
        }
        // Nor does it admit a whole-entity replacement that names no version
        // of the entity. An update with no payload, or an Entity payload with a
        // body, is refused before staging until it carries the base it read.
        if shape != "source_base" {
            let refused = mcp_call_as(
                router(Arc::clone(&state)),
                "kin_transaction_commit",
                serde_json::json!({"transaction_id":tx,"operations":[side,operation]}),
                owner,
            )
            .await;
            assert_eq!(
                refused.is_error,
                Some(true),
                "{shape}: {}",
                mcp_result_text(&refused)
            );
            assert!(
                mcp_result_text(&refused).contains("source_base_required:"),
                "{shape}: {}",
                mcp_result_text(&refused)
            );
            assert_eq!(source_base_roots(&state), before_roots, "{shape}");
            assert_eq!(
                serde_json::to_value(
                    &state.mcp_transactions.lock().unwrap()[&tx].staged_operations
                )
                .unwrap(),
                serde_json::to_value(&before.staged_operations).unwrap(),
                "{shape}"
            );
            operation["payload"] = serde_json::json!({"EntitySourceBase":source["source_base"]});
        }
        let retry = mcp_call_as(
            router(Arc::clone(&state)),
            "kin_transaction_commit",
            serde_json::json!({"transaction_id":tx,"operations":[side,operation]}),
            owner,
        )
        .await;
        assert_ne!(
            retry.is_error,
            Some(true),
            "{shape}: {}",
            mcp_result_text(&retry)
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/queued.rs")).unwrap(),
            "pub fn queued() -> u8 { 1 }\npub fn side() -> u8 { 1 }\n"
        );
        assert!(std::fs::read_to_string(dir.path().join("src/value.rs"))
            .unwrap()
            .contains("{ 2 }"));
        assert!(!dir.path().join("src/clipped.rs").exists());
    }
}

#[tokio::test]
async fn mcp_inline_commit_complete_body_may_contain_marker_and_survives_reopen() {
    let (dir, state, source) = source_base_fixture().await;
    let complete = "pub fn value() -> &'static str { \"[truncated]\" }\n";
    source_base_commit_operation(
        &state,
        source_base_replacement(&source, complete.trim_end(), "complete marker literal"),
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/value.rs")).unwrap(),
        format!("{complete}pub fn sibling() -> u8 {{ 8 }}\n")
    );
    let reopened = Arc::new(DaemonState::open(state.layout.clone()).unwrap());
    reopened
        .is_initialized
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let entity = reopened
        .graph
        .query_entities(&kin_model::EntityFilter {
            name_pattern: Some("value".into()),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .find(|e| e.name == "value" && e.kind != kin_model::EntityKind::Module)
        .unwrap();
    let result = mcp_call(
        router(reopened),
        "get_entity_source",
        serde_json::json!({"entity_id":entity.id.to_string()}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{}", mcp_result_text(&result));
    assert_eq!(tool_result_payload(&result)["body"], complete.trim_end());
}
