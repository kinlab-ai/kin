// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Small query answers, with the existing plain evidence available on demand.
//! This changes presentation only: row membership, trust readings and frozen
//! semantic records are computed before it. A detail choice belongs to this
//! connection, and a continuation can inherit or explicitly change it. The
//! choice decides what a row carries. What an answered reply ends on is the
//! same either way.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

// Keep only the most recently served opaque handles, with the frozen page
// cache's snapshot count and lifetime. Replay of an evicted choice defaults
// to minimal; an explicit details flag is always accepted on a valid cursor.
const MAX_CURSOR_CHOICES: usize = 16;
const CURSOR_CHOICE_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, Default)]
pub struct QueryPresentation {
    choices: Mutex<VecDeque<CursorChoice>>,
}

#[derive(Debug)]
struct CursorChoice {
    tool: String,
    cursor: String,
    details: bool,
    at: Instant,
}

pub fn supported(tool: &str) -> bool {
    matches!(
        tool,
        "find_references"
            | "semantic_locate"
            | "semantic_search"
            | "get_context_pack"
            | "trace_data_flow"
    )
}

impl QueryPresentation {
    pub fn details(&self, tool: &str, arguments: &HashMap<String, Value>) -> bool {
        if let Some(details) = arguments.get("details").and_then(Value::as_bool) {
            return details;
        }
        let Some(cursor) = arguments.get("cursor").and_then(Value::as_str) else {
            return false;
        };
        let mut choices = self
            .choices
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        choices.retain(|choice| choice.at.elapsed() < CURSOR_CHOICE_TTL);
        choices
            .iter()
            .rev()
            .find(|choice| choice.tool == tool && choice.cursor == cursor)
            .is_some_and(|choice| choice.details)
    }

    fn remember(&self, tool: &str, payload: &Value, details: bool) {
        let mut choices = self
            .choices
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        choices.retain(|choice| choice.at.elapsed() < CURSOR_CHOICE_TTL);
        // Same-name replies contain known answer sections. Inspect only that
        // shape, with a bounded walk; keys in code, readings or fragments are
        // data and never become handles. Prefer retaining the outer cursors.
        let mut sections = Vec::new();
        let mut pending = vec![payload];
        while sections.len() < MAX_CURSOR_CHOICES {
            let Some(section) = pending.pop() else {
                break;
            };
            sections.push(section);
            if let Some(answers) = section.get("answers").and_then(Value::as_array) {
                let room = MAX_CURSOR_CHOICES - sections.len() - pending.len();
                pending.extend(answers.iter().take(room).rev());
            }
        }
        for section in sections.into_iter().rev() {
            for key in ["next_cursor", "answer_next_cursor", "evidence_next_cursor"] {
                let Some(cursor) = section.get(key).and_then(Value::as_str) else {
                    continue;
                };
                choices.retain(|choice| choice.tool != tool || choice.cursor != cursor);
                choices.push_back(CursorChoice {
                    tool: tool.into(),
                    cursor: cursor.into(),
                    details,
                    at: Instant::now(),
                });
            }
        }
        while choices.len() > MAX_CURSOR_CHOICES {
            choices.pop_front();
        }
    }

    pub fn present(&self, result: &mut Value, tool: &str, arguments: &HashMap<String, Value>) {
        self.present_with_remember(result, tool, arguments, true);
    }

    /// Render with the chosen details policy but leave outgoing cursor choices
    /// uncommitted until the final byte guard accepts the delivered result.
    pub(crate) fn present_deferred(
        &self,
        result: &mut Value,
        tool: &str,
        arguments: &HashMap<String, Value>,
    ) -> bool {
        self.present_with_remember(result, tool, arguments, false)
    }

    fn present_with_remember(
        &self,
        result: &mut Value,
        tool: &str,
        arguments: &HashMap<String, Value>,
        remember: bool,
    ) -> bool {
        let details = self.details(tool, arguments);
        crate::plain_reply::present_query(result, tool, details);
        if !supported(tool) || result.get("isError").and_then(Value::as_bool) == Some(true) {
            return details;
        }
        let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) else {
            return details;
        };
        for block in content {
            let Some(text) = block.get("text").and_then(Value::as_str) else {
                continue;
            };
            let Ok(mut payload) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            if remember {
                self.remember(tool, &payload, details);
            }
            // Details change what a row carries, never what an answer ends
            // on: an answered reply is quieted the same with or without them.
            if !details {
                compact_answer(&mut payload, tool);
            }
            let ended = end_on_the_answer(&mut payload, tool);
            if !details || ended {
                if let Ok(text) = serde_json::to_string(&payload) {
                    block["text"] = Value::String(text);
                }
            }
        }
        details
    }

    /// Remember only handles actually delivered after final presentation and
    /// sizing. Minimal compaction retains the cursor fields and answer sections
    /// that `remember` visits; code and record fragments are never traversed.
    pub(crate) fn remember_delivered(&self, result: &Value, tool: &str, details: bool) {
        if !supported(tool) || result.get("isError").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let Some(content) = result.get("content").and_then(Value::as_array) else {
            return;
        };
        for block in content {
            let Some(text) = block.get("text").and_then(Value::as_str) else {
                continue;
            };
            if let Ok(payload) = serde_json::from_str::<Value>(text) {
                self.remember(tool, &payload, details);
            }
        }
    }
}

/// End an answered reply on its answer and its open places, whatever detail
/// its rows carry, and say whether anything went.
///
/// Measured in a model run: an agent passed `details: true` on most calls,
/// since the schema said it kept the same rows, and each such reply brought
/// back every trailer the short reply had learned to leave out. One callers
/// reach whose nine members were on the page trailed a count of 102,806 calls
/// through values, and the agent spent five more turns on it before giving
/// the same nine names. So `details` decides only how much a row says: its
/// signature, its ids, its proof and every site with its line. The verdict
/// decides the trailers, by the rules of
/// [`crate::reply_budget::quiet_answered_reach`] and
/// [`crate::reply_budget::quiet_answered_references`]. A partial answer keeps
/// every gap and cursor, and a frozen fragment is never touched.
///
/// The machine reply, with every count, cursor and referrer, is what the
/// `full` profile serves, and an agent profile started with
/// [`crate::plain_reply::MACHINE_REPLIES_ENV`] set. No argument of a call
/// brings the trailers back, so a model cannot reach for them by reflex.
fn end_on_the_answer(payload: &mut Value, tool: &str) -> bool {
    if payload.get("record_fragment").is_some() {
        return false;
    }
    let Some(reply) = payload.as_object_mut() else {
        return false;
    };
    match tool {
        "trace_data_flow" => {
            let hidden = crate::reply_budget::hide_answered_chain(reply);
            let quiet = crate::reply_budget::quiet_answered_reach(reply);
            hidden || quiet
        }
        "find_references" => crate::reply_budget::quiet_answered_references(reply),
        _ => false,
    }
}

fn compact_row(row: &mut Value) {
    let Some(row) = row.as_object_mut() else {
        return;
    };
    // Retain UUID and exact stored names so same-spelled callers remain distinct.
    for key in [
        "signature",
        "line",
        "start_line",
        "end_line",
        "sites",
        "lines",
        "score",
        "match_evidence",
        "doc_summary",
        "description",
        "preview",
        "snippet",
        "body",
    ] {
        row.remove(key);
    }
    if row.get("shown").and_then(Value::as_str) == Some("signature only") {
        row.remove("shown");
    }
}

/// The candidate sites are observations left to resolve, never answer rows.
/// Their counts survive this projection; their complete text remains on the
/// same frozen page when the client explicitly asks for details.
/// Most different calls a short reference row shows, and the most
/// characters of each; a longer call keeps its start and is marked cut.
const CALL_CODE_PER_ROW: usize = 8;
const CALL_CODE_CHARS: usize = 160;

/// Where a short reference row lists the calls it makes after its first, in
/// site order.
pub(crate) const MORE_CALL_CODE_KEY: &str = "more_call_code";

/// The code of a reference row's sites, the evidence a reader would otherwise
/// fetch the caller's whole body to see: `ctx.pop()`. Every different call
/// the caller makes, in site order, up to [`CALL_CODE_PER_ROW`]; a call
/// written the same way at several sites is one line that says how many.
/// Also how many sites those lines stand for. Lines and positions stay on
/// the details page.
///
/// A row once showed its first call and a count of the others. Measured in
/// a model run, the count sent the reader to four whole caller bodies to see
/// whether the second call was a different one.
fn reference_call_code(row: &Value) -> (Vec<Value>, usize) {
    let mut calls: Vec<(&str, usize)> = Vec::new();
    for code in row
        .get("sites")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|site| site.get("code").and_then(Value::as_str))
        .filter(|code| !code.trim().is_empty())
    {
        match calls.iter_mut().find(|(seen, _)| *seen == code) {
            Some((_, sites)) => *sites += 1,
            None => calls.push((code, 1)),
        }
    }
    calls.truncate(CALL_CODE_PER_ROW);
    let shown = calls.iter().map(|(_, sites)| sites).sum();
    let lines = calls
        .into_iter()
        .map(|(code, sites)| {
            let mut line: String = if code.chars().count() <= CALL_CODE_CHARS {
                code.to_string()
            } else {
                let cut: String = code.chars().take(CALL_CODE_CHARS).collect();
                format!("{cut}…")
            };
            if sites > 1 {
                line.push_str(&format!(" (x{sites})"));
            }
            Value::from(line)
        })
        .collect();
    (lines, shown)
}

/// Where a short reference reply spells out the proof codes its rows use.
pub(crate) const PROOF_CODES_KEY: &str = "proof_codes";

/// The short code for each proof a reference row states, and its wording.
const PROOF_CODES: [(&str, &str); 3] = [
    ("type", "target resolved by type"),
    ("imports", "target resolved through imports"),
    ("name", "matched by name only"),
];

/// A reference row as a short reply serves it: what an agent needs to name
/// the caller and trust the row, and nothing it would only re-read.
///
/// The row keeps its entity_id, its name or qualified name, its module, a
/// proof code and a line for each call it makes. The owners its qualified name
/// already spells, the relation ids behind them, a kind that is a function
/// or method, a role that is the ordinary source role, and relation kinds
/// that are the ones asked for are left to `details:true`, which serves the
/// row whole. Measured on Flask, 89 caller rows averaged about 500 bytes in
/// this form before; most of it was the owner record restating the name.
fn compact_reference_row(
    row: &mut Value,
    (call_code, shown): (Vec<Value>, usize),
    sites: usize,
    asked: Option<&Value>,
    proofs: &mut std::collections::BTreeSet<(&'static str, &'static str)>,
) {
    let Some(fields) = row.as_object_mut() else {
        return;
    };
    for key in [
        "semantic_owners",
        "semantic_owner_gap",
        "semantic_owners_omitted",
        "semantic_owner_candidate_count",
        "relation_id",
        "receiver_binding_available",
    ] {
        fields.remove(key);
    }
    let ordinary_kind = fields
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|kind| {
            kind.eq_ignore_ascii_case("function") || kind.eq_ignore_ascii_case("method")
        });
    if ordinary_kind {
        fields.remove("kind");
    }
    if fields.get("role").and_then(Value::as_str) == Some("source") {
        fields.remove("role");
    }
    if let (Some(how), Some(asked)) = (fields.get("how"), asked) {
        if same_kinds(how, asked) {
            fields.remove("how");
        }
    }
    // A qualified name that ends in the stored name carries it, owner and all.
    let carried = match (
        fields.get("qualified_name").and_then(Value::as_str),
        fields.get("name").and_then(Value::as_str),
    ) {
        (Some(qualified), Some(name)) => {
            qualified == name || qualified.ends_with(&format!(".{name}"))
        }
        _ => false,
    };
    if carried {
        fields.remove("name");
    }
    if let Some(wording) = fields.get("proof").and_then(Value::as_str) {
        if let Some((code, wording)) = PROOF_CODES.iter().find(|(_, known)| *known == wording) {
            proofs.insert((code, wording));
            fields.insert("proof".into(), Value::from(*code));
        }
    }
    // A line for each call the row makes, and how many more sites it holds
    // that no line stands for: one past the lines a row shows, or one whose
    // code was not read. Details list every site with its line.
    let mut calls = call_code.into_iter();
    if let Some(first) = calls.next() {
        fields.insert("call_code".into(), first);
        let more: Vec<Value> = calls.collect();
        if !more.is_empty() {
            fields.insert(MORE_CALL_CODE_KEY.into(), Value::Array(more));
        }
        if sites > shown {
            fields.insert("more_calls".into(), Value::from(sites - shown));
        }
    }
}

