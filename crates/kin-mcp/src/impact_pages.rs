// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Frozen, lossless impact pages using the semantic-record page transport.
//!
//! An impact answer used to be cut from the end of its row lists when it did
//! not fit, with a remediation to narrow `depth` and no way to reach the rows
//! it dropped. Paged like a reference answer, every bucket row stays reachable
//! through `next_cursor`, and every safety reading the whole answer carried
//! travels with the pages.

use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Value};

pub use crate::trace_pages::Context;

/// The tool these pages serve.
pub const TOOL: &str = "impact_analysis";

/// The arguments that name what an impact answer is about. A continuation
/// naming none of them is a bare handle on its frozen answer.
pub const SELECTORS: [&str; 5] = ["entity_ids", "change_ids", "base", "head", "files"];

pub fn context(arguments: &HashMap<String, Value>, authority: &Value) -> Context {
    // Every answer-shaping argument binds a continuation that names its
    // targets; only transport controls may change between its pages. One that
    // names no target resumes the frozen answer as it is.
    let query: BTreeMap<_, _> = arguments
        .iter()
        .filter(|(key, _)| !crate::trace_pages::TRANSPORT_CONTROLS.contains(&key.as_str()))
        .collect();
    let named = SELECTORS
        .iter()
        .any(|key| crate::trace_pages::names(arguments, key));
    Context::new(&json!({"tool": TOOL, "arguments": query}), authority).continuing(
        crate::trace_pages::PageKind::Impact,
        arguments,
        named,
    )
}

