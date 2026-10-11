// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Compact-first agent replies for the walk.
//!
//! A first reply carries what an agent needs to answer and act on each row:
//! its entity_id, its name, how it links, and at most one line of the code
//! that links it. What a row would only restate, or what one `source` or
//! `context` call on its entity_id serves, is left out of it, and the reply
//! says so once. An unscoped walk also says where its steps sit, counted by
//! package, and the `scope` that keeps the largest one, since agents did not
//! reach for that argument when nothing in the reply named it.
//!
//! This runs where a plain reply reaches the client, after the plain wording
//! is written and after the pager sized every page to its byte budget, so it
//! only ever removes bytes. Verdicts, cursors and counts are left exactly as
//! the plain reply states them.

use serde_json::{json, Map, Value};

/// Where a reply says what its short rows leave to a call on an entity_id.
pub(crate) const ROW_NOTE_KEY: &str = "rows";

/// Where a reply says which scope narrows an unscoped walk.
pub(crate) const NARROW_KEY: &str = "narrow";

const ROW_NOTE: &str = "Each row names its entity_id; source or context on it serves the \
     signature and body. A row with no kind is a function or method; a member with no proof \
     was resolved by type on a proven path.";

/// A plain walk compact-first, in place: the short form a walk is served in
/// unless the call asked for details. A frozen fragment is a lossless record,
/// never a row to shorten, and is left as it arrived.
pub(crate) fn compact_trace_payload(payload: &mut Value) {
    let Some(reply) = payload.as_object_mut() else {
        return;
    };
    if reply.contains_key("record_fragment") || !reply.contains_key("_kin") {
        return;
    }
    let before = serde_json::to_string(reply).map_or(usize::MAX, |text| text.len());
    let _ = compact_walk(reply, before);
}

/// Present a plain walk compact-first. Errors and prose pass through.
#[cfg(test)]
pub(crate) fn present_trace(result: &mut Value) {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return;
    }
    let Some(blocks) = result.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for block in blocks {
        let Some(text) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        let Ok(Value::Object(mut reply)) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        // A frozen fragment is a lossless record, never a row to shorten.
        if reply.contains_key("record_fragment") || !reply.contains_key("_kin") {
            continue;
        }
        let before = text.len();
        if let Some(rendered) = compact_walk(&mut reply, before) {
            block["text"] = Value::String(rendered);
        }
    }
}

/// The most bytes the scope line may add to a walk. The pager leaves
/// [`crate::trace_pages`]' presentation reserve for it, so a page sized to its
/// budget stays inside it.
const NARROW_ALLOWANCE_BYTES: usize = 256;

fn size(reply: &Map<String, Value>) -> usize {
    serde_json::to_string(reply).map_or(usize::MAX, |text| text.len())
}

/// The walk compact-first, or `None` when nothing changed. Shortened rows
/// always apply; the note on them only when it still saves bytes; the scope
/// line within [`NARROW_ALLOWANCE_BYTES`] of the reply as it arrived.
fn compact_walk(reply: &mut Map<String, Value>, before: usize) -> Option<String> {
    let shortened = compact_rows(reply);
    if shortened {
        let mut noted = reply.clone();
        if let Some(envelope) = noted.get_mut("_kin").and_then(Value::as_object_mut) {
            envelope.insert(ROW_NOTE_KEY.into(), json!(ROW_NOTE));
        }
        if size(&noted) < before {
            *reply = noted;
        }
    }
    let mut narrowed = reply.clone();
    if narrow_hint(&mut narrowed) && size(&narrowed) <= before + NARROW_ALLOWANCE_BYTES {
        *reply = narrowed;
    }
    hide_answered_chain(reply);
    quiet_answered_reach(reply);
    serde_json::to_string(reply).ok()
}

/// An answered callers reach without the trailers a model reads as work.
///
/// Measured in model runs: an agent treats every trailing hint or listed
/// count as a to-do item. One whose reply ended on a value-holder line spent
/// eight turns and 209-300K tokens chasing it after the answer was in hand,
/// and one that read "102806 calls through values" under a nine-member reach
/// spent five more turns before giving the same nine names. When every
/// member is on the page and the verdict is complete, or complete except
/// listed places, the reply leaves out:
///
/// - the display chain, which restates the members' witnesses, with the
///   step count and depth that describe it, the step census by package, the
///   scope hint, the cursor and its delivered sentence;
/// - counts taken across the repository: lookups by a built name that spell
///   no member, calls through values, importers, and the sentence every walk
///   carries about what was searched;
/// - module referrers, since an import is not a call;
/// - a referrer that only names members, once the census of where members
///   are held as values is whole ([`holders_measured`]): it holds none and
///   constructs none, so no later call can run a member through it;
/// - of the value-holder census, the count of holder rows the page already
///   lists, the statement that the census is complete and its glossary.
///
/// Call sites in test code fold into one count with their callers' names.
/// Each open place outside tests stays a row. A referrer that holds a member
/// as a value, or constructs one, stays a row: the records do not say under
/// what name a value held in a collection is reached, or whether it is
/// reached by position, so no reading of the calls that spell the member's
/// name closes it. The value-holder census keeps the members that hold
/// another as a value, the count of holders no row names, and the statement
/// that it is incomplete. The rule is the same whatever detail the call
/// asked for: `details` decides what a row carries, never this.
/// What an answered reach leaves out beside its rows, wherever the reply
/// holds it: the display chain and its counts, the walk's parameters, the
/// focal's own row (the answer line names it), the scope block (the answer
/// line says the scope and that paths stay inside it) and the census of the
/// chain's steps. None of them is a row or a place left to check.
pub(crate) const ANSWERED_REACH_LEAVES_OUT: [&str; 10] = [
    "chain",
    "total_steps",
    "depth",
    crate::trace_scope::MODULES_KEY,
    "entity",
    crate::trace_scope::SCOPE_KEY,
    "truncated",
    "type_uses_withheld",
    "terminal_leaf_steps",
    "terminal_coverage_gap_steps",
];

/// The frontier kind of a member the language can run without a call that
/// spells its name.
const IMPLICIT_DISPATCH: &str = "implicit_dispatch";

/// Where an answered reach counts the members of that kind:
/// `{"count": n, "members": [names]}`.
pub(crate) const FRONTIER_RUN_UNSPELLED_KEY: &str = "frontier_run_without_a_call";

