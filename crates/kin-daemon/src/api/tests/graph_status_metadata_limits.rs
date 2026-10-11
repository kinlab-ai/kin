// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

#[tokio::test]
async fn public_graph_status_preserves_the_producer_verdict_without_private_inventory() {
    let state = test_state_with_committed_sources(&[("src/lib.py", "def caller():\n    return 1\n")]);
    let result = mcp_call(
        router(Arc::clone(&state)),
        "kin_graph_status",
        json!({"max_chars": 60000}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    let text = mcp_result_text(&result);
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["_kin"]["semantic_surface"], 1);
    assert_eq!(body["scope"], "head");
    assert_eq!(body["entity_count"], state.graph.entity_count());
    assert_eq!(body["_kin"]["graph_state"]["entity_count"], body["entity_count"]);
    assert_eq!(body["_kin"]["semantic_coverage"]["total"], body["embeddings_total"]);
    assert_eq!(body["_kin"]["verdict"]["safe_to_conclude_absent"], false);
    assert!(body["_kin"]["repository"]["authority_context"].is_string());
    assert!(body.get("source_derivation").is_none());
    assert!(body["enrichment"].get("files").is_none());
    assert!(body["_kin"]["repository"].get("root").is_none());
    assert!(!text.contains("src/lib.py"));
    assert!(!text.contains(&state.layout.working_dir().display().to_string()));

    // A later transport observation may describe an entirely different HEAD.
    // The forwarded semantic report must retain its selected count and verdict.
    let unrelated = kin_mcp::Envelope::daemon().with_health(&json!({
        "graph_entity_count":99999, "graph_loaded":true,
        "graph_generation":99999, "embed_worker_failed":true,
    }));
    let accepted = kin_mcp::server::finalize_daemon_graph_status(
        result.clone(), unrelated, &[], u64::MAX,
    );
    assert_eq!(serde_json::to_value(accepted).unwrap(), serde_json::to_value(result).unwrap());
}

#[tokio::test]
async fn graph_status_cli_pages_include_local_decorations_without_losing_dependency_rows() {
    let state = test_state_with_committed_sources(&[("src/lib.py", "def caller():\n    return 1\n")]);
    let app = router(Arc::clone(&state));
    let dependencies = std::iter::once("src/lib.py".to_owned())
        .chain((0..48).map(|index| format!("missing/{index:03}.py")))
        .collect::<Vec<_>>();
    let mut cursor = None::<String>;
    let mut rows = Vec::new();
    let mut snapshot = None;
    let mut pages = 0;
    loop {
        let response = app.clone().oneshot(
            Request::post("/commands/graph")
                .header("content-type", "application/json")
                .body(Body::from(json!({
                    "command":"status_detailed", "dependencies":dependencies,
                    "cursor":cursor, "max_chars":60000, "response_overhead_bytes":40000,
                }).to_string())).unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 60_000).await.unwrap();
        assert!(bytes.len() + 40_000 <= 60_000, "{} bytes before local decoration", bytes.len());
        let cli: kin_cli::commands::graph::GraphCommandResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(cli.reference_edge_coverage.is_some());
        assert!(cli.relation_census.is_some());
        assert!(cli.graph_section.is_some());
        assert!(cli.call_sites.is_some());
        assert!(cli.lines.iter().any(|line|line.contains("neither attests every relationship or safe absence")));
        let detail = cli.enrichment.unwrap();
        assert_eq!(detail["all_relationships_attested"], false);
        assert_eq!(detail["page"]["max_chars"], 60000);
        assert_eq!(detail["page"]["start"], rows.len());
        assert_eq!(detail["page"]["total"], 49);
        let id = detail["snapshot_id"].clone();
        assert_eq!(snapshot.get_or_insert(id.clone()), &id);
        let files = detail["files"].as_array().unwrap();
        assert!(!files.is_empty());
        rows.extend(files.iter().cloned());
        pages += 1;
        cursor = detail["page"]["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() { break; }
    }
    assert!(pages > 1);
    assert_eq!(rows.len(), 49);
    let observed = rows.iter().map(|row|row["projection_path"].clone()).collect::<Vec<_>>();
    for dependency in dependencies {
        let path = json!(kin_model::RepoPath::from_utf8(dependency).unwrap());
        assert_eq!(observed.iter().filter(|observed|**observed == path).count(), 1);
    }
    assert_eq!(rows.iter().filter(|row| row["admitted"] == false).count(), 48);
    for missing in rows.iter().filter(|row| row["admitted"] == false) {
        assert_eq!(missing["proof"], "unverified");
        assert_eq!(missing["source_reason"], "not_admitted");
    }
}

#[tokio::test]
async fn enrichment_status_detail_limit_preserves_fenced_counters_and_targeted_rows() {
    let mut state = test_state();
    let mut snapshot = state.graph.to_snapshot();
    // This synthetic larger inventory is not the authority snapshot whose
    // exact runtime binding-history capability the fixture opened with.
    snapshot.verified_binding_history = None;
    let entity = test_entity("status_target", "src/target.py");
    snapshot.entities.insert(entity.id, entity);
    snapshot.resolved_tree = kin_model::ResolvedTree::from_artifacts(
        std::iter::once("src/target.py".to_owned())
            .chain((0..10_000).map(|index| format!("unrelated/{index:05}.py")))
            .map(|path| {
                kin_model::ResolvedArtifact::new(
                    kin_model::ArtifactId::new(),
                    kin_model::RepoPath::from_utf8(path).unwrap(),
                    kin_model::TreeEntry::blob(kin_model::Hash256::from_bytes([3; 32]), false),
                )
            }),
    )
    .unwrap();
    Arc::get_mut(&mut state).unwrap().graph =
        Arc::new(kin_db::InMemoryGraph::from_snapshot_without_text_index(snapshot).unwrap());
    let graph = Arc::clone(&state.graph);
    let calls = std::cell::Cell::new(0);
    let result = mcp_graph_status_snapshot_after_capture(
        &state,
        None,
        &graph,
        RequestGraphAuthority::Head,
        kin_mcp::handlers::entities::GraphStatusScope::Head,
        &kin_mcp::status_pages::StatusRequest::default(),
        |_| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                graph
                    .upsert_entity(&test_entity("later", "src/later.py"))
                    .unwrap();
            }
        },
    )
    .await
    .unwrap();
    assert!(
        calls.get() >= 2,
        "limit disclosure must still cross the truth fence"
    );
    let report = parse_graph_status(&result);
    assert_eq!(report.entity_count, graph.entity_count());
    assert_eq!(report.entity_count, 2);
    assert!(!report.completion_attested);
    assert!(
        report.call_sites.is_none(),
        "no partial global tally on a failed scan"
    );
    let detail = report.enrichment.unwrap();
    assert_eq!(detail["unavailable"]["limit_kind"], "bytes");
    assert_eq!(detail["unavailable"]["limit"], 8 * 1024 * 1024);
    assert_eq!(detail["truth_epoch"], graph.truth_epoch());
    assert!(detail.get("page").is_none());

    let app = router(Arc::clone(&state));
    let raw = mcp_call(app.clone(), "kin_graph_status", json!({"max_chars":60000})).await;
    assert_ne!(raw.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&mcp_result_text(&raw)).unwrap();
    assert_eq!(body["entity_count"], 2);
    assert_eq!(body["enrichment"]["status"], "bounded");
    assert_eq!(body["_kin"]["semantic_surface"], 1);
    assert_eq!(body["scope"], "head");
    assert_eq!(body["enrichment"]["scope"]["kind"], "workspace_head");
    assert_eq!(body["_kin"]["verdict"]["state"], "inconclusive");
    assert_eq!(
        body["_kin"]["verdict"]["inputs"]["enrichment"],
        "inconclusive"
    );
    assert_eq!(body["_kin"]["verdict"]["safe_to_conclude_absent"], false);
    assert!(body["enrichment"].get("page").is_none());
    assert!(body["enrichment"].get("files").is_none());
    assert!(body.get("source_derivation").is_none());
    assert!(body["_kin"].get("source_derivation").is_none());
    assert_eq!(body["binding_coverage"], body["_kin"]["binding_coverage"]);
    assert!(matches!(
        body["binding_coverage"]["scope"].as_str(),
        Some("live_head" | "selected_scope_unavailable")
    ));

    let request = kin_mcp::status_pages::StatusRequest {
        dependencies: vec!["src/target.py".into(), "missing.py".into()],
        ..Default::default()
    };
    let selected_private = mcp_graph_status_snapshot(
        &state,
        None,
        &graph,
        RequestGraphAuthority::Head,
        kin_mcp::handlers::entities::GraphStatusScope::Head,
        &request,
    )
    .await
    .unwrap();
    let selected_private = parse_graph_status(&selected_private);
    assert_eq!(selected_private.entity_count, 2);
    let private_detail = selected_private.enrichment.unwrap();
    assert!(private_detail.get("unavailable").is_none());
    let private_rows = private_detail["files"].as_array().unwrap();
    assert_eq!(private_rows.len(), 2);
    let missing = json!(kin_model::RepoPath::from_utf8("missing.py").unwrap());
    assert!(private_rows
        .iter()
        .any(|row| row["projection_path"] == missing && row["admitted"] == false));

    let selected = mcp_call(
        app,
        "kin_graph_status",
        json!({
            "dependencies":request.dependencies, "max_chars":60000,
        }),
    )
    .await;
    assert_eq!(selected.is_error, Some(true));
    assert!(mcp_result_text(&selected).contains("This selector is not supported; nothing ran."));
}

