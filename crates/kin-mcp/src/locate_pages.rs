// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Frozen, lossless locate pages using the semantic-record page transport.
//!
//! A machine locate answer carries its verdict, its negative reading and its
//! degradations beside its rows, and on a small repository those already
//! measure past 7,000 characters. A budget under that held no row, so the
//! answer was refused, and one a little over it was cut from the end of the
//! ranking with no cursor to the rows it lost. Now the answer is built whole,
//! and one that does not fit its caller's budget is frozen and paged like an
//! impact answer: every row and every safety reading reaches the caller
//! through `next_cursor`, and the ranking's own `next_cursor` comes back among
//! the readings for the ranking page after it.
//!
//! An answer that fits is held to its caller's budget exactly as before, so
//! nothing changes for the reply that was never too large.

use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Value};

use crate::budget::ResponseBudget;
use crate::{ContentBlock, ToolCallResult};

pub use crate::trace_pages::Context;

/// The tool these pages serve.
pub const TOOL: &str = "semantic_locate";

/// The arguments that name what a locate answer is about. A continuation
/// naming none of them is a bare handle on its frozen answer.
pub const SELECTORS: [&str; 2] = ["query", "queries"];

pub fn context(arguments: &HashMap<String, Value>, authority: &Value) -> Context {
    // Every answer-shaping argument binds a continuation that names its
    // question; only transport controls may change between its pages.
    let query: BTreeMap<_, _> = arguments
        .iter()
        .filter(|(key, _)| !crate::trace_pages::TRANSPORT_CONTROLS.contains(&key.as_str()))
        .collect();
    let named = SELECTORS
        .iter()
        .any(|key| crate::trace_pages::names(arguments, key));
    Context::new(&json!({"tool": TOOL, "arguments": query}), authority).continuing(
        crate::trace_pages::PageKind::Locate,
        arguments,
        named,
    )
}

/// Whether `token` continues a frozen answer, as opposed to a ranking cursor,
/// which pages the held ranking itself and is answered by the locate handler.
pub fn is_page_cursor(token: &str) -> bool {
    crate::trace_pages::validate_cursor_syntax(token).is_ok()
}

/// The page-transport cursor a locate call carries, if it carries one.
pub fn page_cursor(arguments: &HashMap<String, Value>) -> Option<&str> {
    arguments
        .get("cursor")
        .and_then(Value::as_str)
        .filter(|token| is_page_cursor(token))
}

pub fn resume(cursor: &str, context: &Context, max_bytes: usize) -> Result<Value, String> {
    crate::trace_pages::resume(cursor, context, max_bytes)
        .map_err(|error| error.replacen("trace", "locate", 1))
}

