// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Synthetic equivalents of a members-first trace page and its continuation.

use serde_json::{json, Value};

use crate::server::{present_for_client, McpServerConfig};
use crate::types::JsonRpcResponse;

fn context() -> crate::trace_pages::Context {
    crate::trace_pages::Context::new(
        &json!({"tool":"trace_data_flow","arguments":{"focal":"focal", "direction":"callers", "depth":8}}),
        &json!({"repository":"fixture", "generation":2}),
    )
}

fn payload() -> Value {
    let members: Vec<_> = (1..=3).map(|n| json!({
        "id":format!("00000000-0000-4000-8000-{n:012}"),
        "name":format!("caller_{n}"), "kind":"Function", "distance":n,
        "via":if n == 1 { "focal".to_string() } else { format!("00000000-0000-4000-8000-{:012}",n-1) },
        "line_in_entity":n+10, "code":"next()", "proof":"type_resolved", "proven_path":true,
    })).collect();
    let chain: Vec<_> = (1..=3)
        .map(|n| {
            json!({
                "step":n, "parent_step":n-1, "depth":n,
                "entity_id":format!("00000000-0000-4000-8000-{n:012}"),
                "entity_name":format!("caller_{n}"), "entity_kind":"Function", "role":"caller",
                "code":"next()", "reference_sites":[{"line_in_entity":n+10, "expr":"next()"}],
                "signature":"s".repeat(2100),
            })
        })
        .collect();
    json!({
        "focal_name":"Encoder.encode", "focal_id":"focal", "direction":"callers", "depth":8,
        "focal_entity":{"entity_id":"focal", "entity_name":"Encoder.encode", "entity_kind":"Method"},
        "chain":chain, "total_steps":3, "truncated":true, "terminal_coverage_gap_steps":1,
        "type_uses_withheld":1, "include_type_edges":false, "bodies_included":false,
        "limit_per_step":12, "max_response_chars":4000,
        "reach":{
            "members":members, "members_total":3, "state":"open",
            "frontier":[{"in":"base", "in_name":"Base.encode", "to":"focal", "to_name":"Encoder.encode",
                "kind":"dispatch", "why":"A proven base call can dispatch to this override; those calls were not followed."}],
            "frontier_total":1, "frontier_withheld":[], "bounds":[], "blind_spots":{},
            "policy":{"follows":["Calls:type_resolved"],"not_followed":"unproved dispatch"},
        },
        "edge_coverage":{"classes":{"calls":"absent","imports":"absent","references":"absent"}},
        "_kin":{
            "verdict":{"state":"inconclusive", "safe_to_conclude_absent":false,
                "limiting_factor":"cross_file_edges_absent; answer_truncated; trace_walk_truncated; substrate_partial"},
            "completeness":{"status":"partial","limits":["edge_coverage:calls_absent"]},
            "graph_as_of":{"generation":2}, "repository":{"authority_context":"a".repeat(64)},
            "semantic_surface":1,
        },
    })
}

fn config() -> McpServerConfig {
    McpServerConfig {
        plain_replies: true,
        compact_trace_evidence: true,
        agent_belt: true,
        ..Default::default()
    }
}

fn request(arguments: Value) -> String {
    json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"trace","arguments":arguments}}).to_string()
}

fn presented(page: &Value, arguments: Value, config: &McpServerConfig) -> Value {
    let response = JsonRpcResponse::success(
        Some(json!(1)),
        json!({"content":[{"type":"text","text":page.to_string()}]}),
    );
    present_for_client(response, &request(arguments), config)
        .result
        .unwrap()
}

fn decoded(result: &Value) -> Value {
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn a_members_first_page_keeps_its_focal_depth_and_actual_gap() {
    let _guard = crate::trace_pages::cache_test_guard();
    let mut original = payload();
    original["limit_per_step"] = json!(25);
    let page = crate::trace_pages::start(original, context(), 4000).unwrap();
    assert_eq!(page["_kin"]["trace_question"]["limit_per_step"], 25);
    assert_eq!(page["reach"]["members"].as_array().unwrap().len(), 3);
    assert!(page["chain"].as_array().unwrap().is_empty());
    let reply = decoded(&presented(
        &page,
        json!({"focal":"focal","direction":"callers","depth":8,"max_output_bytes":4000}),
        &config(),
    ));
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.contains("Encoder.encode") && !meaning.contains("within 8 calls"),
        "{meaning}"
    );
    assert!(
        meaning.contains("1 place") && meaning.contains("could add callers"),
        "{meaning}"
    );
    assert_eq!(reply["_kin"]["verdict"], "partial");
    assert_eq!(reply["reach"]["members"], page["reach"]["members"]);
    assert_eq!(reply["readings"], page["readings"]);
    assert_eq!(reply["next_cursor"], page["next_cursor"]);
}

#[test]
fn delivered_members_are_distinguished_from_remaining_walk_evidence() {
    let _guard = crate::trace_pages::cache_test_guard();
    let page = crate::trace_pages::start(payload(), context(), 4000).unwrap();
    let reply = decoded(&presented(
        &page,
        json!({"focal":"focal","direction":"callers","depth":8,"max_output_bytes":4000}),
        &config(),
    ));
    assert_eq!(reply["_kin"]["page"]["answer_domain"], "reach.members");
    // Plain pages name these transport facts as delivery of recorded rows.
    assert_eq!(reply["_kin"]["page"]["recorded_rows_exhausted"], true);
    assert_eq!(reply["_kin"]["page"]["recorded_rows_has_more"], false);
    assert_eq!(reply["_kin"]["page"]["evidence_has_more"], true);
    for internal in ["answer_rows_complete", "answer_rows_has_more"] {
        assert!(reply["_kin"]["page"].get(internal).is_none(), "{internal}");
    }
    let delivered = reply["_kin"]["delivered"].as_str().unwrap();
    assert!(
        delivered.contains("recorded") && delivered.contains("evidence"),
        "{delivered}"
    );
    assert!(!delivered.starts_with("More rows follow"), "{delivered}");
}

#[test]
fn a_final_continuation_keeps_the_question_without_false_size_advice() {
    let _guard = crate::trace_pages::cache_test_guard();
    let page = crate::trace_pages::start(payload(), context(), 4000).unwrap();
    let cursor = page["next_cursor"].as_str().unwrap();
    let final_page = crate::trace_pages::resume(cursor, &context(), 24576).unwrap();
    assert!(final_page["next_cursor"].is_null());
    let reply = decoded(&presented(&final_page, json!({"cursor":cursor}), &config()));
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.contains("Encoder.encode") && meaning.contains("8 calls"),
        "{meaning}"
    );
    assert!(
        !meaning.contains("what calls the entity and what it calls"),
        "{meaning}"
    );
    // Every reach row was on the first page, so this one is display detail:
    // it does not grade the answer again.
    assert!(meaning.starts_with("Display detail"), "{meaning}");
    assert!(reply["_kin"].get("verdict").is_none(), "{reply}");
    assert_eq!(reply["_kin"]["graph_as_of"]["generation"], 2);
    assert!(reply["_kin"].get("delivered").is_none(), "{reply}");
    assert_eq!(reply["readings"], final_page["readings"]);
    assert!(reply["next_cursor"].is_null());
}

