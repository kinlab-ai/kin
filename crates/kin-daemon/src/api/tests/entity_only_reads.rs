// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

#[tokio::test]
async fn entity_only_reads_refuse_file_reader_and_file_granularity_at_daemon_boundary() {
    let state = test_state();
    for (tool,args) in [
        ("kin_artifact_read",json!({"path":"README.md"})),
        ("semantic_locate",json!({"query":"configuration","granularity":"file","pipeline":"fused"})),
        ("semantic_locate",json!({"query":"configuration","granularity":"file","pipeline":"cosine"})),
    ] {
        let result = mcp_call(router(Arc::clone(&state)),tool,args).await;
        assert_eq!(result.is_error,Some(true),"{tool}: {}",mcp_result_text(&result));
        assert!(!mcp_result_text(&result).contains("content_base64"));
    }
}

#[tokio::test]
async fn entity_only_reads_daemon_refuses_imported_module_surrogates_and_keeps_function_body() {
    let (_dir, state, source) = source_base_fixture().await;
    let wrapper = state
        .graph
        .query_entities(&kin_model::EntityFilter::default())
        .unwrap()
        .into_iter()
        .find(kin_model::is_file_module_surface)
        .expect("real parser emitted a file module");
    for tool in ["get_entity_source", "get_entity_body"] {
        let result = mcp_call(
            router(Arc::clone(&state)),
            tool,
            json!({"entity_id":wrapper.id}),
        )
        .await;
        assert_ne!(
            result.is_error,
            Some(true),
            "{tool}: {}",
            mcp_result_text(&result)
        );
        let payload = tool_result_payload(&result);
        assert!(payload["body"].is_null(), "{payload}");
        assert!(payload["source_base"].is_null(), "{payload}");
        assert_eq!(
            payload["body_omitted"],
            kin_mcp::container_shape::CONTAINER_SHAPE_REASON
        );
        // The unit's module contains its function, so its members are
        // recorded and no coverage gap stands in for them.
        assert!(payload["semantic_gap"].is_null(), "{payload}");
        assert_eq!(payload["member_coverage"], "recorded", "{payload}");
        assert!(!mcp_result_text(&result).contains(SOURCE_BASE_ORIGINAL));
    }
    let result = mcp_call(
        router(Arc::clone(&state)),
        "get_entity_sources",
        json!({"entity_ids":[wrapper.id,source["id"]]}),
    )
    .await;
    let text = mcp_result_text(&result);
    assert!(
        !text.contains(kin_mcp::container_shape::MODULE_MEMBER_GAP),
        "{text}"
    );
    assert!(
        !text.contains(SOURCE_BASE_ORIGINAL),
        "must not read a whole-file surrogate"
    );
    assert!(
        text.contains("pub fn value() -> u8 { 1 }"),
        "real entity source remains available"
    );
    let context = mcp_call(
        router(Arc::clone(&state)),
        "get_context_pack",
        json!({"entity_id":wrapper.id,"compact":true}),
    )
    .await;
    let body = tool_result_payload(&context);
    let focal = &body["focal_entity"];
    assert_eq!(focal["id"], wrapper.id.to_string(), "{body}");
    assert!(focal["body"].is_null(), "{body}");
    assert_eq!(focal["body_complete"], false, "{body}");
    assert_eq!(focal["projection"], "SignatureOnly", "{body}");
    assert_eq!(focal["projection_downgraded_from"], "FullBody", "{body}");
    let gap = focal["body_unavailable"].as_str().unwrap();
    assert!(gap.starts_with("module_body_unavailable:"), "{body}");
    assert!(gap.contains("recorded member relationships"), "{body}");
    assert!(
        gap.contains("Missing member coverage is a semantic gap"),
        "{body}"
    );
    assert!(focal["source_base"].is_null(), "{body}");
    for key in [
        "file",
        "file_path",
        "read_path",
        "span",
        "start_line",
        "end_line",
        "start_col",
        "end_col",
    ] {
        assert!(focal.get(key).is_none(), "{key}: {body}");
    }
    if let Some(origin) = wrapper.file_origin.as_ref() {
        assert!(!gap.contains(&origin.0), "{gap}");
    }
    assert!(
        !mcp_result_text(&context).contains(SOURCE_BASE_ORIGINAL),
        "{body}"
    );
    assert_eq!(
        entity_patch_read(&state, &source["id"]).await["body"],
        "pub fn value() -> u8 { 1 }"
    );
}

#[test]
fn entity_only_locate_filters_before_paging_without_losing_surviving_rows() {
    let mut artifact = fused_locate_entity("README.md");
    artifact.entity_id.clear();
    artifact.id_space = kin_cli::commands::locate::LocateIdSpace::Artifact;
    artifact.artifact_path = Some("README.md".into());
    let mut first = fused_locate_entity("first");
    first.entity_id = "00000000-0000-0000-0000-000000000001".into();
    let mut last = fused_locate_entity("last");
    last.entity_id = "00000000-0000-0000-0000-000000000002".into();
    let mut result = kin_cli::commands::locate::LocateResult {entities:vec![artifact,first,last],total_ranked:3,..Default::default()};
    retain_semantic_locate_entities(&mut result);
    assert_eq!(result.entities.len(),2);
    assert_eq!(result.total_ranked,2);
    let mut page0: kin_cli::commands::locate::LocateResult = serde_json::from_value(serde_json::to_value(&result).unwrap()).unwrap();
    kin_cli::commands::locate::apply_entity_page(&mut page0,"entity-only",0,1);
    assert_eq!(page0.entities[0].name,"first");
    assert!(page0.next_cursor.is_some());
    let mut page1 = result;
    kin_cli::commands::locate::apply_entity_page(&mut page1,"entity-only",1,1);
    assert_eq!(page1.entities[0].name,"last");
    assert!(page1.next_cursor.is_none());
    assert!(page1.degradations.iter().any(|d| d.reason == "non_entity_candidates_omitted"));
}

