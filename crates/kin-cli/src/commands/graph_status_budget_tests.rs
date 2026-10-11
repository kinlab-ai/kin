// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use super::*;
use serde_json::{json, Value};

fn observation(count: usize) -> Value {
    json!({
        "schema":"kin.graph-status.v1", "view":"daemon_selected_graph",
        "scope":"head", "authority":"repo-daemon",
        "sampling":"point_in_time_selected_graph", "authority_epoch":7,
        "entity_count":49, "relation_count":7, "embedding_source":"selected_graph",
        "embeddings_indexed":0, "embeddings_pending":49, "embeddings_total":49,
        "completion_attested":false,
        "call_sites":{"scope":"the store", "proof":"unverified", "callers_owed_enrichment":1},
        "enrichment":{
            "schema":"kin.enrichment-status.v1", "scope":{"kind":"workspace_head"},
            "truth_epoch":7, "snapshot_id":"a".repeat(64), "current":true,
            "all_relationships_attested":false, "proof_scope":"recorded_call_site_census",
            "files":(0..count).map(|index| json!({
                "projection_path":kin_model::RepoPath::from_utf8(format!("src/{index:03}.py")).unwrap(),
                "artifact_id":format!("artifact-{index}"), "admitted":index != count - 1,
                "proof":"unverified", "current_completion":"recorded",
                "source_reason":if index == count - 1 { "not_admitted" } else { "source_gap" },
                "outstanding_bindings":1,
                "contexts":[{"state":"owed", "reason":"uncertain target λ; ".repeat(65)}],
            })).collect::<Vec<_>>()
        }
    })
}

fn base_response(padding: usize) -> GraphCommandResponse {
    serde_json::from_value(json!({
        "lines":[format!("Partial graph coverage: {}", "unverified; ".repeat(padding))],
        "error":"one critical graph health issue remains",
    }))
    .unwrap()
}

fn paged_response(
    base: &GraphCommandResponse,
    full: &Value,
    request: &kin_mcp::status_pages::StatusRequest,
) -> GraphCommandResponse {
    page_status_response(
        base,
        kin_mcp::ToolCallResult::text(full.to_string()),
        request,
        decoration().encoded_overhead().unwrap(),
        &[7; 32],
    )
    .unwrap()
}

fn decoration() -> StatusDecoration {
    StatusDecoration {
        lines: vec![
            String::new(),
            "ℹ fresh λ: \"quoted\"\\projection\nunknown".into(),
            "⚠ partial history: source gaps remain".into(),
        ],
        history_state: json!({"state":"partial", "partial":"unverified \"history\" λ"}),
    }
}

#[test]
fn status_json_pages_fit_final_envelope_and_reconstruct_dependency_proof() {
    let full = observation(49);
    let base = base_response(1_100);
    let mut request = kin_mcp::status_pages::StatusRequest {
        dependencies: (0..49).map(|index| format!("src/{index:03}.py")).collect(),
        max_chars: Some(60_000),
        ..Default::default()
    };
    let mut rows = Vec::new();
    let mut pages = 0;
    loop {
        let response = paged_response(&base, &full, &request);
        let encoded = decoration().encode(&response).unwrap();
        assert!(
            encoded.len() <= 60_000,
            "final CLI JSON is {} bytes",
            encoded.len()
        );
        assert_eq!(response.error, base.error);
        assert_eq!(response.call_sites.as_ref().unwrap(), &full["call_sites"]);
        let enrichment = response.enrichment.unwrap();
        assert_eq!(enrichment["snapshot_id"], full["enrichment"]["snapshot_id"]);
        assert_eq!(enrichment["all_relationships_attested"], false);
        assert_eq!(enrichment["page"]["max_chars"], 60_000);
        assert_eq!(enrichment["page"]["start"], rows.len());
        let page_rows = enrichment["files"].as_array().unwrap();
        assert!(!page_rows.is_empty());
        rows.extend(page_rows.iter().cloned());
        pages += 1;
        request.cursor = enrichment["page"]["next_cursor"]
            .as_str()
            .map(str::to_owned);
        if request.cursor.is_none() {
            break;
        }
    }
    assert!(pages > 1);
    assert_eq!(json!(rows), full["enrichment"]["files"]);
}