#[test]
fn an_explicit_4000_byte_continuation_never_grows_past_its_actual_cap() {
    let _guard = crate::trace_pages::cache_test_guard();
    let mut page = crate::trace_pages::start(payload(), context(), 4000).unwrap();
    let mut arguments =
        json!({"focal":"focal","direction":"callers","depth":8,"max_output_bytes":4000});
    for _ in 0..40 {
        let result = presented(&page, arguments, &config());
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.len() <= 4000, "{} bytes: {text}", text.len());
        let reply = decoded(&result);
        assert_eq!(reply["readings"], page["readings"]);
        assert_eq!(reply["record_fragment"], page["record_fragment"]);
        let Some(cursor) = page["next_cursor"].as_str() else {
            return;
        };
        arguments = json!({"cursor":cursor,"max_output_bytes":4000});
        page = crate::trace_pages::resume(cursor, &context(), 4000).unwrap();
    }
    panic!("synthetic continuation did not terminate");
}

#[test]
fn fragments_and_malformed_reading_wrappers_are_not_reinterpreted() {
    for extra in [
        json!({"record_fragment":{"collection":"readings","key":"direction","field":null,"text":"call","field_complete":false}}),
        json!({"readings":[{"key":"direction"}]}),
        json!({"readings":[{"key":"direction","value":"calls"},{"key":"direction","value":"callers"}]}),
    ] {
        let mut page = json!({"chain":[],"next_cursor":"opaque", "_kin":{"semantic_surface":1,"verdict":{"state":"inconclusive"},"page":{"version":1,"complete":false,"has_more":true},"response":{"max_chars":4000}}});
        page.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert_eq!(
            presented(&page, json!({"cursor":"opaque"}), &config())["content"][0]["text"],
            page.to_string()
        );
    }
}

#[test]
fn a_plain_default_or_machine_connection_keeps_its_existing_representation() {
    let _guard = crate::trace_pages::cache_test_guard();
    let page = crate::trace_pages::start(payload(), context(), 4000).unwrap();
    let mut expected = json!({"content":[{"type":"text","text":page.to_string()}]});
    crate::plain_reply::present(&mut expected, "trace_data_flow");
    // The default plain connection serves the walk compact-first, never in
    // the compact codec the explicit query profiles take.
    crate::reply_budget::present_trace(&mut expected);
    let mut default = config();
    default.compact_trace_evidence = false;
    assert_eq!(
        presented(&page, json!({"focal":"focal"}), &default),
        expected
    );
    let mut machine = config();
    machine.plain_replies = false;
    assert_eq!(
        presented(&page, json!({"focal":"focal"}), &machine)["content"][0]["text"],
        page.to_string()
    );
    let mut citable = config();
    citable.citable = true;
    assert_eq!(
        presented(&page, json!({"focal":"focal"}), &citable)["content"][0]["text"],
        page.to_string()
    );
}

/// A detached page has no local snapshot or decodable cursor. The transport
/// already validated its frozen authority before delivering these records.
fn detached_page() -> Value {
    json!({
        "chain":[], "next_cursor":"opaque-owned-handle", "total_steps":0,
        "reach":{
            "members":[{"id":"caller", "name":"caller", "distance":1, "via":"focal", "proof":"type_resolved", "proven_path":true}],
            "frontier":[{"in":"base", "to":"focal", "to_name":"Encoder.encode", "kind":"dispatch", "why":"A base call may run this override."}],
        },
        "readings":[{"key":"reach", "value":{"members_total":1, "frontier_total":1}}],
        "_kin":{
            "verdict":{"state":"inconclusive", "safe_to_conclude_absent":false, "limiting_factor":"substrate_partial"},
            "graph_as_of":{"generation":2}, "repository":{"authority_context":"a".repeat(64)},
            "semantic_surface":1,
            "page":{"version":1, "complete":false, "has_more":true, "record_offset":0, "next_record":3,
                "field_offset":0, "byte_offset":0, "next_field":0, "next_byte":0, "total_records":10, "total_steps":3},
            "response":{"max_chars":4000, "bounded":true},
        },
    })
}

#[test]
fn unprojected_detached_pages_are_refused_on_every_presentation_mode() {
    let mut page = detached_page();
    page["_kin"]
        .as_object_mut()
        .unwrap()
        .remove("semantic_surface");
    page["_kin"]["repository"] = json!({"root":"/fixture"});
    let mut machine = config();
    machine.plain_replies = false;
    let mut citable = config();
    citable.citable = true;
    for config in [config(), machine, citable] {
        let result = presented(&page, json!({"cursor":"opaque-owned-handle"}), &config);
        assert_eq!(result["isError"], true, "{result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("restart the query without a cursor"),
            "{text}"
        );
        assert!(
            !text.contains("/fixture") && !text.contains("opaque-owned-handle"),
            "{text}"
        );
    }
}

#[test]
fn detached_daemon_page_needs_no_process_local_snapshot() {
    let page = detached_page();
    let reply = decoded(&presented(
        &page,
        json!({"focal":"focal", "direction":"callers", "depth":8,"max_output_bytes":4000}),
        &config(),
    ));
    assert_eq!(reply["_kin"]["page"]["recorded_rows_exhausted"], true);
    assert_eq!(reply["_kin"]["page"]["recorded_rows_total"], 1);
    assert_eq!(reply["reach"], page["reach"]);
    assert_eq!(reply["readings"], page["readings"]);
    assert_eq!(reply["next_cursor"], page["next_cursor"]);
}

#[test]
fn bare_cursor_budget_comes_from_its_current_page_not_an_old_reading() {
    let _guard = crate::trace_pages::cache_test_guard();
    let first = crate::trace_pages::start(payload(), context(), 4000).unwrap();
    let cursor = first["next_cursor"].as_str().unwrap();
    let final_page = crate::trace_pages::resume(cursor, &context(), 24576).unwrap();
    assert_eq!(final_page["_kin"]["response"]["max_chars"], 24576);
    let result = presented(&final_page, json!({"cursor":cursor}), &config());
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.len() > 4000 && text.len() <= 24576);
    let reply = decoded(&result);
    assert_eq!(reply["_kin"]["page"]["recorded_rows_total"], Value::Null);
    assert!(reply["_kin"]["page"]
        .get("recorded_answer_rows_total")
        .is_none());
    assert_eq!(reply["_kin"]["page"]["recorded_rows_exhausted"], true);
    assert_eq!(reply["_kin"]["page"]["evidence_has_more"], false);
}

