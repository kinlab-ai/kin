// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A `refs` reply an agent is handed stays within its byte budget, and the
//! pages behind it return the whole frozen answer.
//!
//! The shape these tests start from is a React answer that found no caller of
//! `DO_NOT_USE_queryAllNodes` and still came back at 112,834 bytes against a
//! 60,000-byte request: 302 groups of retained coverage gaps, each restating
//! the same reason. The same gaps rode on every reference question in that run.

use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Map, Value};

use crate::plain_reply::{COMPLETE, GAP_REASONS_KEY, LEFT_TO_CHECK_KEY, PARTIAL};
use crate::server::{present_for_client, McpServerConfig};
use crate::trace_pages::PageKind;
use crate::types::JsonRpcResponse;

const FOCAL_ID: &str = "484f27c2-bdd8-41a0-b0fe-6afd4d7a7e28";
const FOCAL_NAME: &str = "DO_NOT_USE_queryAllNodes";

fn authority() -> Value {
    json!({"graph": "bounded-refs", "revision": 1})
}

fn arguments(value: Value) -> HashMap<String, Value> {
    serde_json::from_value(value).unwrap()
}

fn inconclusive() -> Value {
    json!({"state": "inconclusive", "absence_claim": "not_applicable",
        "safe_to_conclude_absent": false,
        "limiting_factor": "call_sites_unresolved",
        "inputs": {"call_sites": "inconclusive"}})
}

/// A retained gap the way an answer-only reply carries one: a caller whose
/// text Kin never read, collapsed per caller with how many entries it holds.
/// The state alternates the way it did in the React run, which is what split
/// the old reply into hundreds of groups.
fn gap(index: usize) -> Value {
    json!({
        "caller": format!("00000000-0000-4000-9000-{index:012}"),
        "caller_name": format!("module_{}", index % 41),
        "line_in_entity": null, "callee": null,
        "state": if index % 7 == 6 { "unresolved" } else if index % 2 == 0 { "unresolved" } else { "ambiguous" },
        "expr_unavailable": "caller_source_unavailable",
        "occurrences": 100 + (index * 37) % 105,
    })
}

/// A retained site whose code spells the asked name: the evidence that could
/// add a caller. Alternating states give every site a group of its own, so
/// its plain form states a reason per site and outgrows the machine row.
fn spelled(index: usize) -> Value {
    json!({
        "caller": format!("00000000-0000-4000-a000-{index:012}"),
        "caller_name": format!("callsite_{index}"),
        "line_in_entity": 3 + index % 11, "callee": FOCAL_NAME,
        "state": if index % 2 == 0 { "unresolved" } else { "ambiguous" },
        "expr": format!("{FOCAL_NAME}(container_{index}, selector)"),
        "receiver_hint": "unknown",
    })
}

/// A recorded caller, proven by imports, with its one call.
fn proven(index: usize) -> Value {
    json!({
        "entity_id": format!("00000000-0000-4000-8000-{index:012}"),
        "name": format!("queryHelper{index}"), "kind": "function",
        "qualified_name": format!("packages.react_dom.test_utils.queryHelper{index}"),
        "relation_kinds": ["calls"], "resolution": "import_scoped",
        "sites": [{"line_in_entity": 4 + index % 9,
            "expr": format!("{FOCAL_NAME}(root_{index}, predicate_{index})")}],
    })
}

fn answer(references: Vec<Value>, named: Vec<Value>, verdict: Value) -> Value {
    json!({
        "focal_entity": {"id": FOCAL_ID, "name": FOCAL_NAME, "kind": "function",
            "qualified_name": "packages.react_dom.test_utils.DO_NOT_USE_queryAllNodes"},
        "relation_kinds": ["calls"],
        "caller_scope": {"include_tests": true},
        "total_upstream": references.len(),
        "references": references,
        "call_site_candidates": {"named": named},
        "_kin": {
            "envelope_version": 2, "runtime": "repo-daemon",
            "repository": {"authority_context": "90b5939b2c0fa106b07d37b21b3cd84e29b9aee26c93ec32e11f2b2587906163"},
            "semantic_surface": 1,
            "graph_as_of": {"generation": 9, "graph_root": "captured-root"},
            "verdict": verdict,
        },
    })
}