#[test]
fn status_json_decoration_measures_actual_utf8_and_escaping() {
    let decoration = decoration();
    for padding in [0, 1, 1_100] {
        let base = base_response(padding);
        let original = serde_json::to_vec(&base).unwrap().len();
        let encoded = decoration.encode(&base).unwrap();
        assert_eq!(
            encoded.len(),
            original + decoration.encoded_overhead().unwrap()
        );
        let value: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["history_state"], decoration.history_state);
        assert_eq!(value["error"], base.error.unwrap());
        assert_eq!(
            &value["lines"].as_array().unwrap()[1..],
            serde_json::to_value(&decoration.lines)
                .unwrap()
                .as_array()
                .unwrap()
        );
    }
    let empty: GraphCommandResponse = serde_json::from_value(json!({"lines":[]})).unwrap();
    assert_eq!(
        decoration.encode(&empty).unwrap().len() + 1,
        serde_json::to_vec(&empty).unwrap().len() + decoration.encoded_overhead().unwrap()
    );
}

#[test]
fn status_json_rejects_indivisible_floor_and_single_proof_row_without_clipping() {
    let request = kin_mcp::status_pages::StatusRequest {
        max_chars: Some(60_000),
        ..Default::default()
    };
    let result = |full: &Value| kin_mcp::ToolCallResult::text(full.to_string());
    for count in [0, 1] {
        let full = observation(count);
        let error =
            page_status_response(&base_response(5_100), result(&full), &request, 0, &[7; 32])
                .unwrap_err();
        assert!(error.contains("cannot fit"), "{error}");
    }
    let mut full = observation(1);
    full["enrichment"]["files"][0]["contexts"][0]["reason"] = json!("unverified λ".repeat(10_000));
    assert!(page_status_response(&base_response(0), result(&full), &request, 0, &[7; 32]).is_err());
    assert!(
        full["enrichment"]["files"][0]["contexts"][0]["reason"]
            .as_str()
            .unwrap()
            .len()
            > 60_000
    );
    assert!(page_status_response(
        &base_response(0),
        result(&observation(1)),
        &request,
        usize::MAX,
        &[7; 32]
    )
    .is_err());
}

#[test]
fn status_json_floor_is_measured_for_empty_and_unavailable_observations() {
    let request = kin_mcp::status_pages::StatusRequest {
        max_chars: Some(60_000),
        ..Default::default()
    };
    let mut absent = observation(0);
    absent.as_object_mut().unwrap().remove("enrichment");
    let mut unavailable = observation(0);
    unavailable["enrichment"]["unavailable"] =
        json!({"reason":"source metadata is bounded", "limit_kind":"bytes", "limit":8*1024*1024});
    unavailable["enrichment"]["status"] = json!("bounded");
    unavailable["enrichment"]["limitation"] =
        json!("enrichment_metadata_unavailable: source metadata is bounded");
    for full in [absent, unavailable] {
        let small = page_status_response(
            &base_response(0),
            kin_mcp::ToolCallResult::text(full.to_string()),
            &request,
            0,
            &[7; 32],
        )
        .unwrap();
        assert!(serde_json::to_vec(&small).unwrap().len() < 60_000);
        assert_eq!(
            small
                .enrichment
                .as_ref()
                .and_then(|value| value.get("all_relationships_attested")),
            full.get("enrichment")
                .and_then(|value| value.get("all_relationships_attested"))
        );
        let error = page_status_response(
            &base_response(5_100),
            kin_mcp::ToolCallResult::text(full.to_string()),
            &request,
            0,
            &[7; 32],
        )
        .unwrap_err();
        assert!(error.contains("cannot fit"), "{error}");
    }
}

#[test]
fn status_json_exact_floor_and_budget_boundary_are_admitted() {
    for (ceiling, count) in [(2_000, 0), (2_000, 1), (60_000, 0), (60_000, 1)] {
        let request = kin_mcp::status_pages::StatusRequest {
            max_chars: Some(ceiling),
            ..Default::default()
        };
        let mut full = observation(count);
        if count == 1 {
            full["enrichment"]["files"][0]["contexts"][0]["reason"] = json!("unverified λ");
        }
        let base = base_response(0);
        let page = paged_response(&base, &full, &request);
        let size = serde_json::to_vec(&page).unwrap().len();
        let result = || kin_mcp::ToolCallResult::text(full.to_string());
        let exact =
            page_status_response(&base, result(), &request, ceiling - size, &[7; 32]).unwrap();
        assert_eq!(
            serde_json::to_vec(&exact).unwrap().len() + ceiling - size,
            ceiling
        );
        assert!(
            page_status_response(&base, result(), &request, ceiling + 1 - size, &[7; 32]).is_err()
        );
    }
}