#[test]
fn bad_or_missing_page_summaries_pass_through_without_invented_totals() {
    let mut cases = Vec::new();
    let mut page = detached_page();
    page["readings"][0]["value"]
        .as_object_mut()
        .unwrap()
        .remove("members_total");
    cases.push(page);
    let mut page = detached_page();
    page["readings"]
        .as_array_mut()
        .unwrap()
        .push(json!({"key":"reach","value":{"members_total":2}}));
    cases.push(page);
    let mut page = detached_page();
    page["readings"]
        .as_array_mut()
        .unwrap()
        .push(json!({"key":"depth","value":2}));
    cases.push(page);
    let mut page = detached_page();
    page["readings"]
        .as_array_mut()
        .unwrap()
        .push(json!({"key":"_kin","value":{"graph_as_of":{"generation":3}}}));
    cases.push(page);
    let mut page = detached_page();
    page["_kin"]["page"]["total_steps"] = json!("unknown");
    cases.push(page);
    let mut page = detached_page();
    page["_kin"].as_object_mut().unwrap().remove("response");
    cases.push(page);
    let mut page = detached_page();
    page["record_fragment"] = json!({"collection":"readings","key":"depth","text":"8"});
    page["_kin"].as_object_mut().unwrap().remove("page");
    cases.push(page);
    for page in cases {
        let result = presented(
            &page,
            json!({"focal":"focal", "direction":"callers", "depth":8,"max_output_bytes":4000}),
            &config(),
        );
        assert_eq!(result["content"][0]["text"], page.to_string());
    }
}

#[test]
fn ignored_bare_cursor_scope_cannot_replace_the_frozen_question() {
    let page = detached_page();
    let reply = presented(
        &page,
        json!({"cursor":"opaque-owned-handle", "direction":"calls", "depth":1}),
        &config(),
    );
    assert_eq!(reply["content"][0]["text"], page.to_string());
}

#[test]
fn presentation_that_does_not_fit_returns_the_whole_original_page() {
    let mut page = detached_page();
    page["readings"]
        .as_array_mut()
        .unwrap()
        .push(json!({"key":"retained_evidence", "value":"x".repeat(2000)}));
    let cap = page.to_string().len();
    // Make the original budget accounting stable before checking expansion.
    page["_kin"]["response"]["max_chars"] = json!(cap);
    let cap = page.to_string().len();
    page["_kin"]["response"]["max_chars"] = json!(cap);
    let result = presented(
        &page,
        json!({"focal":"focal", "direction":"callers", "depth":8}),
        &config(),
    );
    assert_eq!(result["content"][0]["text"], page.to_string());
}

#[test]
fn terminal_page_reading_cannot_promote_the_current_verdict() {
    let mut page = detached_page();
    page["reach"] = json!({});
    page["chain"] = json!([{"step":1,"parent_step":0,"entity_id":"caller", "entity_name":"caller", "proof":"type_resolved", "resolution":"type_resolved", "reference_sites":[{"line_in_entity":1,"expr":"focal()", "proof":"type_resolved"}]}]);
    page["next_cursor"] = Value::Null;
    page["_kin"]["page"]["record_offset"] = json!(3);
    page["_kin"]["page"]["next_record"] = json!(10);
    page["_kin"]["page"]["has_more"] = json!(false);
    page["readings"] = json!([
        {"key":"focal_name","value":"Encoder.encode"}, {"key":"depth","value":8}, {"key":"direction","value":"callers"},
        {"key":"_kin","value":{"verdict":{"state":"certified", "safe_to_conclude_absent":true}, "graph_as_of":{"generation":2}}},
    ]);
    let reply = decoded(&presented(
        &page,
        json!({"cursor":"opaque-owned-handle"}),
        &config(),
    ));
    assert_eq!(reply["_kin"]["verdict"], "partial");
    assert!(reply["_kin"].get("delivered").is_none());
    assert_eq!(reply["chain"][0]["proof"], page["chain"][0]["proof"]);
    assert_eq!(
        reply["chain"][0]["resolution"],
        page["chain"][0]["resolution"]
    );
    assert_eq!(
        reply["chain"][0]["reference_sites"],
        page["chain"][0]["reference_sites"]
    );
    assert_eq!(reply["readings"], page["readings"]);
}

#[test]
fn routed_and_discovered_trace_pages_use_the_same_resolved_scope() {
    let page = detached_page();
    let args = json!({"focal":"focal", "direction":"callers", "depth":8,"max_output_bytes":4000});
    let routed = McpServerConfig {
        routed: Some(crate::routed::RoutedSurface::READ_ONLY),
        allowed_tools: Some(crate::tool_name_set(crate::agent_routed_tool_names())),
        ..config()
    };
    let discovered = McpServerConfig {
        allowed_tools: Some(crate::tool_name_set(crate::agent_search_tool_names())),
        ..config()
    };
    let expected = presented(&page, args.clone(), &config());
    for (name, arguments, config) in [
        (
            "kin",
            json!({"command":"trace", "args":args.clone()}),
            routed,
        ),
        (
            "kin_tool_call",
            json!({"tool":"trace", "arguments":args}),
            discovered,
        ),
    ] {
        let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}}).to_string();
        let response = JsonRpcResponse::success(
            Some(json!(1)),
            json!({"content":[{"type":"text","text":page.to_string()}]}),
        );
        assert_eq!(
            present_for_client(response, &request, &config)
                .result
                .unwrap(),
            expected
        );
    }
}

#[test]
fn unicode_fragment_pages_reassemble_without_cutting_proof_or_changing_ids() {
    let _guard = crate::trace_pages::cache_test_guard();
    let mut source = payload();
    let signature = "🧭é".repeat(1200);
    source["chain"][0]["signature"] = json!(signature.clone());
    let mut page = crate::trace_pages::start(source, context(), 4000).unwrap();
    let mut args =
        json!({"focal":"focal", "direction":"callers", "depth":8,"max_output_bytes":4000});
    let mut reconstructed = String::new();
    for _ in 0..80 {
        let result = presented(&page, args, &config());
        assert!(result["content"][0]["text"].as_str().unwrap().len() <= 4000);
        let reply = decoded(&result);
        assert_eq!(reply["record_fragment"], page["record_fragment"]);
        assert_eq!(reply["readings"], page["readings"]);
        if page["record_fragment"]["collection"] == "chain"
            && page["record_fragment"]["index"] == 0
            && page["record_fragment"]["field"] == "signature"
        {
            assert_eq!(
                page["record_fragment"]["entity_id"],
                "00000000-0000-4000-8000-000000000001"
            );
            assert_eq!(page["record_fragment"]["step"], 1);
            assert_eq!(page["record_fragment"]["parent_step"], 0);
            reconstructed.push_str(page["record_fragment"]["text"].as_str().unwrap());
        }
        let Some(cursor) = page["next_cursor"].as_str() else {
            assert_eq!(reconstructed, signature);
            return;
        };
        args = json!({"cursor":cursor,"max_output_bytes":4000});
        page = crate::trace_pages::resume(cursor, &context(), 4000).unwrap();
    }
    panic!("bounded Unicode continuation did not terminate");
}

#[test]
fn trace_errors_keep_the_existing_plain_error_representation() {
    let mut expected = json!({"isError":true, "content":[{"type":"text", "text":json!({"error":"trace refused", "_kin":{"verdict":{"state":"inconclusive"}}}).to_string()}]});
    let before = expected.clone();
    crate::plain_reply::present(&mut expected, "trace_data_flow");
    let response = JsonRpcResponse::success(Some(json!(1)), before);
    assert_eq!(
        present_for_client(response, &request(json!({"focal":"focal"})), &config())
            .result
            .unwrap(),
        expected
    );
}

