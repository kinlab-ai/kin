// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

async fn projection_places_through_route(
    state: &Arc<DaemonState>,
    entity_ids: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = router(Arc::clone(state))
        .oneshot(
            Request::post("/projection/places")
                .header("content-type", "application/json")
                .body(Body::from(json!({"entity_ids": entity_ids}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| json!(String::from_utf8_lossy(&body).into_owned()));
    (status, value)
}

/// An editor asks where entities are, by id, and lands on each declaration in
/// the working copy. What the graph cannot place is answered as such, per id,
/// and the agent surface carries none of it.
#[tokio::test]
async fn projection_places_answer_each_entity_with_its_working_copy_place() {
    let (repo, state) = mcp_lifecycle_fixture();
    state.set_spine_disabled_for_test(true);
    std::fs::write(repo.path().join("local.py"), WAITING_TARGET).unwrap();
    std::fs::write(repo.path().join("caller.py"), WAITING_CALLER).unwrap();
    waiting_admit(&state, "places").await;
    let run = waiting_entity(&state, "caller.py", "run");
    let work = waiting_entity(&state, "local.py", "work");
    let unknown = kin_model::EntityId::new().to_string();

    let (status, answer) = projection_places_through_route(
        &state,
        json!([
            run.id.to_string(),
            work.id.to_string(),
            unknown,
            "not-an-entity",
            run.id.to_string()
        ]),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["column_unit"], "utf8_byte", "{answer}");
    let root = answer["working_copy_root"]
        .as_str()
        .expect("the daemon names its root");
    assert_eq!(
        std::fs::canonicalize(root).unwrap(),
        std::fs::canonicalize(repo.path()).unwrap()
    );
    let places = answer["places"].as_array().expect("places");
    assert_eq!(places.len(), 4, "each distinct id once: {answer:#}");

    // `run` is declared on the caller's third line and ends on its fourth.
    let placed = &places[0];
    assert_eq!(placed["status"], "projected", "{placed:#}");
    assert_eq!(placed["entity_id"], run.id.to_string());
    assert_eq!(placed["name"], "run");
    assert_eq!(placed["kind"], "function");
    assert_eq!(placed["path"], "caller.py");
    assert_eq!(placed["start_line"], 3, "{placed:#}");
    assert_eq!(placed["start_column"], 1, "{placed:#}");
    assert_eq!(placed["end_line"], 4, "{placed:#}");
    assert_eq!(places[1]["path"], "local.py", "{:#}", places[1]);
    assert_eq!(places[1]["start_line"], 1, "{:#}", places[1]);

    assert_eq!(places[2]["status"], "not_projected");
    assert_eq!(places[2]["reason"], "unknown_entity");
    assert_eq!(places[2]["entity_id"], unknown);
    assert_eq!(places[3]["status"], "not_projected");
    assert_eq!(places[3]["reason"], "invalid_id");

    // The call is the editor's boundary and nothing on the MCP surface
    // reaches it.
    for tool in ["projection_places", "places", "entity_places"] {
        let response = router(Arc::clone(&state))
            .oneshot(
                Request::post("/mcp/tools/call")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"name": tool, "arguments": {"entity_ids": [run.id]}}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        let refused = status != StatusCode::OK
            || serde_json::from_slice::<kin_mcp::ToolCallResult>(&body)
                .is_ok_and(|result| result.is_error == Some(true));
        assert!(refused, "{tool} answered over MCP: {status} {text}");
        assert!(!text.contains("caller.py"), "{tool}: {text}");
    }
}

#[tokio::test]
async fn projection_places_refuse_a_batch_over_the_limit_and_answer_an_empty_one() {
    let (_repo, state) = mcp_lifecycle_fixture();
    let too_many: Vec<String> = (0..=kin_projection::places::MAX_PLACES_PER_REQUEST)
        .map(|_| kin_model::EntityId::new().to_string())
        .collect();

    let (status, answer) = projection_places_through_route(&state, json!(too_many)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert!(
        answer
            .as_str()
            .is_some_and(|text| text.contains("at most 512 entities")),
        "{answer}"
    );

    let (status, answer) = projection_places_through_route(&state, json!([])).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["places"], json!([]), "{answer}");
}