#[test]
fn status_json_continuation_keeps_snapshot_dependency_and_mac_guards() {
    let full = observation(49);
    let base = base_response(1_100);
    let mut request = kin_mcp::status_pages::StatusRequest {
        max_chars: Some(60_000),
        ..Default::default()
    };
    let first = paged_response(&base, &full, &request);
    request.cursor = first.enrichment.unwrap()["page"]["next_cursor"]
        .as_str()
        .map(str::to_owned);
    assert!(request.cursor.is_some());
    let start = paged_response(&base, &full, &request).enrichment.unwrap()["page"]["start"].clone();
    request.max_chars = Some(55_000);
    assert_eq!(
        paged_response(&base, &full, &request).enrichment.unwrap()["page"]["start"],
        start
    );
    let mut changed = full.clone();
    changed["enrichment"]["snapshot_id"] = json!("b".repeat(64));
    assert!(page_status_response(
        &base,
        kin_mcp::ToolCallResult::text(changed.to_string()),
        &request,
        0,
        &[7; 32]
    )
    .unwrap_err()
    .contains("changed"));
    assert!(page_status_response(
        &base,
        kin_mcp::ToolCallResult::text(full.to_string()),
        &request,
        0,
        &[8; 32]
    )
    .is_err());
    request.dependencies = vec!["src/000.py".into()];
    assert!(page_status_response(
        &base,
        kin_mcp::ToolCallResult::text(full.to_string()),
        &request,
        0,
        &[7; 32]
    )
    .is_err());
}

#[test]
fn status_json_terminal_rows_fit_when_intermediate_cursor_does_not() {
    let base = base_response(0);
    for resumed in [false, true] {
        let mut full = observation(if resumed { 3 } else { 2 });
        for row in full["enrichment"]["files"].as_array_mut().unwrap() {
            row["contexts"][0]["reason"] = json!("unverified λ");
        }
        let mut request = kin_mcp::status_pages::StatusRequest {
            max_chars: Some(60_000),
            ..Default::default()
        };
        let prefix = |request: &kin_mcp::status_pages::StatusRequest| {
            let result = kin_mcp::status_pages::page_enrichment_for_envelope(
                kin_mcp::ToolCallResult::text(full.to_string()),
                request,
                &[7; 32],
                |candidate| {
                    Ok(
                        if candidate["enrichment"]["files"].as_array().unwrap().len() > 1 {
                            60_001
                        } else {
                            0
                        },
                    )
                },
            )
            .unwrap();
            let kin_mcp::types::ContentBlock::Text { text } = &result.content[0];
            let mut response = base.clone();
            response.with_status_observation(serde_json::from_str(text).unwrap());
            response
        };
        if resumed {
            request.cursor = prefix(&request).enrichment.unwrap()["page"]["next_cursor"]
                .as_str()
                .map(str::to_owned);
        }
        let terminal = paged_response(&base, &full, &request);
        let terminal_bytes = serde_json::to_vec(&terminal).unwrap().len();
        let intermediate_bytes = serde_json::to_vec(&prefix(&request)).unwrap().len();
        assert!(intermediate_bytes > terminal_bytes,
            "fixture must have a larger cursor-bearing prefix: {intermediate_bytes} vs {terminal_bytes}");
        eprintln!("status terminal boundary: resumed={resumed}, terminal={terminal_bytes}, cursor prefix={intermediate_bytes}");
        let result = page_status_response(
            &base,
            kin_mcp::ToolCallResult::text(full.to_string()),
            &request,
            60_000 - terminal_bytes,
            &[7; 32],
        )
        .unwrap();
        let detail = result.enrichment.as_ref().unwrap();
        assert_eq!(
            serde_json::to_vec(&result).unwrap().len() + 60_000 - terminal_bytes,
            60_000
        );
        assert_eq!(detail["page"]["returned"], 2);
        assert_eq!(detail["page"]["complete"], !resumed);
        assert_eq!(detail["page"]["start"], usize::from(resumed));
        assert!(detail["page"]["next_cursor"].is_null());
        assert_eq!(
            detail["files"],
            json!(&full["enrichment"]["files"].as_array().unwrap()[usize::from(resumed)..])
        );
    }
}

#[test]
fn status_detailed_decoration_allowance_is_optional_and_round_trips() {
    let old: GraphCommandRequest =
        serde_json::from_value(json!({"command":"status_detailed", "max_chars":60000})).unwrap();
    assert!(matches!(
        old,
        GraphCommandRequest::StatusDetailed {
            response_overhead_bytes: 0,
            ..
        }
    ));
    assert!(serde_json::to_value(old)
        .unwrap()
        .get("response_overhead_bytes")
        .is_none());
    let request: GraphCommandRequest = serde_json::from_value(json!({"command":"status_detailed", "dependencies":["src/000.py"], "max_chars":60000, "response_overhead_bytes":523})).unwrap();
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(value["max_chars"], 60000);
    assert_eq!(value["response_overhead_bytes"], 523);
    assert!(matches!(
        serde_json::from_value::<GraphCommandRequest>(value).unwrap(),
        GraphCommandRequest::StatusDetailed {
            response_overhead_bytes: 523,
            ..
        }
    ));
}