/// A frozen callers walk says on every page where its reach rows end. The
/// first page, carrying them all, does not invite paging for the display
/// chain left, and a page past them is display detail with no verdict of its
/// own, on the default plain connection (details) and the compact one alike.
#[test]
fn a_chain_page_past_the_reach_rows_never_regrades_the_answer() {
    let _guard = crate::trace_pages::cache_test_guard();
    let page = crate::trace_pages::start(payload(), context(), 4000).unwrap();
    let meta = &page["_kin"]["page"];
    let answer_end = meta["total_answer_records"].as_u64().unwrap();
    assert!(answer_end >= 5, "{meta}");
    assert_eq!(meta["answer_rows_complete"], true, "{meta}");
    assert_eq!(meta["evidence_has_more"], true, "{meta}");
    let mut details = config();
    details.compact_trace_evidence = false;
    let first = decoded(&presented(
        &page,
        json!({"focal":"focal","direction":"callers","depth":8,"details":true,"max_output_bytes":4000}),
        &details,
    ));
    let delivered = first["_kin"]["delivered"].as_str().unwrap();
    assert!(
        delivered.starts_with("All 3 recorded reach members")
            && delivered.contains("only the display chain"),
        "{delivered}"
    );
    assert!(first["_kin"]["verdict"].is_string(), "{first}");

    let mut cursor = page["next_cursor"].as_str().unwrap().to_string();
    let mut seen = 0;
    loop {
        let next = crate::trace_pages::resume(&cursor, &context(), 4000).unwrap();
        assert!(next["_kin"]["page"]["record_offset"].as_u64().unwrap() >= answer_end);
        if next.get("record_fragment").is_none() {
            for config in [details.clone(), config()] {
                let reply = decoded(&presented(
                    &next,
                    json!({"cursor":cursor,"max_output_bytes":4000}),
                    &config,
                ));
                let meaning = reply["_kin"]["meaning"].as_str().unwrap();
                assert!(meaning.starts_with("Display detail"), "{meaning}");
                assert!(meaning.contains("Encoder.encode"), "{meaning}");
                assert!(reply["_kin"].get("verdict").is_none(), "{reply}");
                assert!(!meaning.contains("Partial"), "{meaning}");
                seen += 1;
            }
        }
        match next["next_cursor"].as_str() {
            Some(more) => cursor = more.to_string(),
            None => break,
        }
    }
    assert!(seen > 0, "no whole continuation page was presented");
}

const UNSCOPED_REACH: &str = include_str!("testdata/plain_react_reach_unscoped_trace_page.json");
const SCOPED_REACH: &str = include_str!("testdata/plain_react_reach_scoped_trace_page.json");

/// A captured machine page as an agent connection writes it in text.
fn agent_text(fixture: &str, arguments: Value) -> String {
    let page: Value = serde_json::from_str(fixture).unwrap();
    // The default agent profile: plain replies, without the trace aliases
    // the explicit compact profiles opt into.
    let agent = McpServerConfig {
        plain_replies: true,
        agent_belt: true,
        ..Default::default()
    };
    let mut result = presented(&page, arguments, &agent);
    crate::lean_reply::lean_result(&mut result);
    let handles = crate::session_handles::SessionHandles::default();
    crate::text_reply::render_result(&mut result, "trace_data_flow", &handles);
    result["content"][0]["text"].as_str().unwrap().to_string()
}

/// How strong an agent reply's first line says its answer is.
fn strength(text: &str) -> u8 {
    let line = text.lines().next().unwrap_or_default();
    if line.starts_with("Answer (complete):") {
        2
    } else if line.starts_with("Answer (complete except") {
        1
    } else {
        assert!(line.starts_with("Answer (partial"), "{line}");
        0
    }
}

/// The same nine members, read with a scope and without, on one store. The
/// scoped walk's chain was cut by a different cap than the unscoped one's,
/// and that used to grade the scoped answer partial: the three unresolved
/// call sites in a test file inside the scope were then listed one by one,
/// with the counts, the cursor and the delivery note an answered reach
/// leaves out. A scoped reach grades at least as strong as the same reach
/// unscoped, and ends on the same lines.
#[test]
fn a_scoped_reach_grades_at_least_as_strong_as_the_same_reach_unscoped() {
    let asked = json!({"focal": "isFiberSuspenseAndTimedOut", "direction": "callers", "depth": 8});
    let mut within = asked.clone();
    within["scope"] = json!("react-reconciler");
    let unscoped = agent_text(UNSCOPED_REACH, asked);
    let scoped = agent_text(SCOPED_REACH, within);

    assert_eq!(strength(&unscoped), 1, "{unscoped}");
    assert!(strength(&scoped) >= strength(&unscoped), "{scoped}");
    assert_eq!(
        scoped.lines().next().unwrap(),
        "Answer (complete except the sites listed below): these 9 functions in react-reconciler \
         are every direct and indirect caller of isFiberSuspenseAndTimedOut, on paths that stay \
         inside it; Kin followed callers of callers until no new function appeared (3 call it \
         directly, 6 through other calls)."
    );
    // The unresolved test call sites are one counted line in both.
    for text in [&scoped, &unscoped] {
        assert!(
            text.contains("\n  3 call sites in test code (ReactScope-test.internal) [test]\n"),
            "{text}"
        );
        assert!(!text.contains("no resolver proved"), "{text}");
    }
    // Every row is on the page, so nothing invites another call.
    for gone in [
        "next_cursor",
        "delivered:",
        "102806",
        "871 lookups",
        "total_steps",
        "chain_details",
        "imported without a call",
    ] {
        assert!(!scoped.contains(gone), "{gone}: {scoped}");
    }
    // After its answer line the scoped reply is the unscoped one.
    assert_eq!(
        scoped.lines().skip(1).collect::<Vec<_>>(),
        unscoped.lines().skip(1).collect::<Vec<_>>()
    );
}

/// The scoped page as a walk that leaves tests out would have served it:
/// the three places in test code are counted, not frontier rows.
fn scoped_without_tests(keep_value_holder: bool) -> String {
    let mut page: Value = serde_json::from_str(SCOPED_REACH).unwrap();
    page["reach"]["frontier"] = json!([]);
    let reading = page["readings"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|reading| reading["key"] == "reach")
        .unwrap();
    let block = reading["value"].as_object_mut().unwrap();
    block.insert("frontier_total".into(), json!(0));
    block.insert("state".into(), json!("closed"));
    block.remove(crate::trace_reach::FRONTIER_IN_TESTS_KEY);
    block.insert(
        crate::trace_reach::TESTS_EXCLUDED_KEY.into(),
        json!({"call_sites": 3, "callers": 1}),
    );
    if !keep_value_holder {
        block.insert("also_referenced_total".into(), json!(0));
        block.insert("blind_spots".into(), json!({}));
        page["reach"]["also_referenced_by"] = json!([]);
    }
    let asked = json!({"focal": "isFiberSuspenseAndTimedOut", "direction": "callers",
        "depth": 8, "scope": "react-reconciler", "include_tests": false});
    agent_text(&page.to_string(), asked)
}