pub(crate) fn quiet_answered_reach(reply: &mut Map<String, Value>) -> bool {
    let Some(reach) =
        crate::trace_reach::answer_view(reply).filter(crate::trace_reach::rows_delivered)
    else {
        return false;
    };
    if !answered(reply) {
        return false;
    }
    let measured = holders_measured(&reach);
    for key in ANSWERED_REACH_LEAVES_OUT {
        reply.remove(key);
    }
    reply.remove("next_cursor");
    if let Some(envelope) = reply.get_mut("_kin").and_then(Value::as_object_mut) {
        for key in [NARROW_KEY, "delivered", CHAIN_LEFT_OUT_KEY] {
            envelope.remove(key);
        }
        quiet_searched(envelope);
    }
    let mut dropped_referrers = 0u64;
    if let Some(reach) = reply.get_mut("reach").and_then(Value::as_object_mut) {
        if let Some(referrers) = reach
            .get_mut("also_referenced_by")
            .and_then(Value::as_array_mut)
        {
            let before = referrers.len();
            referrers.retain(|row| {
                let module = row
                    .get("kind")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind.eq_ignore_ascii_case("module"));
                !module && (!measured || referrer_can_run_a_member(row))
            });
            dropped_referrers = (before - referrers.len()) as u64;
            if referrers.is_empty() {
                reach.remove("also_referenced_by");
            }
        }
        if let Some(frontier) = reach.get_mut("frontier").and_then(Value::as_array_mut) {
            let in_tests: Vec<Value> = frontier
                .iter()
                .filter(|row| row.get("in_role").and_then(Value::as_str) == Some("test"))
                .cloned()
                .collect();
            if !in_tests.is_empty() {
                frontier.retain(|row| row.get("in_role").and_then(Value::as_str) != Some("test"));
                let mut callers: Vec<String> = in_tests
                    .iter()
                    .filter_map(|row| row.get("in_name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect();
                callers.dedup();
                reach.insert(
                    "frontier_in_test_code".into(),
                    json!({"sites": in_tests.len(), "callers": callers}),
                );
            }
        }
        // A member the language can run without a call that spells its name,
        // through a decorator or a protocol, has no site to read: its row
        // quoted its own declaration, once for every such member. They are
        // one kind of place, counted with the members.
        let run_unspelled: Vec<String> = reach
            .get("frontier")
            .and_then(Value::as_array)
            .map(|frontier| {
                frontier
                    .iter()
                    .filter(|row| {
                        row.get("kind").and_then(Value::as_str) == Some(IMPLICIT_DISPATCH)
                    })
                    .filter_map(|row| row.get("to_name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if !run_unspelled.is_empty() {
            if let Some(frontier) = reach.get_mut("frontier").and_then(Value::as_array_mut) {
                frontier.retain(|row| {
                    row.get("kind").and_then(Value::as_str) != Some(IMPLICIT_DISPATCH)
                });
            }
            reach.insert(
                FRONTIER_RUN_UNSPELLED_KEY.into(),
                json!({"count": run_unspelled.len(), "members": run_unspelled}),
            );
        }
        // A reply that fits one page holds the census beside its rows.
        quiet_reach_census(reach, dropped_referrers);
    }
    if let Some(readings) = reply.get_mut("readings").and_then(Value::as_array_mut) {
        readings.retain(|reading| {
            !reading
                .get("key")
                .and_then(Value::as_str)
                .is_some_and(|key| ANSWERED_REACH_LEAVES_OUT.contains(&key))
        });
        // A frozen first page holds it as a reading.
        for reading in readings.iter_mut() {
            if reading.get("key").and_then(Value::as_str) != Some("reach") {
                continue;
            }
            if let Some(census) = reading.get_mut("value").and_then(Value::as_object_mut) {
                quiet_reach_census(census, dropped_referrers);
            }
        }
    }
    true
}

/// Whether a reach reading's census of where its members are held as values
/// is whole: every member has a census, and every place one is held is a
/// served row whose ref says how ([`crate::trace_reach::VALUE_HOLDERS_WHY`]).
/// A referrer row with no such ref then holds no member, whatever it names.
fn holders_measured(reach: &Map<String, Value>) -> bool {
    let Some(blind) = reach.get("blind_spots") else {
        return false;
    };
    blind
        .get(crate::trace_reach::VALUE_HOLDERS_KEY)
        .and_then(|holders| holders.get("complete"))
        .and_then(Value::as_bool)
        == Some(true)
        && blind
            .pointer("/uncensused/members")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
}

/// Whether a later call could run a reach member through this referrer: it
/// holds one as a value, or constructs one. A row of plain references names
/// members and can run none. A row this cannot read is kept.
fn referrer_can_run_a_member(row: &Value) -> bool {
    row.get("refs")
        .and_then(Value::as_array)
        .is_none_or(|refs| {
            refs.iter().any(|reference| {
                reference.get("value").is_some_and(|held| !held.is_null())
                    || reference.get("how").and_then(Value::as_str) != Some("reference")
            })
        })
}

/// The census of an answered reach, wherever the reply holds it, without its
/// repository-wide counts. See [`quiet_answered_reach`].
fn quiet_reach_census(census: &mut Map<String, Value>, dropped_referrers: u64) {
    census.remove("also_imported_by");
    // How far each package was read tells a partial reach apart package by
    // package. An answered one is read whole, which its verdict says.
    census.remove(crate::trace_reach::PACKAGES_KEY);
    census.remove(crate::trace_reach::PACKAGES_MORE_KEY);
    if let Some(total) = census.get("also_referenced_total").and_then(Value::as_u64) {
        census.insert(
            "also_referenced_total".into(),
            json!(total.saturating_sub(dropped_referrers)),
        );
    }
    let Some(blind) = census.get_mut("blind_spots").and_then(Value::as_object_mut) else {
        return;
    };
    let holders = blind
        .get(crate::trace_reach::VALUE_HOLDERS_KEY)
        .and_then(Value::as_object)
        .map(open_value_holders)
        .filter(|holders| !holders.is_empty());
    blind.clear();
    match holders {
        Some(holders) => {
            blind.insert(
                crate::trace_reach::VALUE_HOLDERS_KEY.into(),
                Value::Object(holders),
            );
        }
        None => {
            census.remove("blind_spots");
        }
    }
}

/// What a value-holder census leaves open once its holder rows are on the
/// page: each member that holds another as a value, which a meaning names
/// under `in_members`, how many holders no row names, and that the census is
/// incomplete. The count of the rows the page lists, a census that says it
/// is complete and the glossary of its fields open nothing.
fn open_value_holders(holders: &Map<String, Value>) -> Map<String, Value> {
    let mut open = Map::new();
    if let Some(members) = holders
        .get("in_members")
        .filter(|members| members.as_array().is_some_and(|rows| !rows.is_empty()))
    {
        open.insert("in_members".into(), members.clone());
    }
    if let Some(unlisted) = holders
        .get("unlisted")
        .filter(|count| count.as_u64().is_some_and(|count| count > 0))
    {
        open.insert("unlisted".into(), unlisted.clone());
    }
    if holders.get("complete").and_then(Value::as_bool) == Some(false) {
        open.insert("complete".into(), json!(false));
    }
    open
}

/// Whether a plain reply's verdict says it answered: complete, or complete
/// except the places it lists or indirect uses.
fn answered(reply: &Map<String, Value>) -> bool {
    reply
        .get("_kin")
        .and_then(|kin| kin.get("verdict"))
        .and_then(Value::as_str)
        .is_some_and(|verdict| verdict.starts_with("complete"))
}

/// Whether more answer rows of a plain reference reply follow on a later
/// page: an answer cursor, or a page that says recorded rows remain.
fn reference_rows_follow(reply: &Map<String, Value>) -> bool {
    reply
        .get("answer_next_cursor")
        .is_some_and(Value::is_string)
        || reply
            .get("_kin")
            .and_then(|kin| kin.get("page"))
            .and_then(|page| page.get("recorded_rows_has_more"))
            .and_then(Value::as_bool)
            == Some(true)
}

/// Readings a continuation page of a reference answer carries that only
/// restate the question or the first page's machine envelope.
const ECHO_READINGS: [&str; 8] = [
    "_kin",
    "query",
    "relation_kinds",
    "resolution",
    "ambiguous_focal",
    "candidate_count",
    "focal_resolution",
    "caller_scope",
];

/// A `searched` line without what opens no gap: the sentence every answer
/// carries, that coverage is limited by the verdict, and the scope sentence
/// for code that builds a name while it runs, which by construction says no
/// such place spells the name. Any other sentence stays word for word.
fn quiet_searched(envelope: &mut Map<String, Value>) {
    let Some(searched) = envelope.get("searched").and_then(Value::as_str) else {
        return;
    };
    let kept: String = searched
        .split_inclusive(". ")
        .filter(|sentence| {
            let sentence = sentence.trim();
            !sentence.ends_with("coverage is limited by the verdict.")
                && !sentence.starts_with("Code that builds a name while it runs")
        })
        .collect();
    let kept = kept.trim();
    if kept.is_empty() {
        envelope.remove("searched");
    } else if kept != searched {
        envelope.insert("searched".into(), json!(kept));
    }
}

/// An `overridden_by` block without its census counts.
///
/// A block that lists an override, a caller of one, a candidate or a class
/// that could be one keeps those rows, its note and the scope limits that
/// qualify them, and leaves out the coverage counts and the proven count its
/// rows already show. A block that lists none says only that no override
/// exists when its coverage is complete, as [`crate::lean_reply`] serves it,
/// and otherwise goes: its census found nothing, and its limits qualify no
/// row.
fn quiet_overrides(section: &mut Map<String, Value>) {
    let Some(block) = section.get("overridden_by").and_then(Value::as_object) else {
        return;
    };
    let listed = |key: &str| {
        block.get(key).is_some_and(|rows| match rows {
            Value::Array(rows) => !rows.is_empty(),
            Value::Null => false,
            _ => true,
        })
    };
    let open = [
        "methods",
        crate::handlers::overrides::OVERRIDE_CALLERS_KEY,
        crate::handlers::overrides::OVERRIDE_CALLER_CANDIDATES_KEY,
        "unlisted_same_name_methods",
        "classes_spelling_base_without_edge",
        "possible",
        "unadmitted_declarations",
    ]
    .iter()
    .any(|key| listed(key));
    if !open {
        let complete = block.get("coverage").is_some_and(|coverage| {
            coverage == "complete"
                || ["recorded_closure", "repository_completeness"]
                    .iter()
                    .all(|key| coverage.get(*key).and_then(Value::as_str) == Some("complete"))
        });
        if complete {
            section.insert(
                "overridden_by".into(),
                json!({"count": 0, "coverage": "complete"}),
            );
        } else {
            section.remove("overridden_by");
        }
        return;
    }
    if let Some(block) = section
        .get_mut("overridden_by")
        .and_then(Value::as_object_mut)
    {
        for key in ["coverage", "proven", "bounded", "nested_definitions"] {
            block.remove(key);
        }
        // The note says what override rows are; with none it says nothing.
        let rows = [
            "methods",
            crate::handlers::overrides::OVERRIDE_CALLERS_KEY,
            crate::handlers::overrides::OVERRIDE_CALLER_CANDIDATES_KEY,
        ]
        .iter()
        .any(|key| {
            block
                .get(*key)
                .and_then(Value::as_array)
                .is_some_and(|rows| !rows.is_empty())
        });
        if !rows {
            block.remove("note");
        }
    }
}

/// One answered reference answer, or one section of a same-name answer,
/// without its trailers.
fn quiet_reference_section(section: &mut Map<String, Value>) {
    for key in [
        "next_cursor",
        "evidence_next_cursor",
        "evidence_detail",
        "relation_kinds",
        "caller_scope",
    ] {
        section.remove(key);
    }
    if let Some(envelope) = section.get_mut("_kin").and_then(Value::as_object_mut) {
        envelope.remove("delivered");
        envelope.remove("page");
        quiet_searched(envelope);
    }
    quiet_overrides(section);
}

/// An answered reference reply that ends on its answer and its open places.
///
/// The same rule as [`quiet_answered_reach`]: when the verdict is complete,
/// or complete except listed places, and no more answer rows follow, the
/// reply leaves out the cursors to later evidence and their delivered
/// sentence, the invitation to repeat the call with details, the echo of the
/// relation kinds and caller scope its meaning already states, the sentence
/// every answer carries about the verdict, the count of lookups by a built
/// name that spell nothing, the override census, and readings that restate
/// the question or the first page's machine envelope. Every row with its
/// line of call code, every place left to check, every name-only and
/// passed-to row and the verdict stay. The rule is the same whatever detail
/// the call asked for: `details:true` lists every site of a row and of a
/// place left to check, and brings none of this back.
pub(crate) fn quiet_answered_references(reply: &mut Map<String, Value>) -> bool {
    if !answered(reply) || reference_rows_follow(reply) {
        return false;
    }
    if reply
        .get("answers")
        .and_then(Value::as_array)
        .is_some_and(|answers| {
            answers.iter().any(|answer| {
                answer
                    .as_object()
                    .is_none_or(|answer| reference_rows_follow(answer) || !answered(answer))
            })
        })
    {
        return false;
    }
    quiet_reference_section(reply);
    if let Some(answers) = reply.get_mut("answers").and_then(Value::as_array_mut) {
        for answer in answers.iter_mut().filter_map(Value::as_object_mut) {
            quiet_reference_section(answer);
        }
    }
    if let Some(readings) = reply.get_mut("readings").and_then(Value::as_array_mut) {
        readings.retain(|reading| {
            !reading
                .get("key")
                .and_then(Value::as_str)
                .is_some_and(|key| ECHO_READINGS.contains(&key))
        });
        if readings.is_empty() {
            reply.remove("readings");
        }
    }
    true
}

/// The impact buckets that answer what must change. An empty one is still an
/// answer, that nothing of its kind does.
const IMPACT_ANSWER_KEYS: [&str; 2] = ["affected_callers", "affected_overrides"];

/// The impact buckets that hold the direct answer's rows.
const IMPACT_DIRECT_KEYS: [&str; 7] = [
    "affected_callers",
    crate::handlers::review::IMPACT_VALUE_USES_KEY,
    "affected_overrides",
    crate::impact_sections::INTERFACE_IMPLEMENTATIONS_KEY,
    crate::handlers::implementation_family::OVERRIDE_CALLERS_KEY,
    crate::handlers::implementation_family::OVERRIDE_CALLER_CANDIDATES_KEY,
    crate::impact_sections::SAME_NAME_KEY,
];

fn row_id(row: &Value) -> Option<&str> {
    row.get("entity_id")
        .or_else(|| row.get("id"))
        .and_then(Value::as_str)
}

/// The places a direct impact section leaves open, from its call-site census:
/// each unresolved call site and each place that holds the changed entity as
/// a value, which are what its verdict is complete except. Everything else in
/// the census is counts.
fn impact_open_places(direct: &Value) -> Vec<Value> {
    let mut places = Vec::new();
    let censuses: Vec<&Value> = match direct.get("call_sites") {
        Some(Value::Array(censuses)) => censuses.iter().collect(),
        Some(census @ Value::Object(_)) => vec![census],
        _ => Vec::new(),
    };
    for census in censuses {
        for candidate in census
            .get("candidates")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let mut place = Map::new();
            for (from, to) in [
                ("caller_name", "caller"),
                ("expr", "code"),
                ("reason", "why"),
            ] {
                if let Some(value) = candidate.get(from).filter(|value| !value.is_null()) {
                    place.insert(to.into(), value.clone());
                }
            }
            if !place.is_empty() {
                places.push(Value::Object(place));
            }
        }
        for site in census
            .pointer("/focal_escape/residue/values/sites")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let mut place = Map::new();
            if let Some(inside) = site.get("inside").filter(|value| !value.is_null()) {
                place.insert("caller".into(), inside.clone());
            }
            if let Some(code) = site
                .get("expr")
                .or_else(|| site.get("detail"))
                .filter(|value| !value.is_null())
            {
                place.insert("code".into(), code.clone());
            }
            place.insert("why".into(), json!("holds it as a value"));
            places.push(Value::Object(place));
        }
    }
    places
}

/// Whether an enrichment observation reports that nothing is owed.
fn nominal_enrichment(block: &Value) -> bool {
    let zero = |key: &str| block.get(key).and_then(Value::as_u64).unwrap_or(0) == 0;
    block.get("limitation").is_none()
        && zero("owed_entities")
        && zero("total_pending_entities")
        && zero("entities_withheld")
}

/// An answered impact reply that ends on what must change and its open
/// places.
///
/// When the verdict is complete, or complete except listed places or
/// indirect uses, and every row is on this one page, the reply leaves out
/// empty buckets other than callers and overrides, dependents the direct
/// rows already list, the per-entity consumer counts, an enrichment
/// observation that owes nothing, the sentence every answer carries about
/// the verdict, and the section census: each section's verdict inputs, row
/// lists and call-site counts. The census's open places, each unresolved
/// call site and each place that holds the changed entity as a value, stay
/// under `sections.direct.call_sites.candidates` as one short line each, the
/// place the meaning names, and a dependents section that is not certified
/// keeps its state and limiting factor while its rows remain. Rows, the
/// same-name implementations with their note, the implementation family
/// when it says another override could exist, and the verdict stay.
pub(crate) fn quiet_answered_impact(reply: &mut Map<String, Value>) -> bool {
    if !answered(reply)
        || reply.get("next_cursor").is_some_and(Value::is_string)
        || reply.contains_key("readings")
    {
        return false;
    }
    let direct: std::collections::HashSet<String> = IMPACT_DIRECT_KEYS
        .iter()
        .filter_map(|key| reply.get(*key).and_then(Value::as_array))
        .flatten()
        .filter_map(row_id)
        .map(str::to_string)
        .collect();
    if let Some(dependents) = reply
        .get_mut("affected_dependents")
        .and_then(Value::as_array_mut)
    {
        dependents.retain(|row| row_id(row).is_none_or(|id| !direct.contains(id)));
    }
    let empty: Vec<String> = reply
        .iter()
        .filter(|(key, value)| {
            !IMPACT_ANSWER_KEYS.contains(&key.as_str())
                && value.as_array().is_some_and(Vec::is_empty)
        })
        .map(|(key, _)| key.clone())
        .collect();
    for key in empty {
        reply.remove(&key);
    }
    reply.remove("entity_impacts");
    if reply.get("enrichment").is_some_and(nominal_enrichment) {
        reply.remove("enrichment");
    }
    if reply.get(crate::impact_sections::SAME_NAME_KEY).is_none() {
        reply.remove(crate::impact_sections::SAME_NAME_NOTE_KEY);
    }
    if reply
        .get(crate::impact_sections::INTERFACE_IMPLEMENTATIONS_KEY)
        .is_none()
    {
        reply.remove(crate::impact_sections::INTERFACE_IMPLEMENTATIONS_NOTE_KEY);
    }
    // The family's obligations are said in the meaning; it stays only to
    // list where another override could exist, which the meaning points at.
    let family_open = reply
        .get(crate::handlers::implementation_family::FAMILY_KEY)
        .and_then(Value::as_array)
        .is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry
                    .pointer("/coverage/repository_completeness")
                    .and_then(Value::as_str)
                    == Some("incomplete")
            })
        });
    if !family_open {
        reply.remove(crate::handlers::implementation_family::FAMILY_KEY);
        reply.remove(crate::handlers::implementation_family::FAMILY_NOTE_KEY);
    }
    if let Some(sections) = reply.remove(crate::impact_sections::SECTIONS_KEY) {
        let mut kept = Map::new();
        let places = sections
            .get("direct")
            .map(impact_open_places)
            .unwrap_or_default();
        if !places.is_empty() {
            kept.insert(
                "direct".into(),
                json!({"call_sites": {"candidates": places}}),
            );
        }
        let dependents_listed = reply
            .get("affected_dependents")
            .and_then(Value::as_array)
            .is_some_and(|rows| !rows.is_empty());
        if let Some(verdict) = sections
            .pointer("/dependents/verdict")
            .filter(|_| dependents_listed)
            .filter(|verdict| {
                verdict
                    .get("state")
                    .and_then(Value::as_str)
                    .is_none_or(|state| state != "certified")
            })
        {
            let mut short = Map::new();
            for key in ["state", "limiting_factor"] {
                if let Some(value) = verdict.get(key) {
                    short.insert(key.into(), value.clone());
                }
            }
            kept.insert("dependents".into(), json!({"verdict": short}));
        }
        if !kept.is_empty() {
            reply.insert(
                crate::impact_sections::SECTIONS_KEY.into(),
                Value::Object(kept),
            );
        }
    }
    if let Some(envelope) = reply.get_mut("_kin").and_then(Value::as_object_mut) {
        envelope.remove("delivered");
        quiet_searched(envelope);
    }
    true
}

