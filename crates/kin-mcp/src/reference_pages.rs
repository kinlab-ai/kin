// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Frozen, lossless reference pages using the semantic-record page transport.

use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Value};

pub use crate::trace_pages::Context;

/// The arguments that name what a reference answer is about. A continuation
/// naming none of them is a bare handle on its frozen answer.
pub const SELECTORS: [&str; 4] = ["entity_id", "query", "view", "include_tests"];

pub fn context(arguments: &HashMap<String, Value>, authority: &Value) -> Context {
    // Excluding only transport controls binds every answer-shaping argument,
    // including filters, snippets and the answer-only projection. Unknown
    // arguments cannot silently gain semantics partway through a continuation
    // that names its target. One that names none resumes the frozen answer.
    let query: BTreeMap<_, _> = arguments
        .iter()
        .filter(|(key, _)| !crate::trace_pages::TRANSPORT_CONTROLS.contains(&key.as_str()))
        .collect();
    let named = SELECTORS
        .iter()
        .any(|key| crate::trace_pages::names(arguments, key));
    Context::new(
        &json!({"tool": "find_references", "arguments": query}),
        authority,
    )
    .continuing(crate::trace_pages::PageKind::References, arguments, named)
}

pub fn resume(cursor: &str, context: &Context, max_bytes: usize) -> Result<Value, String> {
    crate::trace_pages::resume(cursor, context, max_bytes)
        .map_err(|error| error.replacen("trace", "reference", 1))
}

pub fn finalize(
    result: crate::ToolCallResult,
    envelope: crate::Envelope,
    context: Context,
    budget: &crate::budget::ResponseBudget,
) -> crate::ToolCallResult {
    crate::trace_pages::finalize_kind(
        result,
        envelope,
        context,
        budget.max_chars,
        crate::trace_pages::PageKind::References,
        budget.answer_only,
    )
}