/// A scoped walk that leaves tests out answers a question that excludes
/// them in one reply. With nothing else open it is plainly complete: one
/// line saying so in the words a reference answer uses, the members, and no
/// section of places left, no policy line, no cursor. The places in test
/// code are a count on the answer line and never rows.
#[test]
fn a_walk_without_tests_is_complete_for_non_test_code_when_nothing_else_is_open() {
    let text = scoped_without_tests(false);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "Answer (complete): these 9 functions in react-reconciler are every direct and indirect \
         caller of isFiberSuspenseAndTimedOut, on paths that stay inside it; Kin followed callers \
         of callers until no new function appeared (3 call it directly, 6 through other calls). \
         Complete for non-test code: test code was excluded, as asked (3 call sites in test code \
         left out)."
    );
    assert_eq!(lines.len(), 10, "the answer line and nine members: {text}");
    for gone in [
        "Not followed",
        "by design",
        "[test]",
        "next_cursor",
        "delivered:",
    ] {
        assert!(!text.contains(gone), "{gone}: {text}");
    }
}

/// With a place still open outside test code, the walk is complete except
/// that place, and says the exclusion in the words for an answer that is
/// not whole. The test places stay a count.
#[test]
fn a_walk_without_tests_still_lists_what_is_open_outside_them() {
    let text = scoped_without_tests(true);
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[0].starts_with("Answer (complete except the sites listed below): these 9 functions"),
        "{text}"
    );
    assert!(
        lines[0].ends_with(
            "Test code was excluded, as asked; this covers non-test code (3 call sites in test \
             code left out)."
        ),
        "{text}"
    );
    assert_eq!(
        &lines[10..],
        [
            "Not followed:",
            "  held as a value in createScopeInstance #5b2f692b: DO_NOT_USE_queryAllNodes, \
             DO_NOT_USE_queryFirstNode, getChildContextValues (collection)",
            "  by design: calls made later through a member held as a value, and calls resolved \
             to a method a member overrides",
        ],
        "{text}"
    );
    assert!(!text.contains("[test]"), "{text}");
}

/// Four reach answers that held every caller and still graded partial,
/// captured from one build. Each reach finished its reading: every row is on
/// the page and no bound stopped it. What is left is of kinds no further
/// call settles (a member a decorator or a protocol can run, a member held
/// as a value, a lookup by a built name), so each is complete except those,
/// said once per kind, and ends there: no scope block, focal row, walk
/// parameters, chain note or cursor.
#[test]
fn a_finished_reach_is_complete_except_what_it_lists_and_ends_on_it() {
    let cases = [
        (
            include_str!("testdata/plain_flask_find_error_handler_trace_page.json"),
            Some("flask"),
            "these 6 functions in flask are every direct and indirect caller of \
             App._find_error_handler",
        ),
        (
            include_str!("testdata/plain_flask_scaffold_route_trace_page.json"),
            Some("flask"),
            "these 7 functions in flask are every direct and indirect caller of Scaffold.route",
        ),
        (
            include_str!("testdata/plain_flask_get_root_path_trace_page.json"),
            Some("flask"),
            "these 5 functions in flask are every direct and indirect caller of get_root_path",
        ),
        (
            include_str!("testdata/plain_react_advance_timers_trace_page.json"),
            None,
            "these 4 functions are every direct and indirect caller of advanceTimers",
        ),
    ];
    for (fixture, scope, claim) in cases {
        let mut asked = json!({"focal": "x", "direction": "callers", "depth": 8});
        if let Some(scope) = scope {
            asked["scope"] = json!(scope);
        }
        let text = agent_text(fixture, asked);
        assert_eq!(strength(&text), 1, "{text}");
        assert!(
            text.lines().next().unwrap().contains(claim),
            "{claim}: {text}"
        );
        for gone in [
            "\nentity:",
            "\nscope:",
            "truncated:",
            "type_uses_withheld",
            "terminal_leaf_steps",
            "\ndepth:",
            "total_steps",
            "chain_details",
            "narrow:",
            "next_cursor",
            "delivered:",
            "modules",
            "value holders:",
            "complete=false",
            "calls through values",
            "lookups by a name built",
        ] {
            assert!(!text.contains(gone), "{gone:?}: {text}");
        }
        assert!(text.len() < 2_400, "{} bytes: {text}", text.len());
    }

    // Seven decorated methods are one counted line, not seven declarations.
    let route = agent_text(
        include_str!("testdata/plain_flask_scaffold_route_trace_page.json"),
        json!({"focal": "x", "direction": "callers", "depth": 8, "scope": "flask"}),
    );
    let unspelled: Vec<&str> = route
        .lines()
        .filter(|line| line.contains("the language can run"))
        .collect();
    assert_eq!(
        unspelled,
        [
            "  7 members the language can run without a call that spells their names, through a \
          decorator or a protocol, and no record lists where: route, patch, query, put, post, \
          delete, get"
        ],
        "{route}"
    );
    assert!(!route.contains("@setupmethod"), "{route}");
    // A fold says its reason in words, and a holder nobody listed is a line.
    let handler = agent_text(
        include_str!("testdata/plain_flask_find_error_handler_trace_page.json"),
        json!({"focal": "x", "direction": "callers", "depth": 8, "scope": "flask"}),
    );
    assert!(
        handler.contains(
            "\n  1 observation counted by reason, not listed: calling an instance runs \
             `__call__` 1\n  1 more place holds a member as a value and is not listed\n"
        ),
        "{handler}"
    );
}

/// The page of an impact answer captured from a build that predates the
/// module and the list totals on its rows, with both added the way the
/// handler now writes them.
fn impact_page() -> Value {
    let mut page: Value = serde_json::from_str(include_str!(
        "testdata/plain_flask_impact_open_session_page.json"
    ))
    .unwrap();
    for (list, modules) in [
        ("affected_callers", vec!["flask.ctx", "flask.testing"]),
        (
            "affected_overrides",
            vec!["tests.test_reqctx", "flask.sessions", "tests.test_reqctx"],
        ),
    ] {
        for (row, module) in page[list].as_array_mut().unwrap().iter_mut().zip(modules) {
            row["module"] = json!(module);
        }
    }
    let totals = json!({"affected_callers": 2, "affected_overrides": 3, "affected_dependents": 24});
    page["readings"]
        .as_array_mut()
        .unwrap()
        .push(json!({"key": crate::handlers::review::IMPACT_ROW_TOTALS_KEY, "value": totals}));
    page
}

fn impact_text(page: &Value) -> String {
    let agent = McpServerConfig {
        plain_replies: true,
        agent_belt: true,
        ..Default::default()
    };
    let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"impact","arguments":{"entity_ids":["x"]}}})
    .to_string();
    let response = JsonRpcResponse::success(
        Some(json!(1)),
        json!({"content":[{"type":"text","text":page.to_string()}]}),
    );
    let mut result = present_for_client(response, &request, &agent)
        .result
        .unwrap();
    crate::lean_reply::lean_result(&mut result);
    let handles = crate::session_handles::SessionHandles::default();
    crate::text_reply::render_result(&mut result, "impact_analysis", &handles);
    result["content"][0]["text"].as_str().unwrap().to_string()
}