pub fn resume(cursor: &str, context: &Context, max_bytes: usize) -> Result<Value, String> {
    crate::trace_pages::resume(cursor, context, max_bytes)
        .map_err(|error| error.replacen("trace", "impact", 1))
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
        crate::trace_pages::PageKind::Impact,
        false,
    )
}

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

    fn arguments() -> HashMap<String, Value> {
        HashMap::from([(
            "entity_ids".into(),
            json!(["00000000-0000-4000-8000-000000000001"]),
        )])
    }

    fn semantic_row(bucket: &str, index: usize) -> Value {
        json!({
            "id": format!("00000000-0000-4000-8000-{index:012}"),
            "name": format!("{bucket}_{index}"), "kind": "method",
            "signature": "x".repeat(120),
            "body": "return open('src/settings.json')",
        })
    }

    fn row(bucket: &str, index: usize) -> Value {
        let mut row = semantic_row(bucket, index);
        row["projection"] = json!({"path": format!("src/{bucket}.py")});
        row
    }

    #[test]
    fn impact_pages_reach_every_bucket_row_and_reject_changed_queries() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let arguments = arguments();
        let authority = json!({"repo":"example", "truth":4});
        let current = context(&arguments, &authority);
        let callers: Vec<_> = (0..30).map(|index| row("caller", index)).collect();
        let overrides: Vec<_> = (0..12).map(|index| row("override", index)).collect();
        let dependents: Vec<_> = (0..40).map(|index| row("dependent", index)).collect();
        let original = json!({
            "affected_callers": callers,
            "affected_overrides": overrides,
            "affected_dependents": dependents,
            "affected_tests": [],
            "entity_impacts": [{"entity_id": "00000000-0000-4000-8000-000000000001",
                "consumer_count": 30}],
            "changed_ids": ["00000000-0000-4000-8000-000000000001"],
            "negative": {"safe_to_conclude_absent": false},
            "_kin": {"verdict": {"state": "inconclusive"},
                "repository": {"root": "/example/project"},
                "graph_as_of": {"generation": 4, "graph_root": "captured-root"}},
        });
        let mut expected = original.clone();
        for (bucket, name, count) in [
            ("affected_callers", "caller", 30),
            ("affected_overrides", "override", 12),
            ("affected_dependents", "dependent", 40),
        ] {
            expected[bucket] = json!((0..count)
                .map(|index| semantic_row(name, index))
                .collect::<Vec<_>>());
        }
        expected["_kin"]["repository"] = semantic_repository();
        expected["_kin"]["semantic_surface"] = json!(1);
        let mut page = crate::trace_pages::start_kind(
            original.clone(),
            current.clone(),
            4000,
            crate::trace_pages::PageKind::Impact,
        )
        .unwrap();
        assert_eq!(page["_kin"]["page"]["kind"], "impact");
        assert_eq!(page["_kin"]["page"]["total_rows"], 83);
        let first_cursor = page["next_cursor"].as_str().unwrap().to_string();
        let mut buckets: BTreeMap<String, Vec<Value>> = BTreeMap::new();
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
            assert_eq!(page["negative"]["subject"], "impact_page");
            // The page carries the answer's verdict. This fixture's verdict
            // names no limiting factor, so the page does not invent one.
            // Partialness is the page's own fact.
            assert!(page["_kin"]["verdict"]["limiting_factor"].is_null());
            assert_eq!(
                page["_kin"]["completeness"]["limits"],
                json!(["impact_page_partial"])
            );
            for key in ["repository", "graph_as_of"] {
                assert_eq!(page["_kin"][key], expected["_kin"][key]);
            }
            for bucket in crate::trace_pages::PageKind::Impact.collections() {
                buckets
                    .entry((*bucket).to_string())
                    .or_default()
                    .extend(page[*bucket].as_array().into_iter().flatten().cloned());
            }
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
        assert!(pages > 1);
        let mut reconstructed = Value::Object(readings);
        for (bucket, rows) in buckets {
            if !rows.is_empty() {
                reconstructed[bucket] = json!(rows);
            }
        }
        assert_eq!(reconstructed, expected);
        assert_semantic_fields(&reconstructed);
        for (key, value) in [
            (
                "entity_ids",
                json!(["00000000-0000-4000-8000-000000000002"]),
            ),
            ("depth", json!(1)),
            ("compact", json!(false)),
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
        assert!(resume(&first_cursor, &context(&resized, &authority), 6000).is_ok());
    }

    /// What a plain reply writes its answer line from is on the first page,
    /// ahead of every row: how many rows each list holds in all, what was
    /// left out as test code, and whether the dependents were asked for.
    /// Paged after the rows, they left a reply that held every caller and
    /// override unable to say so, and it ended on a cursor.
    #[test]
    fn impact_pages_lead_with_what_the_answer_line_reads() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let arguments = arguments();
        let authority = json!({"repo":"example", "truth":11});
        let current = context(&arguments, &authority);
        let leading = [
            crate::handlers::review::IMPACT_ROW_TOTALS_KEY,
            crate::handlers::review::IMPACT_TESTS_EXCLUDED_KEY,
            crate::handlers::review::IMPACT_DEPENDENTS_ASKED_KEY,
        ];
        let original = json!({
            "affected_callers": (0..3).map(|index| row("caller", index)).collect::<Vec<_>>(),
            "affected_dependents": (0..60).map(|index| row("dependent", index)).collect::<Vec<_>>(),
            "entity_impacts": [{"entity_id": "00000000-0000-4000-8000-000000000001",
                "consumer_count": 3}],
            leading[0]: {"affected_callers": 3, "affected_dependents": 60},
            leading[1]: {"entities": 4},
            leading[2]: true,
            "negative": {"safe_to_conclude_absent": false},
            "_kin": {"verdict": {"state": "certified"},
                "repository": {"root": "/example/project"},
                "graph_as_of": {"generation": 4, "graph_root": "captured-root"}},
        });
        let page = crate::trace_pages::start_kind(
            original,
            current,
            3000,
            crate::trace_pages::PageKind::Impact,
        )
        .unwrap();
        assert!(page["next_cursor"].is_string(), "{page}");
        let keys: Vec<&str> = page["readings"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|reading| reading["key"].as_str())
            .collect();
        assert_eq!(&keys[..3], leading, "{page}");
        assert_eq!(page["affected_callers"].as_array().unwrap().len(), 3);
    }

    /// The direct answer leads: its callers, overrides and same-name rows,
    /// then the sections that grade it, then the dependents. Every page says
    /// its verdict is the direct section's, carries both sections' states,
    /// and retains semantic binding scope without exposing source coordinates or counts.
    #[test]
    fn impact_pages_lead_with_the_direct_answer_and_carry_both_section_verdicts() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let arguments = arguments();
        let authority = json!({"repo":"example", "truth":9});
        let current = context(&arguments, &authority);
        let callers: Vec<_> = (0..8).map(|index| row("caller", index)).collect();
        let overrides: Vec<_> = (0..3).map(|index| row("override", index)).collect();
        let same_name: Vec<_> = (0..2).map(|index| row("same_name", index)).collect();
        let dependents: Vec<_> = (0..40).map(|index| row("dependent", index)).collect();
        let states = json!({
            "direct": {"state": "certified_except", "limiting_factor": "named_leftovers"},
            "dependents": {"state": "inconclusive", "limiting_factor": "call_sites_unresolved"},
        });
        let source_derivation = json!({
            "checked_scope": "admitted_inventory",
            "report": {"full_adapter_sources": 83, "excluded_artifacts": 153,
                "issues": [], "issue_count": 0, "parse_coverage": "complete"},
            "scope": "live_head",
        });
        let original = json!({
            "affected_callers": callers,
            "affected_overrides": overrides,
            crate::impact_sections::SAME_NAME_KEY: same_name,
            crate::impact_sections::SAME_NAME_NOTE_KEY: crate::impact_sections::SAME_NAME_NOTE,
            "affected_dependents": dependents,
            "entity_impacts": [{"entity_id": "00000000-0000-4000-8000-000000000001",
                "consumer_count": 8}],
            "sections": {
                "direct": {"rows": ["affected_callers", "affected_overrides",
                    crate::impact_sections::SAME_NAME_KEY],
                    "verdict": {"state": "certified_except"}},
                "dependents": {"rows": ["affected_dependents"],
                    "verdict": {"state": "inconclusive"}},
            },
            "negative": {"safe_to_conclude_absent": false},
            "_kin": {"verdict": {"state": "certified_except",
                    "limiting_factor": "named_leftovers", "scope": "direct",
                    "sections": states},
                "source_derivation": source_derivation,
                "repository": {"root": "/example/project"},
                "graph_as_of": {"generation": 4, "graph_root": "captured-root"}},
        });
        let mut expected = original.clone();
        for (bucket, name, count) in [
            ("affected_callers", "caller", 8),
            ("affected_overrides", "override", 3),
            (crate::impact_sections::SAME_NAME_KEY, "same_name", 2),
            ("affected_dependents", "dependent", 40),
        ] {
            expected[bucket] = json!((0..count)
                .map(|index| semantic_row(name, index))
                .collect::<Vec<_>>());
        }
        expected["_kin"]
            .as_object_mut()
            .unwrap()
            .remove("source_derivation");
        let binding_coverage = json!({"checked_scope": "admitted_inventory", "scope": "live_head"});
        expected["_kin"]["binding_coverage"] = binding_coverage.clone();
        expected["_kin"]["repository"] = semantic_repository();
        expected["_kin"]["semantic_surface"] = json!(1);
        let mut page = crate::trace_pages::start_kind(
            original.clone(),
            current.clone(),
            3000,
            crate::trace_pages::PageKind::Impact,
        )
        .unwrap();
        // The page each part of the answer was last (direct rows) or first
        // (sections, dependents) delivered on.
        let mut direct_last = 0;
        let mut direct_rows = 0;
        let mut sections_first = None;
        let mut dependents_first = None;
        let mut buckets: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        let mut readings = serde_json::Map::new();
        let mut pages = 0;
        loop {
            pages += 1;
            assert!(pages < 100);
            let bytes = serde_json::to_vec(&page).unwrap().len();
            assert!(bytes <= 3000);
            assert_eq!(page["_kin"]["response"]["chars_after_budget"], bytes);
            assert_semantic_fields(&page);
            assert_eq!(page["_kin"]["semantic_surface"], 1);
            assert_eq!(page["_kin"]["repository"], semantic_repository());
            assert_eq!(page["_kin"]["graph_as_of"], original["_kin"]["graph_as_of"]);
            assert_eq!(page["_kin"]["verdict"]["state"], "certified_except");
            assert_eq!(page["negative"]["safe_to_conclude_absent"], false);
            assert_eq!(page["_kin"]["verdict"]["scope"], "direct", "{page}");
            assert_eq!(page["_kin"]["verdict"]["sections"], states, "{page}");
            assert_eq!(page["_kin"]["binding_coverage"], binding_coverage);
            for bucket in crate::trace_pages::PageKind::Impact.collections() {
                buckets
                    .entry((*bucket).to_string())
                    .or_default()
                    .extend(page[*bucket].as_array().into_iter().flatten().cloned());
            }
            for reading in page["readings"].as_array().into_iter().flatten() {
                assert!(
                    readings
                        .insert(
                            reading["key"].as_str().unwrap().into(),
                            reading["value"].clone(),
                        )
                        .is_none(),
                    "reading was repeated"
                );
            }
            for bucket in [
                "affected_callers",
                "affected_overrides",
                crate::impact_sections::SAME_NAME_KEY,
            ] {
                let rows = page[bucket].as_array().map_or(0, Vec::len);
                if rows > 0 {
                    direct_rows += rows;
                    direct_last = pages;
                }
            }
            if page["readings"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|reading| reading["key"] == "sections")
            {
                sections_first.get_or_insert(pages);
            }
            if page["affected_dependents"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty())
            {
                dependents_first.get_or_insert(pages);
            }
            let Some(cursor) = page["next_cursor"].as_str() else {
                break;
            };
            page = resume(cursor, &current, 3000).unwrap();
        }
        assert!(pages > 1, "the fixture must page");
        assert_eq!(direct_rows, 13);
        let mut reconstructed = Value::Object(readings);
        for (bucket, rows) in buckets {
            if !rows.is_empty() {
                reconstructed[bucket] = json!(rows);
            }
        }
        assert_eq!(reconstructed, expected);
        assert_semantic_fields(&reconstructed);
        let sections_first = sections_first.expect("the sections are delivered");
        let dependents_first = dependents_first.expect("the dependents are delivered");
        assert!(
            direct_last <= sections_first && sections_first <= dependents_first,
            "direct rows end on page {direct_last}, sections start on {sections_first}, \
             dependents on {dependents_first}"
        );
        assert!(
            dependents_first > 1,
            "the dependents follow the direct answer"
        );
    }

    fn freeze(authority: &Value) -> (Value, Vec<Value>, String) {
        let callers: Vec<_> = (0..60).map(|index| row("caller", index)).collect();
        let original = json!({
            "affected_callers": callers,
            "negative": {"safe_to_conclude_absent": false},
            "_kin": {"verdict": {"state": "inconclusive"},
                "repository": {"root": "/example/project"},
                "graph_as_of": {"generation": 4, "graph_root": "captured-root"}},
        });
        let mut asked = arguments();
        asked.insert("depth".into(), json!(2));
        asked.insert("include_traffic".into(), json!(false));
        let page = crate::trace_pages::start_kind(
            original,
            context(&asked, authority),
            4000,
            crate::trace_pages::PageKind::Impact,
        )
        .unwrap();
        let cursor = page["next_cursor"].as_str().unwrap().to_string();
        let semantic_callers = (0..60).map(|index| semantic_row("caller", index)).collect();
        (page, semantic_callers, cursor)
    }

    #[test]
    fn a_bare_impact_cursor_pages_every_row_under_its_frozen_targets() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let authority = json!({"repo": "example", "truth": 5});
        let (first, callers, cursor) = freeze(&authority);
        for bare in [
            json!({"cursor": cursor}),
            json!({"cursor": cursor, "max_output_bytes": 4000, "compact": false,
                "depth": 1, "entity_ids": [], "files": null}),
        ] {
            let bare: HashMap<String, Value> = serde_json::from_value(bare).unwrap();
            let current = context(&bare, &authority);
            let mut page = first.clone();
            let mut seen = Vec::new();
            let mut pages = 0;
            loop {
                pages += 1;
                assert!(pages < 100);
                let bytes = serde_json::to_vec(&page).unwrap().len();
                assert!(bytes <= 4000);
                assert_eq!(page["_kin"]["response"]["chars_after_budget"], bytes);
                assert_semantic_fields(&page);
                assert_eq!(page["_kin"]["repository"], semantic_repository());
                assert_eq!(page["_kin"]["verdict"]["state"], "inconclusive");
                assert_eq!(page["negative"]["safe_to_conclude_absent"], false);
                seen.extend(
                    page["affected_callers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .cloned(),
                );
                let Some(next) = page["next_cursor"].as_str() else {
                    break;
                };
                page = resume(next, &current, 4000).unwrap();
            }
            assert!(pages > 1, "the fixture must page");
            assert_eq!(seen, callers);
        }
    }

    #[test]
    fn an_impact_cursor_naming_other_targets_is_told_to_send_it_alone() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let authority = json!({"repo": "example", "truth": 6});
        let (_, _, cursor) = freeze(&authority);
        for named in [
            json!({"cursor": cursor, "entity_ids": ["00000000-0000-4000-8000-000000000002"]}),
            json!({"cursor": cursor, "change_ids": ["ab"]}),
            json!({"cursor": cursor, "base": "aa", "head": "bb"}),
            json!({"cursor": cursor, "files": ["src/caller.py"]}),
        ] {
            let named: HashMap<String, Value> = serde_json::from_value(named).unwrap();
            let error = resume(&cursor, &context(&named, &authority), 4000).unwrap_err();
            assert!(error.starts_with("impact cursor query changed"), "{error}");
            assert!(error.contains("send the cursor alone"), "{error}");
        }
    }

    #[test]
    fn a_bare_impact_cursor_is_still_refused_when_authority_moved() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let authority = json!({"repo": "example", "truth": 7});
        let (_, _, cursor) = freeze(&authority);
        let bare = HashMap::from([("cursor".into(), json!(cursor))]);
        let error = resume(
            &cursor,
            &context(&bare, &json!({"repo": "example", "truth": 8})),
            4000,
        )
        .unwrap_err();
        assert!(
            error.starts_with("impact cursor graph or authority scope changed"),
            "{error}"
        );
        let error = crate::reference_pages::resume(
            &cursor,
            &crate::reference_pages::context(&bare, &authority),
            4000,
        )
        .unwrap_err();
        assert!(
            error.starts_with("reference cursor continues an answer from impact_analysis"),
            "{error}"
        );
        assert!(resume(&cursor, &context(&bare, &authority), 4000).is_ok());
    }
}