/// Whether a machine impact result holds its whole answer on one page: no
/// cursor follows it and its page block, when it has one, says it is
/// complete. Read before the plain reply replaces its envelope.
pub(crate) fn impact_single_page(result: &Value) -> bool {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    let Some(text) = result.pointer("/content/0/text").and_then(Value::as_str) else {
        return false;
    };
    let Ok(machine) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    machine.get("record_fragment").is_none()
        && !machine.get("next_cursor").is_some_and(Value::is_string)
        && machine
            .pointer("/_kin/page/complete")
            .and_then(Value::as_bool)
            != Some(false)
}

/// Present a plain impact result that held its whole answer on one page as
/// [`quiet_answered_impact`] serves it. Errors and prose pass through.
pub(crate) fn present_answered_impact(result: &mut Value) {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return;
    }
    let Some(blocks) = result.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for block in blocks {
        let Some(text) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        let Ok(Value::Object(mut reply)) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if quiet_answered_impact(&mut reply) {
            if let Ok(rendered) = serde_json::to_string(&reply) {
                block["text"] = Value::String(rendered);
            }
        }
    }
}

/// Where a walk that left its display chain out says how to reach it.
pub(crate) const CHAIN_LEFT_OUT_KEY: &str = "chain_details";

const CHAIN_LEFT_OUT: &str = "Display chain left out: each reach member's via names the member \
     it calls toward the focal; trace focal=<member> to=<focal> serves one route.";

/// Leave the display chain out of a callers reach whose rows are all on the
/// page, whatever detail the call asked for, and say how to reach it. The
/// members already carry one-hop witnesses (`via`), so the chain only
/// restates them, and on a large reach it was most of the reply: 26 KB of a
/// 33 KB React reply whose answer was nine members. A walk whose page lacks
/// any recorded member, frontier or referrer row keeps its chain, and a route
/// (`to`) has no reach, so its chain, the answer, stays. A reach that is also
/// answered ends without the notice and the cursor this leaves, by
/// [`quiet_answered_reach`].
pub(crate) fn hide_answered_chain(reply: &mut Map<String, Value>) -> bool {
    let Some(reach) =
        crate::trace_reach::answer_view(reply).filter(crate::trace_reach::rows_delivered)
    else {
        return false;
    };
    if reply
        .get("chain")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        return false;
    }
    let mut candidate = reply.clone();
    candidate.remove("chain");
    let more = candidate.get("next_cursor").is_some_and(Value::is_string);
    if let Some(envelope) = candidate.get_mut("_kin").and_then(Value::as_object_mut) {
        envelope.insert(CHAIN_LEFT_OUT_KEY.into(), json!(CHAIN_LEFT_OUT));
        if more {
            envelope.insert("delivered".into(), json!(format!("All {} recorded reach members and listed frontier rows are on this page. Other evidence remains; use next_cursor only if needed. The stated coverage gaps still apply.", reach["members_total"])));
        }
    }
    // A tiny chain can cost less than its notice. Never grow the already
    // byte-bounded reply merely to hide those rows.
    if size(&candidate) <= size(reply) {
        *reply = candidate;
        return true;
    }
    false
}

fn compact_rows(reply: &mut Map<String, Value>) -> bool {
    let mut shortened = false;
    if let Some(chain) = reply.get_mut("chain").and_then(Value::as_array_mut) {
        for step in chain {
            shortened |= compact_step(step);
        }
    }
    for rows in ["members", crate::trace_reach::NAME_ONLY_KEY] {
        if let Some(members) = reply
            .get_mut("reach")
            .and_then(|reach| reach.get_mut(rows))
            .and_then(Value::as_array_mut)
        {
            for member in members {
                shortened |= ordinary_kind_left_out(member);
                shortened |= ordinary_proof_left_out(member);
            }
        }
    }
    shortened
}

/// A reach member's proof when it is the ordinary one: resolved by type, on
/// a proven path. Any other proof, or an unproven path, stays on the row.
fn ordinary_proof_left_out(member: &mut Value) -> bool {
    let Some(fields) = member.as_object_mut() else {
        return false;
    };
    let ordinary = fields.get("proof").and_then(Value::as_str) == Some("type_resolved")
        && fields.get("proven_path").and_then(Value::as_bool) == Some(true);
    if ordinary {
        fields.remove("proof");
        fields.remove("proven_path");
    }
    ordinary
}

/// A chain step without what it restates: its signature, which `source` on
/// its entity_id serves, sites that only repeat its one line of code, and a
/// kind that is a function or method.
fn compact_step(step: &mut Value) -> bool {
    let mut shortened = ordinary_kind_left_out(step);
    let Some(fields) = step.as_object_mut() else {
        return shortened;
    };
    shortened |= fields.remove("signature").is_some();
    let code = fields.get("code").cloned();
    let repeats_code = fields
        .get("sites")
        .and_then(Value::as_array)
        .is_some_and(|sites| {
            !sites.is_empty()
                && sites.iter().all(|site| {
                    site.as_object().is_some_and(|site| {
                        site.len() == 1 && code.is_some() && site.get("code") == code.as_ref()
                    })
                })
        });
    if repeats_code {
        fields.remove("sites");
        shortened = true;
    }
    shortened
}

fn ordinary_kind_left_out(row: &mut Value) -> bool {
    let ordinary = row.get("kind").and_then(Value::as_str).is_some_and(|kind| {
        kind.eq_ignore_ascii_case("function") || kind.eq_ignore_ascii_case("method")
    });
    ordinary
        && row
            .as_object_mut()
            .is_some_and(|row| row.remove("kind").is_some())
}

/// Lift an unscoped walk's module census to the reply and say, in `_kin`,
/// how many steps sit outside its largest package and the scope that keeps
/// that one. A frozen first page carries the census as a leading reading.
fn narrow_hint(reply: &mut Map<String, Value>) -> bool {
    if !reply.contains_key(crate::trace_scope::MODULES_KEY) {
        let lifted = reply
            .get_mut("readings")
            .and_then(Value::as_array_mut)
            .and_then(|readings| {
                let at = readings.iter().position(|reading| {
                    reading.get("key").and_then(Value::as_str)
                        == Some(crate::trace_scope::MODULES_KEY)
                })?;
                Some(readings.remove(at))
            });
        if let Some(reading) = lifted {
            if let Some(value) = reading.get("value") {
                reply.insert(crate::trace_scope::MODULES_KEY.into(), value.clone());
            }
            if reply
                .get("readings")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            {
                reply.remove("readings");
            }
        }
    }
    let Some(census) = reply.get(crate::trace_scope::MODULES_KEY) else {
        return false;
    };
    let (Some(largest), Some(outside), Some(steps)) = (
        census.get("largest").and_then(Value::as_str),
        census.get("outside_largest").and_then(Value::as_u64),
        census.get("steps").and_then(Value::as_u64),
    ) else {
        return false;
    };
    if outside == 0 {
        return false;
    }
    let sentence = format!(
        "{outside} of the {steps} steps sit outside {largest}, the package holding the most; \
         ask again with scope: \"{largest}\" to keep the walk inside it. modules counts the \
         steps by package."
    );
    match reply.get_mut("_kin").and_then(Value::as_object_mut) {
        Some(envelope) => {
            envelope.insert(NARROW_KEY.into(), json!(sentence));
            true
        }
        None => false,
    }
}

/// Where a neighbors reply says how its links name their ends.
pub(crate) const LINKS_NOTE_KEY: &str = "links";

const LINKS_NOTE: &str = "A link names its ends; entities gives each name's entity_id, and a \
     link keeps the id of an end whose name is not unique there. An entity with no kind is a \
     function or method.";

/// Present a plain neighbors reply compact-first, losslessly.
///
/// Every link restated both ends' names and UUIDs, and the reply's own
/// entities already pair each name with its UUID: on React, 40 links cost
/// 6.6 KB of a 27.5 KB reply. A link now leaves out an end's id when the
/// entities list holds exactly one entity of that name with that id, or when
/// the end has no name but its id. Entity rows leave out a kind that is a
/// function or method. Every id is recovered by its name from the same
/// reply, and the reply says so once. Applied only when it saves bytes.
pub(crate) fn present_neighbors(result: &mut Value) {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return;
    }
    let Some(blocks) = result.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for block in blocks {
        let Some(text) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        let Ok(Value::Object(mut reply)) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if !reply.contains_key("_kin") || !reply.contains_key("links") {
            continue;
        }
        let before = text.len();
        compact_neighbors(&mut reply);
        if let Some(envelope) = reply.get_mut("_kin").and_then(Value::as_object_mut) {
            envelope.insert(LINKS_NOTE_KEY.into(), json!(LINKS_NOTE));
        }
        let Ok(rendered) = serde_json::to_string(&Value::Object(reply)) else {
            continue;
        };
        if rendered.len() < before {
            block["text"] = Value::String(rendered);
        }
    }
}

