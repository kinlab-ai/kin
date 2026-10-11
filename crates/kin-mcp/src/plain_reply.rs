// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Plain replies for the agent profiles.
//!
//! An agent reads every field of a reply, and it cannot tell a field it should
//! act on from bookkeeping. In a scored benchmark run an agent held all six
//! callers of a method after its second call and spent the next eight turns
//! re-checking them, because the reply that proved them also said
//! `certified_except`, `named_leftovers; value_calls_unmeasured`,
//! `call_extraction: incomplete` and `excluded_artifacts: 153`. None of those
//! contradicted the answer, and every one of them read as a reason to doubt it.
//!
//! So an agent profile is served each answer in plain words, rendered here
//! from the machine reply at the last step before it leaves the server:
//!
//! - `_kin` holds one plain verdict (`complete`, `complete except the sites
//!   listed below`, or `partial`), one sentence saying what the answer means
//!   and what is left to check, and, when the reply measured it, one line
//!   saying what was searched. Nothing else in the reply restates or qualifies
//!   the verdict, so it is the one source of truth.
//! - A reference answer states what is left to check with plain reasons and
//!   retained evidence counts. Explicit details include each site's caller,
//!   line and code as written.
//! - The blocks the verdict was computed from (`negative`, `edge_coverage`,
//!   `caller_arrival`, `source_derivation` and the rest) are not served. They
//!   are the working, and the verdict already says what they add up to.
//!
//! The machine reply is unchanged. The `full` profile serves it, and so does an
//! agent profile started with `KIN_MCP_MACHINE_REPLIES=1`, which is how tests
//! and acceptance scripts read the codes on these profiles, or one whose client
//! asks for [`MACHINE_REPLIES_CAPABILITY`] at `initialize`, as `kin agent run`
//! does. The `benchmark` and
//! `context-bench` profiles are never rendered here: their bytes are an input to
//! a citable result.

use serde_json::{json, Map, Value};

use crate::answer_account::{AnswerStanding, Basis};

pub use crate::answer_account::SETTLED_KEY;
use crate::answer_account::{
    article, calls_only_question, capitalized, counted, is_call_resolution_code,
    is_link_coverage_code, leave_out_override_census, only_unkeyed, owner_of, plural,
    reach_finished, reach_itemizes, reference_lists_open_places, trace_frontier_listed,
    trace_value_places, trace_value_qualification_open, unrelated_bindings, value_uses_ruled_out,
    MachineVerdict, NameClosure, NameSearchAccount, Operation, OverrideCoverage,
    OverridePageAccount, ReferenceAccount, ValuePlaces, CLASS_ATTRIBUTE_REASON,
};
#[cfg(test)]
use crate::answer_account::{compact_uninspectable_sites, left_to_check, without_proven_sites};
#[cfg(test)]
pub(crate) use crate::answer_account::{
    CALLER_ALREADY_A_ROW, GAP_REASONS_KEY, MEMBER_OF_OUTSIDE_TYPE, PASSER_ALREADY_A_ROW,
    RECEIVER_IMPORTED_FROM_OUTSIDE, RECEIVER_OF_UNRELATED_CLASS, RECEIVER_OUTSIDE_KEY,
};
pub(crate) use crate::answer_account::{
    RECEIVER_IMPORTED_FROM_KEY, REFERENCE_COMPLETION_UNMEASURED, SOURCE_TOOL, TRACE_TOOL,
};

/// The environment variable that keeps an agent profile on machine replies.
pub const MACHINE_REPLIES_ENV: &str = "KIN_MCP_MACHINE_REPLIES";

/// Whether the environment asks for machine replies on an agent profile.
pub fn machine_replies_requested() -> bool {
    std::env::var(MACHINE_REPLIES_ENV).is_ok_and(|value| value == "1")
}

/// The `initialize` capability a client sets, under `experimental.kin`, to be
/// served machine replies on an agent profile. `kin agent run` sets it: its
/// harness reads the verdict codes itself.
pub const MACHINE_REPLIES_CAPABILITY: &str = "machineReplies";

/// Whether an `initialize` request asks for machine replies.
pub fn client_wants_machine_replies(initialize: &Value) -> bool {
    initialize
        .pointer("/params/capabilities/experimental/kin")
        .and_then(|kin| kin.get(MACHINE_REPLIES_CAPABILITY))
        .and_then(Value::as_bool)
        == Some(true)
}

/// The plain verdict words.
pub const COMPLETE: &str = "complete";
pub const COMPLETE_EXCEPT: &str = "complete except the sites listed below";
pub const COMPLETE_EXCEPT_INDIRECT: &str = "complete except indirect uses";
pub const PARTIAL: &str = "partial";

/// Where a reference answer reports the sites a sweep could not decide after
/// repeated passes over unchanged code: `{"count": N, "sites": [...]}`.
pub const UNDECIDED_REFERENCE_SITES_KEY: &str = "undecided_reference_sites";

/// Where a plain reference answer lists what is left to check.
pub const LEFT_TO_CHECK_KEY: &str = "left_to_check";

/// Where a plain reference answer lists the rows it matched by name alone,
/// apart from the rows a type proved.
pub const NAME_ONLY_KEY: &str = "name_only";

/// Where a reference answer lists the modules that import or re-export the
/// entity at module level and make no call to it. Such a row says a file
/// brings the name in, which bears on whether it can be removed, and it is
/// not a function that uses it: listed among the functions, a reader who is
/// asked which functions use the entity copies the module out with them. The
/// key is the heading a text reply prints over the group.
pub const MODULE_LEVEL_USERS_KEY: &str = "imported_or_reexported_at_module_level_by";

/// Where a reference answer lists the modules whose own top-level code uses
/// the entity: a call or a read in a module's statements, or in an anonymous
/// callback those statements pass, such as a test file's `describe` and `it`
/// blocks. Such a module is no function, so it is listed apart from the
/// functions for the reason [`MODULE_LEVEL_USERS_KEY`] gives, and it still
/// counts as a use of the entity: an entity it uses is not dead. The key is
/// the heading a text reply prints over the group.
pub const MODULE_CODE_USERS_KEY: &str = "used_by_module_level_code_of";

/// Where a plain reference answer lists the calls that pass the entity to
/// other code as an argument.
pub const PASSED_TO_CALLS_KEY: &str = "passed_to_calls";

/// Machine blocks an agent profile does not serve. Each is evidence a verdict
/// was computed from, or accounting for the response budget, and the plain
/// verdict states what it adds up to.
const WORKING_KEYS: [&str; 20] = [
    "negative",
    "edge_coverage",
    "caller_arrival",
    "source_derivation",
    "span_coherence",
    "degradations",
    "elisions",
    "dependency_selection",
    "answer_tokens",
    "measured_tokens",
    "tokens_used",
    "token_budget",
    "max_response_chars",
    "include_type_edges",
    "limit_per_step",
    "bodies_included",
    "call_sites",
    "focal_resolution",
    "cross_repo",
    "json_format",
];

/// Render the result of one `tools/call` in plain words, in place.
///
/// `result` is the JSON-RPC result object: `content`, and `isError` when the
/// call failed. Every text block that holds a JSON object is rendered; a
/// block of prose, such as a note appended after the answer, is left alone.
pub fn present(result: &mut Value, tool: &str) {
    present_query(result, tool, true);
}

/// Render the same answer while controlling only optional row-code prose.
/// Trust limits and dispatch qualifications are independent of this choice.
pub(crate) fn present_query(result: &mut Value, tool: &str, details: bool) {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for block in content.iter_mut() {
        let Some(text) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        // A refused write says that nothing was written, why, and the call
        // that fixes it, whether the write path worded the refusal as a code
        // and a sentence, a conflict record or a planner's reason.
        if is_error && crate::write_reply::is_write_tool(tool) {
            if let Some(said) = crate::write_reply::plain_refusal(text, tool) {
                block["text"] = Value::String(crate::verbs::speak_verbs(&said));
                continue;
            }
        }
        let Ok(Value::Object(machine)) = serde_json::from_str::<Value>(text) else {
            // Prose, such as a note after the answer or an error in words:
            // left as written, except that it names each tool the way this
            // profile serves it.
            let spoken = crate::verbs::speak_verbs(text);
            if spoken != text {
                block["text"] = Value::String(spoken);
            }
            continue;
        };
        // Already plain: rendering twice would read its own verdict as missing.
        if machine
            .get(crate::envelope::ENVELOPE_KEY)
            .is_some_and(|envelope| envelope.get("meaning").is_some())
        {
            continue;
        }
        let model_evidence: Vec<_> = ["model_slots", "model_slots_invalid"]
            .into_iter()
            .filter_map(|key| {
                machine
                    .get(crate::envelope::ENVELOPE_KEY)
                    .and_then(|envelope| envelope.get(key))
                    .map(|value| (key, value.clone()))
            })
            .collect();
        // A committed write says what it changed, entity by entity.
        let receipt = (!is_error && crate::write_reply::is_write_tool(tool))
            .then(|| crate::write_reply::plain_receipt(&machine))
            .flatten();
        let mut plain = match receipt {
            Some(receipt) => receipt,
            None if is_error => plain_error(machine),
            None => plain_answer_with_details(machine, tool, details),
        };
        if let Some(envelope) = plain
            .get_mut(crate::envelope::ENVELOPE_KEY)
            .and_then(Value::as_object_mut)
        {
            for (key, value) in model_evidence {
                envelope.insert(key.into(), value);
            }
        }
        speak_served_verbs(&mut plain);
        let rendered = serde_json::to_string(&plain).unwrap_or_default();
        block["text"] = Value::String(rendered);
    }
}

/// Keys whose strings are the repository's own text or an address, or prose
/// this module writes in the served verbs already. Every other string is
/// Kin's own prose, such as a note, a fix, a hint or an error.
const UNSPOKEN_KEYS: [&str; 49] = [
    "model_slots",
    "module",
    "call_code",
    crate::query_reply::MORE_CALL_CODE_KEY,
    "code",
    "uses",
    "signature",
    "name",
    "qualified_name",
    "lookup_name",
    "member_lookup_query",
    "owner",
    "header",
    "body",
    "id",
    "entity_id",
    "change_id",
    "caller",
    "callee",
    "receiver",
    "caller_id",
    "inside",
    "inside_id",
    "class",
    "file",
    "recorded_inventory_issue_files",
    "query",
    "literal",
    "next_cursor",
    "answer_next_cursor",
    "evidence_next_cursor",
    "record_fragment",
    "readings",
    "defined_inside",
    "members",
    "meaning",
    "searched",
    "delivered",
    "verdict",
    "why",
    "possible_note",
    "unadmitted_declarations_note",
    "from",
    "to",
    "commit_message",
    "ticket",
    "revision",
    "section",
    "why_included",
];

/// Name every tool in Kin's own prose the way an agent profile serves it: a
/// hint naming `get_entity_source` sends an agent to a tool its profile does
/// not list. Quoted code and addresses are left as they are.
fn speak_served_verbs(value: &mut Value) {
    match value {
        Value::String(text) => {
            let spoken = crate::verbs::speak_verbs(text);
            if spoken != *text {
                *text = spoken;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(speak_served_verbs),
        Value::Object(fields) => {
            for (key, value) in fields.iter_mut() {
                if !UNSPOKEN_KEYS.contains(&key.as_str()) {
                    speak_served_verbs(value);
                }
            }
        }
        _ => {}
    }
}

/// An error keeps what the tool said, without an envelope or a verdict: a
/// refused request answered nothing, so there is nothing to be complete about.
fn plain_error(mut machine: Map<String, Value>) -> Value {
    machine.remove(crate::envelope::ENVELOPE_KEY);
    for key in WORKING_KEYS {
        machine.remove(key);
    }
    Value::Object(machine)
}

/// One answer, in plain words.
pub fn plain_answer(machine: Map<String, Value>, tool: &str) -> Value {
    plain_answer_with_details(machine, tool, true)
}

fn plain_answer_with_details(mut machine: Map<String, Value>, tool: &str, details: bool) -> Value {
    // Frozen fragments are delivered losslessly, including their existing
    // evidence prose; they cannot be projected into a different record.
    let details = details || machine.contains_key("record_fragment");
    let unadmitted = (!details && tool == "semantic_search")
        .then(|| {
            machine
                .get("edge_coverage")
                .and_then(|coverage| coverage.pointer("/declarations/matching_unadmitted"))
        })
        .flatten()
        .and_then(Value::as_array)
        .cloned();
    let transport: Vec<_> = [
        "record_fragment",
        "readings",
        "next_cursor",
        "answer_next_cursor",
        "evidence_next_cursor",
    ]
    .into_iter()
    .filter_map(|key| {
        machine
            .get(key)
            .filter(|value| !key.ends_with("cursor") || !value.is_null())
            .map(|value| (key, value.clone()))
    })
    .collect();
    let envelope = machine
        .remove(crate::envelope::ENVELOPE_KEY)
        .unwrap_or(Value::Null);
    // An impact page says how many rows each of its lists holds in all,
    // among its readings. With every list whole on this page, the page is
    // the answer and its cursor would serve only what a reader is not shown.
    let impact_whole = tool == "impact_analysis" && {
        // What the answer line reads, wherever the page carries it.
        for key in [
            crate::handlers::review::IMPACT_ROW_TOTALS_KEY,
            crate::handlers::review::IMPACT_TESTS_EXCLUDED_KEY,
            crate::handlers::review::IMPACT_ROW_SCOPE_KEY,
            crate::handlers::review::IMPACT_DEPENDENTS_ASKED_KEY,
            crate::handlers::review::IMPACT_VALUE_USES_WITHHELD_KEY,
            crate::impact_sections::SAME_NAME_NOT_HELD_KEY,
        ] {
            if machine.contains_key(key) {
                continue;
            }
            let held = machine
                .get("readings")
                .and_then(Value::as_array)
                .and_then(|readings| {
                    readings
                        .iter()
                        .find(|reading| reading.get("key").and_then(Value::as_str) == Some(key))
                })
                .and_then(|reading| reading.get("value").cloned());
            if let Some(held) = held {
                machine.insert(key.into(), held);
            }
        }
        machine
            .get(crate::handlers::review::IMPACT_ROW_TOTALS_KEY)
            .is_some_and(|totals| impact_lists_whole(&machine, totals))
    };
    // A callers walk whose reach is its answer, on this page or an earlier
    // one: what the display chain's per-step cap cut is then no part of it.
    let reach_answers = tool == TRACE_TOOL
        && (crate::trace_reach::answer_view(&machine).is_some()
            || envelope
                .pointer("/page/total_answer_records")
                .and_then(Value::as_u64)
                .is_some_and(|records| records > 0));
    // How the answer stands for its question, decided once from the reply.
    let account = crate::answer_account::account(tool, &machine, &envelope, None);
    let mut plain = if let Some(proof) = machine.get("receiver_proof") {
        let mut row = plain_reference_row_with_binding(proof, true);
        for key in ["status", "why"] {
            if let Some(value) = proof.get(key) {
                row[key] = value.clone();
            }
        }
        let recorded = proof["status"] == "recorded" && proof["resolution"] != "name_only";
        let meaning = if !recorded {
            "Answer: Kin cannot show a complete stored receiver chain for this caller. The recorded details and any missing link are below.".to_string()
        } else if account.standing == AnswerStanding::Complete {
            "Answer: this is the stored chain that typed this caller's receiver.".to_string()
        } else {
            format!(
                "Partial: this is the stored chain that typed this caller's receiver, but {}.",
                MachineVerdict::read(&envelope)
                    .open_reasons(Operation::Lookup)
                    .join("; ")
            )
        };
        json!({
            "_kin": plain_envelope_delivered(account.word(), meaning, None, None, None),
            "entity": plain_focal(machine.get("focal_entity").unwrap_or(&Value::Null)),
            "receiver_proof": row,
        })
    } else if tool == STATUS_TOOL {
        plain_status(machine, &envelope)
    } else if is_override_view(&machine, &envelope) {
        plain_override_view(machine, &envelope, &account)
    } else if machine
        .get(crate::handlers::entities::AMBIGUOUS_CANDIDATES_KEY)
        .is_some_and(Value::is_array)
    {
        plain_sectioned_references(machine, &envelope, details, &account)
    } else if is_reference_answer(&machine, tool) {
        plain_references(machine, &envelope, &envelope, details, &account)
    } else if tool == crate::handlers::lexical::TOOL_NAME && machine.contains_key("literal") {
        plain_literal(machine, &envelope, &account)
    } else {
        plain_other(machine, &envelope, tool, details, &account)
    };
    // An answer from a repository other than the folder the client works in
    // says so, in the sentence the connection wrote for it.
    if envelope
        .pointer("/repository/warning")
        .is_some_and(Value::is_string)
    {
        if let Some(kin) = plain
            .get_mut(crate::envelope::ENVELOPE_KEY)
            .and_then(Value::as_object_mut)
        {
            kin.insert("warning".into(), json!("This answer is bound to the selected repository identity, which differs from the client context."));
        }
    }
    let impact_answered = tool == "impact_analysis"
        && impact_is_answered(plain.pointer("/_kin/verdict").and_then(Value::as_str));
    let impact_whole = impact_whole && impact_answered;
    for (key, value) in transport {
        if key == "readings" {
            let value = match (impact_answered, value) {
                (true, Value::Array(mut readings)) => {
                    readings.retain(|reading| {
                        !reading
                            .get("key")
                            .and_then(Value::as_str)
                            .is_some_and(|key| IMPACT_INTERNALS.contains(&key))
                    });
                    Value::Array(readings)
                }
                (_, value) => value,
            };
            match served_readings(value, reach_answers) {
                Some(readings) => plain[key] = readings,
                None => {
                    if let Some(plain) = plain.as_object_mut() {
                        plain.remove(key);
                    }
                }
            }
            continue;
        }
        if impact_whole && key.ends_with("cursor") {
            continue;
        }
        plain[key] = value;
    }
    if let Some(unadmitted) = unadmitted {
        compact_unadmitted_declarations(&mut plain, &unadmitted);
    }
    hold_to_account(tool, &account, &plain);
    plain
}

/// Every plain rendering says the word its account decided, and a word that
/// is complete except something names where that something is listed.
fn hold_to_account(tool: &str, account: &crate::answer_account::Account, plain: &Value) {
    let breaches = crate::answer_account::breaches(account, plain);
    debug_assert!(
        breaches.is_empty(),
        "{tool} reply breaks its account ({:?}, {:?}): {breaches:?}",
        account.standing,
        account.basis,
    );
}

/// Readings of the machine reply an agent profile does not serve on a later
/// page either: the machine envelope, and the copies of the entity asked
/// about that a first page serves as `entity`.
const UNSERVED_READINGS: [&str; 8] = [
    crate::envelope::ENVELOPE_KEY,
    "focal",
    "focal_entity",
    "focal_file",
    "focal_id",
    "focal_kind",
    "focal_name",
    "truncated",
];

/// What the per-step cap cut from a walk's display chain. A callers walk
/// answers with its reach, which is read over the whole closure and never
/// cut by that cap, so beside a reach these count nothing the answer lost.
const CHAIN_CAP_READINGS: [&str; 2] = ["clipped_steps", "spine_clipped_steps"];

/// The readings of a later page as an agent profile serves them, or `None`
/// when none is left.
///
/// A page past the first carries the rest of the frozen answer as readings,
/// one per field of the machine reply. Served as they were, a page of a
/// callers walk printed the machine `negative` and `degradations` blocks,
/// with their advice to pass `target` and to call `graph_neighborhood`,
/// neither of which the agent surface has, and a count of 78 callers the
/// chain's per-step cap had dropped, beside a reach that held every one of
/// them. A reading is served by the rule a first page serves its fields by:
/// the blocks the verdict was computed from ([`WORKING_KEYS`]) are not.
fn served_readings(readings: Value, reach_answers: bool) -> Option<Value> {
    let Value::Array(readings) = readings else {
        return Some(readings);
    };
    let served: Vec<Value> = readings
        .into_iter()
        .filter(|reading| {
            let Some(key) = reading.get("key").and_then(Value::as_str) else {
                return true;
            };
            !(WORKING_KEYS.contains(&key)
                || UNSERVED_READINGS.contains(&key)
                || reach_answers && CHAIN_CAP_READINGS.contains(&key))
        })
        .collect();
    (!served.is_empty()).then_some(Value::Array(served))
}

/// Share only repeated parent metadata of adjacent census rows. This changes
/// neither the conservative match predicate nor the declaration counts. A row
/// without every identity field, or with future extra plain fields, stays as
/// served; no parent is guessed from prose or a filename.
fn compact_unadmitted_declarations(plain: &mut Value, unadmitted: &[Value]) {
    const NOTICE: &str = "Members grouped under an owner can be listed because that owner matches; their names need not match the query.";
    let Some(original) = plain.get("not_held").and_then(Value::as_array) else {
        return;
    };
    // A short reply counts class attributes instead of listing them.
    let unadmitted: Vec<&Value> = unadmitted
        .iter()
        .filter(|row| row.get("reason").and_then(Value::as_str) != Some(CLASS_ATTRIBUTE_REASON))
        .collect();
    if original.len() != unadmitted.len()
        || !plain.pointer("/_kin/meaning").is_some_and(Value::is_string)
    {
        return;
    }
    let mut compacted = Vec::new();
    let mut index = 0;
    while index < original.len() {
        let Some(parent) = unadmitted_group_key(unadmitted[index], &original[index]) else {
            compacted.push(original[index].clone());
            index += 1;
            continue;
        };
        let mut end = index + 1;
        while end < original.len()
            && unadmitted_group_key(unadmitted[end], &original[end]) == Some(parent)
        {
            end += 1;
        }
        if end - index < 2 {
            compacted.push(original[index].clone());
            index = end;
            continue;
        }
        let members: Vec<_> = original[index..end]
            .iter()
            .map(|row| json!({"name":row["name"]}))
            .collect();
        let mut group = json!({"owner":parent.1, "kind":parent.2,
            "why":original[index]["why"], "count":members.len(), "members":members});
        // Rows that share a file and an owner share its module and its
        // enclosing entity, which the group says once.
        for key in ["module", "inside_id"] {
            if let Some(value) = original[index].get(key) {
                group[key] = value.clone();
            }
        }
        if group.to_string().len() < json!(original[index..end]).to_string().len() {
            compacted.push(group);
        } else {
            compacted.extend_from_slice(&original[index..end]);
        }
        index = end;
    }
    // Include the explanation in the comparison because presentation follows
    // budgeting. Small or uncertain groups must never expand the reply.
    if serde_json::to_string(&compacted).unwrap_or_default().len() + NOTICE.len() + 1
        >= serde_json::to_string(original).unwrap_or_default().len()
    {
        return;
    }
    plain["not_held"] = json!(compacted);
    let meaning = plain.pointer_mut("/_kin/meaning").expect("checked meaning");
    *meaning = json!(format!(
        "{} {NOTICE}",
        meaning.as_str().expect("checked string")
    ));
}

fn unadmitted_group_key<'a>(
    raw: &'a Value,
    row: &'a Value,
) -> Option<(&'a str, &'a str, &'a str, &'a str, &'a str)> {
    let row = row.as_object()?;
    if row.keys().any(|key| {
        !["name", "kind", "why", "module", "inside", "inside_id"].contains(&key.as_str())
    }) || !row.get("name").is_some_and(Value::is_string)
    {
        return None;
    }
    // The census and its plain rows must still agree, including each member's
    // exact identity. A future renderer can then safely retain extra fields.
    for field in ["name", "kind"] {
        if row.get(field) != raw.get(field) {
            return None;
        }
    }
    let key = (
        raw.get("file")?.as_str()?,
        raw.get("owner")?.as_str()?,
        raw.get("kind")?.as_str()?,
        raw.get("reason")?.as_str()?,
        row.get("why")?.as_str()?,
    );
    (!key.0.is_empty()
        && !key.1.is_empty()
        && !key.2.is_empty()
        && !key.3.is_empty()
        && !row.get("name")?.as_str()?.is_empty())
    .then_some(key)
}

/// What a callers walk that left test code out says about it, in the words
/// a reference answer uses for the same choice: that the exclusion was asked
/// for, and how many call sites in test code it left out. `None` for a walk
/// that read tests. `complete` is whether the answer is otherwise whole.
pub(crate) fn tests_excluded_sentence(
    reach: &Map<String, Value>,
    complete: bool,
) -> Option<String> {
    let excluded = reach.get(crate::trace_reach::TESTS_EXCLUDED_KEY)?;
    let sites = excluded
        .get("call_sites")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let lead = if complete {
        "Complete for non-test code: test code was excluded, as asked"
    } else {
        "Test code was excluded, as asked; this covers non-test code"
    };
    Some(match sites {
        0 => format!("{lead}."),
        sites => format!(
            "{lead} ({} in test code left out).",
            counted(sites, "call site", "call sites")
        ),
    })
}

impl MachineVerdict {
    /// [`plain_reason`], naming why a language server could not start when
    /// the verdict carries the reason, rather than only that it did not.
    fn plain_reason_named(&self, code: &str) -> Option<String> {
        if code == "reference_enrichment_unusable" {
            if let Some(reason) = &self.server_start_failure {
                return Some(format!("{reason}, so no language server checked its calls"));
            }
        }
        plain_reason(code).map(str::to_string)
    }

    /// Plain reasons the answer is partial for `op`'s question, each once.
    fn open_reasons(&self, op: Operation) -> Vec<String> {
        let mut reasons: Vec<String> = Vec::new();
        let mut unexplained = false;
        for code in self.open_codes(op) {
            let reason = if code == "degraded" {
                Some(degraded_reason(&self.degraded_signals).to_string())
            } else {
                self.plain_reason_named(code).map(|reason| {
                    if reason == UNEXPLAINED {
                        unexplained_for(op).to_string()
                    } else {
                        reason
                    }
                })
            };
            match reason {
                Some(reason) if reason == unexplained_for(op) => unexplained = true,
                Some(reason) if !reasons.contains(&reason) => reasons.push(reason),
                _ => {}
            }
        }
        if unexplained && reasons.is_empty() {
            reasons.push(unexplained_for(op).to_string());
        }
        reasons.truncate(3);
        reasons
    }

    /// The daemon's own degraded signals, said as scope for an answer they do
    /// not make partial, or `None`.
    fn degraded_scope(&self, op: Operation) -> Option<String> {
        if op.analyses_usage()
            || self.degraded_is_delivery()
            || !self.codes.iter().any(|code| code == "degraded")
        {
            return None;
        }
        let reason = degraded_reason(&self.degraded_signals);
        Some(match op {
            Operation::Context | Operation::Neighbors => format!(
                "{}, so links that background work would add may not be listed yet.",
                capitalized(reason)
            ),
            _ => format!(
                "{}; this answer does not depend on it.",
                capitalized(reason)
            ),
        })
    }
}

/// The plain reason for the daemon's degraded signals.
fn degraded_reason(signals: &[String]) -> &'static str {
    if signals.iter().any(|signal| signal == "memory_pressure") {
        "the machine is short of memory, so Kin paused background work such as checking calls"
    } else if signals.iter().any(|signal| signal.starts_with("embed")) {
        "Kin cannot build or save search vectors right now"
    } else if signals
        .iter()
        .any(|signal| signal == "mass_deletion_blocked")
    {
        "Kin held back a change that would delete much of the graph until it is confirmed"
    } else {
        "Kin is running with some checks switched off"
    }
}

/// The reason for a code no plain reason covers, in the terms of `op`'s own
/// question: a history read is never told it could not check every use.
fn unexplained_for(op: Operation) -> &'static str {
    match op {
        Operation::Usage | Operation::Trace | Operation::Other => UNEXPLAINED,
        Operation::NameSearch => "Kin could not confirm it read every declaration",
        Operation::Coordinate => "Kin could not confirm this graph-coordinate lookup",
        Operation::Ranked => "Kin could not confirm what this ranking covered",
        Operation::Source => "Kin could not confirm this copy of the code",
        Operation::History => "Kin could not confirm this history is whole",
        Operation::Context => "Kin could not confirm every part of this context",
        Operation::Neighbors => "Kin could not confirm every link around this entity",
        Operation::Lookup => "Kin could not confirm this lookup against the current code",
    }
}

/// How much of a paged reference answer later pages hold, after what this
/// page delivered: every page of a large answer says what it left for the
/// next, counted, so a page that stopped short to fit is never read as all.
/// Records are numbered with the recorded reference rows first, so the rows
/// still to come are those numbered at or after this page's end.
fn with_records_left(delivered: Option<String>, envelope: &Value) -> Option<String> {
    let page = envelope.get("page");
    let field = |key: &str| page.and_then(|page| page.get(key)).and_then(Value::as_u64);
    let left = (page
        .and_then(|page| page.get("kind"))
        .and_then(Value::as_str)
        == Some("references")
        && page
            .and_then(|page| page.get("has_more"))
            .and_then(Value::as_bool)
            == Some(true))
    .then(|| Some((field("total_records")?, field("next_record")?)))
    .flatten()
    .map(|(total, next)| (total.saturating_sub(next), next))
    .filter(|(left, _)| *left > 0);
    let Some((left, next)) = left else {
        return delivered;
    };
    let rows = field("total_references").map_or(0, |rows| rows.saturating_sub(next));
    let sentence = if rows > 0 {
        format!(
            "{} of this answer {} on later pages, {} of them recorded reference {}.",
            counted(left, "more record", "more records"),
            plural(left, "follows", "follow"),
            rows,
            plural(rows, "row", "rows"),
        )
    } else {
        format!(
            "{} of this answer {} on later pages, none of them recorded reference rows.",
            counted(left, "more record", "more records"),
            plural(left, "follows", "follow"),
        )
    };
    Some(match delivered {
        Some(delivered) => format!("{delivered} {sentence}"),
        None => sentence,
    })
}

/// What a reply delivered or left out to fit, separately from its semantic
/// coverage, and the exact way to retrieve any remaining pages.
fn delivered_sentence(machine: &Map<String, Value>, verdict: &MachineVerdict) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let cursor = verdict.has_more
        || machine
            .get("next_cursor")
            .is_some_and(|cursor| cursor.is_string());
    if verdict.reference_rows_complete.is_some() {
        if verdict.reference_rows_has_more {
            parts.push(
                "More recorded reference rows follow: send answer_next_cursor alone as cursor."
                    .to_string(),
            );
        } else if verdict.reference_evidence_has_more && verdict.reference_sites_delivered {
            parts.push("All recorded reference rows and every unresolved site are shown. The next evidence page holds only the coverage counts behind this verdict and adds no callers.".to_string());
        } else if verdict.reference_evidence_has_more {
            // The study behind this sentence: an invitation to page the
            // evidence drew 56 page calls that added no answer unit. Later
            // pages hold the same kinds this page already lists or counts.
            parts.push("Every recorded reference row is shown, so the answer rows are complete. Later evidence pages hold only more unresolved sites and coverage counts of the kinds left_to_check lists or counts here, never a reference row; evidence_next_cursor pages them.".to_string());
        } else if verdict.reference_rows_complete == Some(true) && !cursor && !verdict.certified() {
            parts.push("No more recorded reference rows or retained evidence pages remain; the verdict still limits what can be concluded.".to_string());
        }
    } else if cursor {
        parts.push("More rows follow: call again with cursor set to next_cursor.".to_string());
    }
    if let Some(withheld) = machine.get("withheld").filter(|value| value.is_object()) {
        if crate::budget::context_withheld_has_ids(withheld) {
            parts.push("withheld lists entity IDs for omitted bodies or rows; read the listed IDs with source. Counts may include other rows whose IDs are not listed.".to_string());
        } else {
            parts.push(
                "withheld records omitted rows without listing their entity IDs.".to_string(),
            );
        }
        if let Some(count) = withheld
            .get("transitive_rows")
            .and_then(Value::as_u64)
            .filter(|count| *count > 0)
        {
            parts.push(format!(
                "{} {} omitted from the transitive context; their IDs are not listed.",
                counted(count, "row", "rows"),
                if count == 1 { "was" } else { "were" }
            ));
        }
        if let Some(count) = machine
            .get("entities_withheld")
            .and_then(Value::as_u64)
            .filter(|count| *count > 0)
        {
            parts.push(format!(
                "entities_withheld separately records {} omitted by the response budget.",
                counted(count, "entity", "entities")
            ));
        }
    } else if !cursor && verdict.reference_rows_complete.is_none() {
        let cut: Vec<String> = [
            ("references_withheld", "row", "rows"),
            ("entities_withheld", "entity", "entities"),
            ("relations_withheld", "relationship", "relationships"),
        ]
        .iter()
        .filter_map(|(key, one, many)| {
            let count = machine.get(*key).and_then(Value::as_u64)?;
            (count > 0).then(|| counted(count, one, many))
        })
        .collect();
        if !cut.is_empty() {
            parts.push(format!(
                "{} did not fit in this reply; ask again with a larger {budget}, up to 60000, to see them.",
                cut.join(" and "), budget = crate::request_schema::OUTPUT_BUDGET
            ));
        } else if verdict.bounded {
            parts.push(format!(
                "Some rows did not fit in this reply; ask again with a larger {}, up to 60000, or a narrower question.",
                crate::request_schema::OUTPUT_BUDGET
            ));
        }
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

const UNEXPLAINED: &str = "Kin could not check every place this could be used";

/// What [`REFERENCE_COMPLETION_UNMEASURED`] leaves unchecked, by name.
const NON_CALL_UNMEASURED: &str = "Kin did not check the places that read the name without \
     calling it (an import, a type hint, a comparison, a decorator, an attribute read), so those \
     places were not matched against the recorded references";

/// What [`REFERENCE_COMPLETION_UNMEASURED`] leaves unchecked when the entity is
/// a module: code imports a module by the path of its file, and an import of
/// some of its declarations is recorded on those declarations.
const MODULE_IMPORTS_UNMEASURED: &str = "it is a module, and code imports a module by the path of \
     its file, which Kin's check of the places that read a name does not follow, while an import \
     of some of its declarations is recorded on those declarations rather than on the module, so \
     modules that import it may be missing here";

/// What [`REFERENCE_COMPLETION_UNMEASURED`] leaves unchecked when the entity
/// holds members: a use of one of them through a value of its type references
/// it without spelling its name.
const MEMBER_READS_UNMEASURED: &str = "it holds members, and a use of one of them through a value \
     of its type is a reference to it that spells no name, recorded only where a language server \
     resolved it, so Kin's check of the places that spell its name cannot show those uses are all \
     here";

/// The plain words for what an unmeasured non-call reading leaves unchecked
/// for this focal: modules are imported by path, a focal with members is
/// referenced through them, anything else by the name Kin did not check.
fn non_call_unmeasured_for(focal: &Value, machine: &Map<String, Value>) -> String {
    if focal_is_module(focal) {
        return MODULE_IMPORTS_UNMEASURED.to_string();
    }
    let census = machine
        .get(crate::handlers::entities::CALL_SITE_CANDIDATES_KEY)
        .and_then(|candidates| {
            candidates
                .pointer("/value_calls/census")
                .or_else(|| candidates.get("census"))
        });
    let mentions = census.and_then(|census| census.pointer("/residue/mentions"));
    if mentions
        .and_then(|mentions| mentions.get("measured"))
        .and_then(Value::as_bool)
        == Some(true)
        && mentions.is_some_and(|mentions| mentions.get("references_unmeasured").is_some())
    {
        return MEMBER_READS_UNMEASURED.to_string();
    }
    match mentions
        .and_then(|mentions| mentions.get("reason"))
        .and_then(Value::as_str)
    {
        Some(kin_db::call_site_escape::MENTIONS_UNRECORDED) => {
            UNPARSED_READS_UNMEASURED.to_string()
        }
        Some(kin_db::call_site_escape::MENTIONS_UNLISTED) => STALE_RECORDS_UNMEASURED.to_string(),
        Some(kin_db::call_site_escape::MENTIONS_NOT_READ_FOR_LANGUAGE) => {
            LANGUAGE_READS_UNMEASURED.to_string()
        }
        _ => NON_CALL_UNMEASURED.to_string(),
    }
}

/// What [`REFERENCE_COMPLETION_UNMEASURED`] leaves unchecked when code that
/// could read the name did not parse: those places alone.
const UNPARSED_READS_UNMEASURED: &str = "some code that spells the name did not parse, so the \
     places there that read it without calling it could not be matched against the recorded \
     references; left_to_check lists where";

/// What [`REFERENCE_COMPLETION_UNMEASURED`] leaves unchecked when the
/// repository's records predate the check of non-call reads.
const STALE_RECORDS_UNMEASURED: &str = "this repository's records predate Kin's check of the \
     places that read a name without calling it, so those places were not matched against the \
     recorded references until `kin upgrade` re-derives them";

/// What [`REFERENCE_COMPLETION_UNMEASURED`] leaves unchecked when the code's
/// language records no non-call reads: those places, which no `kin upgrade`
/// adds.
const LANGUAGE_READS_UNMEASURED: &str = "Kin does not yet record, in this code's language, \
     the places that read a name without calling it, so those places were not matched against \
     the recorded references";

/// Whether a reference answer's focal is a module, which code imports by path.
fn focal_is_module(focal: &Value) -> bool {
    focal
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|kind| {
            ["module", "file", "package"]
                .iter()
                .any(|module| kind.eq_ignore_ascii_case(module))
        })
}

const CUT_TO_FIT: &str = "the reply was cut to fit its size limit, so some rows are left out";

const UNACCOUNTED_CALLERS: &str = "some possible callers could not be accounted for";

/// The plain reason for one verdict code, or `None` for a code whose sites the
/// reply lists instead.
fn plain_reason(code: &str) -> Option<&'static str> {
    if crate::verdict::is_residue_code(code) {
        return None;
    }
    Some(match code {
        "response_bounded" | "answer_truncated" | "counts_are_a_floor" => CUT_TO_FIT,
        "retrieval_degraded" => "parts of the answer were left out or cut short",
        "method_call_resolution_incomplete" | "call_sites_unresolved" => {
            "some calls in this code could not be matched to what they call"
        }
        "binding_unproven" => "some calls go through a variable whose value Kin could not pin down",
        crate::answer_account::UNDECIDED_SITES => UNDECIDED_SITES_REASON,
        "constructor_dispatch_unlisted" => {
            "a constructor also runs when code constructs a subclass that defines no constructor of its own, or constructs through cls, type(self) or self.__class__, and those constructions do not spell its name, so check the subclasses and those constructions"
        }
        "call_sites_owed" | "enrichment_incomplete" | "enrichment_metadata_unavailable" => {
            "Kin has not finished checking some calls yet, so asking again later may find more"
        }
        "call_sites_server_failed" => "the language server failed on some calls",
        "call_sites_unproven_no_resolver" | "call_sites_not_in_build" => {
            "no language server could check some calls"
        }
        "proof_context_stale" | "proof_context_unverified" => {
            "some calls were checked against an older build setup"
        }
        "caller_arrival_unresolved" => UNACCOUNTED_CALLERS,
        "caller_arrival_unmeasured" => {
            "Kin could not establish every entity that can reach this entity"
        }
        "caller_arrival_state_unknown" => {
            "Kin could not interpret the recorded coverage of possible callers"
        }
        "derived_source_stale"
        | "derived_source_unproven"
        | "file_bytes_unadmitted"
        | "file_bytes_unchecked"
        | "file_spans_stale"
        | "body_unverified" => {
            "Kin could not confirm the current source identity of some recorded code"
        }
        "withheld_candidates" => {
            "some possible matches were left out because they match by name only"
        }
        "depth_zero" => "the walk did not follow any links",
        "trace_walk_truncated" => {
            "the walk hit a per-step or total cap before it examined every step"
        }
        "trace_walk_degraded" => "the walk stopped at its own work bounds before it finished",
        "focal_not_in_graph" => "Kin does not hold the entity that was asked about",
        "degraded" => "Kin is running with some checks switched off",
        "store_semantics_behind" => "an older version of Kin built this repository's records",
        "reference_enrichment_unknown" => {
            "Kin could not tell whether a language server checked these calls"
        }
        "reference_enrichment_unusable" => {
            "the language server for this code failed to start, so no language server checked its calls"
        }
        "local_binding_unproven" => "Kin could not confirm what some local names point to",
        "local_binding_outstanding" => {
            "Kin has not resolved what some local names in this code point to"
        }
        // The census of calls and of values passed or stored is measured; what
        // this names is the rest of "every use", concretely.
        REFERENCE_COMPLETION_UNMEASURED => NON_CALL_UNMEASURED,
        code if code.starts_with("graph_") || code.starts_with("tracked_changes") => {
            "the code has changed since Kin last read it"
        }
        "coverage_role_filter_withheld" => {
            "test code was left out of the ranking; ask again with include_tests to rank it"
        }
        "coverage_graph_body_gap" => "some matches have no code body, so they were ranked on their names alone",
        "coverage_absent" => "no part of this repository has search vectors yet, so nothing was ranked by meaning",
        "relevance_floor_unmeasured" => {
            "none of these matches is a confident answer to the question; they are the nearest Kin found"
        }
        code if code.starts_with("coverage_") => "the search index is not complete yet",
        code if code.starts_with("file_parse") || code.starts_with("file_not_parsed") => {
            "Kin could not fully derive some recorded code"
        }
        code if code.starts_with("declaration") || code.starts_with("entity_index") => {
            "Kin could not confirm it read every declaration"
        }
        code if code.starts_with("cross_repo") => "other repositories were not searched",
        code if code.starts_with("dependency_") => {
            "the answer may be in a library this repository uses"
        }
        _ => UNEXPLAINED,
    })
}

/// The semantic scope of a lookup; physical inventory totals are not entity counts.
fn where_checked() -> &'static str {
    "the code it holds"
}

fn searched_line(envelope: &Value) -> Option<String> {
    (envelope
        .pointer("/source_derivation/checked_scope")
        .and_then(Value::as_str)
        == Some("admitted_inventory"))
    .then(|| {
        "Checked the admitted semantic inventory; coverage is limited by the verdict.".to_string()
    })
}

fn searched_or_unmeasured(envelope: &Value) -> String {
    searched_line(envelope).unwrap_or_else(||
        "Searched recorded entity and relationship evidence; coverage is limited by the verdict.".to_string())
}

/// Tools whose answer is a set of rows a reader acts on, which always say what
/// they searched.
fn is_answer_tool(tool: &str) -> bool {
    matches!(tool, "find_references" | TRACE_TOOL | "impact_analysis")
}

/// The plain `_kin` block. Each part answers one question, so no word stands
/// for two promises: `verdict` is whether the answer suffices for the
/// question, `meaning` states the answer, `delivered` says what this reply
/// left out to fit and the exact way to get it, `searched` what was searched
/// and what no reader of code can follow, and `fix` the one step that would
/// make a partial answer whole.
fn plain_envelope(
    verdict: Option<&str>,
    meaning: String,
    searched: Option<String>,
    fix: Option<&str>,
) -> Value {
    plain_envelope_delivered(verdict, meaning, None, searched, fix)
}

fn plain_envelope_delivered(
    verdict: Option<&str>,
    meaning: String,
    delivered: Option<String>,
    searched: Option<String>,
    fix: Option<&str>,
) -> Value {
    let mut envelope = Map::new();
    if let Some(verdict) = verdict {
        envelope.insert("verdict".into(), json!(verdict));
    }
    envelope.insert("meaning".into(), json!(meaning));
    if let Some(delivered) = delivered {
        envelope.insert("delivered".into(), json!(delivered));
    }
    // What makes the answer complete, when one step would: the verdict's own
    // remedy, which already names the command in plain words.
    if let Some(fix) = fix {
        envelope.insert("fix".into(), json!(fix));
    }
    if let Some(searched) = searched {
        envelope.insert("searched".into(), json!(searched));
    }
    Value::Object(envelope)
}

/// The one entity shape every plain reply serves: `entity_id`, `name`, `kind`,
/// `qualified_name` and the dotted `module` when available, read from the
/// stored semantic identity. An
/// entity's id is always `entity_id`, never `id`, so it is never mistaken for
/// the id of the file artifact it sits in, a change or a step.
fn insert_identity(plain: &mut Map<String, Value>, row: &Value) {
    let pick = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| row.get(*key).filter(|value| !value.is_null()))
            .cloned()
    };
    if let Some(id) = pick(&["entity_id", "id"]) {
        plain.insert("entity_id".into(), id);
    }
    if let Some(name) = pick(&["name", "entity_name"]) {
        plain.insert("name".into(), name);
    }
    if let Some(kind) = pick(&["kind", "entity_kind"]) {
        plain.insert("kind".into(), kind);
    }
    if let Some(qualified) = pick(&["qualified_name"]) {
        plain.insert("qualified_name".into(), qualified);
    }
    for key in [
        "module",
        "semantic_owners",
        "semantic_owner_gap",
        "semantic_owners_omitted",
        "semantic_owner_candidate_count",
    ] {
        if let Some(value) = row.get(key) {
            plain.insert(key.into(), value.clone());
        }
    }
    // A foreign UUID must stay paired with its repository authority and must
    // never be promoted into a local entity_id by presentation.
    if let (Some(repo), Some(entity)) = (pick(&["repo_id"]), pick(&["foreign_entity_id"])) {
        plain.insert("repo_id".into(), repo);
        plain.insert("foreign_entity_id".into(), entity);
    }
}

// ---------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------

/// The verb for a reference answer: `call` when only calls were asked for,
/// or when every row calls the entity.
fn reference_verb(machine: &Map<String, Value>, rows: usize) -> &'static str {
    let asked_calls_only = machine
        .get("relation_kinds")
        .and_then(Value::as_array)
        .is_some_and(|kinds| kinds.len() == 1 && kinds[0] == "calls");
    let every_row_calls = machine
        .get("references")
        .and_then(Value::as_array)
        .is_some_and(|rows| {
            !rows.is_empty()
                && rows.iter().all(|row| {
                    row.get("relation_kinds")
                        .and_then(Value::as_array)
                        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "calls"))
                })
        });
    let calls_only = asked_calls_only || every_row_calls;
    match (calls_only, rows == 1) {
        (true, true) => "calls",
        (true, false) => "call",
        (false, true) => "uses",
        (false, false) => "use",
    }
}

/// Whether a reference row is a module that imports the entity and makes no
/// call to it: the module's own top-level code brings the name in, by an
/// import or a re-export, and nothing in the row runs it. A module whose
/// top-level code calls the entity uses it: it stays among the rows a
/// reference account settles sites by, and a reply lists it apart under
/// [`MODULE_CODE_USERS_KEY`] (see [`is_module_code_row`]).
pub(crate) fn is_module_level_import(row: &Value) -> bool {
    let kinds = |kind: &str| {
        row.get("relation_kinds")
            .and_then(Value::as_array)
            .is_some_and(|kinds| kinds.iter().any(|held| held == kind))
    };
    row.get("kind")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("module"))
        && kinds("imports")
        && !kinds("calls")
}

/// Whether a reference row is a module whose own top-level code is proven
/// to use the entity. A module row matched by the name alone is no proven
/// use: it stays among the name-matched rows, where the answer's account
/// counts it, so the reply never reads it as a real use.
pub(crate) fn is_module_code_row(row: &Value) -> bool {
    row.get("kind")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("module"))
        && !crate::answer_account::is_name_only_row(row)
}

/// `functions` when every row is a function or method, `entities` otherwise.
fn reference_noun(rows: &[Value]) -> (&'static str, &'static str) {
    let functions = !rows.is_empty()
        && rows.iter().all(|row| {
            row.get("kind").and_then(Value::as_str).is_some_and(|kind| {
                let kind = kind.to_ascii_lowercase();
                kind == "function" || kind == "method"
            })
        });
    if functions {
        ("function", "functions")
    } else {
        ("entity", "entities")
    }
}

fn plain_reference_row(row: &Value) -> Value {
    plain_reference_row_with_binding(row, false)
}

fn plain_reference_row_with_binding(row: &Value, include_binding: bool) -> Value {
    let mut plain = Map::new();
    insert_identity(&mut plain, row);
    if let Some(kinds) = row.get("relation_kinds") {
        plain.insert("how".into(), kinds.clone());
    }
    if let Some(role) = row.get("role") {
        plain.insert("role".into(), role.clone());
    }
    match row.get("resolution").and_then(Value::as_str) {
        Some("name_only") | Some(crate::handlers::entities::NAME_UNIQUE_RESOLUTION) => {
            plain.insert("proof".into(), json!("matched by name only"));
        }
        // A proven row says how, so a reader does not re-derive the receiver
        // from the caller's body to trust it.
        Some("type_resolved") => {
            plain.insert("proof".into(), json!("target resolved by type"));
        }
        Some("import_scoped") => {
            plain.insert("proof".into(), json!("target resolved through imports"));
        }
        _ => {}
    }
    if let Some(via) = row.get("via_override_of").filter(|value| !value.is_null()) {
        plain.insert("through_override_of".into(), via.clone());
    }
    if include_binding {
        for key in [
            crate::receiver_binding::RECEIVER_BINDING_KEY,
            crate::receiver_binding::RECEIVER_BINDING_MISSING_KEY,
        ] {
            if let Some(value) = row.get(key).filter(|value| !value.is_null()) {
                plain.insert(key.into(), value.clone());
            }
        }
    } else if row
        .get(crate::receiver_binding::RECEIVER_BINDING_KEY)
        .and_then(Value::as_array)
        .is_some_and(|steps| !steps.is_empty())
    {
        plain.insert("receiver_binding_available".into(), json!(true));
    }
    if let Some(sites) = row.get("sites").and_then(Value::as_array) {
        let sites: Vec<Value> = sites.iter().map(plain_site).collect();
        if !sites.is_empty() {
            plain.insert("sites".into(), json!(sites));
        }
    }
    Value::Object(plain)
}

/// One site in a reference row: its line in the caller and its code.
fn plain_site(site: &Value) -> Value {
    let mut plain = Map::new();
    if let Some(line) = site.get("line_in_entity") {
        plain.insert("line_in_caller".into(), line.clone());
    }
    if let Some(code) = site.get("expr") {
        plain.insert("code".into(), code.clone());
    } else if let Some(callee) = site.get("callee") {
        plain.insert("calls".into(), callee.clone());
    }
    Value::Object(plain)
}

/// The scope statement for code that looks a name up while the program runs,
/// by a name no record follows and that spells none of the entity's names.
/// Such a lookup could reach anything, so it is no evidence about this entity,
/// and every tool that reads code without running it shares it as the edge of
/// what it covers. The statement says where the analysis ends; it never says
/// those places were checked, and it is not something to go and check.
fn unkeyed_sentence(count: u64, name: &str) -> String {
    const LEAD: &str = "Code that builds a name while it runs is outside what any reader of \
         code can follow";
    match count {
        0 => format!("{LEAD}, and none of those places here spells {name}."),
        1 => format!("{LEAD}; the 1 such place here does not spell {name}."),
        _ => format!("{LEAD}; none of the {count} such places here spells {name}."),
    }
}

/// `searched`, with the scope statement for run-time lookups after it.
fn with_scope(searched: Option<String>, scope: Option<String>) -> Option<String> {
    match (searched, scope) {
        (Some(searched), Some(scope)) => Some(format!("{searched} {scope}")),
        (searched, scope) => searched.or(scope),
    }
}

/// The sentence for local-name bindings an answer folded: how many, and the
/// one way any of them could matter. Counted beside the answer, never an
/// absence and never a place to go and check.
fn folded_bindings_sentence(verdict: &MachineVerdict, envelope: &Value) -> Option<String> {
    if !verdict
        .codes
        .iter()
        .any(|code| code == crate::verdict::LOCAL_BINDING_UNRELATED)
    {
        return None;
    }
    let count = unrelated_bindings(envelope);
    Some(match count {
        Some(1) => "1 local name elsewhere in this code is not resolved; it sits in no entity this answer lists and spells none of their names, so it could matter only if it now names one of them under another name.".to_string(),
        Some(count) => format!("{count} local names elsewhere in this code are not resolved; none sits in an entity this answer lists or spells one of their names, so one could matter only if it now names one of them under another name."),
        None => "Some local names elsewhere in this code are not resolved; none sits in an entity this answer lists or spells one of their names, so one could matter only if it now names one of them under another name.".to_string(),
    })
}

/// Where a reference answer about a module lists what the module itself
/// imports.
pub(crate) const MODULE_IMPORTS_PLAIN_KEY: &str = "imports";

/// A module's own imports, plainly: one row per name its import statements
/// bind, with the module it comes from, then how many names the module binds
/// in all and how many of the listed ones come from outside the repository.
fn plain_module_imports(block: &Value) -> Option<(Vec<Value>, u64, u64)> {
    let names = block.get("names")?.as_array()?;
    if names.is_empty() {
        return None;
    }
    let mut outside = 0u64;
    let rows: Vec<Value> = names
        .iter()
        .map(|import| {
            let mut row = Map::new();
            for (key, served) in [
                ("name", "name"),
                ("module", "from"),
                ("imported", "imported"),
            ] {
                if let Some(value) = import.get(key).filter(|value| !value.is_null()) {
                    row.insert(served.into(), value.clone());
                }
            }
            if import.get("outside_repository").and_then(Value::as_bool) == Some(true) {
                outside += 1;
                row.insert(OUTSIDE_REPOSITORY_KEY.into(), json!(true));
            }
            Value::Object(row)
        })
        .collect();
    let total = block
        .get("names_total")
        .and_then(Value::as_u64)
        .unwrap_or(rows.len() as u64);
    Some((rows, total, outside))
}

/// The row field marking a module no file of the repository supplies.
const OUTSIDE_REPOSITORY_KEY: &str = "outside_repository";

/// The plain `overridden_by`: the methods that override the focal, and the
/// classes that could but that Kin does not track on their own.
fn plain_overrides(block: &Value) -> Option<Value> {
    block.as_object()?;
    let overriders: Vec<Value> = block
        .get("overriders")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    let mut plain = Map::new();
                    insert_identity(&mut plain, row);
                    for key in ["resolution", "proven", "overrides", "relation"] {
                        if let Some(value) = row.get(key) {
                            plain.insert(key.into(), value.clone());
                        }
                    }
                    Value::Object(plain)
                })
                .collect()
        })
        .unwrap_or_default();
    // A class inside a function or class body is read through the entity
    // around it, so it is a place to check. One in a block at the top level
    // of its file has no entity a reader can read, so it is said plainly
    // rather than listed as something to look up.
    let mut possible: Vec<Value> = Vec::new();
    let mut in_file_blocks: Vec<Value> = Vec::new();
    for row in block
        .get("not_admitted")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let mut plain = Map::new();
        plain.insert("class".into(), row["name"].clone());
        if row.get("placement").and_then(Value::as_str)
            == Some(crate::handlers::overrides::FILE_LEVEL_BLOCK)
        {
            plain.insert("inspection".into(), json!("unavailable_semantic_container"));
            in_file_blocks.push(Value::Object(plain));
        } else {
            plain.insert("defined_inside".into(), row["enclosing"].clone());
            if let Some(id) = row.get("enclosing_id").filter(|id| !id.is_null()) {
                plain.insert("inside_id".into(), id.clone());
            }
            possible.push(Value::Object(plain));
        }
    }
    // The callers of an override change with this method's signature as its
    // own callers do, and each names the override it calls.
    let callers_of = |key: &str| -> Vec<Value> {
        block
            .get(key)
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .map(|row| {
                        let mut plain = Map::new();
                        insert_identity(&mut plain, row);
                        for key in [
                            "calls_override",
                            "calls_override_id",
                            "also_calls_overrides",
                            "resolution",
                            "proven",
                        ] {
                            if let Some(value) = row.get(key) {
                                plain.insert(key.into(), value.clone());
                            }
                        }
                        Value::Object(plain)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let override_callers = callers_of(crate::handlers::overrides::OVERRIDE_CALLERS_KEY);
    let override_caller_candidates =
        callers_of(crate::handlers::overrides::OVERRIDE_CALLER_CANDIDATES_KEY);
    let callers_read = block
        .get(crate::handlers::overrides::OVERRIDE_CALLERS_KEY)
        .is_some();
    let mut plain = Map::new();
    plain.insert(
        "note".into(),
        json!(if callers_read {
            "Recorded overrides and implementations, not callers of this method. A signature change changes each of them, and each caller in override_callers, which calls the override named in calls_override. override_caller_candidates were matched by name only: check each, they are not proof. Each row retains its resolution."
        } else {
            "Recorded overrides and implementations, not callers. Each row retains its resolution."
        }),
    );
    plain.insert("methods".into(), json!(overriders));
    if callers_read {
        plain.insert(
            crate::handlers::overrides::OVERRIDE_CALLERS_KEY.into(),
            json!(override_callers),
        );
    }
    if !override_caller_candidates.is_empty() {
        plain.insert(
            crate::handlers::overrides::OVERRIDE_CALLER_CANDIDATES_KEY.into(),
            json!(override_caller_candidates),
        );
    }
    for key in ["count", "proven", "bounded", "nested_definitions"] {
        if let Some(value) = block.get(key) {
            plain.insert(key.into(), value.clone());
        }
    }
    let OverrideCoverage { coverage, gaps } = OverrideCoverage::read(block);
    plain.insert("coverage".into(), coverage);
    for key in [
        "unlisted_same_name_methods",
        "classes_spelling_base_without_edge",
    ] {
        if let Some(rows) = block.get(key).filter(|rows| !rows.is_null()) {
            plain.insert(key.into(), rows.clone());
        }
    }
    if !possible.is_empty() {
        plain.insert("possible".into(), json!(possible));
        plain.insert(
            "possible_note".into(),
            json!("Unadmitted subclass candidates, not proven overrides. Inspect source of inside_id only if its body is available."),
        );
    }
    if !in_file_blocks.is_empty() {
        plain.insert("unadmitted_declarations".into(), json!(in_file_blocks));
        plain.insert(
            "unadmitted_declarations_note".into(),
            json!("Unadmitted subclasses: Kin cannot show them as entities. Their methods and runtime activation are unmeasured."),
        );
    }
    if !gaps.is_empty() {
        plain.insert("gaps".into(), json!(gaps));
    }
    Some(Value::Object(plain))
}

/// Focused override pages carry their own coverage. A reference page's row
/// completion cannot certify this relation family or repository-wide absence.
/// Whether a reply is the override view of a reference answer.
pub(crate) fn is_override_view(machine: &Map<String, Value>, envelope: &Value) -> bool {
    machine.get("view").and_then(Value::as_str) == Some("overrides")
        || envelope
            .pointer("/reference_view/view")
            .and_then(Value::as_str)
            == Some("overrides")
}

/// The override block an override view shows: what this page carries over
/// what the answer's first page repeated.
fn override_view_block(machine: &Map<String, Value>, envelope: &Value) -> Value {
    let repeated = envelope.get("reference_view").unwrap_or(&Value::Null);
    let mut block = repeated
        .get("overridden_by")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(current) = machine.get("overridden_by").and_then(Value::as_object) {
        block.extend(current.clone());
    }
    plain_overrides(&Value::Object(block)).expect("override block object")
}

/// Whether the census behind an override view accounted for every recorded
/// subclass and class header, nothing else limits the block, and this reply
/// delivered the whole of it.
pub(crate) fn override_census_complete(machine: &Map<String, Value>, envelope: &Value) -> bool {
    OverridePageAccount::read(&override_view_block(machine, envelope), machine, envelope)
        .census_complete
}

/// Whether a reply is a reference answer about one focal.
pub(crate) fn is_reference_answer(machine: &Map<String, Value>, tool: &str) -> bool {
    machine.get("references").is_some_and(Value::is_array)
        && (machine.contains_key("focal_entity") || tool == "find_references")
}

/// Whether a reply is a read of one entity's code.
pub(crate) fn is_source_reply(machine: &Map<String, Value>) -> bool {
    machine.contains_key("body")
        || machine.contains_key("header")
        || crate::artifact_entity::is_artifact_reply(machine)
        || machine.get("body_omitted").and_then(Value::as_str)
            == Some(crate::container_shape::CONTAINER_SHAPE_REASON)
}

/// Whether a page of a callers walk lies past every reach row, holding only
/// display chain and evidence.
pub(crate) fn is_display_page(machine: &Map<String, Value>, envelope: &Value) -> bool {
    trace_page(machine, envelope) == Some(TracePage::Display)
}

fn plain_override_view(
    machine: Map<String, Value>,
    envelope: &Value,
    account: &crate::answer_account::Account,
) -> Value {
    let repeated = envelope.get("reference_view").unwrap_or(&Value::Null);
    let block = override_view_block(&machine, envelope);
    let OverridePageAccount {
        count,
        has_more,
        census_complete,
        checked,
        incomplete,
    } = OverridePageAccount::read(&block, &machine, envelope);
    let delivered = if has_more {
        "More override rows or coverage details remain; send next_cursor alone as cursor."
    } else {
        "This response finishes delivery of the recorded answer; coverage limits still apply."
    };
    let meaning = if census_complete && account.standing == AnswerStanding::Complete {
        format!(
            "Answer: these {count} methods override or implement it. Kin checked all {checked} recorded subclasses and implementers, and every recorded class's declared bases, and found no other; you can answer from this."
        )
    } else if census_complete {
        format!(
            "Partial: these {count} methods override or implement it as Kin recorded them, but {}.",
            MachineVerdict::read(envelope)
                .open_reasons(Operation::Lookup)
                .join("; ")
        )
    } else if incomplete {
        format!("Answer: {count} recorded override or implementation rows on this page. The places listed under unlisted_same_name_methods or classes_spelling_base_without_edge could also override it; check only those.")
    } else {
        format!("Answer: {count} recorded override or implementation rows on this page. Repository-wide override completeness is not measured.")
    };
    let mut plain = json!({
        "view": "overrides",
        "_kin": plain_envelope_delivered(
            account.word(),
            meaning,
            Some(delivered.into()),
            Some("Incoming Overrides and Implements edges in the recorded graph, transitively; callers were not queried.".into()), None,
        ),
        "entity": plain_focal(machine.get("focal_entity").or_else(|| repeated.get("focal_entity")).unwrap_or(&Value::Null)),
        "overridden_by": block,
    });
    // Keep lossless transport records and fragments available for callers who
    // requested more detail. Never rename a reference completion flag into an
    // override coverage claim.
    for key in [
        "next_cursor",
        "answer_next_cursor",
        "evidence_next_cursor",
        "record_fragment",
        "readings",
    ] {
        if let Some(value) = machine.get(key) {
            plain[key] = value.clone();
        }
    }
    plain
}

fn plain_focal(focal: &Value) -> Value {
    let mut plain = Map::new();
    insert_identity(&mut plain, focal);
    if let Some(signature) = focal.get("signature") {
        plain.insert("signature".into(), signature.clone());
    }
    Value::Object(plain)
}

/// How many rows carry no quoted code at any of their sites.
fn rows_without_code(rows: &[Value]) -> usize {
    rows.iter()
        .filter(|row| {
            !row.get("sites")
                .and_then(Value::as_array)
                .is_some_and(|sites| {
                    sites.iter().any(|site| {
                        site.get("expr")
                            .and_then(Value::as_str)
                            .is_some_and(|expr| !expr.trim().is_empty())
                    })
                })
        })
        .count()
}

/// The sentence saying what proves each row: the line of code at its site.
/// A row without quoted code is counted, with the one call that reads it, so
/// the sentence never claims a line it does not show.
fn evidence_sentence(rows: &[Value], verb: &str, details: bool) -> String {
    let foreign = |row: &Value| {
        row.get("repo_id").is_some_and(Value::is_string)
            && row.get("foreign_entity_id").is_some_and(Value::is_string)
            && row.get("entity_id").is_none_or(Value::is_null)
    };
    if rows.iter().any(foreign) {
        let local: Vec<Value> = rows.iter().filter(|row| !foreign(row)).cloned().collect();
        let mut text = if local.is_empty() {
            String::new()
        } else {
            evidence_sentence(&local, verb, details)
        };
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str("Federated callers are identified by repo_id and foreign_entity_id; these rows have no local source handle.");
        return text;
    }
    if rows.iter().any(is_override_dispatch) {
        let mut text = "A through_override_of row proves a call to the named base method, which may dispatch to this override.".to_string();
        if !details {
            return text;
        }
        if rows_without_code(rows) == 0 {
            text.insert_str(0, "Each row shows the code at its site. ");
        } else {
            text.push_str(" Read a row with source and its entity_id when its code is not quoted.");
        }
        return text;
    }
    if !details {
        return String::new();
    }
    let does = if verb.starts_with("call") {
        "calls"
    } else {
        "uses"
    };
    let bare = rows_without_code(rows);
    if bare == 0 {
        return format!("Each row shows the line of code that {does} it.");
    }
    if bare == rows.len() {
        return format!(
            "The rows do not quote the code that {does} it; read a caller with source and its entity_id."
        );
    }
    format!(
        "Each row shows the line of code that {does} it, except {bare} {} whose code is not quoted; read one with source and its entity_id.",
        plural(bare as u64, "row", "rows")
    )
}

/// What a reference answer checked, as a phrase: every call, or every
/// relation of the kinds asked for, that spells the entity's name. With
/// `calls_only`, the kinds the reply measured are calls alone, whatever was
/// asked, and the phrase says only what was measured.
fn uses_checked(machine: &Map<String, Value>, short_name: &str, calls_only: bool) -> String {
    let kinds: Vec<&str> = machine
        .get("relation_kinds")
        .and_then(Value::as_array)
        .map(|kinds| {
            kinds
                .iter()
                .filter_map(Value::as_str)
                .filter(|kind| !calls_only || *kind == "calls")
                .filter_map(|kind| match kind {
                    "calls" => Some("call"),
                    "imports" => Some("import"),
                    "references" => Some("reference"),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let listed = match kinds.as_slice() {
        [] => return format!("references to {short_name} within the recorded query scope"),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    format!("every {listed} that spells {short_name}")
}

/// A resolved call to a base method can run an override, but the base call
/// alone does not prove the receiver will dispatch to this override.
fn is_override_dispatch(row: &Value) -> bool {
    row.get("via_override_of").is_some_and(|via| !via.is_null())
}

/// Name actual unaccounted files only when the answer recorded them. An
/// unmeasured family is a capability gap, not evidence that such files exist.
/// The plain reason for reference sites a sweep could not decide, uncounted.
const UNDECIDED_SITES_REASON: &str = "some reference sites stayed undecided after repeated sweeps of unchanged code, so usages there may be missing";

fn reference_open_reasons(machine: &Map<String, Value>, verdict: &MachineVerdict) -> Vec<String> {
    let mut reasons = verdict.open_reasons(Operation::Usage);
    // Undecided sites are said with their count where the answer gives it.
    if let Some(count) = Value::Object(machine.clone())
        .pointer(crate::answer_account::UNDECIDED_SITES_POINTER)
        .and_then(|undecided| undecided.get("count"))
        .and_then(Value::as_u64)
        .filter(|count| *count > 0)
    {
        for reason in &mut reasons {
            if reason == UNDECIDED_SITES_REASON {
                *reason = format!(
                    "{} stayed undecided after repeated sweeps of unchanged code, so usages there may be missing",
                    counted(count, "reference site", "reference sites")
                );
            }
        }
    }
    let arrival = machine.get(crate::caller_arrival::CALLER_ARRIVAL_KEY);
    // An unaccounted file can have no parse-side count at all. That is still
    // a gap, but does not prove it holds unmatched calls. Only positive
    // recorded shortfalls support the factual sentence below; capped or
    // unmeasured rows are not extrapolated to the whole family.
    let files = arrival
        .and_then(|arrival| arrival.get("unaccounted_files"))
        .and_then(Value::as_array)
        .map_or(0, |files| {
            files
                .iter()
                .filter(|file| {
                    file.get("unaccounted_call_sites")
                        .and_then(Value::as_u64)
                        .is_some_and(|sites| sites > 0)
                })
                .count() as u64
        });
    if arrival
        .and_then(|arrival| arrival.get("state"))
        .and_then(Value::as_str)
        == Some("unaccounted")
        && files > 0
    {
        for reason in &mut reasons {
            if reason == UNACCOUNTED_CALLERS {
                *reason = "Recorded call sites that could reach this entity remain unaccounted for"
                    .to_string();
            }
        }
    }
    reasons
}

/// Expose the recorded binding blocker without mistaking repository binding
/// obligations for missing callers of the focal.
/// The explicit detail reply retains its established rendering.
pub(crate) fn reference_binding_gap(envelope: &Value) -> Option<Value> {
    reference_binding_gap_for_verdict(envelope, &MachineVerdict::read(envelope))
}

fn reference_binding_gap_for_verdict(envelope: &Value, verdict: &MachineVerdict) -> Option<Value> {
    // Bindings the answer showed unrelated to it are counted, with the one
    // way they could matter, and never read as an open gap or an absence.
    if verdict
        .codes
        .iter()
        .any(|code| code == crate::verdict::LOCAL_BINDING_UNRELATED)
    {
        let mut gap = json!({
            "kind": "local_bindings",
            "folded": true,
            "why": "Local names elsewhere in this code that Kin has not resolved. None sits in an entity this answer lists or spells one of their names, so they are counted, not part of this answer's uncertainty.",
            "unit": "outstanding binding obligations (not a caller count)",
        });
        if let Some(count) = unrelated_bindings(envelope) {
            gap["count"] = json!(count);
        }
        return Some(gap);
    }
    if !verdict
        .codes
        .iter()
        .any(|code| code == "local_binding_outstanding")
    {
        return None;
    }
    let derivation = envelope.get("source_derivation")?;
    // The binding-scope copy is disclosure only. Use the same validated
    // report reading as the verdict, which can widen an inconsistent split.
    let observation =
        serde_json::from_value::<crate::source_derivation::SourceDerivationObservation>(
            derivation.clone(),
        )
        .ok();
    let reading = observation
        .as_ref()
        .and_then(|value| value.local_binding_in_scope());
    let mut gap = Map::new();
    gap.insert("kind".into(), json!("local_bindings"));
    gap.insert(
        "why".into(),
        json!(plain_reason("local_binding_outstanding")),
    );
    gap.insert(
        "unit".into(),
        json!("outstanding binding obligations (not a caller count)"),
    );
    gap.insert(
        "scope_note".into(),
        json!("This is an inventory-level binding gap. It does not certify completeness for a narrower semantic scope."),
    );
    if let Some((_, Some(count))) = reading {
        gap.insert("count".into(), json!(count));
        let report = observation.as_ref().and_then(|value| value.report.as_ref());
        let whole = report.map(|report| {
            (
                report.prior_local_binding,
                report.outstanding_local_binding_obligations,
            )
        });
        gap.insert(
            "count_scope".into(),
            json!(if observation
                .as_ref()
                .and_then(|value| value.binding_scope.as_ref())
                .is_some_and(|scope| !scope.include_tests)
            {
                if report
                    .and_then(|report| report.binding_languages_excluding_tests())
                    .is_some()
                {
                    "referring_languages_and_caller_roles"
                } else {
                    "referring_languages_all_caller_roles_unpartitioned"
                }
            } else if reading == whole {
                "checked_inventory"
            } else {
                "referring_languages"
            }),
        );
    } else {
        gap.insert("count_measured".into(), json!(false));
    }
    if observation
        .as_ref()
        .is_some_and(|value| value.checked_scope == "admitted_inventory")
        && matches!(
            reading,
            Some((
                kin_review::source_derivation::PriorLocalBindingStatus::Outstanding,
                _
            ))
        )
        && gap.get("count_scope").and_then(Value::as_str) == Some("checked_inventory")
    {
        gap.insert("resolution".into(), json!("requires_new_binding_evidence"));
        gap.insert(
            "resolution_note".into(),
            json!("Additional reads from the same admitted state do not discharge these inventory binding obligations. Inspect named relevant sites or pending pages when needed. Once the requested positive evidence is established, report it with this coverage gap; do not infer that no other matches exist."),
        );
    }
    if let Some(scope) = derivation.get("checked_scope") {
        gap.insert("checked_scope".into(), scope.clone());
    }
    if let Some(binding) = derivation.get("binding_scope") {
        for key in ["basis", "languages", "include_tests"] {
            if let Some(value) = binding.get(key) {
                gap.insert(key.into(), value.clone());
            }
        }
    }
    Some(Value::Object(gap))
}

/// Name page transport facts as delivery of recorded rows, not semantic coverage.
/// Internal and machine pages retain their original keys.
pub(crate) fn plain_delivery_page(mut page: Map<String, Value>) -> Value {
    for (internal, public) in [
        ("answer_rows_complete", "recorded_rows_exhausted"),
        ("answer_rows_has_more", "recorded_rows_has_more"),
        ("total_references", "recorded_references_total"),
        ("returned_references", "recorded_references_returned"),
        ("recorded_answer_rows_total", "recorded_rows_total"),
        ("returned_answer_rows", "recorded_rows_returned"),
        ("total_steps", "recorded_steps_total"),
        ("returned_steps", "recorded_steps_returned"),
        ("complete", "single_page_reply"),
    ] {
        if let Some(value) = page.remove(internal) {
            page.insert(public.into(), value);
        }
    }
    Value::Object(page)
}

/// The recorded caller scope also travels in every frozen page's envelope.
/// Same-name sections may inherit it from the enclosing answer.
fn reference_caller_scope<'a>(
    machine: &'a Map<String, Value>,
    envelope: &'a Value,
    scope: &'a Value,
) -> Option<&'a Value> {
    machine
        .get("caller_scope")
        .or_else(|| envelope.get("caller_scope"))
        .or_else(|| scope.get("caller_scope"))
}

/// Preserve the selected kinds from the producer or frozen page, never from
/// the kinds that happen to occur among the rows delivered on this page.
fn reference_relation_kinds<'a>(
    machine: &'a Map<String, Value>,
    envelope: &'a Value,
    scope: &'a Value,
) -> Option<&'a Value> {
    machine
        .get("relation_kinds")
        .or_else(|| envelope.get("relation_kinds"))
        .or_else(|| scope.get("relation_kinds"))
}

/// The module path a reference answer's callers were confined to, when the
/// question named one.
fn reference_module_scope(caller_scope: Option<&Value>) -> Option<&str> {
    caller_scope?
        .get(crate::handlers::entities::REFERENCE_SCOPE_MODULE_KEY)?
        .as_str()
}

fn excludes_test_callers(caller_scope: Option<&Value>) -> bool {
    caller_scope
        .and_then(|scope| scope.get("include_tests"))
        .and_then(Value::as_bool)
        == Some(false)
}

/// Scope qualifies the entire primary claim, including its proof, absence,
/// continuation and partial readings. Excluded callers and unrequested kinds
/// are outside the claim, rather than proven absent by a scoped completion.
fn reference_scope_meaning(
    meaning: String,
    caller_scope: Option<&Value>,
    relation_kinds: Option<&Value>,
    word: &str,
) -> String {
    let mut limits = Vec::new();
    // A test exclusion the question asked for is the answer's scope, not a
    // gap in it: a whole answer is complete for non-test code.
    if excludes_test_callers(caller_scope) {
        limits.push(if word == COMPLETE {
            "Complete for non-test code: test code was excluded, as asked.".to_string()
        } else {
            "Test code was excluded, as asked; this covers non-test code.".to_string()
        });
    }
    // A module scope the question asked for is the answer's scope too: what
    // sits outside it is counted, and nothing is said about it.
    if let Some(module) = reference_module_scope(caller_scope) {
        let outside = caller_scope
            .and_then(|scope| scope.get(crate::handlers::entities::REFERENCE_SCOPE_OUTSIDE_KEY))
            .and_then(Value::as_u64);
        limits.push(match outside {
            Some(0) => format!(
                "Only what sits inside {module} is listed, as asked; no recorded row sits outside it."
            ),
            Some(outside) => format!(
                "Only what sits inside {module} is listed, as asked; {} outside it {} not listed.",
                counted(outside, "recorded row", "recorded rows"),
                plural(outside, "is", "are"),
            ),
            None => format!("Only what sits inside {module} is listed, as asked."),
        });
    }
    if let Some(kinds) = relation_kinds.filter(|value| value.is_array()) {
        limits.push(format!("This answer covers only relation kinds {kinds}; unrequested relation kinds are outside its claim."));
    } else {
        limits.push("The selected relation kinds are not recorded in this reply, so that selection is unknown; its claim does not establish coverage of other relation kinds.".to_string());
    }
    let (prefix, answer) = ["Answer: ", "Partial: "]
        .into_iter()
        .find_map(|prefix| meaning.strip_prefix(prefix).map(|answer| (prefix, answer)))
        .unwrap_or(("", meaning.as_str()));
    format!("{prefix}{} Within that scope: {answer}", limits.join(" "))
}

fn attach_plain_reference_scope(
    plain: &mut Map<String, Value>,
    machine: &Map<String, Value>,
    envelope: &Value,
    scope: &Value,
) {
    if let Some(caller_scope) = reference_caller_scope(machine, envelope, scope) {
        plain.insert("caller_scope".into(), caller_scope.clone());
    }
    if let Some(kinds) = reference_relation_kinds(machine, envelope, scope) {
        plain.insert("relation_kinds".into(), kinds.clone());
    }
}

fn plain_references(
    mut machine: Map<String, Value>,
    envelope: &Value,
    scope: &Value,
    details: bool,
    account: &crate::answer_account::Account,
) -> Value {
    // A continuation may hold only rows while its selected kinds remain in the
    // frozen envelope. All wording below must read that same selected scope.
    if let Some(kinds) = reference_relation_kinds(&machine, envelope, scope).cloned() {
        machine.entry("relation_kinds").or_insert(kinds);
    }
    let verdict = MachineVerdict::read(envelope);
    let caller_scope = reference_caller_scope(&machine, envelope, scope);
    let relation_kinds = reference_relation_kinds(&machine, envelope, scope);
    let crate::answer_account::ReferenceRows {
        focal,
        full_focal_missing,
        every_row,
        module_rows,
        rows,
        total: count,
    } = crate::answer_account::ReferenceRows::read(&machine, envelope, scope);
    let focal_name = focal
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("this entity")
        .to_string();
    let modules = module_rows.len() as u64;
    // A module whose top-level code is proven to use the entity is listed
    // apart from the functions too, on a page that holds every row. It stays
    // among `rows` for the account below: a site in that code is settled by
    // its row. A module matched by the name alone stays with the other
    // name-matched rows.
    let whole_page = every_row.len() as u64 == count;
    let (module_code_rows, function_rows): (Vec<Value>, Vec<Value>) =
        if whole_page && !focal_is_module(&focal) {
            rows.iter().cloned().partition(is_module_code_row)
        } else {
            (Vec::new(), rows.clone())
        };
    let module_code = module_code_rows.len() as u64;
    // Every use the answer holds, and the ones that are not such a module.
    let total = count;
    let count = total.saturating_sub(modules + module_code);
    let verb = reference_verb(&machine, count as usize);
    let (one, many) = reference_noun(&function_rows);
    let ReferenceAccount {
        groups,
        left,
        unkeyed,
        other_names,
        passed,
        settled,
        settled_entries,
        folded_entries,
        passed_by_rows: _,
    } = ReferenceAccount::read(&machine, &every_row, &rows, &focal, &focal_name, details);
    let short_name = focal_name
        .rsplit('.')
        .next()
        .unwrap_or(&focal_name)
        .to_string();
    let owner = owner_of(&focal);

    // Rows a type proved and rows matched by the name alone answer different
    // questions, so they are served apart and counted apart.
    let (by_name, proven_rows): (Vec<Value>, Vec<Value>) = function_rows
        .iter()
        .cloned()
        .partition(crate::answer_account::is_name_only_row);
    let named = by_name.len() as u64;
    let proven = count.saturating_sub(named);
    let does = if verb.starts_with("call") {
        "calls"
    } else {
        "uses"
    };
    let noun = |count: u64| if count == 1 { one } else { many };
    // An empty answer beside a count it folded says what was proven, never
    // that nothing does: a folded place is not an absence.
    let counted_beside = folded_entries > 0
        || verdict
            .codes
            .iter()
            .any(|code| crate::verdict::FOLDED_CODES.contains(&code.as_str()));
    // Confined to a module path, an empty answer speaks of that path alone.
    let inside = reference_module_scope(caller_scope).map(str::to_string);
    let within = inside
        .as_ref()
        .map(|module| format!(" inside {module}"))
        .unwrap_or_default();
    let mut found = if named == 0 {
        match (count, &inside) {
            (0, Some(module)) if modules + module_code == 0 => {
                format!("nothing inside {module} {does} {focal_name}")
            }
            _ => match count {
                0 if modules + module_code > 0 => {
                    format!("no function or method {does} {focal_name}")
                }
                0 if counted_beside => format!(
                    "no {one} is proven to {} {focal_name}",
                    if does == "calls" { "call" } else { "use" }
                ),
                0 => format!("nothing {does} {focal_name}"),
                1 => format!("this {} {verb} {focal_name}", noun(1)),
                _ => format!("these {count} {} {verb} {focal_name}", noun(count)),
            },
        }
    } else {
        let act = if does == "calls" { "call" } else { "use" };
        let lead = match proven {
            0 => format!("no {one} is proven to {act} {focal_name}"),
            1 => format!("1 {one} is proven to {act} {focal_name}"),
            _ => format!("{proven} {many} are proven to {act} {focal_name}"),
        };
        let more = if named == 1 { "calls" } else { "call" };
        let what = match &owner {
            Some(_) => {
                format!("{more} a method named {short_name} on an object Kin could not type")
            }
            None => format!("{more} {short_name} by a name Kin matched to {focal_name} alone"),
        };
        format!(
            "{lead}, and {named} more {what}; name_only lists {}",
            if named == 1 { "it" } else { "them" }
        )
    };
    let dispatch = function_rows
        .iter()
        .filter(|row| is_override_dispatch(row))
        .count() as u64;
    if dispatch > 0 {
        // Count only the shown rows here: a continuation may carry the other
        // references, so its unseen rows cannot be assigned to either group.
        let direct = proven_rows
            .iter()
            .filter(|row| !is_override_dispatch(row))
            .count() as u64;
        let direct = match direct {
            0 => format!("no shown {one} {does} {focal_name} directly"),
            1 => format!("1 {one} {does} {focal_name} directly"),
            n => format!("{n} {many} {verb} {focal_name} directly"),
        };
        found = format!(
            "{direct}; {dispatch} more may {} it through a method it overrides; through_override_of names the base method",
            if does == "calls" { "call" } else { "use" },
        );
        if named > 0 {
            found.push_str(&format!(
                "; {named} more match by name alone and are listed under name_only"
            ));
        }
    }
    if module_code > 0 {
        found.push_str(&format!(
            "; {} {} it in top-level code or the callbacks that code passes, no function, \
             listed apart under {MODULE_CODE_USERS_KEY}",
            counted(module_code, "module", "modules"),
            plural(module_code, "uses", "use"),
        ));
    }
    if modules > 0 {
        found.push_str(&format!(
            "; {} it at module level, listed apart",
            match modules {
                1 => "1 module imports or re-exports".to_string(),
                more => format!("{more} modules import or re-export"),
            }
        ));
    }

    // What a complete answer checked, said once: every use of the name in
    // every code file, and, when the reply measured it, every place that
    // could hold the entity as a value. The primary meaning qualifies this
    // entire claim with the answer's caller scope below.
    let where_ = where_checked();
    // Asked about more than calls, a reply that measured only calls and the
    // values that pass or store the entity says exactly that much.
    let calls_only = verdict
        .codes
        .iter()
        .any(|code| code == REFERENCE_COMPLETION_UNMEASURED);
    let checked = uses_checked(&machine, &short_name, calls_only);
    let values = if value_uses_ruled_out(&machine) {
        ", and every place that passes or stores it as a value"
    } else {
        ""
    };
    let proof = if total == 0 {
        format!("Kin checked {where_} for {checked}{values}, and found none")
    } else {
        [
            evidence_sentence(&rows, verb, details),
            format!("Kin checked {where_} for {checked}{values}"),
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
    };
    // Whether the question was every use rather than a narrower kind, which
    // decides whether a certified absence may say "unused".
    let asked_every_use = machine
        .get("relation_kinds")
        .and_then(Value::as_array)
        .is_some_and(|kinds| {
            ["calls", "imports", "references"]
                .iter()
                .all(|kind| kinds.iter().any(|asked| asked.as_str() == Some(kind)))
        });
    let claim_scope = if relation_kinds.is_some_and(Value::is_array) {
        " within the selected scope"
    } else {
        " within this answer's recorded scope"
    };
    let passed_count = passed.len() as u64;
    let open_parts: Vec<String> = [
        (passed_count > 0).then(|| {
            format!(
                "{} {} it as an argument to other code, which may call it; passed_to_calls lists {} with {} code",
                counted(passed_count, "call", "calls"),
                if passed_count == 1 { "passes" } else { "pass" },
                if passed_count == 1 { "it" } else { "each" },
                "its",
            )
        }),
        (left > 0).then(|| {
            format!(
                "{} could reach it only in the way {} group in left_to_check says",
                counted(left, "other place", "other places"),
                plural(left, "its", "their")
            )
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    let otherwise = if let (0, Some(module)) = (total, &inside) {
        format!("otherwise answer that nothing inside {module} {does} it")
    } else if total == 0 {
        format!("otherwise answer that nothing {does} it{claim_scope}")
    } else {
        "otherwise answer from the rows".to_string()
    };
    // How to weigh a row matched by name alone, said once, where the other
    // open sites are weighed.
    let weigh_named = (named > 0).then(|| match &owner {
        Some(owner) => format!(
            "Count a name_only row only if its object could be {} {owner}; ",
            article(owner)
        ),
        None => format!("Count a name_only row only if it could be {focal_name}; "),
    });
    // A folded group is a count, so it leaves nothing to check by itself.
    let listed_open = reference_lists_open_places(named, passed_count, &groups);
    // A page of a paged reference answer can carry rows without its focal,
    // which rides on another page: it says which rows of how many it holds.
    let continued = full_focal_missing.then(|| {
        let total = envelope
            .pointer("/page/total_references")
            .and_then(Value::as_u64)
            .unwrap_or(rows.len() as u64);
        if rows.is_empty()
            && envelope.pointer("/page/answer_rows_complete").and_then(Value::as_bool) == Some(true)
        {
            return "Answer: no more recorded reference rows remain; this page carries candidate or coverage evidence. Unresolved sites could still add callers, each only in the way its group in left_to_check says.".to_string();
        }
        format!(
            "Answer: this page holds {} of the {total} rows of this reference answer. {}",
            rows.len(),
            evidence_sentence(&rows, verb, details)
        ).trim_end().to_string()
    });
    // The account graded the answer once, on its places counted as a short
    // reply counts them; the sentence says what decided it.
    let word = account.standing.word();
    let meaning = match account.basis {
        Basis::RefContinued => continued.clone().unwrap_or_default(),
        Basis::RefWhole => {
            let tail = if let (true, Some(module)) = (total > 0, &inside) {
                format!(", and nothing else inside {module} {does} it{claim_scope}")
            } else if inside.is_some() {
                // An absence inside one module path says nothing of the rest
                // of the repository, so it is never called unused.
                String::new()
            } else if total > 0 {
                format!(", and nothing else {does} it{claim_scope}")
            } else if verdict.safe_to_conclude_absent && asked_every_use {
                format!(", so it is safe to conclude {focal_name} is unused{claim_scope}")
            } else if verdict.safe_to_conclude_absent {
                format!(", so it is safe to conclude nothing {does} {focal_name}{claim_scope}")
            } else {
                String::new()
            };
            format!("Answer: {found}. {proof}{tail}; you can answer from this.")
        }
        Basis::RefListedOpen => {
            let mut text = format!("Answer: {found}. {proof}.");
            if !open_parts.is_empty() {
                text.push_str(&format!(" {}.", open_parts.join("; ")));
            }
            text.push_str(&match &weigh_named {
                Some(weigh) => format!(
                    " {weigh}check any other listed site only if its reason could apply, and {otherwise}."
                ),
                None => {
                    format!(" Check a listed site only if its reason could apply, {otherwise}.")
                }
            });
            text
        }
        // Every place left is folded into a count: the rows are the answer,
        // and the count is said below with the one way each place could
        // matter. It stays complete except that count, never an absence.
        Basis::RefFolded => format!("Answer: {found}. {proof}; answer from the rows."),
        // Every place that spelled the name and was not a row is settled:
        // proved to add no function to the answer, or what is left is the
        // edge of what any reader of code can follow, which `searched`
        // states, or a gap counted beside the answer, said below.
        Basis::RefSettled | Basis::RefCounted => {
            format!("Answer: {found}. {proof}; you can answer from this.")
        }
        Basis::RefIndirect => format!("Answer: {found}. {proof}. It could also be reached indirectly, by being passed as a value or looked up by name, and this reply does not list those places."),
        // Calls and values were measured and certify; only the non-call
        // mentions were not. Say what was checked, then name what was not.
        Basis::RefNonCallUnmeasured => {
            let found = if total == 0 {
                format!("no recorded reference to {focal_name}{within} was found")
            } else {
                found
            };
            let mut text = format!(
                "Partial: {found}. {proof}. Left unchecked: {}. Only such a place could {}.",
                non_call_unmeasured_for(&focal, &machine),
                if total == 0 { "use it" } else { "add more" }
            );
            if !open_parts.is_empty() {
                text.push_str(&format!(" {}.", capitalized(&open_parts.join("; "))));
            }
            if listed_open {
                text.push_str(" Check a listed site only if its reason could apply.");
            }
            text
        }
        _ => {
            let mut reasons = reference_open_reasons(&machine, &verdict);
            for reason in &mut reasons {
                if reason == NON_CALL_UNMEASURED {
                    *reason = non_call_unmeasured_for(&focal, &machine);
                }
            }
            let why = if reasons.is_empty() {
                "Kin could not check every place it could be used".to_string()
            } else {
                reasons.join("; ")
            };
            let found = if total == 0 {
                format!(
                    "no recorded {} to {focal_name}{within} was found",
                    if verb.starts_with("call") { "call" } else { "reference" },
                )
            } else {
                found
            };
            let mut text = format!("Partial: {found}, and there may be more, because {why}.");
            if listed_open {
                text.push_str(" The places listed below could add more.");
            }
            text
        }
    };
    let mut meaning = meaning;
    // A module asked about imports also says what it imports itself, which
    // is the other question the same words ask.
    let own_imports = machine
        .get(crate::handlers::entities::MODULE_IMPORTS_KEY)
        .and_then(plain_module_imports);
    if let Some((rows, total, outside)) = &own_imports {
        let listed = rows.len() as u64;
        let which = if listed < *total {
            format!("the first {listed} of the {total} names")
        } else {
            counted(*total, "name", "names")
        };
        meaning.push_str(&format!(
            " What {focal_name} itself imports is in {MODULE_IMPORTS_PLAIN_KEY}: {which} its \
             import statements bind, each with the module it comes from; {outside} of them \
             come from a module no file of this repository supplies, marked \
             outside_repository. An `import x` whose name {focal_name} never reads is not \
             among them."
        ));
    }
    if folded_entries > 0 {
        meaning.push_str(&format!(
            " {folded_entries} more {} counted by reason in left_to_check, not listed; each could add a caller only through the mechanism its reason names.",
            plural(folded_entries, "place is", "places are"),
        ));
    }
    if settled_entries > 0 {
        meaning.push_str(&format!(
            " {settled_entries} more {} {short_name} {} settled: {} proved to add no function to this answer, and {SETTLED_KEY} counts {} by reason.",
            plural(settled_entries, "place that spells", "places that spell"),
            plural(settled_entries, "is", "are"),
            plural(settled_entries, "it is", "each is"),
            plural(settled_entries, "it", "them"),
        ));
    }
    if let Some(folded) = folded_bindings_sentence(&verdict, scope) {
        meaning.push(' ');
        meaning.push_str(&folded);
    }
    let binding_gap = scope
        .get("reference_gap")
        .cloned()
        .or_else(|| reference_binding_gap_for_verdict(scope, &verdict));
    if account.standing == AnswerStanding::Partial {
        if let Some(note) = binding_gap
            .as_ref()
            .filter(|gap| {
                gap.get("resolution").and_then(Value::as_str)
                    == Some("requires_new_binding_evidence")
            })
            .and_then(|gap| gap.get("resolution_note"))
            .and_then(Value::as_str)
        {
            meaning.push(' ');
            meaning.push_str(note);
        }
    }
    if account.standing == AnswerStanding::Partial
        && verdict
            .codes
            .iter()
            .any(|code| code == REFERENCE_COMPLETION_UNMEASURED)
    {
        meaning.push_str(
            " Answer from the recorded references and report the unchecked coverage. Read source for a listed entity-local site if its role needs inspection. A text lookup can suggest candidates, but cannot establish missing reference bindings or close this completeness gap."
        );
    }
    if dispatch > 0 {
        meaning.push_str(" Read a through_override_of caller with source to inspect its receiver; a call to the base alone does not prove the override runs.");
    }
    // A caller of an override is not a reference to this method, and changes
    // with its signature all the same, so the answer says where they are.
    if let Some(callers) = machine
        .get(crate::handlers::overrides::OVERRIDDEN_BY_KEY)
        .and_then(|block| block.get(crate::handlers::overrides::OVERRIDE_CALLERS_KEY))
        .and_then(Value::as_array)
        .map(|rows| rows.len() as u64)
        .filter(|count| *count > 0)
    {
        meaning.push_str(&format!(
            " {callers} more {} an override of it rather than it: {} under overridden_by.override_callers, each naming the override in calls_override, and {} with its signature too.",
            plural(callers, "caller calls", "callers call"),
            plural(callers, "it is", "they are"),
            plural(callers, "changes", "change"),
        ));
    }
    if let Some(held) = machine
        .get("unconfirmed_candidates")
        .and_then(Value::as_u64)
        .filter(|held| *held > 0)
    {
        meaning.push_str(&format!(
            " {held} more possible {} matched by name only {} not listed.",
            plural(held, "caller", "callers"),
            plural(held, "is", "are")
        ));
    }
    let other_matches = machine
        .get("focal_resolution")
        .and_then(|resolution| resolution.get("other_candidates"))
        .and_then(Value::as_array)
        .filter(|others| !others.is_empty())
        .cloned();
    if let Some(others) = &other_matches {
        meaning.push_str(&format!(
            " {} other {} share this name; pass an entity id to pick one.",
            others.len(),
            plural(others.len() as u64, "entity", "entities"),
        ));
    }
    // The edge of what any reader of code can follow is scope, said where the
    // scope is, never in the lead.
    let edge =
        (unkeyed > 0 || only_unkeyed(&verdict)).then(|| unkeyed_sentence(unkeyed, &short_name));

    let mut plain = Map::new();
    plain.insert(
        crate::envelope::ENVELOPE_KEY.into(),
        plain_envelope_delivered(
            account.word(),
            reference_scope_meaning(meaning, caller_scope, relation_kinds, word),
            with_records_left(delivered_sentence(&machine, &verdict), envelope),
            with_scope(Some(searched_or_unmeasured(scope)), edge),
            verdict.remedy.as_deref(),
        ),
    );
    attach_plain_reference_scope(&mut plain, &machine, envelope, scope);
    plain.insert("entity".into(), plain_focal(&focal));
    // A short reply shows each proven row's receiver where the row records
    // it, so a reader trusts the row without reading the caller's body.
    // Details keep the rows as they were.
    let described = (!details).then(|| crate::receiver_binding::described_steps(&rows));
    plain.insert(
        "references".into(),
        json!(proven_rows
            .iter()
            .map(|row| {
                let mut plain_row = plain_reference_row(row);
                if let Some(receiver) = described
                    .as_ref()
                    .and_then(|described| crate::receiver_binding::short_receiver(row, described))
                {
                    plain_row[crate::receiver_binding::SHORT_RECEIVER_KEY] = receiver;
                }
                plain_row
            })
            .collect::<Vec<_>>()),
    );
    if !by_name.is_empty() {
        plain.insert(
            NAME_ONLY_KEY.into(),
            json!(by_name.iter().map(plain_reference_row).collect::<Vec<_>>()),
        );
    }
    if !module_code_rows.is_empty() {
        plain.insert(
            MODULE_CODE_USERS_KEY.into(),
            json!(module_code_rows
                .iter()
                .map(plain_reference_row)
                .collect::<Vec<_>>()),
        );
    }
    if !module_rows.is_empty() {
        plain.insert(
            MODULE_LEVEL_USERS_KEY.into(),
            json!(module_rows
                .iter()
                .map(plain_reference_row)
                .collect::<Vec<_>>()),
        );
    }
    if !passed.is_empty() {
        plain.insert(
            PASSED_TO_CALLS_KEY.into(),
            json!({
                "why": format!("Calls that pass {short_name} to other code as an argument. The code it is passed to may call it, as a test helper that takes a function and calls it does. Each site's note says who passes it and to what: the function to count as a caller is the one that passes it, never the code it is passed to."),
                "sites": passed,
            }),
        );
    }
    if !groups.is_empty() {
        plain.insert(LEFT_TO_CHECK_KEY.into(), json!(groups));
    }
    // Sites a sweep could not decide after repeated passes over unchanged
    // code are served as the answer reports them, each with its caller and
    // line.
    if let Some(undecided) = machine
        .get(UNDECIDED_REFERENCE_SITES_KEY)
        .filter(|block| !block.is_null())
    {
        plain.insert(UNDECIDED_REFERENCE_SITES_KEY.into(), undecided.clone());
    }
    if !settled.is_empty() {
        plain.insert(SETTLED_KEY.into(), json!(settled));
    }
    if let Some((rows, _, _)) = own_imports {
        plain.insert(MODULE_IMPORTS_PLAIN_KEY.into(), json!(rows));
    }
    if let Some(other_names) = other_names {
        plain.insert("other_names".into(), other_names);
    }
    if let Some(mut overrides) = machine
        .get(crate::handlers::overrides::OVERRIDDEN_BY_KEY)
        .and_then(plain_overrides)
    {
        // A question about calls alone is not one about which classes could
        // override the method: the census of classes that spell its base is
        // left out, counted in coverage, and its scope gap goes with it.
        if calls_only_question(&machine) {
            leave_out_override_census(&mut overrides);
        }
        plain.insert("overridden_by".into(), overrides);
    }
    if let Some(others) = other_matches {
        plain.insert("other_matches".into(), json!(others));
    }
    // Coverage limits apply equally to compact and detailed evidence.
    if let Some(gap) = binding_gap {
        plain.insert("coverage_gaps".into(), json!([gap]));
    }
    if let Some(page) = envelope
        .get("page")
        .filter(|page| page.get("answer_rows_complete").is_some())
    {
        let keys = [
            "answer_rows_complete",
            "answer_rows_has_more",
            "evidence_has_more",
            "total_references",
            "returned_references",
        ];
        plain[crate::envelope::ENVELOPE_KEY]["page"] = plain_delivery_page(
            keys.into_iter()
                .filter_map(|key| page.get(key).map(|value| (key.to_string(), value.clone())))
                .collect(),
        );
    }
    for key in ["next_cursor", "answer_next_cursor", "evidence_next_cursor"] {
        if let Some(cursor) = machine.get(key).filter(|cursor| !cursor.is_null()) {
            plain.insert(key.into(), cursor.clone());
        }
    }
    Value::Object(plain)
}

/// A reference answer for a name several entities share: one plain answer per
/// entity, and a sentence saying so.
fn plain_sectioned_references(
    mut machine: Map<String, Value>,
    envelope: &Value,
    details: bool,
    account: &crate::answer_account::Account,
) -> Value {
    let sections: Vec<Value> = machine
        .remove(crate::handlers::entities::AMBIGUOUS_CANDIDATES_KEY)
        .and_then(|value| match value {
            Value::Array(sections) => Some(sections),
            _ => None,
        })
        .unwrap_or_default();
    let query = machine
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("this name")
        .to_string();
    let answered: Vec<Value> = sections
        .into_iter()
        .map(|section| match section {
            Value::Object(mut section) => {
                let section_envelope = section
                    .remove(crate::envelope::ENVELOPE_KEY)
                    .unwrap_or(Value::Null);
                // The section label may be the only recorded spelling that
                // distinguishes same-named members in different modules.
                if let Some(qualified) = section.get("qualified_name").cloned() {
                    if let Some(focal) = section
                        .get_mut("focal_entity")
                        .and_then(Value::as_object_mut)
                    {
                        focal.entry("qualified_name").or_insert(qualified);
                    }
                }
                for key in crate::handlers::entities::SECTION_LABEL_KEYS {
                    section.remove(key);
                }
                let section_account =
                    crate::answer_account::reference_account(&section, &section_envelope, envelope);
                plain_references(
                    section,
                    &section_envelope,
                    envelope,
                    details,
                    &section_account,
                )
            }
            other => other,
        })
        .collect();
    let verdict = MachineVerdict::read(envelope);
    let word = account.standing.word();
    // A page that holds only some of the sections says how many there are,
    // so its count is never read as every entity of that name.
    let sectioned = machine
        .get("focal_resolution")
        .and_then(|resolution| resolution.get("sectioned"))
        .and_then(Value::as_u64)
        .filter(|total| *total > answered.len() as u64);
    let more_follow = machine
        .get("answer_next_cursor")
        .is_some_and(Value::is_string)
        || envelope
            .pointer("/page/answer_rows_has_more")
            .and_then(Value::as_bool)
            == Some(true);
    let mut meaning = match sectioned {
        Some(total) => format!(
            "{total} entities are named {query}; this page answers {} of them, and the answer cursor serves the rest. Each is answered on its own in answers, with its own verdict; this verdict is the least complete of those here.",
            answered.len()
        ),
        None if more_follow => format!(
            "This page answers {} named {query}, and the answer cursor serves the rest. Each is answered on its own in answers, with its own verdict; this verdict is the least complete of those here.",
            counted(answered.len() as u64, "entity", "entities")
        ),
        None => format!(
            "{} named {query}. Each is answered on its own in answers, with its own verdict; this verdict is the least complete of them.",
            counted(answered.len() as u64, "entity is", "entities are")
        ),
    };
    let omitted = machine
        .get("omitted_candidates")
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
        + machine
            .get("unsectioned_candidates")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
    if omitted > 0 {
        meaning.push_str(&format!(
            " {omitted} more {} this name {} not answered here; pass an entity id to ask about one.",
            plural(omitted as u64, "entity has", "entities have"),
            plural(omitted as u64, "is", "are"),
        ));
    }
    let mut plain = Map::new();
    plain.insert(
        crate::envelope::ENVELOPE_KEY.into(),
        plain_envelope(
            verdict.state.is_some().then_some(word),
            reference_scope_meaning(
                meaning,
                reference_caller_scope(&machine, envelope, envelope),
                reference_relation_kinds(&machine, envelope, envelope),
                word,
            ),
            Some(searched_or_unmeasured(envelope)),
            verdict.remedy.as_deref(),
        ),
    );
    attach_plain_reference_scope(&mut plain, &machine, envelope, envelope);
    plain.insert("answers".into(), json!(answered));
    for key in ["unsectioned_candidates", "omitted_candidates"] {
        if let Some(value) = machine
            .get(key)
            .filter(|value| value.as_array().is_some_and(|rows| !rows.is_empty()))
        {
            plain.insert(key.into(), value.clone());
        }
    }
    if let Some(cursor) = machine
        .get("next_cursor")
        .filter(|cursor| !cursor.is_null())
    {
        plain.insert("next_cursor".into(), cursor.clone());
    }
    Value::Object(plain)
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// The registered tool `status` answers through.
pub(crate) const STATUS_TOOL: &str = "kin_graph_status";

/// The sentence that opens every status reply. A status counts facts about the
/// whole repository, such as how many of its calls Kin matched, and none of
/// them is a verdict on an answer: each answer judges itself.
pub const STATUS_LEAD: &str =
    "Health report for this repository, not a verdict on any answer: every answer carries its own.";

/// How long ago, in the largest whole unit: `40 seconds`, `3 minutes`.
fn ago(seconds: u64) -> String {
    match seconds {
        0..=119 => counted(seconds, "second", "seconds"),
        120..=7199 => counted(seconds / 60, "minute", "minutes"),
        7200..=172_799 => counted(seconds / 3600, "hour", "hours"),
        _ => counted(seconds / 86_400, "day", "days"),
    }
}

/// When Kin last took in changes from the working copy, as the envelope's
/// admission clock records it. The clock says when, never that the graph is
/// level with the files now; a status that finds it is not says so apart.
fn freshness_sentence(envelope: &Value) -> Option<String> {
    let freshness = envelope.get("freshness")?;
    match freshness.get("state").and_then(Value::as_str)? {
        "recorded" => Some(match freshness.get("age_seconds").and_then(Value::as_u64) {
            Some(age) => format!("Kin last admitted ingested changes {} ago.", ago(age)),
            None => "Kin has admitted ingested changes since it started.".to_string(),
        }),
        "no_admission_recorded" => Some(
            "Kin has not recorded ingestion since it started; it answers from the graph it loaded."
                .to_string(),
        ),
        "stale" => Some(
            "This status repeats an earlier reading because Kin could not read the graph live."
                .to_string(),
        ),
        _ => None,
    }
}

/// A status reply as a health report: what Kin holds, what is settled and what
/// is still being built, with no verdict word. The machine fields stay on the
/// machine reply.
fn plain_status(machine: Map<String, Value>, envelope: &Value) -> Value {
    let machine = Value::Object(machine);
    let verdict = MachineVerdict::read(envelope);
    let number = |pointer: &str| machine.pointer(pointer).and_then(Value::as_u64);
    let mut sentences = vec![STATUS_LEAD.to_string()];
    let mut holds = Map::new();

    let entities = number("/entity_count");
    let relations = number("/relation_count");
    if let (Some(entities), Some(relations)) = (entities, relations) {
        let mut text = format!(
            "Kin holds {} and {}",
            counted(entities, "entity", "entities"),
            counted(relations, "relationship", "relationships")
        );
        text.push('.');
        sentences.push(text);
        holds.insert("entities".into(), json!(entities));
        holds.insert("relationships".into(), json!(relations));
    }
    if machine
        .pointer("/source_derivation/report/parse_coverage")
        .or_else(|| envelope.pointer("/source_derivation/report/parse_coverage"))
        .and_then(Value::as_str)
        == Some("complete")
    {
        sentences.push("Parsing is complete for the checked semantic inventory.".to_string());
    }
    if let Some(freshness) = freshness_sentence(envelope) {
        sentences.push(freshness);
    }
    if let Some(generation) = envelope
        .pointer("/graph_as_of/generation")
        .filter(|generation| generation.is_u64())
    {
        holds.insert("graph_generation".into(), generation.clone());
    }

    // Calls still owed a check, the one call fact that changes with time.
    let owed = number("/call_sites/callers_owed").or_else(|| {
        Some(
            number("/call_sites/callers_owed_derivation")?
                + number("/call_sites/callers_owed_enrichment")?,
        )
    });
    if let Some(owed) = owed {
        holds.insert("callers_waiting_to_be_checked".into(), json!(owed));
        if owed == 0 {
            sentences.push("No call is waiting to be checked.".to_string());
        } else {
            sentences.push(format!(
                "Kin is still checking the calls of {}; an answer that depends on them says so.",
                counted(owed, "caller", "callers")
            ));
        }
    }

    let total = number("/embeddings_total");
    let built = number("/embeddings_indexed");
    let pending = number("/embeddings_pending").unwrap_or(0);
    let vectors_unused = machine
        .pointer("/search_vectors_used")
        .and_then(Value::as_bool)
        == Some(false);
    if vectors_unused {
        // A setup that ranks without search vectors is not waiting for them, so
        // counting the ones it does not have would read as a gap it does not have.
        holds.insert("search_vectors".into(), json!({"used": false}));
        sentences.push(
            "This setup uses no search vectors, so locate ranks descriptions by entity cards \
             and the graph."
                .to_string(),
        );
    } else if let (Some(total), Some(built)) = (total, built) {
        holds.insert(
            "search_vectors".into(),
            json!({"built": built, "total": total}),
        );
        sentences.push(if total == 0 {
            "No search vectors are built, so locate matches names and text only.".to_string()
        } else if pending == 0 && built >= total {
            format!("All {total} search vectors are built.")
        } else {
            format!("{built} of {total} search vectors are built; the rest are still being built.")
        });
    }

    if let Some(open) = number("/open_transactions/page/total") {
        holds.insert("open_changes".into(), json!(open));
        sentences.push(match open {
            0 => "No staged change is open.".to_string(),
            1 => "1 staged change is open.".to_string(),
            _ => format!("{open} staged changes are open."),
        });
    }

    // Whatever else the verdict found wrong with Kin itself: a store an older
    // Kin built, code changed since Kin read it, checks switched off.
    let mut health: Vec<String> = Vec::new();
    for code in &verdict.codes {
        if is_call_resolution_code(code) {
            continue;
        }
        if let Some(reason) = verdict
            .plain_reason_named(code)
            .filter(|reason| reason != UNEXPLAINED)
        {
            if !health.contains(&reason) {
                health.push(reason);
            }
        }
    }
    let switched_off: Vec<&str> = envelope
        .get("degraded")
        .and_then(Value::as_object)
        .map(|degraded| {
            degraded
                .iter()
                .filter(|(_, value)| value.as_bool() == Some(true))
                .map(|(key, _)| key.as_str())
                .collect()
        })
        .unwrap_or_default();
    if !switched_off.is_empty() {
        let signals: Vec<String> = switched_off
            .iter()
            .map(|signal| signal.to_string())
            .collect();
        let reason = degraded_reason(&signals).to_string();
        health.retain(|known| known != plain_reason("degraded").unwrap_or(UNEXPLAINED));
        if !health.contains(&reason) {
            health.push(reason);
        }
    }
    let fix = (!health.is_empty())
        .then_some(verdict.remedy.as_deref())
        .flatten();
    if !health.is_empty() {
        let mut text = health.join("; ");
        if let Some(first) = text.get(..1) {
            text = format!("{}{}", first.to_ascii_uppercase(), &text[1..]);
        }
        sentences.push(format!("{text}."));
    }

    let mut plain = Map::new();
    plain.insert(
        crate::envelope::ENVELOPE_KEY.into(),
        plain_envelope(None, sentences.join(" "), None, fix),
    );
    plain.extend(holds);
    Value::Object(plain)
}

// ---------------------------------------------------------------------------
// Text search
// ---------------------------------------------------------------------------

/// What a text search reads, said in one line. A literal lookup matches the
/// fields Kin stores for each entity, not every byte of every file, so this
/// line is the scope its answer is complete over.
pub const LITERAL_SCOPE: &str = "Searched stored lookup fields associated with entities: names, \
    signatures, documentation, code previews and recorded context. Code previews collapse whitespace \
    and may be sampled past 8000 characters. A contextual match does not prove the text occurs \
    inside the entity, and missing matches do not prove semantic absence.";

fn matched_field(field: &str) -> Option<&'static str> {
    match field {
        "name" => Some("name"),
        "signature" => Some("signature"),
        "doc_summary" => Some("doc summary"),
        "file_import_context" => Some("recorded import context"),
        _ => None,
    }
}

/// Keep semantic identity and exact returned code without a physical coordinate.
fn plain_literal_hit(hit: &Value) -> Value {
    let mut plain = Map::new();
    insert_identity(&mut plain, hit);
    let field = hit.get("matched_field").and_then(Value::as_str);
    if field == Some("file_surface_context")
        || hit.get("semantic_gap").and_then(Value::as_str) == Some("entity_local_match_unavailable")
    {
        plain.insert(
            "semantic_gap".into(),
            json!("entity_local_match_unavailable"),
        );
        plain.insert(
            "note".into(),
            json!("No entity-local occurrence is provided; the associated entity's source may not contain this match."),
        );
        return Value::Object(plain);
    }
    if let Some(field) = field.and_then(matched_field) {
        plain.insert("matched_in".into(), json!(field));
    }
    if field == Some("file_import_context")
        && hit.get("line_confidence").and_then(Value::as_str) != Some("exact")
    {
        plain.insert(
            "at".into(),
            json!("recorded context; lexical scope unproven"),
        );
    } else if let Some(offset) = hit.get("line_in_entity").filter(|offset| offset.is_u64()) {
        plain.insert("line_in_entity".into(), offset.clone());
    }
    if matches!(field, Some("name" | "signature" | "doc_summary")) {
        plain.insert("at".into(), json!("its declaration"));
    }
    if let Some(code) = hit.get("excerpt").filter(|code| !code.is_null()) {
        plain.insert("code".into(), code.clone());
    }
    for key in ["more_lines", "role", "held_as"] {
        if let Some(value) = hit.get(key).filter(|value| !value.is_null()) {
            plain.insert(key.into(), value.clone());
        }
    }
    Value::Object(plain)
}

/// What a text search over held module text says it read, and how it stands:
/// whole when every parsed module was read back, partial when some could not
/// be, since the text could sit in one of those.
fn held_text_meaning(
    literal: &str,
    total: u64,
    coverage: &Value,
    options: &Value,
) -> (String, String) {
    let count = |key: &str| coverage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let modules = count("modules");
    let unread = count("modules_unread");
    let outside = count("occurrences_outside_entities");
    let tests_left_out = count("tests_left_out");
    let artifacts = count("artifacts_text");
    let artifacts_unread = count("artifacts_unread");
    let spans_gap = count("modules_without_complete_spans");
    let unaddressable = count("unaddressable");
    let parse_gap = count("modules_with_unverified_parse");
    let authority_gap = coverage
        .get("authority_unavailable")
        .and_then(Value::as_bool)
        == Some(true);
    let partial = crate::answer_account::held_text_open(coverage);
    // What Kin read, by kind, leaving out a kind it holds none of.
    let read = |modules: u64, artifacts: u64| {
        let held: Vec<String> = [
            (modules, "parsed module", "parsed modules"),
            (
                artifacts,
                "unparsed entity held as text",
                "unparsed entities held as text",
            ),
        ]
        .into_iter()
        .filter(|(count, _, _)| *count > 0)
        .map(|(count, one, many)| counted(count, one, many))
        .collect();
        held.join(" and ")
    };
    // Matches left out as test code are said below; the miss is of the rest.
    let outside_tests = if tests_left_out > 0 {
        " outside test code"
    } else {
        ""
    };
    let mut meaning = if total == 0 && !partial {
        match read(modules, artifacts) {
            held if held.is_empty() => format!(
                "Answer: Kin holds no code or text in this repository, so {literal:?} is in none of it."
            ),
            held => format!(
                "Answer: {literal:?} is in nothing Kin holds as code or text{outside_tests}. Kin read every one of its {held}, and none holds it."
            ),
        }
    } else if total == 0 {
        // Said only of what Kin read; what it could not read is named below.
        match read(
            modules.saturating_sub(unread),
            artifacts.saturating_sub(artifacts_unread),
        ) {
            held if held.is_empty() => format!(
                "Partial: Kin could read none of the code or text it holds, so it cannot say whether any holds {literal:?}."
            ),
            held => format!("Partial: {literal:?} is in none of the {held} Kin read{outside_tests}."),
        }
    } else {
        format!(
            "Answer: {literal:?} is in {} Kin holds. Each row is the entity and the first line in \
             it that holds the text, numbered from 1 within the entity.",
            counted(total, "entity", "entities")
        )
    };
    if unread > 0 {
        meaning.push_str(&format!(
            " Kin could not read back {}, so the text could also be there.",
            counted(unread, "parsed module", "parsed modules")
        ));
    }
    if outside > 0 {
        meaning.push_str(&format!(
            " {} in no entity Kin holds.",
            counted(outside, "occurrence is", "occurrences are")
        ));
    }
    if tests_left_out > 0 {
        meaning.push_str(&format!(
            " {} left out as test code (include_tests true lists {}).",
            counted(tests_left_out, "entity was", "entities were"),
            plural(tests_left_out, "it", "them")
        ));
    }
    if spans_gap > 0 {
        meaning.push_str(&format!(" {} have missing or partial entity spans; text there has no complete semantic coverage.", counted(spans_gap, "parsed module", "parsed modules")));
    }
    if parse_gap > 0 {
        meaning.push_str(&format!(" {} have no current full-parse certificate; their text matches do not prove complete semantic binding.", counted(parse_gap, "parsed module", "parsed modules")));
    }
    if artifacts_unread > 0 {
        meaning.push_str(&format!(
            " Kin could not read back {}, so the text could also be there.",
            counted(artifacts_unread, "unparsed entity", "unparsed entities")
        ));
    }
    if unaddressable > 0 {
        meaning.push_str(" Some held members have no supported semantic address.");
    }
    if authority_gap {
        meaning.push_str(" Current repository source authority is unavailable; this answer cannot prove absence.");
    }
    let flag = |key| options.get(key).and_then(Value::as_bool).unwrap_or(false);
    let matching = if flag("pattern") {
        if flag("ignore_case") {
            "matched by regex with Unicode case folding (inline flags apply)"
        } else {
            "matched by regex, case included unless an inline flag changes it"
        }
    } else if flag("ignore_case") {
        "matched literally with Unicode case folding"
    } else {
        "matched exactly, case included"
    };
    let tests = if options
        .get("include_tests")
        .and_then(Value::as_bool)
        .unwrap_or(true)
    {
        "test code included"
    } else {
        "test-role entities excluded; unparsed text has no test role"
    };
    let skipped = count("artifacts_bytes_skipped");
    let searched = format!(
        "Searched the text Kin holds of {} and {} in this repository, {matching}; {tests}. \
         {} held as bytes, links or submodule pointers {} not searched. Each row is bounded match \
         evidence; unparsed text retains its identity without claiming a semantic binding. \
         A text match is not a call or a reference; refs answers who calls something.",
        if modules == 1 {
            "its 1 parsed module".to_string()
        } else {
            format!("all {modules} parsed modules")
        },
        counted(
            artifacts,
            "unparsed entity held as text",
            "unparsed entities held as text"
        ),
        counted(skipped, "unparsed entity", "unparsed entities"),
        plural(skipped, "was", "were"),
    );
    (meaning, searched)
}

/// A literal lookup, plainly. A text search is complete over the fields it
/// reads, so it answers `complete`: how many entities hold the text in those
/// fields, then which ones this page lists and the exact call that lists the
/// rest, which is delivery and never doubt about the answer. A lookup that
/// found nothing says it read only those fields, so no reader takes it for
/// proof the text is nowhere in the repository.
fn plain_literal(
    mut machine: Map<String, Value>,
    envelope: &Value,
    account: &crate::answer_account::Account,
) -> Value {
    let literal = machine
        .get("literal")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let total = machine
        .get("total_matching")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let offset = machine
        .get("page_offset")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let shown = machine
        .get("hits")
        .and_then(Value::as_array)
        .map_or(0, |hits| hits.len() as u64);
    let cursor = machine
        .get("next_cursor")
        .and_then(Value::as_str)
        .map(str::to_string);
    let kind = machine
        .get("kind")
        .and_then(Value::as_str)
        .map(str::to_string);
    let held_text = machine
        .get(crate::handlers::lexical::HELD_TEXT_KEY)
        .and_then(Value::as_bool)
        == Some(true);
    let held_text_reading = held_text.then(|| {
        held_text_meaning(
            &literal,
            total,
            machine
                .get(crate::handlers::lexical::HELD_TEXT_COVERAGE_KEY)
                .unwrap_or(&Value::Null),
            &Value::Object(machine.clone()),
        )
    });
    let mut meaning = if let Some((meaning, _)) = &held_text_reading {
        meaning.clone()
    } else if total == 0 {
        format!(
            "Answer: no searched lookup field associated with an entity Kin holds contains {literal:?}. That does not \
             show it is absent from the repository: it could sit in code or a comment outside \
             every admitted entity. locate with mode name finds a declaration by its name."
        )
    } else {
        format!(
            "Answer: {} {literal:?} in associated lookup fields. A contextual match may not be inside that entity. A text match is not a call or a reference; refs answers who calls something.",
            counted(total, "entity matched", "entities matched"),
        )
    };
    // Partial for a reason the text it read does not show: the graph is not
    // level with the code it was read from.
    if !account.open.is_empty() {
        meaning.push_str(&format!(
            " This answer is partial because {}.",
            MachineVerdict::read(envelope)
                .open_reasons(Operation::Lookup)
                .join("; ")
        ));
    }
    let first = offset + 1;
    let last = offset + shown;
    let delivered = match &cursor {
        Some(_) => Some(format!(
            "Showing {first} to {last} of the {total}. For the rest, call locate again with only cursor set to next_cursor."
        )),
        None if offset > 0 => Some(format!(
            "Showing {first} to {last} of the {total}, the last of them."
        )),
        None => None,
    };
    let mut scope = match &held_text_reading {
        Some((_, searched)) => searched.clone(),
        None => LITERAL_SCOPE.to_string(),
    };
    let include_tests = machine.get("include_tests").and_then(Value::as_bool);
    if include_tests == Some(false) && !held_text {
        scope.push_str(
            " Test-role entities were excluded; pass include_tests true to include them.",
        );
    }
    if let Some(kind) = &kind {
        scope.push_str(&format!(" Only entities of kind {kind} were searched."));
    }
    let mut hits: Vec<Value> = machine
        .remove("hits")
        .and_then(|hits| match hits {
            Value::Array(hits) => Some(hits),
            _ => None,
        })
        .unwrap_or_default()
        .iter()
        .map(plain_literal_hit)
        .collect();
    // Reuse identical contextual text by an existing entity identity. This
    // shares bytes, not a claim about lexical ownership or an import binding.
    let mut imports = std::collections::HashMap::new();
    let mut has_import_preview = false;
    for hit in &mut hits {
        if hit.get("at").and_then(Value::as_str) != Some("recorded context; lexical scope unproven")
        {
            continue;
        }
        has_import_preview = true;
        let (Some(code), Some(id)) = (
            hit.get("code").and_then(Value::as_str),
            hit.get("entity_id").and_then(Value::as_str),
        ) else {
            continue;
        };
        if let Some(first_id) = imports.get(code) {
            let first_id: &String = first_id;
            hit["same_context_as"] = json!(first_id);
            hit.as_object_mut().unwrap().remove("code");
        } else {
            imports.insert(code.to_string(), id.to_string());
        }
    }
    if has_import_preview {
        scope.push_str(" Import context is normalized text, not exact import syntax or proof of a local binding. same_context_as reuses identical context from the named entity in this page.");
    }
    let mut plain = Map::new();
    plain.insert(
        crate::envelope::ENVELOPE_KEY.into(),
        plain_envelope_delivered(account.word(), meaning, delivered, Some(scope), None),
    );
    plain.insert(
        if held_text { "text" } else { "literal" }.into(),
        json!(literal),
    );
    if let Some(include_tests) = include_tests {
        plain.insert("include_tests".into(), json!(include_tests));
    }
    if let Some(kind) = kind {
        plain.insert("kind".into(), json!(kind));
    }
    if held_text {
        for key in ["pattern", "ignore_case"] {
            if let Some(value) = machine.get(key) {
                plain.insert(key.into(), value.clone());
            }
        }
    }
    plain.insert("total".into(), json!(total));
    plain.insert("hits".into(), json!(hits));
    if let Some(cursor) = cursor {
        plain.insert("next_cursor".into(), json!(cursor));
    }
    Value::Object(plain)
}

// ---------------------------------------------------------------------------
// Every other answer
// ---------------------------------------------------------------------------

/// How many entities a trace's reach reading found can end up calling its
/// focal, wherever the reply carried the count: `reach.members_total`, the same
/// field a page's `readings` carries when the reading was split across pages,
/// or the members it lists. `None` when the reply carries no reach reading.
fn reach_members(machine: &Map<String, Value>) -> Option<u64> {
    let reach = machine.get("reach");
    reach
        .and_then(|reach| reach.get("members_total"))
        .and_then(Value::as_u64)
        .or_else(|| {
            machine
                .get("readings")
                .and_then(Value::as_array)?
                .iter()
                .find(|reading| reading.get("key").and_then(Value::as_str) == Some("reach"))?
                .pointer("/value/members_total")
                .and_then(Value::as_u64)
        })
        .or_else(|| {
            reach
                .and_then(|reach| reach.get("members"))
                .and_then(Value::as_array)
                .map(|members| members.len() as u64)
        })
}

/// The member count of a reach whose every member row, and every row matched
/// by name alone, is on this page, when this is the first page of the reply.
fn members_on_first_page(
    machine: &Map<String, Value>,
    envelope: &Value,
    reach: &Map<String, Value>,
) -> Option<u64> {
    if envelope
        .pointer("/page/record_offset")
        .and_then(Value::as_u64)
        .is_some_and(|offset| offset > 0)
    {
        return None;
    }
    let rows = machine.get(crate::trace_reach::REACH_KEY)?;
    let on_page = |key: &str| rows.get(key).and_then(Value::as_array).map_or(0, Vec::len) as u64;
    let total = |key: &str| reach.get(key).and_then(Value::as_u64).unwrap_or(0);
    let members = total("members_total");
    (members > 0
        && on_page("members") == members
        && on_page(crate::trace_reach::NAME_ONLY_KEY)
            == total(crate::trace_reach::NAME_ONLY_TOTAL_KEY))
    .then_some(members)
}

/// The focal's name as a page of a callers walk carries it when the page does
/// not repeat the focal: a member one call away names the focal's id as its
/// `via`, and a frontier row, or a withheld frontier group, aimed at that id
/// names it.
fn reach_focal_name(machine: &Map<String, Value>) -> Option<&str> {
    fn named(reach: &Value) -> Option<&str> {
        reach.pointer("/target/name")?.as_str()
    }
    if let Some(name) = machine.get("reach").and_then(named).or_else(|| {
        machine
            .get("readings")?
            .as_array()?
            .iter()
            .find(|reading| reading.get("key").and_then(Value::as_str) == Some("reach"))
            .and_then(|reading| reading.get("value"))
            .and_then(named)
    }) {
        return Some(name);
    }
    let reach = machine.get("reach")?;
    let focal = reach
        .get("members")?
        .as_array()?
        .iter()
        .find(|member| member.get("distance").and_then(Value::as_u64) == Some(1))?
        .get("via")?
        .as_str()?;
    let withheld = machine
        .get("readings")
        .and_then(Value::as_array)
        .and_then(|readings| {
            readings
                .iter()
                .find(|reading| reading.get("key").and_then(Value::as_str) == Some("reach"))
        })
        .and_then(|reading| reading.pointer("/value/frontier_withheld"));
    [
        reach.get("frontier"),
        reach.get("frontier_withheld"),
        withheld,
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_array)
    .flatten()
    .find(|row| row.get("to").and_then(Value::as_str) == Some(focal))
    .and_then(|row| row.get("to_name"))
    .and_then(Value::as_str)
}

/// The scope beside a walk or its paginated reach reading. Rich outer
/// scope metadata wins only when every carrier agrees on module and mode;
/// outside counts describe different populations and are not scope identity.
fn trace_scope(
    machine: &Map<String, Value>,
) -> Result<Option<std::borrow::Cow<'_, Map<String, Value>>>, &'static str> {
    let mut candidates = Vec::new();
    if let Some(scope) = machine.get(crate::trace_scope::SCOPE_KEY) {
        candidates.push(scope);
    }
    if let Some(readings) = machine.get("readings").and_then(Value::as_array) {
        for reading in readings {
            if reading.get("key").and_then(Value::as_str) == Some(crate::trace_scope::SCOPE_KEY) {
                candidates.push(
                    reading
                        .get("value")
                        .ok_or("the trace's module or path scope is missing")?,
                );
            }
        }
    }
    // Outer carriers describe the same scope block; unlike nested reach
    // metadata, their overlapping answer and coverage claims must agree.
    for (index, scope) in candidates.iter().enumerate() {
        for prior in &candidates[..index] {
            for field in ["answer", "state"] {
                if let (Some(left), Some(right)) = (scope.get(field), prior.get(field)) {
                    if left != right {
                        return Err("the trace carries conflicting scope answer or coverage, so its scope is not established");
                    }
                }
            }
        }
    }
    let outer_count = candidates.len();
    if let Some(scope) = machine.get("reach").and_then(|reach| reach.get("scope")) {
        candidates.push(scope);
    }
    if let Some(readings) = machine.get("readings").and_then(Value::as_array) {
        for reading in readings {
            if reading.get("key").and_then(Value::as_str) == Some("reach") {
                if let Some(scope) = reading.pointer("/value/scope") {
                    candidates.push(scope);
                }
            }
        }
    }
    let mut selected: Option<std::borrow::Cow<'_, Map<String, Value>>> = None;
    let mut identity = None;
    for (index, scope) in candidates.into_iter().enumerate() {
        let scope = scope
            .as_object()
            .ok_or("the trace's module or path scope is malformed")?;
        let module = scope
            .get("module")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or("the trace's module scope is missing or malformed")?;
        let mode = scope
            .get("mode")
            .and_then(Value::as_str)
            .filter(|mode| matches!(*mode, "paths" | "rows"))
            .ok_or("the trace's path scope is missing or malformed")?;
        if identity.is_some_and(|prior| prior != (module, mode)) {
            return Err("the trace carries conflicting module or path scope, so its scope is not established");
        }
        identity = Some((module, mode));
        match &mut selected {
            None => selected = Some(std::borrow::Cow::Borrowed(scope)),
            Some(selected) if index < outer_count => {
                // Fill sparse outer metadata without discarding a supplied
                // coverage bound or answer kind. Do not merge reach counts.
                for (key, value) in scope {
                    if !selected.contains_key(key) {
                        selected.to_mut().insert(key.clone(), value.clone());
                    }
                }
            }
            Some(_) => {}
        }
    }
    Ok(selected)
}

/// How far a package of a reach was read, in a few words, from its entry in
/// `reach.packages` ([`crate::trace_reach::PACKAGES_KEY`]).
pub(crate) fn reach_package_state(package: &Value) -> String {
    let module = package
        .get("module")
        .and_then(Value::as_str)
        .unwrap_or("its module");
    let count = |key: &str| package.get(key).and_then(Value::as_u64).unwrap_or(0);
    match package.get("state").and_then(Value::as_str) {
        Some("closed") => format!("complete inside {module}"),
        Some("open") => format!(
            "complete inside {module} except {}",
            counted(
                count("places") + count("held"),
                "listed place",
                "listed places"
            )
        ),
        Some("unread") => format!(
            "{} not read for calls Kin could not resolve",
            count("unread")
        ),
        _ => "cut by a work bound".to_string(),
    }
}

/// A reach that is not answered whole, said package by package after its
/// count, so a question about the callers in one package is answered by the
/// first line: `; by package: flask 7, complete inside flask; tests 108 in
/// test code, 102 not read for calls Kin could not resolve`. Empty when the
/// members sit in one package.
fn reach_packages_clause(machine: &Map<String, Value>) -> String {
    let Some(reach) = crate::trace_reach::answer_view(machine) else {
        return String::new();
    };
    let Some(packages) = reach
        .get(crate::trace_reach::PACKAGES_KEY)
        .and_then(Value::as_array)
        .filter(|packages| packages.len() > 1)
    else {
        return String::new();
    };
    let mut said: Vec<String> = packages
        .iter()
        .map(|package| {
            let count = |key: &str| package.get(key).and_then(Value::as_u64).unwrap_or(0);
            let mut entry = format!(
                "{} {}",
                package
                    .get("module")
                    .and_then(Value::as_str)
                    .unwrap_or("no module"),
                count("members")
            );
            if package.get("role").and_then(Value::as_str) == Some("test") {
                entry.push_str(" in test code");
            }
            entry.push_str(&format!(", {}", reach_package_state(package)));
            entry
        })
        .collect();
    if let Some(more) = reach
        .get(crate::trace_reach::PACKAGES_MORE_KEY)
        .and_then(Value::as_u64)
        .filter(|more| *more > 0)
    {
        said.push(counted(more, "more package", "more packages"));
    }
    format!("; by package: {}", said.join("; "))
}

/// How many members of an answered reach call the entity itself and how many
/// reach it through other calls, in brackets after the lead, when every
/// member row is here to count: `(3 call it directly, 6 through other calls)`.
/// Empty when a row is on a later page or states no distance.
fn reach_distances(machine: &Map<String, Value>, members: u64) -> String {
    let Some(reach) = crate::trace_reach::answer_view(machine) else {
        return String::new();
    };
    let Some(rows) = reach.get("members").and_then(Value::as_array) else {
        return String::new();
    };
    let distances: Vec<u64> = rows
        .iter()
        .filter_map(|row| row.get("distance").and_then(Value::as_u64))
        .collect();
    if distances.len() as u64 != members || rows.len() as u64 != members {
        return String::new();
    }
    let direct = distances.iter().filter(|distance| **distance == 1).count() as u64;
    let indirect = members - direct;
    let calls = |count: u64| if count == 1 { "calls" } else { "call" };
    match (direct, indirect) {
        (1, 0) => " (it calls it directly)".into(),
        (0, 1) => " (it calls it through another call)".into(),
        (_, 0) => " (all call it directly)".into(),
        (0, _) => " (all through other calls)".into(),
        _ => format!(
            " ({direct} {} it directly, {indirect} through other calls)",
            calls(direct)
        ),
    }
}

/// What a trace answer lists, in a few words: which way it walked, from what,
/// how far, and what each row shows.
///
/// A callers walk carries a reach reading, whose `members` are the entities
/// that can end up calling the focal, each with a proven path. That is the
/// answer, so the lead counts it. The chain is the walk that found them: it
/// can hold steps that are not callers (a class body that names a method), and
/// on a page it can be empty because its steps ride on a later page, so a lead
/// counted from the chain said "0 steps in all" beside four proven callers.
fn trace_lead(machine: &Map<String, Value>, envelope: &Value) -> String {
    trace_lead_as(machine, envelope, false)
}

/// [`trace_lead`], and for a callers reach that is `answered`, the lead that
/// says the walk is finished.
///
/// An agent that read "9 functions can end up calling it" took the nine for
/// the callers "at a shallow depth" and planned to trace callers again
/// "until I reach a fixpoint". The reach is that fixed point already: the
/// closure followed callers of callers until none was new. So an answered
/// reach says the members are every direct and indirect caller, in one
/// sentence, and how many of them call the entity itself. A partial reach
/// keeps the lead that claims no more than it found.
fn trace_lead_as(machine: &Map<String, Value>, envelope: &Value, answered: bool) -> String {
    let focal = machine
        .get("focal_name")
        .and_then(Value::as_str)
        .or_else(|| {
            machine
                .get("focal_entity")
                .and_then(|focal| focal.get("entity_name"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            machine
                .get("entity")
                .and_then(|focal| focal.get("name"))
                .and_then(Value::as_str)
        })
        .or_else(|| reach_focal_name(machine))
        .unwrap_or("the entity");
    let chain_len = machine
        .get("chain")
        .and_then(Value::as_array)
        .map_or(0, Vec::len) as u64;
    // A page counts its own steps; the whole walk's count rides on the page.
    let steps = envelope
        .pointer("/page/total_steps")
        .and_then(Value::as_u64)
        .map_or(chain_len, |total| total.max(chain_len));
    let depth = machine.get("depth").and_then(Value::as_u64);
    let members = reach_members(machine);
    // Only a callers walk takes a reach reading, so one present says which way
    // the walk went even on a page that does not repeat `direction`.
    let direction = machine
        .get("direction")
        .and_then(Value::as_str)
        .or(members.map(|_| "callers"));
    if let (Some("callers"), Some(members)) = (direction, members) {
        // A scoped closure is not cut at depth; it is held to its scope.
        let scoped = trace_scope(machine).ok().flatten().and_then(|scope| {
            let module = scope.get("module")?.as_str()?;
            Some((
                format!(" in {module}"),
                match scope.get("mode").and_then(Value::as_str) {
                    Some("rows") => String::new(),
                    _ => ", on paths that stay inside it".to_string(),
                },
            ))
        });
        let closure = crate::trace_reach::answer_view(machine).is_some();
        let (in_scope, within) = scoped.unwrap_or_else(|| {
            (
                String::new(),
                depth
                    .filter(|_| !closure)
                    .map(|depth| format!(" within {}", counted(depth, "call", "calls")))
                    .unwrap_or_default(),
            )
        });
        let mut lead = if members == 0 {
            format!(
                "no function{in_scope} can end up calling {focal} through a call Kin \
                 followed{within}"
            )
        } else if answered && closure {
            format!(
                "{} of {focal}{within}; Kin followed callers of callers until no new function \
                 appeared{}. reach.members lists {}, each with the line where it calls the next \
                 step toward {focal} (via)",
                if members == 1 {
                    format!("this function{in_scope} is the only direct or indirect caller")
                } else {
                    format!(
                        "these {members} functions{in_scope} are every direct and indirect caller"
                    )
                },
                reach_distances(machine, members),
                if members == 1 { "it" } else { "every one" }
            )
        } else {
            format!(
                "{}{in_scope} can end up calling {focal}{within}{}. reach.members lists {}, each \
                 with the line where it calls the next step toward {focal} (via)",
                counted(members, "function", "functions"),
                reach_packages_clause(machine),
                if members == 1 { "it" } else { "every one" }
            )
        };
        if chain_len > 0 && !closure {
            lead.push_str(&format!(
                "; the chain shows the same walk in {}, each naming its parent_step",
                counted(steps, "step", "steps")
            ));
        }
        return lead;
    }
    let walked = match direction {
        Some("callers") => format!("every caller of {focal}"),
        Some("calls") => format!("everything {focal} calls"),
        _ => format!("what calls {focal} and what it calls"),
    };
    let quotes = |step: &Value| {
        step.get(crate::site_expression::TRACE_REFERENCE_SITES_KEY)
            .and_then(Value::as_array)
            .is_some_and(|sites| sites.iter().any(|site| site.get("expr").is_some()))
    };
    // The steps that link to a parent, which are the ones a line could show.
    let linked: Vec<&Value> = machine
        .get("chain")
        .and_then(Value::as_array)
        .map(|chain| {
            chain
                .iter()
                .filter(|step| {
                    step.get("parent_step")
                        .is_some_and(|parent| !parent.is_null())
                })
                .collect()
        })
        .unwrap_or_default();
    let quoted = linked.iter().filter(|step| quotes(step)).count() as u64;
    let bare = linked.len() as u64 - quoted;
    let mut lead = match depth {
        Some(depth) => format!(
            "this chain lists {walked} up to {}, {} in all",
            counted(depth, "call away", "calls away"),
            counted(steps, "step", "steps")
        ),
        None => format!(
            "this chain lists {walked}, {} in all",
            counted(steps, "step", "steps")
        ),
    };
    if quoted > 0 && bare == 0 {
        lead.push_str(
            ". Each step names its parent_step and shows the line of code that links the two",
        );
    } else if quoted > 0 {
        lead.push_str(&format!(
            ". Each step names its parent_step and shows the line of code that links the two, except {} whose line Kin did not record, such as a type used in a signature",
            counted(bare, "step", "steps")
        ));
    }
    lead
}

/// Why a trace cannot claim every caller, apart from a response size cap:
/// the walk's depth or per-step limit, unchecked calls at an end of the walk,
/// or calls the reach reading did not follow. A type use excluded from a
/// callers walk is not one of those gaps.
pub(crate) fn trace_limits(machine: &Map<String, Value>) -> Vec<String> {
    let mut limits = Vec::new();
    // A scoped callers walk answers with its closure, which is held to the
    // scope rather than cut at the chain's depth or per-step cap, so only the
    // closure's own frontier and bounds limit it.
    let scope = match trace_scope(machine) {
        Ok(scope) => scope,
        Err(reason) => {
            limits.push(reason.to_string());
            None
        }
    };
    let reach_answer = crate::trace_reach::answer_view(machine);
    let closure_answers = reach_answer.is_some()
        || scope
            .as_deref()
            .and_then(|scope| scope.get("answer"))
            .and_then(Value::as_str)
            == Some("reach");
    if closure_answers {
        trace_reach_limits(machine, &mut limits);
        if reach_answer.as_ref().is_some_and(|reach| {
            reach.get("state").and_then(Value::as_str) == Some("bounded")
                || reach
                    .get("bounds")
                    .and_then(Value::as_array)
                    .is_some_and(|bounds| !bounds.is_empty())
        }) || scope
            .as_deref()
            .and_then(|scope| scope.get("state"))
            .and_then(Value::as_str)
            == Some("bounded")
        {
            limits.push(
                "a work bound or the reply's size stopped the reach reading, as \
                 reach.bounds says"
                    .to_string(),
            );
        }
        if let Some(reach) = &reach_answer {
            let folded = reach
                .get(crate::trace_reach::FRONTIER_FOLDED_KEY)
                .and_then(|folded| folded.get("count"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if folded > 0 {
                limits.push(format!("{folded} unresolved observations are counted by reason in reach.frontier_folded; unread code or other unverified calls could add callers"));
            }
            let total = reach
                .get("members_total")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let returned = reach
                .get("members")
                .and_then(Value::as_array)
                .map_or(0, Vec::len) as u64;
            if returned != total {
                limits.push(
                    "not all recorded reach members are on this page; continue with next_cursor"
                        .into(),
                );
            }
            if reach.get("state").and_then(Value::as_str) == Some("open")
                && folded == 0
                && reach.get("frontier_total").and_then(Value::as_u64) == Some(0)
            {
                limits.push("the reach reading remains open; its unresolved coverage is not fully described".into());
            }
        }
        return limits;
    }
    let depth = machine.get("depth").and_then(Value::as_u64).unwrap_or(1);
    let at_bound = machine
        .get("terminal_bound_steps")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if at_bound > 0 {
        limits.push(format!(
            "the walk stopped at depth {depth}, where {} further; ask again with a larger depth to follow {}",
            if at_bound == 1 {
                "1 step leads".to_string()
            } else {
                format!("{at_bound} steps lead")
            },
            if at_bound == 1 { "it" } else { "them" }
        ));
    }
    let crowded = machine
        .get("chain")
        .and_then(Value::as_array)
        .is_some_and(|chain| {
            chain
                .iter()
                .any(|step| step.get("fanout_truncated") == Some(&Value::Bool(true)))
        });
    if crowded {
        let per_step = machine.get("limit_per_step").and_then(Value::as_u64);
        limits.push(match per_step {
            Some(limit) => format!("some display steps have more links than the {limit} per step the walk keeps; the limit_per_step ceiling is {}", crate::remediation::TRACE_MAX_LIMIT_PER_STEP),
            None => "some display steps exceeded the configured limit_per_step; this page does not include the effective limit".into(),
        });
    }
    let coverage_gaps = machine
        .get("terminal_coverage_gap_steps")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        + u64::from(machine.get("focal_terminal").and_then(Value::as_str) == Some("coverage_gap"));
    if coverage_gaps > 0 {
        limits.push(format!(
            "Kin could not confirm every call at {} of the walk",
            counted(coverage_gaps, "end", "ends")
        ));
    }
    trace_reach_limits(machine, &mut limits);
    // Neighbours a scope could not place were not followed, and whether they
    // are in scope is unknown.
    let unplaced = scope
        .as_deref()
        .and_then(|scope| scope.get("frontier"))
        .and_then(Value::as_array)
        .map_or(0, |rows| {
            rows.iter()
                .filter(|row| row.get("kind").and_then(Value::as_str) == Some("no_module"))
                .count()
        }) as u64;
    if unplaced > 0 {
        limits.push(format!(
            "{} no module record places {} not followed, as scope.frontier lists",
            counted(unplaced, "neighbour", "neighbours"),
            if unplaced == 1 { "was" } else { "were" },
        ));
    }
    limits
}

/// The places a trace's reach reading did not follow, as a limit.
fn trace_reach_limits(machine: &Map<String, Value>, limits: &mut Vec<String>) {
    // A page can carry the reach reading under `readings` rather than beside
    // its chain. The same open places qualify either form of the answer.
    let reach = machine.get("reach").or_else(|| {
        machine
            .get("readings")?
            .as_array()?
            .iter()
            .find(|reading| reading.get("key").and_then(Value::as_str) == Some("reach"))?
            .get("value")
    });
    let frontier = reach
        .and_then(|reach| reach.get("frontier_total"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if frontier > 0 {
        limits.push(format!(
            "{} where calls Kin did not follow could add callers {} described in reach.frontier or reach.frontier_withheld",
            counted(frontier, "place", "places"),
            if frontier == 1 { "is" } else { "are" },
        ));
    }
}

/// The sentence for the places a callers walk folded into counts by reason,
/// when it folded any: how many, where they are counted, and that each could
/// matter only through the mechanism its reason names.
fn trace_folded_sentence(machine: &Map<String, Value>) -> Option<String> {
    let reach = machine.get("reach").or_else(|| {
        machine
            .get("readings")?
            .as_array()?
            .iter()
            .find(|reading| reading.get("key").and_then(Value::as_str) == Some("reach"))?
            .get("value")
    })?;
    let folded = reach
        .pointer(&format!(
            "/{}/count",
            crate::trace_reach::FRONTIER_FOLDED_KEY
        ))
        .and_then(Value::as_u64)
        .filter(|count| *count > 0)?;
    let scope = reach
        .get(crate::trace_reach::FRONTIER_FOLDED_KEY)
        .and_then(|folded| folded.get("scope"));
    let scope_note = match scope.and_then(|scope| scope.get("mode")).and_then(Value::as_str) {
        Some("paths") => scope
            .and_then(|scope| scope.get("module"))
            .and_then(Value::as_str)
            .map(|module| format!(" This count is after the {module} path filter: known outside callers were excluded; callers with unknown placement remain."))
            .unwrap_or_default(),
        Some("repository") => " These observations were counted across the repository.".to_string(),
        _ => String::new(),
    };
    Some(format!(
        "{folded} {} counted by reason in reach.{}, not listed; each could add a caller only through the mechanism its reason names.{scope_note}",
        plural(folded, "observation is", "observations are"),
        crate::trace_reach::FRONTIER_FOLDED_KEY,
    ))
}

/// Up to six names, quoted and joined, then how many more.
fn named_list(names: &[String]) -> String {
    const SHOWN: usize = 6;
    let quoted: Vec<String> = names
        .iter()
        .take(SHOWN)
        .map(|name| format!("`{name}`"))
        .collect();
    match (quoted.split_last(), names.len().saturating_sub(SHOWN)) {
        (None, _) => String::new(),
        (Some((only, [])), 0) => only.clone(),
        (Some((last, rest)), 0) => format!("{} and {last}", rest.join(", ")),
        (Some(_), more) => format!("{} and {more} more", quoted.join(", ")),
    }
}

/// The places [`trace_value_places`] names, as a sentence, or `None` when
/// there are none.
fn trace_value_places_sentence(places: &ValuePlaces) -> Option<String> {
    let rows = (!places.holders.is_empty()).then(|| {
        format!(
            "{} (reach.also_referenced_by, each ref with value)",
            named_list(&places.holders)
        )
    });
    let members = (!places.members.is_empty()).then(|| {
        format!(
            "inside {} (reach.blind_spots.value_holders.in_members)",
            named_list(&places.members)
        )
    });
    let named = match (rows, members) {
        (None, None) => return None,
        (Some(one), None) | (None, Some(one)) => one,
        (Some(rows), Some(members)) => format!("{rows}, and {members}"),
    };
    Some(format!(
        "A call through a value can reach these entities only where one is held as a value: {named}."
    ))
}

/// How the listed frontier places of a reach reading sit in test code, as a
/// clause after the count, or empty when none do.
fn trace_frontier_tests_clause(machine: &Map<String, Value>, places: u64) -> String {
    let tests = crate::trace_reach::answer_view(machine)
        .and_then(|reach| {
            reach
                .get(crate::trace_reach::FRONTIER_IN_TESTS_KEY)
                .and_then(Value::as_u64)
        })
        .unwrap_or(0);
    match tests {
        0 => String::new(),
        _ if tests == places => {
            if places == 1 {
                " (it sits in test code: in_role test)".to_string()
            } else {
                " (all of them sit in test code: in_role test)".to_string()
            }
        }
        _ => format!(" ({tests} of them sit in test code: in_role test)"),
    }
}

/// How a page of a callers walk stands against the reach rows that answer
/// it, read from the boundary the frozen walk publishes on every page
/// (`_kin.page.total_answer_records`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TracePage {
    /// This page and the ones before it carry every reach row, and only the
    /// display chain and the walk's evidence remain.
    AnswerDelivered,
    /// A page past every reach row: display chain and evidence, no member.
    Display,
}

fn trace_page(machine: &Map<String, Value>, envelope: &Value) -> Option<TracePage> {
    let page = envelope.get("page")?;
    let answer_end = page
        .get("total_answer_records")
        .and_then(Value::as_u64)
        .filter(|end| *end > 0)?;
    let offset = page.get("record_offset").and_then(Value::as_u64)?;
    let carries_reach = machine.contains_key(crate::trace_reach::REACH_KEY)
        || machine
            .get("readings")
            .and_then(Value::as_array)
            .is_some_and(|readings| {
                readings.iter().any(|reading| {
                    reading.get("key").and_then(Value::as_str)
                        == Some(crate::trace_reach::REACH_KEY)
                })
            });
    if offset >= answer_end && !carries_reach {
        return Some(TracePage::Display);
    }
    (page.get("answer_rows_complete").and_then(Value::as_bool) == Some(true)
        && page.get("has_more").and_then(Value::as_bool) == Some(true))
    .then_some(TracePage::AnswerDelivered)
}

/// The name of the entity an answer is about, wherever the machine reply put
/// it.
fn subject_name(machine: &Map<String, Value>) -> Option<&str> {
    machine
        .get("name")
        .or_else(|| {
            machine
                .get("focal_entity")
                .and_then(|focal| focal.get("name"))
        })
        .or_else(|| {
            machine
                .get("focal_entity")
                .and_then(|focal| focal.get("entity_name"))
        })
        .or_else(|| machine.get("focal_name"))
        .and_then(Value::as_str)
}

/// What a complete answer from `tool` is, in a few words.
fn complete_lead(tool: &str, machine: &Map<String, Value>, envelope: &Value) -> String {
    let name = subject_name(machine);
    match (tool, name) {
        (SOURCE_TOOL, name)
            if machine.get("body_omitted").and_then(Value::as_str)
                == Some(crate::container_shape::CONTAINER_SHAPE_REASON) =>
        {
            let members = machine
                .get("members")
                .and_then(Value::as_array)
                .map_or(0, Vec::len) as u64;
            let name = name.unwrap_or("this entity");
            let header = if machine.get("header").is_some_and(Value::is_string) {
                "its header and "
            } else {
                ""
            };
            format!(
                "this is the outline of {name}: {header}its {}, each with its entity_id{}. Read the one you need with source and its entity_id",
                counted(members, "member", "members"),
                if members > 0 { " and signature" } else { "" }
            )
        }
        (SOURCE_TOOL, name) if crate::module_code::is_module_code_reply(machine) => {
            let unnamed = if crate::module_code::is_class_code_reply(machine) {
                "this class"
            } else {
                "this module"
            };
            crate::module_code::plain_lead(name.unwrap_or(unnamed), machine)
        }
        (SOURCE_TOOL, name) if crate::artifact_entity::is_artifact_reply(machine) => {
            crate::artifact_entity::plain_lead(name.unwrap_or("this artifact"), machine)
        }
        (SOURCE_TOOL, Some(name)) => format!("this is the exact current source of {name}"),
        ("get_context_pack", Some(name)) => {
            // "then", not "followed by": the plain-word check reads any word
            // holding "owed" as the internal debt term.
            format!("this is the context around {name}: its code first, then related context")
        }
        ("get_context_pack", None) => {
            // A context asked for several entities names each one it is around.
            let names: Vec<&str> = machine
                .get("focals")
                .and_then(Value::as_array)
                .map(|focals| {
                    focals
                        .iter()
                        .filter_map(|focal| focal.get("name").and_then(Value::as_str))
                        .collect()
                })
                .unwrap_or_default();
            let around = match names.as_slice() {
                [] => "the entities asked about".to_string(),
                [one] => (*one).to_string(),
                [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
            };
            format!("this is the context around {around}: the code asked about first, then related context")
        }
        (TRACE_TOOL, _) => trace_lead(machine, envelope),
        ("graph_neighborhood", _) => neighborhood_lead(machine),
        ("impact_analysis", _) => "these are the entities a change to it could affect".to_string(),
        _ => "this is the whole answer".to_string(),
    }
}

/// The scope statement for code that builds names while it runs, when the
/// reply counted such places without saying how many.
const UNKEYED_SCOPE: &str = "Code that builds a name while it runs is outside what any reader \
     of code can follow.";

/// What one renderer says about its answer, before the `_kin` block is built.
struct Said {
    word: Option<&'static str>,
    meaning: String,
    delivered: Option<String>,
    searched: Option<String>,
    fix: Option<String>,
}

impl Said {
    fn into_envelope(self) -> Value {
        plain_envelope_delivered(
            self.word,
            self.meaning,
            self.delivered,
            self.searched,
            self.fix.as_deref(),
        )
    }
}

/// Scope sentences joined into one `searched` line, or `None`.
fn joined(parts: Vec<String>) -> Option<String> {
    let parts: Vec<String> = parts.into_iter().filter(|part| !part.is_empty()).collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Any other answer, plainly: the verdict for what this operation did, and the
/// answer without its working.
fn plain_other(
    machine: Map<String, Value>,
    envelope: &Value,
    tool: &str,
    details: bool,
    account: &crate::answer_account::Account,
) -> Value {
    if machine.contains_key(crate::file_coordinate_query::KEY)
        && crate::file_coordinate_query::recognizes(tool, &Value::Object(machine.clone()))
    {
        return plain_coordinate_lookup(machine, envelope, account);
    }
    let op = Operation::of(tool);
    match op {
        Operation::NameSearch if machine.get("results").is_some_and(Value::is_array) => {
            plain_name_search(machine, envelope, details, account)
        }
        Operation::Ranked if machine.get("entities").is_some_and(Value::is_array) => {
            plain_ranked(machine, envelope, account)
        }
        Operation::Source if is_source_reply(&machine) => {
            plain_source(machine, envelope, tool, account)
        }
        Operation::History if machine.get("changes").is_some_and(Value::is_array) => {
            plain_history(machine, envelope, account)
        }
        _ => plain_generic(machine, envelope, tool, op, account),
    }
}

// ---------------------------------------------------------------------------
// Name search
// ---------------------------------------------------------------------------

/// A coordinate lookup says only which adapter surfaces the graph records.
/// Missing extraction is a graph gap, never a filesystem absence certificate.
fn plain_coordinate_lookup(
    mut machine: Map<String, Value>,
    envelope: &Value,
    account: &crate::answer_account::Account,
) -> Value {
    let block = machine
        .remove(crate::file_coordinate_query::KEY)
        .expect("checked coordinate block");
    let total = block["matching_surfaces"]
        .as_u64()
        .expect("checked coordinate count");
    let rows: Vec<_> = machine
        .remove("results")
        .and_then(|rows| rows.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .map(|mut row| {
            // Retained compatibility payloads can still carry generated
            // location-based module headers. Project before dropping origin
            // metadata, while the exact synthetic header is recognizable.
            crate::semantic_surface::project("semantic_search", &mut row);
            plain_row(row)
        })
        .collect();
    let verdict = MachineVerdict::read(envelope);
    let partial = account.standing == AnswerStanding::Partial;
    let mut meaning = match block["state"].as_str() {
        Some("module_unrecorded") => "Partial: the selected inventory has no recorded module entity; extraction is a semantic gap.".to_string(),
        Some("coordinate_unrecorded") => "Partial: the selected inventory has no recorded module identity.".to_string(),
        Some("filtered_out") => format!(
            "{}: the selected inventory has {} recorded module entities, but none matches the requested kind, language or role filters.",
            if partial { "Partial" } else { "Answer" }, block["recorded_surfaces"]),
        _ => format!(
            "{}: {} the {total} module {} recorded for this selection. Read a returned entity_id with source for its recorded member inventory.",
            if partial { "Partial" } else { "Answer" },
            if total == 1 { "this is" } else { "these are" },
            if total == 1 { "entity" } else { "entities" }),
    };
    if !account.open.is_empty() {
        meaning.push_str(&format!(
            " This lookup remains qualified because {}.",
            verdict.open_reasons(Operation::Coordinate).join("; ")
        ));
    }
    meaning.push_str(" This lookup does not establish that every declaration was parsed.");
    let delivered = (total > rows.len() as u64).then(|| format!(
        "Showing {} of {total} matching surfaces; repeat this query with a larger limit to see the rest.", rows.len()));
    let kin = Said {
        word: account.word(),
        meaning,
        delivered,
        searched: Some("Looked up recorded module entities.".to_string()),
        fix: verdict.remedy,
    }
    .into_envelope();
    json!({"_kin":kin,"results":rows,"total":total})
}

/// One declaration the name matched that Kin keeps no entity for, plainly:
/// its name and kind, the dotted module it sits in, the entity that encloses
/// it (by name and, when the graph names exactly one, by entity_id), and why.
/// A top-level assignment that only re-exports another module's member says
/// so. Never a path.
fn plain_not_held_row(row: &Value) -> Value {
    let mut plain = Map::new();
    for key in ["name", "kind", "module"] {
        if let Some(value) = row.get(key).filter(|value| !value.is_null()) {
            plain.insert(key.into(), value.clone());
        }
    }
    let owner = row.get("owner").and_then(Value::as_str);
    if let Some(owner) = owner {
        plain.insert("inside".into(), json!(owner));
        if let Some(id) = row.get("inside_id").filter(|id| id.is_string()) {
            plain.insert("inside_id".into(), id.clone());
        }
    }
    let why = match row.get("reexport_of").and_then(Value::as_str) {
        Some(source) => format!("re-export of `{source}`, a top-level assignment Kin does not keep as an entity of its own"),
        None => not_held_why(
            row.get("reason").and_then(Value::as_str).unwrap_or_default(),
            owner,
        ),
    };
    plain.insert("why".into(), json!(why));
    Value::Object(plain)
}

/// Why a declaration the name matched is not an entity Kin holds, and where a
/// reader goes instead.
fn not_held_why(reason: &str, owner: Option<&str>) -> String {
    match reason {
        "nested_in_callable" => format!(
            "declared inside the body of {}; read that body with source",
            owner.unwrap_or("a function")
        ),
        "in_module_block" => "declared inside a module-level block, such as an \
             if or try block, whose declarations Kin has not admitted as entities"
            .to_string(),
        "class_member_not_admitted" => format!(
            "an attribute of {}, not an entity of its own; read the class with source",
            owner.unwrap_or("its class")
        ),
        "object_literal_member" => {
            "a member of an object literal, not an entity of its own".to_string()
        }
        "module_binding_filtered" => {
            "a top-level assignment Kin does not keep as an entity of its own".to_string()
        }
        _ => "a declaration Kin does not keep as an entity of its own".to_string(),
    }
}

/// A name search, plainly: complete when the declaration search finished over
/// every declaration in its scope, with each declaration the name matched that
/// Kin holds no entity for listed as what the answer is complete except. It
/// never says anything about uses, which a name search does not look at.
fn plain_name_search(
    mut machine: Map<String, Value>,
    envelope: &Value,
    details: bool,
    account: &crate::answer_account::Account,
) -> Value {
    let op = Operation::NameSearch;
    let verdict = MachineVerdict::read(envelope);
    let query = machine
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("this name")
        .to_string();
    let rows: Vec<Value> = match machine.remove("results") {
        Some(Value::Array(rows)) => rows.into_iter().map(plain_row).collect(),
        _ => Vec::new(),
    };
    let NameSearchAccount {
        total,
        not_held,
        folded_attributes,
        folded_total,
        not_held_total,
        closure,
        artifacts,
    } = NameSearchAccount::read(&machine, &verdict, &rows, details, plain_not_held_row);

    let named = |count: u64| {
        let declarations = count.saturating_sub(artifacts);
        match (declarations, artifacts) {
            (_, 0) if count == 1 => {
                format!("this is the 1 declaration Kin holds whose name matches {query}")
            }
            (_, 0) => {
                format!("these are the {count} declarations Kin holds whose name matches {query}")
            }
            (0, 1) => {
                format!("this is the 1 unparsed entity Kin holds whose name matches {query}")
            }
            (0, _) => format!(
                "these are the {artifacts} unparsed entities Kin holds whose name matches {query}"
            ),
            _ => format!(
                "these are the {count} entities Kin holds whose name matches {query}: {} and {}",
                counted(declarations, "declaration", "declarations"),
                counted(artifacts, "unparsed entity", "unparsed entities"),
            ),
        }
    };
    let found = || {
        if total == 0 {
            format!("no declaration Kin holds is named {query}")
        } else {
            named(total)
        }
    };
    // Source Kin could not read could hold another declaration: the reply
    // says what it holds as it would otherwise, led as partial, and what
    // could hold another is said below, place by place.
    let unread = account.basis == Basis::NameUnread;
    let mut meaning = match account.basis {
        Basis::NameNotHeldOnly | Basis::NameUnread if total == 0 && not_held_total > 0 => {
            let mut text = format!(
                "Answer: no entity Kin holds is named {query}, but {} named like it {} where Kin keeps no entity; not_held lists {}. Kin cannot read {} on its own; locate with mode literal finds recorded code that mentions it.",
                counted(not_held_total, "declaration", "declarations"),
                plural(not_held_total, "sits", "sit"),
                listed_of(not_held.len() as u64, not_held_total),
                plural(not_held_total, "it", "one"),
            );
            if let Some(all) = closure.stated(&query, 0, not_held.len() as u64, folded_total) {
                text.push(' ');
                text.push_str(&all);
            }
            text
        }
        Basis::NameNotHeldOnly | Basis::NameNothing => format!(
            "Answer: no declaration Kin holds is named {query}. Kin holds this checkout's current code, so a {query} you remember from another version of this project is not in it. To search by what the code does, call locate with mode description; to find the text inside code, call locate with mode literal."
        ),
        Basis::NameUnread if total == 0 => {
            format!("Partial: {}, and there may be more.", found())
        }
        Basis::NameRows | Basis::NameUnread => {
            // Every declaration of the name is in this reply, so the answer is
            // that: a reader who is told only what Kin holds asks next whether
            // another exists, which Kin had already settled.
            let stated = closure.stated(
                &query,
                rows.len() as u64,
                not_held.len() as u64,
                folded_total,
            );
            // A reply of nothing but declarations named exactly so leads with
            // the claim. One that also holds longer names says first what its
            // rows are, then which of them the claim is about.
            let leads = stated.is_some() && closure.named_exactly == closure.in_reply;
            let mut text = match &stated {
                Some(all) if leads => format!("Answer: {}", uncapitalized(all)),
                _ => format!("Answer: {}.", named(total)),
            };
            if not_held_total > 0 && leads {
                text.push_str(&format!(
                    " Kin keeps no entity for the {} under not_held.",
                    plural(not_held_total, "one", "ones"),
                ));
            } else if not_held_total > 0 {
                text.push_str(&format!(
                    " {} more named like it {} where Kin keeps no entity; not_held lists {}.",
                    counted(not_held_total, "declaration", "declarations"),
                    plural(not_held_total, "sits", "sit"),
                    listed_of(not_held.len() as u64, not_held_total),
                ));
            }
            if let (Some(all), false) = (&stated, leads) {
                text.push(' ');
                text.push_str(all);
            }
            text
        }
        _ => format!(
            "Partial: {}, and there may be more, because {}.",
            found(),
            verdict.open_reasons(op).join("; ")
        ),
    };
    if unread {
        if let Some(rest) = meaning.strip_prefix("Answer: ") {
            meaning = format!("Partial: {rest}");
        }
    }
    // Attributes counted rather than listed leave a whole answer complete
    // except that count; they are never an absence of the name.
    if folded_total > 0 && account.standing != AnswerStanding::Partial {
        meaning.push_str(&format!(
            " {folded_total} {} named like it {} counted in not_held_attributes by class, not listed; one matters only as a value its class holds.",
            plural(folded_total, "class attribute", "class attributes"),
            plural(folded_total, "is", "are"),
        ));
    }
    // A name asked for in the other member spelling is said as a respelling,
    // never read as the name the caller wrote.
    let respelled_from = machine
        .get(crate::handlers::entities::RESPELLED_FROM_KEY)
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(written) = &respelled_from {
        meaning.push_str(&format!(
            " Nothing is named {written} as written; Kin read it as {query}, the same member \
             written with the other separator."
        ));
    }
    // What could hold a declaration of the name that this reply does not: said
    // by what it is, so a reader knows the answer is not the whole set and why.
    for open in closure.open_places(&query) {
        meaning.push(' ');
        meaning.push_str(&open);
    }
    // Rows short of the limit were cut to fit the reply, which a larger
    // limit does not change: measured, a reader raised the limit from 50 to
    // 200 and was shown the same ten rows with the same advice.
    let asked_limit = machine.get("limit").and_then(Value::as_u64);
    let delivered = (total > rows.len() as u64).then(|| {
        if asked_limit.is_some_and(|limit| (rows.len() as u64) < limit) {
            format!(
                "Showing {} of the {total}, the closest matches first; the rest did not fit in this reply. Ask again with a larger {}, up to 60000, or a narrower query such as module.name.",
                rows.len(),
                crate::request_schema::OUTPUT_BUDGET
            )
        } else {
            format!(
                "Showing {} of the {total}; ask again with a larger limit to see the rest.",
                rows.len()
            )
        }
    });
    let searched = if artifacts > 0 {
        "Searched recorded declarations and their semantic names, and the names of the \
         entities Kin holds without parsing them."
            .to_string()
    } else {
        "Searched recorded declarations and their semantic names.".to_string()
    };
    let mut plain = Map::new();
    plain.insert(
        crate::envelope::ENVELOPE_KEY.into(),
        Said {
            word: account.word(),
            meaning,
            delivered,
            searched: Some(searched),
            fix: verdict.remedy.clone(),
        }
        .into_envelope(),
    );
    plain.insert("query".into(), json!(query));
    if let Some(written) = respelled_from {
        plain.insert(
            crate::handlers::entities::RESPELLED_FROM_KEY.into(),
            json!(written),
        );
    }
    plain.insert("results".into(), json!(rows));
    if total > 0 {
        plain.insert("total".into(), json!(total));
    }
    if !not_held.is_empty() {
        plain.insert("not_held".into(), json!(not_held));
    }
    if folded_total > 0 {
        plain.insert(
            NOT_HELD_ATTRIBUTES_KEY.into(),
            json!({
                "count": folded_total,
                "by_class": folded_attributes,
                "why": "Attributes of classes Kin keeps, named like the query: each is a value its class holds, not a declaration of its own. To list them, repeat this request with details:true.",
            }),
        );
    }
    Value::Object(plain)
}

/// Where a short name search counts the class attributes it folded.
pub(crate) const NOT_HELD_ATTRIBUTES_KEY: &str = "not_held_attributes";

/// `text` with its first letter in lower case, to follow `Answer:`.
fn uncapitalized(text: &str) -> String {
    let mut letters = text.chars();
    match letters.next() {
        Some(first) => first.to_lowercase().chain(letters).collect(),
        None => String::new(),
    }
}

/// How a sentence refers to the declarations `not_held` lists: all of them,
/// or how many of the total when the census listed only its first ones.
fn listed_of(listed: u64, total: u64) -> String {
    if listed < total {
        format!("{listed} of them")
    } else {
        plural(total, "it", "them").to_string()
    }
}

impl NameClosure {
    /// The sentence saying this reply holds every declaration named `query`,
    /// or `None` when it cannot say so. `shown` rows, `listed` declarations
    /// under `not_held` and `counted_apart` class attributes are what the
    /// reply holds.
    ///
    /// A name selects every declaration whose name holds it, so a reply can
    /// hold longer names beside the one asked for. The claim is about the
    /// declarations named exactly so: all of the reply, some of it or none.
    fn stated(&self, query: &str, shown: u64, listed: u64, counted_apart: u64) -> Option<String> {
        if !self.closed || self.in_reply == 0 {
            return None;
        }
        let mut read = match self.declarations_read {
            Some(read) => format!("Kin read all {read} declarations in its source"),
            None => "Kin read every declaration in its source".to_string(),
        };
        // Source parsed around a syntax error was read back where the parse
        // could not read it, which is what lets the claim stand over it.
        let unread = self.partly_parsed.then(|| match self.partly_parsed_count {
            Some(1) => "the name is spelled nowhere in the part of 1 source unit it could not parse".to_string(),
            Some(count) => format!("the name is spelled nowhere in the parts of {count} source units it could not parse"),
            None => "the name is spelled nowhere in what it could not parse".to_string(),
        });
        if self.named_exactly == 0 {
            if let Some(unread) = unread {
                read = format!("{read}, and {unread}");
            }
            return Some(format!(
                "None here is named exactly {query}, and no declaration in this repository is: {read}."
            ));
        }
        if let Some(unread) = unread {
            read = format!("{read}, {unread}");
        }
        let (subject, count) = if self.named_exactly == self.in_reply {
            let mut beside: Vec<String> = Vec::new();
            if listed > 0 {
                beside.push(format!("the {listed} listed under not_held"));
            }
            if counted_apart > 0 {
                beside.push(format!(
                    "the {counted_apart} counted in {NOT_HELD_ATTRIBUTES_KEY}"
                ));
            }
            let beside = beside.join(" and ");
            let subject = match (shown, beside.is_empty()) {
                (0, _) => capitalized(&beside),
                (1, true) => "This one".to_string(),
                (_, true) => format!("These {shown}"),
                (1, false) => format!("This one, with {beside},"),
                (_, false) => format!("These {shown}, with {beside},"),
            };
            (subject, self.in_reply)
        } else {
            (
                format!("The {} named exactly {query} here", self.named_exactly),
                self.named_exactly,
            )
        };
        Some(format!(
            "{subject} {} named {query} in this repository: {read}, and no other has that name.",
            plural(count, "is the only declaration", "are every declaration"),
        ))
    }

    /// What could hold a declaration named `query` that this reply does not,
    /// each as one sentence. Empty when the reply is closed, and when nothing
    /// the census read says where another could be.
    fn open_places(&self, query: &str) -> Vec<String> {
        use crate::declaration_coverage as coverage;
        let mut open = Vec::new();
        if self.closed {
            return open;
        }
        if self.unmeasured {
            open.push(format!(
                "Kin has no current record of the declarations in part of this repository's source, so another {query} could be declared there."
            ));
        }
        if self.unparsed {
            open.push(format!(
                "Part of this repository's source is held as bytes and was never parsed, so another {query} could be declared there."
            ));
        }
        let unread = self.unread_regions.as_deref();
        if self.partly_parsed && unread == Some(coverage::UNREAD_SPELL_NAME) {
            // Where another could hide, by module: the reader's next look.
            const NAMED: usize = 6;
            let named: Vec<&str> = self
                .spelled_in
                .iter()
                .take(NAMED)
                .map(String::as_str)
                .collect();
            let more = self.spelled_in_count.saturating_sub(named.len() as u64);
            let places = match (named.is_empty(), more) {
                (true, _) => counted(self.spelled_in_count, "source unit", "source units"),
                (false, 0) => named.join(", "),
                (false, more) => format!("{} and {more} more", named.join(", ")),
            };
            open.push(format!(
                "{query} is spelled inside a part of {places} that Kin could not parse, so another declaration of it could be there."
            ));
        } else if self.partly_parsed && unread == Some(coverage::UNREAD_NOT_READ) {
            // A census that did not count them still names them.
            let units = match self.not_read_back {
                0 => "the source units".to_string(),
                count => counted(count, "source unit", "source units"),
            };
            open.push(format!(
                "Kin could not read back {units} it parsed only in part, so a declaration named {query} inside what it could not parse there would not be in this answer.",
            ));
        } else if self.partly_parsed && unread.is_none() {
            let which = match self.partly_parsed_count {
                Some(1) => "1 of this repository's source units was".to_string(),
                Some(count) => format!("{count} of this repository's source units were"),
                None => "Part of this repository's source was".to_string(),
            };
            open.push(format!(
                "{which} parsed around a syntax error, so a declaration named {query} inside what Kin could not read there would not be in this answer."
            ));
        }
        if self.unlisted > 0 {
            open.push(format!(
                "{} named like it that Kin keeps no entity for {} not listed, so another {query} can be among them; a narrower name such as module.name lists fewer.",
                counted(self.unlisted, "declaration", "declarations"),
                plural(self.unlisted, "is", "are"),
            ));
        }
        open
    }
}

// ---------------------------------------------------------------------------
// Ranked search
// ---------------------------------------------------------------------------

/// A ranking against a description, plainly: these are the ranked results,
/// best first, and never a claim that every relevant entity was found. What
/// the ranking could rank on is its scope.
fn plain_ranked(
    mut machine: Map<String, Value>,
    envelope: &Value,
    account: &crate::answer_account::Account,
) -> Value {
    let op = Operation::Ranked;
    let verdict = MachineVerdict::read(envelope);
    let query = machine
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("the description")
        .to_string();
    let rows: Vec<Value> = match machine.remove("entities") {
        Some(Value::Array(rows)) => rows,
        _ => Vec::new(),
    };
    // One way of matching shared by every row is said once, as scope.
    let shared_match = rows
        .first()
        .and_then(|row| row.get("matched"))
        .filter(|matched| rows.iter().all(|row| row.get("matched") == Some(matched)))
        .cloned();
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            let mut row = plain_row(row);
            if let Some(map) = row.as_object_mut() {
                map.remove("score");
                if shared_match.is_some() {
                    map.remove("matched");
                }
            }
            row
        })
        .collect();
    let shown = rows.len() as u64;
    let confident = !verdict
        .codes
        .iter()
        .any(|code| code == "relevance_floor_unmeasured");
    let limited_by: Vec<&str> = machine
        .get("semantic_coverage")
        .and_then(|coverage| coverage.get("limited_by"))
        .and_then(Value::as_array)
        .map(|limits| limits.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let no_vectors = machine.get("search_vectors_used") == Some(&json!(false))
        || machine
            .get("semantic_coverage")
            .and_then(|coverage| coverage.get("embedding_state"))
            .and_then(Value::as_str)
            == Some("not_used")
        || machine
            .get("retrieval_arm")
            .and_then(Value::as_str)
            .is_some_and(|arm| arm == "cards" || arm.starts_with("cards+"))
        || limited_by.contains(&"vector_support_disabled")
        || verdict.codes.iter().any(|code| code == "coverage_absent");
    let tests_left_out = limited_by.contains(&"graph_role_filter")
        || verdict
            .codes
            .iter()
            .any(|code| code == "coverage_role_filter_withheld");
    // A class of code the ranking never looked at is said in the answer
    // line, beside the verdict it qualifies, with the call that ranks it.
    // Measured in a task run: the line read "Answer (complete)" over a ranking
    // that held no test, the exclusion sat only in the trailing `searched`
    // line, and a model looking for a test went on to run the whole suite.
    let left_out = if tests_left_out {
        ", with test code left out (include_tests true ranks it)"
    } else {
        ""
    };
    let mut meaning = match account.standing {
        AnswerStanding::Partial => format!(
            "Partial: these are the {shown} entities Kin ranks closest to {query:?}{left_out}, but {}.",
            verdict.open_reasons(op).join("; ")
        ),
        _ if shown == 0 => format!(
            "Answer: nothing ranked as a match for {query:?}{left_out}. That does not mean no code does this; try other words, or locate with mode name or mode literal."
        ),
        _ => {
            let mut text = format!(
                "Answer: these are the {shown} entities Kin ranks closest to {query:?}, best first{left_out}. A ranking shows the nearest matches; it does not claim to have found every entity that does this."
            );
            if !confident {
                text.push_str(" None of them is a confident match, so read the first few before relying on one.");
            }
            text
        }
    };
    // The question set the scope: said in the answer line, where a reader
    // looking for a test learns that tests are among the rows.
    let tests_ranked_for_question = machine
        .get("degradations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|entry| {
            entry.get("component").and_then(Value::as_str)
                == Some(crate::negative::QUESTION_SCOPE_COMPONENT)
                && entry.get("reason").and_then(Value::as_str)
                    == Some(crate::negative::TESTS_RANKED_FOR_QUESTION_REASON)
        });
    if tests_ranked_for_question {
        meaning.push_str(" The question asks about tests, so test code is ranked.");
    }
    // Rows that sit across packages teach the scope that ranks inside one.
    if let Some(clause) = ranked_scope_clause(&rows, &machine) {
        meaning.push(' ');
        meaning.push_str(&clause);
    }
    let ranked_on = if let Some(actual) = machine
        .get("ranked_by")
        .and_then(Value::as_str)
        .filter(|actual| !actual.trim().is_empty())
    {
        format!("Ranked on {}.", actual.trim().trim_end_matches('.'))
    } else if no_vectors {
        "Ranked on entity text and the graph, without search vectors.".to_string()
    } else {
        "Ranked from the available retrieval signals; this reply does not establish whether search vectors were used.".to_string()
    };
    let searched = joined(vec![
        ranked_on,
        verdict.degraded_scope(op).unwrap_or_default(),
    ]);
    let delivered = delivered_sentence(&machine, &verdict);
    let mut plain = Map::new();
    plain.insert(
        crate::envelope::ENVELOPE_KEY.into(),
        Said {
            word: account.word(),
            meaning,
            delivered,
            searched,
            fix: None,
        }
        .into_envelope(),
    );
    plain.insert("query".into(), json!(query));
    plain.insert("results".into(), json!(rows));
    if let Some(total) = machine.get("total_ranked").filter(|total| total.is_u64()) {
        plain.insert("total".into(), total.clone());
    }
    if let Some(cursor) = machine
        .get("next_cursor")
        .filter(|cursor| cursor.is_string())
    {
        plain.insert("next_cursor".into(), cursor.clone());
    }
    Value::Object(plain)
}

/// The reason a description ranking confined to a module path records, as a
/// note among its degradations.
const ROWS_IN_SCOPE: &str = "rows_in_scope";

/// Where a ranking's rows sit when they span more than one package and the
/// ranking was not confined to one: the two packages holding the most rows,
/// how many more there are, and the scope that ranks inside one. A package is
/// a row's module path cut to its first two parts, the prefix `scope` takes.
fn ranked_scope_clause(rows: &[Value], machine: &Map<String, Value>) -> Option<String> {
    let confined = machine
        .get("degradations")
        .and_then(Value::as_array)
        .is_some_and(|entries| {
            entries
                .iter()
                .any(|entry| entry.get("reason").and_then(Value::as_str) == Some(ROWS_IN_SCOPE))
        });
    if confined {
        return None;
    }
    let mut packages: Vec<(String, u64)> = Vec::new();
    for module in rows
        .iter()
        .filter_map(|row| row.get("module").and_then(Value::as_str))
    {
        let package = module.splitn(3, '.').take(2).collect::<Vec<_>>().join(".");
        match packages.iter_mut().find(|(seen, _)| *seen == package) {
            Some((_, count)) => *count += 1,
            None => packages.push((package, 1)),
        }
    }
    if packages.len() < 2 {
        return None;
    }
    // Most rows first; a tie keeps the order the ranking met them in.
    packages.sort_by(|a, b| b.1.cmp(&a.1));
    let named: Vec<String> = packages
        .iter()
        .take(2)
        .map(|(package, count)| format!("{package} ({count})"))
        .collect();
    let span = match packages.len() - 2 {
        0 => named.join(" and "),
        more => format!("{} and {more} more", named.join(", ")),
    };
    Some(format!(
        "Rows span {span}; scope: {:?} ranks inside one.",
        packages[0].0
    ))
}

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

/// The revision a source read was served at: the workspace generation and tree
/// its edit base names, or the change it was committed in.
fn source_revision(machine: &Map<String, Value>) -> Option<Value> {
    let context = machine
        .get("source_base")
        .and_then(|base| base.get("context"));
    if let Some(generation) = context
        .and_then(|context| context.get("workspace_generation"))
        .or_else(|| machine.get("workspace_generation"))
        .filter(|generation| generation.is_u64())
    {
        let mut revision = Map::new();
        revision.insert("workspace_generation".into(), generation.clone());
        if let Some(tree) = context
            .and_then(|context| context.get("workspace_tree_hash"))
            .or_else(|| machine.get("workspace_tree_hash"))
            .and_then(Value::as_str)
        {
            revision.insert(
                "tree".into(),
                json!(tree.chars().take(12).collect::<String>()),
            );
        }
        return Some(Value::Object(revision));
    }
    machine
        .get("source_change_id")
        .and_then(Value::as_str)
        .map(|change| json!({"change": change.chars().take(12).collect::<String>()}))
}

/// A source read, plainly: the code, who it is, the revision it was read at,
/// whether the body is whole, and a compact reference to its write
/// precondition. The whole `source_base` is what a machine reply serves; a
/// reader who edits copies this reference into `source_base` unchanged and
/// the server resolves it to that exact base, so nothing a write checks is
/// weaker for it.
fn plain_source(
    machine: Map<String, Value>,
    envelope: &Value,
    tool: &str,
    account: &crate::answer_account::Account,
) -> Value {
    let op = Operation::Source;
    let verdict = MachineVerdict::read(envelope);
    let name = subject_name(&machine).unwrap_or("this entity").to_string();
    let outline = machine.get("body_omitted").and_then(Value::as_str)
        == Some(crate::container_shape::CONTAINER_SHAPE_REASON);
    let lead = complete_lead(tool, &machine, envelope);
    let excerpt = crate::source_pages::plain_clause(&machine);
    let meaning = match (account.basis, account.standing) {
        // Missing membership evidence remains a gap in an outline, including
        // when some recorded members are available. Preserve those rows
        // without turning an incomplete inventory into an absence claim.
        (Basis::SourceMemberGap, _) => format!("Partial outline: {name}'s available recorded members are listed, but Kin cannot establish complete member coverage. This is not a finding that it declares nothing or that every declaration is listed."),
        (_, AnswerStanding::Partial) => format!(
            "Partial: this is the source of {name} as Kin holds it, but {}.",
            verdict.open_reasons(op).join("; ")
        ),
        _ if outline => format!("Answer: {lead}."),
        // An entity held as bytes is answered by its description.
        _ if crate::artifact_entity::is_bytes_reply(&machine) => format!("Answer: {lead}."),
        // A derived candidate has no body of its own by design; the code that
        // generates it is served beside it.
        _ if machine.get("body").is_none_or(Value::is_null) => format!(
            "Answer: {name} has no body of its own; generator_source holds the code that makes it."
        ),
        _ => match &excerpt {
            Some(clause) => format!("Excerpt: {lead}, {clause}"),
            None => format!("Answer: {lead}, the whole body."),
        },
    };
    // Whatever else the reading says, an excerpt says it is one.
    let meaning = match &excerpt {
        Some(clause) if !meaning.starts_with("Excerpt:") => {
            format!("{meaning} This reply is an excerpt: {clause}")
        }
        _ => meaning,
    };
    let revision = source_revision(&machine);
    let mut identity = Map::new();
    insert_identity(&mut identity, &Value::Object(machine.clone()));
    let mut plain = machine;
    for key in WORKING_KEYS {
        plain.remove(key);
    }
    // How the bytes were read and proven, which `revision` and the verdict
    // say: the byte span, the coherence label, the read path's label, the
    // provenance fields, and the outline marker `meaning` states.
    for key in [
        "id",
        "file_path",
        "read_path",
        "start_line",
        "end_line",
        "line",
        "file",
        "entity_file",
        "start_byte",
        "end_byte",
        "span_coherence",
        "source",
        "source_state",
        "source_change_id",
        "workspace_tree_hash",
        "workspace_generation",
        "base_change_id",
        "artifact_id",
        "language",
        "body_omitted",
        // A module's own code is said in `meaning`.
        crate::module_code::REPLY_KEY,
        crate::module_code::CLASS_REPLY_KEY,
        // So is what an artifact is; its level stays on the reply.
        crate::artifact_entity::REPLY_KEY,
    ] {
        plain.remove(key);
    }
    // The default outline note restates `meaning`; a note that says why a
    // full body was refused does not, and stays.
    if plain.get("note").and_then(Value::as_str) == Some(crate::container_shape::SHAPE_NOTE) {
        plain.remove("note");
    }
    // So does an excerpt's note, and its withheld base.
    if plain
        .get("note")
        .and_then(Value::as_str)
        .is_some_and(crate::source_pages::is_excerpt_note)
    {
        plain.remove("note");
    }
    if plain.get("source_base_unavailable").and_then(Value::as_str)
        == Some(crate::source_pages::SOURCE_BASE_UNAVAILABLE_EXCERPT)
    {
        plain.remove("source_base_unavailable");
    }
    plain.extend(identity);
    // An outline's members are entities too, each named by its entity_id.
    if let Some(Value::Array(members)) = plain.get_mut("members") {
        for member in members.iter_mut() {
            if let Some(map) = member.as_object_mut() {
                if let Some(id) = map.remove("id") {
                    map.insert("entity_id".into(), id);
                }
            }
        }
    }
    if let Some(revision) = revision {
        plain.insert("revision".into(), revision);
    }
    if let Some(base) = plain.remove("source_base") {
        if base.is_object() {
            plain.insert(
                "source_base".into(),
                crate::source_ticket::compact(&base).unwrap_or(base),
            );
        }
    }
    plain.retain(|_, value| !value.is_null());
    plain.insert(
        crate::envelope::ENVELOPE_KEY.into(),
        Said {
            word: account.word(),
            meaning,
            delivered: None,
            searched: None,
            fix: verdict.remedy.clone(),
        }
        .into_envelope(),
    );
    Value::Object(plain)
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

/// One recorded change, plainly: its id, who, when, what it said, and where
/// it came from. Its own id is `change_id`, never an entity's.
fn plain_change(change: &Value) -> Value {
    let mut plain = Map::new();
    if let Some(id) = change.get("id") {
        plain.insert("change_id".into(), id.clone());
    }
    for key in ["author", "timestamp"] {
        if let Some(value) = change.get(key).filter(|value| !value.is_null()) {
            plain.insert(key.into(), value.clone());
        }
    }
    if let Some(message) = change.get("message").and_then(Value::as_str) {
        plain.insert("commit_message".into(), json!(message.trim_end()));
    }
    match change.pointer("/origin/type").and_then(Value::as_str) {
        Some("git_commit") => {
            if let Some(oid) = change.pointer("/origin/oid") {
                plain.insert("git_commit".into(), oid.clone());
            }
        }
        Some(origin) => {
            plain.insert("origin".into(), json!(origin));
        }
        None => {}
    }
    if let Some(entities) = change
        .get("entity_delta_count")
        .filter(|count| count.is_u64())
    {
        plain.insert("entities_changed".into(), entities.clone());
    }
    if let Some(parents) = change.get("parents").and_then(Value::as_array) {
        if parents.is_empty() {
            plain.insert("first_recorded".into(), json!(true));
        } else {
            plain.insert("parents".into(), json!(parents));
        }
    }
    for key in ["entity_deltas", "relation_deltas", "tree_deltas"] {
        if let Some(value) = change.get(key).filter(|value| !value.is_null()) {
            plain.insert(key.into(), value.clone());
        }
    }
    Value::Object(plain)
}

/// A history read, plainly: complete when it delivered the requested page of
/// the recorded history, newest first, with the latest change said once and
/// where the recorded history begins said as its scope. Nothing about uses is
/// ever said of it.
fn plain_history(
    machine: Map<String, Value>,
    envelope: &Value,
    account: &crate::answer_account::Account,
) -> Value {
    let op = Operation::History;
    let verdict = MachineVerdict::read(envelope);
    let changes: Vec<&Value> = machine
        .get("changes")
        .and_then(Value::as_array)
        .map(|changes| changes.iter().collect())
        .unwrap_or_default();
    let count = machine
        .get("change_count")
        .and_then(Value::as_u64)
        .unwrap_or(changes.len() as u64);
    let offset = machine.get("offset").and_then(Value::as_u64).unwrap_or(0);
    let returned = changes.len() as u64;
    let next = machine
        .get("next_offset")
        .filter(|next| next.is_u64())
        .cloned();
    let latest = machine
        .get("latest_change")
        .filter(|latest| latest.is_object());
    // The latest change is the first row of the first page; it is said once.
    let latest_on_page = offset == 0
        && latest.is_some_and(|latest| {
            changes.first().and_then(|first| first.get("id")) == latest.get("id")
        });
    let approvals: Vec<Value> = machine
        .get("approvals")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let audit: Vec<Value> = machine
        .get("recent_audit_events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let page = if count == 0 {
        "Answer: no change is recorded for this entity.".to_string()
    } else if offset == 0 && next.is_none() {
        format!(
            "Answer: {} recorded for this entity, newest first{}.",
            counted(count, "change is", "changes are"),
            if count > 1 {
                ", and the first is the latest"
            } else {
                ""
            }
        )
    } else {
        format!(
            "Answer: {} recorded for this entity, newest first; this page holds {} to {}{}.",
            counted(count, "change is", "changes are"),
            offset + 1,
            offset + returned,
            if latest_on_page {
                ", and the first is the latest"
            } else {
                ""
            }
        )
    };
    let mut meaning = page;
    if count > 0 {
        meaning.push_str(if approvals.is_empty() {
            " No approval is recorded on the latest change."
        } else {
            " approvals lists the approvals recorded on the latest change."
        });
    }
    let meaning = match account.standing {
        AnswerStanding::Partial => format!(
            "Partial: {}, because {}.",
            meaning.trim_start_matches("Answer: ").trim_end_matches('.'),
            verdict.open_reasons(op).join("; ")
        ),
        _ => meaning,
    };
    let delivered = next.as_ref().map(|_| {
        "Older changes follow: call history again with offset set to next_offset.".to_string()
    });
    // Where the recorded history begins, when this page reaches it: the
    // change with no parent is the oldest Kin holds.
    let root = next
        .is_none()
        .then(|| changes.last())
        .flatten()
        .filter(|change| {
            change
                .get("parents")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
        });
    let boundary = root.map(|change| {
        let id: String = change
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .chars()
            .take(12)
            .collect();
        let imported = change
            .pointer("/origin/oid")
            .and_then(Value::as_str)
            .filter(|_| change.pointer("/origin/type").and_then(Value::as_str) == Some("git_commit"))
            .map(|oid| {
                format!(
                    ", which recorded Git commit {}",
                    oid.chars().take(12).collect::<String>()
                )
            })
            .unwrap_or_default();
        format!(
            "Kin's recorded history for this entity begins at change {id}{imported}; Kin holds nothing older for it, so a change made before then is not listed."
        )
    });
    let searched = joined(vec![
        "Every change Kin has recorded for this entity; audit events are the recent ones only."
            .to_string(),
        boundary.unwrap_or_default(),
    ]);

    let mut plain = Map::new();
    plain.insert(
        crate::envelope::ENVELOPE_KEY.into(),
        Said {
            word: account.word(),
            meaning,
            delivered,
            searched,
            fix: verdict.remedy.clone(),
        }
        .into_envelope(),
    );
    if let Some(id) = machine.get("entity_id") {
        plain.insert("entity_id".into(), id.clone());
    }
    plain.insert("change_count".into(), json!(count));
    plain.insert(
        "changes".into(),
        json!(changes
            .iter()
            .map(|change| plain_change(change))
            .collect::<Vec<_>>()),
    );
    if let (Some(latest), false) = (latest, latest_on_page) {
        plain.insert("latest_change".into(), plain_change(latest));
    }
    if !approvals.is_empty() {
        plain.insert("approvals".into(), json!(approvals));
    }
    if !audit.is_empty() {
        plain.insert("recent_audit_events".into(), json!(audit));
    }
    if let Some(next) = next {
        plain.insert("next_offset".into(), next);
    }
    Value::Object(plain)
}

// ---------------------------------------------------------------------------
// Neighbourhood
// ---------------------------------------------------------------------------

/// The name of the entity a neighbourhood was walked from.
fn neighborhood_subject(machine: &Map<String, Value>) -> Option<String> {
    let focal = machine.get("focal_id")?;
    machine
        .get("entities")
        .and_then(Value::as_array)?
        .iter()
        .find(|row| row.get("id") == Some(focal) || row.get("entity_id") == Some(focal))
        .and_then(|row| row.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// What a neighbourhood lists, in a few words.
fn neighborhood_lead(machine: &Map<String, Value>) -> String {
    let depth = machine.get("depth").and_then(Value::as_u64).unwrap_or(1);
    let entities = machine
        .get("entities")
        .and_then(Value::as_array)
        .map_or(0, Vec::len) as u64;
    let subject = neighborhood_subject(machine).unwrap_or_else(|| "it".to_string());
    let direction = match machine.get("direction").and_then(Value::as_str) {
        Some("out" | "outgoing") => " that it links to",
        Some("in" | "incoming") => " that link to it",
        _ => "",
    };
    format!(
        "these are {}{direction} within {} of {subject}, and the links between them",
        counted(entities, "entity", "entities"),
        counted(depth, "hop", "hops"),
    )
}

/// A neighbourhood's links as readable records: each names both of its ends
/// by name and entity_id, and how the first links to the second.
fn readable_links(machine: &mut Map<String, Value>) {
    let names: std::collections::HashMap<String, Value> = machine
        .get("entities")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let id = row.get("id").or_else(|| row.get("entity_id"))?.as_str()?;
                    Some((
                        id.to_string(),
                        row.get("name").cloned().unwrap_or(Value::Null),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let end = |value: Option<&Value>| -> Option<String> {
        let value = value?;
        value
            .as_str()
            .or_else(|| value.get("Entity").and_then(Value::as_str))
            .or_else(|| value.as_object()?.values().next()?.as_str())
            .map(str::to_string)
    };
    let Some(Value::Array(relations)) = machine.remove("relations") else {
        return;
    };
    let links: Vec<Value> = relations
        .iter()
        .filter_map(|relation| {
            let from = end(relation.get("src"))?;
            let to = end(relation.get("dst"))?;
            let how = relation
                .get("kind")
                .and_then(Value::as_str)
                .map(|kind| kind.to_ascii_lowercase())
                .unwrap_or_default();
            let mut link = Map::new();
            link.insert(
                "from".into(),
                names.get(&from).cloned().unwrap_or_else(|| json!(from)),
            );
            link.insert("how".into(), json!(how));
            link.insert(
                "to".into(),
                names.get(&to).cloned().unwrap_or_else(|| json!(to)),
            );
            link.insert("from_id".into(), json!(from));
            link.insert("to_id".into(), json!(to));
            if relation.get("resolution").and_then(Value::as_str) == Some("name_only") {
                link.insert("proof".into(), json!("matched by name only"));
            }
            Some(Value::Object(link))
        })
        .collect();
    machine.insert("links".into(), json!(links));
}

// ---------------------------------------------------------------------------
// Every other answer: walks, impact, context and anything else
// ---------------------------------------------------------------------------

/// Whether a context reply lost the body of an entity it was asked about.
pub(crate) fn context_focal_body_missing(machine: &Map<String, Value>) -> Option<String> {
    let lost = |row: &Value| {
        row.get("body").is_none_or(Value::is_null)
            && row
                .get("body_elided")
                .and_then(Value::as_array)
                .is_some_and(|keys| keys.iter().any(|key| key == "body"))
    };
    let focals: Vec<&Value> = match machine.get("focal_entity") {
        Some(focal) => vec![focal],
        None => machine
            .get("entities")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter(|row| row.get("section").and_then(Value::as_str) == Some("focal"))
                    .collect()
            })
            .unwrap_or_default(),
    };
    let missing: Vec<&str> = focals
        .into_iter()
        .filter(|row| lost(row))
        .filter_map(|row| row.get("name").and_then(Value::as_str))
        .collect();
    (!missing.is_empty()).then(|| {
        format!(
            "the body of {} did not fit in this reply; read it with source and its entity_id",
            missing.join(" and ")
        )
    })
}

/// The pack's bookkeeping a context reply does not serve: how it was fitted,
/// and empty route and call lists.
fn drop_context_bookkeeping(machine: &mut Map<String, Value>) {
    for key in [
        "method",
        "measurement_scope",
        "projection_measurement_scope",
        "neighborhood_depth",
    ] {
        machine.remove(key);
    }
    for key in ["routes", "external_calls"] {
        if machine
            .get(key)
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        {
            machine.remove(key);
        }
    }
    if machine
        .get("route_search")
        .and_then(|search| search.get("pairs_examined"))
        .and_then(Value::as_u64)
        == Some(0)
    {
        machine.remove("route_search");
    }
    // Who each entity asked about is, not how its name was resolved.
    if let Some(Value::Array(focals)) = machine.get_mut("focals") {
        for focal in focals.iter_mut() {
            if let Some(map) = focal.as_object_mut() {
                map.remove("projection");
                map.remove("resolution");
            }
        }
    }
    // Keep each row's projection until plain_row translates it into the
    // content actually shown. A signature does not disclose its entity body.
    for key in [
        "dependencies",
        "dependents",
        "tests",
        "transitive_deps",
        "entities",
    ] {
        if let Some(Value::Array(rows)) = machine.get_mut(key) {
            for row in rows.iter_mut() {
                if let Some(map) = row.as_object_mut() {
                    map.remove("relation");
                }
            }
        }
    }
}

/// What a signature change to the changed methods obliges across their
/// implementation families, from each `implementation_family` entry, said in
/// the answer's meaning so the obligation travels with the answer: the
/// overrides, the callers that reach the method only through one, what was
/// matched by name only, and whether another override could exist. `None`
/// when no family has an override, an override caller or a missed override
/// to name, which leaves an ordinary method's answer as it was.
fn impact_family_sentence(machine: &Map<String, Value>) -> Option<String> {
    let entries = machine
        .get(crate::handlers::implementation_family::FAMILY_KEY)?
        .as_array()?;
    let sum = |pointer: &str| -> u64 {
        entries
            .iter()
            .filter_map(|entry| entry.pointer(pointer).and_then(Value::as_u64))
            .sum()
    };
    let overrides = sum("/obligations/change_signature/count").saturating_sub(entries.len() as u64);
    let override_callers = sum("/obligations/update_calls/of_overrides");
    let by_name = sum("/obligations/check_calls/count");
    let sites = sum("/obligations/check_sites/count");
    let completeness = |state: &str| {
        entries.iter().any(|entry| {
            entry
                .pointer("/coverage/repository_completeness")
                .and_then(Value::as_str)
                == Some(state)
        })
    };
    let incomplete = completeness("incomplete");
    if overrides == 0 && override_callers == 0 && !incomplete {
        return None;
    }
    let mut parts = vec![format!(
        "A signature change also changes {} in affected_overrides and {} in affected_override_callers that {} an override, named under calls_override.",
        counted(overrides, "override", "overrides"),
        counted(override_callers, "caller", "callers"),
        if override_callers == 1 { "calls" } else { "call" },
    )];
    if by_name + sites > 0 {
        let more = by_name + sites;
        parts.push(format!(
            "{more} more {} matched by name only and {} a check, as none is proof: rows with proven false, override_caller_candidates, and the unresolved call sites in sections.direct.call_sites.",
            plural(more, "caller or call site was", "callers or call sites were"),
            plural(more, "needs", "need"),
        ));
    }
    if incomplete {
        parts.push("Another override could exist: check unlisted_same_name_methods and classes_spelling_base_without_edge in implementation_family.".to_string());
    } else if entries.iter().any(|entry| {
        entry
            .pointer("/obligations/change_signature/count")
            .and_then(Value::as_u64)
            .is_some_and(|members| members > 1)
            && entry
                .pointer("/coverage/repository_completeness")
                .and_then(Value::as_str)
                != Some("complete")
    }) {
        parts.push("Whether another override exists was not measured.".to_string());
    }
    Some(parts.join(" "))
}

/// The relation classes an impact answer did not establish, read before its
/// working is stripped. A debt-free call-site observation settles no other
/// class, and cannot supply a missing class reading.
fn impact_relation_gap(machine: &Map<String, Value>, envelope: &Value) -> Option<String> {
    let classes = machine
        .get("edge_coverage")
        .and_then(|coverage| coverage.get("classes"))
        .or_else(|| envelope.pointer("/completeness/classes"))
        .and_then(Value::as_object);
    if let Some(classes) = classes {
        let unestablished: Vec<&str> = [
            ("calls", "calls"),
            ("imports", "imports"),
            ("references", "non-call references"),
        ]
        .into_iter()
        .filter_map(|(key, words)| {
            classes
                .get(key)
                .and_then(Value::as_str)
                .filter(|state| *state != "present")
                .map(|_| words)
        })
        .collect();
        if !unestablished.is_empty() {
            let names = match unestablished.as_slice() {
                [one] => (*one).to_string(),
                [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
                [] => unreachable!(),
            };
            return Some(format!(
                "complete coverage of {names} between entities was not established"
            ));
        }
        return None;
    }
    let observation = machine.get("enrichment")?;
    (observation.get("status").and_then(Value::as_str) == Some("no_recorded_call_site_debt")
        && observation
            .get("basis")
            .and_then(Value::as_str)
            .is_some_and(|basis| basis.contains("other relation enrichment is not established")))
    .then(|| "the call-site observation does not establish other relation coverage".to_string())
}

fn plain_generic(
    machine: Map<String, Value>,
    envelope: &Value,
    tool: &str,
    op: Operation,
    account: &crate::answer_account::Account,
) -> Value {
    plain_generic_with_trace(machine, envelope, tool, op, None, account)
}

/// Render a trace page using complete readings only for its lead and limits.
/// The current page's envelope alone supplies its verdict and delivery state.
pub(crate) fn plain_trace_page(
    mut machine: Map<String, Value>,
    semantic: &Map<String, Value>,
) -> Value {
    let envelope = machine
        .remove(crate::envelope::ENVELOPE_KEY)
        .unwrap_or(Value::Null);
    let account = crate::answer_account::account(TRACE_TOOL, &machine, &envelope, Some(semantic));
    let mut plain = plain_generic_with_trace(
        machine,
        &envelope,
        TRACE_TOOL,
        Operation::Trace,
        Some(semantic),
        &account,
    );
    speak_served_verbs(&mut plain);
    hold_to_account(TRACE_TOOL, &account, &plain);
    plain
}

/// What names an unparsed entity in its text, plainly: the places, matched by
/// name, and why that is never the whole answer. Other reasons the answer is
/// partial follow.
fn named_in_meaning(machine: &Map<String, Value>, verdict: &MachineVerdict) -> String {
    use crate::artifact_entity::mentions::NAMED_IN_KEY;
    let name = machine
        .get("entity")
        .and_then(|entity| entity.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("this entity");
    let count = |key: &str| machine.get(key).and_then(Value::as_u64).unwrap_or(0);
    let listed = machine
        .get(NAMED_IN_KEY)
        .and_then(Value::as_array)
        .map_or(0, Vec::len) as u64;
    let total = count("named_in_total").max(listed);
    let mut meaning = match total {
        0 => format!("Partial: no entity's text names {name} by name."),
        1 => format!("Partial: this is the 1 entity whose text names {name}, matched by name."),
        _ => format!(
            "Partial: these are the {total} entities whose text names {name}, matched by name."
        ),
    };
    meaning.push_str(match total {
        0 => " Kin looks for an entity's name in the text of others; a name built while code runs, or a pattern, is not followed, so something could still use it.",
        1 => " Kin has not resolved what it uses it for, and a name built while code runs, or a pattern, is not followed.",
        _ => " Kin has not resolved what they use it for, and a name built while code runs, or a pattern, is not followed.",
    });
    let withheld = total.saturating_sub(listed);
    if withheld > 0 {
        meaning.push_str(&format!(
            " {NAMED_IN_KEY} lists the first {listed}; {withheld} more {} not listed.",
            plural(withheld, "is", "are"),
        ));
    }
    let unread = count("named_in_unread");
    if unread > 0 {
        meaning.push_str(&format!(
            " Kin could not read the text of {}, so {} could also name it.",
            counted(unread, "entity", "entities"),
            plural(unread, "it", "they"),
        ));
    }
    let mut others: Vec<String> = Vec::new();
    for reason in verdict
        .codes
        .iter()
        .filter(|code| code.as_str() != crate::verdict::MATCHED_BY_NAME)
        .filter_map(|code| verdict.plain_reason_named(code))
    {
        if !others.contains(&reason) {
            others.push(reason);
        }
    }
    if !others.is_empty() {
        meaning.push_str(&format!(" Also, {}.", others.join("; ")));
    }
    meaning
}

fn plain_generic_with_trace(
    mut machine: Map<String, Value>,
    envelope: &Value,
    tool: &str,
    op: Operation,
    trace_semantic: Option<&Map<String, Value>>,
    account: &crate::answer_account::Account,
) -> Value {
    let mut verdict = MachineVerdict::read(envelope);
    // Page identity repeats only the small query parameters; the frozen
    // reading still preserves the full question losslessly across pages.
    if tool == TRACE_TOOL {
        if let Some(question) = envelope.get("trace_question").and_then(Value::as_object) {
            for key in ["direction", "depth", "limit_per_step", "focal_name"] {
                if let Some(value) = question.get(key) {
                    machine.entry(key).or_insert_with(|| value.clone());
                }
            }
        }
    }
    let mut normalized = (tool == TRACE_TOOL).then(|| trace_semantic.unwrap_or(&machine).clone());
    let reach = normalized
        .as_ref()
        .and_then(crate::trace_reach::answer_view);
    if let (Some(reach), Some(normalized)) = (&reach, normalized.as_mut()) {
        normalized.insert("reach".into(), Value::Object(reach.clone()));
        // Chain-specific caps do not bound the independently walked closure.
        // Generic truncation is set aside only once its answer rows arrived.
        // A degradation the walk reported is one of those caps: a fan-out
        // cap, a chain step matched by name, a body the chain could not
        // quote, or the walk's own clock. The closure reads every caller
        // whatever the chain shows, and says its own bounds under
        // `reach.bounds`. Left standing, the code graded a scoped reach
        // partial where the same nine members unscoped were complete, only
        // because the scoped chain was cut by a different cap.
        let delivered = crate::trace_reach::rows_delivered(reach);
        // A reach that finished reading itemizes every place a call could
        // still be made from: its frontier rows, the observations it folded
        // by reason, the holders of a member as a value and the callers
        // matched by name alone. The codes that say some call in scope is
        // not resolved restate those places, so beside them they grade
        // nothing: the answer is complete except what the reach lists, and
        // partial only when work stopped or rows are missing.
        let finished = reach_finished(reach);
        verdict.codes.retain(|code| {
            let chain_cap = matches!(
                code.as_str(),
                "trace_spine_clipped" | "trace_walk_truncated" | "trace_walk_degraded"
            );
            let delivered_cut =
                delivered && matches!(code.as_str(), "answer_truncated" | "counts_are_a_floor");
            let itemized = finished && reach_itemizes(code);
            !(chain_cap || delivered_cut || itemized)
        });
    }
    let trace_semantic = normalized.as_ref();
    // A chain walked across the code says what it checked, as a reference
    // answer does.
    let followed = (tool == TRACE_TOOL).then(|| {
        if reach.is_some() {
            " Complete under the reach reading's declared call policy; its blind spots remain excluded.".to_string()
        } else {
            format!(" Kin checked every call in {}; you can answer from this.", where_checked())
        }
    });
    // A walk's own limits or unchecked calls are separate from what the
    // response budget delivered, whose cursor and advice remain below.
    let limits = if tool == TRACE_TOOL {
        trace_limits(trace_semantic.unwrap_or(&machine))
    } else {
        Vec::new()
    };
    // A context that lost the body it was asked for says so: the focal
    // evidence is the answer, and nothing else a context leaves out is.
    let focal_missing = (op == Operation::Context)
        .then(|| context_focal_body_missing(&machine))
        .flatten();
    // The account graded the walk once. Its basis says which places, if any,
    // the answer is complete except: a frontier listed place by place, a
    // finished reach's own lists, or the places a value could call from.
    // A walk's limits stand against it only when nothing settled them.
    let frontier_listed = (account.basis == Basis::WalkFrontierListed)
        .then(|| trace_frontier_listed(trace_semantic.unwrap_or(&machine)))
        .flatten();
    let walk_open = account.standing == AnswerStanding::Partial;
    // Where a call through a value could take a reached entity from, when
    // the reading serves every such place: those are the places to check,
    // not a count of every call through a value in the repository.
    let value_places = (tool == TRACE_TOOL)
        .then(|| trace_value_places(trace_semantic.unwrap_or(&machine)))
        .flatten();
    let value_sentence = value_places.as_ref().and_then(trace_value_places_sentence);
    let listed = !is_empty_answer(&machine);
    // A page of a callers walk past every reach row holds display chain and
    // evidence only. It adds no member, so it does not grade the answer
    // again: the first page's verdict stands, and this page carries none.
    let trace_page = (tool == TRACE_TOOL)
        .then(|| trace_page(&machine, envelope))
        .flatten();
    let display_page = trace_page == Some(TracePage::Display);
    // A callers reach whose verdict is complete, or complete except listed
    // places, says its members are every caller: the walk is finished.
    let reach_answered = reach.is_some() && !display_page && !walk_open;
    let lead = if reach_answered {
        trace_lead_as(trace_semantic.unwrap_or(&machine), envelope, true)
    } else {
        complete_lead(tool, trace_semantic.unwrap_or(&machine), envelope)
    };
    let mut meaning = match account.basis {
        Basis::WalkDisplayPage => {
            let focal = machine
                .get("focal_name")
                .and_then(Value::as_str)
                .unwrap_or("the focal");
            let steps = machine
                .get("chain")
                .and_then(Value::as_array)
                .map_or(0, Vec::len) as u64;
            let depth = machine
                .get("depth")
                .and_then(Value::as_u64)
                .map(|depth| format!(" up to {}", counted(depth, "call away", "calls away")))
                .unwrap_or_default();
            format!(
                "Display detail: this page continues the chain the callers walk from {focal} took{depth}, {} here. The answer is reach.members on the first page of this reply, under the verdict given there; these steps add no member and carry no verdict of their own.",
                counted(steps, "step", "steps")
            )
        }
        Basis::WalkReachListed => format!(
                "Answer: {lead}. Kin followed every call it can read by name; what it could not \
                 follow is listed or counted under reach, each with why.{} Check one only if its \
                 reason could apply to this question, then answer.",
                value_sentence
                    .as_deref()
                    .map(|sentence| format!(" {sentence}"))
                    .unwrap_or_default(),
        ),
        Basis::WalkReachWhole | Basis::WalkWhole | Basis::WalkCounted => {
            format!("Answer: {lead}.{}", followed.clone().unwrap_or_default())
        }
        Basis::WalkFrontierListed => {
            let places = frontier_listed.unwrap_or(0);
            format!(
                    "Answer: {lead}. Kin followed every call it can read by name; {} it cannot follow {} listed in reach.frontier with why{}.{} Check one only if its reason could apply to this question, then answer.",
                    counted(places, "place", "places"),
                    if places == 1 { "is" } else { "are" },
                    trace_frontier_tests_clause(trace_semantic.unwrap_or(&machine), places),
                    value_sentence
                        .as_deref()
                        .map(|sentence| format!(" {sentence}"))
                        .unwrap_or_default(),
            )
        }
        Basis::WalkValuePlaces => format!(
            "Answer: {lead}. Kin followed every call it can read by name. {} Check one only if it could apply to this question, then answer.",
            value_sentence.as_deref().unwrap_or_default(),
        ),
        // Only a usage analysis that lists entities can be complete except the
        // indirect uses of what it lists.
        Basis::WalkIndirect => format!(
            "Answer: {lead}. Some of these entities could also be reached indirectly, by being passed as a value or looked up by name; refs on one lists those places."
        ),
        Basis::WalkScope => format!("Answer: {lead}."),
        Basis::NamedIn => named_in_meaning(&machine, &verdict),
        _ => {
            let mut reasons: Vec<String> = verdict.open_reasons(op);
            if tool == "impact_analysis" {
                if let Some(gap) = impact_relation_gap(&machine, envelope) {
                    reasons.retain(|reason| reason != UNEXPLAINED);
                    reasons.push(gap);
                }
            }
            if !limits.is_empty() {
                // `truncated` also means a walk ended on incomplete call
                // coverage. Its lower-bound verdict is not proof of a size
                // cut; the recorded walk gaps say the cause, and `delivered`
                // separately keeps any actual response cut and continuation.
                // The walk's own caps are the limits said first, in their
                // own words with the way past them, so they are said once.
                let walk_caps: Vec<&str> = ["trace_walk_truncated", "trace_walk_degraded"]
                    .into_iter()
                    .filter_map(plain_reason)
                    .collect();
                reasons.retain(|reason| {
                    reason != CUT_TO_FIT
                        && reason != UNEXPLAINED
                        && !walk_caps.contains(&reason.as_str())
                });
                reasons.splice(0..0, limits);
            }
            if let Some(missing) = &focal_missing {
                reasons.insert(0, missing.clone());
            }
            let why = if reasons.is_empty() {
                unexplained_for(op).to_string()
            } else {
                reasons.join("; ")
            };
            if tool == TRACE_TOOL {
                format!("Partial: {lead}. There may be more, because {why}.")
            } else {
                format!("Partial: {lead}, but {why}.")
            }
        }
    };
    // Entities that reach it only through a call matched by a bare name are
    // possible callers, listed apart from the members. The count beside the
    // verdict is the members, so the meaning says how many more there are
    // and where, and an answer that was complete is complete except them.
    let by_name = reach
        .as_ref()
        .and_then(|reach| reach.get(crate::trace_reach::NAME_ONLY_TOTAL_KEY))
        .and_then(Value::as_u64)
        .filter(|count| *count > 0 && !display_page);
    // A walk asked to leave tests out says so beside its answer: the
    // exclusion is the answer's scope, never a gap in it.
    if let Some(excluded) = reach.as_ref().filter(|_| !display_page).and_then(|reach| {
        tests_excluded_sentence(reach, account.standing == AnswerStanding::Complete)
    }) {
        meaning.push(' ');
        meaning.push_str(&excluded);
    }
    if let Some(count) = by_name {
        meaning.push_str(&format!(
            " {count} more {} only through a call matched by name alone, which nothing tied to \
             its target; reach.name_only lists {}. Count one only if the object it calls on \
             could be the one its row names.",
            if count == 1 {
                "function reaches it"
            } else {
                "functions reach it"
            },
            if count == 1 { "it" } else { "them" },
        ));
    }
    if tool == "impact_analysis" {
        if let Some(family) = impact_family_sentence(&machine) {
            meaning.push(' ');
            meaning.push_str(&family);
        }
    }
    if let Some(folded) = folded_bindings_sentence(&verdict, envelope) {
        meaning.push(' ');
        meaning.push_str(&folded);
    }
    if tool == TRACE_TOOL {
        if let Some(folded) = trace_folded_sentence(trace_semantic.unwrap_or(&machine)) {
            meaning.push(' ');
            meaning.push_str(&folded);
        }
    }
    if walk_open && tool == "impact_analysis" {
        let changed = machine
            .get("changed_ids")
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().any(Value::is_string));
        meaning.push_str(if changed {
            " To inspect this impact, call refs on each changed entity in changed_ids, then read relevant affected callers and overrides with source and their ids. This inspection does not certify complete impact coverage."
        } else {
            " To inspect this impact, read relevant listed entities with source and call refs on their ids. This inspection does not certify complete impact coverage."
        });
    }
    if !verdict.safe_to_conclude_absent && verdict.certified() && !listed {
        meaning.push_str(" Nothing was found, but that does not prove it does not exist.");
    }
    // What a listing set aside is said, with the way to it.
    if verdict.set_aside_calls(op) {
        meaning.push_str(" Calls in this code that Kin could not match to a definition are not listed; locate by the called name finds one.");
    }
    let unmeasured_links =
        op.is_listing() && verdict.codes.iter().any(|code| is_link_coverage_code(code));
    // A neighbourhood is an exploration, said once with how far its links
    // were measured; a context lists the links around its focal.
    let (exploration, listing_scope) = match (op == Operation::Neighbors, unmeasured_links) {
        (true, true) => (
            Some("This explores the links Kin holds for the hops asked, and how completely they cover this code was not measured; it is not a list of every use, which refs answers."),
            None,
        ),
        (true, false) => (
            Some("This explores the links Kin holds for the hops asked; it is not a list of every use, which refs answers."),
            None,
        ),
        (false, true) => (
            None,
            Some("It lists the links Kin holds; how completely they cover this code was not measured for this listing."),
        ),
        (false, false) => (None, None),
    };
    // Lookups by a name built at run time that spell no reached entity's
    // name are where any reader of code stops, said as scope with the count
    // the reach reading keeps; one that could spell a name is a frontier row.
    let trace_unkeyed = reach
        .as_ref()
        .and_then(|r| r.get("blind_spots"))
        .and_then(|b| b.get("unkeyed_lookups"))
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .map(|count| {
            let focal = reach
                .as_ref()
                .and_then(|r| r.get("target"))
                .and_then(|target| target.get("name"))
                .and_then(Value::as_str)
                .or_else(|| subject_name(trace_semantic.unwrap_or(&machine)))
                .unwrap_or("the focal");
            unkeyed_sentence(count, &format!("{focal} or any entity in reach.members"))
        });
    let edge = (trace_unkeyed.is_none() && only_unkeyed(&verdict)).then_some(UNKEYED_SCOPE);
    let server_failed = (op.is_listing()
        && verdict
            .codes
            .iter()
            .any(|code| code == "reference_enrichment_unusable"))
    .then(|| match &verdict.server_start_failure {
        Some(reason) => format!(
            "{reason}, so links only a language server can find are not listed."
        ),
        None => "The language server for this code failed to start, so links only it can find are not listed.".to_string(),
    });
    let scope = joined(
        [exploration, listing_scope, server_failed.as_deref(), edge]
            .into_iter()
            .flatten()
            .map(str::to_string)
            .chain(trace_unkeyed)
            .chain(verdict.degraded_scope(op))
            .collect(),
    );
    if let Some(sentence) = value_sentence
        .as_deref()
        .filter(|sentence| !meaning.contains(sentence))
    {
        meaning.push(' ');
        meaning.push_str(sentence);
    }
    if let Some(reach) = reach.as_ref().filter(|_| value_places.is_none()) {
        if let Some(values) = reach
            .get("blind_spots")
            .and_then(|b| b.get("calls_through_values"))
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
        {
            meaning.push_str(&format!(" {values} calls through values are counted, not followed; this is not a claim that no other call can reach the focal."));
        } else if verdict
            .codes
            .iter()
            .any(|code| code == kin_model::call_site_reading::VALUE_CALLS_UNMEASURED)
        {
            meaning.push_str(" Calls through values remain unmeasured; this is not a claim that no other call can reach the focal.");
        }
        if let Some(members) = reach
            .get("blind_spots")
            .and_then(|blind| blind.pointer("/uncensused/members"))
            .and_then(Value::as_array)
            .filter(|members| !members.is_empty())
        {
            meaning.push_str(&format!(
                " {} in the reach reading have no escape census; their indirect uses remain unmeasured.",
                counted(members.len() as u64, "entity", "entities")
            ));
        }
    }
    if reach.is_none()
        && tool == TRACE_TOOL
        && trace_value_qualification_open(&verdict, trace_semantic.unwrap_or(&machine))
        && trace_frontier_listed(trace_semantic.unwrap_or(&machine)).is_some()
    {
        meaning.push_str(" Value-call coverage remains unmeasured in the reach metadata; a fully listed frontier does not establish exhaustive callers.");
    }
    let delivered = match trace_page {
        Some(TracePage::Display) => verdict.has_more.then(|| {
            "More display chain steps and walk evidence follow; send next_cursor as cursor only if you need them.".to_string()
        }),
        // The page says every reach row is delivered by it; whether all of
        // them sit on this page depends on where it starts.
        Some(TracePage::AnswerDelivered) => reach
            .as_ref()
            .and_then(|reach| reach.get("members_total"))
            .map(|members| {
                let here = if envelope.pointer("/page/record_offset").and_then(Value::as_u64)
                    == Some(0)
                {
                    "are on this page"
                } else {
                    "have been delivered"
                };
                format!(
                    "All {members} recorded reach members and listed frontier rows {here}. next_cursor pages only the display chain and the walk's evidence, which add no member and do not change this verdict; send it only if you need them."
                )
            })
            .or_else(|| delivered_sentence(&machine, &verdict)),
        // The members come first, so a first page that holds them all is
        // followed only by more of the places that could add a caller.
        None => reach
            .as_ref()
            .filter(|_| verdict.has_more)
            .and_then(|reach| members_on_first_page(&machine, envelope, reach))
            .map(|members| {
                format!(
                    "All {members} reach members are on this page. next_cursor serves more of \
                     the places that could add a caller, then the display chain; it adds no \
                     member, so send it only to check those places."
                )
            })
            .or_else(|| delivered_sentence(&machine, &verdict)),
    };
    let other_matches = machine
        .get("focal_resolution")
        .and_then(|resolution| resolution.get("other_candidates"))
        .and_then(Value::as_array)
        .filter(|others| !others.is_empty())
        .cloned();
    if let Some(others) = &other_matches {
        meaning.push_str(&format!(
            " {} other {} share this name; pass an entity id to pick one.",
            others.len(),
            plural(others.len() as u64, "entity", "entities"),
        ));
    }
    if op == Operation::Neighbors {
        readable_links(&mut machine);
        for key in ["focal_id", "entity_count", "relation_count"] {
            machine.remove(key);
        }
    }
    if op == Operation::Context {
        drop_context_bookkeeping(&mut machine);
    }
    for key in WORKING_KEYS {
        machine.remove(key);
    }
    // A bare `truncated` flag restates what `delivered` or the walk's limit in
    // `meaning` already says with the way to the rest, so it goes when they
    // say it, and stays when nothing else would.
    if delivered.is_some() || walk_open && tool == TRACE_TOOL {
        machine.remove("truncated");
    }
    if let Some(others) = other_matches {
        machine.insert("other_matches".into(), json!(others));
    }
    // See `CHAIN_CAP_READINGS`: beside a reach, the display chain's cap cut
    // nothing the answer holds.
    if reach.is_some() {
        for key in CHAIN_CAP_READINGS {
            machine.remove(key);
        }
    }
    plain_entity_rows(&mut machine);
    // Signatures left out to fit are said by `delivered`, not on every row.
    if op == Operation::Neighbors {
        if let Some(Value::Array(rows)) = machine.get_mut("entities") {
            for row in rows.iter_mut() {
                if let Some(map) = row.as_object_mut() {
                    map.remove(crate::budget::BODY_ELIDED_KEY);
                }
            }
        }
    }
    // Reference rows keep one shape wherever they appear, such as on a later
    // page of a reference answer, which carries rows without its focal.
    if let Some(rows) = machine.get_mut("references").and_then(Value::as_array_mut) {
        for row in rows.iter_mut() {
            *row = plain_reference_row(row);
        }
    }
    if let Some(withheld) = machine.get_mut("withheld").and_then(Value::as_object_mut) {
        let has_ids = crate::budget::context_withheld_has_ids(&Value::Object(withheld.clone()));
        withheld.insert(
            "how".into(),
            json!(if has_ids {
                "Read the listed entity IDs with source; counts may include other rows whose IDs are not listed."
            } else {
                "These counts describe omitted rows; their entity IDs are not listed."
            }),
        );
    }
    let searched = if is_answer_tool(tool) {
        Some(searched_or_unmeasured(envelope))
    } else {
        searched_line(envelope)
    };
    // A display page grades nothing, so it names no verdict and no scope
    // that a verdict would limit.
    let (word, searched) = if display_page {
        (None, None)
    } else {
        (account.word(), with_scope(searched, scope))
    };
    // An impact answer is its rows: who calls the changed entity, what
    // overrides or implements it, and what else depends on those, each once.
    // A partial answer keeps what it holds: its meaning sends the reader to
    // the places it could not settle, which sit in those blocks.
    let impact_answered = tool == "impact_analysis" && impact_is_answered(word);
    if tool == "impact_analysis" && !impact_answered {
        machine.remove(crate::handlers::review::IMPACT_ROW_TOTALS_KEY);
    }
    let (meaning, delivered) = if impact_answered {
        let answer = lean_impact(&mut machine);
        let meaning = impact_meaning(&meaning, &answer, word == Some(COMPLETE));
        if answer.whole {
            for key in ["next_cursor", "answer_next_cursor", "evidence_next_cursor"] {
                machine.remove(key);
            }
        }
        (meaning, delivered.filter(|_| !answer.whole))
    } else {
        (meaning, delivered)
    };
    // A complete impact answer says what it found; the sentence about what
    // any reader of code can follow qualifies nothing it lists.
    let searched = searched.filter(|_| !(impact_answered && word == Some(COMPLETE)));
    machine.insert(
        crate::envelope::ENVELOPE_KEY.into(),
        plain_envelope_delivered(
            word,
            meaning,
            delivered,
            searched,
            verdict.remedy.as_deref(),
        ),
    );
    Value::Object(machine)
}

/// The entity an answer is about, and the rows it lists, in plain field names.
///
/// `focal_entity` becomes `entity`, the `focal_*` copies of its fields go, and
/// each row drops the fields that only say how it was proven or cut: a null,
/// a proof label other than a name-only match, and the edge and projection
/// labels its list already names.
/// What an impact reply holds beside its rows that no reader of the answer
/// acts on: each section's verdict with the inputs it was computed from and
/// the call-site reading behind it, the implementation family's coverage
/// counters and obligations with the note that glosses them, and per-entity
/// consumer counts. The full profile serves them all.
const IMPACT_INTERNALS: [&str; 11] = [
    "sections",
    "implementation_family",
    "implementation_family_note",
    "entity_impacts",
    crate::handlers::review::IMPACT_ROW_TOTALS_KEY,
    // What was left out as test code, and whether the dependents were asked
    // for, are said on the answer line.
    crate::handlers::review::IMPACT_TESTS_EXCLUDED_KEY,
    crate::handlers::review::IMPACT_ROW_SCOPE_KEY,
    crate::handlers::review::IMPACT_DEPENDENTS_ASKED_KEY,
    crate::handlers::review::IMPACT_VALUE_USES_WITHHELD_KEY,
    // What same-name rows are, and how many more there are, is said on the
    // answer line.
    crate::impact_sections::SAME_NAME_NOTE_KEY,
    crate::impact_sections::SAME_NAME_NOT_HELD_KEY,
];

/// The row lists that answer who calls the changed entity, who holds it as
/// a value and what overrides it. A row in one of these is not listed again
/// as a dependent.
const IMPACT_DIRECT_ROWS: [&str; 6] = [
    "affected_callers",
    crate::handlers::review::IMPACT_VALUE_USES_KEY,
    "affected_overrides",
    crate::impact_sections::INTERFACE_IMPLEMENTATIONS_KEY,
    crate::handlers::implementation_family::OVERRIDE_CALLERS_KEY,
    crate::handlers::implementation_family::OVERRIDE_CALLER_CANDIDATES_KEY,
];

/// Whether an impact reply graded `word` is an answer to lean to its rows:
/// complete, or complete except what it says. A partial one is left whole.
fn impact_is_answered(word: Option<&str>) -> bool {
    matches!(
        word,
        Some(COMPLETE | COMPLETE_EXCEPT | COMPLETE_EXCEPT_INDIRECT)
    )
}

/// The row lists that answer what else a change could reach: what depends on
/// the callers without calling the changed entity. A plain reply counts them
/// unless they were asked for.
const IMPACT_DEPENDENT_ROWS: [&str; 3] = crate::impact_sections::DEPENDENT_ROWS;

/// Whether the caller asked for the dependents lists.
fn impact_dependents_asked(machine: &Map<String, Value>) -> bool {
    machine
        .get(crate::handlers::review::IMPACT_DEPENDENTS_ASKED_KEY)
        .and_then(Value::as_bool)
        == Some(true)
}

/// Whether `key`, a row list `totals` counts, is on the page at its full
/// length.
fn impact_list_whole(machine: &Map<String, Value>, totals: &Value, key: &str) -> bool {
    totals.get(key).and_then(Value::as_u64).unwrap_or(0)
        == machine
            .get(key)
            .and_then(Value::as_array)
            .map_or(0, Vec::len) as u64
}

/// Whether every row list the reply shows is on the page at its full
/// length: every list `totals` counts, less the dependents lists when they
/// were not asked for, which the reply counts and does not list.
fn impact_lists_whole(machine: &Map<String, Value>, totals: &Value) -> bool {
    let asked = impact_dependents_asked(machine);
    totals.as_object().is_some_and(|counted| {
        counted
            .keys()
            .filter(|key| asked || !IMPACT_DEPENDENT_ROWS.contains(&key.as_str()))
            .all(|key| impact_list_whole(machine, totals, key))
    })
}

/// What a leaned impact reply holds, for its answer line.
struct ImpactAnswer {
    callers: usize,
    /// Entities that hold the changed entity or an override as a value.
    value_uses: usize,
    /// Those the store held past what one answer lists.
    value_uses_withheld: u64,
    overrides: usize,
    /// Methods whose types satisfy a changed Go interface method's
    /// interface, which no edge records.
    implementations: usize,
    override_callers: usize,
    dependents: usize,
    /// Dependents that are modules, counted and not listed.
    modules: usize,
    /// Held declarations that share the changed function's name with no
    /// link to it, listed under their own key.
    same_name: usize,
    /// Declarations of that name the graph holds no entity for, counted.
    same_name_not_held: u64,
    /// Whether the dependents are listed: the caller asked for them.
    dependents_listed: bool,
    /// Whether the dependents counted are every one: their lists were whole
    /// on this page. Otherwise `dependents` is the most there can be.
    dependents_counted: bool,
    /// Affected entities left out as test code, when the caller asked for
    /// none.
    tests_excluded: Option<u64>,
    /// The module path the lists were confined to, and how many affected
    /// entities sat outside it, when the caller named one.
    row_scope: Option<(String, u64)>,
    /// Whether every row of every list the reply shows is on this page.
    whole: bool,
}

/// Lean an impact reply to its rows, each entity once. See
/// [`IMPACT_INTERNALS`] for what goes; a row keeps its module, its name, its
/// id, what marks it a test, and a proof that is weaker than a resolved call.
fn lean_impact(machine: &mut Map<String, Value>) -> ImpactAnswer {
    // The totals the handler recorded, read before the lists are deduped.
    let whole = machine
        .get(crate::handlers::review::IMPACT_ROW_TOTALS_KEY)
        .is_some_and(|totals| impact_lists_whole(machine, totals));
    if let Some(readings) = machine.get_mut("readings").and_then(Value::as_array_mut) {
        readings.retain(|reading| {
            !reading
                .get("key")
                .and_then(Value::as_str)
                .is_some_and(|key| IMPACT_INTERNALS.contains(&key))
        });
        if readings.is_empty() {
            machine.remove("readings");
        }
    }
    let same_name_not_held = machine
        .get(crate::impact_sections::SAME_NAME_NOT_HELD_KEY)
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let value_uses_withheld = machine
        .get(crate::handlers::review::IMPACT_VALUE_USES_WITHHELD_KEY)
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tests_excluded = machine
        .get(crate::handlers::review::IMPACT_TESTS_EXCLUDED_KEY)
        .map(|excluded| {
            excluded
                .get("entities")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        });
    let row_scope = machine
        .get(crate::handlers::review::IMPACT_ROW_SCOPE_KEY)
        .and_then(|scope| {
            Some((
                scope.get("module")?.as_str()?.to_string(),
                scope.get("outside").and_then(Value::as_u64).unwrap_or(0),
            ))
        });
    let dependents_listed = impact_dependents_asked(machine);
    // How many rows the dependents lists hold in all, when this page does
    // not hold every one of them.
    let dependents_beyond_page = machine
        .get(crate::handlers::review::IMPACT_ROW_TOTALS_KEY)
        .filter(|totals| {
            IMPACT_DEPENDENT_ROWS
                .iter()
                .any(|key| !impact_list_whole(machine, totals, key))
        })
        .map(|totals| {
            IMPACT_DEPENDENT_ROWS
                .iter()
                .filter_map(|key| totals.get(*key).and_then(Value::as_u64))
                .sum::<u64>()
        });
    for key in IMPACT_INTERNALS {
        machine.remove(key);
    }
    let rows = |machine: &Map<String, Value>, key: &str| {
        machine
            .get(key)
            .and_then(Value::as_array)
            .map_or(0, Vec::len)
    };
    let id_of = |row: &Value| {
        row.get("id")
            .or_else(|| row.get("entity_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let direct: std::collections::HashSet<String> = IMPACT_DIRECT_ROWS
        .iter()
        .filter_map(|key| machine.get(*key).and_then(Value::as_array))
        .flatten()
        .filter_map(id_of)
        .collect();
    let mut modules = 0usize;
    if let Some(dependents) = machine
        .get_mut("affected_dependents")
        .and_then(Value::as_array_mut)
    {
        dependents.retain(|row| {
            if id_of(row).is_some_and(|id| direct.contains(&id)) {
                return false;
            }
            let module = row.get("kind").and_then(Value::as_str).is_some_and(|kind| {
                ["module", "file", "package"]
                    .iter()
                    .any(|container| kind.eq_ignore_ascii_case(container))
            });
            if module {
                modules += 1;
            }
            !module
        });
        if dependents.is_empty() {
            machine.remove("affected_dependents");
        }
    }
    // Dependents nobody asked for are counted, each entity once across the
    // lists that hold them, and not listed.
    let mut unlisted_dependents = 0usize;
    if !dependents_listed {
        let mut counted: std::collections::HashSet<String> = std::collections::HashSet::new();
        for key in IMPACT_DEPENDENT_ROWS {
            for row in machine
                .remove(key)
                .as_ref()
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                match id_of(row) {
                    Some(id) if direct.contains(&id) => {}
                    Some(id) => {
                        counted.insert(id);
                    }
                    None => unlisted_dependents += 1,
                }
            }
        }
        unlisted_dependents += counted.len();
    }
    // One changed entity is the one every caller row calls.
    let calls: std::collections::HashSet<String> = machine
        .get("affected_callers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| row.get("calls").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    for key in IMPACT_DIRECT_ROWS.into_iter().chain([
        "affected_dependents",
        "affected_contract_consumers",
        "affected_tests",
        crate::impact_sections::SAME_NAME_KEY,
    ]) {
        let Some(list) = machine.get_mut(key).and_then(Value::as_array_mut) else {
            continue;
        };
        for row in list.iter_mut() {
            let Some(row) = row.as_object_mut() else {
                continue;
            };
            for field in ["signature", "start_line", "end_line", "projection", "file"] {
                row.remove(field);
            }
            if row.get("proven").and_then(Value::as_bool) == Some(true) {
                row.remove("proven");
            }
            // A same-name row is one by the list it is in.
            if matches!(
                row.get("resolution").and_then(Value::as_str),
                Some("type_resolved" | "import_scoped" | "same_name")
            ) {
                row.remove("resolution");
            }
            if row.get("kind").and_then(Value::as_str).is_some_and(|kind| {
                kind.eq_ignore_ascii_case("method") || kind.eq_ignore_ascii_case("function")
            }) {
                row.remove("kind");
            }
            if calls.len() <= 1 {
                row.remove("calls");
            }
        }
    }
    ImpactAnswer {
        callers: rows(machine, "affected_callers"),
        value_uses: rows(machine, crate::handlers::review::IMPACT_VALUE_USES_KEY),
        value_uses_withheld,
        overrides: rows(machine, "affected_overrides"),
        implementations: rows(
            machine,
            crate::impact_sections::INTERFACE_IMPLEMENTATIONS_KEY,
        ),
        override_callers: rows(
            machine,
            crate::handlers::implementation_family::OVERRIDE_CALLERS_KEY,
        ),
        dependents: match (dependents_listed, dependents_beyond_page) {
            (true, _) => rows(machine, "affected_dependents"),
            (false, Some(total)) => total as usize,
            (false, None) => unlisted_dependents,
        },
        modules: match (dependents_listed, dependents_beyond_page) {
            (false, Some(_)) => 0,
            _ => modules,
        },
        same_name: rows(machine, crate::impact_sections::SAME_NAME_KEY),
        same_name_not_held,
        dependents_listed,
        dependents_counted: dependents_beyond_page.is_none(),
        tests_excluded,
        row_scope,
        whole,
    }
}

/// The answer line of a leaned impact reply: what it lists, by list, in
/// place of the sentence that only said rows follow. Whatever the meaning
/// said after its first sentence stays.
fn impact_meaning(meaning: &str, answer: &ImpactAnswer, complete: bool) -> String {
    let rest = meaning
        .strip_prefix("Answer: ")
        .and_then(|rest| rest.split_once(". "))
        .map(|(_, rest)| rest.trim())
        .unwrap_or_default();
    let mut lead = format!(
        "Answer: a change to it affects {} in affected_callers and {} in affected_overrides",
        counted(answer.callers as u64, "caller", "callers"),
        counted(
            answer.overrides as u64,
            "method that overrides or implements it",
            "methods that override or implement it"
        ),
    );
    if answer.implementations > 0 {
        lead.push_str(&format!(
            ", and {} in {}, whose types satisfy its interface",
            counted(
                answer.implementations as u64,
                "method that may implement it",
                "methods that may implement it"
            ),
            crate::impact_sections::INTERFACE_IMPLEMENTATIONS_KEY,
        ));
    }
    if answer.override_callers > 0 {
        lead.push_str(&format!(
            ", and {} of an override in affected_override_callers",
            counted(answer.override_callers as u64, "caller", "callers")
        ));
    }
    if answer.value_uses > 0 {
        lead.push_str(&format!(
            ", and {} it or an override as a value in {}{}",
            match answer.value_uses {
                1 => "1 place that holds".to_string(),
                more => format!("{more} places that hold"),
            },
            crate::handlers::review::IMPACT_VALUE_USES_KEY,
            match answer.value_uses_withheld {
                0 => String::new(),
                more => format!(" ({more} more not listed)"),
            }
        ));
    }
    lead.push_str(", each listed once.");
    if let Some((module, outside)) = &answer.row_scope {
        lead.push_str(&match outside {
            0 => format!(
                " Only what sits inside {module} is listed, as asked; no affected entity sits outside it."
            ),
            outside => format!(
                " Only what sits inside {module} is listed, as asked; {} outside it {} not listed.",
                counted(*outside, "affected entity", "affected entities"),
                plural(*outside, "is", "are"),
            ),
        });
    }
    if let Some(excluded) = answer.tests_excluded {
        // The words a reference answer and a callers walk use for the same
        // choice.
        lead.push_str(&format!(
            " {}{}.",
            if complete {
                "Complete for non-test code: test code was excluded, as asked"
            } else {
                "Test code was excluded, as asked; this covers non-test code"
            },
            match excluded {
                0 => String::new(),
                excluded => format!(
                    " ({} in test code left out)",
                    counted(excluded, "affected entity", "affected entities")
                ),
            }
        ));
    }
    if answer.dependents > 0 || answer.modules > 0 {
        let modules = match answer.modules {
            0 => String::new(),
            modules => format!(", and {}", counted(modules as u64, "module", "modules")),
        };
        if answer.dependents_listed {
            lead.push_str(&format!(
                " {} depend on those without calling it, in affected_dependents{modules}.",
                counted(answer.dependents as u64, "more entity", "more entities"),
            ));
        } else {
            let entities = counted(answer.dependents as u64, "more entity", "more entities");
            lead.push_str(&format!(
                " {}{entities}{} {} on those without calling it and {} not listed: pass \
                 dependents true to list them.",
                if answer.dependents_counted {
                    ""
                } else {
                    "Up to "
                },
                match answer.modules {
                    0 => String::new(),
                    modules => format!(" and {}", counted(modules as u64, "module", "modules")),
                },
                plural(
                    (answer.dependents + answer.modules) as u64,
                    "depends",
                    "depend"
                ),
                plural((answer.dependents + answer.modules) as u64, "is", "are"),
            ));
        }
    }
    if answer.same_name > 0 {
        lead.push_str(&format!(
            " {} its name and no link to it, in {}: where implementations are swapped by \
             module, as a host config is, each needs the same change.",
            match answer.same_name {
                1 => "1 other declaration has".to_string(),
                more => format!("{more} other declarations have"),
            },
            crate::impact_sections::SAME_NAME_KEY,
        ));
        if answer.same_name_not_held > 0 {
            lead.push_str(&format!(
                " {} of that name {} a variable or a re-export Kin holds no entity for; locate \
                 by the name lists {}.",
                counted(
                    answer.same_name_not_held,
                    "more declaration",
                    "more declarations"
                ),
                plural(answer.same_name_not_held, "is", "are"),
                plural(answer.same_name_not_held, "it", "them"),
            ));
        }
    }
    if rest.is_empty() {
        lead
    } else {
        format!("{lead} {rest}")
    }
}

fn plain_entity_rows(machine: &mut Map<String, Value>) {
    for key in [
        "focal",
        "focal_file",
        "focal_id",
        "focal_kind",
        "focal_name",
    ] {
        machine.remove(key);
    }
    if let Some(focal) = machine.remove("focal_entity") {
        machine.insert("entity".into(), plain_row(focal));
    }
    if let Some(focals) = machine.remove("focals") {
        let focals = match focals {
            Value::Array(rows) => Value::Array(rows.into_iter().map(plain_row).collect()),
            other => other,
        };
        machine.insert("asked_about".into(), focals);
    }
    for key in [
        "chain",
        "dependencies",
        "dependents",
        "tests",
        "transitive_deps",
        "external_calls",
        "entities",
        "results",
    ] {
        if let Some(rows) = machine.get_mut(key).and_then(Value::as_array_mut) {
            for row in rows.iter_mut() {
                *row = plain_row(std::mem::take(row));
            }
        }
    }
}

/// One entity row with the proof and presentation labels an agent does not
/// act on taken out, and the `entity_` prefix a trace step carries dropped.
fn plain_row(row: Value) -> Value {
    let Value::Object(row) = row else {
        return row;
    };
    let body_shown = row.get("body").is_some_and(Value::is_string);
    let signature_shown = row
        .get("signature")
        .and_then(Value::as_str)
        .is_some_and(|signature| !signature.is_empty());
    let mut plain = Map::new();
    let mut quoted = false;
    for (key, value) in row {
        // A body that was cut says so; any other false flag says nothing.
        if value.is_null() || (matches!(value, Value::Bool(false)) && key != "body_complete") {
            continue;
        }
        match key.as_str() {
            // A trace step's call sites with their code: they replace the bare
            // line numbers, which say nothing a reader can check.
            crate::site_expression::TRACE_REFERENCE_SITES_KEY => {
                let sites: Vec<Value> = value
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .map(|site| {
                        let mut plain = Map::new();
                        if let Some(line) = site.get("line_in_entity") {
                            plain.insert("line_in_entity".into(), line.clone());
                        }
                        if let Some(code) = site.get("expr") {
                            plain.insert("code".into(), code.clone());
                        }
                        Value::Object(plain)
                    })
                    .collect();
                if !sites.is_empty() {
                    quoted = true;
                    plain.insert("sites".into(), json!(sites));
                }
            }
            "resolution" => {
                if value == "name_only" {
                    plain.insert("proof".into(), json!("matched by name only"));
                }
            }
            "projection" => {
                if !body_shown && (value == "SignatureOnly" || value == "NameAndKind") {
                    plain.insert(
                        "shown".into(),
                        json!(if signature_shown {
                            "signature only"
                        } else {
                            "identity only"
                        }),
                    );
                }
            }
            "relation" | "span_coherence" | "site_state" | "fanout_dropped" | "crossing"
            | "package" | "stdlib" | "terminal" | "symbol" | "proof" => {}
            "fanout_truncated" => {
                plain.insert("more_not_shown".into(), value);
            }
            key if key.ends_with("_reason") => {}
            "entity_role" if value == "source" => {}
            "fanout_dropped_count" => {}
            // The pack's own labels for a row: its budget state, which the
            // reply's `delivered` states, and physical coordinates.
            "body_complete" if value == Value::Bool(true) => {}
            "start_byte" | "end_byte" | "start_line" | "end_line" | "line" | "file"
            | "file_path" | "read_path" | "entity_file" | "reference_lines" | "language"
            | "member_name" => {}
            "section" => {
                let section = match value.as_str() {
                    Some("focal") => json!("asked"),
                    _ => value,
                };
                plain.insert("section".into(), section);
            }
            key => {
                let key = match key {
                    "id" | "entity_id" => "entity_id",
                    "relation_kind" => "how",
                    other => other.strip_prefix("entity_").unwrap_or(other),
                };
                plain.insert(key.to_string(), value);
            }
        }
    }
    if quoted
        || plain
            .get("lines")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
    {
        plain.remove("lines");
    }
    Value::Object(plain)
}

pub(crate) fn is_empty_answer(machine: &Map<String, Value>) -> bool {
    ["results", "entities", "chain", "references"]
        .iter()
        .filter_map(|key| machine.get(*key).and_then(Value::as_array))
        .any(Vec::is_empty)
}

#[cfg(test)]
#[path = "plain_reply_tests.rs"]
mod tests;

#[cfg(test)]
mod unadmitted_group_tests {
    use super::*;
    use crate::query_reply::QueryPresentation;
    use std::collections::HashMap;

    fn fixture() -> Value {
        let mut declarations = Vec::new();
        for (file, owner, first_line, names) in [
            (
                "src/cachetools/__init__.pyi",
                "_cached_wrapper",
                154,
                vec![
                    "__wrapped__",
                    "__name__",
                    "__doc__",
                    "cache",
                    "cache_key",
                    "cache_lock",
                    "cache_condition",
                ],
            ),
            (
                "src/cachetools/__init__.pyi",
                "_cachedmethod_wrapper",
                196,
                vec![
                    "__wrapped__",
                    "__name__",
                    "__doc__",
                    "cache",
                    "cache_key",
                    "cache_lock",
                    "cache_condition",
                ],
            ),
            (
                "src/cachetools/func.pyi",
                "_cachetools_cache_wrapper",
                14,
                vec!["__wrapped__", "__name__", "__doc__"],
            ),
        ] {
            for (index, name) in names.into_iter().enumerate() {
                declarations.push(json!({"file":file,"owner":owner,"kind":"variable",
                    "reason":"nested_in_callable","name":name,"line":first_line + index}));
            }
        }
        json!({"query":"_wrapper","total_matches":2,
            "results":[
                {"entity_id":"method-wrapper","name":"_wrapper","qualified_name":"_wrapper",
                    "kind":"function","file":"src/cachetools/_cachedmethod.py","start_line":393,
                    "signature":"def _wrapper(method, cache, key, lock=None, cond=None, info=None)"},
                {"entity_id":"function-wrapper","name":"_wrapper","qualified_name":"_wrapper",
                    "kind":"function","file":"src/cachetools/_cached.py","start_line":230,
                    "signature":"def _wrapper(func, cache, key, lock=None, cond=None, info=None)"}],
            "edge_coverage":{"declarations":{"files":23,"matching_unadmitted_total":17,
                "matching_unadmitted":declarations}},
            "_kin":{"verdict":{"state":"certified","safe_to_conclude_absent":false}},
            "next_cursor":"opaque-next","readings":{"coverage":"unchanged"}})
    }

    fn rendered(machine: Value, details: bool) -> Value {
        let mut result = json!({"content":[{"type":"text","text":machine.to_string()}]});
        QueryPresentation::default().present(
            &mut result,
            "semantic_search",
            &HashMap::from([("details".into(), json!(details))]),
        );
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    fn flatten(rows: &Value) -> Value {
        let mut flat = Vec::new();
        for row in rows.as_array().unwrap() {
            if let Some(members) = row.get("members").and_then(Value::as_array) {
                assert_eq!(row["count"], members.len());
                for member in members {
                    flat.push(json!({"name":member["name"],"kind":row["kind"],
                        "inside":row["owner"],"why":row["why"]}));
                }
            } else {
                flat.push(row.clone());
            }
        }
        json!(flat)
    }

    #[test]
    fn default_locate_shares_parent_metadata_without_losing_any_declaration() {
        let rich = rendered(fixture(), true);
        let short = rendered(fixture(), false);
        assert_eq!(short["not_held"].as_array().unwrap().len(), 3);
        assert_eq!(flatten(&short["not_held"]), rich["not_held"]);
        for (index, owner, count) in [
            (0, "_cached_wrapper", 7),
            (1, "_cachedmethod_wrapper", 7),
            (2, "_cachetools_cache_wrapper", 3),
        ] {
            assert_eq!(short["not_held"][index]["owner"], owner);
            assert_eq!(short["not_held"][index]["count"], count);
        }
        for key in ["query", "total", "next_cursor", "readings"] {
            assert_eq!(short[key], rich[key]);
        }
        for key in ["verdict", "searched"] {
            assert_eq!(short["_kin"][key], rich["_kin"][key]);
        }
        assert_eq!(short["_kin"]["verdict"], COMPLETE_EXCEPT);
        assert!(short["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .starts_with(rich["_kin"]["meaning"].as_str().unwrap()));
        assert!(short["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("their names need not match"));
        for index in 0..2 {
            for key in ["entity_id", "file", "name", "qualified_name", "kind"] {
                assert_eq!(short["results"][index][key], rich["results"][index][key]);
            }
        }
        // Include the added scope explanation, not only the grouped rows.
        assert!(short.to_string().len() * 3 < rich.to_string().len() * 2);
    }

    #[test]
    fn details_true_is_the_existing_rich_reply_and_fragments_stay_lossless() {
        let mut existing = json!({"content":[{"type":"text","text":fixture().to_string()}]});
        present(&mut existing, "semantic_search");
        let existing: Value =
            serde_json::from_str(existing["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(rendered(fixture(), true), existing);
        assert!(existing["not_held"][0].get("members").is_none());

        let mut fragment = fixture();
        fragment["record_fragment"] = json!({"record":"declarations","field":"why","text":"part"});
        assert_eq!(rendered(fragment.clone(), false), rendered(fragment, true));
    }

    #[test]
    fn uncertain_or_future_row_shapes_are_retained_verbatim() {
        let machine = fixture();
        let raw = machine
            .pointer("/edge_coverage/declarations/matching_unadmitted")
            .unwrap()
            .as_array()
            .unwrap()
            .clone();
        for change in [
            "owner",
            "reason",
            "empty_owner",
            "empty_name",
            "line_type",
            "identity_mismatch",
            "extra_field",
        ] {
            let mut raw = raw.clone();
            let mut plain = rendered(machine.clone(), true);
            for (raw, row) in raw
                .iter_mut()
                .zip(plain["not_held"].as_array_mut().unwrap())
            {
                match change {
                    "owner" | "reason" => {
                        raw.as_object_mut().unwrap().remove(change);
                    }
                    "empty_owner" => raw["owner"] = json!(""),
                    "empty_name" => {
                        raw["name"] = json!("");
                        row["name"] = json!("");
                    }
                    "line_type" => {
                        raw["line"] = json!("154");
                        row["line"] = json!("154");
                    }
                    "identity_mismatch" => raw["name"] = json!("different_name"),
                    "extra_field" => row["code"] = json!("extra information must survive"),
                    _ => unreachable!(),
                }
            }
            let expected = plain.clone();
            compact_unadmitted_declarations(&mut plain, &raw);
            assert_eq!(plain, expected, "{change}");
        }
        let mut plain = rendered(machine, true);
        let expected = plain.clone();
        compact_unadmitted_declarations(&mut plain, &raw[..raw.len() - 1]);
        assert_eq!(plain, expected, "unaligned census rows");
    }

    #[test]
    fn grouping_preserves_adjacent_order_and_separates_every_parent_key() {
        let seed = fixture()["edge_coverage"]["declarations"]["matching_unadmitted"][0].clone();
        let mut raw = Vec::new();
        for (group, field, value) in [
            (0, "owner", "Same"),
            (1, "owner", "Different"),
            (2, "file", "different.pyi"),
            (3, "kind", "function"),
            (4, "reason", "unknown_reason"),
            (5, "reason", "another_unknown_reason"),
            (6, "owner", "Same"),
        ] {
            for member in 0..4 {
                let mut row = seed.clone();
                row["owner"] = json!("Same");
                row[field] = json!(value);
                row["name"] = json!(format!("group_{group}_member_{member}"));
                raw.push(row);
            }
        }
        let mut machine = fixture();
        machine["edge_coverage"]["declarations"]["matching_unadmitted"] = json!(raw);
        machine["edge_coverage"]["declarations"]["matching_unadmitted_total"] = json!(28);
        let rich = rendered(machine.clone(), true);
        let short = rendered(machine, false);
        assert_eq!(short["not_held"].as_array().unwrap().len(), 7);
        assert_eq!(flatten(&short["not_held"]), rich["not_held"]);
        assert_eq!(short["not_held"][0]["owner"], short["not_held"][6]["owner"]);
        assert_ne!(
            short["not_held"][0]["members"],
            short["not_held"][6]["members"]
        );
        assert_eq!(short["not_held"][4]["why"], short["not_held"][5]["why"]);
    }

    #[test]
    fn groups_keep_duplicate_unicode_and_tool_spelled_identities_exact() {
        let mut machine = fixture();
        let rows = machine["edge_coverage"]["declarations"]["matching_unadmitted"]
            .as_array_mut()
            .unwrap();
        for row in rows.iter_mut() {
            row["owner"] = json!("find_references.Δ\\\"_wrapper");
            row["file"] = json!("src/Δ\\\".pyi");
            row["name"] = json!("__wrapped__Δ\\\"");
            row["line"] = json!(154);
        }
        let rich = rendered(machine.clone(), true);
        let short = rendered(machine, false);
        assert_eq!(short["not_held"].as_array().unwrap().len(), 1);
        assert_eq!(
            short["not_held"][0]["owner"],
            "find_references.Δ\\\"_wrapper"
        );
        assert_eq!(flatten(&short["not_held"]), rich["not_held"]);
        assert_eq!(short["not_held"][0]["count"], 17);
        assert!(short.to_string().len() < rich.to_string().len());
    }

    /// Class attributes named like the query are counted by class in a short
    /// reply, never listed and never an absence; details lists every one.
    #[test]
    fn class_attributes_are_counted_by_class_and_details_list_them() {
        let mut machine = fixture();
        let rows = machine["edge_coverage"]["declarations"]["matching_unadmitted"]
            .as_array_mut()
            .unwrap();
        for row in rows.iter_mut().take(10) {
            row["reason"] = json!(CLASS_ATTRIBUTE_REASON);
            row["kind"] = json!("attribute");
        }
        let rich = rendered(machine.clone(), true);
        let short = rendered(machine, false);
        assert_eq!(rich["not_held"].as_array().unwrap().len(), 17);
        assert!(rich.get(NOT_HELD_ATTRIBUTES_KEY).is_none());
        let folded = &short[NOT_HELD_ATTRIBUTES_KEY];
        assert_eq!(folded["count"], 10, "{short:#}");
        assert_eq!(
            folded["by_class"],
            json!({"_cached_wrapper": 7, "_cachedmethod_wrapper": 3})
        );
        assert!(folded["why"].as_str().unwrap().contains("details:true"));
        // The nested declarations stay listed, each placed by its owner.
        assert_eq!(flatten(&short["not_held"]).as_array().unwrap().len(), 7);
        assert_eq!(short["_kin"]["verdict"], COMPLETE_EXCEPT);
        let meaning = short["_kin"]["meaning"].as_str().unwrap();
        assert!(
            meaning.contains("10 class attributes named like it are counted"),
            "{meaning}"
        );
        // A search that found only attributes is complete except their count.
        let mut machine = fixture();
        for row in machine["edge_coverage"]["declarations"]["matching_unadmitted"]
            .as_array_mut()
            .unwrap()
        {
            row["reason"] = json!(CLASS_ATTRIBUTE_REASON);
        }
        machine["results"] = json!([]);
        machine["total_matches"] = json!(0);
        let short = rendered(machine, false);
        assert!(short.get("not_held").is_none(), "{short:#}");
        assert_eq!(short["_kin"]["verdict"], COMPLETE_EXCEPT, "{short:#}");
    }

    /// A listed declaration names its module and the entity around it,
    /// never its file; a re-export says what it re-exports.
    #[test]
    fn not_held_rows_name_their_module_and_enclosing_entity_never_a_path() {
        let rows = [
            json!({"file":"packages/x/src/forks/Config.custom.js","owner":null,
                "kind":"variable","reason":"module_binding_filtered","name":"measureInstance",
                "line":12,"module":"x.forks.Config.custom",
                "reexport_of":"$$$config.measureInstance"}),
            json!({"file":"packages/x/src/Fizz.js","owner":"renderFizz","kind":"function",
                "reason":"nested_in_callable","name":"measureInstance","line":40,
                "module":"x.Fizz","inside_id":"owner-id"}),
        ];
        let first = plain_not_held_row(&rows[0]);
        assert_eq!(first["module"], "x.forks.Config.custom");
        assert!(first.get("inside").is_none());
        assert!(first["why"]
            .as_str()
            .unwrap()
            .starts_with("re-export of `$$$config.measureInstance`"));
        let second = plain_not_held_row(&rows[1]);
        assert_eq!(second["inside"], "renderFizz");
        assert_eq!(second["inside_id"], "owner-id");
        assert_eq!(second["module"], "x.Fizz");
        for row in [first, second] {
            assert!(!row.to_string().contains("packages/"), "{row}");
            assert!(row.get("file").is_none());
            assert!(row.get("line").is_none());
        }
    }

    #[test]
    fn tiny_groups_and_non_name_search_modes_do_not_gain_bytes_or_groups() {
        let mut machine = fixture();
        machine["edge_coverage"]["declarations"]["matching_unadmitted"] = json!([
            {"name":"a","line":1,"kind":"attribute","file":"a","owner":"A","reason":"unknown"},
            {"name":"b","line":2,"kind":"attribute","file":"a","owner":"A","reason":"unknown"}]);
        machine["edge_coverage"]["declarations"]["matching_unadmitted_total"] = json!(2);
        let rich = rendered(machine.clone(), true);
        let short = rendered(machine, false);
        assert_eq!(short["not_held"], rich["not_held"]);
        assert_eq!(short["_kin"], rich["_kin"]);

        let mut plain = plain_answer_with_details(
            fixture().as_object().unwrap().clone(),
            "semantic_locate",
            false,
        );
        assert!(plain.get("not_held").is_none());
        plain = plain_answer_with_details(
            fixture().as_object().unwrap().clone(),
            "semantic_search",
            true,
        );
        assert!(plain["not_held"][0].get("members").is_none());
    }
}

#[cfg(test)]
mod smoke_reply_contract_tests {
    use super::*;

    fn rendered(machine: Value, tool: &str) -> Value {
        let mut result = json!({"content":[{"type":"text","text":machine.to_string()}]});
        present(&mut result, tool);
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn compact_overrides_keep_empty_counts_unmeasured_coverage_and_bounds() {
        for measured in [false, true] {
            let block = json!({
                "count":0,"proven":0,"overriders":[],
                "coverage":{"scope":"recorded_overrides_and_implements","recorded_closure":"complete",
                    "nested_definitions":if measured {"focal_file_measured"} else {"unmeasured"},
                    "repository_completeness":"unmeasured"},
            });
            let plain = plain_overrides(&block).expect("empty override answers stay visible");
            assert_eq!(plain["methods"], json!([]));
            assert_eq!(plain["count"], 0);
            assert_eq!(plain["coverage"], block["coverage"]);
            let codes: Vec<_> = plain["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .map(|g| g["code"].as_str().unwrap())
                .collect();
            assert_eq!(
                codes.contains(&"nested_definition_census_unmeasured"),
                !measured
            );
            assert!(codes.contains(&"repository_override_coverage_unmeasured"));
        }
        let block = json!({"count":1,"proven":0,"bounded":true,"nested_definitions":"unmeasured",
            "overriders":[{"entity_id":"weak","name":"Child.run","resolution":"name_only","proven":false,
                "overrides":"Middle.run","relation":"implements","projection":{"path":"child.py"}}]});
        let plain = plain_overrides(&block).unwrap();
        assert_eq!(plain["bounded"], true);
        for key in [
            "entity_id",
            "name",
            "resolution",
            "proven",
            "overrides",
            "relation",
        ] {
            assert_eq!(plain["methods"][0][key], block["overriders"][0][key]);
        }
        assert!(plain["methods"][0].get("file").is_none());
        assert!(plain["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["code"] == "override_closure_bounded"));
    }

    #[test]
    fn focused_override_reply_does_not_borrow_reference_completion() {
        let machine = json!({
            "view":"overrides","focal_entity":{"entity_id":"base","name":"Base.run"},
            "overridden_by":{"count":0,"proven":0,"overriders":[],"nested_definitions":"unmeasured"},
            "_kin":{"verdict":{"state":"certified","safe_to_conclude_absent":true},
                "page":{"answer_rows_complete":true,"has_more":false}},
        });
        let plain = rendered(machine, "find_references");
        assert_eq!(plain["view"], "overrides");
        assert_eq!(plain["_kin"]["verdict"], PARTIAL);
        assert!(plain["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("completeness is not measured"));
        assert_eq!(plain["overridden_by"]["methods"], json!([]));
        assert!(plain.get("references").is_none());
        assert!(plain["_kin"].get("page").is_none());
    }

    #[test]
    fn focused_override_continuation_retains_family_metadata_and_transport() {
        let machine = json!({
            "overridden_by":{"overriders":[{"entity_id":"child","name":"Child.run","resolution":"type_resolved","proven":true}]},
            "next_cursor":"next","record_fragment":{"record":"coverage","field":"detail","text":"part"},
            "_kin":{"reference_view":{"view":"overrides","focal_entity":{"entity_id":"base","name":"Base.run"},
                "overridden_by":{"count":5,"proven":4,"bounded":true,
                    "coverage":{"scope":"recorded_overrides_and_implements","recorded_closure":"bounded","repository_completeness":"unmeasured"}}},
                "page":{"has_more":true,"answer_rows_complete":true}},
        });
        let plain = rendered(machine.clone(), "find_references");
        assert_eq!(plain["entity"]["entity_id"], "base");
        assert_eq!(plain["overridden_by"]["count"], 5);
        assert_eq!(plain["overridden_by"]["methods"][0]["entity_id"], "child");
        assert_eq!(
            plain["overridden_by"]["coverage"]["recorded_closure"],
            "bounded"
        );
        assert_eq!(plain["record_fragment"], machine["record_fragment"]);
        assert_eq!(plain["next_cursor"], "next");
        assert!(plain["_kin"]["delivered"]
            .as_str()
            .unwrap()
            .contains("next_cursor"));
    }

    #[test]
    fn compact_source_preserves_member_scope_and_failure_diagnostics() {
        let fields = json!({
            "member_coverage_reason":"contains_targets_unavailable","member_coverage_scope":"recorded_contains",
            "member_counts":{"recorded":2,"returned":1,"missing":1},
            "members_unavailable":[{"id":"missing","reason":"member_entity_missing"}],
            "source_base_unavailable":"module_inventory_only","body_bytes_basis":"recorded_span",
        });
        let mut machine = fields.as_object().unwrap().clone();
        machine.extend(
            json!({"entity_id":"module","name":"module","members":[],
            "body_omitted":crate::container_shape::CONTAINER_SHAPE_REASON,
            "semantic_gap":crate::container_shape::MODULE_MEMBER_GAP,
            "_kin":{"verdict":{"state":"inconclusive"}}})
            .as_object()
            .unwrap()
            .clone(),
        );
        let plain = rendered(Value::Object(machine), "get_entity_source");
        for (key, value) in fields.as_object().unwrap() {
            assert_eq!(&plain[key], value, "{key}: {plain}");
        }
    }

    #[test]
    fn compact_unknown_sites_keep_each_typed_source_failure() {
        let codes = [
            "caller_has_no_source_coordinates",
            "caller_has_no_span",
            "caller_has_no_file_origin",
            "caller_independent_source_unavailable",
            "caller_absent_at_generation",
            "caller_source_budget_exhausted",
            "caller_graph_authority_unavailable",
            "caller_graph_store_read_failed",
            "caller_source_read_failed",
            "caller_source_unavailable",
        ];
        let rows: Vec<_> = codes
            .iter()
            .map(|code| {
                json!({"caller":"same-id","caller_name":"module",
            "callee":null,"expr_unavailable":code,"state":"unresolved"})
            })
            .collect();
        let left = left_to_check(&json!({"named":rows}), &json!({"name":"Base.run"}));
        let sites = left.groups[0]["sites"].as_array().unwrap();
        assert_eq!(
            sites.len(),
            codes.len(),
            "different causes must not collapse"
        );
        for (site, code) in sites.iter().zip(codes) {
            assert_eq!(site["expr_unavailable"], code);
            assert!(site.get("code").is_none());
        }
        assert!(left.groups[0]["why"]
            .as_str()
            .unwrap()
            .contains("coverage gaps"));
    }

    #[test]
    fn counts_only_context_never_promises_missing_entity_ids() {
        let machine = json!({
            "entities":[{"id":"focal", "name":"ctx", "section":"focal", "body_unavailable":"whole module", "body_complete":false}],
            "withheld":{"transitive_rows":106, "how":"read these IDs with get_entity_source"},
            "entities_withheld":63,
            "_kin":{"verdict":{"state":"certified", "safe_to_conclude_absent":true}}
        });
        let reply = rendered(machine, "get_context_pack");
        let delivered = reply["_kin"]["delivered"].as_str().unwrap();
        assert!(
            delivered.contains("106 rows") && delivered.contains("63 entities"),
            "{reply}"
        );
        assert!(
            delivered.contains("without listing their entity IDs"),
            "{reply}"
        );
        assert!(
            !delivered.contains("each row") && !delivered.contains("read any"),
            "{reply}"
        );
        assert!(
            !reply["withheld"]["how"]
                .as_str()
                .unwrap()
                .contains("source"),
            "{reply}"
        );
        assert_eq!(reply["withheld"]["transitive_rows"], 106);
        assert_eq!(reply["entities_withheld"], 63);
        assert!(reply["entities"][0].get("body_unavailable").is_some());
    }

    #[test]
    fn mixed_context_ledger_recovers_only_the_ids_actually_listed() {
        let reply = rendered(
            json!({"entities":[], "withheld":{
                "bodies":["body-id"], "rows":{"dependencies":["row-id"]},
                "rows_total":30, "transitive_rows":106},
                "_kin":{"verdict":{"state":"inconclusive","safe_to_conclude_absent":false}}
            }),
            "get_context_pack",
        );
        let delivered = reply["_kin"]["delivered"].as_str().unwrap();
        assert!(
            delivered.contains("read the listed IDs with source"),
            "{reply}"
        );
        assert!(
            delivered.contains("other rows whose IDs are not listed"),
            "{reply}"
        );
        assert!(!delivered.contains("each row"), "{reply}");
        assert_eq!(reply["withheld"]["bodies"], json!(["body-id"]));
        assert_eq!(reply["withheld"]["rows_total"], 30);
    }

    #[test]
    fn ranking_reports_the_active_card_or_lexical_route() {
        for (ranked_by, arm, state) in [
            (
                "entity cards and the graph; this setup uses no search vectors",
                "cards",
                "not_used",
            ),
            (
                "lexical and graph signals; embedding coverage could not be read",
                "fused",
                "unknown",
            ),
            (
                "vector similarity, lexical and graph signals",
                "fused",
                "complete",
            ),
        ] {
            let reply = rendered(
                json!({"query":"request context", "entities":[{"id":"one","name":"pop"}],
                    "ranked_by":ranked_by,"retrieval_arm":arm,
                    "semantic_coverage":{"embedding_state":state},"next_cursor":"opaque", "total_ranked":9,
                    "_kin":{"verdict":{"state":"certified","safe_to_conclude_absent":false}}
                }),
                "semantic_locate",
            );
            assert_eq!(reply["_kin"]["searched"], format!("Ranked on {ranked_by}."));
            assert_eq!(reply["results"][0]["entity_id"], "one");
            assert_eq!(reply["next_cursor"], "opaque");
            assert_eq!(reply["total"], 9);
        }
    }

    #[test]
    fn missing_ranking_provenance_does_not_invent_vectors() {
        let reply = rendered(
            json!({"query":"request", "entities":[],
                "_kin":{"verdict":{"state":"certified","safe_to_conclude_absent":false}}
            }),
            "semantic_locate",
        );
        assert!(reply["_kin"]["searched"]
            .as_str()
            .unwrap()
            .contains("does not establish whether search vectors were used"));
    }

    #[test]
    fn identical_import_context_is_losslessly_shared_by_entity_identity() {
        let hit = |id: &str, file: &str, field: &str, excerpt: &str| {
            json!({
                "entity_id":id,"name":id,"file":file,"start_line":10,"end_line":20,
                "line":if field == "file_import_context" { Value::Null } else { json!(11) },
                "line_confidence":if field == "file_import_context" { "entity_span_only" } else { "exact" },
                "matched_field":field,"excerpt":excerpt
            })
        };
        let reply = rendered(
            json!({"literal":"_app_ctx","total_matching":5,"page_offset":16,"next_cursor":"same-cursor",
                "hits":[hit("a","same.py","file_import_context","flask globals _app_ctx"),
                    hit("b","same.py","file_import_context","flask globals _app_ctx"),
                    hit("c","other.py","file_import_context","flask globals _app_ctx"),
                    hit("d","same.py","code_preview","_app_ctx.pop()"),
                    hit("e","same.py","file_import_context","different preview")],
                "_kin":{"verdict":{"state":"certified","safe_to_conclude_absent":false}}
            }),
            crate::handlers::lexical::TOOL_NAME,
        );
        let rows = reply["hits"].as_array().unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row["entity_id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c", "d", "e"]
        );
        assert_eq!(reply["total"], 5);
        assert_eq!(reply["next_cursor"], "same-cursor");
        assert!(rows[0].get("line").is_none() && rows[0].get("within_lines").is_none());
        assert_eq!(rows[0]["at"], "recorded context; lexical scope unproven");
        assert!(reply["_kin"]["searched"]
            .as_str()
            .unwrap()
            .contains("not exact import syntax"));
        assert!(rows.iter().all(|row| row.get("about_code").is_none()));
        assert_eq!(rows[1]["same_context_as"], "a");
        assert!(rows[1].get("code").is_none());
        assert_eq!(
            rows[1]
                .get("same_context_as")
                .and_then(Value::as_str)
                .and_then(|id| rows.iter().find(|row| row["entity_id"] == id))
                .unwrap()["code"],
            rows[0]["code"]
        );
        assert_eq!(rows[2]["same_context_as"], "a");
        assert!(rows[4].get("code").is_some());
        assert!(rows[3].get("line_in_entity").is_none());
        assert_eq!(rows[3]["code"], "_app_ctx.pop()");
    }

    #[test]
    fn held_text_discloses_match_options_test_scope_and_semantic_gaps() {
        let reading =
            json!({"modules": 1, "artifacts_text": 1, "modules_with_unverified_parse": 1});
        assert!(crate::answer_account::held_text_open(&reading));
        let (meaning, searched) = held_text_meaning(
            "n.*",
            1,
            &reading,
            &json!({"pattern":true,"ignore_case":true,"include_tests":true}),
        );
        assert!(meaning.contains("no current full-parse certificate"));
        assert!(searched.contains("regex with Unicode case folding"));
        assert!(searched.contains("test code included"));
        let row = plain_literal_hit(
            &json!({"entity_id":"artifact-id", "name":"NOTICE", "module":"project", "kind":"text", "held_as":"text", "matched_field":"text", "line_in_entity":2, "excerpt":"bounded marker"}),
        );
        assert_eq!(row["held_as"], "text");
        assert_eq!(row["module"], "project");
        assert_eq!(row["code"], "bounded marker");
        assert!(row.get("source_base").is_none());
    }

    #[test]
    fn held_text_miss_names_only_searched_addresses_and_never_proves_full_code_absence() {
        let coverage = json!({"modules":1,"tests_left_out":1});
        assert!(!crate::answer_account::held_text_open(&coverage));
        let (meaning, searched) =
            held_text_meaning("needle", 0, &coverage, &json!({"include_tests":false}));
        assert!(
            meaning.starts_with(
                "Answer: \"needle\" is in nothing Kin holds as code or text outside test code. Kin read every one of its 1 parsed module, and none holds it."
            ),
            "{meaning}"
        );
        assert!(searched.contains("test-role entities excluded"));
        assert!(crate::answer_account::held_text_open(
            &json!({"authority_unavailable":true})
        ));
    }

    #[test]
    fn literal_scope_and_context_gaps_preserve_rows_without_promising_source_evidence() {
        let reply = rendered(
            json!({"literal":"needle","total_matching":2,"page_offset":0,"include_tests":false,
                "hits":[
                    {"entity_id":"local", "name":"caller", "matched_field":"body_preview",
                        "line_in_entity":2, "excerpt":"return needle(value)"},
                    {"entity_id":"context", "name":"associated", "semantic_gap":"entity_local_match_unavailable"}
                ],
                "_kin":{"verdict":{"state":"certified","safe_to_conclude_absent":false}}}),
            crate::handlers::lexical::TOOL_NAME,
        );
        assert_eq!(reply["include_tests"], false);
        assert_eq!(reply["total"], 2);
        assert_eq!(reply["hits"].as_array().unwrap().len(), 2);
        assert_eq!(reply["hits"][0]["entity_id"], "local");
        assert_eq!(reply["hits"][0]["code"], "return needle(value)");
        assert_eq!(reply["hits"][1]["entity_id"], "context");
        assert_eq!(
            reply["hits"][1]["semantic_gap"],
            "entity_local_match_unavailable"
        );
        assert!(reply["hits"][1]["note"]
            .as_str()
            .unwrap()
            .contains("may not contain this match"));
        assert!(reply["_kin"]["searched"]
            .as_str()
            .unwrap()
            .contains("Test-role entities were excluded"));
    }

    #[test]
    fn import_field_hits_with_exact_source_enrichment_keep_the_line_and_code() {
        let rows = (0..2).map(|n| json!({"entity_id":format!("caller-{n}"),"name":"callback",
            "file":"same.py","line":20,"line_confidence":"exact", "matched_field":"file_import_context",
            "excerpt":"return needle()"})).collect::<Vec<_>>();
        let reply = rendered(
            json!({"literal":"needle","total_matching":2,"page_offset":0,"hits":rows,
            "_kin":{"verdict":{"state":"certified","safe_to_conclude_absent":false}}}),
            crate::handlers::lexical::TOOL_NAME,
        );
        for row in reply["hits"].as_array().unwrap() {
            assert!(row.get("line").is_none());
            assert!(row.get("line_in_entity").is_none());
            assert_eq!(row["code"], "return needle()");
            assert!(row.get("at").is_none() && row.get("same_context_as").is_none());
        }
        assert_eq!(reply["_kin"]["searched"], LITERAL_SCOPE);
    }

    #[test]
    fn many_unicode_import_previews_stay_bounded_through_finalize_and_present() {
        use crate::server::{present_for_client, McpServerConfig};
        use crate::types::{JsonRpcResponse, ToolCallParams};
        for unique in [true, false] {
            for cap in [2000, 4000, 12000] {
                let rows = (0..80).map(|n|json!({
                    "entity_id":format!("00000000-0000-4000-8000-{n:012}"), "name":format!("callback_{n}"),
                    "kind":"function","file":if unique { format!("src/🧭{n}.py") } else { "src/🧭same.py".to_string() },
                    "line":null,"line_confidence":"entity_span_only","start_line":10,"end_line":20,
                    "matched_field":"file_import_context","excerpt":format!("from package import needle 🧭é {}",if unique { n } else { 0 })
                })).collect::<Vec<_>>();
                let payload = json!({"literal":"needle","kind":null,"hits":rows,"page_offset":0,
                    "total_matching":80,"matching_snapshot":"0123456789abcdef0123456789abcdef01234567","order":"entity_id"});
                let params = json!({"name":"locate","arguments":{"mode":"literal","query":"needle","max_output_bytes":cap}});
                let mut call: ToolCallParams = serde_json::from_value(params.clone()).unwrap();
                crate::tool_invocation::expand(&mut call, None).unwrap();
                crate::verbs::resolve(&mut call.name, &mut call.arguments).unwrap();
                crate::agent_belt::apply_belt_defaults(&call.name, &mut call.arguments);
                let budget = crate::budget::ResponseBudget::from_arguments(&call.arguments);
                assert_eq!(budget.max_chars, cap);
                let machine_result = crate::envelope::finalize_bounded(
                    crate::ToolCallResult::text(payload.to_string()),
                    crate::envelope::Envelope::offline(),
                    crate::handlers::lexical::TOOL_NAME,
                    &budget,
                );
                let machine_wire = serde_json::to_value(&machine_result).unwrap();
                let machine: Value =
                    serde_json::from_str(machine_wire["content"][0]["text"].as_str().unwrap())
                        .unwrap();
                assert!(
                    machine["hits"]
                        .as_array()
                        .is_some_and(|rows| !rows.is_empty()),
                    "{machine}"
                );
                let response = JsonRpcResponse::success(
                    Some(json!(1)),
                    serde_json::to_value(&machine_result).unwrap(),
                );
                let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":params})
                    .to_string();
                let response = present_for_client(
                    response,
                    &request,
                    &McpServerConfig {
                        plain_replies: true,
                        agent_belt: true,
                        ..Default::default()
                    },
                );
                let result = response.result.unwrap();
                let text = result["content"][0]["text"].as_str().unwrap();
                assert!(
                    text.len() <= cap,
                    "unique={unique}, cap={cap}, actual={} bytes: {text}",
                    text.len()
                );
                let reply: Value = serde_json::from_str(text).unwrap();
                let hits = reply["hits"].as_array().unwrap();
                let before = machine["hits"].as_array().unwrap();
                assert_eq!(hits.len(), before.len());
                assert_eq!(
                    hits.iter().map(|r| &r["entity_id"]).collect::<Vec<_>>(),
                    before.iter().map(|r| &r["entity_id"]).collect::<Vec<_>>()
                );
                assert_eq!(reply["total"], machine["total_matching"]);
                assert_eq!(reply["next_cursor"], machine["next_cursor"]);
                for (hit, source) in hits.iter().zip(before) {
                    let code = hit
                        .get("code")
                        .or_else(|| {
                            hit.get("same_context_as")
                                .and_then(Value::as_str)
                                .and_then(|id| hits.iter().find(|row| row["entity_id"] == id))
                                .and_then(|row| row.get("code"))
                        })
                        .unwrap();
                    assert_eq!(code, &source["excerpt"]);
                }
            }
        }
    }
}

#[cfg(test)]
mod unknown_call_scope_tests {
    use super::*;

    fn expand_sites(sites: &[Value]) -> Vec<Value> {
        sites
            .iter()
            .flat_map(|site| {
                let mut original = site.clone();
                let count = original
                    .as_object_mut()
                    .unwrap()
                    .remove("occurrences")
                    .and_then(|count| count.as_u64())
                    .unwrap_or(1);
                std::iter::repeat_n(original, count as usize)
            })
            .collect()
    }

    fn focal() -> Value {
        json!({"id":"focal", "name":"Container.acquire", "kind":"Method"})
    }

    fn unknown(id: &str) -> Value {
        json!({"caller":id,"caller_name":"large_module","line_in_entity":null,
            "callee":null,"state":"unresolved", "reason":kin_model::call_site_reading::REACH_TEXT_UNKNOWN,
            "expr_unavailable":"caller_source_unavailable"})
    }

    fn known(id: &str) -> Value {
        json!({"caller":id,"caller_name":"callback","line_in_entity":6,
            "callee":"acquire","state":"unresolved", "reason":kin_model::call_site_reading::REACH_CALLEE_SPELLS,
            "expr":"ctx.acquire(Item)","receiver":"ctx","receiver_hint":"unknown",
            "receiver_binding_missing":{"step":"parameter","name":"ctx","reason":"parameter_type_unknown"}})
    }

    #[test]
    fn identical_uninspectable_entries_round_trip_without_reordering() {
        let a = json!({"caller_id":"a","reason":"text_unknown","line_in_caller":null});
        let b = json!({"caller_id":"b","reason":"text_unknown","line_in_caller":null});
        let original = vec![a.clone(), a.clone(), b.clone(), a.clone(), a];
        let (sites, compacted) = compact_uninspectable_sites(original.clone());
        assert!(compacted);
        assert_eq!(sites.len(), 3);
        assert_eq!(sites[0]["occurrences"], 2);
        assert_eq!(sites[1], b);
        assert_eq!(sites[2]["occurrences"], 2);
        assert_eq!(expand_sites(&sites), original);
    }

    #[test]
    fn tiny_identical_gaps_keep_original_bytes_when_notice_would_grow_them() {
        let row = json!({"caller":"a"});
        let left = left_to_check(&json!({"named":[row.clone(),row.clone()]}), &focal());
        let mut before = left_to_check(&json!({"named":[row]}), &focal());
        before.groups[0]["sites"] = json!([{"caller_id":"a"},{"caller_id":"a"}]);
        before.groups[0][GAP_REASONS_KEY] = json!({"callee_name_unknown": 2});
        assert_eq!(left.total, 2);
        assert_eq!(left.groups, before.groups);
        assert!(!left.groups[0]["why"]
            .as_str()
            .unwrap()
            .contains("occurrences"));
    }

    #[test]
    fn projected_field_variants_and_supplied_code_are_never_merged() {
        let base = json!({"caller_id":"same","reason":"text_unknown"});
        let mut variants = vec![base.clone()];
        for (key, value) in [
            ("line_in_caller", Value::Null),
            ("line_in_caller", json!(3)),
            ("offset", json!(20)),
            ("reason", json!("name_unknown")),
            ("receiver", json!("ctx")),
            ("callee", json!("acquire")),
            ("expr_unavailable", json!("caller_source_unavailable")),
            ("code", Value::Null),
            ("code", json!("")),
        ] {
            let mut site = base.clone();
            site[key] = value;
            variants.push(site);
        }
        let (sites, compacted) = compact_uninspectable_sites(variants.clone());
        assert!(!compacted);
        assert_eq!(sites, variants);
        let code = json!({"caller_id":"same","code":"ctx.acquire()","reason":"text_unknown"});
        let (sites, compacted) = compact_uninspectable_sites(vec![code.clone(), code.clone()]);
        assert!(!compacted);
        assert_eq!(sites, vec![code.clone(), code]);
        // A future producer that already owns this key is left untouched.
        let reserved = json!({"caller_id":"same","occurrences":7});
        let (sites, compacted) =
            compact_uninspectable_sites(vec![reserved.clone(), reserved.clone()]);
        assert!(!compacted);
        assert_eq!(sites, vec![reserved.clone(), reserved]);
    }

    #[test]
    fn grouped_gap_multiplicity_keeps_raw_totals_and_observed_call_rows() {
        let rows = json!([
            unknown("a"),
            unknown("a"),
            known("b"),
            known("b"),
            unknown("a"),
            unknown("a")
        ]);
        let left = left_to_check(&json!({"named":rows,"named_withheld":5}), &focal());
        assert_eq!(left.total, 11);
        // The observed calls lead; every gap follows in one group, its reason
        // said once, and the withheld count after them.
        assert_eq!(left.groups.len(), 3);
        assert_eq!(left.gap_group, Some(1));
        assert_eq!(left.groups[0]["sites"].as_array().unwrap().len(), 2);
        assert!(left.groups[0]["sites"][0].get("occurrences").is_none());
        assert_eq!(left.groups[1]["sites"][0]["occurrences"], 4);
        assert_eq!(
            left.groups[1][GAP_REASONS_KEY],
            json!({"caller_source_unavailable": 4})
        );
        assert!(left.groups[1]["why"]
            .as_str()
            .unwrap()
            .contains("not observed calls"));
        let ids = left.groups[..2]
            .iter()
            .flat_map(|group| expand_sites(group["sites"].as_array().unwrap()))
            .map(|site| site["caller_id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["b", "b", "a", "a", "a", "a"]);
        assert!(left.groups[2]["why"]
            .as_str()
            .unwrap()
            .starts_with("5 more retained candidate sites"));
    }

    #[test]
    fn a_collapsed_site_counts_every_entry_it_stands_for() {
        let left = left_to_check(
            &json!({"named":[{"caller":"spec","caller_name":"page.spec","callee":null,
                "line_in_entity":null,"state":"unresolved","occurrences":41}]}),
            &json!({"name":"setEnabled"}),
        );
        assert_eq!(left.total, 41);
        assert_eq!(left.groups[0]["sites"][0]["occurrences"], 41);
    }

    /// A left-to-check call site names the module its caller sits in. With
    /// the caller's name alone, a reader naming `test_move_cursor_to_column`
    /// in an answer had to guess its module and wrote the focal's own.
    #[test]
    fn a_left_to_check_site_names_its_callers_module() {
        let left = left_to_check(
            &json!({"named":[
                {"caller":"test-id","caller_name":"test_move_cursor_to_column",
                 "module":"tests.test_win32_console","line_in_entity":4,
                 "expr":"term.move_cursor_to_column(5)","callee":"move_cursor_to_column",
                 "state":"unresolved"},
                {"caller":"bare-id","caller_name":"elsewhere","module":"",
                 "line_in_entity":2,"expr":"x.move_cursor_to_column(1)",
                 "callee":"move_cursor_to_column","state":"unresolved"}]}),
            &json!({"name":"LegacyWindowsTerm.move_cursor_to_column"}),
        );
        let sites: Vec<&Value> = left
            .groups
            .iter()
            .flat_map(|group| group["sites"].as_array().into_iter().flatten())
            .collect();
        let named = |caller: &str| {
            *sites
                .iter()
                .find(|site| site["caller"] == caller)
                .unwrap_or_else(|| panic!("{caller}: {sites:?}"))
        };
        assert_eq!(
            named("test_move_cursor_to_column")[crate::answer_account::CALLER_MODULE_KEY],
            "tests.test_win32_console"
        );
        // An unknown module is left out, never written as an empty one.
        assert!(named("elsewhere")
            .get(crate::answer_account::CALLER_MODULE_KEY)
            .is_none());
    }

    #[test]
    fn a_site_a_proven_row_answers_is_not_left_to_check() {
        let rows = vec![
            json!({"entity_id":"commit-host-text","name":"commitHostTextUpdate",
            "resolution":"import_scoped",
            "sites":[{"expr":"commitTextUpdate(textInstance, oldText, newText)","line_in_entity":4}]}),
        ];
        let candidates = json!({"named":[
            {"caller":"commit-host-text","caller_name":"commitHostTextUpdate","line_in_entity":4,
             "expr":"commitTextUpdate(textInstance, oldText, newText)","callee":"commitTextUpdate"},
            {"caller":"commit-host-text","caller_name":"commitHostTextUpdate","line_in_entity":9,
             "expr":"commitTextUpdate(other, a, b)","callee":"commitTextUpdate"},
            {"caller":"elsewhere","caller_name":"elsewhere","line_in_entity":4,
             "expr":"commitTextUpdate(textInstance, oldText, newText)","callee":"commitTextUpdate"}]});
        let kept = without_proven_sites(&candidates, &rows);
        let named = kept["named"].as_array().unwrap();
        // Same caller, line and code is answered; another line or caller stays.
        assert_eq!(named.len(), 2);
        assert!(named
            .iter()
            .all(|site| site["line_in_entity"] != 4 || site["caller"] != "commit-host-text"));
        // A row matched by name alone answers nothing.
        let mut weak = rows.clone();
        weak[0]["resolution"] = json!("name_only");
        assert_eq!(
            without_proven_sites(&candidates, &weak)["named"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn an_evidence_page_after_every_site_says_it_adds_no_callers() {
        let page = |next_record: u64| {
            json!({"focal_entity":focal(),
                "references":[{"entity_id":"proven","name":"verified_caller","resolution":"type_resolved"}],
                "total_upstream":1,"next_cursor":"frozen-evidence-page",
                "call_site_candidates":{"named":[unknown("module"),unknown("module")]},
                "_kin":{"verdict":{"state":"inconclusive","safe_to_conclude_absent":false},
                    "page":{"kind":"references","has_more":true,"answer_rows_complete":true,
                        "answer_rows_has_more":false,"evidence_has_more":true,
                        "total_references":1,"returned_references":1,
                        "total_answer_records":1,"total_candidates":2,"next_record":next_record}}})
        };
        let delivered = |machine: Value| {
            let mut result = json!({"content":[{"type":"text","text":machine.to_string()}]});
            present(&mut result, "find_references");
            let reply: Value =
                serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
            reply["_kin"]["delivered"].as_str().unwrap().to_string()
        };
        // Answer row and both sites delivered: what remains is coverage counts.
        assert!(delivered(page(3)).contains("every unresolved site are shown"));
        assert!(!delivered(page(3)).contains("may still add callers"));
        // A site still on a later page is said as what later pages hold,
        // never as an invitation: the rows are complete, and the pages add
        // no reference row.
        assert!(delivered(page(2)).contains("never a reference row"));
        assert!(delivered(page(2)).contains("answer rows are complete"));
        assert!(!delivered(page(2)).contains("may still add callers"));
        // The proven row says how it was proven.
        let mut result = json!({"content":[{"type":"text","text":page(3).to_string()}]});
        present(&mut result, "find_references");
        let reply: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(reply["references"][0]["proof"], "target resolved by type");
    }

    #[test]
    fn final_gap_compaction_keeps_proof_listed_gaps_and_frozen_page_metadata() {
        let machine = json!({"focal_entity":focal(),
            "references":[{"entity_id":"proven","name":"verified_caller","resolution":"type_resolved"}],
            "total_upstream":1,"next_cursor":"frozen-evidence-page",
            "call_site_candidates":{"named":[unknown("module"),unknown("module")],"named_withheld":7},
            "_kin":{"verdict":{"state":"inconclusive","safe_to_conclude_absent":false},
                "page":{"kind":"references","has_more":true,"answer_rows_complete":true,
                    "answer_rows_has_more":false,"evidence_has_more":true,
                    "total_references":1,"returned_references":1}}});
        let mut result = json!({"content":[{"type":"text","text":machine.to_string()}]});
        present(&mut result, "find_references");
        let reply: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        // The existing presenter says complete-except when its open places
        // are listed; compaction must not promote that to full completeness.
        assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT);
        assert!(reply["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("9 other places could reach it"));
        assert!(reply["_kin"]["delivered"]
            .as_str()
            .unwrap()
            .contains("Later evidence pages hold only more unresolved sites"));
        assert_eq!(
            reply["_kin"]["page"],
            json!({
                "recorded_rows_exhausted":true,"recorded_rows_has_more":false,
                "evidence_has_more":true,"recorded_references_total":1,"recorded_references_returned":1,
            })
        );
        assert_eq!(reply["next_cursor"], machine["next_cursor"]);
        assert_eq!(reply["references"][0]["entity_id"], "proven");
        assert_eq!(reply[LEFT_TO_CHECK_KEY][0]["sites"][0]["occurrences"], 2);
        assert!(reply[LEFT_TO_CHECK_KEY][1]["why"]
            .as_str()
            .unwrap()
            .starts_with("7 more retained candidate sites"));
        // A distinct projected receiver keeps both gap rows uncompressed,
        // with the same identities, counts, verdict and coverage inputs.
        let mut uncompressed = machine.clone();
        uncompressed["call_site_candidates"]["named"][1]["receiver"] = json!("ctx");
        let uncompressed =
            plain_answer(uncompressed.as_object().unwrap().clone(), "find_references");
        assert_eq!(reply["_kin"], uncompressed["_kin"]);
        assert_eq!(reply["references"], uncompressed["references"]);
        assert_eq!(reply["next_cursor"], uncompressed["next_cursor"]);
        assert_eq!(
            uncompressed[LEFT_TO_CHECK_KEY][0]["sites"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn unicode_duplicate_gap_page_is_bounded_after_finalize_and_present() {
        use crate::server::{present_for_client, McpServerConfig};
        use crate::types::JsonRpcResponse;
        let mut row = unknown("module");
        row["caller_name"] = json!("🧭large_module_é");
        let payload = json!({"focal_entity":focal(),"references":[],"total_upstream":0,
            "next_cursor":"frozen-next","call_site_candidates":{"named":vec![row;48]},
            "_kin":{"verdict":{"state":"inconclusive","safe_to_conclude_absent":false},
                "page":{"kind":"references","has_more":true,"answer_rows_complete":true,
                    "answer_rows_has_more":false,"evidence_has_more":true}}});
        let cap = 12000;
        let arguments =
            std::collections::HashMap::from([("max_output_bytes".to_string(), json!(cap))]);
        let budget = crate::budget::ResponseBudget::from_arguments(&arguments);
        let finalized = crate::envelope::finalize_bounded(
            crate::ToolCallResult::text(payload.to_string()),
            crate::envelope::Envelope::offline(),
            "find_references",
            &budget,
        );
        let result = serde_json::to_value(finalized).unwrap();
        let before: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        let original_rows = before["call_site_candidates"]["named"].as_array().unwrap();
        assert_eq!(original_rows.len(), 48);
        let serve = |arguments: Value| -> Value {
            let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"refs","arguments":arguments}})
            .to_string();
            let response = present_for_client(
                JsonRpcResponse::success(Some(json!(1)), result.clone()),
                &request,
                &McpServerConfig {
                    plain_replies: true,
                    agent_belt: true,
                    ..Default::default()
                },
            );
            let result = response.result.unwrap();
            let text = result["content"][0]["text"].as_str().unwrap();
            assert!(text.len() <= cap, "{} bytes: {text}", text.len());
            serde_json::from_str(text).unwrap()
        };
        let reply = serve(json!({"entity_id":"focal","max_output_bytes":cap}));
        // The short form counts the 48 gap entries by reason and says how to
        // list them; details list them, caller and count.
        let folded = &reply[LEFT_TO_CHECK_KEY][0];
        assert!(folded.get("sites").is_none(), "{folded}");
        assert_eq!(folded["retained_entries"], 48);
        assert_eq!(
            folded[GAP_REASONS_KEY],
            json!({"caller_source_unavailable": 48})
        );
        assert!(folded["why"].as_str().unwrap().contains("details:true"));
        let listed = plain_answer(before.as_object().unwrap().clone(), "find_references");
        let site = &listed[LEFT_TO_CHECK_KEY][0]["sites"][0];
        assert_eq!(site["occurrences"], 48);
        assert_eq!(site["caller"], "🧭large_module_é");
        // Every answer row is delivered and the verdict is complete beside
        // the counted places, whether or not details list them, so the reply
        // ends on them either way: the cursor to later evidence, its page
        // accounting and its sentence stay in the plain answer the pager
        // sized, and in the machine reply the full profile serves.
        let detailed = serve(json!({"entity_id":"focal","max_output_bytes":cap,"details":true}));
        assert_eq!(reply["_kin"]["verdict"], COMPLETE);
        assert_eq!(detailed["_kin"]["verdict"], COMPLETE);
        for (form, reply) in [("short", &reply), ("details", &detailed)] {
            assert!(reply.get("next_cursor").is_none(), "{form}: {reply}");
            for key in ["page", "delivered"] {
                assert!(reply["_kin"].get(key).is_none(), "{form} {key}: {reply}");
            }
        }
        // Details change the rows: the entries the short form counts are
        // listed, caller and count.
        assert_eq!(
            detailed[LEFT_TO_CHECK_KEY][0]["sites"],
            listed[LEFT_TO_CHECK_KEY][0]["sites"]
        );
        assert_eq!(listed["next_cursor"], before["next_cursor"]);
        assert_eq!(
            listed["_kin"]["page"],
            json!({
                "recorded_rows_exhausted":true,"recorded_rows_has_more":false,
                "evidence_has_more":true,
            })
        );
        for (public, internal) in [
            ("recorded_rows_exhausted", "answer_rows_complete"),
            ("recorded_rows_has_more", "answer_rows_has_more"),
            ("evidence_has_more", "evidence_has_more"),
        ] {
            assert_eq!(
                listed["_kin"]["page"][public],
                before["_kin"]["page"][internal]
            );
        }
        assert!(listed["_kin"]["delivered"]
            .as_str()
            .unwrap()
            .contains("Later evidence pages hold only more unresolved sites"));
    }

    #[test]
    fn unread_text_without_a_callee_is_not_an_observed_named_call() {
        let left = left_to_check(&json!({"named":[unknown("module")]}), &focal());
        assert_eq!(left.total, 1);
        let group = &left.groups[0];
        let why = group["why"].as_str().unwrap();
        assert!(why.starts_with("Unresolved coverage gaps:"), "{group}");
        assert!(
            why.contains("not observed calls to Container.acquire"),
            "{group}"
        );
        assert!(!why.contains("Calls to a method named acquire"), "{group}");
        let site = &group["sites"][0];
        assert_eq!(site["caller_id"], "module");
        assert_eq!(
            site["reason"],
            kin_model::call_site_reading::REACH_TEXT_UNKNOWN
        );
        assert_eq!(site["expr_unavailable"], "caller_source_unavailable");
        assert!(site.get("callee").is_none() && site.get("code").is_none());
    }

    #[test]
    fn observed_calls_lead_and_unknown_scope_follows_with_every_id_in_order() {
        let rows = json!([
            known("a"),
            unknown("b"),
            known("c"),
            unknown("d"),
            unknown("e")
        ]);
        let left = left_to_check(&json!({"named":rows.clone()}), &focal());
        assert_eq!(left.total, 5);
        let ids: Vec<_> = left
            .groups
            .iter()
            .flat_map(|group| group["sites"].as_array().unwrap())
            .map(|site| site["caller_id"].as_str().unwrap())
            .collect();
        // Sites that spell the name answer the question, so they come first,
        // each kind in its own order; no id is lost or repeated.
        assert_eq!(ids, vec!["a", "c", "b", "d", "e"]);
        assert_eq!(left.groups.len(), 2);
        assert!(left.groups[0]["why"]
            .as_str()
            .unwrap()
            .contains("Calls to a method named acquire"));
        assert!(left.groups[1]["why"]
            .as_str()
            .unwrap()
            .contains("coverage gaps"));
        assert_eq!(left.groups[0]["sites"][0]["receiver"], "ctx");
        assert_eq!(
            left.groups[0]["sites"][0]["receiver_binding_missing"],
            rows[0]["receiver_binding_missing"]
        );
    }

    #[test]
    fn known_unbound_receiver_guidance_and_partial_verdict_stay_intact() {
        let machine = json!({"focal_entity":focal(), "references":[{"entity_id":"proven","name":"caller"}],
            "total_upstream":1, "call_site_candidates":{"named":[known("possible"),unknown("module")]},
            "_kin":{"verdict":{"state":"inconclusive","safe_to_conclude_absent":false,
                "limiting_factor":"local_binding_unproven: unresolved local names"},
                "page":{"kind":"references","has_more":false,"answer_rows_complete":true,
                    "answer_rows_has_more":false,"evidence_has_more":false,"total_references":1,"returned_references":1}}});
        let reply = plain_answer(machine.as_object().unwrap().clone(), "find_references");
        assert_eq!(reply["_kin"]["verdict"], PARTIAL);
        assert_eq!(reply["references"][0]["entity_id"], "proven");
        assert_eq!(reply["_kin"]["page"]["recorded_rows_exhausted"], true);
        let groups = reply[LEFT_TO_CHECK_KEY].as_array().unwrap();
        assert_eq!(
            groups
                .iter()
                .map(|g| g["sites"].as_array().unwrap().len())
                .sum::<usize>(),
            2
        );
        assert!(groups[0]["why"]
            .as_str()
            .unwrap()
            .contains("only if its object could be a Container"));
        assert_eq!(groups[0]["sites"][0]["code"], "ctx.acquire(Item)");
        assert_eq!(groups[0]["sites"][0]["line_in_caller"], 6);
        assert_eq!(groups[1]["sites"][0]["caller_id"], "module");
        assert!(groups[1]["why"]
            .as_str()
            .unwrap()
            .contains("caller_id alone does not prove a call"));
    }

    #[test]
    fn missing_callee_never_borrows_the_focal_name_even_without_a_reason() {
        for callee in [Value::Null, json!("")] {
            let row = json!({"caller":"legacy","callee":callee,"state":"unresolved","line_in_entity":null});
            let left = left_to_check(&json!({"named":[row]}), &focal());
            assert_eq!(left.total, 1);
            assert!(left.groups[0]["why"]
                .as_str()
                .unwrap()
                .contains("coverage gaps"));
            assert!(left.groups[0]["sites"][0].get("callee").is_none());
        }
        let left = left_to_check(
            &json!({"named":[{"caller":"legacy","state":"unresolved"}]}),
            &focal(),
        );
        assert_eq!(left.total, 1);
        assert!(left.groups[0]["sites"][0].get("callee").is_none());
    }

    #[test]
    fn a_known_callee_does_not_erase_unknown_text_or_target_name_scope() {
        for reason in [
            kin_model::call_site_reading::REACH_TEXT_UNKNOWN,
            kin_model::call_site_reading::REACH_NAME_UNKNOWN,
        ] {
            let mut row = known("mixed");
            row["reason"] = json!(reason);
            let left = left_to_check(&json!({"named":[row]}), &focal());
            assert_eq!(left.total, 1);
            assert!(left.groups[0]["why"]
                .as_str()
                .unwrap()
                .starts_with("Unresolved coverage gaps:"));
            assert!(left.groups[0]["why"]
                .as_str()
                .unwrap()
                .contains("target-name scope"));
            assert_eq!(left.groups[0]["sites"][0]["reason"], reason);
            assert_eq!(left.groups[0]["sites"][0]["callee"], "acquire");
            assert_eq!(left.groups[0]["sites"][0]["code"], "ctx.acquire(Item)");
        }
    }

    #[test]
    fn withheld_candidates_are_counted_without_becoming_observed_calls() {
        let left = left_to_check(
            &json!({"named":[known("call"),unknown("gap")],"named_withheld":5}),
            &focal(),
        );
        assert_eq!(left.total, 7);
        assert_eq!(left.groups.len(), 3);
        assert!(left.groups[2]["why"]
            .as_str()
            .unwrap()
            .starts_with("5 more retained candidate sites"));
        assert_eq!(left.groups[0]["sites"][0]["caller_id"], "call");
        assert_eq!(left.groups[1]["sites"][0]["caller_id"], "gap");
    }

    #[test]
    fn final_presentation_preserves_tool_shaped_repository_symbols() {
        let mut row = unknown("caller");
        row["callee"] = json!("get_entity_source");
        row["receiver"] = json!("find_references");
        let machine = json!({"focal_entity":focal(),"references":[],"total_upstream":0,
            "call_site_candidates":{"named":[row]},"_kin":{"verdict":{"state":"inconclusive", "safe_to_conclude_absent":false}}});
        let mut result = json!({"content":[{"type":"text","text":machine.to_string()}]});
        present(&mut result, "find_references");
        let reply: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            reply[LEFT_TO_CHECK_KEY][0]["sites"][0]["callee"],
            "get_entity_source"
        );
        assert_eq!(
            reply[LEFT_TO_CHECK_KEY][0]["sites"][0]["receiver"],
            "find_references"
        );
    }
}

#[cfg(test)]
mod trace_gap_tests {
    use super::*;

    /// A small callers walk with a complete returned chain, independent of
    /// repository fixtures. Its caller set still has an open dispatch edge.
    fn callers_walk() -> Map<String, Value> {
        json!({
            "_kin": {
                "verdict": {
                    "state": "inconclusive", "safe_to_conclude_absent": false,
                    "limiting_factor": "answer_truncated: the walk stopped early; counts_are_a_floor: the count is a lower bound"
                },
                "response": {"bounded": false}
            },
            "focal_name": "Utf8Encoder.encode", "direction": "callers", "depth": 8,
            "truncated": true, "next_cursor": null,
            "terminal_coverage_gap_steps": 1, "type_uses_withheld": 1,
            "chain": [
                {"step": 1, "parent_step": 0, "entity_name": "save_packet"},
                {"step": 2, "parent_step": 1, "entity_name": "flush_packet"},
                {"step": 3, "parent_step": 2, "entity_name": "export_packet", "terminal": "coverage_gap"}
            ],
            "reach": {
                "state": "open", "members_total": 3,
                "members": [
                    {"name": "save_packet", "distance": 1, "via": "encode", "proven_path": true},
                    {"name": "flush_packet", "distance": 2, "via": "save", "proven_path": true},
                    {"name": "export_packet", "distance": 3, "via": "flush", "proven_path": true}
                ],
                "frontier_total": 1, "frontier_withheld": [],
                "frontier": [{
                    "kind": "dispatch", "in_name": "PacketEncoder.encode",
                    "to_name": "Utf8Encoder.encode",
                    "why": "calls resolved to the base method can run the override and are not followed"
                }]
            }
        }).as_object().unwrap().clone()
    }

    /// The same walk with no gap but its fully listed frontier, under a
    /// verdict whose only qualifiers are counted residue.
    fn frontier_only_walk() -> Map<String, Value> {
        let mut machine = callers_walk();
        machine["_kin"]["verdict"] = json!({"state": "certified_except",
            "safe_to_conclude_absent": false,
            "limiting_factor": "escape_residue_listed; unkeyed_lookups_counted"});
        machine.remove("terminal_coverage_gap_steps");
        machine["truncated"] = json!(false);
        machine["chain"][2]
            .as_object_mut()
            .unwrap()
            .remove("terminal");
        machine
    }

    #[test]
    fn a_closed_unscoped_reach_is_not_cut_by_display_chain_limits() {
        let mut machine = frontier_only_walk();
        machine["_kin"]["verdict"]["limiting_factor"] =
            json!("value_calls_unmeasured; escape_residue_listed; unkeyed_lookups_counted");
        machine["reach"]["state"] = json!("closed");
        machine["reach"]["frontier_total"] = json!(0);
        machine["reach"]["frontier"] = json!([]);
        machine["reach"]["bounds"] = json!([]);
        machine["reach"]["blind_spots"] = json!({"calls_through_values": 17});
        machine.insert("terminal_bound_steps".into(), json!(2));
        machine["chain"][0]["fanout_truncated"] = json!(true);
        machine.insert("limit_per_step".into(), json!(25));
        let reply = plain_answer(machine.clone(), TRACE_TOOL);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(
            reply["_kin"]["verdict"], COMPLETE_EXCEPT_INDIRECT,
            "{meaning}"
        );
        assert!(!meaning.contains("within 8"), "{meaning}");
        assert!(meaning.contains("17 calls through values"), "{meaning}");
        // The closure has its own work bounds, independent of the display.
        machine["reach"]["state"] = json!("bounded");
        machine["reach"]["bounds"] = json!([{"which":"edge_budget"}]);
        let reply = plain_answer(machine, TRACE_TOOL);
        assert_eq!(reply["_kin"]["verdict"], PARTIAL);
        assert!(reply["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("reach.bounds"));
    }

    #[test]
    fn incomplete_or_unexplained_reach_evidence_never_certifies_the_plain_answer() {
        let mut base = frontier_only_walk();
        base["reach"]["state"] = json!("closed");
        base["reach"]["frontier_total"] = json!(0);
        base["reach"]["frontier"] = json!([]);
        base["reach"]["bounds"] = json!([]);
        base.insert("terminal_bound_steps".into(), json!(1));
        for case in [
            "open",
            "missing_members",
            "missing_bounds",
            "conflict",
            "folded",
        ] {
            let mut machine = base.clone();
            match case {
                "open" => machine["reach"]["state"] = json!("open"),
                "missing_members" => {
                    machine["reach"].as_object_mut().unwrap().remove("members");
                }
                "missing_bounds" => {
                    machine["reach"].as_object_mut().unwrap().remove("bounds");
                }
                "conflict" => {
                    machine.insert(
                        "readings".into(),
                        json!([{"key":"reach","value":{"state":"open"}}]),
                    );
                }
                "folded" => {
                    machine["reach"]["state"] = json!("open");
                    machine["reach"]["frontier_folded"] =
                        json!({"count":9,"by_reason":{"caller_text_not_read":9}});
                }
                _ => unreachable!(),
            }
            let reply = plain_answer(machine, TRACE_TOOL);
            assert_eq!(reply["_kin"]["verdict"], PARTIAL, "{case}: {reply}");
            if case == "folded" {
                assert!(reply["_kin"]["meaning"]
                    .as_str()
                    .unwrap()
                    .contains("9 unresolved observations"));
            }
        }
    }

    /// A scoped callers walk answers with its closure, which is held to the
    /// scope and not cut at the chain's depth, so the chain's depth bound does
    /// not make a closed scoped answer partial, and the lead names the scope.
    #[test]
    fn a_scoped_callers_walk_reads_its_closure_not_its_chain_depth() {
        let mut machine = frontier_only_walk();
        machine["_kin"]["verdict"] = json!({"state": "certified",
            "safe_to_conclude_absent": true});
        machine.insert("terminal_bound_steps".into(), json!(2));
        machine["reach"]["state"] = json!("closed");
        machine["reach"]["frontier_total"] = json!(0);
        machine["reach"]["frontier"] = json!([]);
        machine.insert(
            "scope".into(),
            json!({"module": "codec", "mode": "paths", "answer": "reach", "state": "complete"}),
        );
        let reply = plain_answer(machine.clone(), TRACE_TOOL);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], COMPLETE, "{meaning}");
        assert!(
            meaning.contains(
                "3 functions in codec can end up calling Utf8Encoder.encode, on paths that \
                 stay inside it"
            ),
            "{meaning}"
        );
        assert!(!meaning.contains("depth"), "{meaning}");

        // The same walk without the scope keeps the chain's depth as a limit.
        let mut unscoped = machine.clone();
        unscoped.remove("scope");
        let reply = plain_answer(unscoped, TRACE_TOOL);
        assert_eq!(reply["_kin"]["verdict"], PARTIAL);

        // A bounded scoped answer is partial and says why.
        machine["scope"]["state"] = json!("bounded");
        machine["reach"]["state"] = json!("bounded");
        let reply = plain_answer(machine, TRACE_TOOL);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], PARTIAL, "{meaning}");
        assert!(meaning.contains("reach.bounds"), "{meaning}");
    }

    /// Same page shape as a callers response: rows under reach, closure
    /// metadata (including scope) under a reach reading, without a separate
    /// scope reading. The source scope must survive normalization.
    fn paginated_scope_walk() -> Map<String, Value> {
        let mut machine = frontier_only_walk();
        machine["reach"]["bounds"] = json!([]);
        machine["reach"]["blind_spots"] = json!({"calls_through_values":17});
        machine["reach"]["scope"] = json!({"module":"codec", "mode":"paths", "outside":0});
        let mut reading = machine.remove("reach").unwrap();
        let mut rows = Map::new();
        for key in ["members", "frontier", "also_referenced_by"] {
            if let Some(value) = reading.as_object_mut().unwrap().remove(key) {
                rows.insert(key.into(), value);
            }
        }
        machine.insert("reach".into(), Value::Object(rows));
        machine.insert("readings".into(), json!([{"key":"reach", "value":reading}]));
        machine
    }

    #[test]
    fn paginated_reach_scope_qualifies_the_plain_caller_answer() {
        let machine = paginated_scope_walk();
        let reply = plain_answer(machine.clone(), TRACE_TOOL);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert!(meaning.contains("3 functions in codec can end up calling Utf8Encoder.encode, on paths that stay inside it"), "{meaning}");
        assert_eq!(reply["_kin"]["verdict"], PARTIAL);
        assert_eq!(reply["readings"], machine["readings"]);
        assert!(meaning.contains("17 calls through values"), "{meaning}");
    }

    fn closed_scope_control() -> Map<String, Value> {
        let mut machine = frontier_only_walk();
        machine["_kin"]["verdict"] = json!({"state":"certified", "safe_to_conclude_absent":true});
        machine["reach"]["state"] = json!("closed");
        machine["reach"]["bounds"] = json!([]);
        machine["reach"]["frontier_total"] = json!(0);
        machine["reach"]["frontier"] = json!([]);
        machine
    }

    #[test]
    fn scope_carriers_keep_members_and_the_declared_path_policy() {
        let base = closed_scope_control();
        let unscoped = plain_answer(base.clone(), TRACE_TOOL);
        assert_eq!(unscoped["_kin"]["verdict"], COMPLETE);
        assert!(!unscoped["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("in codec"));
        for mode in ["paths", "rows"] {
            for carrier in ["outer", "scope_reading", "reach", "reach_reading"] {
                let mut machine = base.clone();
                let scope = json!({"module":"codec", "mode":mode, "outside":0});
                match carrier {
                    "outer" => {
                        machine.insert("scope".into(), scope);
                    }
                    "scope_reading" => {
                        machine.insert("readings".into(), json!([{"key":"scope", "value":scope}]));
                    }
                    "reach" => machine["reach"]["scope"] = scope,
                    "reach_reading" => {
                        machine.insert(
                            "readings".into(),
                            json!([{"key":"reach", "value":{"scope":scope}}]),
                        );
                    }
                    _ => unreachable!(),
                }
                let reply = plain_answer(machine, TRACE_TOOL);
                let meaning = reply["_kin"]["meaning"].as_str().unwrap();
                assert_eq!(
                    reply["_kin"]["verdict"], COMPLETE,
                    "{carrier}/{mode}: {meaning}"
                );
                assert!(
                    meaning.contains("3 functions in codec"),
                    "{carrier}/{mode}: {meaning}"
                );
                assert_eq!(
                    meaning.contains("on paths that stay inside it"),
                    mode == "paths",
                    "{carrier}/{mode}: {meaning}"
                );
                assert_eq!(reply["reach"]["members"], unscoped["reach"]["members"]);
                assert_eq!(reply["chain"], unscoped["chain"]);
            }
        }
    }

    #[test]
    fn agreeing_scope_carriers_preserve_rich_outer_metadata() {
        let mut machine = closed_scope_control();
        machine.insert("scope".into(), json!({"module":"codec", "mode":"paths", "outside":9, "answer":"reach", "state":"complete"}));
        machine["reach"]["scope"] = json!({"module":"codec", "mode":"paths", "outside":0});
        machine.insert("readings".into(), json!([{"key":"scope", "value":{"module":"codec", "mode":"paths", "outside":9, "answer":"reach", "state":"complete"}}]));
        let selected = trace_scope(&machine).unwrap().unwrap();
        assert_eq!(selected.as_ref(), machine["scope"].as_object().unwrap());
        let reply = plain_answer(machine, TRACE_TOOL);
        assert_eq!(reply["_kin"]["verdict"], COMPLETE);
        assert!(reply["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("3 functions in codec"));
    }

    #[test]
    fn conflicting_scope_carriers_are_partial_without_a_chosen_scope() {
        for carrier in [
            "reach_module",
            "reach_mode",
            "scope_reading",
            "reach_reading",
            "duplicate_scope_readings",
        ] {
            let mut machine = closed_scope_control();
            machine.insert(
                "scope".into(),
                json!({"module":"codec", "mode":"paths", "answer":"reach", "state":"complete"}),
            );
            match carrier {
                "reach_module" => {
                    machine["reach"]["scope"] = json!({"module":"other", "mode":"paths"})
                }
                "reach_mode" => {
                    machine["reach"]["scope"] = json!({"module":"codec", "mode":"rows"})
                }
                "scope_reading" => {
                    machine.insert(
                        "readings".into(),
                        json!([{"key":"scope", "value":{"module":"other", "mode":"paths"}}]),
                    );
                }
                "reach_reading" => {
                    machine["reach"]["scope"] = json!({"module":"codec", "mode":"paths"});
                    machine.insert("readings".into(), json!([{"key":"reach", "value":{"scope":{"module":"other", "mode":"paths"}}}]));
                }
                "duplicate_scope_readings" => {
                    machine.remove("scope");
                    machine.insert("readings".into(), json!([{"key":"scope", "value":{"module":"codec", "mode":"paths"}}, {"key":"scope", "value":{"module":"codec", "mode":"rows"}}]));
                }
                _ => unreachable!(),
            }
            let members = machine["reach"]["members"].clone();
            let reply = plain_answer(machine, TRACE_TOOL);
            let meaning = reply["_kin"]["meaning"].as_str().unwrap();
            assert_eq!(reply["_kin"]["verdict"], PARTIAL, "{carrier}: {meaning}");
            assert!(
                meaning.contains("conflicting module or path scope"),
                "{carrier}: {meaning}"
            );
            assert!(!meaning.contains("in codec"), "{carrier}: {meaning}");
            assert!(!meaning.contains("in other"), "{carrier}: {meaning}");
            assert!(
                !meaning.contains("on paths that stay inside it"),
                "{carrier}: {meaning}"
            );
            assert!(!meaning.contains("Complete under"), "{carrier}: {meaning}");
            assert_eq!(reply["reach"]["members"], members);
        }
    }

    #[test]
    fn malformed_scope_never_defaults_to_a_path_claim() {
        for scope in [
            json!({"module":"codec"}),
            json!({"module":"codec", "mode":"unknown"}),
            json!({"module":"", "mode":"paths"}),
            json!({"module":7, "mode":"paths"}),
            Value::Null,
        ] {
            let mut machine = closed_scope_control();
            machine["reach"]["scope"] = scope;
            let reply = plain_answer(machine, TRACE_TOOL);
            let meaning = reply["_kin"]["meaning"].as_str().unwrap();
            assert_eq!(reply["_kin"]["verdict"], PARTIAL, "{meaning}");
            assert!(meaning.contains("scope is"), "{meaning}");
            assert!(!meaning.contains("in codec"), "{meaning}");
            assert!(
                !meaning.contains("on paths that stay inside it"),
                "{meaning}"
            );
        }
    }

    #[test]
    fn conflicting_rich_scope_metadata_is_not_silently_preferred() {
        for (field, other) in [("state", "bounded"), ("answer", "chain")] {
            let mut machine = closed_scope_control();
            let scope =
                json!({"module":"codec", "mode":"paths", "answer":"reach", "state":"complete"});
            machine.insert("scope".into(), scope.clone());
            let mut contradicted = scope;
            contradicted[field] = json!(other);
            machine.insert(
                "readings".into(),
                json!([{"key":"scope", "value":contradicted}]),
            );
            let reply = plain_answer(machine, TRACE_TOOL);
            let meaning = reply["_kin"]["meaning"].as_str().unwrap();
            assert_eq!(reply["_kin"]["verdict"], PARTIAL, "{field}: {meaning}");
            assert!(
                meaning.contains("conflicting scope answer or coverage"),
                "{field}: {meaning}"
            );
            assert!(!meaning.contains("in codec"), "{field}: {meaning}");
            assert!(!meaning.contains("Complete under"), "{field}: {meaning}");
        }
    }

    #[test]
    fn agreeing_sparse_outer_scope_retains_the_readings_coverage_bound() {
        let mut machine = closed_scope_control();
        machine.insert("scope".into(), json!({"module":"codec", "mode":"paths"}));
        machine.insert("readings".into(), json!([{"key":"scope", "value":{"module":"codec", "mode":"paths", "answer":"reach", "state":"bounded"}}]));
        let selected = trace_scope(&machine).unwrap().unwrap();
        assert_eq!(selected["state"], "bounded");
        assert_eq!(selected["answer"], "reach");
        let reply = plain_answer(machine.clone(), TRACE_TOOL);
        assert_eq!(reply["_kin"]["verdict"], PARTIAL);
        assert_eq!(reply["readings"], machine["readings"]);
        assert!(!reply["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("Complete under"));
    }

    fn assert_value_qualified_frontier(machine: Map<String, Value>) -> Value {
        assert_value_qualified(machine, PARTIAL)
    }

    /// The reply of a walk whose value calls qualify it, graded `word`, with
    /// its evidence and its chain unchanged.
    fn assert_value_qualified(mut machine: Map<String, Value>, word: &str) -> Value {
        // A complete reach reading carries bounds even when none stopped it.
        machine["reach"]["bounds"] = json!([]);
        let reach = machine["reach"].clone();
        let chain: Vec<Value> = machine["chain"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                json!({"step":row["step"], "parent_step":row["parent_step"],
                "name":row["entity_name"]})
            })
            .collect();
        let reply = plain_answer(machine, TRACE_TOOL);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], word, "{meaning}");
        assert!(
            meaning.contains("reach.frontier") || meaning.contains("under reach"),
            "{meaning}"
        );
        assert_eq!(reply["reach"], reach, "qualifying does not change evidence");
        assert_eq!(
            reply["chain"],
            json!(chain),
            "qualifying does not change the walk"
        );
        reply
    }

    /// A listed frontier beside calls through values, with the one place a
    /// member is held as a value served as a row.
    fn value_held_walk() -> Map<String, Value> {
        let mut machine = frontier_only_walk();
        machine["reach"]["bounds"] = json!([]);
        machine["reach"]["blind_spots"] = json!({"calls_through_values": 17,
            "value_holders": {"rows": 1, "complete": true}});
        machine["reach"]["also_referenced_by"] = json!([{
            "name": "holder", "refs": [{"to_name": "save_packet", "how": "value",
                "value": "collection", "code": "return {save_packet}"}]
        }]);
        machine["reach"]["also_referenced_total"] = json!(1);
        machine
    }

    /// A later page of a callers walk serves the rest of the frozen answer
    /// as readings. The blocks a first page never serves are not served
    /// there either, and neither is what the display chain's cap cut: a
    /// model run was told on such a page to pass `target` and to call
    /// `graph_neighborhood`, which its surface does not have, and that the
    /// cap had dropped 78 callers its reach held all along.
    #[test]
    fn a_later_page_serves_no_machine_block_and_no_chain_cap_count() {
        let working = |key: &str| json!({"key": key, "value": {"advice": "pass `target`"}});
        let machine = json!({
            "_kin": {
                "verdict": {"state": "inconclusive", "safe_to_conclude_absent": false},
                "page": {"version": 1, "complete": false, "has_more": false, "record_offset": 140,
                    "total_answer_records": 130, "answer_rows_complete": true},
                "trace_question": {"direction": "callers", "depth": 8,
                    "focal_name": "Scaffold.route"},
            },
            "chain": [{"step": 1, "parent_step": 0, "entity_name": "Scaffold._method_route"}],
            "readings": [
                working("_kin"), working("negative"), working("call_sites"),
                working("edge_coverage"), working("focal_resolution"), working("focal_entity"),
                {"key": "degradations", "value": [{"component": "fanout_cap",
                    "remediation": "name the symbol you are looking for as `target`. List them \
                                    with graph_neighborhood on entity_id cfaa9527 at depth 1"}]},
                {"key": "focal_id", "value": "cfaa9527-8673-4daa-8d27-d8a5ac89d37c"},
                {"key": "focal_name", "value": "Scaffold.route"},
                {"key": "limit_per_step", "value": 25},
                {"key": "max_response_chars", "value": 60000},
                {"key": "truncated", "value": true},
                {"key": "clipped_steps", "value": [{"entity_name": "Scaffold.route", "step": 0,
                    "dropped_callers": 78, "limit_per_step": 25}]},
                {"key": "spine_clipped_steps", "value": 1},
                {"key": "terminal_leaf_steps", "value": 27},
                {"key": "total_steps", "value": 48},
            ],
        });
        let reply = plain_answer(machine.as_object().unwrap().clone(), TRACE_TOOL);
        assert_eq!(
            reply["readings"],
            json!([{"key": "terminal_leaf_steps", "value": 27},
                   {"key": "total_steps", "value": 48}])
        );
        let text = crate::text_reply::render(
            reply.as_object().unwrap(),
            TRACE_TOOL,
            &crate::session_handles::SessionHandles::default(),
        );
        for unserved in [
            "graph_neighborhood",
            "target",
            "negative",
            "dropped_callers",
            "advice",
        ] {
            assert!(!text.contains(unserved), "{unserved}: {text}");
        }
        assert!(text.contains("total_steps: 48"), "{text}");

        // A walk with no reach keeps the count of what its cap cut: there
        // the chain is the answer. A page whose readings are all machine
        // blocks carries none.
        let mut chain_only = machine.clone();
        chain_only["_kin"]["page"]
            .as_object_mut()
            .unwrap()
            .remove("total_answer_records");
        chain_only["_kin"]["trace_question"]["direction"] = json!("calls");
        let reply = plain_answer(chain_only.as_object().unwrap().clone(), TRACE_TOOL);
        let kept: Vec<&str> = reply["readings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|reading| reading["key"].as_str().unwrap())
            .collect();
        assert_eq!(
            kept,
            [
                "clipped_steps",
                "spine_clipped_steps",
                "terminal_leaf_steps",
                "total_steps"
            ]
        );
        let mut blocks_only = machine;
        blocks_only["readings"] = json!([working("negative"), working("_kin")]);
        let reply = plain_answer(blocks_only.as_object().unwrap().clone(), TRACE_TOOL);
        assert!(reply.get("readings").is_none(), "{reply}");
    }

    /// A reach the read bound left partial says how far each package was
    /// read in its first sentence, and a text reply heads each package's
    /// members with the same words. Asked which functions inside one package
    /// can end up calling a method, a model got 126 members under one
    /// "partial" and paged for more, though the seven it was asked about
    /// were on the page and their package had been read whole.
    #[test]
    fn a_partial_reach_says_how_far_each_package_was_read() {
        let mut machine = frontier_only_walk();
        machine["reach"]["state"] = json!("bounded");
        machine["reach"]["frontier_total"] = json!(0);
        machine["reach"]["frontier"] = json!([]);
        machine["reach"]["bounds"] = json!([{"which": "frontier_reads_max", "limit": 24,
            "left": 1, "at": [{"name": "export_packet"}],
            "detail": "the closure reads the unresolved calls of at most 24 members"}]);
        for (at, module) in ["codec.packets", "codec.packets", "tests.test_codec"]
            .iter()
            .enumerate()
        {
            machine["reach"]["members"][at]["module"] = json!(module);
        }
        machine["reach"]["members"][2]["role"] = json!("test");
        machine["reach"]["packages"] = json!([
            {"module": "codec", "members": 2, "state": "closed"},
            {"module": "tests", "members": 1, "state": "unread", "unread": 1, "role": "test"},
        ]);
        let reply = plain_answer(machine.clone(), TRACE_TOOL);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], PARTIAL, "{meaning}");
        assert!(
            meaning.starts_with(
                "Partial: 3 functions can end up calling Utf8Encoder.encode; by package: codec \
                 2, complete inside codec; tests 1 in test code, 1 not read for calls Kin could \
                 not resolve. reach.members lists every one"
            ),
            "{meaning}"
        );
        let text = crate::text_reply::render(
            reply.as_object().unwrap(),
            TRACE_TOOL,
            &crate::session_handles::SessionHandles::default(),
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[..6],
            [
                "Answer (partial): 3 functions can end up calling Utf8Encoder.encode; by \
                 package: codec 2, complete inside codec; tests 1 in test code, 1 not read for \
                 calls Kin could not resolve.",
                "codec (2), complete inside codec:",
                "  codec.packets :: save_packet  via=encode",
                "  codec.packets :: flush_packet  via=save",
                "tests (1, test code), 1 not read for calls Kin could not resolve:",
                "  tests.test_codec :: export_packet  [test]  via=flush",
            ],
            "{text}"
        );

        // Places and held values a package still has open are counted in
        // its words, and a bound that cut the members is said as one.
        machine["reach"]["packages"] = json!([
            {"module": "codec", "members": 2, "state": "open", "places": 2, "held": 1},
            {"module": "tests", "members": 1, "state": "bounded", "role": "test"},
        ]);
        let meaning = plain_answer(machine.clone(), TRACE_TOOL)["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            meaning.contains(
                "; by package: codec 2, complete inside codec except 3 listed places; tests 1 \
                 in test code, cut by a work bound. "
            ),
            "{meaning}"
        );

        // A first page that holds every member says so, whatever follows it.
        machine["_kin"]["page"] = json!({"version": 1, "complete": false, "has_more": true,
            "record_offset": 0});
        machine["next_cursor"] = json!("frozen");
        let reply = plain_answer(machine, TRACE_TOOL);
        assert_eq!(
            reply["_kin"]["delivered"],
            "All 3 reach members are on this page. next_cursor serves more of the places that \
             could add a caller, then the display chain; it adds no member, so send it only to \
             check those places.",
            "{reply}"
        );
    }

    /// What reaches the entity only through a call matched by a bare name
    /// is listed apart, so the count beside the verdict is the callers a
    /// resolver established. The meaning says how many more there are, and
    /// an answer that was complete is complete except them.
    #[test]
    fn name_only_reach_is_said_apart_and_a_complete_answer_is_complete_except_it() {
        let mut machine = frontier_only_walk();
        machine["_kin"]["verdict"] = json!({"state": "certified", "safe_to_conclude_absent": true});
        machine["reach"]["state"] = json!("closed");
        machine["reach"]["frontier_total"] = json!(0);
        machine["reach"]["frontier"] = json!([]);
        machine["reach"]["bounds"] = json!([]);
        let closed = plain_answer(machine.clone(), TRACE_TOOL);
        assert_eq!(
            closed["_kin"]["verdict"], COMPLETE,
            "{}",
            closed["_kin"]["meaning"]
        );
        machine["reach"]["name_only"] = json!([{"name": "BlueprintSetupState.__init__",
            "distance": 2, "via": "get", "proof": "name_only", "proven_path": false,
            "code": "subdomain = self.options.get(\"subdomain\")"}]);
        machine["reach"]["name_only_total"] = json!(1);
        let reply = plain_answer(machine.clone(), TRACE_TOOL);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT, "{meaning}");
        assert!(
            meaning.starts_with("Answer: these 3 functions are every direct and indirect caller"),
            "{meaning}"
        );
        assert!(
            meaning.contains(
                " 1 more function reaches it only through a call matched by name alone, which \
                 nothing tied to its target; reach.name_only lists it. Count one only if the \
                 object it calls on could be the one its row names."
            ),
            "{meaning}"
        );
        // A text reply lists the rows under their own heading, after the
        // members and without the proof fields the heading stands for.
        let text = crate::text_reply::render(
            reply.as_object().unwrap(),
            TRACE_TOOL,
            &crate::session_handles::SessionHandles::default(),
        );
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[4..6],
            [
                "Matched by name alone, not proven callers (1):",
                "  BlueprintSetupState.__init__  subdomain = self.options.get(\"subdomain\")",
            ],
            "{text}"
        );
        // Several are counted in the plural.
        machine["reach"]["name_only_total"] = json!(3);
        let meaning = plain_answer(machine, TRACE_TOOL)["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            meaning.contains(" 3 more functions reach it only through a call matched by name")
                && meaning.contains("reach.name_only lists them."),
            "{meaning}"
        );
    }

    /// An answered callers reach says the walk is finished: its members are
    /// every direct and indirect caller, and how many call the entity itself.
    /// A model that read "3 functions can end up calling it" traced callers
    /// again to reach a fixed point the reach had already reached.
    #[test]
    fn an_answered_reach_says_its_members_are_every_direct_and_indirect_caller() {
        let lead_of = |machine: Map<String, Value>| {
            let reply = plain_answer(machine, TRACE_TOOL);
            (
                reply["_kin"]["verdict"].as_str().unwrap().to_string(),
                reply["_kin"]["meaning"].as_str().unwrap().to_string(),
            )
        };
        // Complete except the places it lists.
        let (verdict, meaning) = lead_of(value_held_walk());
        assert_eq!(verdict, COMPLETE_EXCEPT, "{meaning}");
        assert!(
            meaning.starts_with(
                "Answer: these 3 functions are every direct and indirect caller of \
                 Utf8Encoder.encode; Kin followed callers of callers until no new function \
                 appeared (1 calls it directly, 2 through other calls). reach.members lists \
                 every one, each with the line where it calls the next step toward \
                 Utf8Encoder.encode (via). Kin followed every call it can read by name; 1 place"
            ),
            "{meaning}"
        );
        // A text reply writes the first sentence beside the verdict.
        let reply = plain_answer(value_held_walk(), TRACE_TOOL);
        let text = crate::text_reply::render(
            reply.as_object().unwrap(),
            TRACE_TOOL,
            &crate::session_handles::SessionHandles::default(),
        );
        assert_eq!(
            text.lines().next().unwrap(),
            "Answer (complete except the sites listed below): these 3 functions are every \
             direct and indirect caller of Utf8Encoder.encode; Kin followed callers of callers \
             until no new function appeared (1 calls it directly, 2 through other calls)."
        );
        // A closed reach says the same, and a reach with one member, or
        // whose members all call the entity itself, counts them in words.
        let mut closed = value_held_walk();
        closed["reach"]["state"] = json!("closed");
        closed["reach"]["frontier_total"] = json!(0);
        closed["reach"]["frontier"] = json!([]);
        let (verdict, meaning) = lead_of(closed.clone());
        assert!(verdict.starts_with("complete"), "{verdict}: {meaning}");
        assert!(
            meaning.starts_with("Answer: these 3 functions are every direct and indirect caller"),
            "{meaning}"
        );
        let mut direct = closed.clone();
        for member in direct["reach"]["members"].as_array_mut().unwrap() {
            member["distance"] = json!(1);
        }
        let (_, meaning) = lead_of(direct);
        assert!(
            meaning.contains("until no new function appeared (all call it directly). "),
            "{meaning}"
        );
        let mut only = closed.clone();
        only["reach"]["members"].as_array_mut().unwrap().truncate(1);
        only["reach"]["members_total"] = json!(1);
        let (verdict, meaning) = lead_of(only);
        assert!(verdict.starts_with("complete"), "{verdict}: {meaning}");
        assert!(
            meaning.starts_with(
                "Answer: this function is the only direct or indirect caller of \
                 Utf8Encoder.encode; Kin followed callers of callers until no new function \
                 appeared (it calls it directly). reach.members lists it, "
            ),
            "{meaning}"
        );
        // A row that states no distance leaves the count out, never guessed.
        let mut uncounted = closed.clone();
        uncounted["reach"]["members"][2]
            .as_object_mut()
            .unwrap()
            .remove("distance");
        let (_, meaning) = lead_of(uncounted);
        assert!(
            meaning.contains("until no new function appeared. reach.members lists every one"),
            "{meaning}"
        );
        // A reach a bound stopped claims only what it found.
        let mut bounded = closed;
        bounded["reach"]["state"] = json!("bounded");
        bounded["reach"]["bounds"] = json!([{"which": "edge_budget"}]);
        let (verdict, meaning) = lead_of(bounded);
        assert_eq!(verdict, PARTIAL, "{meaning}");
        assert!(
            meaning.starts_with("Partial: 3 functions can end up calling Utf8Encoder.encode. "),
            "{meaning}"
        );
        assert!(!meaning.contains("every direct and indirect"), "{meaning}");
    }

    #[test]
    fn listed_frontier_keeps_unfollowed_value_calls_open() {
        // Without the reading's census of every place a member is held as a
        // value, a listed holder row does not discharge the value calls: the
        // answer says they are counted and not followed. The reach still
        // finished its reading and every row is here, so that qualifies the
        // answer and never makes it partial.
        let mut machine = value_held_walk();
        machine["reach"]["blind_spots"]
            .as_object_mut()
            .unwrap()
            .remove("value_holders");
        let reply = assert_value_qualified(machine, COMPLETE_EXCEPT);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert!(meaning.contains("17 calls through values"), "{meaning}");
        assert_eq!(reply["reach"]["members"].as_array().unwrap().len(), 3);
        // Nor does a census that says a place is not served, or a member
        // with no census: each is complete except what it counts. A holder
        // row this reply does not carry is a row missing, which is partial.
        for (case, word) in [
            ("incomplete", COMPLETE_EXCEPT),
            ("row_not_served", PARTIAL),
            ("uncensused", COMPLETE_EXCEPT),
        ] {
            let mut machine = value_held_walk();
            match case {
                "incomplete" => {
                    machine["reach"]["blind_spots"]["value_holders"]["complete"] = json!(false);
                    machine["reach"]["blind_spots"]["value_holders"]["unlisted"] = json!(1);
                }
                "row_not_served" => machine["reach"]["also_referenced_total"] = json!(2),
                "uncensused" => {
                    machine["reach"]["blind_spots"]["uncensused"] = json!({"members": [{"name": "export_packet", "why": "record unavailable"}]});
                }
                _ => unreachable!(),
            }
            let reply = plain_answer(machine, TRACE_TOOL);
            let meaning = reply["_kin"]["meaning"].as_str().unwrap();
            assert_eq!(reply["_kin"]["verdict"], word, "{case}: {meaning}");
            assert!(!meaning.contains("`holder`"), "{case}: {meaning}");
        }
    }

    /// Calls through values that spell no member's name can run a member only
    /// through a value taken where one is held. When the reading serves every
    /// such place, those are the places to check: the answer is complete
    /// except them and its listed frontier, and the repository-wide count
    /// stays in the machine field.
    #[test]
    fn listed_frontier_with_every_value_holder_served_reads_complete_except_them() {
        let machine = value_held_walk();
        let reach = machine["reach"].clone();
        let reply = plain_answer(machine, TRACE_TOOL);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT, "{meaning}");
        assert!(
            meaning.contains("1 place it cannot follow is listed in reach.frontier"),
            "{meaning}"
        );
        assert!(
            meaning.contains("only where one is held as a value: `holder`"),
            "{meaning}"
        );
        assert!(!meaning.contains("17"), "{meaning}");
        assert!(
            !meaning.contains("calls through values are counted"),
            "{meaning}"
        );
        assert_eq!(reply["reach"], reach, "grading does not change evidence");
    }

    /// The React shape: a page whose rows ride under reach and whose closure
    /// metadata rides under a reach reading, three deduplicated places all in
    /// test code, a repository-wide value-call and lookup count, and one
    /// served holder. It reads complete except those places, names the
    /// holder, and says the lookups as scope.
    #[test]
    fn a_paged_reach_with_test_sites_and_served_holders_reads_complete_except_them() {
        let mut machine = frontier_only_walk();
        machine["_kin"]["verdict"] = json!({"state": "inconclusive",
            "safe_to_conclude_absent": false,
            "limiting_factor": "answer_truncated; trace_spine_clipped; retrieval_degraded; counts_are_a_floor"});
        machine["_kin"]["response"] = json!({"bounded": true});
        let row = |to: &str, sites: u64| {
            json!({"kind": "unresolved_call", "in": "scope-test", "in_name": "ReactScope-test.internal",
                "in_role": "test", "to": to, "to_name": to, "sites": sites,
                "why": format!("calls `{to}`, and no resolver proved where the call goes")})
        };
        machine["reach"]["frontier"] = json!([
            row("getChildContextValues", 2),
            row("DO_NOT_USE_queryAllNodes", 10),
            row("DO_NOT_USE_queryFirstNode", 4)
        ]);
        machine["reach"]["frontier_total"] = json!(3);
        machine["reach"]["frontier_in_tests"] = json!(3);
        machine["reach"]["bounds"] = json!([]);
        machine["reach"]["blind_spots"] = json!({"calls_through_values": 68606,
            "unkeyed_lookups": 871, "value_holders": {"rows": 1, "complete": true}});
        machine["reach"]["also_referenced_by"] = json!([
            {"name": "ReactFiberScope", "kind": "Module",
                "refs": [{"to_name": "isFiberSuspenseAndTimedOut", "how": "reference"}]},
            {"name": "createScopeInstance", "refs": [
                {"to_name": "DO_NOT_USE_queryAllNodes", "how": "reference", "value": "collection"},
                {"to_name": "getChildContextValues", "how": "reference", "value": "collection"}]}
        ]);
        machine["reach"]["also_referenced_total"] = json!(2);
        let mut reading = machine.remove("reach").unwrap();
        let mut rows = Map::new();
        for key in ["members", "frontier", "also_referenced_by"] {
            if let Some(value) = reading.as_object_mut().unwrap().remove(key) {
                rows.insert(key.into(), value);
            }
        }
        machine.insert("reach".into(), Value::Object(rows));
        machine.insert(
            "readings".into(),
            json!([{"key": "reach", "value": reading}]),
        );
        let reply = plain_answer(machine.clone(), TRACE_TOOL);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT, "{meaning}");
        assert!(
            meaning.contains("3 places it cannot follow are listed in reach.frontier with why (all of them sit in test code"),
            "{meaning}"
        );
        assert!(meaning.contains("`createScopeInstance`"), "{meaning}");
        assert!(
            !meaning.contains("ReactFiberScope"),
            "a plain reference holds no value: {meaning}"
        );
        assert!(!meaning.contains("68606"), "{meaning}");
        assert!(!meaning.contains("871"), "{meaning}");
        let searched = reply["_kin"]["searched"].as_str().unwrap();
        assert!(
            searched.contains("none of the 871 such places here spells"),
            "{searched}"
        );
        assert_eq!(reply["readings"], machine["readings"]);
    }

    /// A member that holds another as a value is named beside the holder
    /// rows, and with no holder row it is the only place named.
    #[test]
    fn members_holding_a_value_are_named_as_places_too() {
        let mut machine = value_held_walk();
        machine["reach"]["blind_spots"]["value_holders"]["in_members"] =
            json!([{"name": "flush_packet", "to_name": "save_packet", "line": 12}]);
        let reply = plain_answer(machine.clone(), TRACE_TOOL);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT, "{meaning}");
        assert!(
            meaning.contains("`holder` (reach.also_referenced_by, each ref with value), and inside `flush_packet`"),
            "{meaning}"
        );
        machine["reach"]["blind_spots"]["value_holders"]["rows"] = json!(0);
        machine["reach"]["also_referenced_by"] = json!([]);
        machine["reach"]["also_referenced_total"] = json!(0);
        let reply = plain_answer(machine, TRACE_TOOL);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT, "{meaning}");
        assert!(
            meaning.contains("held as a value: inside `flush_packet`"),
            "{meaning}"
        );
        assert!(!meaning.contains("`holder`"), "{meaning}");
    }

    /// A closed reach whose only qualifier is calls through values names the
    /// served holders as the places to check, rather than sending the reader
    /// to refs for indirect uses.
    #[test]
    fn a_closed_reach_with_served_holders_names_them_instead_of_indirect_uses() {
        let mut machine = value_held_walk();
        machine["_kin"]["verdict"]["limiting_factor"] =
            json!("value_calls_unmeasured; escape_residue_listed; unkeyed_lookups_counted");
        machine["reach"]["state"] = json!("closed");
        machine["reach"]["frontier_total"] = json!(0);
        machine["reach"]["frontier"] = json!([]);
        let reply = plain_answer(machine.clone(), TRACE_TOOL);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT, "{meaning}");
        assert!(meaning.contains("`holder`"), "{meaning}");
        assert!(!meaning.contains("refs on one"), "{meaning}");
        assert!(!meaning.contains("17"), "{meaning}");

        // A bound still makes it partial; the holders are still named and
        // the repository-wide count is still not the reason.
        machine["reach"]["state"] = json!("bounded");
        machine["reach"]["bounds"] = json!([{"which": "edge_budget"}]);
        let reply = plain_answer(machine, TRACE_TOOL);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], PARTIAL, "{meaning}");
        assert!(meaning.contains("reach.bounds"), "{meaning}");
        assert!(meaning.contains("`holder`"), "{meaning}");
        assert!(!meaning.contains("17 calls through values"), "{meaning}");
    }

    #[test]
    fn listed_frontier_keeps_uncounted_value_call_qualification_open() {
        let mut machine = frontier_only_walk();
        // A caller without a ledger can leave the value qualification open
        // without contributing a sites count to reach.blind_spots.
        machine["_kin"]["verdict"]["limiting_factor"] =
            json!("value_calls_unmeasured; escape_residue_listed");
        assert_value_qualified_frontier(machine);
    }

    #[test]
    fn listed_frontier_keeps_missing_value_census_open() {
        let mut machine = frontier_only_walk();
        machine["reach"]["blind_spots"] = json!({"uncensused": {
            "members": [{"name": "export_packet", "why": "record unavailable"}]
        }});
        let reply = assert_value_qualified_frontier(machine);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert!(meaning.contains("no escape census"), "{meaning}");
    }

    #[test]
    fn listed_frontier_does_not_place_value_invocation_by_its_outside_holder() {
        let mut machine = frontier_only_walk();
        machine.insert(
            "scope".into(),
            json!({
                "module": "codec", "mode": "paths", "answer": "reach", "state": "open"
            }),
        );
        machine["reach"]["blind_spots"] = json!({"calls_through_values": 17});
        machine["reach"]["withheld"] = json!({"referrers_outside": 1});
        machine["reach"]["also_referenced_by"] = json!([]);
        let reply = assert_value_qualified_frontier(machine);
        assert_eq!(reply["reach"]["withheld"]["referrers_outside"], 1);
    }

    #[test]
    fn listed_frontier_keeps_value_qualification_without_a_complete_reading() {
        let mut machine = frontier_only_walk();
        // Without bounds this is not a complete answer_view, but the raw
        // frontier decision must still retain its positive value qualifier.
        machine["reach"]["blind_spots"] = json!({"calls_through_values": 17});
        assert!(crate::trace_reach::answer_view(&machine).is_none());
        let reply = plain_answer(machine, TRACE_TOOL);
        assert_eq!(reply["_kin"]["verdict"], PARTIAL, "{reply:#}");
        assert_eq!(reply["reach"]["blind_spots"]["calls_through_values"], 17);
        assert!(reply["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("Value-call coverage remains unmeasured"));
    }

    #[test]
    fn listed_frontier_keeps_value_qualification_in_raw_reach_readings() {
        let mut machine = frontier_only_walk();
        machine["reach"]["blind_spots"] = json!({"calls_through_values": 17});
        let reach = machine.remove("reach").unwrap();
        machine.insert("readings".into(), json!([{"key":"reach", "value":reach}]));
        assert!(crate::trace_reach::answer_view(&machine).is_none());
        let reply = plain_answer(machine, TRACE_TOOL);
        assert_eq!(reply["_kin"]["verdict"], PARTIAL, "{reply:#}");
        assert_eq!(
            reply["readings"][0]["value"]["blind_spots"]["calls_through_values"],
            17
        );
        assert!(reply["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("Value-call coverage remains unmeasured"));
    }

    #[test]
    fn a_fully_listed_frontier_alone_reads_complete_except_its_places() {
        let reply = plain_answer(frontier_only_walk(), TRACE_TOOL);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT, "{meaning}");
        assert!(
            meaning.contains("3 functions can end up calling Utf8Encoder.encode"),
            "{meaning}"
        );
        assert!(
            meaning.contains("1 place it cannot follow is listed in reach.frontier"),
            "{meaning}"
        );
        // A withheld frontier place is not listed, so the walk stays partial.
        let mut withheld = frontier_only_walk();
        withheld["reach"]["frontier_total"] = json!(2);
        withheld["reach"]["frontier_withheld"] = json!([{"to_name": "Other.encode"}]);
        let reply = plain_answer(withheld, TRACE_TOOL);
        assert_eq!(reply["_kin"]["verdict"], PARTIAL);
    }

    #[test]
    fn coverage_and_dispatch_gaps_are_not_response_size_cuts() {
        let machine = callers_walk();
        let reach = machine["reach"].clone();
        let reply = plain_answer(machine, TRACE_TOOL);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], PARTIAL);
        assert!(
            meaning.contains("3 functions can end up calling Utf8Encoder.encode"),
            "{meaning}"
        );
        assert!(
            meaning.contains("Kin could not confirm every call at 1 end of the walk"),
            "{meaning}"
        );
        assert!(
            meaning.contains("1 place where calls Kin did not follow could add callers"),
            "{meaning}"
        );
        assert!(!meaning.contains("size limit"), "{meaning}");
        assert!(
            !meaning.contains("type"),
            "excluded type edges do not explain a caller gap: {meaning}"
        );
        assert!(reply["_kin"].get("delivered").is_none(), "{reply:#}");
        assert_eq!(reply["reach"], reach);
        assert_eq!(reply["chain"].as_array().unwrap().len(), 3);
        assert_eq!(reply["next_cursor"], Value::Null);
    }

    #[test]
    fn a_coverage_gap_without_dispatch_names_the_unchecked_calls() {
        let mut machine = callers_walk();
        machine.remove("reach");
        let reply = plain_answer(machine, TRACE_TOOL);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], PARTIAL);
        assert!(
            meaning.contains("Kin could not confirm every call at 1 end of the walk"),
            "{meaning}"
        );
        assert!(!meaning.contains("size limit"), "{meaning}");
    }

    #[test]
    fn excluded_type_uses_alone_do_not_make_a_callers_walk_partial() {
        let mut machine = callers_walk();
        machine["_kin"]["verdict"] =
            json!({"state": "certified", "safe_to_conclude_absent": false});
        machine["truncated"] = json!(false);
        machine.remove("terminal_coverage_gap_steps");
        machine["chain"][2]["terminal"] = json!("leaf");
        machine["reach"]["state"] = json!("closed");
        machine["reach"]["frontier_total"] = json!(0);
        machine["reach"]["frontier"] = json!([]);
        let reply = plain_answer(machine, TRACE_TOOL);
        assert_eq!(reply["_kin"]["verdict"], COMPLETE);
        assert!(reply["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .starts_with("Answer: 3 functions"));
    }

    #[test]
    fn a_real_response_cut_keeps_its_cursor_and_delivery_instructions() {
        let mut machine = callers_walk();
        machine["_kin"]["response"]["bounded"] = json!(true);
        machine["_kin"]["page"] = json!({"has_more": true});
        machine["next_cursor"] = json!("trace:next");
        let reply = plain_answer(machine, TRACE_TOOL);
        assert_eq!(reply["_kin"]["verdict"], PARTIAL);
        assert_eq!(reply["next_cursor"], "trace:next");
        assert_eq!(
            reply["_kin"]["delivered"],
            "More rows follow: call again with cursor set to next_cursor."
        );
        assert!(reply["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("1 place where calls Kin did not follow could add callers"));
    }

    #[test]
    fn a_real_response_cut_without_a_cursor_keeps_its_size_advice() {
        let mut machine = callers_walk();
        machine.remove("reach");
        machine.remove("terminal_coverage_gap_steps");
        machine["_kin"]["response"]["bounded"] = json!(true);
        let reply = plain_answer(machine, TRACE_TOOL);
        assert_eq!(reply["_kin"]["delivered"], "Some rows did not fit in this reply; ask again with a larger max_output_bytes, up to 60000, or a narrower question.");
    }
}

#[cfg(test)]
mod remaining_reply_tests {
    use super::*;

    fn references(name: &str, rows: Vec<Value>, codes: &str) -> Map<String, Value> {
        json!({
            "_kin": {
                "verdict": {"state": "inconclusive", "safe_to_conclude_absent": false,
                    "limiting_factor": codes},
                "page": {"kind": "references", "answer_rows_complete": true,
                    "answer_rows_has_more": false, "evidence_has_more": false,
                    "total_references": rows.len(), "returned_references": rows.len()}
            },
            "focal_entity": {"id": "focal", "name": name, "kind": "Function", "file": "packet.py"},
            "relation_kinds": ["calls", "imports", "references"],
            "total_upstream": rows.len(), "references": rows
        })
        .as_object()
        .unwrap()
        .clone()
    }

    fn direct_row() -> Value {
        json!({"entity_id": "save", "name": "save_packet", "kind": "Function",
            "relation_kinds": ["calls"], "resolution": "type_resolved",
            "sites": [{"expr": "encoder.encode(value)", "line_in_entity": 1}]})
    }

    fn dispatch_row() -> Value {
        json!({"entity_id": "send", "name": "send_packet", "kind": "Function",
            "relation_kinds": ["calls"], "resolution": "via_override_of=PacketEncoder.encode",
            "via_override_of": "PacketEncoder.encode",
            "receiver_binding": [{"entity_id": "send", "code": "encoder: PacketEncoder"}],
            "sites": [{"expr": "encoder.encode(value)", "line_in_entity": 1}]})
    }

    #[test]
    fn an_empty_unmeasured_reference_answer_does_not_invent_importing_files() {
        let mut machine = references("orphan_helper", vec![],
            "reference_completion_unmeasured: non-call completion is unmeasured; caller_arrival_unmeasured: no import family was established");
        machine.insert(
            "caller_arrival".into(),
            json!({"state": "unmeasured", "family_files": 0,
            "unmeasured_reason": "no imports were observed"}),
        );
        let reply = plain_answer(machine, "find_references");
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], PARTIAL);
        assert!(
            meaning.contains("no recorded reference to orphan_helper was found"),
            "{meaning}"
        );
        assert!(
            meaning.contains("could not establish every entity that can reach this entity"),
            "{meaning}"
        );
        assert!(!meaning.contains("some files that import"), "{meaning}");
        assert!(
            !meaning.contains("nothing in the searched code uses"),
            "{meaning}"
        );
        assert!(!meaning.contains("locate with mode literal"), "{meaning}");
        assert!(
            meaning.contains("report the unchecked coverage"),
            "{meaning}"
        );
        assert!(
            meaning.contains("cannot establish missing reference bindings"),
            "{meaning}"
        );
        assert!(meaning.contains("close this completeness gap"), "{meaning}");
        assert_eq!(reply["references"], json!([]));
        assert_eq!(reply["_kin"]["page"]["recorded_rows_exhausted"], true);
    }

    #[test]
    fn a_recorded_unaccounted_family_can_name_its_actual_call_gap() {
        let mut machine = references(
            "target",
            vec![],
            "caller_arrival_unresolved: unsettled family",
        );
        machine.insert(
            "caller_arrival".into(),
            json!({"state": "unaccounted", "unaccounted_file_count": 1,
            "unaccounted_files": [{"file": "consumer.py", "unaccounted_call_sites": 2}]}),
        );
        let reply = plain_answer(machine, "find_references");
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], PARTIAL);
        assert!(
            meaning.contains(
                "Recorded call sites that could reach this entity remain unaccounted for"
            ),
            "{meaning}"
        );
        assert!(!meaning.contains("safe to conclude"), "{meaning}");
    }

    #[test]
    fn an_unresolved_arrival_code_without_file_evidence_does_not_invent_files() {
        for arrival in [
            Value::Null,
            json!({"state": "unaccounted",
            "unaccounted_file_count": 1,
            "unaccounted_files": [{"file": "consumer.py", "unaccounted_call_sites": null}]}),
        ] {
            let mut machine = references("target", vec![], "caller_arrival_unresolved: unsettled");
            machine.insert("caller_arrival".into(), arrival);
            let reply = plain_answer(machine, "find_references");
            let meaning = reply["_kin"]["meaning"].as_str().unwrap();
            assert_eq!(reply["_kin"]["verdict"], PARTIAL);
            assert!(!meaning.contains("some files"), "{meaning}");
            assert!(!meaning.contains("has calls Kin"), "{meaning}");
            assert!(
                meaning.contains("possible callers could not be accounted for"),
                "{meaning}"
            );
        }
    }

    #[test]
    fn override_dispatch_is_possible_and_keeps_the_same_rows_and_proof_fields() {
        let mut machine = references(
            "Utf8Encoder.encode",
            vec![direct_row(), dispatch_row()],
            "edge_coverage_unknown: call coverage is not established",
        );
        machine["relation_kinds"] = json!(["calls"]);
        machine["_kin"]["page"]["answer_rows_has_more"] = json!(true);
        machine.insert("answer_next_cursor".into(), json!("answer:next"));
        let reply = plain_answer(machine, "find_references");
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], PARTIAL);
        assert!(
            meaning.contains("1 function calls Utf8Encoder.encode directly"),
            "{meaning}"
        );
        assert!(
            meaning.contains("1 more may call it through a method it overrides"),
            "{meaning}"
        );
        assert!(
            !meaning.contains("these 2 functions call Utf8Encoder.encode"),
            "{meaning}"
        );
        assert_eq!(reply["references"].as_array().unwrap().len(), 2);
        assert_eq!(
            reply["references"][1]["through_override_of"],
            "PacketEncoder.encode"
        );
        assert_eq!(reply["references"][1]["receiver_binding_available"], true);
        assert_eq!(
            reply["references"][1]["sites"][0]["code"],
            "encoder.encode(value)"
        );
        assert_eq!(reply["answer_next_cursor"], "answer:next");
        assert!(reply["_kin"]["delivered"]
            .as_str()
            .unwrap()
            .contains("answer_next_cursor"));
    }

    #[test]
    fn override_only_rows_are_never_called_certain_callers() {
        let mut machine = references(
            "Utf8Encoder.encode",
            vec![dispatch_row()],
            "edge_coverage_unknown: call coverage is not established",
        );
        machine["relation_kinds"] = json!(["calls"]);
        let reply = plain_answer(machine, "find_references");
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], PARTIAL);
        assert!(
            meaning.contains("no shown function calls Utf8Encoder.encode directly"),
            "{meaning}"
        );
        assert!(
            meaning.contains("1 more may call it through a method it overrides"),
            "{meaning}"
        );
        assert_eq!(
            reply["references"][0]["through_override_of"],
            "PacketEncoder.encode"
        );
    }

    fn impact() -> Map<String, Value> {
        json!({
            "_kin": {"verdict": {"state": "inconclusive", "safe_to_conclude_absent": false,
                "limiting_factor": "cross_file_edges_absent: imports and references are not established"}},
            "changed_ids": ["base"], "next_cursor": null,
            "affected_callers": [{"id": "send", "name": "send_packet"}],
            "affected_overrides": [{"id": "encode", "name": "Utf8Encoder.encode", "resolution": "type_resolved"}],
            "edge_coverage": {"classes": {"calls": "present", "imports": "absent", "references": "unknown"}},
            "enrichment": {"status": "no_recorded_call_site_debt", "owed_entities": 0,
                "total_pending_entities": 0, "pending_entities": [], "entities_withheld": 0,
                "basis": "persisted_call_site_ledgers_and_context_validation; other relation enrichment is not established by this observation"}
        }).as_object().unwrap().clone()
    }

    #[test]
    fn debt_free_impact_names_unestablished_relations_and_a_limited_inspection() {
        let machine = impact();
        let callers = machine["affected_callers"].clone();
        let overrides = machine["affected_overrides"].clone();
        let reply = plain_answer(machine, "impact_analysis");
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], PARTIAL);
        assert!(
            meaning.contains("imports and non-call references between entities"),
            "{meaning}"
        );
        assert!(meaning.contains("refs on each changed entity"), "{meaning}");
        assert!(
            meaning.contains("does not certify complete impact coverage"),
            "{meaning}"
        );
        assert!(!meaning.contains("asking again later"), "{meaning}");
        assert_eq!(reply["affected_callers"], callers);
        assert_eq!(reply["affected_overrides"], overrides);
        assert_eq!(reply["enrichment"]["owed_entities"], 0);
    }

    #[test]
    fn a_call_site_observation_without_class_readings_certifies_no_other_relations() {
        let mut machine = impact();
        machine.remove("edge_coverage");
        let reply = plain_answer(machine, "impact_analysis");
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert_eq!(reply["_kin"]["verdict"], PARTIAL);
        assert!(
            meaning.contains("call-site observation does not establish other relation coverage"),
            "{meaning}"
        );
        assert!(meaning.contains("refs on each changed entity"), "{meaning}");
        assert!(!meaning.contains("asking again later"), "{meaning}");
    }

    #[test]
    fn impact_inspection_keeps_separate_owed_work_and_does_not_demote_complete_answers() {
        let mut machine = impact();
        machine["_kin"]["verdict"]["limiting_factor"] = json!("enrichment_incomplete: 1 caller is owed; cross_file_edges_absent: imports are not established");
        machine["enrichment"]["status"] = json!("bounded");
        machine["enrichment"]["owed_entities"] = json!(1);
        let reply = plain_answer(machine, "impact_analysis");
        assert_eq!(reply["_kin"]["verdict"], PARTIAL);
        assert!(reply["_kin"]["meaning"]
            .as_str()
            .unwrap()
            .contains("has not finished checking some calls"));
        let mut machine = impact();
        machine["_kin"]["verdict"] =
            json!({"state": "certified", "safe_to_conclude_absent": false});
        machine["edge_coverage"]["classes"] =
            json!({"calls": "present", "imports": "present", "references": "present"});
        let complete = plain_answer(machine, "impact_analysis");
        assert_eq!(complete["_kin"]["verdict"], COMPLETE);
        // A complete answer says what it lists, by list.
        assert_eq!(
            complete["_kin"]["meaning"],
            "Answer: a change to it affects 1 caller in affected_callers and 1 method that \
             overrides or implements it in affected_overrides, each listed once."
        );
    }
}