/// `page` with one more leading reading, as the handler writes one.
fn with_reading(mut page: Value, key: &str, value: Value) -> Value {
    page["readings"]
        .as_array_mut()
        .unwrap()
        .push(json!({"key": key, "value": value}));
    page
}

/// An impact answer is its rows. The same page ran to 7,624 bytes of text:
/// under `Answer (complete)` it printed each section's verdict with its
/// seventeen inputs (`state=inconclusive` among them), the implementation
/// family's counters, obligations and note, a call-site reading with sample
/// lookups, every caller a second time among the dependents, a signature on
/// every row and a cursor. It now says what a change affects, then lists
/// the callers and the overriding methods as their own group, each entity
/// once, by module and name. What depends on those without calling the
/// changed entity is what else a change could reach: it is counted, and
/// listed when asked for.
#[test]
fn an_impact_answer_lists_each_affected_entity_once_and_nothing_else() {
    let text = impact_text(&impact_page());
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "Answer (complete): a change to it affects 2 callers in affected_callers and 3 methods \
         that override or implement it in affected_overrides, each listed once. 17 more \
         entities and 5 modules depend on those without calling it and are not listed: pass \
         dependents true to list them."
    );
    assert_eq!(
        &lines[1..],
        [
            "affected_callers (2):",
            "  flask.ctx :: AppContext._get_session",
            "  flask.testing :: FlaskClient.session_transaction",
            "affected_overrides (3):",
            "  tests.test_reqctx :: \
             test_open_session_with_endpoint.<locals>.MySessionInterface.open_session  [test]",
            "  flask.sessions :: SecureCookieSessionInterface.open_session",
            "  tests.test_reqctx :: \
             test_session_error_pops_context.<locals>.FailingSessionInterface.open_session  [test]",
        ],
        "{text}"
    );
    assert!(text.len() < 800, "{} bytes: {text}", text.len());

    // Asked for, the dependents are listed, each once and none of them a
    // caller or an override again.
    let asked = with_reading(
        impact_page(),
        crate::handlers::review::IMPACT_DEPENDENTS_ASKED_KEY,
        json!(true),
    );
    let text = impact_text(&asked);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "Answer (complete): a change to it affects 2 callers in affected_callers and 3 methods \
         that override or implement it in affected_overrides, each listed once. 17 more \
         entities depend on those without calling it, in affected_dependents, and 5 modules."
    );
    assert_eq!(lines[8], "affected_dependents (17):", "{text}");
    assert_eq!(lines.len(), 9 + 17, "{text}");
    for again in ["_get_session", "session_transaction", ".open_session"] {
        assert!(
            lines[9..].iter().all(|line| !line.contains(again)),
            "{again}: {text}"
        );
    }
    assert!(text.len() < 1_400, "{} bytes: {text}", text.len());

    for text in [impact_text(&impact_page()), text] {
        for gone in [
            "sections",
            "verdict",
            "inconclusive",
            "implementation_family",
            "obligations",
            "coverage:",
            "focal_escape",
            "name_unique",
            "affected_row_totals",
            "dependents_asked",
            "def ",
            "calls=",
            "(module)",
            "next_cursor",
            "delivered:",
            "searched:",
        ] {
            let rows = text.lines().skip(1).collect::<Vec<_>>().join("\n");
            assert!(!rows.contains(gone), "{gone}: {text}");
        }
    }
}

/// A reply ends on a cursor only when a list it shows is cut. The
/// dependents it only counts are no reason for one, however many pages they
/// fill: an agent that sees a cursor fetches it.
#[test]
fn an_impact_answer_keeps_its_cursor_only_for_a_list_it_shows() {
    let cut = |list: &str, total: u64| {
        let mut page = impact_page();
        let last = page["readings"].as_array().unwrap().len() - 1;
        page["readings"][last]["value"][list] = json!(total);
        page
    };
    // Sixty dependents, twenty-four of them on this page, none asked for.
    let text = impact_text(&cut("affected_dependents", 60));
    assert!(
        text.lines().next().unwrap().ends_with(
            "Up to 60 more entities depend on those without calling it and are not listed: \
             pass dependents true to list them."
        ),
        "{text}"
    );
    assert!(!text.contains("next_cursor"), "{text}");
    assert!(!text.contains("delivered:"), "{text}");
    // Asked for, the rest of them are a page away.
    let asked = with_reading(
        cut("affected_dependents", 60),
        crate::handlers::review::IMPACT_DEPENDENTS_ASKED_KEY,
        json!(true),
    );
    let text = impact_text(&asked);
    assert!(text.contains("\nnext_cursor: p"), "{text}");
    assert!(text.contains("\ndelivered: "), "{text}");
    // A cut list of overrides is a cut answer, asked or not.
    let text = impact_text(&cut("affected_overrides", 9));
    assert!(text.contains("\nnext_cursor: p"), "{text}");
    assert!(text.contains("\ndelivered: "), "{text}");
}

/// Asked to leave test code out, the answer says so in the words a
/// reference answer and a callers walk use, with how many affected entities
/// that left out.
#[test]
fn an_impact_answer_that_left_tests_out_says_so_on_its_answer_line() {
    let mut page = impact_page();
    page["affected_overrides"]
        .as_array_mut()
        .unwrap()
        .retain(|row| row["role"] != "test");
    let last = page["readings"].as_array().unwrap().len() - 1;
    page["readings"][last]["value"]["affected_overrides"] = json!(1);
    let page = with_reading(
        page,
        crate::handlers::review::IMPACT_TESTS_EXCLUDED_KEY,
        json!({"entities": 2}),
    );
    let text = impact_text(&page);
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[0].starts_with(
            "Answer (complete): a change to it affects 2 callers in affected_callers and 1 \
             method that overrides or implements it in affected_overrides, each listed once. \
             Complete for non-test code: test code was excluded, as asked (2 affected entities \
             in test code left out). "
        ),
        "{text}"
    );
    assert_eq!(
        &lines[1..],
        [
            "affected_callers (2):",
            "  flask.ctx :: AppContext._get_session",
            "  flask.testing :: FlaskClient.session_transaction",
            "affected_overrides (1):",
            "  flask.sessions :: SecureCookieSessionInterface.open_session",
        ],
        "{text}"
    );
    assert!(!text.contains("tests_excluded"), "{text}");
}

/// Confined to a module path, an impact answer says so on its answer line
/// with how many affected entities sat outside it.
#[test]
fn an_impact_answer_confined_to_a_module_path_says_so_and_counts_the_rest() {
    let page = with_reading(
        impact_page(),
        crate::handlers::review::IMPACT_ROW_SCOPE_KEY,
        json!({"module": "flask", "outside": 4}),
    );
    let text = impact_text(&page);
    let lead = text.lines().next().unwrap();
    assert!(
        lead.contains(
            "each listed once. Only what sits inside flask is listed, as asked; 4 affected \
             entities outside it are not listed."
        ),
        "{text}"
    );
    assert!(!text.contains("row_scope"), "{text}");
}