#[tokio::test]
async fn enrichment_status_cache_never_replays_a_different_dependency_selection() {
    let state = test_state_with_committed_sources(&[
        ("src/a.py", "def alpha():\n    return 1\n"),
        ("src/b.py", "def beta():\n    return 2\n"),
    ]);
    let graph = Arc::clone(&state.graph);
    let request = kin_mcp::status_pages::StatusRequest {
        dependencies: vec!["src/a.py".into()],
        ..Default::default()
    };
    let captured = mcp_graph_status_snapshot(
        &state,
        None,
        &graph,
        RequestGraphAuthority::Head,
        kin_mcp::handlers::entities::GraphStatusScope::Head,
        &request,
    )
    .await
    .unwrap();
    assert_eq!(
        parse_graph_status(&captured).enrichment.unwrap()["files"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let _guard = state.embedding_work.lock().unwrap();
    let same = mcp_graph_status_snapshot(
        &state,
        None,
        &graph,
        RequestGraphAuthority::Head,
        kin_mcp::handlers::entities::GraphStatusScope::Head,
        &request,
    )
    .await
    .unwrap();
    assert!(parse_graph_status(&same).stale.is_some());
    for dependencies in [vec![], vec!["src/b.py".into()]] {
        let other = kin_mcp::status_pages::StatusRequest {
            dependencies,
            ..Default::default()
        };
        let refused = mcp_graph_status_snapshot(
            &state,
            None,
            &graph,
            RequestGraphAuthority::Head,
            kin_mcp::handlers::entities::GraphStatusScope::Head,
            &other,
        )
        .await
        .unwrap();
        assert_eq!(
            refused.is_error,
            Some(true),
            "a different query has no matching settled detail"
        );
    }
}