#[cfg(test)]
#[path = "bounded_refs_tests.rs"]
mod bounded_refs_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn semantic_repository() -> Value {
        // Pinned identity for /example/project under repository-context v1.
        json!({"authority_context": "90b5939b2c0fa106b07d37b21b3cd84e29b9aee26c93ec32e11f2b2587906163"})
    }

    fn assert_semantic_fields(value: &Value) {
        match value {
            Value::Object(record) => {
                for (key, value) in record {
                    assert!(
                        !matches!(
                            key.as_str(),
                            "file"
                                | "file_path"
                                | "files"
                                | "projection"
                                | "root"
                                | "client_root"
                                | "source_path"
                                | "source_span"
                                | "start_line"
                                | "end_line"
                                | "line"
                                | "start_byte"
                                | "end_byte"
                                | "source_derivation"
                                | "full_adapter_sources"
                                | "excluded_artifacts"
                        ),
                        "physical field {key} escaped: {record:?}"
                    );
                    assert_semantic_fields(value);
                }
            }
            Value::Array(values) => values.iter().for_each(assert_semantic_fields),
            _ => {}
        }
    }

    #[test]
    fn focused_override_pages_keep_scope_and_reject_view_changes() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let authority = json!({"repo": "example", "truth": 8});
        let asked = HashMap::from([
            (
                "entity_id".into(),
                json!("00000000-0000-4000-8000-000000000001"),
            ),
            ("view".into(), json!("overrides")),
        ]);
        let expected_rows = rows(80);
        let coverage = json!({
            "scope": "recorded_overrides_and_implements", "transitive": true,
            "recorded_closure": "complete", "repository_completeness": "unmeasured",
            "nested_definitions": "unmeasured", "unadmitted_candidates": 0,
            "unproven_relations": 0,
        });
        let original = json!({
            "view": "overrides",
            "focal_entity": {"entity_id": asked["entity_id"], "name": "Base.render"},
            "overridden_by": {"count": 80, "proven": 80, "overriders": expected_rows,
                "coverage": coverage, "nested_definitions": "unmeasured"},
            "_kin": {"verdict": {"state": "inconclusive"}},
        });
        let mut page = crate::trace_pages::start_kind(
            original,
            context(&asked, &authority),
            4000,
            crate::trace_pages::PageKind::References,
        )
        .unwrap();
        let first_cursor = page["next_cursor"].as_str().unwrap().to_owned();
        let mut seen = Vec::new();
        loop {
            assert!(serde_json::to_vec(&page).unwrap().len() <= 4000);
            assert_eq!(page["_kin"]["reference_view"]["view"], "overrides");
            assert_eq!(
                page["_kin"]["reference_view"]["overridden_by"]["coverage"],
                coverage
            );
            seen.extend(
                page.pointer("/overridden_by/overriders")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
            let Some(cursor) = page["next_cursor"].as_str() else {
                break;
            };
            let arguments = HashMap::from([("cursor".into(), json!(cursor))]);
            page = resume(cursor, &context(&arguments, &authority), 4000).unwrap();
        }
        assert_eq!(seen, expected_rows);
        for changed in [
            HashMap::from([
                ("cursor".into(), json!(first_cursor)),
                ("view".into(), json!("references")),
            ]),
            HashMap::from([
                ("cursor".into(), json!(first_cursor)),
                ("view".into(), json!("overrides")),
            ]),
        ] {
            assert!(resume(&first_cursor, &context(&changed, &authority), 4000)
                .unwrap_err()
                .contains("query changed"));
        }
    }

    fn query() -> HashMap<String, Value> {
        HashMap::from([("query".into(), json!("Adapter.send"))])
    }

    #[test]
    fn reference_pages_preserve_all_callers_and_reject_query_or_authority_changes() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let arguments = query();
        let authority = json!({"repo":"example", "truth":4, "source":"committed", "session":"a"});
        let current = context(&arguments, &authority);
        let rows: Vec<_> = (0..80).map(|index| json!({
            "entity_id": format!("00000000-0000-4000-8000-{index:012}"),
            "name":format!("caller_{index}"), "sites":[{"callee":"adapter.send", "line_in_entity":index}],
            "resolution":"type_resolved", "signature":"x".repeat(150),
        })).collect();
        let mut internal_rows = rows.clone();
        for row in &mut internal_rows {
            row["file"] = json!("src/adapter.py");
            row["projection"] = json!({"path": "src/adapter.py"});
            row["start_line"] = json!(44);
            row["sites"][0]["expr"] = json!("adapter.send('assets/input.json')");
        }
        let mut expected_rows = rows;
        for row in &mut expected_rows {
            row["sites"][0]["expr"] = json!("adapter.send('assets/input.json')");
        }
        let original = json!({
            "references": internal_rows, "total_upstream": 80,
            "call_sites":{"candidate_count":3,"candidates":[{"caller":"a"},{"caller":"b"},{"caller":"c"}],"clauses":["unresolved binding"]},
            "negative":{"safe_to_conclude_absent":false},
            "target_ambiguity":{"candidates":[]},
            "_kin":{"verdict":{"state":"inconclusive"},
                "repository":{"root":"/example/project"},
                "graph_as_of":{"generation":4,"graph_root":"captured-root"}},
        });
        let mut expected = original.clone();
        expected["references"] = json!(expected_rows);
        expected["_kin"]["repository"] = semantic_repository();
        expected["_kin"]["semantic_surface"] = json!(1);
        let mut page = crate::trace_pages::start_kind(
            original.clone(),
            current.clone(),
            4000,
            crate::trace_pages::PageKind::References,
        )
        .unwrap();
        let first_cursor = page["next_cursor"].as_str().unwrap().to_string();
        let mut seen = Vec::new();
        let mut candidates = Vec::new();
        let mut readings = serde_json::Map::new();
        let mut pages = 0;
        loop {
            pages += 1;
            assert!(pages < 100);
            let bytes = serde_json::to_vec(&page).unwrap().len();
            assert!(bytes <= 4000);
            assert_eq!(page["_kin"]["response"]["chars_after_budget"], bytes);
            assert_semantic_fields(&page);
            assert_eq!(page["_kin"]["semantic_surface"], 1);
            assert_eq!(page["_kin"]["verdict"]["state"], "inconclusive");
            assert_eq!(page["negative"]["safe_to_conclude_absent"], false);
            for key in ["repository", "graph_as_of"] {
                assert_eq!(page["_kin"][key], expected["_kin"][key]);
            }
            seen.extend(page["references"].as_array().unwrap().iter().cloned());
            candidates.extend(
                page.pointer("/call_sites/candidates")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
            for reading in page["readings"].as_array().into_iter().flatten() {
                readings.insert(
                    reading["key"].as_str().unwrap().into(),
                    reading["value"].clone(),
                );
            }
            let Some(cursor) = page["next_cursor"].as_str() else {
                break;
            };
            page = resume(cursor, &current, 4000).unwrap();
        }
        assert!(pages > 1, "the fixture must page");
        assert_eq!(seen, expected_rows);
        assert_eq!(
            candidates,
            original["call_sites"]["candidates"]
                .as_array()
                .unwrap()
                .clone()
        );
        let mut reconstructed = Value::Object(readings);
        reconstructed["references"] = json!(seen);
        reconstructed["call_sites"]["candidates"] = json!(candidates);
        assert_eq!(reconstructed, expected);
        assert_semantic_fields(&reconstructed);
        for (key, value) in [
            ("query", json!("Other.send")),
            ("relation_kinds", json!(["calls"])),
            ("include_snippets", json!(true)),
            ("min_resolution", json!("name_only")),
            ("answer_only", json!(true)),
            ("view", json!("overrides")),
        ] {
            let mut changed = arguments.clone();
            changed.insert(key.into(), value);
            assert!(resume(&first_cursor, &context(&changed, &authority), 4000)
                .unwrap_err()
                .contains("query changed"));
        }
        assert!(resume(
            &first_cursor,
            &context(&arguments, &json!({"repo":"other"})),
            4000
        )
        .unwrap_err()
        .contains("authority scope changed"));
        let mut resized = arguments;
        resized.insert("max_chars".into(), json!(6000));
        resized.insert("cursor".into(), json!(&first_cursor));
        assert!(resume(&first_cursor, &context(&resized, &authority), 6000).is_ok());
    }

    fn rows(count: usize) -> Vec<Value> {
        (0..count)
            .map(|index| {
                json!({
                    "entity_id": format!("00000000-0000-4000-8000-{index:012}"),
                    "name": format!("caller_{index}"),
                    "sites": [{"callee": "adapter.send", "line_in_entity": index}],
                    "resolution": "type_resolved", "signature": "x".repeat(150),
                })
            })
            .collect()
    }

    fn freeze(arguments: &HashMap<String, Value>, authority: &Value) -> (Value, String) {
        let original = json!({
            "references": rows(80), "total_upstream": 80,
            "negative": {"safe_to_conclude_absent": false},
            "_kin": {"verdict": {"state": "inconclusive"},
                "repository": {"root": "/example/project"},
                "graph_as_of": {"generation": 4, "graph_root": "captured-root"}},
        });
        let page = crate::trace_pages::start_kind(
            original,
            context(arguments, authority),
            4000,
            crate::trace_pages::PageKind::References,
        )
        .unwrap();
        let cursor = page["next_cursor"].as_str().unwrap().to_string();
        (page, cursor)
    }

    /// The cursor alone, with the answer-only default an agent profile adds
    /// and a budget in any spelling, pages every row of the frozen answer.
    /// The filters it was frozen under still hold, because the snapshot froze
    /// them.
    #[test]
    fn a_bare_reference_cursor_pages_every_row_under_its_frozen_filters() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let authority = json!({"repo": "example", "truth": 5});
        let asked = HashMap::from([
            ("query".into(), json!("Adapter.send")),
            ("relation_kinds".into(), json!(["calls"])),
            ("include_snippets".into(), json!(true)),
        ]);
        let (first, cursor) = freeze(&asked, &authority);
        for bare in [
            json!({"cursor": cursor}),
            json!({"cursor": cursor, "answer_only": true, "max_output_bytes": 4000,
                "max_chars": 4000, "entity_id": null, "query": ""}),
        ] {
            let bare: HashMap<String, Value> = serde_json::from_value(bare).unwrap();
            let current = context(&bare, &authority);
            let mut page = first.clone();
            let mut seen = Vec::new();
            let mut pages = 0;
            loop {
                pages += 1;
                assert!(pages < 100);
                assert!(serde_json::to_vec(&page).unwrap().len() <= 4000);
                seen.extend(page["references"].as_array().unwrap().iter().cloned());
                let Some(next) = page["next_cursor"].as_str() else {
                    break;
                };
                page = resume(next, &current, 4000).unwrap();
            }
            assert!(pages > 1, "the fixture must page");
            assert_eq!(seen, rows(80));
        }
    }

    #[test]
    fn a_reference_cursor_naming_another_target_is_told_to_send_it_alone() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let authority = json!({"repo": "example", "truth": 6});
        let (_, cursor) = freeze(&query(), &authority);
        for named in [
            json!({"cursor": cursor, "query": "Other.send"}),
            json!({"cursor": cursor, "entity_id": "00000000-0000-4000-8000-000000000001"}),
            json!({"cursor": cursor, "query": "Adapter.send", "include_snippets": true}),
        ] {
            let named: HashMap<String, Value> = serde_json::from_value(named).unwrap();
            let error = resume(&cursor, &context(&named, &authority), 4000).unwrap_err();
            assert!(
                error.starts_with("reference cursor query changed"),
                "{error}"
            );
            assert!(error.contains("send the cursor alone"), "{error}");
            assert!(error.contains("restart without cursor"), "{error}");
        }
    }

    #[test]
    fn a_bare_reference_cursor_is_still_refused_when_authority_moved() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let authority = json!({"repo": "example", "truth": 7});
        let (_, cursor) = freeze(&query(), &authority);
        let bare = HashMap::from([("cursor".into(), json!(cursor))]);
        let error = resume(
            &cursor,
            &context(&bare, &json!({"repo": "example", "truth": 8})),
            4000,
        )
        .unwrap_err();
        assert!(
            error.starts_with("reference cursor graph or authority scope changed"),
            "{error}"
        );
        // Another tool's bare cursor names the tool that can continue it.
        let error = crate::impact_pages::resume(
            &cursor,
            &crate::impact_pages::context(&bare, &authority),
            4000,
        )
        .unwrap_err();
        assert!(
            error.starts_with("impact cursor continues an answer from find_references"),
            "{error}"
        );
        assert!(resume(&cursor, &context(&bare, &authority), 4000).is_ok());
    }

    fn unresolved_answer(known: usize) -> Value {
        json!({
            "references": (0..known).map(|index| json!({
                "entity_id": format!("caller-{index}"), "name": format!("caller_{index}"),
                "resolution": "type_resolved",
            })).collect::<Vec<_>>(),
            "total_upstream": known,
            "call_site_candidates": {"named": (0..120).map(|index| json!({
                "caller": format!("unresolved-{index}"), "caller_name": format!("possible_{index}"),
                "line_in_entity": null, "callee": null, "state": "could_not_match",
            })).collect::<Vec<_>>()},
            "negative": {"safe_to_conclude_absent": false},
            "_kin": {"verdict": {"state": "inconclusive", "safe_to_conclude_absent": false,
                "limiting_factor": "call_extraction_incomplete: unresolved sites may add callers"}},
        })
    }

    #[test]
    fn completed_recorded_rows_leave_uncertain_evidence_on_a_separate_cursor() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let current = context(&query(), &json!({"truth": 9}));
        for known in [0, 7] {
            let original = unresolved_answer(known);
            let first = crate::trace_pages::start_kind(
                original.clone(),
                current.clone(),
                4000,
                crate::trace_pages::PageKind::References,
            )
            .unwrap();
            assert_eq!(first["references"], original["references"]);
            assert_eq!(first["_kin"]["page"]["answer_rows_complete"], true);
            assert_eq!(first["_kin"]["page"]["answer_rows_has_more"], false);
            assert_eq!(first["_kin"]["page"]["evidence_has_more"], true);
            assert!(first["answer_next_cursor"].is_null());
            assert_eq!(first["evidence_next_cursor"], first["next_cursor"]);
            assert_eq!(first["_kin"]["verdict"]["state"], "inconclusive");
            assert_eq!(first["negative"]["safe_to_conclude_absent"], false);

            let mut page = first;
            let mut candidates = Vec::new();
            loop {
                assert!(serde_json::to_vec(&page).unwrap().len() <= 4000);
                candidates.extend(
                    page.pointer("/call_site_candidates/named")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .cloned(),
                );
                let Some(cursor) = page["evidence_next_cursor"].as_str() else {
                    break;
                };
                page = resume(cursor, &current, 4000).unwrap();
                assert!(page["references"].as_array().unwrap().is_empty());
                assert_eq!(page["_kin"]["verdict"]["safe_to_conclude_absent"], false);
            }
            assert_eq!(
                candidates,
                original["call_site_candidates"]["named"]
                    .as_array()
                    .unwrap()
                    .clone()
            );
        }
    }

    #[test]
    fn answer_cursor_continues_real_rows_and_evidence_cursor_can_skip_them() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let current = context(&query(), &json!({"truth": 10}));
        let original = unresolved_answer(80);
        let first = crate::trace_pages::start_kind(
            original.clone(),
            current.clone(),
            4000,
            crate::trace_pages::PageKind::References,
        )
        .unwrap();
        assert_eq!(first["_kin"]["page"]["answer_rows_complete"], false);
        assert_eq!(first["_kin"]["page"]["answer_rows_has_more"], true);
        let evidence = resume(
            first["evidence_next_cursor"].as_str().unwrap(),
            &current,
            4000,
        )
        .unwrap();
        assert!(evidence["references"].as_array().unwrap().is_empty());
        assert!(!evidence["call_site_candidates"]["named"]
            .as_array()
            .unwrap()
            .is_empty());
        let mut page = first;
        let mut seen = Vec::new();
        loop {
            seen.extend(page["references"].as_array().unwrap().iter().cloned());
            let Some(cursor) = page["answer_next_cursor"].as_str() else {
                break;
            };
            page = resume(cursor, &current, 4000).unwrap();
        }
        assert_eq!(seen, original["references"].as_array().unwrap().clone());
        assert_eq!(page["_kin"]["page"]["answer_rows_complete"], true);
        assert_eq!(page["_kin"]["page"]["evidence_has_more"], true);
    }

    #[test]
    fn owner_and_override_sections_precede_unresolved_evidence() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let current = context(&query(), &json!({"truth": 11}));
        let mut original = unresolved_answer(0);
        original["candidates_by_owner"] = json!((0..10).map(|index| json!({
            "entity_id": format!("owner-{index}"), "owner_qualified_name": format!("Owner{index}.send"),
            "references": [{"entity_id": format!("caller-{index}"), "name": format!("run_{index}")}],
            "signature": "x".repeat(200),
        })).collect::<Vec<_>>());
        original["overridden_by"]["overriders"] =
            json!((0..10).map(|index| json!({
            "entity_id": format!("override-{index}"), "name": format!("Adapter{index}.send"),
        })).collect::<Vec<_>>());
        let mut page = crate::trace_pages::start_kind(
            original.clone(),
            current.clone(),
            4000,
            crate::trace_pages::PageKind::References,
        )
        .unwrap();
        assert_eq!(page["_kin"]["page"]["answer_rows_has_more"], true);
        let mut owners = Vec::new();
        let mut overrides = Vec::new();
        loop {
            owners.extend(
                page["candidates_by_owner"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
            overrides.extend(
                page.pointer("/overridden_by/overriders")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
            let Some(cursor) = page["answer_next_cursor"].as_str() else {
                break;
            };
            page = resume(cursor, &current, 4000).unwrap();
        }
        assert_eq!(
            owners,
            original["candidates_by_owner"].as_array().unwrap().clone()
        );
        assert_eq!(
            overrides,
            original["overridden_by"]["overriders"]
                .as_array()
                .unwrap()
                .clone()
        );
        assert_eq!(page["_kin"]["page"]["answer_rows_complete"], true);
        assert!(page["evidence_next_cursor"].is_string());
    }

    #[test]
    fn a_fragmented_answer_row_remains_pending_until_its_final_field() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let current = context(&query(), &json!({"truth": 12}));
        let mut original = unresolved_answer(1);
        let signature = format!("fn send(path: &str = {:?})", "assets/δ.json".repeat(500));
        original["references"][0]["signature"] = json!(signature);
        original["references"][0]["file"] = json!("src/private.py");
        original["references"][0]["projection"] = json!({"path": "src/private.py"});
        let expected = json!({
            "entity_id": "caller-0", "name": "caller_0", "resolution": "type_resolved",
            "signature": signature,
        });
        let mut page = crate::trace_pages::start_kind(
            original,
            current.clone(),
            2000,
            crate::trace_pages::PageKind::References,
        )
        .unwrap();
        assert!(page["record_fragment"].is_object());
        let mut fragments = 0;
        let mut pending: BTreeMap<String, String> = BTreeMap::new();
        let mut reconstructed = serde_json::Map::new();
        loop {
            let bytes = serde_json::to_vec(&page).unwrap().len();
            assert!(bytes <= 2000);
            assert_eq!(page["_kin"]["response"]["chars_after_budget"], bytes);
            assert_semantic_fields(&page);
            assert_eq!(page["_kin"]["verdict"]["state"], "inconclusive");
            assert_eq!(page["negative"]["safe_to_conclude_absent"], false);
            if let Some(fragment) = page.get("record_fragment") {
                fragments += 1;
                assert!(fragments < 40);
                assert_eq!(fragment["collection"], "references");
                assert_eq!(fragment["index"], 0);
                let field = fragment["field"].as_str().unwrap();
                assert!(
                    expected.get(field).is_some(),
                    "unexpected fragmented field {field}"
                );
                let text = pending.entry(field.to_string()).or_default();
                assert_eq!(fragment["byte_offset"], text.len());
                text.push_str(fragment["text"].as_str().unwrap());
                assert!(!text.contains("src/private.py"));
                if fragment["field_complete"] == true {
                    let text = pending.remove(field).unwrap();
                    let value = if fragment["encoding"] == "utf8" {
                        json!(text)
                    } else {
                        serde_json::from_str(&text).unwrap()
                    };
                    assert!(reconstructed.insert(field.to_string(), value).is_none());
                }
            }
            let Some(cursor) = page["answer_next_cursor"].as_str() else {
                break;
            };
            assert_eq!(page["_kin"]["page"]["answer_rows_complete"], false);
            page = resume(cursor, &current, 2000).unwrap();
        }
        assert!(fragments > 1);
        assert!(pending.is_empty());
        assert_eq!(Value::Object(reconstructed), expected);
        assert_eq!(page["_kin"]["page"]["answer_rows_complete"], true);
        assert!(page["evidence_next_cursor"].is_string());
    }
}