/// Each caller row shows the call it makes, and a place that holds the
/// changed method or an override as a value is a row in a short group of
/// its own, beside the callers, with the line that hands it on. The answer
/// line counts it, and it is not listed again among the dependents.
#[test]
fn an_impact_answer_shows_each_call_and_the_places_that_hold_it_as_a_value() {
    const HOLDER: &str = "Flask.process_response";
    let mut page = impact_page();
    page["affected_callers"][0]["code"] =
        json!("sess = self.app.session_interface.open_session(self.app, self.request)");
    // One of the dependents is such a place: the handler lists it here.
    let holder = page["affected_dependents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == HOLDER)
        .cloned()
        .expect("the captured page lists it as a dependent");
    page[crate::handlers::review::IMPACT_VALUE_USES_KEY] = json!([
        {"id": holder["id"], "name": HOLDER, "kind": "method", "role": "source",
         "module": "flask.app", "uses": "SecureCookieSessionInterface.open_session",
         "code": "self.open = SecureCookieSessionInterface.open_session"},
    ]);
    let last = page["readings"].as_array().unwrap().len() - 1;
    page["readings"][last]["value"][crate::handlers::review::IMPACT_VALUE_USES_KEY] = json!(1);
    let page = with_reading(
        page,
        crate::handlers::review::IMPACT_DEPENDENTS_ASKED_KEY,
        json!(true),
    );
    let text = impact_text(&page);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "Answer (complete): a change to it affects 2 callers in affected_callers and 3 methods \
         that override or implement it in affected_overrides, and 1 place that holds it or an \
         override as a value in affected_value_uses, each listed once. 16 more entities depend \
         on those without calling it, in affected_dependents, and 5 modules.",
        "{text}"
    );
    assert_eq!(
        &lines[1..6],
        [
            "affected_callers (2):",
            "  flask.ctx :: AppContext._get_session  sess = \
             self.app.session_interface.open_session(self.app, self.request)",
            "  flask.testing :: FlaskClient.session_transaction",
            "affected_value_uses (1):",
            "  flask.app :: Flask.process_response  self.open = \
             SecureCookieSessionInterface.open_session  \
             uses=SecureCookieSessionInterface.open_session",
        ],
        "{text}"
    );
    assert_eq!(lines[6], "affected_overrides (3):", "{text}");
    // The holder is a row once.
    assert_eq!(text.matches(HOLDER).count(), 1, "{text}");
}

/// A function whose implementations are swapped by module, as a renderer's
/// host config is: the answer names the held siblings as rows, each by the
/// module that tells it from the others, counts the ones Kin holds no
/// entity for, and says on its answer line that each needs the same change.
#[test]
fn an_impact_answer_names_same_named_siblings_by_module_and_counts_the_rest() {
    let mut page = impact_page();
    page[crate::impact_sections::SAME_NAME_KEY] = json!([
        {"id": "30c87138-0000-4000-8000-000000000001", "name": "commitTextUpdate",
         "kind": "function", "role": "source", "resolution": "same_name",
         "module": "react-art.ReactFiberConfigART",
         "signature": "function commitTextUpdate(textInstance, oldText, newText)"},
        {"id": "effa6be2-0000-4000-8000-000000000002", "name": "commitTextUpdate",
         "kind": "function", "role": "source", "resolution": "same_name",
         "module": "react-test-renderer.ReactFiberConfigTestHost",
         "signature": "function commitTextUpdate(textInstance: TextInstance): void"},
    ]);
    page[crate::impact_sections::SAME_NAME_NOTE_KEY] =
        json!(crate::impact_sections::SAME_NAME_NOTE);
    page[crate::impact_sections::SAME_NAME_NOT_HELD_KEY] = json!(4);
    let text = impact_text(&page);
    let lead = text.lines().next().unwrap();
    assert!(
        lead.ends_with(
            "2 other declarations have its name and no link to it, in \
             same_name_implementations: where implementations are swapped by module, as a host \
             config is, each needs the same change. 4 more declarations of that name are a \
             variable or a re-export Kin holds no entity for; locate by the name lists them."
        ),
        "{lead}"
    );
    assert!(
        text.contains(
            "\nsame_name_implementations (2):\n  react-art.ReactFiberConfigART :: \
             commitTextUpdate\n  react-test-renderer.ReactFiberConfigTestHost :: commitTextUpdate"
        ),
        "{text}"
    );
    for gone in [
        "same_name_implementations_note",
        "same_name_not_held",
        "resolution=",
        "function commitTextUpdate(",
    ] {
        assert!(!text.contains(gone), "{gone}: {text}");
    }
}

/// A reference answer as an agent on the default profile reads it.
fn refs_text(page: &Value) -> String {
    let agent = McpServerConfig {
        plain_replies: true,
        agent_belt: true,
        ..Default::default()
    };
    let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"refs","arguments":{"entity_id":"x"}}})
    .to_string();
    let response = JsonRpcResponse::success(
        Some(json!(1)),
        json!({"content":[{"type":"text","text":page.to_string()}]}),
    );
    let mut result = present_for_client(response, &request, &agent)
        .result
        .unwrap();
    crate::lean_reply::lean_result(&mut result);
    let handles = crate::session_handles::SessionHandles::default();
    crate::text_reply::render_result(&mut result, "find_references", &handles);
    result["content"][0]["text"].as_str().unwrap().to_string()
}

/// The line index of a heading in a text reply.
fn at_in(lines: &[&str], heading: &str, text: &str) -> usize {
    lines
        .iter()
        .position(|line| *line == heading)
        .unwrap_or_else(|| panic!("{heading}: {text}"))
}