/// Whether two relation-kind lists name the same kinds, in any order.
fn same_kinds(left: &Value, right: &Value) -> bool {
    let set = |value: &Value| -> Option<std::collections::BTreeSet<String>> {
        value.as_array().map(|kinds| {
            kinds
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_ascii_lowercase)
                .collect()
        })
    };
    matches!((set(left), set(right)), (Some(left), Some(right)) if left == right)
}

/// At most this many retained sites are shown inline, each as its caller and
/// its line of code, instead of a count behind a details request.
const INLINE_SITE_LIMIT: u64 = 12;

fn compact_reference_gaps(payload: &mut Value) {
    const DETAIL: &str = "For these retained evidence entries (not proven callers), repeat this request or its incoming cursor with details:true. Returned cursors advance later evidence.";
    if payload.get("evidence_detail").is_some() {
        return;
    }
    let Some(original) = payload
        .get("left_to_check")
        .filter(|value| value.is_array())
    else {
        return;
    };
    let mut compacted = original.clone();
    let groups = compacted.as_array_mut().expect("copied groups");
    let mut shortened = false;
    for group in groups {
        let Some(fields) = group.as_object_mut() else {
            continue;
        };
        if !fields.get("why").is_some_and(Value::is_string)
            || fields.contains_key("retained_entries")
        {
            continue;
        }
        let Some(sites) = fields.get("sites").and_then(Value::as_array) else {
            continue;
        };
        if sites.is_empty() {
            continue;
        }
        // The plain renderer can collapse identical uninspectable records.
        // Count their retained occurrences rather than the projected objects.
        let Some(retained) = sites.iter().try_fold(0u64, |total, site| {
            let occurrences = match site.get("occurrences") {
                Some(value) => value.as_u64().filter(|count| *count > 0)?,
                None => 1,
            };
            total.checked_add(occurrences)
        }) else {
            continue;
        };
        // A handful of sites is cheaper to show than to send a reader after:
        // each keeps its caller and its one line of code, which is usually
        // enough to rule it out (`values.pop("k")` on a dict) without a read.
        if retained <= INLINE_SITE_LIMIT {
            let inline: Vec<Value> = sites
                .iter()
                .map(|site| {
                    let mut kept = serde_json::Map::new();
                    for key in [
                        "caller",
                        crate::answer_account::CALLER_MODULE_KEY,
                        "code",
                        "occurrences",
                        crate::plain_reply::RECEIVER_IMPORTED_FROM_KEY,
                    ] {
                        if let Some(value) = site.get(key).filter(|value| !value.is_null()) {
                            kept.insert(key.into(), value.clone());
                        }
                    }
                    // A site no caller holds, such as a module-level
                    // assignment, is named by where it sits; one with no
                    // line to quote, by what it is and the name it spells.
                    // It is never an empty entry.
                    if !kept.contains_key("caller") {
                        for key in ["inside", "in_module"] {
                            if let Some(value) = site.get(key).filter(|value| !value.is_null()) {
                                kept.insert(key.into(), value.clone());
                            }
                        }
                    }
                    if !kept.contains_key("code") {
                        for key in ["kind", "name"] {
                            if let Some(value) = site.get(key).filter(|value| !value.is_null()) {
                                kept.insert(key.into(), value.clone());
                            }
                        }
                    }
                    Value::Object(kept)
                })
                .collect();
            fields.insert("sites".into(), Value::Array(inline));
            shortened = true;
            continue;
        }
        fields.remove("sites");
        if let Some(files) = fields.get_mut("by_file").and_then(Value::as_array_mut) {
            for file in files {
                if let Some(file) = file.as_object_mut() {
                    file.remove("lines");
                }
            }
        }
        fields.insert("retained_entries".into(), Value::from(retained));
        shortened = true;
    }
    // Projection follows the frozen machine-page budget. Include the hint
    // and JSON field overhead so shortening evidence cannot enlarge it.
    if shortened
        && compacted.to_string().len() + DETAIL.len() + "evidence_detail".len() + 6
            < original.to_string().len()
    {
        payload["left_to_check"] = compacted;
        payload["evidence_detail"] = Value::String(DETAIL.into());
    }
}

/// Context carries code, so it must never use the row projection that removes
/// bodies. Omit only recorded source positions and an exact duplicate of a
/// retained signature. Rows without complete navigable identity stay intact.
fn compact_context_row(row: &mut Value) {
    let Some(fields) = row.as_object_mut() else {
        return;
    };
    if !["entity_id", "name"].iter().all(|key| {
        fields
            .get(*key)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    }) {
        return;
    }
    for key in ["line", "start_line", "end_line"] {
        if fields
            .get(key)
            .is_some_and(|value| value.is_null() || value.as_u64().is_some())
        {
            fields.remove(key);
        }
    }
    let duplicate = match (
        fields.get("body").and_then(Value::as_str),
        fields.get("signature").and_then(Value::as_str),
    ) {
        (Some(body), Some(signature)) => !signature.is_empty() && body.contains(signature),
        _ => false,
    };
    if duplicate {
        fields.remove("signature");
    }
    // The owner record a qualified name already spells, as on a reference row.
    if fields
        .get("qualified_name")
        .and_then(Value::as_str)
        .is_some_and(|name| !name.is_empty())
    {
        for key in [
            "semantic_owners",
            "semantic_owner_gap",
            "semantic_owners_omitted",
            "semantic_owner_candidate_count",
        ] {
            fields.remove(key);
        }
    }
}

/// Where a short context reply says what its bodiless rows show.
pub(crate) const CONTEXT_ROWS_KEY: &str = "rows";

const CONTEXT_ROWS_NOTE: &str = "A row without a body shows its signature, or only its \
     identity when it has none; source on its entity_id serves the body.";

/// Whether a row's `shown` label states only what its own fields already do:
/// a row with no body shows its signature only when it has one, and its
/// identity only when it has none.
fn shown_is_implied(row: &Value) -> bool {
    let Some(fields) = row.as_object() else {
        return false;
    };
    let bodiless = fields.get("body").is_none_or(Value::is_null);
    let signed = fields
        .get("signature")
        .and_then(Value::as_str)
        .is_some_and(|signature| !signature.is_empty());
    match fields.get("shown").and_then(Value::as_str) {
        Some("signature only") => bodiless && signed,
        Some("identity only") => bodiless && !signed,
        _ => false,
    }
}

fn compact_context(payload: &mut Value) {
    if let Some(entity) = payload.get_mut("entity") {
        compact_context_row(entity);
    }
    const SECTIONS: [&str; 5] = [
        "entities",
        "dependencies",
        "dependents",
        "tests",
        "transitive_deps",
    ];
    let mut implied = 0usize;
    for key in SECTIONS {
        if let Some(rows) = payload.get_mut(key).and_then(Value::as_array_mut) {
            for row in rows {
                compact_context_row(row);
                implied += usize::from(shown_is_implied(row));
            }
        }
    }
    // Said once instead of on every row, only when that is shorter, and the
    // note always comes with the labels it replaces.
    if implied * "\"shown\":\"identity only\",".len() <= CONTEXT_ROWS_NOTE.len() {
        return;
    }
    for key in SECTIONS {
        if let Some(rows) = payload.get_mut(key).and_then(Value::as_array_mut) {
            for row in rows.iter_mut().filter(|row| shown_is_implied(row)) {
                if let Some(fields) = row.as_object_mut() {
                    fields.remove("shown");
                }
            }
        }
    }
    if let Some(envelope) = payload.get_mut("_kin").and_then(Value::as_object_mut) {
        envelope.insert(CONTEXT_ROWS_KEY.into(), Value::from(CONTEXT_ROWS_NOTE));
    }
}