/// The omission is by design on every semantic query that ranks a non-entity
/// row, so it is published as a note: visible, and not a reason to refuse.
#[test]
fn entity_only_locate_notes_its_omission_without_refusing_the_verdict() {
    let mut artifact = fused_locate_entity("README.md");
    artifact.entity_id.clear();
    artifact.id_space = kin_cli::commands::locate::LocateIdSpace::Artifact;
    artifact.artifact_path = Some("README.md".into());
    for surface in [
        kin_cli::commands::locate_compact::LocateSurface::Full,
        kin_cli::commands::locate_compact::LocateSurface::Compact,
    ] {
        let result = kin_cli::commands::locate::LocateResult {
            entities: vec![artifact.clone(), fused_locate_entity("start_daemon")],
            total_ranked: 2,
            ..Default::default()
        };
        let raw = fused_semantic_locate_payload(result, "start_daemon", false, false, surface);
        let finalized =
            kin_mcp::envelope::finalize(raw, kin_mcp::Envelope::daemon(), "semantic_locate");
        let body = tool_result_payload(&finalized);
        let entry = body["degradations"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|entry| entry["reason"] == "non_entity_candidates_omitted")
            .unwrap_or_else(|| panic!("the omission stays visible: {body}"));
        assert_eq!(entry["severity"], "note", "{body}");
        assert_eq!(
            body["_kin"]["verdict"]["inputs"]["degradations"], "certified",
            "a policy note must not refuse the verdict: {body}"
        );
    }
}

#[test]
fn entity_only_locate_cannot_fall_back_to_files_when_entity_projection_is_empty() {
    let mut result = compact_surface_fixture(1);
    assert!(!result.files.is_empty());
    result.entities.clear();
    retain_semantic_locate_entities(&mut result);
    assert!(result.files.is_empty());
    assert!(result.degradations.iter().any(|d|d.reason == "non_entity_candidates_omitted"));
}

#[tokio::test]
async fn module_source_span_daemon_refuses_legacy_sibling_body_surrogate() {
    let (_dir,state) = mcp_lifecycle_fixture();
    let source = "pub mod defaults { pub fn inside() {} }\npub fn outside() { sibling_only(); }\n";
    source_tree_conversion_fixture(&state,json!({"verb":"create","target":"src/defaults.rs","body":source,"description":"module declaration fixture"})).await;
    let module = state.graph.query_entities(&kin_model::EntityFilter::default()).unwrap().into_iter()
        .find(|e|e.kind == kin_model::EntityKind::Module && e.name == "defaults").unwrap();
    let result = mcp_call(router(Arc::clone(&state)),"get_entity_source",json!({"entity_id":module.id})).await;
    assert_ne!(result.is_error,Some(true),"{}",mcp_result_text(&result));
    let payload = tool_result_payload(&result);
    assert!(payload["body"].is_null(), "{payload}");
    assert!(payload["source_base"].is_null(), "{payload}");
    assert_eq!(payload["semantic_gap"], kin_mcp::container_shape::MODULE_MEMBER_GAP);
    assert_eq!(source_base_read_named(&state, "inside").await["body"], "pub fn inside() {}");
    let mut legacy = module.clone();
    legacy.span.as_mut().unwrap().end_byte = source.len();
    legacy.span.as_mut().unwrap().end_line = 2;
    legacy.span.as_mut().unwrap().end_col = 0;
    state.graph.upsert_entity(&legacy).unwrap();
    for tool in ["get_entity_source","get_entity_body"] {
        let result = mcp_call(router(Arc::clone(&state)),tool,json!({"entity_id":module.id})).await;
        assert_ne!(result.is_error,Some(true),"{tool}: {}",mcp_result_text(&result));
        let payload = tool_result_payload(&result);
        assert!(payload["body"].is_null(), "{payload}");
        assert!(payload["source_base"].is_null(), "{payload}");
        assert_eq!(payload["semantic_gap"], kin_mcp::container_shape::MODULE_MEMBER_GAP);
        assert!(!mcp_result_text(&result).contains("sibling_only"));
    }
    for (tool,args) in [
        ("get_entity_sources",json!({"entity_ids":[module.id]})),
        ("get_context_pack",json!({"entity_id":module.id,"compact":true})),
    ] {
        let result = mcp_call(router(Arc::clone(&state)),tool,args).await;
        assert!(!mcp_result_text(&result).contains("sibling_only"),"{tool}: {}",mcp_result_text(&result));
        assert!(mcp_result_text(&result).contains(kin_mcp::container_shape::MODULE_MEMBER_GAP),"{tool}: {}",mcp_result_text(&result));
    }
    assert_eq!(serde_json::to_value(state.graph.get_entity(&module.id).unwrap().unwrap()).unwrap(),serde_json::to_value(&legacy).unwrap());
}