fn request(arguments: &Value) -> String {
    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "refs", "arguments": arguments}})
    .to_string()
}

fn agent() -> McpServerConfig {
    McpServerConfig {
        plain_replies: true,
        agent_belt: true,
        ..Default::default()
    }
}

/// The machine page `page` as the agent connection hands it over for the
/// call `call`, and the bytes of every text block it carries.
fn presented(page: &Value, call: &Value, config: &McpServerConfig) -> (Value, usize) {
    let result = json!({"content": [{"type": "text", "text": page.to_string()}]});
    let response = present_for_client(
        JsonRpcResponse::success(Some(json!(1)), result),
        &request(call),
        config,
    );
    let result = response.result.expect("a presented result");
    let bytes = result["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|block| block["text"].as_str())
        .map(str::len)
        .sum();
    (result, bytes)
}

fn reply(result: &Value) -> Value {
    assert_ne!(result["isError"], true, "{result}");
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

/// Every machine page of `answer` asked as `call` at `max` bytes, the first
/// frozen the way the pager freezes a fresh answer and each later one resumed
/// from the cursor alone.
fn walk(answer: Value, call: &Value, max: usize) -> Vec<Value> {
    let first_context = crate::reference_pages::context(&arguments(call.clone()), &authority());
    let first =
        crate::trace_pages::start_kind(answer, first_context, max, PageKind::References).unwrap();
    let mut pages = vec![first];
    while let Some(cursor) = pages.last().unwrap()["next_cursor"].as_str() {
        let cursor = cursor.to_string();
        let context =
            crate::reference_pages::context(&arguments(json!({"cursor": cursor})), &authority());
        pages.push(crate::reference_pages::resume(&cursor, &context, max).unwrap());
        assert!(pages.len() < 2_000, "the walk does not end");
    }
    pages
}

/// The answer the pages add up to: every collection's rows in page order and
/// every reading, put back where the frozen answer held them.
fn reassembled(pages: &[Value]) -> Value {
    let mut rows: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    let mut readings = Map::new();
    for page in pages {
        assert!(
            page.get("record_fragment").is_none(),
            "fixture rows page whole"
        );
        assert!(page.pointer("/_kin/page/compacted").is_none());
        for &collection in PageKind::References.collections() {
            let path = format!("/{}", collection.replace('.', "/"));
            for row in page
                .pointer(&path)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                rows.entry(collection).or_default().push(row.clone());
            }
        }
        for reading in page["readings"].as_array().into_iter().flatten() {
            let key = reading["key"].as_str().unwrap().to_string();
            assert!(readings.insert(key, reading["value"].clone()).is_none());
        }
    }
    let mut whole = Value::Object(readings);
    for (collection, rows) in rows {
        let mut slot = &mut whole;
        for key in collection.split('.') {
            slot = &mut slot[key];
        }
        *slot = Value::Array(rows);
    }
    whole
}

fn projected(mut answer: Value) -> Value {
    crate::semantic_surface::project("find_references", &mut answer);
    answer
}

fn continuation(page: &Value, max: usize, details: bool) -> Option<Value> {
    let cursor = page["next_cursor"].as_str()?;
    let mut call = json!({"cursor": cursor, "max_output_bytes": max});
    if details {
        call["details"] = json!(true);
    }
    Some(call)
}

/// Every page of a walk, presented short and with details, within `max`.
fn assert_every_presentation_fits(pages: &[Value], first_call: &Value, max: usize) {
    for details in [false, true] {
        let config = agent();
        let mut call = first_call.clone();
        if details {
            call["details"] = json!(true);
        }
        for (index, page) in pages.iter().enumerate() {
            let (result, bytes) = presented(page, &call, &config);
            assert!(
                bytes <= max,
                "page {index} details={details}: {bytes} bytes over {max}: {result}"
            );
            assert_ne!(result["isError"], true, "page {index}: {result}");
            if let Some(next) = continuation(page, max, details) {
                call = next;
            }
        }
    }
}

#[test]
fn bounded_refs_react_shaped_gap_reply_is_a_few_kilobytes() {
    let _cache = crate::trace_pages::cache_test_guard();
    let call = json!({"entity_id": FOCAL_ID, "include_tests": true,
        "max_output_bytes": 60000, "relation_kinds": ["calls"]});
    let named: Vec<Value> = (0..1_200).map(gap).collect();
    let pages = walk(answer(vec![], named, inconclusive()), &call, 60_000);
    assert!(pages.len() > 1, "the gaps fill more than one page");
    let first = &pages[0];
    let page_entries: u64 = first["call_site_candidates"]["named"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["occurrences"].as_u64().unwrap())
        .sum();

    let (short, short_bytes) = presented(first, &call, &agent());
    let short = reply(&short);
    let mut detailed_call = call.clone();
    detailed_call["details"] = json!(true);
    let (detailed, detailed_bytes) = presented(first, &detailed_call, &agent());
    let detailed = reply(&detailed);
    eprintln!(
        "react-shaped refs page: {} gap records holding {page_entries} retained entries; \
         machine page {} bytes, short reply {short_bytes} bytes, details reply {detailed_bytes} bytes",
        first["call_site_candidates"]["named"].as_array().unwrap().len(),
        first.to_string().len(),
    );

    assert!(short_bytes <= 4_096, "{short_bytes}: {short}");
    assert!(detailed_bytes <= 60_000, "{detailed_bytes}");
    // One count by reason, one line on what the gaps are and how to list them.
    let groups = short[LEFT_TO_CHECK_KEY].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{short}");
    let folded = &groups[0];
    assert!(folded.get("sites").is_none());
    assert_eq!(folded["retained_entries"], page_entries);
    assert_eq!(
        folded[GAP_REASONS_KEY],
        json!({"caller_source_unavailable": page_entries})
    );
    let why = folded["why"].as_str().unwrap();
    assert!(
        why.contains(&format!("not observed calls to {FOCAL_NAME}")),
        "{why}"
    );
    assert!(why.contains("could add a caller only if"), "{why}");
    assert!(why.contains("details:true"), "{why}");
    // The fold is a count, never an absence: the answer stays partial.
    assert_eq!(short["_kin"]["verdict"], PARTIAL);
    let meaning = short["_kin"]["meaning"].as_str().unwrap();
    assert!(!meaning.contains("safe to conclude"), "{meaning}");
    assert!(!meaning.contains("nothing calls"), "{meaning}");
    assert!(short["evidence_next_cursor"].is_string());
    assert!(short["_kin"]["delivered"]
        .as_str()
        .unwrap()
        .contains("later pages"));
    // Details list the same entries under one reason, said once.
    let listed = detailed[LEFT_TO_CHECK_KEY].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    let listed_entries: u64 = listed[0]["sites"]
        .as_array()
        .unwrap()
        .iter()
        .map(|site| site["occurrences"].as_u64().unwrap_or(1))
        .sum();
    assert_eq!(listed_entries, page_entries);
}

#[test]
fn bounded_refs_walking_the_cursor_returns_every_record_byte_for_byte() {
    let _cache = crate::trace_pages::cache_test_guard();
    let references: Vec<Value> = (0..120).map(proven).collect();
    let mut named: Vec<Value> = (0..300).map(spelled).collect();
    named.extend((0..900).map(gap));
    let whole = answer(references, named, inconclusive());
    let expected = projected(whole.clone()).to_string();
    for max in [4_000, 12_000, 60_000] {
        let call = json!({"entity_id": FOCAL_ID, "include_tests": true,
            "max_output_bytes": max, "relation_kinds": ["calls"]});
        let pages = walk(whole.clone(), &call, max);
        let mut restored = reassembled(&pages);
        // The envelope rides as a reading, beside the page metadata the
        // pager adds; the answer is everything else.
        restored["_kin"] = projected(whole.clone())["_kin"].clone();
        assert_eq!(restored.to_string(), expected, "at {max} bytes");
        assert_every_presentation_fits(&pages, &call, max);
    }
}

#[test]
fn bounded_refs_proven_rows_lead_and_are_never_cut_silently() {
    let _cache = crate::trace_pages::cache_test_guard();
    let max = 12_000;
    let call = json!({"entity_id": FOCAL_ID, "max_output_bytes": max});
    let references: Vec<Value> = (0..200).map(proven).collect();
    let mut named: Vec<Value> = (0..50).map(spelled).collect();
    named.extend((0..400).map(gap));
    let pages = walk(
        answer(references.clone(), named, inconclusive()),
        &call,
        max,
    );
    let config = agent();
    let mut next_call = call.clone();
    let mut served: Vec<String> = Vec::new();
    for page in &pages {
        let (result, bytes) = presented(page, &next_call, &config);
        assert!(bytes <= max, "{bytes}");
        let plain = reply(&result);
        let rows = plain["references"].as_array().unwrap();
        served.extend(
            rows.iter()
                .map(|row| row["entity_id"].as_str().unwrap().to_string()),
        );
        let rows_left = references.len() - served.len();
        if rows_left > 0 {
            // A page that stopped before every recorded row says so, counts
            // them, and names the cursor that returns them.
            assert_eq!(plain["_kin"]["page"]["recorded_rows_has_more"], true);
            let delivered = plain["_kin"]["delivered"].as_str().unwrap();
            assert!(delivered.contains("answer_next_cursor"), "{delivered}");
            assert!(
                delivered.contains(&format!("{rows_left} of them recorded reference rows")),
                "{rows_left}: {delivered}"
            );
            assert!(plain["answer_next_cursor"].is_string());
            assert!(
                plain.get(LEFT_TO_CHECK_KEY).is_none(),
                "no evidence before the rows"
            );
        }
        if let Some(call) = continuation(page, max, false) {
            next_call = call;
        }
    }
    let expected: Vec<String> = references
        .iter()
        .map(|row| row["entity_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(served, expected);
}

#[test]
fn bounded_refs_an_answer_whose_presentation_outgrows_its_page_is_paged_by_it() {
    let _cache = crate::trace_pages::cache_test_guard();
    let max = 60_000;
    let call = json!({"entity_id": FOCAL_ID, "max_output_bytes": max});
    // Fits the budget as machine JSON, and states a reason for every site in
    // plain form: a complete page could not be cut and handed back.
    let mut named: Vec<Value> = (0..220).map(spelled).collect();
    named.extend((0..10).map(gap));
    let whole = answer(vec![], named.clone(), inconclusive());
    let machine = whole.to_string().len();
    let mut complete = whole.clone();
    complete["_kin"]["page"] = json!({"version": 1, "kind": "references", "complete": true});
    let (unbounded, _) = presented(
        &complete,
        &json!({"entity_id": FOCAL_ID, "details": true,
        "max_output_bytes": max}),
        &McpServerConfig {
            plain_replies: true,
            ..Default::default()
        },
    );
    assert!(machine < max, "{machine}");
    // Without paging by presentation this answer would be refused here.
    assert_eq!(
        unbounded["isError"], true,
        "machine {machine} bytes: {unbounded}"
    );

    let pages = walk(whole.clone(), &call, max);
    assert!(pages.len() > 1, "frozen rather than served whole");
    assert_eq!(
        reassembled(&pages)["call_site_candidates"]["named"],
        projected(whole)["call_site_candidates"]["named"]
    );
    assert_every_presentation_fits(&pages, &call, max);
}

#[test]
fn bounded_refs_an_explicit_budget_is_honored_and_a_default_applies() {
    let _cache = crate::trace_pages::cache_test_guard();
    let references: Vec<Value> = (0..60).map(proven).collect();
    let mut named: Vec<Value> = (0..80).map(spelled).collect();
    named.extend((0..300).map(gap));
    let whole = answer(references, named, inconclusive());
    for max in [2_000, 4_000, 9_000, 30_000, 60_000] {
        let call = json!({"entity_id": FOCAL_ID, "max_output_bytes": max});
        let pages = walk(whole.clone(), &call, max);
        assert_every_presentation_fits(&pages, &call, max);
    }
    // No budget named: an agent profile's default for a reference answer.
    let default = crate::agent_belt::agent_default_response_max_chars("find_references") as usize;
    let call = json!({"entity_id": FOCAL_ID});
    let pages = walk(whole.clone(), &call, default);
    for page in &pages {
        let (_, bytes) = presented(page, &call, &agent());
        assert!(bytes <= default, "{bytes}");
    }
}

#[test]
fn bounded_refs_a_presentation_over_its_budget_is_refused_not_served() {
    // A page that never went through the pager, larger than its budget once
    // presented: the client is refused, never handed more than it asked for.
    let mut named: Vec<Value> = (0..170).map(spelled).collect();
    named.extend((0..20).map(gap));
    let mut page = answer(vec![], named, inconclusive());
    page["_kin"]["page"] = json!({"version": 1, "kind": "references", "complete": true});
    let call = json!({"entity_id": FOCAL_ID, "details": true, "max_output_bytes": 30000});
    let (result, bytes) = presented(&page, &call, &agent());
    assert!(bytes <= 30_000, "{bytes}");
    assert_eq!(result["isError"], true);
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("response_budget_exceeded"));
    // A machine connection keeps the pager's own accounting.
    let machine = McpServerConfig::default();
    let (result, _) = presented(&page, &call, &machine);
    assert_ne!(result["isError"], true);
}

#[test]
fn bounded_refs_folded_gaps_never_read_as_an_absence() {
    let page = |verdict: Value| {
        let mut page = answer(vec![], (0..30).map(gap).collect(), verdict);
        page["_kin"]["page"] = json!({"version": 1, "kind": "references", "complete": true,
            "has_more": false, "answer_rows_complete": true, "answer_rows_has_more": false,
            "evidence_has_more": false, "total_references": 0, "returned_references": 0});
        page
    };
    let call = json!({"entity_id": FOCAL_ID, "max_output_bytes": 12000});
    // Inconclusive stays partial.
    let (result, _) = presented(&page(inconclusive()), &call, &agent());
    let plain = reply(&result);
    assert_eq!(plain["_kin"]["verdict"], PARTIAL, "{plain}");
    // Even an answer the graph reads as whole keeps its open places open: a
    // fold counts them and is never promoted to a plain complete.
    let whole = json!({"state": crate::verdict::CERTIFIED_EXCEPT,
        "absence_claim": "not_applicable", "safe_to_conclude_absent": false});
    let (result, _) = presented(&page(whole), &call, &agent());
    let plain = reply(&result);
    assert_ne!(plain["_kin"]["verdict"], COMPLETE, "{plain}");
    let meaning = plain["_kin"]["meaning"].as_str().unwrap();
    assert!(!meaning.contains("safe to conclude"), "{meaning}");
    let folded = &plain[LEFT_TO_CHECK_KEY][0];
    assert_eq!(folded["retained_entries"].as_u64().unwrap() > 0, true);
    // The count is said as a count, with the one way its places could
    // matter, and never as places to go and check.
    assert!(
        meaning.contains("more places are counted by reason in left_to_check"),
        "{meaning}"
    );
    assert!(!meaning.contains("nothing calls"), "{meaning}");
}