fn compact_answer(payload: &mut Value, tool: &str) {
    // A fragment is a lossless semantic record, not a row to shorten. Readings
    // are likewise retained unchanged, including on detached terminal pages.
    if payload.get("record_fragment").is_some() {
        return;
    }
    if tool == "get_context_pack" {
        compact_context(payload);
        return;
    }
    if tool == "trace_data_flow" {
        crate::reply_budget::compact_trace_payload(payload);
        return;
    }
    if let Some(entity) = payload.get_mut("entity") {
        compact_row(entity);
    }
    let asked = payload.get("relation_kinds").cloned();
    let mut proofs = std::collections::BTreeSet::new();
    for key in [
        "results",
        "references",
        "name_only",
        crate::plain_reply::MODULE_CODE_USERS_KEY,
        crate::plain_reply::MODULE_LEVEL_USERS_KEY,
        "other_matches",
    ] {
        if let Some(rows) = payload.get_mut(key).and_then(Value::as_array_mut) {
            for row in rows {
                let reference = key != "results" && key != "other_matches";
                let call_code = if reference {
                    reference_call_code(row)
                } else {
                    (Vec::new(), 0)
                };
                let sites = row
                    .get("sites")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                compact_row(row);
                if reference {
                    compact_reference_row(row, call_code, sites, asked.as_ref(), &mut proofs);
                }
            }
        }
    }
    if !proofs.is_empty() {
        let legend: Vec<String> = proofs
            .iter()
            .map(|(code, wording)| format!("{code}: {wording}"))
            .collect();
        if let Some(envelope) = payload.get_mut("_kin").and_then(Value::as_object_mut) {
            envelope.insert(PROOF_CODES_KEY.into(), Value::from(legend.join("; ")));
        }
    }
    if let Some(methods) = payload
        .pointer_mut("/overridden_by/methods")
        .and_then(Value::as_array_mut)
    {
        for row in methods {
            compact_row(row);
        }
    }
    if tool == "find_references" {
        compact_reference_gaps(payload);
    }
    if let Some(answers) = payload.get_mut("answers").and_then(Value::as_array_mut) {
        for answer in answers {
            compact_answer(answer, tool);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn result(payload: Value) -> Value {
        json!({"content":[{"type":"text","text":payload.to_string()}]})
    }
    fn payload(result: &Value) -> Value {
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    /// A short reference row agrees with its details row: the same entity,
    /// and every identity field it leaves out is the ordinary value, the one
    /// its qualified name already spells, or the relation that was asked.
    fn assert_short_row_agrees(short: &Value, rich: &Value, asked: &Value) {
        assert_eq!(short["entity_id"], rich["entity_id"]);
        assert_eq!(short["qualified_name"], rich["qualified_name"]);
        assert_eq!(short.get("file"), None);
        match short.get("name") {
            Some(name) => assert_eq!(name, &rich["name"]),
            None => {
                let qualified = rich["qualified_name"].as_str().unwrap();
                let name = rich["name"].as_str().unwrap();
                assert!(qualified == name || qualified.ends_with(&format!(".{name}")));
            }
        }
        match short.get("kind") {
            Some(kind) => assert_eq!(kind, &rich["kind"]),
            None => assert!(matches!(
                rich["kind"]
                    .as_str()
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some("function" | "method") | None
            )),
        }
        match short.get("how") {
            Some(how) => assert_eq!(how, &rich["how"]),
            None => assert!(rich.get("how").is_none() || same_kinds(&rich["how"], asked)),
        }
    }

    /// A short group shows a handful of sites inline as caller and code, and
    /// counts a larger set behind a details request.
    fn assert_inline_or_counted(short: &Value, rich: &Value) {
        let rich_sites = rich["sites"].as_array().unwrap();
        if rich_sites.len() as u64 <= INLINE_SITE_LIMIT {
            let inline = short["sites"].as_array().expect("sites shown inline");
            assert_eq!(inline.len(), rich_sites.len());
            for (shown, full) in inline.iter().zip(rich_sites) {
                assert_eq!(shown.get("code"), full.get("code").filter(|v| !v.is_null()));
                assert!(shown.get("caller_id").is_none() && shown.get("line_in_caller").is_none());
            }
            assert!(short.get("retained_entries").is_none());
        } else {
            assert_eq!(short["retained_entries"], rich_sites.len());
            assert!(short.get("sites").is_none());
        }
    }

    /// A short site no caller holds keeps where it sits, and one with no
    /// line to quote keeps what it is and the name it spells: a module-level
    /// re-export was served as an empty entry, which a text reply wrote as a
    /// blank line under its group.
    #[test]
    fn a_short_site_without_a_caller_or_code_is_never_an_empty_entry() {
        let mut payload = json!({
            "left_to_check": [{"why": "Places that assign something named finale to another name.",
                "sites": [
                    {"in_module": "react-server.ReactFizzConfig.custom",
                     "in_module_id": "7d97c8e1-0000-4000-8000-000000000001",
                     "kind": "variable", "name": "pushSegmentFinale"},
                    {"inside": "createConfig", "inside_id": "7d97c8e1-0000-4000-8000-000000000002",
                     "line_in_entity": 3, "code": "cfg.finale = pushSegmentFinale"},
                    {"caller": "flush", "code": "pushSegmentFinale(chunks)", "line_in_caller": 9,
                     "caller_id": "7d97c8e1-0000-4000-8000-000000000003",
                     "in_module": "react-server.ReactFizzServer",
                     "in_module_id": "7d97c8e1-0000-4000-8000-000000000004"}]}],
        });
        compact_reference_gaps(&mut payload);
        assert_eq!(
            payload["left_to_check"][0]["sites"],
            json!([
                {"in_module": "react-server.ReactFizzConfig.custom", "kind": "variable",
                 "name": "pushSegmentFinale"},
                {"inside": "createConfig", "code": "cfg.finale = pushSegmentFinale"},
                {"caller": "flush", "code": "pushSegmentFinale(chunks)"},
            ])
        );
    }

    #[test]
    fn context_keeps_all_code_identity_links_and_gap_authority_while_details_recover_metadata() {
        let machine: Value =
            serde_json::from_str(include_str!("testdata/context_qualified.json")).unwrap();
        let mut old_plain = result(machine.clone());
        crate::plain_reply::present(&mut old_plain, "get_context_pack");
        let mut rich = result(machine.clone());
        QueryPresentation::default().present(
            &mut rich,
            "get_context_pack",
            &HashMap::from([("details".into(), json!(true))]),
        );
        assert_eq!(
            rich, old_plain,
            "details preserves the prior context rendering"
        );
        let mut short = result(machine);
        QueryPresentation::default().present(&mut short, "get_context_pack", &HashMap::new());
        assert!(
            short["content"][0]["text"].as_str().unwrap().len()
                < rich["content"][0]["text"].as_str().unwrap().len()
        );
        let short = payload(&short);
        let rich = payload(&rich);
        let mut short_kin = short["_kin"].clone();
        short_kin.as_object_mut().unwrap().remove(CONTEXT_ROWS_KEY);
        assert_eq!(short_kin, rich["_kin"], "_kin adds only the rows note");
        for key in [
            "withheld",
            "routes",
            "route_search",
            "links",
            "asked_about",
            "readings",
        ] {
            assert_eq!(short[key], rich[key], "{key}");
        }
        assert_eq!(short["entity"]["body"], rich["entity"]["body"]);
        assert!(short["entity"]["body"]
            .as_str()
            .is_some_and(|body| !body.is_empty()));
        assert!(short["entity"].get("line").is_none());
        assert!(rich["entity"].get("line").is_none());
        assert!(rich["entity"].get("file").is_none());
        for key in [
            "entity_id",
            "name",
            "qualified_name",
            "file",
            "kind",
            "body_complete",
            "source_base",
            "source_state",
            "span_coherence",
        ] {
            assert_eq!(short["entity"][key], rich["entity"][key], "{key}");
        }
        for key in [
            "entities",
            "dependencies",
            "dependents",
            "tests",
            "transitive_deps",
        ] {
            let short_rows = short.get(key).and_then(Value::as_array);
            let rich_rows = rich.get(key).and_then(Value::as_array);
            assert_eq!(short_rows.map(Vec::len), rich_rows.map(Vec::len), "{key}");
            for (short, rich) in short_rows
                .into_iter()
                .flatten()
                .zip(rich_rows.into_iter().flatten())
            {
                for field in [
                    "entity_id",
                    "name",
                    "qualified_name",
                    "file",
                    "kind",
                    "section",
                    "body",
                    "body_complete",
                    "body_unavailable",
                    "members",
                    "source_base",
                ] {
                    assert_eq!(short[field], rich[field], "{key}.{field}");
                }
                if rich.get("body").is_none_or(Value::is_null) {
                    assert_eq!(
                        short["signature"], rich["signature"],
                        "bodyless signature stays"
                    );
                }
            }
        }
    }

    /// A short context reply says once what its bodiless rows show, instead
    /// of labelling each one, and keeps every label it cannot imply.
    #[test]
    fn short_context_rows_state_what_they_show_once() {
        let row = |index: usize, signature: Option<&str>, shown: &str| {
            let mut row = json!({"entity_id": format!("dep-{index}"), "name": format!("dep{index}"),
                "qualified_name": format!("pkg.dep{index}"), "kind": "constant", "shown": shown,
                "semantic_owners": [{"entity_id":"owner","name":"pkg"}], "semantic_owner_gap": null});
            if let Some(signature) = signature {
                row["signature"] = json!(signature);
            }
            row
        };
        let mut rows: Vec<Value> = (0..12)
            .map(|index| row(index, Some("dep: boolean = true"), "signature only"))
            .collect();
        rows.push(row(12, None, "identity only"));
        rows.push(row(13, None, "signature only"));
        let mut payload = json!({"_kin": {"verdict": "partial", "meaning": "Partial: context."},
            "dependencies": rows});
        compact_context(&mut payload);
        let rows = payload["dependencies"].as_array().unwrap();
        assert!(
            rows[..13].iter().all(|row| row.get("shown").is_none()),
            "{payload}"
        );
        // A label the fields do not imply stays on its row.
        assert_eq!(rows[13]["shown"], "signature only");
        assert!(rows.iter().all(|row| row.get("semantic_owners").is_none()));
        assert_eq!(payload["_kin"][CONTEXT_ROWS_KEY], CONTEXT_ROWS_NOTE);
        // One or two labels are cheaper than the note, so they stay.
        let mut few = json!({"_kin": {"verdict": "partial"},
            "dependencies": [row(0, Some("dep: boolean = true"), "signature only")]});
        compact_context(&mut few);
        assert_eq!(few["dependencies"][0]["shown"], "signature only");
        assert!(few["_kin"].get(CONTEXT_ROWS_KEY).is_none());
    }

    #[test]
    fn context_signature_deduplication_is_exact_and_incomplete_identity_stays_intact() {
        let row = json!({"entity_id":"held", "name":"Owner.method", "qualified_name":"pkg.Owner.method",
            "file":"src/owner.py", "kind":"method", "line":7, "end_line":9,
            "signature":"def method(value)", "body":"def method(value):\n    return value",
            "body_complete":false,"members":[{"name":"nested", "line":8,"signature":"def nested()"}]});
        let mut duplicate = row.clone();
        compact_context_row(&mut duplicate);
        assert!(duplicate.get("signature").is_none());
        assert_eq!(duplicate["body"], row["body"]);
        assert_eq!(duplicate["body_complete"], false);
        assert_eq!(duplicate["members"], row["members"]);
        assert_eq!(duplicate["qualified_name"], row["qualified_name"]);
        for body in [
            Value::Null,
            json!("def method( value ):\n    return value"),
            json!(""),
            json!({"line":"literal code"}),
        ] {
            let mut nonduplicate = row.clone();
            nonduplicate["body"] = body.clone();
            compact_context_row(&mut nonduplicate);
            assert_eq!(nonduplicate["signature"], row["signature"]);
            assert_eq!(nonduplicate["body"], body);
        }
        for field in ["entity_id", "name"] {
            let mut unidentified = row.clone();
            unidentified.as_object_mut().unwrap().remove(field);
            let expected = unidentified.clone();
            compact_context_row(&mut unidentified);
            assert_eq!(unidentified, expected, "{field}");
        }
        let mut unusual_position = row.clone();
        unusual_position["line"] = json!("unknown position contract");
        compact_context_row(&mut unusual_position);
        assert_eq!(unusual_position["line"], "unknown position contract");
    }

    #[test]
    fn context_fragments_errors_and_unaddressed_outlines_remain_lossless() {
        let mut context = json!({"entity":{"entity_id":"held","name":"named","file":"src/named.py",
            "body":"def named():\n    return 1", "signature":"def named()", "line":4},
            "record_fragment":{"path":"entity.body", "utf8":"exact text", "byte_offset":0},
            "readings":[{"path":"source_derivation", "value":{"line":9}}]});
        let expected = context.clone();
        compact_answer(&mut context, "get_context_pack");
        assert_eq!(context, expected);
        let mut error =
            json!({"isError":true,"content":[{"type":"text","text":"Context refused."}]});
        let expected = error.clone();
        QueryPresentation::default().present(&mut error, "get_context_pack", &HashMap::new());
        assert_eq!(error, expected);
        let mut outlines = json!({"entities":[{"name":"only_name", "line":2,"signature":"def only_name()",
            "members":[{"entity_id":"nested", "file":"src/nested.py", "name":"nested", "line":3}]}],
            "routes":[{"path":"a -> b", "via":["a","b"],"withheld":2}],
            "withheld":{"rows":{"dependencies":["missing-body"]},"rows_total":17}});
        let expected = outlines.clone();
        compact_answer(&mut outlines, "get_context_pack");
        assert_eq!(outlines, expected);
    }

    fn refs() -> Value {
        json!({"_kin":{"verdict":{"state":"certified","safe_to_conclude_absent":false}},
            "focal_entity":{"id":"target","name":"normalize","qualified_name":"rates.normalize","kind":"Function","file":"rates.py","signature":"def normalize(value)"},
            "total_upstream":2,"relation_kinds":["calls"],"references":[
                {"entity_id":"one","name":"audit_manifest","qualified_name":"publisher.audit_manifest","kind":"Function","file":"publisher.py","line":3,
                    "relation_kinds":["calls"],"resolution":"type_resolved","sites":[{"expr":"normalize(value)","line_in_entity":1}]},
                {"entity_id":"two","name":"audit_manifest","kind":"Function","file":"other.py","line":7,
                    "relation_kinds":["calls"],"resolution":"type_resolved","sites":[{"expr":"normalize(value)","line_in_entity":2}]}]})
    }

    #[test]
    fn default_keeps_exact_callers_and_relationships_without_inline_evidence() {
        let choice = QueryPresentation::default();
        let mut short = result(refs());
        choice.present(&mut short, "find_references", &HashMap::new());
        let short = payload(&short);
        assert_eq!(short["references"].as_array().unwrap().len(), 2);
        assert_eq!(short["references"][0]["entity_id"], "one");
        assert!(short["references"][0].get("file").is_none());
        assert!(short["references"][1].get("file").is_none());
        // The qualified name carries the stored name, and the relation is
        // the one asked for, so the short row spells neither again.
        assert!(short["references"][0].get("name").is_none());
        assert_eq!(
            short["references"][0]["qualified_name"],
            "publisher.audit_manifest"
        );
        assert!(short["entity"].get("file").is_none());
        assert_eq!(short["entity"]["qualified_name"], "rates.normalize");
        assert!(short["references"][0].get("how").is_none());
        assert_eq!(short["references"][0]["proof"], "type");
        assert_eq!(short["references"][0]["call_code"], "normalize(value)");
        assert_eq!(
            short["_kin"][PROOF_CODES_KEY],
            "type: target resolved by type"
        );
        assert!(short["references"][0].get("sites").is_none());
        assert!(short["entity"].get("signature").is_none());
        assert!(!short["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("Each row shows"));
        assert_eq!(short["_kin"]["verdict"], "complete");
    }

    /// A short reference row names the caller, its module, a proof code and
    /// one call line; everything else is one details call away, unchanged.
    #[test]
    fn short_reference_rows_keep_only_what_names_and_proves_the_caller() {
        let owner = json!([{"entity_id":"f8fbb5f0-0000-4000-8000-000000000000",
            "kind":"function","name":"create_app",
            "relation_id":"43d262e9-0000-4000-8000-000000000000"}]);
        let row = |id: &str, kind: &str, role: &str, kinds: Value, resolution: &str| {
            json!({"entity_id":id,"name":"create_app.<locals>.index","kind":kind,
                "module":"task_app","role":role,"relation_kinds":kinds,"resolution":resolution,
                "semantic_owners":owner,"semantic_owner_gap":null,
                "sites":[{"expr":"app.route(\"/\")","line_in_entity":3},
                    {"expr":"app.route(\"/about\")","line_in_entity":9}]})
        };
        let mut machine = refs();
        machine["total_upstream"] = json!(3);
        machine["references"] = json!([
            row(
                "020ca13b-0000-4000-8000-000000000001",
                "Function",
                "source",
                json!(["calls"]),
                "type_resolved"
            ),
            row(
                "020ca13b-0000-4000-8000-000000000002",
                "Class",
                "test",
                json!(["calls", "imports"]),
                "import_scoped"
            ),
            row(
                "020ca13b-0000-4000-8000-000000000003",
                "Method",
                "source",
                json!(["calls"]),
                "receiver_bound"
            ),
        ]);
        let mut short = result(machine.clone());
        QueryPresentation::default().present(&mut short, "find_references", &HashMap::new());
        let short = payload(&short);
        let rows = short["references"].as_array().unwrap();
        assert_eq!(
            rows[0],
            json!({"entity_id":"020ca13b-0000-4000-8000-000000000001",
                "name":"create_app.<locals>.index","module":"task_app","proof":"type",
                "call_code":"app.route(\"/\")","more_call_code":["app.route(\"/about\")"]})
        );
        assert!(rows[0].to_string().len() < 230, "{}", rows[0]);
        // What differs from the ordinary case is still said.
        assert_eq!(rows[1]["kind"], "Class");
        assert_eq!(rows[1]["role"], "test");
        assert_eq!(rows[1]["how"], json!(["calls", "imports"]));
        assert_eq!(rows[1]["proof"], "imports");
        assert!(rows[2].get("kind").is_none() && rows[2].get("proof").is_none());
        assert_eq!(
            short["_kin"][PROOF_CODES_KEY],
            "imports: target resolved through imports; type: target resolved by type"
        );
        for row in rows {
            assert!(row.get("semantic_owners").is_none());
            assert!(!row.to_string().contains("relation_id"));
        }
        // Details serve every row exactly as the full reply does.
        let mut rich = result(machine.clone());
        QueryPresentation::default().present(
            &mut rich,
            "find_references",
            &HashMap::from([("details".into(), json!(true))]),
        );
        let mut full = result(machine);
        crate::plain_reply::present(&mut full, "find_references");
        let rich = payload(&rich);
        assert_eq!(rich["references"], payload(&full)["references"]);
        assert_eq!(rich["references"][0]["semantic_owners"], owner);
        assert!(rich["_kin"].get(PROOF_CODES_KEY).is_none());
    }

    #[test]
    fn federated_references_keep_foreign_authority_in_plain_and_both_query_shapes() {
        let mut machine = refs();
        machine["total_upstream"] = json!(1);
        machine["references"] = json!([{
            "entity_id": null, "repo_id":"consumer",
            "foreign_entity_id":"00000000-0000-0000-0000-000000000001",
            "name":"run_task", "kind":"Function", "relation_kinds":[],
            "projection":{"path":"src/app.rs"}, "sites":[],
            "sites_absent_reason":"federated_xref"
        }]);
        for details in [None, Some(false), Some(true)] {
            let mut reply = result(machine.clone());
            match details {
                None => crate::plain_reply::present(&mut reply, "find_references"),
                Some(details) => QueryPresentation::default().present(
                    &mut reply,
                    "find_references",
                    &HashMap::from([("details".into(), json!(details))]),
                ),
            }
            let answer = payload(&reply);
            let row = &answer["references"][0];
            assert_eq!(row["name"], "run_task", "{answer}");
            assert_eq!(row["repo_id"], "consumer", "{answer}");
            assert_eq!(
                row["foreign_entity_id"], "00000000-0000-0000-0000-000000000001",
                "{answer}"
            );
            assert!(row.get("entity_id").is_none(), "{answer}");
            assert!(row.get("projection").is_none(), "{answer}");
            assert!(!answer.to_string().contains("src/app.rs"), "{answer}");
            let meaning = answer["_kin"]["meaning"].as_str().unwrap();
            assert!(meaning.contains("no local source handle"), "{answer}");
            assert!(!meaning.contains("read a caller with source"), "{answer}");
        }
    }

    /// The short reply shows what each proven row's receiver is, from what
    /// the row records: the chain the linker bound it through, or the line
    /// the caller's facts say binds it. Details stay the plain answer.
    #[test]
    fn the_short_reply_shows_each_proven_receiver_it_records_and_details_stay_unchanged() {
        let mut machine = refs();
        let chain = json!([
            {"step":"call","line_in_entity":2},
            {"step":"binding","name":"ctx","line_in_entity":1,"code":"ctx = app.app_context()"},
            {"step":"returns","name":"Flask.app_context","entity_id":"ac"},
            {"step":"type","resolves_to":"AppContext","via":"returned","entity_id":"appctx"}]);
        machine["references"][0]["receiver_binding"] = chain.clone();
        machine["references"][1]["receiver_bound_in_caller"] = json!([
            {"name":"rates","line_in_entity":1,"code":"rates = load_rates()"}]);
        // A proven row recording nothing, and a row matched by name alone.
        machine["references"].as_array_mut().unwrap().extend([
            json!({"entity_id":"three","name":"plain_caller","kind":"Function",
                "relation_kinds":["calls"],"resolution":"type_resolved",
                "sites":[{"expr":"normalize(value)","line_in_entity":1}]}),
            json!({"entity_id":"four","name":"guessed","kind":"Function",
                "relation_kinds":["calls"],"resolution":"name_only","receiver_binding":chain,
                "sites":[{"expr":"x.normalize(value)","line_in_entity":1}]}),
        ]);
        machine["total_upstream"] = json!(4);
        let mut short = result(machine.clone());
        QueryPresentation::default().present(&mut short, "find_references", &HashMap::new());
        let short = payload(&short);
        let rows = short["references"].as_array().unwrap();
        assert_eq!(rows.len(), 3, "{short:#}");
        assert_eq!(
            rows[0]["receiver"],
            json!([
                "ctx = app.app_context()",
                "ctx: Flask.app_context() -> AppContext"
            ])
        );
        assert_eq!(rows[1]["receiver"], json!(["rates = load_rates()"]));
        assert!(rows[2].get("receiver").is_none(), "{short:#}");
        assert!(short["name_only"][0].get("receiver").is_none(), "{short:#}");
        assert_eq!(rows[0]["call_code"], "normalize(value)");

        let mut expected = result(machine.clone());
        crate::plain_reply::present(&mut expected, "find_references");
        let mut rich = result(machine);
        QueryPresentation::default().present(
            &mut rich,
            "find_references",
            &HashMap::from([("details".into(), json!(true))]),
        );
        let (rich, expected) = (payload(&rich), payload(&expected));
        for key in ["entity", "references", "name_only"] {
            assert_eq!(rich[key], expected[key], "{key}");
        }
        for row in rich["references"].as_array().unwrap() {
            assert!(row.get("receiver").is_none(), "{rich:#}");
            assert!(row.get("receiver_bound_in_caller").is_none(), "{rich:#}");
        }
    }

    /// What an answered reference reply leaves out, whatever its rows carry.
    fn assert_ends_on_its_answer(reply: &Value) {
        for key in [
            "next_cursor",
            "evidence_next_cursor",
            "evidence_detail",
            "relation_kinds",
            "caller_scope",
        ] {
            assert!(reply.get(key).is_none(), "{key}: {reply}");
        }
        for key in ["delivered", "page"] {
            assert!(reply["_kin"].get(key).is_none(), "{key}: {reply}");
        }
    }

    /// Details serve every row of the plain answer whole, and an answered
    /// reply still ends on its answer: the plain answer exactly, as
    /// [`crate::reply_budget::quiet_answered_references`] ends it.
    #[test]
    fn details_serve_the_plain_rows_and_an_answered_reply_still_ends_on_them() {
        let mut plain = result(refs());
        crate::plain_reply::present(&mut plain, "find_references");
        let plain = payload(&plain);
        let mut detailed = result(refs());
        QueryPresentation::default().present(
            &mut detailed,
            "find_references",
            &HashMap::from([("details".into(), json!(true))]),
        );
        let detailed = payload(&detailed);
        for key in ["entity", "references"] {
            assert_eq!(detailed[key], plain[key], "{key}");
        }
        assert!(detailed["references"][0]["sites"][0]["line_in_caller"].is_u64());
        assert!(detailed["entity"]["signature"].is_string());
        for key in ["verdict", "meaning"] {
            assert_eq!(detailed["_kin"][key], plain["_kin"][key], "{key}");
        }
        assert!(detailed["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("Each row shows"));
        // The plain answer echoed the question and accounted for its page;
        // the reply an agent gets does neither.
        assert!(plain["relation_kinds"].is_array());
        assert_ends_on_its_answer(&detailed);
        let mut expected = plain.as_object().unwrap().clone();
        assert!(crate::reply_budget::quiet_answered_references(
            &mut expected
        ));
        assert_eq!(detailed, Value::Object(expected));
    }

    /// The reply a model run was served most often: `refs` with
    /// `details: true` on an answer that is complete except listed sites.
    /// Its rows keep every site with its line, its open places stay listed,
    /// and none of the trailers the short reply leaves out comes back.
    #[test]
    fn details_on_an_answered_reply_change_its_rows_and_never_its_trailers() {
        let fixture = include_str!("testdata/plain_flask_callers_find_references.json");
        let machine: Value = serde_json::from_str(fixture).unwrap();
        let mut plain = result(machine.clone());
        crate::plain_reply::present(&mut plain, "find_references");
        let plain = payload(&plain);
        assert_eq!(
            plain["_kin"]["verdict"],
            "complete except the sites listed below"
        );
        let served = |details: bool| {
            let mut reply = result(machine.clone());
            QueryPresentation::default().present(
                &mut reply,
                "find_references",
                &HashMap::from([("details".into(), json!(details))]),
            );
            payload(&reply)
        };
        let (short, detailed) = (served(false), served(true));
        // Rows whole: each caller's sites with their lines, and each open
        // place with its caller, line and code.
        for key in [
            "entity",
            "references",
            "left_to_check",
            "passed_to_calls",
            "other_names",
        ] {
            assert_eq!(detailed[key], plain[key], "{key}");
        }
        assert!(detailed["references"][0]["sites"].is_array());
        assert!(short["references"][0].get("sites").is_none());
        // The same trailers go from both, and the same override rows stay.
        for reply in [&short, &detailed] {
            assert_ends_on_its_answer(reply);
            assert!(reply["_kin"].get("searched").is_none(), "{reply}");
            for key in ["coverage", "proven", "note"] {
                assert!(reply["overridden_by"].get(key).is_none(), "{key}");
            }
            for key in ["count", "methods", "possible", "possible_note", "gaps"] {
                assert_eq!(reply["overridden_by"][key], plain["overridden_by"][key]);
            }
            for key in ["verdict", "fix"] {
                assert_eq!(reply["_kin"][key], plain["_kin"][key], "{key}");
            }
        }
        // The plain answer carried each of them.
        assert!(plain["_kin"]["searched"].is_string());
        assert!(plain["relation_kinds"].is_array());
        assert!(plain["overridden_by"]["coverage"].is_object());
        let bytes = |reply: &Value| reply.to_string().len();
        assert!(
            bytes(&detailed) < bytes(&plain),
            "{} / {}",
            bytes(&detailed),
            bytes(&plain)
        );
    }

    #[test]
    fn the_default_cuts_large_row_evidence_without_changing_answer_membership() {
        let mut machine = refs();
        machine["references"] = json!((0..64)
            .map(|index| json!({
                "entity_id":format!("caller-{index}"),"name":format!("call_{index}"),
                "file":"publisher.py","kind":"Function","relation_kinds":["calls"],
                "resolution":"type_resolved","sites":[{"line_in_entity":1,
                    "expr":format!("normalize({})", "long_argument, ".repeat(80))}]
            }))
            .collect::<Vec<_>>());
        machine["total_upstream"] = json!(64);
        // An answered reply echoes the asked kinds in neither form.
        let asked = machine["relation_kinds"].clone();
        let mut rich = result(machine.clone());
        QueryPresentation::default().present(
            &mut rich,
            "find_references",
            &HashMap::from([("details".into(), json!(true))]),
        );
        let mut short = result(machine);
        QueryPresentation::default().present(&mut short, "find_references", &HashMap::new());
        let rich_bytes = rich["content"][0]["text"].as_str().unwrap().len();
        let short_bytes = short["content"][0]["text"].as_str().unwrap().len();
        assert!(
            short_bytes * 2 < rich_bytes,
            "{short_bytes} >= half of {rich_bytes}"
        );
        let rich = payload(&rich);
        let short = payload(&short);
        let (short_rows, rich_rows) = (
            short["references"].as_array().unwrap(),
            rich["references"].as_array().unwrap(),
        );
        assert_eq!(short_rows.len(), rich_rows.len());
        for (short_row, rich_row) in short_rows.iter().zip(rich_rows) {
            assert_short_row_agrees(short_row, rich_row, &asked);
        }
    }

    #[test]
    fn both_locate_modes_keep_identity_and_ranking_scope_without_signatures() {
        for tool in ["semantic_search", "semantic_locate"] {
            let row = json!({"id":"method", "name":"Cache.__setitem__", "qualified_name":"cache.Cache.__setitem__",
                "kind":"Method", "file_path":"cache.py", "start_line":9, "end_line":20,
                "signature":"def __setitem__(self, key, value)", "projection":"SignatureOnly"});
            let machine = if tool == "semantic_search" {
                json!({"query":"Cache.__setitem__", "results":[row], "total_matches":1,
                    "_kin":{"verdict":{"state":"certified","safe_to_conclude_absent":false}}})
            } else {
                json!({"query":"stores a cache value", "entities":[row], "total_ranked":1,
                    "ranked_by":"entity cards and the graph", "retrieval_arm":"cards",
                    "_kin":{"verdict":{"state":"certified","safe_to_conclude_absent":false}}})
            };
            let mut rich = result(machine.clone());
            crate::plain_reply::present(&mut rich, tool);
            let mut short = result(machine);
            QueryPresentation::default().present(&mut short, tool, &HashMap::new());
            let rich = payload(&rich);
            let short = payload(&short);
            let row = &short["results"][0];
            assert_eq!(row["entity_id"], "method");
            assert!(row.get("file").is_none());
            assert_eq!(row["name"], "Cache.__setitem__");
            assert_eq!(row["qualified_name"], "cache.Cache.__setitem__");
            for key in ["signature", "line", "end_line", "shown"] {
                assert!(row.get(key).is_none(), "{key}: {short}");
            }
            assert!(rich["results"][0]["signature"].is_string());
            assert_eq!(short["_kin"], rich["_kin"]);
            if tool == "semantic_locate" {
                assert!(short["_kin"]["meaning"]
                    .as_str()
                    .unwrap()
                    .contains("does not claim to have found every entity"));
            }
        }
    }

    #[test]
    fn a_bare_cursor_inherits_details_and_can_change_them_without_another_lookup() {
        let choice = QueryPresentation::default();
        let mut first = refs();
        first["next_cursor"] = json!("next");
        let mut first = result(first);
        choice.present(
            &mut first,
            "find_references",
            &HashMap::from([("details".into(), json!(true))]),
        );
        let continuation = HashMap::from([("cursor".into(), json!("next"))]);
        assert!(choice.details("find_references", &continuation));
        let mut next = result(refs());
        choice.present(&mut next, "find_references", &continuation);
        assert!(payload(&next)["references"][0]["sites"].is_array());
        let mut short = continuation;
        short.insert("details".into(), json!(false));
        assert!(!choice.details("find_references", &short));
        assert!(!QueryPresentation::default().details(
            "find_references",
            &HashMap::from([("cursor".into(), json!("next"))])
        ));
    }

    #[test]
    fn partial_coverage_name_matches_and_override_qualifications_survive() {
        let mut machine = refs();
        machine["_kin"]["verdict"] = json!({"state":"inconclusive","limiting_factor":"call_sites_owed: callers remain unchecked","safe_to_conclude_absent":false});
        machine["references"][0]["resolution"] = json!("name_only");
        machine["references"][1]["via_override_of"] = json!("Base.normalize");
        let mut rich = result(machine.clone());
        crate::plain_reply::present(&mut rich, "find_references");
        let mut short = result(machine);
        QueryPresentation::default().present(&mut short, "find_references", &HashMap::new());
        let rich = payload(&rich);
        let short = payload(&short);
        assert_eq!(short["_kin"]["verdict"], rich["_kin"]["verdict"]);
        assert_eq!(short["_kin"]["searched"], rich["_kin"]["searched"]);
        assert_eq!(short["name_only"][0]["proof"], "name");
        assert_eq!(rich["name_only"][0]["proof"], "matched by name only");
        assert_eq!(
            short["references"][0]["through_override_of"],
            "Base.normalize"
        );
        // Partial replies qualify dispatch in their finding and receiver
        // warning, rather than in the complete reply's evidence sentence.
        assert_eq!(short["_kin"]["meaning"], rich["_kin"]["meaning"]);
        let meaning = short["_kin"]["meaning"].as_str().unwrap();
        assert!(
            meaning.contains("may call it through a method it overrides"),
            "{meaning}"
        );
        assert!(
            meaning.contains("a call to the base alone does not prove the override runs"),
            "{meaning}"
        );
        assert!(meaning.contains("there may be more"), "{meaning}");
        assert!(!short["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("Each row shows"));
    }

    #[test]
    fn complete_override_qualification_survives_without_optional_quote_prose() {
        let mut machine = refs();
        machine["references"][1]["via_override_of"] = json!("Base.normalize");
        let mut rich = result(machine.clone());
        crate::plain_reply::present(&mut rich, "find_references");
        let mut short = result(machine);
        QueryPresentation::default().present(&mut short, "find_references", &HashMap::new());
        let rich = payload(&rich);
        let short = payload(&short);
        assert_eq!(short["_kin"]["verdict"], rich["_kin"]["verdict"]);
        let meaning = short["_kin"]["meaning"].as_str().unwrap();
        assert!(
            meaning.contains("may dispatch to this override"),
            "{meaning}"
        );
        assert!(
            meaning.contains("a call to the base alone does not prove the override runs"),
            "{meaning}"
        );
        assert!(!meaning.contains("Each row shows"), "{meaning}");
        assert!(rich["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("Each row shows"));
    }

    #[test]
    fn real_partial_scope_and_actionable_gap_summaries_are_preserved() {
        let fixture = include_str!("testdata/plain_flask_callers_find_references.json");
        let machine: Value = serde_json::from_str(fixture).unwrap();
        let mut rich = result(machine.clone());
        crate::plain_reply::present(&mut rich, "find_references");
        let mut short = result(machine);
        QueryPresentation::default().present(&mut short, "find_references", &HashMap::new());
        let rich = payload(&rich);
        let short = payload(&short);
        for key in ["passed_to_calls", "other_names"] {
            assert_eq!(short[key], rich[key], "{key}");
        }
        // An answered reply keeps every override row and candidate and the
        // limits that qualify them; the census counts and a note about rows
        // it does not have go.
        for key in ["count", "methods", "possible", "possible_note", "gaps"] {
            assert_eq!(
                short["overridden_by"][key], rich["overridden_by"][key],
                "{key}"
            );
        }
        for key in ["coverage", "proven", "note"] {
            assert!(short["overridden_by"].get(key).is_none(), "{key}");
        }
        let short_groups = short["left_to_check"].as_array().unwrap();
        let rich_groups = rich["left_to_check"].as_array().unwrap();
        assert_eq!(short_groups.len(), rich_groups.len());
        for (short, rich) in short_groups.iter().zip(rich_groups) {
            assert_eq!(short["why"], rich["why"]);
            assert_eq!(short["count"], rich["count"]);
            assert_inline_or_counted(short, rich);
        }
        for key in ["verdict", "fix"] {
            assert_eq!(short["_kin"][key], rich["_kin"][key], "{key}");
        }
        // It ends on its open places: the searched line said only that the
        // verdict limits coverage and that no lookup by a built name spells
        // pop, which opens no gap. The plain answer holds it.
        assert!(rich["_kin"]["searched"].is_string());
        for key in ["searched", "delivered", "page"] {
            assert!(short["_kin"].get(key).is_none(), "{key}");
        }
        assert!(short["left_to_check"][0]["why"]
            .as_str()
            .unwrap()
            .contains("could not identify"));
        assert!(short["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("Check a listed site only if"));
        assert!(!short["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("Each row shows"));
    }

    fn unresolved_refs() -> Value {
        let mut machine = refs();
        machine["_kin"]["verdict"] = json!({"state":"inconclusive",
            "safe_to_conclude_absent":false,"limiting_factor":"binding_unproven"});
        machine["_kin"]["page"] = json!({"answer_rows_complete":true,
            "answer_rows_has_more":false,"evidence_has_more":true,
            "returned_references":2,"total_references":2});
        machine["call_site_candidates"] = json!({"named":(0..55).map(|index| json!({
            "state":"binding","callee":"normalize","caller":format!("possible-{index}"),
            "caller_name":format!("possible_call_{index}"),"line_in_entity":index + 1,
            "expr":format!("normalize(argument_{index})")
        })).collect::<Vec<_>>()});
        machine["next_cursor"] = json!("later-evidence");
        machine["evidence_next_cursor"] = json!("later-evidence");
        machine["readings"] = json!({"scope":{"checked":"admitted"}});
        machine
    }

    fn binding_observation() -> Value {
        let mut observation = serde_json::to_value(
            crate::source_derivation::SourceDerivationObservation::unavailable("unmeasured"),
        )
        .unwrap();
        observation["scope"] = json!("live_head");
        observation["checked_scope"] = json!("admitted_inventory");
        observation["binding_scope"] = json!({"basis":"answer_entities","languages":["Python"],
            "prior_local_binding":"outstanding","outstanding_local_binding_obligations":777});
        observation["report"]["body_binding"] = json!("current");
        observation["report"]["prior_local_binding"] = json!("outstanding");
        observation["report"]["outstanding_local_binding_obligations"] = json!(99);
        observation["report"]["local_binding_by_language"] = json!({
            "Python":{"prior_local_binding":"outstanding", "outstanding_local_binding_obligations":23,"files":2},
            "JavaScript":{"prior_local_binding":"outstanding", "outstanding_local_binding_obligations":76,"files":1}});
        observation["report"]["issues"] = json!([
            {"path":"tests/a.py","reason":"local_binding_outstanding: unsettled names"},
            {"path":"tests/b.py","reason":"local_binding_outstanding"},
            {"path":"tests/a.py","reason":"local_binding_outstanding: another binding"},
            {"path":"different.py","reason":"import_resolution_incomplete"}]);
        observation
    }

    #[test]
    fn many_unresolved_sites_cost_counts_while_every_requested_caller_survives() {
        let machine = unresolved_refs();
        let mut expected_rich = result(machine.clone());
        crate::plain_reply::present(&mut expected_rich, "find_references");
        let mut rich = result(machine.clone());
        QueryPresentation::default().present(
            &mut rich,
            "find_references",
            &HashMap::from([("details".into(), json!(true))]),
        );
        assert_eq!(rich, expected_rich);
        let mut short = result(machine);
        QueryPresentation::default().present(&mut short, "find_references", &HashMap::new());
        let short_bytes = short["content"][0]["text"].as_str().unwrap().len();
        let rich_bytes = rich["content"][0]["text"].as_str().unwrap().len();
        assert!(short_bytes * 3 < rich_bytes, "{short_bytes} / {rich_bytes}");
        let short = payload(&short);
        let rich = payload(&rich);
        assert_eq!(short["left_to_check"][0]["retained_entries"], 55);
        assert_eq!(
            rich["left_to_check"][0]["sites"].as_array().unwrap().len(),
            55
        );
        assert!(short["left_to_check"][0].get("sites").is_none());
        for (short_row, rich_row) in short["references"]
            .as_array()
            .unwrap()
            .iter()
            .zip(rich["references"].as_array().unwrap())
        {
            assert_short_row_agrees(short_row, rich_row, &rich["relation_kinds"]);
        }
        for key in ["verdict", "searched", "delivered", "page"] {
            assert_eq!(short["_kin"][key], rich["_kin"][key], "{key}");
        }
        for key in ["next_cursor", "evidence_next_cursor", "readings"] {
            assert_eq!(short[key], rich[key], "{key}");
        }
        let hint = short["evidence_detail"].as_str().unwrap();
        assert!(hint.contains("not proven callers"));
        assert!(hint.contains("incoming cursor with details:true"));
        assert!(hint.contains("Returned cursors advance later evidence"));
    }

    #[test]
    fn evidence_totals_and_compacted_occurrences_are_distinct_from_page_entries() {
        let mut plain = json!({"left_to_check":[{"why":"Unresolved target scope; these are not calls.",
            "count":120,"by_file":[{"file":"a.py","count":120,"lines":[1,2,3]}],
            "sites":[{"occurrences":55,"caller_id":"unknown","expr_unavailable":"No stored text".repeat(80)}]}]});
        compact_reference_gaps(&mut plain);
        assert_eq!(plain["left_to_check"][0]["retained_entries"], 55);
        assert_eq!(plain["left_to_check"][0]["count"], 120);
        assert_eq!(plain["left_to_check"][0]["by_file"][0]["count"], 120);
        assert!(plain["left_to_check"][0]["by_file"][0]
            .get("lines")
            .is_none());
        assert!(plain["left_to_check"][0].get("sites").is_none());
    }

    #[test]
    fn unknown_counts_and_tiny_evidence_do_not_get_invented_or_enlarged() {
        for sites in [
            json!([{}]),
            json!([]),
            json!([{"occurrences":0,"code":"long".repeat(100)}]),
            json!([{"occurrences":"many","code":"long".repeat(100)}]),
            json!([{"occurrences":u64::MAX,"code":"long".repeat(100)},{}]),
        ] {
            let mut plain = json!({"left_to_check":[{"why":"Unresolved", "sites":sites}]});
            let original = plain.clone();
            compact_reference_gaps(&mut plain);
            assert_eq!(plain, original);
        }
        let mut existing = json!({"evidence_detail":"kept", "left_to_check":[{
            "why":"Unresolved", "sites":[{"code":"long".repeat(100)}]}]});
        let original = existing.clone();
        compact_reference_gaps(&mut existing);
        assert_eq!(existing, original);
    }

    #[test]
    fn unresolved_detail_can_be_recovered_on_the_same_page_without_advancing() {
        let choice = QueryPresentation::default();
        let mut short = result(unresolved_refs());
        choice.present(&mut short, "find_references", &HashMap::new());
        assert!(!choice.details(
            "find_references",
            &HashMap::from([("cursor".into(), json!("later-evidence"))])
        ));
        // The incoming cursor selects this page, while the returned cursor
        // selects the next one. A presentation override changes neither.
        let incoming = HashMap::from([
            ("cursor".into(), json!("this-evidence")),
            ("details".into(), json!(true)),
        ]);
        let mut recovered = result(unresolved_refs());
        choice.present(&mut recovered, "find_references", &incoming);
        let recovered = payload(&recovered);
        assert_eq!(
            recovered["left_to_check"][0]["sites"]
                .as_array()
                .unwrap()
                .len(),
            55
        );
        assert_eq!(recovered["evidence_next_cursor"], "later-evidence");
        assert!(recovered.get("evidence_detail").is_none());
        assert!(choice.details(
            "find_references",
            &HashMap::from([("cursor".into(), json!("later-evidence"))])
        ));
    }

    #[test]
    fn a_frozen_evidence_page_replays_its_sites_with_an_explicit_detail_override() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let authority = json!({"repo":"private","generation":4});
        let arguments = HashMap::from([
            ("entity_id".into(), json!("target")),
            ("relation_kinds".into(), json!(["calls"])),
            ("details".into(), json!(false)),
        ]);
        let mut machine = unresolved_refs();
        machine["_kin"]["verdict"]["limiting_factor"] = json!("local_binding_outstanding");
        machine["_kin"]["source_derivation"] = binding_observation();
        machine.as_object_mut().unwrap().remove("next_cursor");
        machine
            .as_object_mut()
            .unwrap()
            .remove("evidence_next_cursor");
        machine["references"] = json!([]);
        machine["total_upstream"] = json!(0);
        let first = crate::trace_pages::start_kind(
            machine,
            crate::reference_pages::context(&arguments, &authority),
            4000,
            crate::trace_pages::PageKind::References,
        )
        .unwrap();
        assert_eq!(first["_kin"]["reference_gap"]["count"], 23);
        let cursor = first["evidence_next_cursor"].as_str().unwrap();
        let incoming = HashMap::from([("cursor".into(), json!(cursor))]);
        let mut detailed_incoming = incoming.clone();
        detailed_incoming.insert("details".into(), json!(true));
        let short_page = crate::reference_pages::resume(
            cursor,
            &crate::reference_pages::context(&incoming, &authority),
            4000,
        )
        .unwrap();
        let rich_page = crate::reference_pages::resume(
            cursor,
            &crate::reference_pages::context(&detailed_incoming, &authority),
            4000,
        )
        .unwrap();
        assert_eq!(short_page, rich_page);
        let choice = QueryPresentation::default();
        let mut short = result(short_page);
        choice.present(&mut short, "find_references", &incoming);
        let mut rich = result(rich_page);
        choice.present(&mut rich, "find_references", &detailed_incoming);
        let short = payload(&short);
        let rich = payload(&rich);
        // The binding gap is stated once, on the first page, beside the
        // answer's whole identity. A continuation keeps the verdict and its
        // code, and does not restate the gap at the tool floor or anywhere.
        let mut opening = result(first.clone());
        choice.present(&mut opening, "find_references", &arguments);
        let opening = payload(&opening);
        assert_eq!(opening["coverage_gaps"][0]["count"], 23);
        assert_eq!(
            opening["coverage_gaps"][0]["count_scope"],
            "referring_languages"
        );
        assert!(short.get("coverage_gaps").is_none(), "{short}");
        assert_eq!(short["coverage_gaps"], rich["coverage_gaps"]);
        assert_eq!(short["_kin"]["verdict"], opening["_kin"]["verdict"]);
        assert_inline_or_counted(&short["left_to_check"][0], &rich["left_to_check"][0]);
        for key in [
            "next_cursor",
            "evidence_next_cursor",
            "readings",
            "record_fragment",
        ] {
            assert_eq!(short[key], rich[key], "{key}");
        }
        assert_eq!(short["_kin"]["verdict"], rich["_kin"]["verdict"]);
        assert_eq!(short["_kin"]["page"], rich["_kin"]["page"]);
    }

    #[test]
    fn recorded_binding_obligations_keep_their_semantic_scope_without_locations() {
        let mut machine = refs();
        machine["_kin"]["verdict"] = json!({"state":"inconclusive",
            "safe_to_conclude_absent":false,"limiting_factor":"local_binding_outstanding"});
        machine["_kin"]["source_derivation"] = binding_observation();
        machine["call_site_candidates"] = json!({"census":{"residue":{"mentions":{
            "unmatched":{"count":1,"sites":[{"file":"different.py","kind":"import"}]}}}}});
        let mut rich = result(machine.clone());
        crate::plain_reply::present(&mut rich, "find_references");
        let mut explicit = result(machine.clone());
        QueryPresentation::default().present(
            &mut explicit,
            "find_references",
            &HashMap::from([("details".into(), json!(true))]),
        );
        assert_eq!(explicit, rich);
        let mut short = result(machine.clone());
        QueryPresentation::default().present(&mut short, "find_references", &HashMap::new());
        let short = payload(&short);
        let rich = payload(&rich);
        assert_eq!(short["coverage_gaps"], rich["coverage_gaps"]);
        // The short form adds only the legend for its proof codes.
        let mut short_kin = short["_kin"].clone();
        short_kin.as_object_mut().unwrap().remove(PROOF_CODES_KEY);
        assert_eq!(short_kin, rich["_kin"]);
        assert_eq!(short["entity"]["entity_id"], rich["entity"]["entity_id"]);
        assert_eq!(
            short["references"].as_array().unwrap().len(),
            rich["references"].as_array().unwrap().len()
        );
        for (short_row, rich_row) in short["references"]
            .as_array()
            .unwrap()
            .iter()
            .zip(rich["references"].as_array().unwrap())
        {
            assert_eq!(short_row["entity_id"], rich_row["entity_id"]);
            assert!(short_row.get("sites").is_none());
            assert!(!rich_row["sites"].as_array().unwrap().is_empty());
        }
        let gap = &short["coverage_gaps"][0];
        assert_eq!(gap["kind"], "local_bindings");
        assert_eq!(gap["count"], 23);
        assert_eq!(gap["count_scope"], "referring_languages");
        assert_eq!(gap["basis"], "answer_entities");
        assert_eq!(gap["checked_scope"], "admitted_inventory");
        assert!(gap.get("recorded_inventory_issue_files").is_none());
        assert!(gap.get("omitted_recorded_inventory_issue_files").is_none());
        assert!(gap["unit"].as_str().unwrap().contains("not a caller count"));
        assert!(gap["scope_note"]
            .as_str()
            .unwrap()
            .contains("narrower semantic scope"));
        assert_eq!(short["_kin"]["verdict"], "partial");
        machine["_kin"]["verdict"]["limiting_factor"] = json!("binding_unproven");
        for details in [false, true] {
            let mut other = result(machine.clone());
            QueryPresentation::default().present(
                &mut other,
                "find_references",
                &HashMap::from([("details".into(), json!(details))]),
            );
            assert!(payload(&other).get("coverage_gaps").is_none());
        }
    }

    #[test]
    fn a_disclosure_copy_does_not_replace_unmeasured_binding_evidence() {
        let mut machine = refs();
        machine["_kin"]["verdict"] = json!({"state":"inconclusive",
            "safe_to_conclude_absent":false,"limiting_factor":"local_binding_outstanding"});
        let mut observation = binding_observation();
        observation["report"]["outstanding_local_binding_obligations"] = Value::Null;
        observation["report"]["local_binding_by_language"] = Value::Null;
        machine["_kin"]["source_derivation"] = observation;
        let mut short = result(machine);
        QueryPresentation::default().present(&mut short, "find_references", &HashMap::new());
        let short = payload(&short);
        let gap = &short["coverage_gaps"][0];
        assert!(gap.get("count").is_none());
        assert_eq!(gap["count_measured"], false);
        assert_eq!(gap["basis"], "answer_entities");
    }

    #[test]
    fn exhausted_pages_and_excluded_issue_samples_do_not_certify_reference_coverage() {
        for empty in [false, true] {
            for details in [false, true] {
                let mut machine = refs();
                if empty {
                    machine["references"] = json!([]);
                    machine["total_upstream"] = json!(0);
                }
                machine["_kin"]["verdict"] = json!({"state":"inconclusive",
                    "safe_to_conclude_absent":false,"limiting_factor":"local_binding_outstanding"});
                let mut observation = binding_observation();
                // The producer retained only these diagnostics. Other issues
                // were counted before this presentation received the report.
                observation["report"]["issue_count"] = json!(12);
                observation["report"]["issues"] = json!([
                    {"path":"tests/a.py","reason":"local_binding_outstanding"},
                    {"path":"tests/b.py","reason":"local_binding_outstanding"},
                    {"path":"tests/c.py","reason":"local_binding_outstanding"},
                    {"path":"tests/d.py","reason":"local_binding_outstanding"}]);
                machine["_kin"]["source_derivation"] = observation;
                machine["_kin"]["page"] = json!({"kind":"references","has_more":false,
                    "answer_rows_complete":true,"answer_rows_has_more":false,"evidence_has_more":false});
                let mut served = result(machine);
                QueryPresentation::default().present(
                    &mut served,
                    "find_references",
                    &HashMap::from([("details".into(), json!(details))]),
                );
                let served = payload(&served);
                assert_eq!(served["_kin"]["verdict"], "partial");
                // Plain replies name the transport fact as delivery of
                // recorded rows; the verdict alone limits the conclusion.
                assert_eq!(served["_kin"]["page"]["recorded_rows_exhausted"], true);
                assert!(served["_kin"]["page"].get("answer_rows_complete").is_none());
                let delivered = served["_kin"]["delivered"].as_str().unwrap();
                assert!(
                    delivered.contains(
                        "No more recorded reference rows or retained evidence pages remain; \
                         the verdict still limits what can be concluded."
                    ),
                    "{delivered}"
                );
                let gap = &served["coverage_gaps"][0];
                assert_eq!(gap["count"], 23);
                assert!(gap.get("recorded_inventory_issue_files").is_none());
                assert!(gap.get("omitted_recorded_inventory_issue_files").is_none());
                assert!(gap["scope_note"]
                    .as_str()
                    .unwrap()
                    .contains("does not certify completeness"));
            }
        }
    }

    #[test]
    fn an_inconsistent_language_split_keeps_the_inventory_reading() {
        let mut machine = refs();
        machine["_kin"]["verdict"] = json!({"state":"inconclusive",
            "safe_to_conclude_absent":false,"limiting_factor":"local_binding_outstanding"});
        let mut observation = binding_observation();
        observation["report"]["local_binding_by_language"]
            .as_object_mut()
            .unwrap()
            .remove("JavaScript");
        machine["_kin"]["source_derivation"] = observation;
        let mut short = result(machine);
        QueryPresentation::default().present(&mut short, "find_references", &HashMap::new());
        let short = payload(&short);
        assert_eq!(short["coverage_gaps"][0]["count"], 99);
        assert_eq!(
            short["coverage_gaps"][0]["count_scope"],
            "checked_inventory"
        );
        assert_eq!(short["_kin"]["verdict"], "partial");
    }

    #[test]
    fn binding_diagnostics_remain_semantic_in_default_detailed_and_detached_replies() {
        let paths = json!(["tests/find_references.py", "tools/get_entity_source.py"]);
        let mut machine = refs();
        machine["_kin"]["verdict"] = json!({"state":"inconclusive",
            "safe_to_conclude_absent":false,"limiting_factor":"local_binding_outstanding"});
        let mut observation = binding_observation();
        observation["report"]["issues"] = json!(paths
            .as_array()
            .unwrap()
            .iter()
            .map(|path| json!({
            "path":path,"reason":"local_binding_outstanding"}))
            .collect::<Vec<_>>());
        machine["_kin"]["source_derivation"] = observation;
        let gap = crate::plain_reply::reference_binding_gap(&machine["_kin"]).unwrap();
        for detached in [false, true] {
            let mut page = machine.clone();
            if detached {
                page.as_object_mut().unwrap().remove("focal_entity");
                page["_kin"]["reference_focal"] =
                    json!({"entity_id":"target", "name":"normalize", "file":"rates.py"});
                page["_kin"]["reference_gap"] = gap.clone();
                page["_kin"]["source_derivation"] = json!({"checked_scope":"admitted_inventory", "counts_only":true,
                    "report":{"full_adapter_sources":3,"excluded_artifacts":0}});
            }
            let mut rich = result(page.clone());
            QueryPresentation::default().present(
                &mut rich,
                "find_references",
                &HashMap::from([("details".into(), json!(true))]),
            );
            let mut served = result(page);
            QueryPresentation::default().present(&mut served, "find_references", &HashMap::new());
            let served = payload(&served);
            let rich = payload(&rich);
            assert_eq!(served["coverage_gaps"], rich["coverage_gaps"]);
            assert_eq!(served["coverage_gaps"][0]["scope_note"], gap["scope_note"]);
            let visible_gap = &served["coverage_gaps"][0];
            assert!(visible_gap.get("recorded_inventory_issue_files").is_none());
            assert!(visible_gap
                .get("omitted_recorded_inventory_issue_files")
                .is_none());
            for path in paths.as_array().unwrap() {
                assert!(!visible_gap.to_string().contains(path.as_str().unwrap()));
            }
            assert_eq!(served["coverage_gaps"][0]["count"], 23);
            assert_eq!(served["_kin"]["verdict"], "partial");
            assert_eq!(served["entity"]["entity_id"], "target");
        }
    }

    #[test]
    fn unresolved_fragment_pages_and_non_reference_shapes_remain_lossless() {
        let mut machine = unresolved_refs();
        machine["record_fragment"] = json!({"path":"call_site_candidates.named[0].expr",
            "utf8":"normalize(argument)","byte_offset":0});
        let mut rich = result(machine.clone());
        crate::plain_reply::present(&mut rich, "find_references");
        let mut short = result(machine);
        QueryPresentation::default().present(&mut short, "find_references", &HashMap::new());
        assert_eq!(short, rich);
        let mut other = json!({"left_to_check":[{"why":"Different tool", "sites":[{
            "code":"long".repeat(100)}]}]});
        let original = other.clone();
        compact_answer(&mut other, "semantic_search");
        assert_eq!(other, original);
    }

    #[test]
    fn zero_partial_and_detached_fragment_pages_show_the_frozen_focal_without_claiming_completion()
    {
        let frozen = json!({"entity_id":"nested-target", "name":"factory.<locals>.wrapper",
            "qualified_name":"module.factory.<locals>.wrapper", "file":"module.py", "kind":"function"});
        for fragment in [false, true] {
            let mut machine = unresolved_refs();
            machine.as_object_mut().unwrap().remove("focal_entity");
            machine["references"] = json!([]);
            machine["total_upstream"] = json!(0);
            machine["_kin"]["reference_focal"] = frozen.clone();
            machine["_kin"]["page"]["returned_references"] = json!(0);
            machine["_kin"]["page"]["total_references"] = json!(0);
            if fragment {
                machine["record_fragment"] = json!({"path":"focal_entity.body", "utf8":"exact text",
                    "record":1,"byte_offset":0});
            }
            let mut rich = result(machine.clone());
            crate::plain_reply::present(&mut rich, "find_references");
            let mut short = result(machine);
            QueryPresentation::default().present(&mut short, "find_references", &HashMap::new());
            let short = payload(&short);
            let rich = payload(&rich);
            let mut expected = frozen.clone();
            expected.as_object_mut().unwrap().remove("file");
            assert_eq!(short["entity"], expected);
            assert_eq!(rich["entity"], expected);
            assert_eq!(short["_kin"]["verdict"], "partial");
            assert_eq!(short["_kin"]["meaning"], rich["_kin"]["meaning"]);
            assert!(short["_kin"]["meaning"]
                .as_str()
                .unwrap()
                .contains("Unresolved sites could still add callers"));
            for key in [
                "next_cursor",
                "evidence_next_cursor",
                "readings",
                "record_fragment",
            ] {
                assert_eq!(short[key], rich[key], "{key}");
            }
            if fragment {
                assert_eq!(short, rich);
            }
        }
        let mut direct = unresolved_refs();
        direct["_kin"]["reference_focal"] = frozen;
        let mut short = result(direct);
        QueryPresentation::default().present(&mut short, "find_references", &HashMap::new());
        assert_eq!(payload(&short)["entity"]["entity_id"], "target");
    }

    #[test]
    fn detached_method_evidence_uses_the_frozen_owner_without_changing_page_delivery() {
        let focal = json!({"entity_id":"method", "name":"AppContext.pop", "kind":"Method",
            "qualified_name":"context.AppContext.pop", "file":"context.py"});
        for state in ["inconclusive", "certified_except"] {
            let mut machine = refs();
            machine["focal_entity"] = focal.clone();
            machine["references"] = json!([{"entity_id":"named-caller", "name":"consume",
                "file":"consumer.py", "kind":"Function", "resolution":"name_only",
                "relation_kinds":["calls"], "sites":[{"expr":"value.pop()", "line_in_entity":2}]}]);
            machine["total_upstream"] = json!(1);
            machine["_kin"]["verdict"] = json!({"state":state, "safe_to_conclude_absent":false,
                "limiting_factor":"binding_unproven; named_leftovers"});
            machine["_kin"]["page"] = json!({"answer_rows_complete":true,
                "answer_rows_has_more":false,"evidence_has_more":true,
                "returned_references":1,"total_references":1});
            machine["next_cursor"] = json!("later");
            machine["evidence_next_cursor"] = json!("later");
            machine["call_site_candidates"] = json!({"named":[{
                "caller":"possible", "caller_name":"possible_call", "callee":"pop",
                "expr":"other.pop()", "line_in_entity":3, "state":"unresolved"}],
                "value_calls":{"census":{"escape":"escapes","residue":{"bindings":{
                    "count":1, "sites":[{"file":"consumer.py", "line":4, "expr":"alias = pop"}]}}}}});
            let mut full = result(machine.clone());
            QueryPresentation::default().present(
                &mut full,
                "find_references",
                &HashMap::from([("details".into(), json!(true))]),
            );
            let full = payload(&full);
            if state == "certified_except" {
                assert!(full["_kin"]["meaning"]
                    .as_str()
                    .unwrap()
                    .contains("Count a name_only row only if its object could be an AppContext"));
            }
            let mut detached = machine.clone();
            detached.as_object_mut().unwrap().remove("focal_entity");
            let mut without_label = result(detached.clone());
            QueryPresentation::default().present(
                &mut without_label,
                "find_references",
                &HashMap::new(),
            );
            let without_label = payload(&without_label);
            detached["_kin"]["reference_focal"] = focal.clone();
            let mut rich = result(detached.clone());
            QueryPresentation::default().present(
                &mut rich,
                "find_references",
                &HashMap::from([("details".into(), json!(true))]),
            );
            let rich = payload(&rich);
            assert_eq!(rich["left_to_check"], full["left_to_check"]);
            assert_eq!(rich["name_only"], full["name_only"]);
            assert!(rich["left_to_check"][0]["why"]
                .as_str()
                .unwrap()
                .contains("an AppContext"));
            assert!(rich["left_to_check"][1]["why"]
                .as_str()
                .unwrap()
                .contains("AppContext.pop"));
            let mut short = result(detached);
            QueryPresentation::default().present(&mut short, "find_references", &HashMap::new());
            let short = payload(&short);
            let mut expected = focal.clone();
            expected.as_object_mut().unwrap().remove("file");
            assert_eq!(short["entity"], expected);
            assert_eq!(short["name_only"][0]["entity_id"], "named-caller");
            assert_eq!(short["name_only"][0]["proof"], "name");
            assert_eq!(rich["name_only"][0]["proof"], "matched by name only");
            assert!(short["_kin"][PROOF_CODES_KEY]
                .as_str()
                .unwrap()
                .contains("name: matched by name only"));
            for (short_group, full_group) in short["left_to_check"]
                .as_array()
                .unwrap()
                .iter()
                .zip(full["left_to_check"].as_array().unwrap())
            {
                assert_eq!(short_group["why"], full_group["why"]);
            }
            for key in ["meaning", "verdict", "page", "delivered"] {
                assert_eq!(short["_kin"][key], without_label["_kin"][key], "{key}");
            }
            assert!(short["_kin"]["meaning"]
                .as_str()
                .unwrap()
                .contains("this page holds 1 of the 1 rows"));
            for key in ["next_cursor", "evidence_next_cursor"] {
                assert_eq!(short[key], without_label[key]);
            }
            machine["_kin"]["reference_focal"] =
                json!({"entity_id":"wrong", "name":"Wrong.pop", "kind":"Method"});
            let mut direct = result(machine);
            QueryPresentation::default().present(
                &mut direct,
                "find_references",
                &HashMap::from([("details".into(), json!(true))]),
            );
            let direct = payload(&direct);
            assert_eq!(direct["entity"], full["entity"]);
            assert_eq!(direct["left_to_check"], full["left_to_check"]);
            assert_eq!(direct["_kin"]["meaning"], full["_kin"]["meaning"]);
        }
    }

    #[test]
    fn section_qualification_and_all_delivery_handles_remain_actionable() {
        let mut section = refs();
        section["qualified_name"] = json!("other.rates.normalize");
        section["focal_entity"]
            .as_object_mut()
            .unwrap()
            .remove("qualified_name");
        let machine = json!({"query":"normalize", "candidates_by_owner":[section],
            "next_cursor":"both", "answer_next_cursor":"rows", "evidence_next_cursor":"gaps",
            "_kin":{"verdict":{"state":"inconclusive","safe_to_conclude_absent":false}}});
        let mut short = result(machine);
        QueryPresentation::default().present(&mut short, "find_references", &HashMap::new());
        let short = payload(&short);
        assert_eq!(
            short["answers"][0]["entity"]["qualified_name"],
            "other.rates.normalize"
        );
        assert_eq!(short["next_cursor"], "both");
        assert_eq!(short["answer_next_cursor"], "rows");
        assert_eq!(short["evidence_next_cursor"], "gaps");
    }

    #[test]
    fn returned_section_cursors_inherit_details_but_data_never_becomes_a_handle() {
        let choice = QueryPresentation::default();
        let mut section = refs();
        section["next_cursor"] = json!("section-rows");
        section["evidence_next_cursor"] = json!("section-gaps");
        let machine = json!({"query":"normalize", "candidates_by_owner":[section], "next_cursor":"outer",
            "readings":{"answers":[{"next_cursor":"reading-data"}]},
            "record_fragment":{"next_cursor":"fragment-data","text":"get_entity_source"},
            "_kin":{"verdict":{"state":"inconclusive","safe_to_conclude_absent":false}}});
        let mut rich = result(machine);
        choice.present(
            &mut rich,
            "find_references",
            &HashMap::from([("details".into(), json!(true))]),
        );
        assert_eq!(payload(&rich)["answers"][0]["next_cursor"], "section-rows");
        for cursor in ["outer", "section-rows", "section-gaps"] {
            assert!(
                choice.details(
                    "find_references",
                    &HashMap::from([("cursor".into(), json!(cursor))])
                ),
                "{cursor}"
            );
        }
        for cursor in ["reading-data", "fragment-data"] {
            assert!(
                !choice.details(
                    "find_references",
                    &HashMap::from([("cursor".into(), json!(cursor))])
                ),
                "{cursor}"
            );
        }
    }

    #[test]
    fn detail_choices_are_bounded_expire_and_do_not_cross_tools() {
        let choice = QueryPresentation::default();
        for index in 0..=MAX_CURSOR_CHOICES {
            choice.remember(
                "find_references",
                &json!({"next_cursor":format!("opaque-{index}")}),
                true,
            );
        }
        let args = |index| HashMap::from([("cursor".into(), json!(format!("opaque-{index}")))]);
        assert_eq!(choice.choices.lock().unwrap().len(), MAX_CURSOR_CHOICES);
        assert!(!choice.details("find_references", &args(0)));
        assert!(choice.details("find_references", &args(MAX_CURSOR_CHOICES)));
        assert!(!choice.details("semantic_search", &args(MAX_CURSOR_CHOICES)));
        choice.choices.lock().unwrap().back_mut().unwrap().at = Instant::now() - CURSOR_CHOICE_TTL;
        assert!(!choice.details("find_references", &args(MAX_CURSOR_CHOICES)));
        assert!(choice.details(
            "find_references",
            &HashMap::from([
                ("cursor".into(), json!("unparsed")),
                ("details".into(), json!(true))
            ])
        ));
    }

    #[test]
    fn changing_details_resumes_the_same_frozen_answer_without_changing_authority() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let authority = json!({"repo":"private", "generation":4});
        let arguments = HashMap::from([
            ("query".into(), json!("rates.normalize")),
            ("details".into(), json!(false)),
        ]);
        let mut machine = refs();
        machine["references"] = json!((0..80)
            .map(|index| json!({"entity_id":format!("caller-{index}"),
            "name":format!("call_{index}"), "file":"publisher.py", "signature":"x".repeat(150),
            "relation_kinds":["calls"], "resolution":"type_resolved"}))
            .collect::<Vec<_>>());
        let first = crate::trace_pages::start_kind(
            machine,
            crate::reference_pages::context(&arguments, &authority),
            4000,
            crate::trace_pages::PageKind::References,
        )
        .unwrap();
        let cursor = first["next_cursor"].as_str().unwrap();
        let mut changed = arguments.clone();
        changed.insert("details".into(), json!(true));
        let expected = crate::reference_pages::resume(
            cursor,
            &crate::reference_pages::context(&arguments, &authority),
            4000,
        )
        .unwrap();
        let actual = crate::reference_pages::resume(
            cursor,
            &crate::reference_pages::context(&changed, &authority),
            4000,
        )
        .unwrap();
        assert_eq!(actual, expected);
        assert!(crate::reference_pages::resume(
            cursor,
            &crate::reference_pages::context(&changed, &json!({"repo":"private","generation":5})),
            4000
        )
        .is_err());
    }

    #[test]
    fn fragments_readings_and_delivery_are_preserved_losslessly() {
        let mut machine = refs();
        machine["record_fragment"] = json!({"path":"references[2].signature","utf8":"get_entity_source semantic_locate","record":2,"byte_offset":0});
        machine["readings"] = json!({"scope":{"repository":"repo"},"safety":{"open":"unresolved"}});
        machine["next_cursor"] = json!("more");
        let mut rich = result(machine.clone());
        crate::plain_reply::present(&mut rich, "find_references");
        let mut short = result(machine);
        QueryPresentation::default().present(&mut short, "find_references", &HashMap::new());
        assert_eq!(short, rich);
        assert_eq!(
            payload(&short)["record_fragment"]["utf8"],
            "get_entity_source semantic_locate"
        );
        assert_eq!(payload(&short)["readings"]["safety"]["open"], "unresolved");
        assert_eq!(payload(&short)["next_cursor"], "more");
    }
}