/// "Which functions use it" was answered with the importing module in the
/// same list as the functions, and a reader copied it out as one of them.
/// A module that imports or re-exports the entity and makes no call to it
/// is listed under its own heading after the functions, and the answer line
/// counts the two apart. A module whose top-level code calls it is listed
/// apart too, under its own heading.
#[test]
fn a_reference_answer_lists_importing_modules_apart_from_the_functions() {
    let row = |id: u8, name: &str, kind: &str, kinds: Value| {
        json!({"entity_id": format!("00000000-0000-4000-8000-0000000000{id:02}"),
            "name": name, "kind": kind, "role": "source",
            "module": "react-dom-bindings.ReactFiberConfigDOM",
            "relation_kinds": kinds, "resolution": "import_scoped"})
    };
    let page = |rows: Vec<Value>| {
        json!({
            "focal_entity": {"entity_id": "00000000-0000-4000-8000-000000000001",
                "name": "setEnabled", "kind": "Function"},
            "relation_kinds": ["calls", "imports", "references"],
            "total_upstream": rows.len(),
            "references": rows,
            "_kin": {"verdict": {"state": "certified", "safe_to_conclude_absent": false}},
        })
    };
    let module = row(
        2,
        "ReactFiberConfigDOM",
        "Module",
        json!(["imports", "references"]),
    );
    let callers = vec![
        row(
            3,
            "prepareForCommit",
            "Function",
            json!(["calls", "references"]),
        ),
        row(
            4,
            "resetAfterCommit",
            "Function",
            json!(["calls", "references"]),
        ),
    ];
    let mut rows = vec![module.clone()];
    rows.extend(callers.clone());
    let text = refs_text(&page(rows));
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[0].contains(
            "these 2 functions use setEnabled; 1 module imports or re-exports it at module \
             level, listed apart."
        ),
        "{text}"
    );
    let at = |heading: &str| {
        lines
            .iter()
            .position(|line| *line == heading)
            .unwrap_or_else(|| panic!("{heading}: {text}"))
    };
    let functions = at("references (2):");
    let modules = at("imported_or_reexported_at_module_level_by (1):");
    assert!(functions < modules, "{text}");
    for (line, name) in [
        (functions + 1, "prepareForCommit"),
        (functions + 2, "resetAfterCommit"),
    ] {
        assert!(lines[line].contains(name), "{text}");
    }
    assert!(
        lines[modules + 1].contains("ReactFiberConfigDOM  (Module)"),
        "{text}"
    );
    assert!(
        !lines[functions + 1..modules]
            .iter()
            .any(|line| line.contains("(Module)")),
        "{text}"
    );

    // Imported and never used: no function uses it, and the answer does not
    // say that nothing does.
    let text = refs_text(&page(vec![module.clone()]));
    let lead = text.lines().next().unwrap();
    assert!(
        lead.contains(
            "no function or method uses setEnabled; 1 module imports or re-exports it at \
             module level, listed apart."
        ),
        "{text}"
    );
    assert!(!lead.contains("found none"), "{text}");
    assert!(!lead.contains("nothing uses"), "{text}");
    assert!(
        text.contains("\nimported_or_reexported_at_module_level_by (1):"),
        "{text}"
    );

    // A module whose own top-level code calls it uses it, and is still no
    // function: a test file's `describe` and `it` blocks call through code
    // that belongs to the module. It is listed apart under its own heading,
    // so a reader asked which functions call it does not copy the module.
    let mut top_level = module.clone();
    top_level["relation_kinds"] = json!(["calls", "imports", "references"]);
    let mut rows = vec![top_level.clone()];
    rows.extend(callers.clone());
    let text = refs_text(&page(rows));
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[0].contains(
            "these 2 functions call setEnabled; 1 module uses it in top-level code or the \
             callbacks that code passes, no function, listed apart under \
             used_by_module_level_code_of."
        ),
        "{text}"
    );
    assert!(!text.contains("imported_or_reexported"), "{text}");
    let functions = at_in(&lines, "references (2):", &text);
    let module_code = at_in(&lines, "used_by_module_level_code_of (1):", &text);
    assert!(functions < module_code, "{text}");
    assert!(
        lines[module_code + 1].contains("ReactFiberConfigDOM  (Module)"),
        "{text}"
    );
    assert!(
        !lines[functions + 1..module_code]
            .iter()
            .any(|line| line.contains("(Module)")),
        "{text}"
    );

    // A module whose top-level code Kin matched to it by the name alone is
    // no proven use. It is counted with the other name-matched rows, as the
    // answer's own account counts it, and never read as a module whose code
    // uses the entity.
    let mut by_name = top_level;
    by_name["resolution"] = json!("name_only");
    let mut rows = vec![by_name];
    rows.extend(callers);
    let text = refs_text(&page(rows));
    let lead = text.lines().next().unwrap();
    assert!(
        lead.contains(
            "are proven to call setEnabled, and 1 more calls setEnabled by a name Kin matched \
             to setEnabled alone; name_only lists it"
        ),
        "{text}"
    );
    assert!(!lead.contains("top-level code"), "{text}");
    assert!(!text.contains("used_by_module_level_code_of"), "{text}");

    // Asked who imports a module, its importers are the answer.
    let mut about_module = page(vec![module]);
    about_module["focal_entity"]["kind"] = json!("Module");
    let text = refs_text(&about_module);
    assert!(!text.contains("imported_or_reexported"), "{text}");
    assert!(text.contains("\nreferences (1):"), "{text}");
}

/// A row shows every call its caller makes, not the first and a count of
/// the rest: two different calls are two lines of code on the row, and one
/// call written the same way twice says so. Only a site no line stands for
/// is still a count.
#[test]
fn a_reference_row_shows_every_call_its_caller_makes() {
    let site = |code: &str, line: u64| json!({"expr": code, "line_in_entity": line});
    let row = |id: u8, name: &str, sites: Vec<Value>| {
        json!({"entity_id": format!("00000000-0000-4000-8000-0000000000{id:02}"),
            "name": name, "kind": "Function", "role": "source",
            "module": "react-server.ReactFizzServer",
            "relation_kinds": ["calls"], "resolution": "import_scoped", "sites": sites})
    };
    let many: Vec<Value> = (0..10)
        .map(|index| site(&format!("pushSegmentFinale(chunk{index})"), index))
        .collect();
    let page = json!({
        "focal_entity": {"entity_id": "00000000-0000-4000-8000-000000000001",
            "name": "pushSegmentFinale", "kind": "Function"},
        "relation_kinds": ["calls"],
        "total_upstream": 3,
        "references": [
            row(2, "renderSuspenseBoundary", vec![
                site("pushSegmentFinale(boundarySegment.chunks)", 40),
                site("pushSegmentFinale(contentRootSegment.chunks)", 71),
            ]),
            row(3, "afterActiveInstanceBlur", vec![
                site("pushSegmentFinale(chunks)", 3),
                site("pushSegmentFinale(chunks)", 9),
            ]),
            row(4, "flushEverything", many),
        ],
        "_kin": {"verdict": {"state": "certified", "safe_to_conclude_absent": false}},
    });
    let text = refs_text(&page);
    let line = |name: &str| {
        text.lines()
            .find(|line| line.contains(name))
            .unwrap_or_else(|| panic!("{name}: {text}"))
            .to_string()
    };
    assert!(
        line("renderSuspenseBoundary").contains(
            "pushSegmentFinale(boundarySegment.chunks)  \
             pushSegmentFinale(contentRootSegment.chunks)"
        ),
        "{text}"
    );
    assert!(
        line("afterActiveInstanceBlur").contains("pushSegmentFinale(chunks) (x2)"),
        "{text}"
    );
    for row in ["renderSuspenseBoundary", "afterActiveInstanceBlur"] {
        assert!(!line(row).contains("more_calls"), "{text}");
    }
    // Past the calls one row shows, the rest are counted.
    let long = line("flushEverything");
    assert!(
        long.contains("pushSegmentFinale(chunk0)  pushSegmentFinale(chunk1)")
            && long.contains("pushSegmentFinale(chunk7)")
            && !long.contains("pushSegmentFinale(chunk8)")
            && long.contains("more_calls=2"),
        "{text}"
    );
    assert!(!text.contains("more_call_code"), "{text}");
}