/// Hold a whole, enveloped machine locate answer to its caller's budget.
///
/// `whole` was built at the widest budget. When it fits `budget`, it is bound
/// to that budget the way any answer is, which cuts nothing and states the
/// caller's ceiling in its accounting. When it does not and the graph it was
/// read from is known (`context`), it is frozen and its first page served.
/// With no context it is cut the way it always was, so no continuation is
/// handed out that could later resume against a different graph.
pub fn finalize(
    whole: ToolCallResult,
    context: Option<Context>,
    budget: &ResponseBudget,
) -> ToolCallResult {
    if whole.is_error == Some(true) {
        return whole;
    }
    let Some(ContentBlock::Text { text }) = whole.content.first() else {
        return whole;
    };
    let Ok(mut payload) = serde_json::from_str::<Value>(text) else {
        return whole;
    };
    if !payload.is_object() || crate::trace_pages::is_page(&payload) {
        return whole;
    }
    let Some(context) = context.filter(|_| text.len() > budget.max_chars) else {
        crate::envelope::rebound(&mut payload, TOOL, budget);
        return match crate::budget::render(&payload) {
            Ok(rendered) => ToolCallResult::text(rendered),
            Err(error) => ToolCallResult::error(error.to_string()),
        };
    };
    // Built under the widest budget, so its accounting describes a reply no
    // caller receives. Every page states its own.
    if let Some(envelope) = payload.get_mut("_kin").and_then(Value::as_object_mut) {
        envelope.remove("response");
    }
    match crate::trace_pages::start_kind(
        payload,
        context,
        budget.max_chars,
        crate::trace_pages::PageKind::Locate,
    ) {
        Ok(page) => ToolCallResult::text(page.to_string()),
        Err(error) => ToolCallResult::error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authority() -> Value {
        json!({"graph_as_of": {"generation": 7}})
    }

    fn arguments(query: &str) -> HashMap<String, Value> {
        HashMap::from([
            ("query".to_string(), json!(query)),
            ("limit".to_string(), json!(60)),
        ])
    }

    /// A machine locate answer whose envelope alone is larger than the
    /// smallest budget a caller may name, as a real one is.
    fn whole_answer(rows: usize, ranking_cursor: Option<&str>) -> Value {
        let entities: Vec<Value> = (0..rows)
            .map(|index| {
                json!({
                    "entity_id": format!("00000000-0000-4000-8000-{index:012}"),
                    "kind": "function", "name": format!("hop_{index}"),
                    "module": "src.chain", "signature": format!("def hop_{index}(value)"),
                    "score": 1.0 / (index as f64 + 1.0),
                })
            })
            .collect();
        json!({
            "query": "hop", "granularity": "entity", "routing": "fused-v1",
            "page": 0, "total_ranked": rows, "next_cursor": ranking_cursor,
            "entities": entities,
            "negative": {
                "kind": "qualified_answer", "result_count": rows, "trust": "inconclusive",
                "safe_to_conclude_absent": false,
                "advice": "x".repeat(1500), "trust_reason": "y".repeat(800),
            },
            "degradations": [{"component": "retrieval_arm", "reason": "embeddings_not_used",
                              "severity": "note"}],
            "_kin": {
                "envelope_version": 2, "semantic_surface": 1,
                "graph_as_of": {"generation": 7},
                "verdict": {"state": "inconclusive", "safe_to_conclude_absent": false,
                            "absence_claim": "not_applicable",
                            "limiting_factor": "graph_role_filter_withheld"},
                "completeness": {"status": "unknown", "bound": "at_least"},
                "durability": {"state": "recorded", "detail": "z".repeat(600)},
                "response": {"max_chars": 60000, "chars_after_budget": 1, "bounded": false,
                             "compact": true},
            },
        })
    }

    fn whole(payload: &Value) -> ToolCallResult {
        ToolCallResult::text(payload.to_string())
    }

    fn text(result: &ToolCallResult) -> &str {
        let ContentBlock::Text { text } = result.content.first().expect("content");
        text
    }

    /// Follow `next_cursor` from `first` to the end, returning every page.
    fn walk(first: Value, context: &Context, max_bytes: usize) -> Vec<Value> {
        let mut pages = vec![first];
        while let Some(token) = pages.last().unwrap()["next_cursor"]
            .as_str()
            .filter(|token| is_page_cursor(token))
            .map(str::to_string)
        {
            pages.push(resume(&token, context, max_bytes).expect("page resumes"));
            assert!(pages.len() < 500, "the walk must end");
        }
        pages
    }

    #[test]
    fn an_answer_that_fits_is_bound_to_its_caller_and_never_paged() {
        let _cache = crate::trace_pages::cache_test_guard();
        let answer = whole_answer(3, Some("v2.ranking.1.3.3"));
        let budget = ResponseBudget {
            max_chars: 60_000,
            explicit_max_chars: true,
            ..ResponseBudget::default()
        };
        let context = context(&arguments("hop"), &authority());
        let served = finalize(whole(&answer), Some(context), &budget);
        let payload: Value = serde_json::from_str(text(&served)).unwrap();
        assert!(!crate::trace_pages::is_page(&payload), "{payload}");
        assert_eq!(payload["entities"].as_array().unwrap().len(), 3);
        assert_eq!(payload["next_cursor"], "v2.ranking.1.3.3");
        assert_eq!(payload["_kin"]["response"]["max_chars"], 60_000);
    }

    #[test]
    fn an_answer_too_large_for_its_budget_pages_every_row_within_it() {
        let _cache = crate::trace_pages::cache_test_guard();
        let answer = whole_answer(25, Some("v2.ranking.1.25.25"));
        let max = crate::budget::RESPONSE_MIN_MAX_CHARS;
        let budget = ResponseBudget {
            max_chars: max,
            explicit_max_chars: true,
            ..ResponseBudget::default()
        };
        let context = context(&arguments("hop"), &authority());
        let served = finalize(whole(&answer), Some(context.clone()), &budget);
        assert_ne!(served.is_error, Some(true), "{}", text(&served));
        let first: Value = serde_json::from_str(text(&served)).unwrap();
        assert!(crate::trace_pages::is_page(&first), "{first}");
        assert_eq!(first["_kin"]["page"]["kind"], "locate");
        assert_eq!(first["_kin"]["page"]["total_rows"], 25);
        // The pager's cursor, not the ranking's, until every page is read.
        assert!(is_page_cursor(first["next_cursor"].as_str().unwrap()));

        let pages = walk(first, &context, max);
        assert!(pages.len() > 1, "a 2,000-character budget must page");
        for page in &pages {
            assert!(page.to_string().len() <= max, "{page}");
            assert_eq!(
                page["_kin"]["completeness"]["limits"],
                json!(["locate_page_partial"])
            );
            assert_eq!(page["_kin"]["verdict"]["state"], "inconclusive");
        }
        let rows: Vec<&Value> = pages
            .iter()
            .flat_map(|page| page["entities"].as_array().into_iter().flatten())
            .collect();
        assert_eq!(rows.len(), 25, "every ranked row reaches the caller");
        assert_eq!(rows[0]["name"], "hop_0");
        assert_eq!(rows[24]["name"], "hop_24");
        // The ranking's cursor is a reading, so the reassembled answer can
        // ask for the ranking page after this one.
        let readings: Vec<&Value> = pages
            .iter()
            .flat_map(|page| page["readings"].as_array().into_iter().flatten())
            .collect();
        assert!(readings
            .iter()
            .any(|reading| reading["key"] == "next_cursor"
                && reading["value"] == "v2.ranking.1.25.25"));
        assert!(pages.last().unwrap()["next_cursor"].is_null());
    }

    #[test]
    fn a_continuation_is_refused_once_the_graph_moves() {
        let _cache = crate::trace_pages::cache_test_guard();
        let answer = whole_answer(25, None);
        let budget = ResponseBudget {
            max_chars: crate::budget::RESPONSE_MIN_MAX_CHARS,
            explicit_max_chars: true,
            ..ResponseBudget::default()
        };
        let served = finalize(
            whole(&answer),
            Some(context(&arguments("hop"), &authority())),
            &budget,
        );
        let first: Value = serde_json::from_str(text(&served)).unwrap();
        let token = first["next_cursor"].as_str().unwrap();
        let moved = context(
            &arguments("hop"),
            &json!({"graph_as_of": {"generation": 8}}),
        );
        let error = resume(token, &moved, budget.max_chars).unwrap_err();
        assert!(error.starts_with("locate"), "{error}");
        assert!(error.contains("restart without cursor"), "{error}");
    }

    #[test]
    fn with_no_graph_identity_an_oversized_answer_is_cut_and_never_paged() {
        let _cache = crate::trace_pages::cache_test_guard();
        let answer = whole_answer(25, None);
        let budget = ResponseBudget {
            max_chars: 6_000,
            explicit_max_chars: true,
            ..ResponseBudget::default()
        };
        let served = finalize(whole(&answer), None, &budget);
        let payload: Value = serde_json::from_str(text(&served)).unwrap();
        assert!(!crate::trace_pages::is_page(&payload));
        let kept = payload["entities"].as_array().unwrap().len();
        assert!(kept > 0 && kept < 25, "kept {kept}: {payload}");
        assert_eq!(payload["_kin"]["response"]["max_chars"], 6_000);
    }

    #[test]
    fn ranking_and_page_cursors_are_told_apart() {
        assert!(!is_page_cursor("v2.0123abcd.1.25.25"));
        assert!(!is_page_cursor("0123abcd.1"));
        let args = HashMap::from([("cursor".to_string(), json!("v2.0123abcd.1.25.25"))]);
        assert_eq!(page_cursor(&args), None);
    }
}