fn compact_neighbors(reply: &mut Map<String, Value>) {
    let mut ids: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    if let Some(rows) = reply.get_mut("entities").and_then(Value::as_array_mut) {
        for row in rows.iter() {
            let (Some(name), Some(id)) = (
                row.get("name").and_then(Value::as_str),
                row.get("entity_id").and_then(Value::as_str),
            ) else {
                continue;
            };
            ids.entry(name.to_string())
                .and_modify(|seen| {
                    if seen.as_deref() != Some(id) {
                        *seen = None;
                    }
                })
                .or_insert_with(|| Some(id.to_string()));
        }
        for row in rows.iter_mut() {
            ordinary_kind_left_out(row);
        }
    }
    let Some(links) = reply.get_mut("links").and_then(Value::as_array_mut) else {
        return;
    };
    for link in links.iter_mut().filter_map(Value::as_object_mut) {
        for (name_key, id_key) in [("from", "from_id"), ("to", "to_id")] {
            let (Some(name), Some(id)) = (
                link.get(name_key).and_then(Value::as_str),
                link.get(id_key).and_then(Value::as_str),
            ) else {
                continue;
            };
            let recovered = name == id || ids.get(name).and_then(Option::as_deref) == Some(id);
            if recovered {
                link.remove(id_key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presented(reply: Value) -> Value {
        let mut result = json!({"content": [{"type": "text", "text": reply.to_string()}]});
        present_trace(&mut result);
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    fn walk() -> Value {
        json!({
            "_kin": {"verdict": "partial", "meaning": "Partial: 9 functions can end up calling it."},
            "chain": [
                {"step": 1, "parent_step": 0, "depth": 1, "entity_id": "535ce03c-ed5e-46fa-8603-8de10f52f6d4",
                 "name": "collectScopedNodes", "kind": "Function", "how": "Calls", "role": "caller",
                 "code": "if (isFiberSuspenseAndTimedOut(node)) {",
                 "signature": "function collectScopedNodes(node: Fiber, fn: ReactScopeQuery, scopedNodes: Array<any>,): void",
                 "sites": [{"code": "if (isFiberSuspenseAndTimedOut(node)) {"}]},
                {"step": 2, "parent_step": 0, "depth": 1, "entity_id": "42c0b4be-8b83-470f-a99b-700ec9577019",
                 "name": "ReactFiberScope", "kind": "Module", "how": "Imports", "role": "importer",
                 "sites": [{"code": "import {isFiberSuspenseAndTimedOut} from './ReactFiberTreeReflection';"}]},
                {"step": 3, "parent_step": 1, "depth": 2, "entity_id": "7c49126d-6e98-4fc4-9f61-ccec9c5ba038",
                 "name": "collectScopedNodesFromChildren", "kind": "Function", "how": "Calls", "role": "caller",
                 "code": "collectScopedNodes(child, fn, scopedNodes);",
                 "sites": [{"code": "collectScopedNodes(child, fn, scopedNodes);"},
                           {"code": "collectScopedNodes(sibling, fn, scopedNodes);"}]},
            ],
            "reach": {"members": [{"id": "535ce03c-ed5e-46fa-8603-8de10f52f6d4", "name": "collectScopedNodes",
                "kind": "Function", "proof": "type_resolved", "via": "x", "distance": 1}]},
            "readings": [{"key": "modules", "value": {"steps": 43, "largest": "packages.react_reconciler",
                "outside_largest": 13, "packages": [{"module": "packages.react_reconciler", "steps": 30},
                    {"module": "packages.react_dom", "steps": 13}]}}],
            "next_cursor": "frozen",
        })
    }

    #[test]
    fn a_delivered_reach_hides_only_duplicate_chain_and_keeps_coverage() {
        let mut original = walk();
        original["readings"].as_array_mut().unwrap().extend([
            json!({"key":"direction","value":"callers"}),
            json!({"key":"reach","value":{"state":"open","bounds":[],
                "members_total":1,"frontier_total":0,"frontier":[],"also_referenced_total":0,
                "frontier_folded":{"count":4},"blind_spots":{"calls_through_values":7}}}),
        ]);
        let reply = presented(original.clone());
        assert!(reply.get("chain").is_none());
        assert_eq!(reply["reach"]["members"][0]["via"], "x");
        assert_eq!(reply["_kin"]["verdict"], "partial");
        assert!(reply["_kin"][CHAIN_LEFT_OUT_KEY]
            .as_str()
            .unwrap()
            .contains("to=<focal>"));
        assert!(reply["_kin"]["delivered"]
            .as_str()
            .unwrap()
            .contains("All 1 recorded reach members"));
        assert_eq!(reply["next_cursor"], "frozen");
        assert!(reply["_kin"]["narrow"].is_string());
        assert_eq!(reply["readings"][1]["value"]["frontier_folded"]["count"], 4);
        original["reach"]["members"] = json!([]);
        assert!(presented(original.clone())["chain"].is_array());
        original["record_fragment"] = json!({"text":"part"});
        assert_eq!(presented(original.clone()), original);
    }

    #[test]
    fn an_answered_reach_ends_on_its_open_places_not_on_trailers() {
        let mut original = walk();
        original["_kin"]["verdict"] = json!("complete except the sites listed below");
        original["reach"]["frontier"] = json!([
            {"in": "t1", "in_name": "test_scope", "in_role": "test", "code": "scope.q(fn)", "why": "value call"},
            {"in": "t2", "in_name": "test_scope_more", "in_role": "test", "code": "scope.q(fn)", "why": "value call"},
            {"in": "p1", "in_name": "createScopeInstance", "code": "return {q}", "why": "held as a value"}]);
        original["reach"]["also_referenced_by"] = json!([
            {"id": "m", "kind": "Module", "name": "ReactFiberScope", "refs": []},
            {"id": "f", "kind": "Function", "name": "holder", "refs": [
                {"how": "reference", "to": "t1", "to_name": "collectScopedNodes",
                 "value": "collection"}]}]);
        original["readings"].as_array_mut().unwrap().extend([
            json!({"key":"direction","value":"callers"}),
            json!({"key":"reach","value":{"state":"open","bounds":[],"members_total":1,
                "frontier_total":3,"also_referenced_total":2,"also_imported_by":1,
                "blind_spots":{"unkeyed_lookups":871,"calls_through_values":102806,
                    "value_holders":{"rows":1,"complete":true}}}}),
        ]);
        let reply = presented(original.clone());
        assert!(reply.get("chain").is_none() && reply.get("modules").is_none());
        assert!(reply.get("next_cursor").is_none());
        for key in [NARROW_KEY, "delivered", CHAIN_LEFT_OUT_KEY] {
            assert!(reply["_kin"].get(key).is_none(), "{key}");
        }
        assert_eq!(
            reply["reach"]["frontier"].as_array().unwrap().len(),
            1,
            "the open place stays"
        );
        assert_eq!(
            reply["reach"]["frontier_in_test_code"],
            json!({"sites": 2, "callers": ["test_scope", "test_scope_more"]})
        );
        assert_eq!(
            reply["reach"]["also_referenced_by"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let census = |reply: &Value| {
            reply["readings"]
                .as_array()
                .unwrap()
                .iter()
                .find(|reading| reading["key"] == "reach")
                .unwrap()["value"]
                .clone()
        };
        let reading = census(&reply);
        // The holder row is on the page, so a count of it, a census that
        // says it is complete and the repository-wide counts open nothing.
        assert!(reading.get("blind_spots").is_none(), "{reading}");
        assert!(reading.get("also_imported_by").is_none());
        assert_eq!(reading["also_referenced_total"], 1);
        // What the census leaves open stays: a member that holds another as
        // a value, which the meaning names, holders no row names, and that
        // the census is incomplete. Its glossary goes.
        let mut open = original.clone();
        let at = open["readings"].as_array().unwrap().len() - 1;
        open["readings"][at]["value"]["blind_spots"]["value_holders"] = json!({
            "rows": 0, "complete": false, "unlisted": 2, "why": "a long glossary",
            "in_members": [{"id": "p1", "name": "createScopeInstance", "to": "t1",
                "to_name": "collectScopedNodes", "line": 12}]});
        assert_eq!(
            census(&presented(open))["blind_spots"],
            json!({"value_holders": {"complete": false, "unlisted": 2,
                "in_members": [{"id": "p1", "name": "createScopeInstance", "to": "t1",
                    "to_name": "collectScopedNodes", "line": 12}]}})
        );
        // A reply that fits one page holds the census beside its rows, and
        // it ends the same way.
        let mut whole = original.clone();
        let reading = whole["readings"].as_array_mut().unwrap().pop().unwrap();
        for (key, value) in reading["value"].as_object().unwrap() {
            whole["reach"][key] = value.clone();
        }
        let whole = presented(whole);
        for key in ["blind_spots", "also_imported_by"] {
            assert!(whole["reach"].get(key).is_none(), "{key}: {whole}");
        }
        assert_eq!(whole["reach"]["also_referenced_total"], 1);
        assert_eq!(whole["reach"]["frontier_in_test_code"]["sites"], 2);
        // A partial reach keeps every trailer.
        original["_kin"]["verdict"] = json!("partial");
        let partial = presented(original);
        assert!(partial["next_cursor"].is_string() && partial["_kin"][NARROW_KEY].is_string());
        assert_eq!(partial["reach"]["frontier"].as_array().unwrap().len(), 3);
        assert_eq!(
            census(&partial)["blind_spots"]["calls_through_values"],
            102806
        );
    }

    /// The plain reply an agent profile rendered for a callers reach of
    /// React's `isFiberSuspenseAndTimedOut`, before any trailing cut: nine
    /// members, three unresolved calls in one test module, one module that
    /// imports the focal and one function that holds three members as values.
    /// Rebuilt from the reply a model run was served, whose text form was
    /// 3,244 bytes under `details: true` and whose first line then read "9
    /// functions can end up calling isFiberSuspenseAndTimedOut."; the
    /// meaning here is the one an answered reach carries now.
    fn react_reach() -> Value {
        const FOCAL: &str = "b29b1f12-be53-4b09-9111-aa98a3c332e4";
        const TEST: &str = "350c8067-4d6a-4a53-9c3e-0d3c3f6f2a10";
        const SCOPE: &str = "react-reconciler.ReactFiberScope";
        let member = |id: &str, name: &str, code: &str, via: &str, distance: u64| {
            json!({"id": id, "name": name, "module": SCOPE, "kind": "Function", "code": code,
                "via": via, "proof": "type_resolved", "proven_path": true, "distance": distance})
        };
        let nodes = "535ce03c-ed5e-46fa-8603-8de10f52f6d4";
        let first = "1f0e3a52-7a0c-4b7e-8a51-2f6f3f5d9c01";
        let nearest = "2a4b6c8d-1e3f-4a5b-9c7d-0e1f2a3b4c02";
        let nodes_children = "7c49126d-6e98-4fc4-9f61-ccec9c5ba038";
        let first_children = "3b5d7f91-2c4e-4f60-8a1b-3c5d7e9f1a03";
        let nearest_children = "4c6e8a02-3d5f-4071-9b2c-4d6e8f0a2b04";
        let query_all = "5d7f9b13-4e60-4182-8c3d-5e7f9a1b3c05";
        let query_first = "6e8a0c24-5f71-4293-9d4e-6f8a0b2c4d06";
        let child_values = "7f9b1d35-6082-43a4-8e5f-7a9b1c3d5e07";
        let unresolved = |name: &str, to: &str| {
            json!({"in": TEST, "in_name": "ReactScope-test.internal", "in_role": "test",
                "kind": "unresolved_call", "to": to, "to_name": name,
                "why": format!("calls `{name}`, and no resolver proved where the call goes")})
        };
        let held = |name: &str, to: &str| {
            json!({"code": format!("{name},"), "how": "reference", "to": to, "to_name": name,
                "value": "collection"})
        };
        let step = |step: u64, parent: u64, id: &str, name: &str, code: &str| {
            json!({"step": step, "parent_step": parent, "depth": parent + 1, "entity_id": id,
                "name": name, "module": SCOPE, "kind": "Function", "how": "Calls",
                "role": "caller", "code": code})
        };
        json!({
            "_kin": {
                "verdict": "complete except the sites listed below",
                "meaning": "Answer: these 9 functions are every direct and indirect caller of isFiberSuspenseAndTimedOut; Kin followed callers of callers until no new function appeared (3 call it directly, 6 through other calls). reach.members lists every one, each with the line where it calls the next step toward isFiberSuspenseAndTimedOut (via). Kin followed every call it can read by name; 3 places it cannot follow are listed in reach.frontier with why (all of them sit in test code: in_role test). A call through a value can reach these entities only where one is held as a value: `createScopeInstance` (reach.also_referenced_by, each ref with value). Check one only if its reason could apply to this question, then answer.",
                "searched": "Searched recorded entity and relationship evidence; coverage is limited by the verdict. Code that builds a name while it runs is outside what any reader of code can follow; none of the 871 such places here spells isFiberSuspenseAndTimedOut or any entity in reach.members.",
            },
            "direction": "callers",
            "depth": 8,
            "total_steps": 65,
            "next_cursor": "eyJ2IjoxLCJzbmFwc2hvdCI6IjliNjM0YjgxLTkwNTEtNGQzNy05YTYzLWYzOWJjMzNiZTY0YSIsInJlY29yZCI6MTV9",
            "chain": [
                step(1, 0, nodes, "collectScopedNodes", "if (isFiberSuspenseAndTimedOut(node)) {"),
                step(2, 0, first, "collectFirstScopedNode", "if (isFiberSuspenseAndTimedOut(node)) {"),
                step(3, 1, nodes_children, "collectScopedNodesFromChildren", "collectScopedNodes(child, fn, scopedNodes);"),
                step(4, 3, query_all, "DO_NOT_USE_queryAllNodes", "collectScopedNodesFromChildren(child, fn, scopedNodes);"),
            ],
            "reach": {
                "members": [
                    member(nodes, "collectScopedNodes", "if (isFiberSuspenseAndTimedOut(node)) {", FOCAL, 1),
                    member(first, "collectFirstScopedNode", "if (isFiberSuspenseAndTimedOut(node)) {", FOCAL, 1),
                    member(nearest, "collectNearestContextValues", "if (isFiberSuspenseAndTimedOut(node)) {", FOCAL, 1),
                    member(nodes_children, "collectScopedNodesFromChildren", "collectScopedNodes(child, fn, scopedNodes);", nodes, 2),
                    member(first_children, "collectFirstScopedNodeFromChildren", "const scopedNode = collectFirstScopedNode(child, fn);", first, 2),
                    member(nearest_children, "collectNearestChildContextValues", "collectNearestContextValues(child, context, childContextValues);", nearest, 2),
                    member(query_all, "DO_NOT_USE_queryAllNodes", "collectScopedNodesFromChildren(child, fn, scopedNodes);", nodes_children, 3),
                    member(query_first, "DO_NOT_USE_queryFirstNode", "return collectFirstScopedNodeFromChildren(child, fn);", first_children, 3),
                    member(child_values, "getChildContextValues", "collectNearestChildContextValues(child, context, childContextValues);", nearest_children, 3),
                ],
                "frontier": [
                    unresolved("getChildContextValues", child_values),
                    unresolved("DO_NOT_USE_queryAllNodes", query_all),
                    unresolved("DO_NOT_USE_queryFirstNode", query_first),
                ],
                "also_referenced_by": [
                    {"id": "42c0b4be-8b83-470f-a99b-700ec9577019", "name": "ReactFiberScope", "kind": "Module",
                     "refs": [{"how": "reference", "to": FOCAL, "to_name": "isFiberSuspenseAndTimedOut"}]},
                    {"id": "5b2f692b-0d1e-4f2a-8b3c-9d0e1f2a3b08", "name": "createScopeInstance", "kind": "Function",
                     "refs": [held("DO_NOT_USE_queryAllNodes", query_all),
                              held("DO_NOT_USE_queryFirstNode", query_first),
                              held("getChildContextValues", child_values)]},
                ],
            },
            "readings": [
                {"key": "modules", "value": {"steps": 200, "largest": "react-reconciler",
                    "outside_largest": 57, "other_packages": 31, "packages": [
                        {"module": "react-reconciler", "steps": 143}, {"module": "react-dom", "steps": 10},
                        {"module": "react-art", "steps": 6}, {"module": "ReactDOMComponent", "steps": 3},
                        {"module": "react-noop-renderer", "steps": 3}, {"module": "react-test-renderer", "steps": 3}]}},
                {"key": "direction", "value": "callers"},
                {"key": "reach", "value": {"state": "open", "bounds": [], "members_total": 9,
                    "frontier_total": 3, "also_referenced_total": 2, "also_imported_by": 1,
                    "target": {"entity_id": FOCAL, "name": "isFiberSuspenseAndTimedOut"},
                    "blind_spots": {"unkeyed_lookups": 871, "calls_through_values": 102806,
                        "value_holders": {"rows": 1, "complete": true,
                            "why": crate::trace_reach::VALUE_HOLDERS_WHY}},
                    "policy": {"follows": ["Calls:type_resolved"],
                        "not_followed": "calls made later through a member held as a value, and calls resolved to a method a member overrides"}}},
            ],
        })
    }

    /// One plain reply as an agent connection writes it: presented with the
    /// call's arguments, leaned, then written in the connection's format.
    fn agent_reply(reply: &Value, tool: &str, arguments: Value, text: bool) -> String {
        let mut result = json!({"content": [{"type": "text", "text": reply.to_string()}]});
        let arguments = serde_json::from_value(arguments).unwrap();
        crate::query_reply::QueryPresentation::default().present(&mut result, tool, &arguments);
        crate::lean_reply::lean_result(&mut result);
        let handles = crate::session_handles::SessionHandles::default();
        if text {
            crate::text_reply::render_result(&mut result, tool, &handles);
        } else {
            handles.shorten_result(&mut result);
        }
        result["content"][0]["text"].as_str().unwrap().to_string()
    }

    /// `details` decides what a row carries, never what an answered reach
    /// ends on. In the measured run this reply, asked with `details: true`,
    /// was 3,244 bytes of text: it trailed 871 lookups by a built name,
    /// 102,806 calls through values, a module referrer, an importer count,
    /// the step census by package, the step count, a note on the chain, a
    /// delivered sentence and a cursor, and the agent spent five more turns
    /// on them. It now ends on the nine members and three open lines.
    #[test]
    fn details_never_bring_back_what_an_answered_reach_leaves_out() {
        let reply = react_reach();
        let asked = |details: bool| {
            json!({"focal": "isFiberSuspenseAndTimedOut", "direction": "callers",
                "depth": 8, "limit_per_step": 25, "max_output_bytes": 60000, "details": details})
        };
        let short = agent_reply(&reply, "trace_data_flow", asked(false), true);
        let detailed = agent_reply(&reply, "trace_data_flow", asked(true), true);
        // A text row leaves every ordinary value unwritten, so the two forms
        // of an answered reach read alike.
        assert_eq!(detailed, short);
        let lines: Vec<&str> = detailed.lines().collect();
        assert_eq!(
            lines[0],
            "Answer (complete except the sites listed below): these 9 functions are every direct and indirect caller of isFiberSuspenseAndTimedOut; Kin followed callers of callers until no new function appeared (3 call it directly, 6 through other calls)."
        );
        assert_eq!(
            lines[9],
            "  react-reconciler.ReactFiberScope :: getChildContextValues  collectNearestChildContextValues(child, context, childContextValues);"
        );
        assert_eq!(
            lines[10..],
            [
                "Not followed:",
                "  3 call sites in test code (ReactScope-test.internal) [test]",
                "  held as a value in createScopeInstance #5b2f692b: DO_NOT_USE_queryAllNodes, DO_NOT_USE_queryFirstNode, getChildContextValues (collection)",
                "  by design: calls made later through a member held as a value, and calls resolved to a method a member overrides",
            ],
            "{detailed}"
        );
        // About half the size it was served at, with a first line that
        // now says the walk is finished.
        assert!(
            detailed.len() < 1700,
            "{} bytes: {detailed}",
            detailed.len()
        );

        let json_form = |reply: &Value, details: bool| -> Value {
            serde_json::from_str(&agent_reply(
                reply,
                "trace_data_flow",
                asked(details),
                false,
            ))
            .unwrap()
        };
        let short = json_form(&reply, false);
        let detailed = json_form(&reply, true);
        // Details serve each member whole: its kind and its proof.
        for member in detailed["reach"]["members"].as_array().unwrap() {
            assert_eq!(member["kind"], "Function");
            assert_eq!(member["proof"], "type_resolved");
            assert_eq!(member["proven_path"], true);
        }
        assert!(short["reach"]["members"][0].get("proof").is_none());
        // Past its rows and the note on short rows, the reply is the same.
        let mut same = short.clone();
        same["reach"]["members"] = detailed["reach"]["members"].clone();
        same["_kin"].as_object_mut().unwrap().remove(ROW_NOTE_KEY);
        assert_eq!(same, detailed);
        for key in [
            "chain",
            "next_cursor",
            "total_steps",
            "depth",
            crate::trace_scope::MODULES_KEY,
        ] {
            assert!(detailed.get(key).is_none(), "{key}: {detailed}");
        }
        for key in ["searched", "delivered", CHAIN_LEFT_OUT_KEY, NARROW_KEY] {
            assert!(detailed["_kin"].get(key).is_none(), "{key}: {detailed}");
        }
        let census = |reply: &Value| {
            reply["readings"]
                .as_array()
                .unwrap()
                .iter()
                .find(|reading| reading["key"] == "reach")
                .unwrap()["value"]
                .clone()
        };
        let reading = census(&detailed);
        for key in ["blind_spots", "also_imported_by"] {
            assert!(reading.get(key).is_none(), "{key}: {reading}");
        }
        assert!(detailed["readings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|reading| reading["key"] != crate::trace_scope::MODULES_KEY));
        // The one holder of members as values stays, its refs whole; the
        // module that imports the focal goes; the test sites are one count.
        let referrers = detailed["reach"]["also_referenced_by"].as_array().unwrap();
        assert_eq!(referrers.len(), 1);
        assert_eq!(referrers[0]["name"], "createScopeInstance");
        assert_eq!(referrers[0]["refs"].as_array().unwrap().len(), 3);
        assert_eq!(detailed["reach"]["frontier"], json!([]));
        assert_eq!(
            detailed["reach"]["frontier_in_test_code"],
            json!({"sites": 3, "callers": ["ReactScope-test.internal"]})
        );
        assert_eq!(detailed["_kin"]["meaning"], reply["_kin"]["meaning"]);

        // A partial reach keeps every gap, count and cursor, as before:
        // details serve it whole but for the chain its members restate.
        let mut partial = reply.clone();
        partial["_kin"]["verdict"] = json!("partial");
        let kept = json_form(&partial, true);
        assert!(kept["next_cursor"].is_string());
        assert!(kept["_kin"][CHAIN_LEFT_OUT_KEY].is_string());
        assert_eq!(kept["_kin"]["searched"], partial["_kin"]["searched"]);
        assert_eq!(kept["depth"], 8);
        assert_eq!(kept["total_steps"], 65);
        assert_eq!(kept["reach"]["frontier"].as_array().unwrap().len(), 3);
        assert_eq!(
            kept["reach"]["also_referenced_by"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let reading = census(&kept);
        assert_eq!(reading["also_imported_by"], 1);
        assert_eq!(reading["blind_spots"]["calls_through_values"], 102806);
        assert_eq!(reading["blind_spots"]["unkeyed_lookups"], 871);
        assert_eq!(reading["blind_spots"]["value_holders"]["rows"], 1);
        assert!(kept["readings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reading| reading["key"] == crate::trace_scope::MODULES_KEY));
        let text = agent_reply(&partial, "trace_data_flow", asked(true), true);
        for kept in [
            "871 lookups by a name built at run time; 102806 calls through values",
            "referenced without a call by ReactFiberScope #42c0b4be (Module): isFiberSuspenseAndTimedOut",
            "imported without a call by 1 modules",
            "ReactScope-test.internal #350c8067  (calls `getChildContextValues`, and no resolver proved where the call goes) [test]",
        ] {
            assert!(text.contains(kept), "{kept}: {text}");
        }
        // Even partial, a text reach ends on its rows and the places left:
        // the walk's parameters are no place, and a page that holds every
        // row invites no further call.
        for gone in ["total_steps", "next_cursor", "chain_details"] {
            assert!(!text.contains(gone), "{gone}: {text}");
        }
    }

    /// A referrer opens a gap only when a later call could run a member
    /// through it. Once the census of where members are held as values is
    /// whole, a row of plain references holds none, so it goes. A row that
    /// holds a member as a value stays, one short line: the records say it
    /// sits in a collection and not under what name, or whether by position,
    /// so the calls that spell the member's name do not account for it. A
    /// row that constructs a member stays too.
    #[test]
    fn a_referrer_goes_when_it_holds_no_member_and_stays_when_it_holds_one() {
        const NAMER: &str = "8a0b2d46-7193-45b5-9f60-8b0c2d4e6f09";
        const MAKER: &str = "9b1c3e57-82a4-46c6-8a71-9c1d3e5f7a0a";
        let mut reply = react_reach();
        let member = reply["reach"]["members"][0]["id"].clone();
        reply["reach"]["also_referenced_by"]
            .as_array_mut()
            .unwrap()
            .extend([
                json!({"id": NAMER, "name": "isScopeQuery", "kind": "Function", "refs": [
                    {"how": "reference", "to": member, "to_name": "collectScopedNodes",
                     "code": "return typeof collectScopedNodes === 'function';"}]}),
                json!({"id": MAKER, "name": "makeScope", "kind": "Function", "refs": [
                    {"how": "instantiates", "to": member, "to_name": "collectScopedNodes",
                     "code": "new collectScopedNodes(node)"}]}),
            ]);
        let at = reply["readings"].as_array().unwrap().len() - 1;
        reply["readings"][at]["value"]["also_referenced_total"] = json!(4);
        let asked = json!({"focal": "isFiberSuspenseAndTimedOut", "direction": "callers"});
        let served = |reply: &Value| -> (Value, String) {
            (
                serde_json::from_str(&agent_reply(reply, "trace_data_flow", asked.clone(), false))
                    .unwrap(),
                agent_reply(reply, "trace_data_flow", asked.clone(), true),
            )
        };
        let names = |reply: &Value| -> Vec<String> {
            reply["reach"]["also_referenced_by"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .map(|row| row["name"].as_str().unwrap().to_string())
                        .collect()
                })
                .unwrap_or_default()
        };
        // Closed: the module that imports the focal and the function that
        // only names a member open nothing.
        let (closed, text) = served(&reply);
        assert_eq!(names(&closed), ["createScopeInstance", "makeScope"]);
        assert_eq!(
            closed["readings"]
                .as_array()
                .unwrap()
                .iter()
                .find(|reading| reading["key"] == "reach")
                .unwrap()["value"]["also_referenced_total"],
            2
        );
        assert!(!text.contains("isScopeQuery"), "{text}");
        // Kept: the holder, as one short line, and the constructor.
        for line in [
            "  held as a value in createScopeInstance #5b2f692b: DO_NOT_USE_queryAllNodes, DO_NOT_USE_queryFirstNode, getChildContextValues (collection)",
            "  referenced without a call by makeScope #9b1c3e57: collectScopedNodes",
        ] {
            assert!(text.lines().any(|written| written == line), "{line}\n{text}");
        }
        // Kept: every referrer that is not a module, while the census of
        // where members are held is not whole. A row with no value ref may
        // then hold one all the same.
        for unmeasured in ["incomplete", "uncensused", "absent"] {
            let mut open = reply.clone();
            let blind = &mut open["readings"][at]["value"]["blind_spots"];
            match unmeasured {
                "incomplete" => blind["value_holders"]["complete"] = json!(false),
                "uncensused" => {
                    blind["uncensused"] = json!({"members": [{"name": "collectScopedNodes"}]})
                }
                _ => {
                    blind.as_object_mut().unwrap().remove("value_holders");
                }
            }
            let (kept, text) = served(&open);
            assert_eq!(
                names(&kept),
                ["createScopeInstance", "isScopeQuery", "makeScope"],
                "{unmeasured}"
            );
            assert!(
                text.contains(
                    "  referenced without a call by isScopeQuery #8a0b2d46: collectScopedNodes"
                ),
                "{unmeasured}: {text}"
            );
        }
    }

    /// Impact takes no `details`; a call that passes one anyway is answered
    /// the same, on what must change and its open places.
    #[test]
    fn an_answered_impact_ends_the_same_when_a_call_passes_details() {
        use crate::server::{present_for_client, McpServerConfig};
        use crate::types::JsonRpcResponse;
        let config = McpServerConfig {
            plain_replies: true,
            agent_belt: true,
            ..Default::default()
        };
        let served = |arguments: Value| -> Value {
            let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "impact", "arguments": arguments}})
            .to_string();
            let result = json!({"content": [{"type": "text",
                "text": answered_impact().to_string()}]});
            let response = present_for_client(
                JsonRpcResponse::success(Some(json!(1)), result),
                &request,
                &config,
            );
            let result = response.result.unwrap();
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
        };
        let short = served(json!({"entity_ids": ["focal"]}));
        let detailed = served(json!({"entity_ids": ["focal"], "details": true}));
        assert_eq!(detailed, short);
        // The rows stay whole, signature and all, and no census follows them.
        assert_eq!(
            detailed["affected_callers"],
            answered_impact()["affected_callers"]
        );
        for key in [
            "entity_impacts",
            "enrichment",
            "affected_tests",
            "implementation_family",
        ] {
            assert!(detailed.get(key).is_none(), "{key}: {detailed}");
        }
        assert_eq!(
            detailed["sections"]["direct"]["call_sites"]["candidates"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(detailed["sections"]["direct"].get("verdict").is_none());
    }

    fn answered_references() -> Value {
        json!({
            "_kin": {"verdict": "complete except the sites listed below",
                "meaning": "Answer: these 2 functions call pop. Check a listed site only if its reason could apply, otherwise answer from the rows.",
                "delivered": "Every recorded reference row is shown, so the answer rows are complete. Later evidence pages hold only more unresolved sites; evidence_next_cursor pages them.",
                "page": {"recorded_rows_exhausted": true, "recorded_rows_has_more": false, "evidence_has_more": true},
                "searched": "Checked the admitted semantic inventory; coverage is limited by the verdict. Code that builds a name while it runs is outside what any reader of code can follow; none of the 14 such places here spells pop. The language server for this code failed to start, so links only it can find are not listed.",
                "proof_codes": "imports: target resolved through imports"},
            "entity": {"entity_id": "e", "name": "AppContext.pop"},
            "references": [
                {"entity_id": "a", "qualified_name": "Flask.wsgi_app", "call_code": "ctx.pop(error)"},
                {"entity_id": "b", "qualified_name": "AppContext.__exit__", "call_code": "self.pop(exc)", "proof": "imports"}],
            "left_to_check": [{"why": "Calls to a method named pop on an object Kin could not identify.",
                "sites": [{"caller": "pull_lang_code", "code": "values.pop(\"lang\")"}]}],
            "passed_to_calls": {"why": "Calls that pass pop to other code.", "sites": [{"code": "expect(pop)"}]},
            "other_names": {"note": "Places where recorded bindings give pop another name.", "sites": [{"code": "x = pop"}]},
            "overridden_by": {"count": 0, "methods": [], "proven": 0,
                "note": "Recorded overrides and implementations, not callers. Each row retains its resolution.",
                "coverage": {"recorded_closure": "unmeasured", "repository_completeness": "unmeasured"},
                "gaps": [{"code": "repository_override_coverage_unmeasured", "action": "report_scope_limit"}]},
            "evidence_detail": "For these retained evidence entries (not proven callers), repeat this request or its incoming cursor with details:true.",
            "relation_kinds": ["calls"],
            "caller_scope": {"include_tests": false, "excluded_role": "test"},
            "next_cursor": "later-evidence",
            "evidence_next_cursor": "later-evidence",
            "readings": [
                {"key": "_kin", "value": {"envelope_version": 2, "durability": {"state": "recorded"}}},
                {"key": "query", "value": "pop"},
                {"key": "candidate_count", "value": 3},
                {"key": "call_site_candidates", "value": {"named": []}}],
        })
    }

    #[test]
    fn an_answered_reference_reply_ends_on_its_answer_and_open_places() {
        let original = answered_references();
        let mut reply = original.as_object().unwrap().clone();
        assert!(quiet_answered_references(&mut reply));
        let reply = Value::Object(reply);
        // Cursors to later evidence, their sentence, the details invitation,
        // the echoes of the question and the page accounting go.
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
        // The searched line keeps only what is specific to this answer.
        assert_eq!(
            reply["_kin"]["searched"],
            "The language server for this code failed to start, so links only it can find are not listed."
        );
        // An override census that lists nothing, over unmeasured coverage,
        // qualifies no row.
        assert!(reply.get("overridden_by").is_none(), "{reply}");
        // Readings that restate the question or the first page's envelope go;
        // evidence readings stay.
        let readings: Vec<&str> = reply["readings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|reading| reading["key"].as_str().unwrap())
            .collect();
        assert_eq!(readings, ["call_site_candidates"]);
        // The answer, its evidence lines and every open place stay.
        for key in [
            "entity",
            "references",
            "left_to_check",
            "passed_to_calls",
            "other_names",
        ] {
            assert_eq!(reply[key], original[key], "{key}");
        }
        for key in ["verdict", "meaning", "proof_codes"] {
            assert_eq!(reply["_kin"][key], original["_kin"][key], "{key}");
        }

        // Nothing changes while answer rows follow, or the answer is partial.
        let mut following = original.clone();
        following["answer_next_cursor"] = json!("more-rows");
        let mut kept = following.as_object().unwrap().clone();
        assert!(!quiet_answered_references(&mut kept));
        assert_eq!(Value::Object(kept), following);
        let mut paged = original.clone();
        paged["_kin"]["page"]["recorded_rows_has_more"] = json!(true);
        let mut kept = paged.as_object().unwrap().clone();
        assert!(!quiet_answered_references(&mut kept));
        assert_eq!(Value::Object(kept), paged);
        let mut partial = original.clone();
        partial["_kin"]["verdict"] = json!("partial");
        let mut kept = partial.as_object().unwrap().clone();
        assert!(!quiet_answered_references(&mut kept));
        assert_eq!(Value::Object(kept), partial);
    }

    #[test]
    fn an_answered_same_name_reply_quiets_every_section_and_keeps_override_rows() {
        let mut section = answered_references();
        section.as_object_mut().unwrap().remove("readings");
        section["overridden_by"] = json!({"count": 1, "proven": 1,
            "note": "Recorded overrides and implementations, not callers. Each row retains its resolution.",
            "methods": [{"entity_id": "o", "name": "Sub.pop"}],
            "coverage": {"recorded_closure": "complete", "repository_completeness": "unmeasured"},
            "gaps": [{"code": "repository_override_coverage_unmeasured", "action": "report_scope_limit"}]});
        let mut none_complete = answered_references();
        none_complete.as_object_mut().unwrap().remove("readings");
        none_complete["overridden_by"] = json!({"count": 0, "methods": [], "proven": 0,
            "note": "Recorded overrides and implementations, not callers.",
            "coverage": {"recorded_closure": "complete", "repository_completeness": "complete"}});
        let original = json!({
            "_kin": {"verdict": "complete except the sites listed below",
                "meaning": "Answer: 2 entities are named pop.",
                "searched": "Checked the admitted semantic inventory; coverage is limited by the verdict."},
            "answers": [section, none_complete],
            "relation_kinds": ["calls"],
        });
        let mut reply = original.as_object().unwrap().clone();
        assert!(quiet_answered_references(&mut reply));
        let reply = Value::Object(reply);
        assert!(reply["_kin"].get("searched").is_none(), "{reply}");
        assert!(reply.get("relation_kinds").is_none());
        let first = &reply["answers"][0];
        for key in ["next_cursor", "evidence_next_cursor", "evidence_detail"] {
            assert!(first.get(key).is_none(), "{key}");
        }
        // Override rows keep their note and the limits that qualify them; the
        // census counts go.
        let overrides = &first["overridden_by"];
        assert_eq!(
            overrides["methods"],
            original["answers"][0]["overridden_by"]["methods"]
        );
        assert_eq!(
            overrides["gaps"],
            original["answers"][0]["overridden_by"]["gaps"]
        );
        assert!(overrides["note"].is_string());
        assert!(overrides.get("coverage").is_none() && overrides.get("proven").is_none());
        // No override over complete coverage says so in two words.
        assert_eq!(
            reply["answers"][1]["overridden_by"],
            json!({"count": 0, "coverage": "complete"})
        );
        assert_eq!(first["references"], original["answers"][0]["references"]);

        // A section whose rows continue keeps the whole reply as it was.
        let mut following = original.clone();
        following["answers"][1]["answer_next_cursor"] = json!("more-rows");
        let mut kept = following.as_object().unwrap().clone();
        assert!(!quiet_answered_references(&mut kept));
        assert_eq!(Value::Object(kept), following);
    }

    fn answered_impact() -> Value {
        json!({
            "_kin": {"verdict": "complete except indirect uses",
                "meaning": "Answer: these are the entities a change to it could affect.",
                "searched": "Searched recorded entity and relationship evidence; coverage is limited by the verdict."},
            "affected_callers": [{"id": "c1", "name": "renderPreamble", "signature": "renderPreamble(request)"}],
            "affected_overrides": [],
            "affected_dependents": [
                {"id": "c1", "name": "renderPreamble"},
                {"id": "d1", "name": "performWork"}],
            "affected_tests": [],
            "affected_work_items": [],
            "actor_attribution": [],
            "unreviewed_agent_changes": [],
            "changed_ids": ["focal"],
            "entity_impacts": [{"entity_id": "focal", "consumer_count": 1, "covering_tests": 0}],
            "enrichment": {"status": "no_recorded_call_site_debt", "owed_entities": 0,
                "total_pending_entities": 0, "pending_entities": [], "entities_withheld": 0},
            "same_name_implementations": [{"id": "s1", "name": "pushSegmentFinale", "resolution": "same_name"}],
            "same_name_implementations_note": "These are declarations with the same name.",
            "implementation_family": [{"entity_id": "focal",
                "coverage": {"repository_completeness": "complete", "recorded_classes_read": 149}}],
            "implementation_family_note": "Every member of a method's implementation family changes.",
            "sections": {
                "direct": {"rows": ["affected_callers"],
                    "verdict": {"state": "certified_except", "inputs": {"absence_gate": "certified"}},
                    "call_sites": {"callers": 54, "sites": 6,
                        "candidates": [{"callee": "pushSegmentFinale", "caller": "c9",
                            "caller_name": "flushSegment", "expr": "config.pushSegmentFinale(chunks)",
                            "line_in_entity": 4, "reason": "callee spells the name", "state": "unresolved"}],
                        "focal_escape": {"residue": {"values": {"count": 1, "sites": [
                            {"inside": "createConfig", "expr": "cfg.finale = pushSegmentFinale"}]}}}}},
                "dependents": {"rows": ["affected_dependents"],
                    "verdict": {"state": "inconclusive", "limiting_factor": "call_sites_unresolved",
                        "inputs": {"absence_gate": "inconclusive"}}},
            },
        })
    }

    #[test]
    fn an_answered_impact_ends_on_what_must_change_and_its_open_places() {
        let original = answered_impact();
        let mut reply = original.as_object().unwrap().clone();
        assert!(quiet_answered_impact(&mut reply));
        let reply = Value::Object(reply);
        assert_eq!(
            reply,
            json!({
                "_kin": {"verdict": "complete except indirect uses",
                    "meaning": "Answer: these are the entities a change to it could affect."},
                "affected_callers": original["affected_callers"],
                // An empty callers or overrides bucket is an answer and stays.
                "affected_overrides": [],
                // A dependent the direct rows already list is not repeated.
                "affected_dependents": [{"id": "d1", "name": "performWork"}],
                "changed_ids": ["focal"],
                "same_name_implementations": original["same_name_implementations"],
                "same_name_implementations_note": original["same_name_implementations_note"],
                "sections": {
                    // Each open place, one short line, where the meaning
                    // names them.
                    "direct": {"call_sites": {"candidates": [
                        {"caller": "flushSegment", "code": "config.pushSegmentFinale(chunks)",
                         "why": "callee spells the name"},
                        {"caller": "createConfig", "code": "cfg.finale = pushSegmentFinale",
                         "why": "holds it as a value"}]}},
                    // Listed dependents keep their section's state.
                    "dependents": {"verdict": {"state": "inconclusive",
                        "limiting_factor": "call_sites_unresolved"}},
                },
            })
        );

        // A family that says another override could exist stays whole.
        let mut open = original.clone();
        open["implementation_family"][0]["coverage"]["repository_completeness"] =
            json!("incomplete");
        let mut quieted = open.as_object().unwrap().clone();
        assert!(quiet_answered_impact(&mut quieted));
        assert_eq!(
            quieted["implementation_family"],
            open["implementation_family"]
        );
        assert_eq!(
            quieted["implementation_family_note"],
            open["implementation_family_note"]
        );
        // An enrichment observation that owes work stays.
        let mut owed = original.clone();
        owed["enrichment"]["owed_entities"] = json!(4);
        let mut quieted = owed.as_object().unwrap().clone();
        assert!(quiet_answered_impact(&mut quieted));
        assert_eq!(quieted["enrichment"], owed["enrichment"]);

        // A partial answer, a page with more after it, or a page of a frozen
        // answer is left as it is.
        for change in ["partial", "cursor", "readings"] {
            let mut kept = original.clone();
            match change {
                "partial" => kept["_kin"]["verdict"] = json!("partial"),
                "cursor" => kept["next_cursor"] = json!("more"),
                _ => kept["readings"] = json!([{"key": "sections", "value": {}}]),
            }
            let mut reply = kept.as_object().unwrap().clone();
            assert!(!quiet_answered_impact(&mut reply), "{change}");
            assert_eq!(Value::Object(reply), kept, "{change}");
        }
    }

    /// The methods that may implement a changed interface method are direct
    /// rows: a dependent they already list is not repeated, and their note
    /// stays with them and goes without them.
    #[test]
    fn an_answered_impact_keeps_interface_implementations_as_direct_rows() {
        let mut original = answered_impact();
        original["affected_implementations"] = json!([{"id": "d1", "name": "fileWriter.Write", "resolution": "implements_interface_candidate"}]);
        original["affected_implementations_note"] = json!("These methods belong to types.");
        let mut reply = original.as_object().unwrap().clone();
        assert!(quiet_answered_impact(&mut reply));
        assert_eq!(
            reply["affected_implementations"],
            original["affected_implementations"]
        );
        assert_eq!(
            reply["affected_implementations_note"],
            original["affected_implementations_note"]
        );
        // Both dependents are direct rows already, so none is listed again.
        assert!(reply.get("affected_dependents").is_none(), "{reply:?}");

        let mut stray = answered_impact();
        stray["affected_implementations_note"] = json!("These methods belong to types.");
        let mut reply = stray.as_object().unwrap().clone();
        assert!(quiet_answered_impact(&mut reply));
        assert!(reply.get("affected_implementations_note").is_none());
    }

    #[test]
    fn only_a_single_page_impact_is_presented_as_answered() {
        let page = |envelope: Value, cursor: Value| {
            json!({"content": [{"type": "text", "text": json!({"_kin": envelope,
                "affected_callers": [], "next_cursor": cursor}).to_string()}]})
        };
        assert!(impact_single_page(&page(
            json!({"page": {"complete": true}}),
            Value::Null
        )));
        assert!(impact_single_page(&page(json!({}), Value::Null)));
        // The last page of a paged answer holds only part of it.
        assert!(!impact_single_page(&page(
            json!({"page": {"complete": false, "has_more": false}}),
            Value::Null
        )));
        assert!(!impact_single_page(&page(
            json!({"page": {"complete": false}}),
            json!("more")
        )));
        let error = json!({"isError": true, "content": [{"type": "text", "text": "{}"}]});
        assert!(!impact_single_page(&error));

        let mut result = json!({"content": [{"type": "text",
            "text": answered_impact().to_string()}]});
        present_answered_impact(&mut result);
        let served: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(served.get("entity_impacts").is_none() && served.get("affected_tests").is_none());
        let mut refused = error.clone();
        present_answered_impact(&mut refused);
        assert_eq!(refused, error);
    }

    #[test]
    fn hiding_a_tiny_chain_never_costs_more_than_the_chain() {
        let mut original = json!({"_kin":{"verdict":"partial"},"direction":"callers","chain":[{}],
            "reach":{"state":"closed","bounds":[],"frontier_total":0,"members_total":0,
                "members":[],"also_referenced_total":0}});
        let reply = presented(original.clone());
        assert_eq!(reply, original);
        // An answered reach has no display chain at all, and so no notice
        // about one: nothing is added in its place.
        original["_kin"]["verdict"] = json!("complete");
        let reply = presented(original.clone());
        original.as_object_mut().unwrap().remove("chain");
        assert_eq!(reply, original);
    }

    #[test]
    fn an_unscoped_walk_names_the_scope_that_keeps_its_largest_package() {
        let reply = presented(walk());
        let narrow = reply["_kin"][NARROW_KEY].as_str().unwrap();
        assert!(
            narrow.starts_with("13 of the 43 steps sit outside packages.react_reconciler"),
            "{narrow}"
        );
        assert!(
            narrow.contains("scope: \"packages.react_reconciler\""),
            "{narrow}"
        );
        assert_eq!(reply["modules"]["packages"][1]["steps"], 13);
        assert!(
            reply.get("readings").is_none(),
            "the census moved out of readings"
        );
        assert_eq!(reply["next_cursor"], "frozen");
        assert_eq!(reply["_kin"]["verdict"], "partial");
    }

    #[test]
    fn walk_rows_drop_only_what_they_restate_or_an_entity_id_serves() {
        let original = walk();
        let reply = presented(original.clone());
        let chain = reply["chain"].as_array().unwrap();
        assert_eq!(chain.len(), 3);
        // The signature and a site list that repeats the one code line go.
        assert!(chain[0].get("signature").is_none());
        assert!(chain[0].get("sites").is_none());
        assert!(chain[0].get("kind").is_none());
        assert_eq!(chain[0]["code"], original["chain"][0]["code"]);
        // A module keeps its kind and its import line, which is its only code.
        assert_eq!(chain[1]["kind"], "Module");
        assert_eq!(chain[1]["sites"], original["chain"][1]["sites"]);
        // Sites that add a second call stay.
        assert_eq!(chain[2]["sites"], original["chain"][2]["sites"]);
        for (step, before) in chain.iter().zip(original["chain"].as_array().unwrap()) {
            for key in [
                "step",
                "parent_step",
                "depth",
                "entity_id",
                "name",
                "how",
                "role",
            ] {
                assert_eq!(step[key], before[key], "{key}");
            }
        }
        assert!(reply["reach"]["members"][0].get("kind").is_none());
        // A type-resolved proof on an unproven path stays whole.
        assert_eq!(reply["reach"]["members"][0]["proof"], "type_resolved");
        let mut proven = original.clone();
        proven["reach"]["members"][0]["proven_path"] = json!(true);
        let proven = presented(proven);
        assert!(proven["reach"]["members"][0].get("proof").is_none());
        assert!(proven["reach"]["members"][0].get("proven_path").is_none());
        assert!(proven["_kin"][ROW_NOTE_KEY]
            .as_str()
            .unwrap()
            .contains("no proof"));
        assert!(reply["_kin"][ROW_NOTE_KEY]
            .as_str()
            .unwrap()
            .contains("entity_id"));
        // Shortened rows only ever save bytes; the scope line costs at most
        // its allowance, which the pager reserves.
        let size_of = |reply: &Value| {
            let mut result = json!({"content": [{"type": "text", "text": reply.to_string()}]});
            present_trace(&mut result);
            result["content"][0]["text"].as_str().unwrap().len()
        };
        let mut uncounted = original.clone();
        uncounted.as_object_mut().unwrap().remove("readings");
        assert!(size_of(&uncounted) < uncounted.to_string().len());
        assert!(size_of(&original) <= original.to_string().len() + NARROW_ALLOWANCE_BYTES);
    }

    /// On an agent connection a walk is short by default, and `details:true`
    /// serves every row whole. Neither brings back what an answered reach
    /// leaves out: its display chain, its step count and depth, and the
    /// cursor to its remaining evidence. A reach whose rows are not all on
    /// the page keeps its chain and its cursor, and a bare cursor continues
    /// in the form its first page was served in.
    #[test]
    fn trace_details_serve_every_row_whole_and_a_cursor_inherits_the_choice() {
        use crate::server::{present_for_client, McpServerConfig};
        use crate::types::JsonRpcResponse;
        let config = McpServerConfig {
            plain_replies: true,
            agent_belt: true,
            ..Default::default()
        };
        let mut machine = json!({
            "focal_id": "535ce03c-ed5e-46fa-8603-8de10f52f6d4", "focal_name": "isFiberSuspenseAndTimedOut",
            "chain": [{"step": 1, "parent_step": 0, "depth": 1, "role": "caller",
                "relation_kind": "Calls", "resolution": "type_resolved",
                "entity_id": "7c49126d-6e98-4fc4-9f61-ccec9c5ba038", "entity_name": "collectScopedNodes",
                "entity_kind": "Function", "code": "if (isFiberSuspenseAndTimedOut(node)) {".repeat(16),
                "signature": "function collectScopedNodes(node: Fiber): void",
                "sites": [{"code": "if (isFiberSuspenseAndTimedOut(node)) {"}]}],
            "direction":"callers", "depth":1,
            "reach":{"state":"closed","bounds":[],"members_total":1,"frontier_total":0,
                "frontier":[],"also_referenced_total":0,
                "members":[{"id":"caller","name":"caller","kind":"Function","via":"focal",
                    "proof":"type_resolved","proven_path":true}]},
            "total_steps": 1, "next_cursor": "frozen-walk",
            "_kin": {"semantic_surface": 1,
                "verdict": {"state": "inconclusive", "safe_to_conclude_absent": false},
                "page": {"version": 1, "complete": false, "has_more": true}},
        });
        let served = |machine: &Value, arguments: Value| {
            let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "trace", "arguments": arguments}})
            .to_string();
            let result = json!({"content": [{"type": "text", "text": machine.to_string()}]});
            let response = present_for_client(
                JsonRpcResponse::success(Some(json!(1)), result),
                &request,
                &config,
            );
            let result = response.result.unwrap();
            let text = result["content"][0]["text"].as_str().unwrap();
            serde_json::from_str::<Value>(text).unwrap_or_else(|_| panic!("{result}"))
        };
        let plain_of = |machine: &Value| -> Value {
            let mut plain = json!({"content": [{"type": "text", "text": machine.to_string()}]});
            crate::plain_reply::present(&mut plain, "trace_data_flow");
            serde_json::from_str(plain["content"][0]["text"].as_str().unwrap()).unwrap()
        };
        let plain = plain_of(&machine);
        assert!(plain["chain"].is_array() && plain["next_cursor"].is_string());
        let short = served(&machine, json!({"focal": "isFiberSuspenseAndTimedOut"}));
        let detailed = served(
            &machine,
            json!({"focal": "isFiberSuspenseAndTimedOut", "details": true}),
        );
        // A complete reach with every member on the page ends on its answer,
        // whichever form its rows are served in.
        for (form, reply) in [("short", &short), ("details", &detailed)] {
            for key in ["chain", "next_cursor", "total_steps", "depth"] {
                assert!(reply.get(key).is_none(), "{form} {key}: {reply}");
            }
            for key in [CHAIN_LEFT_OUT_KEY, "delivered", NARROW_KEY] {
                assert!(reply["_kin"].get(key).is_none(), "{form} {key}: {reply}");
            }
            assert_eq!(reply["_kin"]["verdict"], plain["_kin"]["verdict"], "{form}");
            assert_eq!(reply["_kin"]["meaning"], plain["_kin"]["meaning"], "{form}");
        }
        // Details serve the member whole; the short row leaves out the kind
        // and the proof that are the ordinary ones.
        assert_eq!(detailed["reach"], plain["reach"]);
        assert_eq!(detailed["reach"]["members"][0]["proof"], "type_resolved");
        assert!(short["reach"]["members"][0].get("proof").is_none());
        assert!(short["reach"]["members"][0].get("kind").is_none());
        // A connection that writes text ends the same way: the verdict line
        // and the member, with or without details.
        let mut text_config = McpServerConfig {
            plain_replies: true,
            agent_belt: true,
            ..Default::default()
        };
        text_config.session_handles = Some(Default::default());
        text_config.reply_format = crate::text_reply::ReplyFormat::Text;
        let written = |arguments: Value| -> String {
            let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "trace", "arguments": arguments}})
            .to_string();
            let result = json!({"content": [{"type": "text", "text": machine.to_string()}]});
            let response = present_for_client(
                JsonRpcResponse::success(Some(json!(1)), result),
                &request,
                &text_config,
            );
            response.result.unwrap()["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let text = written(json!({"focal": "isFiberSuspenseAndTimedOut", "details": true}));
        assert_eq!(
            text,
            written(json!({"focal": "isFiberSuspenseAndTimedOut"}))
        );
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("Answer (complete"), "{text}");
        assert_eq!(lines[1..], ["  caller  via=focal"], "{text}");
        // A reach whose rows are not all on the page keeps its chain, whole,
        // and its cursor.
        machine["reach"]["members_total"] = json!(2);
        let plain = plain_of(&machine);
        let detailed = served(
            &machine,
            json!({"focal": "isFiberSuspenseAndTimedOut", "details": true}),
        );
        assert_eq!(detailed, plain);
        assert!(detailed["chain"][0]["signature"].is_string());
        assert_eq!(detailed["next_cursor"], "frozen-walk");
        // The page that handed out this cursor asked for details, so a bare
        // continuation keeps them, and an explicit choice changes them.
        let continued = served(&machine, json!({"cursor": "frozen-walk"}));
        assert_eq!(continued, detailed);
        let shortened = served(&machine, json!({"cursor": "frozen-walk", "details": false}));
        assert!(shortened["chain"][0].get("signature").is_none());
        assert_eq!(shortened["next_cursor"], "frozen-walk");
    }

    #[test]
    fn neighbors_links_name_their_ends_and_every_id_is_recovered() {
        let entity =
            |id: &str, name: &str, kind: &str| json!({"entity_id": id, "name": name, "kind": kind});
        let link = |from: (&str, &str), how: &str, to: (&str, &str)| json!({"from": from.1, "from_id": from.0, "how": how, "to": to.1, "to_id": to.0});
        let hooks = ("6aad1611-ac83-4fd0-89bd-c5b5fe3cb4c4", "ReactFiberHooks");
        let mount = ("6c9517d3-3fc6-4ad4-86a8-6d7b3276ce29", "mountState");
        let twin_a = ("aaaaaaaa-0000-4000-8000-000000000001", "update");
        let twin_b = ("aaaaaaaa-0000-4000-8000-000000000002", "update");
        let outside = (
            "bbbbbbbb-0000-4000-8000-000000000003",
            "bbbbbbbb-0000-4000-8000-000000000003",
        );
        let original = json!({
            "_kin": {"verdict": "partial", "meaning": "Partial: these are the entities it links to."},
            "entities": [entity(hooks.0, hooks.1, "Module"), entity(mount.0, mount.1, "Function"),
                entity(twin_a.0, twin_a.1, "Method"), entity(twin_b.0, twin_b.1, "Method")],
            "links": (0..20).flat_map(|_| vec![
                link(hooks, "calls", mount), link(hooks, "calls", twin_a),
                link(hooks, "calls", twin_b), link(hooks, "references", outside)]).collect::<Vec<_>>(),
        });
        let mut result = json!({"content": [{"type": "text", "text": original.to_string()}]});
        present_neighbors(&mut result);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.len() < original.to_string().len());
        let reply: Value = serde_json::from_str(text).unwrap();
        assert_eq!(reply["_kin"][LINKS_NOTE_KEY], LINKS_NOTE);
        assert_eq!(reply["entities"][0]["kind"], "Module");
        assert!(reply["entities"][1].get("kind").is_none());
        // A unique name or a bare id drops its id; a shared name keeps it.
        let links = reply["links"].as_array().unwrap();
        assert_eq!(
            links[0],
            json!({"from": "ReactFiberHooks", "how": "calls", "to": "mountState"})
        );
        assert_eq!(links[1]["to_id"], twin_a.0);
        assert_eq!(links[2]["to_id"], twin_b.0);
        assert!(links[3].get("to_id").is_none() && links[3]["to"] == outside.1);
        // Every id comes back from the same reply.
        let by_name: std::collections::HashMap<&str, &str> = reply["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["name"].as_str().unwrap(),
                    row["entity_id"].as_str().unwrap(),
                )
            })
            .collect();
        for (short, full) in links.iter().zip(original["links"].as_array().unwrap()) {
            for (name, id) in [("from", "from_id"), ("to", "to_id")] {
                let end = short[name].as_str().unwrap();
                let recovered = short
                    .get(id)
                    .and_then(Value::as_str)
                    .or_else(|| by_name.get(end).copied())
                    .unwrap_or(end);
                assert_eq!(recovered, full[id].as_str().unwrap());
                assert_eq!(short[name], full[name]);
            }
            assert_eq!(short["how"], full["how"]);
        }
    }

    #[test]
    fn a_walk_in_one_package_or_a_fragment_page_is_left_alone() {
        let mut one = walk();
        one["readings"][0]["value"]["outside_largest"] = json!(0);
        assert!(presented(one)["_kin"].get(NARROW_KEY).is_none());
        let fragment = json!({"_kin": {"verdict": "partial"}, "record_fragment": {"text": "x"},
            "chain": [{"kind": "Function", "signature": "f()"}]});
        assert_eq!(presented(fragment.clone()), fragment);
        let error = json!({"isError": true, "content": [{"type": "text", "text": "refused"}]});
        let mut result = error.clone();
        present_trace(&mut result);
        assert_eq!(result, error);
    }
}
