// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Plain replies, read against replies a real repository produced.
//!
//! Each fixture is an agent-profile reply the 0.8.2 build served for Flask, with
//! the repository root and third-party source bodies replaced.

use serde_json::{json, Map, Value};

use super::*;

const CALLERS: &str = include_str!("testdata/plain_flask_callers_find_references.json");
const IMPACT: &str = include_str!("testdata/plain_flask_impact_find_references.json");
const SAME_NAME: &str = include_str!("testdata/plain_flask_deadcode_find_references.json");
const SOURCE: &str = include_str!("testdata/plain_flask_callers_get_entity_source_focal.json");
/// The machine reply a React store gave for the direct callers of
/// `react-dom-bindings.ReactFiberConfigDOM :: commitTextUpdate`.
const COMMIT_TEXT_UPDATE: &str =
    include_str!("testdata/plain_react_commit_text_update_find_references.json");
const TRACE: &str = include_str!("testdata/plain_flask_reach_trace_data_flow.json");
const CONTEXT: &str = include_str!("testdata/plain_flask_callers_get_context_pack_focal.json");

/// The claim scope a reference answer leads with when every relation kind was
/// selected. Each fixture asked for calls, imports and references.
const EVERY_KIND_SCOPE: &str = "This answer covers only relation kinds \
    [\"calls\",\"imports\",\"references\"]; unrequested relation kinds are outside its \
    claim. Within that scope: ";

/// Words an agent-profile reply must not use, in its keys or in the prose it
/// writes. The code an answer quotes is the repository's, and is not scanned.
const JARGON: [&str; 26] = [
    "certified",
    "inconclusive",
    "limiting_factor",
    "safe_to_conclude_absent",
    "named_leftovers",
    "value_calls",
    "census",
    "escape",
    "residue",
    "caller_arrival",
    "call_site_candidates",
    "source_derivation",
    "call_extraction",
    "excluded_artifacts",
    "enrichment",
    "hydration",
    "authority",
    "spine",
    "owed",
    "focal",
    "upstream",
    "projection",
    "edge_coverage",
    "binding_unproven",
    "type_resolved",
    "sites_partial_reason",
];

fn machine(fixture: &str) -> Map<String, Value> {
    match serde_json::from_str(fixture).expect("fixture parses") {
        Value::Object(map) => map,
        other => panic!("fixture is not an object: {other}"),
    }
}

fn rendered(fixture: &str, tool: &str) -> (Value, usize) {
    let mut result = json!({"content": [{"type": "text", "text": fixture}]});
    present(&mut result, tool);
    let text = result["content"][0]["text"].as_str().unwrap().to_string();
    (serde_json::from_str(&text).unwrap(), text.len())
}

/// Every prose key/string, excluding quoted code and typed evidence metadata.
/// Resolution and measured coverage retain canonical values so compaction
/// cannot erase the distinction between proof, candidates and unmeasured scope.
/// Their semantic retention is exercised by the focused override controls.
fn words(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                out.push(key.clone());
                if !matches!(
                    key.as_str(),
                    "code"
                        | "signature"
                        | "caller"
                        | "name"
                        | "class"
                        | "inside"
                        | "resolution"
                        | "coverage"
                ) && !key.ends_with("id")
                {
                    words(value, out);
                }
            }
        }
        Value::Array(rows) => rows.iter().for_each(|row| words(row, out)),
        Value::String(text) => out.push(text.clone()),
        _ => {}
    }
}

fn assert_plain(reply: &Value) {
    let mut found = Vec::new();
    words(reply, &mut found);
    for word in &found {
        for jargon in JARGON {
            assert!(
                !word.to_ascii_lowercase().contains(jargon),
                "`{jargon}` reached an agent in {word:?}\n{reply:#}"
            );
        }
        assert!(
            !word.contains('\u{2014}'),
            "an em dash reached an agent: {word}"
        );
    }
}

#[test]
fn plain_reference_rows_keep_an_unfinished_receiver_chain_without_promoting_it() {
    let chain = json!([
        {"step": "call", "line_in_entity": 2},
        {"step": "binding", "name": "store", "line_in_entity": 1,
         "code": "const store = useContext(LooseContext);"}
    ]);
    let missing = crate::receiver_binding::missing_json(&kin_index::receiver_origin::MissingStep {
        step: kin_index::receiver_origin::OriginStepKind::Provider,
        name: "LooseContext".into(),
        reason: "provider_value_unknown".into(),
        kind: Some("react_context".into()),
        file: None,
    });
    let row = json!({
        "entity_id": "caller", "name": "Loose", "kind": "Function",
        "resolution": "name_only", "relation_kinds": ["calls"],
        "receiver_binding": chain,
        "receiver_binding_missing": missing,
    });
    let plain = plain_reference_row_with_binding(&row, true);
    assert_plain(&plain);
    assert_eq!(plain["receiver_binding"], chain);
    assert_eq!(plain["receiver_binding_missing"], missing);
    assert_eq!(plain["proof"], "matched by name only");
}

#[test]
fn plain_reference_rows_do_not_invent_an_absent_receiver_proof() {
    for extra in [
        json!({}),
        json!({"receiver_binding": null, "receiver_binding_missing": null}),
    ] {
        let mut row = json!({"entity_id": "caller", "name": "run", "kind": "Function"});
        row.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let plain = plain_reference_row_with_binding(&row, true);
        assert!(plain
            .get(crate::receiver_binding::RECEIVER_BINDING_KEY)
            .is_none());
        assert!(plain
            .get(crate::receiver_binding::RECEIVER_BINDING_MISSING_KEY)
            .is_none());
    }
}

#[test]
fn requested_receiver_proof_preserves_a_partial_chain_and_its_missing_link() {
    let proof = json!({
        "entity_id": "caller", "name": "Loose", "kind": "Function",
        "status": "partial", "resolution": "name_only",
        "receiver_binding": [{"step": "binding", "name": "ctx"}],
        "receiver_binding_missing": {"step": "returns", "name": "Factory.make", "reason": "method_declares_no_return"},
    });
    let mut result = json!({"content": [{"type": "text", "text": json!({
        "focal_entity": {"id": "target", "name": "Context.pop", "kind": "Method"},
        "receiver_proof": proof,
    }).to_string()}]});
    present(&mut result, "find_references");
    let reply: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], PARTIAL);
    assert_eq!(
        reply["receiver_proof"]["receiver_binding"],
        proof["receiver_binding"]
    );
    assert_eq!(
        reply["receiver_proof"]["receiver_binding_missing"],
        proof["receiver_binding_missing"]
    );
    assert_eq!(reply["receiver_proof"]["proof"], "matched by name only");
}

#[test]
fn the_flask_callers_answer_reads_complete_with_what_is_left_to_check() {
    let (reply, bytes) = rendered(CALLERS, "find_references");
    assert_plain(&reply);
    assert!(bytes <= CALLERS.len(), "{bytes} > {}", CALLERS.len());
    assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT);
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    // The answer first, then what proves each row and what was checked.
    assert!(
        meaning.starts_with(&format!(
            "Answer: {EVERY_KIND_SCOPE}these 6 functions call AppContext.pop. Each row shows the \
             line of code that calls it. Kin checked the code it holds for every call, import and \
             reference that spells pop."
        )),
        "{meaning}"
    );
    // What was searched, and where every reader of code stops, is scope.
    assert_eq!(
        reply["_kin"]["searched"],
        "Checked the admitted semantic inventory; coverage is limited by the verdict. Code that \
         builds a name while it runs is outside what any reader of code can follow; none of the \
         14 such places here spells pop."
    );
    assert_eq!(reply["references"].as_array().unwrap().len(), 6);
    assert_eq!(reply["references"][0]["name"], "Flask.wsgi_app");
    assert_eq!(
        reply["references"][0]["sites"][0],
        json!({"line_in_caller": 50, "code": "ctx.pop(error)"})
    );
    let groups = reply[LEFT_TO_CHECK_KEY].as_array().unwrap();
    // The nine unresolved calls to a method named pop, each with its caller,
    // its line and its code, under one plain reason.
    assert!(groups[0]["why"]
        .as_str()
        .unwrap()
        .starts_with("Calls to a method named pop on an object Kin could not identify."));
    assert!(groups[0]["why"]
        .as_str()
        .unwrap()
        .contains("could be an AppContext"));
    let named = groups[0]["sites"].as_array().unwrap();
    assert_eq!(named.len(), 9);
    assert_eq!(
        named[0],
        json!({
            "caller": "test_url_processors.<locals>.pull_lang_code",
            "caller_id": "1548a800-31aa-46ab-a39e-23654d19abc1",
            "line_in_caller": 2,
            "code": "values.pop(\"lang_code\", None)",
            "receiver": "values",
        })
    );
    // Then the place pop is bound under another name.
    let reasons: Vec<&str> = groups.iter().map(|g| g["why"].as_str().unwrap()).collect();
    assert!(reasons[1].starts_with("Places that assign something named pop"));
    // The 14 lookups by a name no record follows are no evidence about pop:
    // they are scope, not a group to check.
    assert_eq!(groups.len(), 2, "{groups:#?}");
    assert!(
        !reasons.iter().any(|reason| reason.contains("looks")),
        "{reasons:?}"
    );
    // A call that passes pop to other code is evidence of its own kind.
    assert_eq!(
        reply[PASSED_TO_CALLS_KEY]["sites"][0]["code"],
        "expect_exception(flask.session.pop, \"foo\")"
    );
    assert_eq!(
        meaning,
        format!(
            "Answer: {EVERY_KIND_SCOPE}these 6 functions call AppContext.pop. Each row shows the \
             line of code that calls it. Kin checked the code it holds for every call, import and \
             reference that spells pop. 1 call passes it as an argument to other code, which may \
             call it; passed_to_calls lists it with its code; 10 other places could reach it only \
             in the way their group in left_to_check says. Check a listed site only if its reason \
             could apply, otherwise answer from the rows."
        )
    );
    // Overrides Kin could not track are named plainly.
    assert_eq!(
        reply["overridden_by"]["possible"][0]["class"],
        "AppContextProxy"
    );
}

/// The Flask callers reply with its one passed value moved into, or handed
/// on by, the function `passer` names, and `to` recorded as what it is
/// handed to.
fn callers_with_passed(passer: (&str, &str), to: Option<Value>) -> Map<String, Value> {
    let mut machine = machine(CALLERS);
    let site = machine
        .get_mut("call_site_candidates")
        .and_then(|candidates| candidates.pointer_mut("/value_calls/census/residue/values/sites/0"))
        .expect("the recorded reply lists one value use");
    site["inside"] = json!(passer.0);
    site["inside_id"] = json!(passer.1);
    if let Some(to) = to {
        site[crate::site_expression::PASSED_TO_KEY] = to;
    }
    machine
}

/// A value handed on by a function that is not a row is listed, and its note
/// names who hands it over, to what, and which of the two to count.
#[test]
fn a_passed_value_says_who_passes_it_to_what_and_whom_to_count() {
    let outsider = (
        "test_missing_session",
        "7975d788-0000-4000-8000-000000000001",
    );
    let reply = plain_answer(callers_with_passed(outsider, None), "find_references");
    assert_plain(&reply);
    let group = &reply[PASSED_TO_CALLS_KEY];
    let why = group["why"].as_str().unwrap();
    assert!(
        why.ends_with(
            "the function to count as a caller is the one that passes it, never the code it is \
             passed to."
        ),
        "{why}"
    );
    assert!(!why.contains("count one"), "{why}");
    assert_eq!(
        group["sites"][0]["note"],
        "test_missing_session passes it to other code as an argument; if that code calls what \
         it is given, the caller to count is test_missing_session."
    );

    // The call it sits in, when the answer read it.
    let to = json!({"callee": "expect_exception", "position": 0});
    let reply = plain_answer(callers_with_passed(outsider, Some(to)), "find_references");
    assert_eq!(
        reply[PASSED_TO_CALLS_KEY]["sites"][0]["note"],
        "test_missing_session passes it to expect_exception; if expect_exception calls what it \
         is given, the caller to count is test_missing_session, not expect_exception."
    );

    // The declaration that call reaches calls its parameter: nothing is left
    // for the reader to work out.
    let to = json!({
        "callee": "expect_exception",
        "position": 0,
        "target_id": "7975d788-0000-4000-8000-000000000002",
        "target": "test_missing_session.<locals>.expect_exception",
        "calls_parameter": "f",
    });
    let reply = plain_answer(callers_with_passed(outsider, Some(to)), "find_references");
    assert_plain(&reply);
    assert_eq!(
        reply[PASSED_TO_CALLS_KEY]["sites"][0]["note"],
        "test_missing_session passes it to test_missing_session.<locals>.expect_exception, and \
         test_missing_session.<locals>.expect_exception calls what it is given (its parameter \
         f), so test_missing_session calls it through \
         test_missing_session.<locals>.expect_exception. Count test_missing_session, not \
         test_missing_session.<locals>.expect_exception."
    );
}

/// React's `commitHostTextUpdate` calls `commitTextUpdate` on one branch and
/// hands it to `runWithFiberInDEV` on the other. It is a row already, so the
/// handed-on use adds no function to the answer: it is counted, and nothing
/// is listed for a reader to open `runWithFiberInDEV` over.
#[test]
fn a_value_passed_by_a_function_that_is_already_a_row_is_counted_not_listed() {
    let machine_reply = machine(CALLERS);
    let row = machine_reply["references"][0].clone();
    let passer = (
        row["name"].as_str().unwrap().to_string(),
        row["entity_id"].as_str().unwrap().to_string(),
    );
    let to = json!({"callee": "run_in_context", "position": 0});
    let folded = || callers_with_passed((&passer.0, &passer.1), Some(to.clone()));
    let reply = plain_answer_with_details(folded(), "find_references", false);
    assert_plain(&reply);
    assert!(reply.get(PASSED_TO_CALLS_KEY).is_none(), "{reply:#}");
    let counted = settled_by(&reply, "to other code as an argument");
    assert_eq!(counted["folded_entries"], 1);
    assert_eq!(counted[GAP_REASONS_KEY], json!({PASSER_ALREADY_A_ROW: 1}));
    assert_eq!(
        counted["why"],
        "1 call that passes AppContext.pop to other code as an argument, each made by a \
         function that is already a row of this answer, so it adds no function to the answer. \
         Counted, not listed."
    );
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(!meaning.contains("passed_to_calls"), "{meaning}");
    assert!(
        meaning.contains(
            "1 more place that spells pop is settled: it is proved to add no function to \
             this answer, and settled counts it by reason."
        ),
        "{meaning}"
    );
    assert!(
        !reply.to_string().contains("run_in_context"),
        "the code it is handed to is not named anywhere: {reply:#}"
    );
    // Asked for in full, it is listed, with its note.
    let detailed = plain_answer_with_details(folded(), "find_references", true);
    assert_eq!(
        detailed[PASSED_TO_CALLS_KEY]["sites"][0]["note"],
        format!(
            "{0} passes it to run_in_context; if run_in_context calls what it is given, the \
             caller to count is {0}, not run_in_context.",
            passer.0
        )
    );
}

/// React's `commitTextUpdate`: the census finds one value use, the argument
/// `commitHostTextUpdate` hands to `runWithFiberInDEV`, and that function is
/// already a row. The use is real, so the machine verdict keeps its codes for
/// the residue and for calls through values. It adds no function, so the
/// plain answer is complete, not "complete except indirect uses" it then
/// does not list. A value use that is not settled keeps the weaker verdict.
#[test]
fn a_value_use_settled_by_its_passer_does_not_hold_the_answer_at_indirect_uses() {
    let recorded = machine(CALLERS);
    let row = recorded["references"][0].clone();
    let passer = (
        row["name"].as_str().unwrap().to_string(),
        row["entity_id"].as_str().unwrap().to_string(),
    );
    let shaped = |passer: (&str, &str), bindings: bool| {
        let to = json!({"callee": "runWithFiberInDEV", "position": 1});
        let mut machine = callers_with_passed(passer, Some(to));
        machine.insert(
            "_kin".into(),
            json!({"verdict": {"state": "certified_except", "safe_to_conclude_absent": false,
                "limiting_factor": "local_binding_unrelated; escape_residue_listed; \
                    named_leftovers; unkeyed_lookups_counted; value_calls_unmeasured"}}),
        );
        let candidates = &mut machine[crate::handlers::entities::CALL_SITE_CANDIDATES_KEY];
        candidates["named"] = json!([]);
        candidates["value_calls"]["sites"] = json!([]);
        if !bindings {
            candidates["value_calls"]["census"]["residue"]
                .as_object_mut()
                .unwrap()
                .remove("bindings");
        }
        machine
    };

    let reply = plain_answer_with_details(
        shaped((&passer.0, &passer.1), false),
        "find_references",
        false,
    );
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE, "{reply:#}");
    assert!(reply.get(LEFT_TO_CHECK_KEY).is_none(), "{reply:#}");
    assert!(reply.get(PASSED_TO_CALLS_KEY).is_none(), "{reply:#}");
    assert_eq!(
        settled_by(&reply, "to other code as an argument")[GAP_REASONS_KEY],
        json!({PASSER_ALREADY_A_ROW: 1})
    );
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(meaning.contains("you can answer from this"), "{meaning}");
    assert!(!meaning.contains("indirectly"), "{meaning}");

    // Passed on by a function that is not a row: listed, and not complete.
    let outsider = (
        "test_missing_session",
        "7975d788-0000-4000-8000-000000000001",
    );
    let reply = plain_answer_with_details(shaped(outsider, false), "find_references", false);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT, "{reply:#}");
    assert!(reply.get(PASSED_TO_CALLS_KEY).is_some(), "{reply:#}");

    // Another kind of place in the residue beside the settled use: the answer
    // is not complete.
    let reply = plain_answer_with_details(
        shaped((&passer.0, &passer.1), true),
        "find_references",
        false,
    );
    assert_ne!(reply["_kin"]["verdict"], COMPLETE, "{reply:#}");
}

/// The recorded React reply itself: two rows, and one value use passed on by
/// one of them. That use is the only leftover and it is settled, so the
/// answer is complete.
#[test]
fn the_recorded_commit_text_update_answer_is_complete() {
    let reply = plain_answer_with_details(machine(COMMIT_TEXT_UPDATE), "find_references", false);
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE, "{reply:#}");
    assert!(reply.get(LEFT_TO_CHECK_KEY).is_none(), "{reply:#}");
    assert!(reply.get(PASSED_TO_CALLS_KEY).is_none(), "{reply:#}");
    // The two calls sit in the rows themselves and are those rows' own
    // evidence; the value use is the one settled place.
    assert_eq!(reply[SETTLED_KEY].as_array().unwrap().len(), 1, "{reply:#}");
    assert_eq!(
        settled_by(&reply, "to other code as an argument")[GAP_REASONS_KEY],
        json!({PASSER_ALREADY_A_ROW: 1})
    );
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.contains("these 2 functions call commitTextUpdate"),
        "{meaning}"
    );
    assert!(meaning.contains("you can answer from this"), "{meaning}");
    assert!(
        meaning.contains("1 more place that spells commitTextUpdate is settled"),
        "{meaning}"
    );
    assert!(!meaning.contains("indirectly"), "{meaning}");
}

#[test]
fn a_reply_that_names_a_residue_it_does_not_list_says_so() {
    let (reply, bytes) = rendered(IMPACT, "find_references");
    assert_plain(&reply);
    assert!(bytes <= IMPACT.len());
    assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT_INDIRECT);
    assert_eq!(
        reply["overridden_by"]["methods"].as_array().unwrap().len(),
        3
    );
}

#[test]
fn a_listed_census_is_a_residue_to_check() {
    let mut machine = machine(IMPACT);
    machine.insert(
        crate::handlers::entities::CALL_SITE_CANDIDATES_KEY.into(),
        json!({"named": [], "census": {"escape": "escapes", "residue": {"values": {"count": 1,
            "sites": [{"expr": "handler = si.open_session", "file": "src/app.py", "line": 9,
            "kind": "binding", "name": "open_session"}]}}}}),
    );
    let reply = plain_answer(machine, "find_references");
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT);
    assert_eq!(
        reply[LEFT_TO_CHECK_KEY][0]["sites"][0],
        json!({"code": "handler = si.open_session"})
    );
}

/// A non-call read no recorded reference carries is a place to check, listed
/// by the declaration it sits in and its code, as the verdict that names it
/// as residue says.
#[test]
fn an_unmatched_non_call_read_is_listed_to_check() {
    let mut machine = machine(IMPACT);
    machine.insert(
        "_kin".into(),
        json!({"verdict": {
            "state": "certified_except",
            "safe_to_conclude_absent": false,
            "limiting_factor": "reference_mentions_unmatched",
        }}),
    );
    machine.insert(
        crate::handlers::entities::CALL_SITE_CANDIDATES_KEY.into(),
        json!({"named": [], "census": {"escape": "contained", "residue": {"mentions": {
            "measured": true, "count": 2, "matched": {"imports": 1},
            "unmatched": {"count": 1, "sites": [{"file": "src/server.js", "line": 79,
                "kind": "import", "name": "pushSegmentFinale", "inside": "ReactFizzServer",
                "inside_id": "8315e7d9-61e5-4c36-bfe1-e4ea243eb714",
                "expr": "pushSegmentFinale,"}]}}}}}),
    );
    let reply = plain_answer(machine, "find_references");
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT);
    let group = &reply[LEFT_TO_CHECK_KEY][0];
    assert_eq!(
        group["sites"][0],
        json!({"code": "pushSegmentFinale,", "inside": "ReactFizzServer",
            "inside_id": "8315e7d9-61e5-4c36-bfe1-e4ea243eb714"})
    );
    assert!(
        group["why"]
            .as_str()
            .unwrap()
            .starts_with("Places that read open_session without calling it"),
        "{group}"
    );
}

/// A module whose imports were not measured says why in its own terms: code
/// imports a module by its path, and an import of some of its declarations is
/// recorded on them. It never says Kin does not record import lines.
#[test]
fn an_unmeasured_module_says_modules_are_imported_by_path() {
    let mut machine = machine(DEADCODE_BY_ID);
    machine["focal_entity"]["kind"] = json!("module");
    let reply = plain_answer(machine, "find_references");
    assert_eq!(reply["_kin"]["verdict"], PARTIAL);
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.contains("Left unchecked: it is a module, and code imports a module by the path"),
        "{meaning}"
    );
    assert!(!meaning.contains("import lines"), "{meaning}");
}

/// A reading left unmeasured by code that did not parse, or by records that
/// predate it, says which, rather than that Kin checked no such place.
#[test]
fn an_unmeasured_reading_names_why_it_was_not_measured() {
    for (reason, said) in [
        (
            kin_db::call_site_escape::MENTIONS_UNRECORDED,
            "some code that spells the name did not parse",
        ),
        (
            kin_db::call_site_escape::MENTIONS_UNLISTED,
            "records predate Kin's check",
        ),
        (
            kin_db::call_site_escape::MENTIONS_NOT_READ_FOR_LANGUAGE,
            "does not yet record, in this code's language",
        ),
    ] {
        let mut machine = machine(DEADCODE_BY_ID);
        machine.insert(
            crate::handlers::entities::CALL_SITE_CANDIDATES_KEY.into(),
            json!({"named": [], "census": {"escape": "contained", "residue": {
                "mentions": {"measured": false, "reason": reason}}}}),
        );
        let reply = plain_answer(machine, "find_references");
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert!(meaning.contains(said), "{meaning}");
    }
}

/// A reply whose only leftovers are lookups by a name no record follows reads
/// complete, with one sentence naming them. It never says nothing else uses
/// the entity, and it lists no site to check.
#[test]
fn an_answer_whose_only_leftovers_are_unkeyed_lookups_reads_complete() {
    let mut machine = machine(CALLERS);
    machine.insert(
        "_kin".into(),
        json!({"verdict": {
            "state": "certified_except",
            "safe_to_conclude_absent": false,
            "limiting_factor": "unkeyed_lookups_counted",
        }}),
    );
    machine.insert(
        crate::handlers::entities::CALL_SITE_CANDIDATES_KEY.into(),
        json!({"named": [], "census": {"escape": "contained", "residue": {"unkeyed_lookups": {
            "count": 13, "files": 6,
            "sites": [{"detail": "app = getattr(module, attr_name, None)", "file": "src/flask/cli.py",
                "kind": "computed_member", "line": 49}]}}}}),
    );
    let reply = plain_answer(machine, "find_references");
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE);
    assert_eq!(
        reply["_kin"]["meaning"],
        format!(
            "Answer: {EVERY_KIND_SCOPE}these 6 functions call AppContext.pop. Each row shows the \
             line of code that calls it. Kin checked the code it holds for every call, import and \
             reference that spells pop, and every place that passes or stores it as a value; you \
             can answer from this."
        )
    );
    assert!(reply["_kin"]["searched"]
        .as_str()
        .unwrap()
        .ends_with("none of the 13 such places here spells pop."));
    assert!(reply.get(LEFT_TO_CHECK_KEY).is_none(), "{reply:#}");
}

/// Rows matched by name alone are not proven, so they are served apart from
/// the rows a type proved, counted apart in the answer, and weighed in the
/// closing sentence; they are never a group left to check.
#[test]
fn rows_matched_by_name_alone_are_served_apart_from_proven_rows() {
    let mut machine = machine(CALLERS);
    machine.insert(
        "_kin".into(),
        json!({"verdict": {
            "state": "certified_except",
            "safe_to_conclude_absent": false,
            "limiting_factor": "named_leftovers; unkeyed_lookups_counted",
        }}),
    );
    machine["references"][1]["resolution"] = json!("name_unique");
    machine.insert(
        crate::handlers::entities::CALL_SITE_CANDIDATES_KEY.into(),
        json!({"named": [], "census": {"escape": "contained", "residue": {"unkeyed_lookups": {
            "count": 597, "sites": []}}}}),
    );
    let reply = plain_answer(machine, "find_references");
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT);
    assert_eq!(
        reply["_kin"]["meaning"],
        format!(
            "Answer: {EVERY_KIND_SCOPE}5 functions are proven to call AppContext.pop, and 1 more \
             calls a method named pop on an object Kin could not type; name_only lists it. Each \
             row shows the line of code that calls it. Kin checked the code it holds for every \
             call, import and reference that spells pop, and every place that passes or stores it \
             as a value. Count a name_only row only if its object could be an AppContext; check \
             any other listed site only if its reason could apply, and otherwise answer from the \
             rows."
        )
    );
    assert!(reply.get(LEFT_TO_CHECK_KEY).is_none(), "{reply:#}");
    assert_eq!(reply["references"].as_array().unwrap().len(), 5);
    let named = reply[NAME_ONLY_KEY].as_array().unwrap();
    assert_eq!(named.len(), 1);
    assert_eq!(named[0]["name"], "AppContext.__exit__");
    assert_eq!(named[0]["sites"][0]["code"], "self.pop(exc_value)");
    assert_eq!(named[0]["proof"], "matched by name only");
}

/// A dead-code answer: nothing calls the function. A lookup whose key spells
/// its name (`getattr(helpers, "send_file")`, which the adapter keeps as a use
/// of the name) and a lookup whose key is built to match it stay sites to
/// check, so the answer is never complete on its own; the lookups that spell
/// nothing are one sentence beside them.
#[test]
fn a_dead_code_answer_with_a_literal_lookup_key_stays_unresolved() {
    let mut machine = machine(CALLERS);
    machine.insert(
        "_kin".into(),
        json!({"verdict": {
            "state": "certified_except",
            "safe_to_conclude_absent": false,
            "limiting_factor": "escape_residue_listed; unkeyed_lookups_counted",
        }}),
    );
    machine.insert(
        "focal_entity".into(),
        json!({"id": "d8539973-4e5a-41b9-89f1-8b97677dbc7d", "kind": "function",
            "name": "send_file", "file": "src/flask/helpers.py"}),
    );
    machine.insert("references".into(), json!([]));
    machine.insert("total_upstream".into(), json!(0));
    machine.insert(
        crate::handlers::entities::CALL_SITE_CANDIDATES_KEY.into(),
        json!({"named": [], "census": {"escape": "escapes", "residue": {
            "values": {"count": 1, "sites": [{"expr": "handler = getattr(helpers, \"send_file\")",
                "file": "src/flask/app.py", "line": 9, "kind": "assignment", "name": "send_file",
                "inside": "Flask.handler", "inside_id": "0b99106c-5229-4fde-83a6-ac73678c3993"}]},
            "reflective": {"count": 1, "sites": [{"detail": "getattr(helpers, \"send_\" + kind)",
                "file": "src/flask/app.py", "line": 12, "kind": "computed_member"}]},
            "unkeyed_lookups": {"count": 13, "sites": []}}}}),
    );
    let reply = plain_answer(machine, "find_references");
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT);
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert_eq!(
        meaning,
        format!(
            "Answer: {EVERY_KIND_SCOPE}nothing uses send_file. Kin checked the code it holds for \
             every call, import and reference that spells send_file, and found none. 2 other \
             places could reach it only in the way their group in left_to_check says. Check a \
             listed site only if its reason could apply, otherwise answer that nothing uses it \
             within the selected scope."
        )
    );
    assert!(
        reply["_kin"]["searched"]
            .as_str()
            .unwrap()
            .ends_with("none of the 13 such places here spells send_file."),
        "{reply:#}"
    );
    let groups = reply[LEFT_TO_CHECK_KEY].as_array().unwrap();
    assert_eq!(groups.len(), 2, "{groups:#?}");
    assert_eq!(
        groups[0]["sites"][0],
        json!({"inside": "Flask.handler",
            "inside_id": "0b99106c-5229-4fde-83a6-ac73678c3993",
            "code": "handler = getattr(helpers, \"send_file\")"})
    );
    assert!(groups[1]["why"].as_str().unwrap().starts_with(
        "Code that looks a name up while it runs, where the name as written could be send_file."
    ));
}

/// React's reconciler calls `measureInstance(instance)` by a bare name. Listed
/// against a member of an object, such a call is not one "on an object Kin
/// could not identify": no object is written, and it runs the member only
/// where code binds the member to that name. Calls made on an object keep
/// their own group and reason.
#[test]
fn a_bare_call_listed_against_a_member_says_it_is_made_on_no_object() {
    let mut machine = machine(CALLERS);
    let site = |caller: &str, code: &str, bare: bool| {
        let mut row = json!({"callee": "pop", "caller": "11111111-2222-4333-8444-555555555555",
            "caller_name": caller, "expr": code, "line_in_entity": 3,
            "reason": "callee_spells_name", "state": "unresolved"});
        if bare {
            row["bare_call"] = json!(true);
        }
        row
    };
    machine.insert(
        crate::handlers::entities::CALL_SITE_CANDIDATES_KEY.into(),
        json!({"named": [
            site("teardown", "pop(error)", true),
            site("teardown_again", "pop(None)", true),
            site("finish", "values.pop(\"lang_code\")", false),
        ], "census": {"escape": "contained", "residue": {}}}),
    );
    let reply = plain_answer(machine, "find_references");
    assert_plain(&reply);
    let groups = reply[LEFT_TO_CHECK_KEY].as_array().unwrap();
    assert_eq!(groups.len(), 2, "{groups:#?}");
    let bare = groups[0]["why"].as_str().unwrap();
    assert!(
        bare.starts_with("Calls of the bare name pop, made on no object."),
        "{bare}"
    );
    assert!(bare.contains("could be an AppContext's"), "{bare}");
    assert_eq!(groups[0]["sites"].as_array().unwrap().len(), 2);
    assert!(groups[1]["why"]
        .as_str()
        .unwrap()
        .starts_with("Calls to a method named pop on an object Kin could not identify."));
    assert_eq!(groups[1]["sites"][0]["code"], "values.pop(\"lang_code\")");
}

/// A proved alias binding is metadata; coverage of calls through that name
/// remains the reference verdict's claim.
#[test]
fn another_name_for_the_entity_is_not_left_to_check() {
    let mut machine = machine(CALLERS);
    machine.insert(
        "_kin".into(),
        json!({"verdict": {
            "state": "certified_except",
            "safe_to_conclude_absent": false,
            "limiting_factor": "unkeyed_lookups_counted",
        }}),
    );
    machine.insert(
        crate::handlers::entities::CALL_SITE_CANDIDATES_KEY.into(),
        json!({"named": [], "census": {"escape": "contained", "residue": {
            "alias_names": ["ReactBrowserEventEmitterSetEnabled"],
            "alias_sites": {"count": 1, "sites": [{"detail": "ReactBrowserEventEmitterSetEnabled",
                "file": "packages/react-dom-bindings/src/client/ReactFiberConfigDOM.js",
                "kind": "import", "line": 106}]},
            "unkeyed_lookups": {"count": 964, "sites": []}}}}),
    );
    let reply = plain_answer(machine, "find_references");
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE);
    assert!(reply.get(LEFT_TO_CHECK_KEY).is_none(), "{reply:#}");
    assert_eq!(reply["other_names"]["count"], json!(null));
    assert_eq!(
        reply["other_names"]["sites"][0]["code"],
        "ReactBrowserEventEmitterSetEnabled"
    );
    let note = reply["other_names"]["note"].as_str().unwrap();
    assert!(note.contains("recorded bindings"));
    assert!(note.contains("coverage verdict"));
    assert!(!note.contains("included in references"));
    assert!(!note.contains("nothing here is left to check"));
    assert!(reply["_kin"]["searched"]
        .as_str()
        .unwrap()
        .contains("none of the 964 such places here spells"));
}

/// Any other answer whose only open code is counted unkeyed lookups reads
/// complete too.
#[test]
fn a_trace_whose_only_open_code_is_unkeyed_lookups_reads_complete() {
    let mut machine = machine(TRACE);
    machine.insert(
        "_kin".into(),
        json!({"verdict": {
            "state": "certified_except",
            "safe_to_conclude_absent": false,
            "limiting_factor": "unkeyed_lookups_counted",
        }}),
    );
    let reply = plain_answer(machine, "trace_data_flow");
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE);
    assert_eq!(
        reply["_kin"]["meaning"],
        "Answer: this chain lists every caller of App._find_error_handler up to 8 calls away, 7 \
         steps in all. Kin checked every call in the code it holds; you can answer from this."
    );
    assert!(reply["_kin"]["searched"]
        .as_str()
        .unwrap()
        .ends_with(super::UNKEYED_SCOPE));
}

#[test]
fn a_name_several_entities_share_is_answered_once_for_each() {
    let (reply, bytes) = rendered(SAME_NAME, "find_references");
    assert_plain(&reply);
    assert!(bytes <= SAME_NAME.len());
    let answers = reply["answers"].as_array().unwrap();
    assert_eq!(answers.len(), 2);
    assert_eq!(answers[0]["_kin"]["verdict"], COMPLETE);
    // A section searched what the reply around it searched, and says so.
    assert_eq!(
        answers[0]["_kin"]["meaning"],
        format!(
            "Answer: {EVERY_KIND_SCOPE}nothing uses _make_timedelta. Kin checked the code it holds \
             for every call, import and reference that spells _make_timedelta, and found none, so \
             it is safe to conclude _make_timedelta is unused within the selected scope; you can \
             answer from this."
        )
    );
    assert_eq!(answers[1]["_kin"]["verdict"], COMPLETE_EXCEPT);
    // Passed as a keyword argument to code that calls it.
    assert_eq!(
        answers[1][PASSED_TO_CALLS_KEY]["sites"][0]["code"],
        "get_converter=_make_timedelta,"
    );
    // This 0.8.2 reply recorded its selected kinds only per section, so the
    // enclosing claim says that selection is unknown rather than inferring it.
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.starts_with(
            "The selected relation kinds are not recorded in this reply, so that selection is \
             unknown; its claim does not establish coverage of other relation kinds. Within that \
             scope: 2 entities are named _make_timedelta."
        ),
        "{meaning}"
    );
    // The current producer records them beside the sections, and the
    // enclosing claim is scoped to exactly those kinds.
    let mut recorded = machine(SAME_NAME);
    recorded.insert(
        "relation_kinds".into(),
        json!(["calls", "imports", "references"]),
    );
    let reply = plain_answer(recorded, "find_references");
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.starts_with(&format!(
            "{EVERY_KIND_SCOPE}2 entities are named _make_timedelta."
        )),
        "{meaning}"
    );
    assert_eq!(
        reply["relation_kinds"],
        json!(["calls", "imports", "references"])
    );
}

/// A page that holds only some sections of a same-name answer says so: the
/// first page of a React answer said "1 entities are named this name" when
/// three were, and the other two followed under its answer cursor.
#[test]
fn a_page_holding_some_sections_never_counts_them_as_every_entity_of_the_name() {
    let mut first = machine(SAME_NAME);
    let sections = first[crate::handlers::entities::AMBIGUOUS_CANDIDATES_KEY]
        .as_array_mut()
        .unwrap();
    sections.truncate(1);
    // With no section count recorded, the answer cursor alone says more follow.
    first.remove("focal_resolution");
    first.insert("answer_next_cursor".into(), json!("more-sections"));
    let reply = plain_answer(first.clone(), "find_references");
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.contains(
            "This page answers 1 entity named _make_timedelta, and the answer cursor serves the rest."
        ),
        "{meaning}"
    );
    first.insert("focal_resolution".into(), json!({"sectioned": 3}));
    let reply = plain_answer(first, "find_references");
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.contains(
            "3 entities are named _make_timedelta; this page answers 1 of them, and the answer cursor serves the rest."
        ),
        "{meaning}"
    );
    // One section on a page that holds the whole answer reads as one.
    let mut alone = machine(SAME_NAME);
    alone[crate::handlers::entities::AMBIGUOUS_CANDIDATES_KEY]
        .as_array_mut()
        .unwrap()
        .truncate(1);
    alone.remove("focal_resolution");
    let reply = plain_answer(alone, "find_references");
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.contains("1 entity is named _make_timedelta."),
        "{meaning}"
    );
}

#[test]
fn source_trace_and_context_answers_carry_a_plain_verdict_and_no_working() {
    let (source, bytes) = rendered(SOURCE, "get_entity_source");
    assert_plain(&source);
    assert!(bytes <= SOURCE.len());
    assert_eq!(source["_kin"]["verdict"], COMPLETE);
    assert_eq!(
        source["_kin"]["meaning"],
        "Answer: this is the exact current source of AppContext.pop, the whole body."
    );
    // A read carries a compact reference to its edit base, which a write
    // resolves to the whole base; the whole base is a machine reply's.
    assert!(
        crate::source_ticket::is_ticket(&source["source_base"]),
        "an edit needs its base: {source:#}"
    );

    let (trace, bytes) = rendered(TRACE, "trace_data_flow");
    assert_plain(&trace);
    assert!(bytes <= TRACE.len());
    assert_eq!(trace["_kin"]["verdict"], COMPLETE_EXCEPT_INDIRECT);
    assert!(trace.get("chain").is_some());

    // The context lists some omitted IDs and counts other omitted rows.
    // Delivery must preserve that distinction without promising unlisted IDs.
    let (context, bytes) = rendered(CONTEXT, "get_context_pack");
    assert_plain(&context);
    assert!(bytes <= CONTEXT.len());
    assert_eq!(context["_kin"]["verdict"], COMPLETE);
    let meaning = context["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.starts_with("Answer: this is the context around "),
        "{meaning}"
    );
    assert_eq!(
        context["_kin"]["delivered"],
        "withheld lists entity IDs for omitted bodies or rows; read the listed IDs with source. Counts may include other rows whose IDs are not listed. 102 rows were omitted from the transitive context; their IDs are not listed."
    );
    assert_eq!(
        context["withheld"]["how"],
        "Read the listed entity IDs with source; counts may include other rows whose IDs are not listed."
    );
    assert_eq!(context["withheld"]["transitive_rows"], 102);
    assert_eq!(
        context["withheld"]["rows"]["dependents"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
}

#[test]
fn a_refused_request_gets_no_verdict() {
    let mut result = json!({
        "isError": true,
        "content": [{"type": "text", "text": json!({
            "_kin": {"verdict": {"state": "inconclusive"}},
            "error": "no entity named nothing_here",
        }).to_string()}],
    });
    present(&mut result, "find_references");
    let reply: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(reply, json!({"error": "no entity named nothing_here"}));
}

#[test]
fn a_note_after_the_answer_is_left_as_written() {
    let mut result = json!({"content": [
        {"type": "text", "text": SOURCE},
        {"type": "text", "text": "compact is not taken on this profile."},
    ]});
    present(&mut result, "get_entity_source");
    assert_eq!(
        result["content"][1]["text"],
        "compact is not taken on this profile."
    );
}

#[test]
fn every_limiting_code_has_a_plain_reason_or_is_a_listed_residue() {
    for clause in crate::verdict::CLAUSE_CODES {
        let reason = plain_reason(clause.code);
        if crate::verdict::is_residue_code(clause.code) {
            assert!(
                reason.is_none(),
                "{} is listed or counted, not explained",
                clause.code
            );
        } else {
            let reason = reason.unwrap();
            for jargon in JARGON {
                assert!(!reason.contains(jargon), "{}: {reason}", clause.code);
            }
        }
    }
}

#[test]
fn a_partial_answer_says_why_in_plain_words_and_what_fixes_it() {
    let mut machine = machine(CALLERS);
    machine.insert(
        "_kin".into(),
        json!({"verdict": {
            "state": "inconclusive",
            "safe_to_conclude_absent": false,
            "limiting_factor": "store_semantics_behind; reference_enrichment_unknown; call_sites_owed; \
                value_calls_unmeasured; method_call_resolution_incomplete; local_binding_unproven",
            "remedy": "Run `kin upgrade` in this repository.",
        }}),
    );
    machine.remove(crate::handlers::entities::CALL_SITE_CANDIDATES_KEY);
    let reply = plain_answer(machine, "find_references");
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], PARTIAL);
    assert_eq!(
        reply["_kin"]["fix"],
        "Run `kin upgrade` in this repository."
    );
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.starts_with(&format!(
            "Partial: {EVERY_KIND_SCOPE}these 6 functions call AppContext.pop, and there may be \
             more, because an older version of Kin built this repository's records;"
        )),
        "{meaning}"
    );
    assert!(
        !meaning.contains("could not check every place"),
        "{meaning}"
    );
}

#[test]
fn a_client_asks_for_machine_replies_at_initialize() {
    assert!(client_wants_machine_replies(
        &json!({"params": {"capabilities": {
        "experimental": {"kin": {"machineReplies": true}}}}})
    ));
    assert!(!client_wants_machine_replies(
        &json!({"params": {"capabilities": {}}})
    ));
}

#[test]
fn an_answer_from_another_repository_keeps_its_warning_and_renders_once() {
    let mut machine = machine(SOURCE);
    machine["_kin"]["repository"] = json!({
        "root": "/work/flask",
        "warning": "This answer is from /work/flask, not the folder you are working in.",
    });
    let text = Value::Object(machine).to_string();
    let (reply, _) = rendered(&text, "get_entity_source");
    assert_eq!(
        reply["_kin"]["warning"],
        "This answer is bound to the selected repository identity, which differs from the client context."
    );
    let again = reply.to_string();
    let (twice, _) = rendered(&again, "get_entity_source");
    assert_eq!(twice, reply, "a plain answer is not rendered a second time");
}

#[test]
fn each_search_coverage_code_says_what_it_means() {
    // A withheld test role or a missing body is not an unfinished index; only
    // the codes that mean the index is incomplete may say so.
    let tests = super::plain_reason("coverage_role_filter_withheld").unwrap();
    assert!(tests.contains("include_tests"), "{tests}");
    assert!(!tests.contains("not complete"), "{tests}");
    let body = super::plain_reason("coverage_graph_body_gap").unwrap();
    assert!(!body.contains("not complete"), "{body}");
    let floor = super::plain_reason("relevance_floor_unmeasured").unwrap();
    assert!(floor.contains("nearest"), "{floor}");
    assert_eq!(
        super::plain_reason("coverage_partial"),
        Some("the search index is not complete yet")
    );
}

/// The machine status a settled Flask store served, trimmed to the fields a
/// status reply is read from.
fn flask_status() -> Map<String, Value> {
    machine(
        &json!({
            "_kin": {
                "source_derivation": {"checked_scope": "admitted_inventory", "report": {
                    "full_adapter_sources": 83, "excluded_artifacts": 153,
                    "parse_coverage": "complete"}},
                "degraded": {},
                "verdict": {"state": "inconclusive", "safe_to_conclude_absent": false,
                    "limiting_factor": "binding_unproven; call_sites_unresolved"},
            },
            "entity_count": 1898,
            "relation_count": 12779,
            "embeddings_total": 3949, "embeddings_indexed": 3949, "embeddings_pending": 0,
            "call_sites": {"callers": 1898, "callers_owed": 0, "owed_file_count": 0,
                "sites": 4179, "by_state": {"unresolved": 940, "binding": 38}},
            "open_transactions": {"items": [], "page": {"total": 0}},
            "enrichment": {"files": [{"proof": "unverified"}], "page": {"next_cursor": "abc"}},
            "status_page": {"next_cursor": "abc"},
            "authority": "repo-daemon",
        })
        .to_string(),
    )
}

/// A status reply is a health report. The repository-wide count of calls Kin
/// could not match is a fact about the code, not a verdict on any answer, so
/// it gives the reply no verdict word, and the reply says what Kin holds and
/// what is settled.
#[test]
fn status_is_a_health_report_without_a_verdict() {
    let reply = plain_answer(flask_status(), "kin_graph_status");
    assert_plain(&reply);
    assert!(reply["_kin"].get("verdict").is_none(), "{reply:#}");
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert_eq!(
        meaning,
        "Health report for this repository, not a verdict on any answer: every answer carries \
         its own. Kin holds 1898 entities and 12779 relationships. Parsing is complete for the checked semantic inventory. No call is waiting to \
         be checked. All 3949 search vectors are built. No staged change is open."
    );
    assert!(!meaning.to_ascii_lowercase().contains("partial"));
    assert!(!meaning.contains("could not be matched"), "{meaning}");
    assert_eq!(reply["entities"], 1898);
    assert!(reply.get("code_files").is_none());
    assert_eq!(reply["callers_waiting_to_be_checked"], 0);
    assert_eq!(
        reply["search_vectors"],
        json!({"built": 3949, "total": 3949})
    );
    // The per-file list and its cursor are the machine reply's.
    for key in ["enrichment", "status_page", "call_sites", "authority"] {
        assert!(reply.get(key).is_none(), "{key} reached an agent");
    }
}

/// A setup that ranks without search vectors is not short of them, so the
/// status reply says what locate uses instead of counting vectors it will
/// never build.
#[test]
fn status_in_a_setup_without_search_vectors_counts_none_as_missing() {
    let mut machine = flask_status();
    machine["embeddings_indexed"] = json!(0);
    machine["embeddings_pending"] = json!(1898);
    machine["embeddings_total"] = json!(1898);
    machine.insert("search_vectors_used".into(), json!(false));
    let reply = plain_answer(machine, "kin_graph_status");
    assert_plain(&reply);
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.contains(
            "This setup uses no search vectors, so locate ranks descriptions by entity cards \
             and the graph."
        ),
        "{meaning}"
    );
    assert!(!meaning.contains("of 1898 search vectors"), "{meaning}");
    assert!(
        !meaning.to_ascii_lowercase().contains("partial"),
        "{meaning}"
    );
    assert_eq!(reply["search_vectors"], json!({"used": false}));
}

/// A description answer that ranked without search vectors says nothing about
/// an incomplete index, and says how it was ranked.
#[test]
fn a_locate_ranked_without_vectors_reports_no_index_gap() {
    let machine = machine(
        &json!({
            "_kin": {
                "verdict": {"state": "certified", "safe_to_conclude_absent": false},
            },
            "query": "register a function to run when the app context is torn down",
            "granularity": "entity",
            "surface": "compact",
            "entities": [{"id": "8430a5c8-26b7-4e0e-bb0a-bf046af752bb",
                "name": "App.teardown_appcontext", "kind": "method",
                "file": "src/flask/sansio/app.py", "line": 827, "score": 1.0}],
            "total_ranked": 1,
            "retrieval_arm": "cards",
            "ranked_by": "entity cards and the graph; this setup uses no search vectors",
            "semantic_coverage": {"indexed": 0, "total": 1898, "pending": 1898,
                "complete": true, "embedding_state": "not_used"},
        })
        .to_string(),
    );
    let reply = plain_answer(machine, "semantic_locate");
    assert_plain(&reply);
    let meaning = reply["_kin"]["meaning"].as_str().unwrap_or_default();
    assert!(!meaning.contains("search index"), "{meaning}");
    assert!(
        !meaning.to_ascii_lowercase().starts_with("partial"),
        "{meaning}"
    );
}

/// What does concern Kin's own health is still said, with its fix: a store an
/// older Kin built, and calls still being checked.
#[test]
fn status_says_what_is_not_settled_and_how_to_fix_it() {
    let mut machine = flask_status();
    machine["_kin"]["verdict"] = json!({"state": "inconclusive",
        "limiting_factor": "store_semantics_behind; call_sites_unresolved",
        "remedy": "Run `kin upgrade` in this repository."});
    machine["call_sites"]["callers_owed"] = json!(12);
    machine["call_sites"]["owed_file_count"] = json!(3);
    machine["embeddings_indexed"] = json!(3000);
    machine["embeddings_pending"] = json!(949);
    let reply = plain_answer(machine, "kin_graph_status");
    assert_plain(&reply);
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.contains("Kin is still checking the calls of 12 callers"),
        "{meaning}"
    );
    assert!(
        meaning.contains("3000 of 3949 search vectors are built"),
        "{meaning}"
    );
    assert!(
        meaning.ends_with("An older version of Kin built this repository's records."),
        "{meaning}"
    );
    assert_eq!(
        reply["_kin"]["fix"],
        "Run `kin upgrade` in this repository."
    );
}

/// A reference answer with no rows, from a reply that measured every value
/// use, says everything it checked: every code file, every use of the name,
/// and every place the entity could be held as a value.
#[test]
fn a_dead_code_answer_says_what_it_searched() {
    let mut machine = machine(CALLERS);
    machine["_kin"]["verdict"] = json!({"state": "certified", "safe_to_conclude_absent": true});
    machine.insert(
        "focal_entity".into(),
        json!({"id": "1d1f3fa7-bc54-4c75-8ef3-8d7d95c4ca87", "kind": "function",
            "name": "_make_timedelta", "projection": {"path": "src/flask/app.py"}}),
    );
    machine.insert("references".into(), json!([]));
    machine.insert("total_upstream".into(), json!(0));
    machine.remove("overridden_by");
    machine.insert(
        crate::handlers::entities::CALL_SITE_CANDIDATES_KEY.into(),
        json!({"named": [], "census": {"entities_checked": 1767, "escape": "contained",
            "residue": {}}}),
    );
    let reply = plain_answer(machine, "find_references");
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE);
    assert_eq!(
        reply["_kin"]["meaning"],
        format!(
            "Answer: {EVERY_KIND_SCOPE}nothing uses _make_timedelta. Kin checked the code it holds \
             for every call, import and reference that spells _make_timedelta, and every place \
             that passes or stores it as a value, and found none, so it is safe to conclude \
             _make_timedelta is unused within the selected scope; you can answer from this."
        )
    );
    assert!(reply.get(LEFT_TO_CHECK_KEY).is_none());
}

/// A row whose code is not quoted is counted, never claimed.
#[test]
fn the_evidence_sentence_never_claims_a_line_it_does_not_show() {
    let mut machine = machine(CALLERS);
    machine["references"][2]["sites"] = json!([{"line_in_entity": 10, "callee": "pop"}]);
    let reply = plain_answer(machine, "find_references");
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.contains(
            "Each row shows the line of code that calls it, except 1 row whose code is not \
             quoted; read one with source and its entity_id."
        ),
        "{meaning}"
    );
}

/// A trace step that carries its sites' code shows them in place of its bare
/// line numbers, and the answer says each step shows the line that links it.
#[test]
fn a_trace_step_shows_the_line_that_links_it_to_its_parent() {
    let mut machine = machine(TRACE);
    machine["_kin"]["verdict"] = json!({"state": "certified", "safe_to_conclude_absent": false});
    for step in machine["chain"].as_array_mut().unwrap() {
        step[crate::site_expression::TRACE_REFERENCE_SITES_KEY] = json!([
            {"line": 946, "expr": "handler = self._find_error_handler(e, ctx.request.blueprints)"}
        ]);
    }
    // A step with no recorded line is counted, never claimed.
    let mut one_bare = machine.clone();
    one_bare["chain"][1]
        .as_object_mut()
        .unwrap()
        .remove(crate::site_expression::TRACE_REFERENCE_SITES_KEY);
    let bare = plain_answer(one_bare, "trace_data_flow");
    assert!(
        bare["_kin"]["meaning"].as_str().unwrap().contains(
            "shows the line of code that links the two, except 1 step whose line Kin did not record"
        ),
        "{bare:#}"
    );
    let reply = plain_answer(machine, "trace_data_flow");
    assert_plain(&reply);
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.contains(
            "Each step names its parent_step and shows the line of code that links the two."
        ),
        "{meaning}"
    );
    let step = &reply["chain"][0];
    assert_eq!(
        step["sites"],
        json!([{"code": "handler = self._find_error_handler(e, ctx.request.blueprints)"}])
    );
    assert!(step.get("lines").is_none(), "{step:#}");
}

/// A walk that stopped at its depth was not cut to fit, and says which limit
/// stopped it and how to lift it.
#[test]
fn a_trace_stopped_by_its_depth_says_so_rather_than_cut_to_fit() {
    let mut machine = machine(TRACE);
    machine["_kin"]["verdict"] = json!({"state": "inconclusive", "safe_to_conclude_absent": false,
        "limiting_factor": "answer_truncated: stopped early; value_calls_unmeasured; \
            unkeyed_lookups_counted; trace_walk_truncated: hit a cap"});
    machine["_kin"]["response"] = json!({"bounded": false});
    machine.insert("depth".into(), json!(3));
    machine.insert("terminal_bound_steps".into(), json!(1));
    let reply = plain_answer(machine, "trace_data_flow");
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], PARTIAL);
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.ends_with(
            "There may be more, because the walk stopped at depth 3, where 1 step leads further; \
             ask again with a larger depth to follow it."
        ),
        "{meaning}"
    );
    assert!(!meaning.contains("size limit"), "{meaning}");
    // The limit is said in words with the way past it, so no bare flag
    // restates it.
    assert!(reply.get("truncated").is_none(), "{reply:#}");
}

/// A container served as its outline says it is an outline, and how to read
/// one member, never that it is the exact source.
#[test]
fn an_outline_says_it_is_an_outline() {
    let reply = plain_answer(
        machine(
            &json!({
                "_kin": {"verdict": {"state": "certified", "safe_to_conclude_absent": false}},
                "id": "6f957f2b-6976-4c2f-966e-6f2bb4845001", "name": "App", "kind": "Class",
                "header": "class App(Scaffold):", "header_complete": true,
                "members": [{"id": "a", "name": "App.__init__", "signature": "def __init__(self)"},
                    {"id": "b", "name": "App.run", "signature": "def run(self)"}],
                "body_bytes": 37566, "body_omitted": "container_shape",
                "note": crate::container_shape::SHAPE_NOTE,
            })
            .to_string(),
        ),
        "get_entity_source",
    );
    assert_plain(&reply);
    assert_eq!(
        reply["_kin"]["meaning"],
        "Answer: this is the outline of App: its header and its 2 members, each with its entity_id \
         and signature. Read the one you need with source and its entity_id."
    );
}

fn literal_page(offset: u64, shown: u64, cursor: Option<&str>) -> Map<String, Value> {
    let hits: Vec<Value> = (0..shown)
        .map(|index| {
            json!({"entity_id": format!("id-{}", offset + index), "name": "Flask.wsgi_app",
                "kind": "method", "file": "src/flask/app.py", "line": 1618,
                "matched_field": "body_preview", "line_confidence": "exact", "score": null,
                "body_elided": ["excerpt"], "start_line": 1569, "end_line": 1619})
        })
        .collect();
    let mut page = json!({
        "_kin": {"verdict": {"state": "inconclusive",
            "limiting_factor": "response_bounded; lexical_lookup_not_structural"}},
        "literal": ".pop(", "kind": null, "hits": hits, "total_matching": 45,
        "page_offset": offset, "truncated": cursor.is_some(), "order": "entity_id",
        "matching_snapshot": "0123456789abcdef",
        "disclosure": {"caution": "These are lexical occurrences"},
    });
    if let Some(cursor) = cursor {
        page["next_cursor"] = json!(cursor);
    }
    machine(&page.to_string())
}

/// A text search that pages is not partial knowledge. It is complete over
/// the text it reads, says how many matches there are, and says in
/// `delivered` which ones this page lists and the exact call for the rest.
#[test]
fn a_paged_text_search_is_complete_and_says_how_to_get_the_rest() {
    let reply = plain_answer(literal_page(0, 14, Some("abc")), "lexical_lookup");
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE);
    assert_eq!(
        reply["_kin"]["meaning"],
        "Answer: 45 entities matched \".pop(\" in associated lookup fields. A contextual match may not be inside that entity. A text match is not a call or a reference; refs answers who calls something."
    );
    assert_eq!(
        reply["_kin"]["delivered"],
        "Showing 1 to 14 of the 45. For the rest, call locate again with only cursor set to \
         next_cursor."
    );
    assert!(reply["_kin"]["searched"]
        .as_str()
        .unwrap()
        .contains("sampled past 8000 characters"));
    assert!(!reply["_kin"]["meaning"]
        .as_str()
        .unwrap()
        .contains("could not check every place"));
    assert_eq!(reply["next_cursor"], "abc");
    assert_eq!(reply["total"], 45);
    assert_eq!(
        reply["hits"][0],
        json!({"entity_id": "id-0", "name": "Flask.wsgi_app", "kind": "method",
        })
    );
    for key in [
        "disclosure",
        "matching_snapshot",
        "page_offset",
        "truncated",
        "order",
    ] {
        assert!(reply.get(key).is_none(), "{key} reached an agent");
    }
    assert_eq!(reply["literal"], ".pop(");

    let last = plain_answer(literal_page(43, 2, None), "lexical_lookup");
    assert_eq!(last["_kin"]["verdict"], COMPLETE);
    assert_eq!(
        last["_kin"]["delivered"],
        "Showing 44 to 45 of the 45, the last of them."
    );

    let mut none = literal_page(0, 0, None);
    none["total_matching"] = json!(0);
    let none = plain_answer(none, "lexical_lookup");
    // A miss says what it read, never that the text is nowhere.
    let meaning = none["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.starts_with("Answer: no searched lookup field associated with an entity Kin holds contains \".pop(\"."),
        "{meaning}"
    );
    assert!(
        meaning.contains("does not show it is absent from the repository"),
        "{meaning}"
    );
    assert!(none["_kin"].get("delivered").is_none());
}

/// A class Kin cannot show, declared in a block at the top level of its file,
/// is said plainly rather than listed as something to look up; one in a
/// function body keeps the id of the body around it.
#[test]
fn a_possible_override_kin_cannot_show_is_said_plainly() {
    let mut machine = machine(CALLERS);
    machine["overridden_by"]["not_admitted"] = json!([
        {"name": "AppContextProxy", "enclosing": "globals",
            "enclosing_id": "8d3e2f0a-0000-4000-8000-000000000001",
            "placement": "file_level_block", "line": 41,
            "projection": {"path": "src/flask/globals.py"}},
        {"name": "test_ctx.<locals>.Ctx", "enclosing": "test_ctx",
            "enclosing_id": "8d3e2f0a-0000-4000-8000-000000000002",
            "placement": "function_body", "line": 12,
            "projection": {"path": "tests/test_ctx.py"}},
    ]);
    let reply = plain_answer(machine, "find_references");
    assert_plain(&reply);
    let overrides = &reply["overridden_by"];
    assert_eq!(
        overrides["unadmitted_declarations"],
        json!([{"class": "AppContextProxy", "inspection": "unavailable_semantic_container"}])
    );
    assert!(overrides["unadmitted_declarations_note"]
        .as_str()
        .unwrap()
        .contains("cannot show them"));
    assert_eq!(overrides["possible"].as_array().unwrap().len(), 1);
    assert_eq!(overrides["possible"][0]["class"], "test_ctx.<locals>.Ctx");
    assert_eq!(
        overrides["possible"][0]["inside_id"],
        "8d3e2f0a-0000-4000-8000-000000000002"
    );
}

/// Kin's own prose names each tool the way an agent profile serves it: a
/// note, a fix and an error in words all say `source` or `locate`, never a
/// retired tool name, while quoted code keeps the repository's own words.
#[test]
fn replies_name_the_tools_the_profile_serves() {
    let mut result = json!({"content": [{"type": "text", "text": json!({
        "_kin": {"verdict": {"state": "certified", "safe_to_conclude_absent": false}},
        "id": "a", "name": "App", "kind": "Class",
        "header": "def semantic_search(query): pass",
        "notes": ["Read one member with get_entity_source and its id."],
        "hint": "find it with semantic_search or semantic_locate first",
    }).to_string()}, {"type": "text", "text":
        "Entity not found. Find it with semantic_search, then read it with get_entity_source."}]});
    present(&mut result, "get_entity_source");
    let reply: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(reply["notes"][0], "Read one member with source and its id.");
    assert_eq!(reply["hint"], "find it with locate first");
    assert_eq!(reply["header"], "def semantic_search(query): pass");
    assert_eq!(
        result["content"][1]["text"],
        "Entity not found. Find it with locate (mode name), then read it with source."
    );
}

/// A listing cut to fit is a whole answer delivered in part: it says what
/// was left out and how to get it, and a listing whose coverage was not
/// measured says so as scope, never as partial.
#[test]
fn a_neighborhood_cut_to_fit_says_what_was_left_out() {
    let reply = plain_answer(
        machine(
            &json!({
                "_kin": {"verdict": {"state": "inconclusive", "safe_to_conclude_absent": false,
                    "limiting_factor": "response_bounded; answer_coverage_unmeasured: none; \
                        answer_truncated: cut; retrieval_degraded: cut"},
                    "response": {"bounded": true}},
                "entities": [{"entity_id": "a", "name": "AppContext.push"}],
                "entities_withheld": 1, "relations_withheld": 2, "truncated": true,
            })
            .to_string(),
        ),
        "graph_neighborhood",
    );
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE);
    assert_eq!(
        reply["_kin"]["delivered"],
        "1 entity and 2 relationships did not fit in this reply; ask again with a larger \
         max_output_bytes, up to 60000, to see them."
    );
    let schema = crate::requests::served_schema(
        crate::verbs::NEIGHBORS,
        crate::request_schema::Profile::Agent,
        &|_| None,
    )
    .unwrap();
    assert!(schema["properties"].get("max_output_bytes").is_some());
    assert!(schema["properties"].get("max_chars").is_none());
    assert!(schema["properties"].get("max_response_chars").is_none());
    assert!(reply["_kin"]["searched"]
        .as_str()
        .unwrap()
        .contains("was not measured"));
    assert!(reply.get("truncated").is_none(), "{reply:#}");
}

/// A context asked for by entity names each entity it is around.
#[test]
fn a_context_asked_for_by_entities_names_them() {
    let reply = plain_answer(
        machine(
            &json!({
                "_kin": {"verdict": {"state": "certified", "safe_to_conclude_absent": false}},
                "focals": [{"entity_id": "a", "name": "AppContext.pop"},
                    {"entity_id": "b", "name": "AppContext.push"}],
                "entities": [{"entity_id": "c", "name": "AppContext.__exit__"}],
            })
            .to_string(),
        ),
        "get_context_pack",
    );
    assert_eq!(
        reply["_kin"]["meaning"],
        "Answer: this is the context around AppContext.pop and AppContext.push: the code asked about first, then related context."
    );
}

/// Calls around a context Kin could not match leave some dependencies
/// unlisted, which the answer says with the way to find one; they do not make
/// the context a partial answer about what surrounds the entity.
#[test]
fn a_context_says_which_calls_it_could_not_list() {
    let mut machine = machine(CONTEXT);
    machine["_kin"]["verdict"] = json!({"state": "inconclusive", "safe_to_conclude_absent": false,
        "limiting_factor": "method_call_resolution_incomplete: 3 calls; binding_unproven: 1"});
    machine["_kin"]["response"] = json!({"bounded": false});
    let reply = plain_answer(machine, "get_context_pack");
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE);
    assert!(reply["_kin"]["meaning"].as_str().unwrap().ends_with(
        "Calls in this code that Kin could not match to a definition are not listed; locate by \
         the called name finds one."
    ));
    // A reference answer about the same code is still partial for them.
    let mut refs = super::tests::machine(CALLERS);
    refs["_kin"]["verdict"] = json!({"state": "inconclusive", "safe_to_conclude_absent": false,
        "limiting_factor": "method_call_resolution_incomplete: 3 calls"});
    assert_eq!(
        plain_answer(refs, "find_references")["_kin"]["verdict"],
        PARTIAL
    );
}

/// A later page of a reference answer carries rows without its focal. Its rows
/// have the one shape every reference row has, and it says what it searched.
#[test]
fn a_later_reference_page_has_plain_rows() {
    let mut page = machine(CALLERS);
    page.remove("focal_entity");
    page["references"][1]["resolution"] = json!("name_only");
    page.insert("next_cursor".into(), json!("next"));
    let reply = plain_answer(page, "find_references");
    assert_plain(&reply);
    let rows = reply["references"].as_array().unwrap();
    let named = reply[NAME_ONLY_KEY].as_array().unwrap();
    assert_eq!((rows.len(), named.len()), (5, 1));
    for row in rows.iter().chain(named) {
        for key in ["projection", "relation_kinds", "resolution", "id"] {
            assert!(row.get(key).is_none(), "{key} in {row}");
        }
        assert!(row["entity_id"].is_string(), "{row}");
        assert!(row["sites"][0].get("code").is_some(), "{row}");
    }
    assert_eq!(named[0]["proof"], "matched by name only");
    assert!(reply["_kin"]["meaning"]
        .as_str()
        .unwrap()
        .starts_with(&format!(
            "Answer: {EVERY_KIND_SCOPE}this page holds 6 of the 6 rows of this reference answer."
        )));
    assert!(reply["_kin"]["searched"].is_string(), "{reply:#}");
}

/// An answer tool always says what it searched, plainly when the reply did
/// not count its files.
#[test]
fn an_answer_without_a_measured_scope_says_so() {
    let mut machine = machine(TRACE);
    machine["_kin"] = json!({"verdict": {"state": "certified", "safe_to_conclude_absent": false}});
    let reply = plain_answer(machine, "trace_data_flow");
    assert_eq!(
        reply["_kin"]["searched"],
        "Searched recorded entity and relationship evidence; coverage is limited by the verdict."
    );
}

/// The first page of a paged reference answer can hold rows without its focal.
/// Being a page is delivery, not doubt: it says which rows of how many it
/// holds, and how to get the rest.
#[test]
fn a_reference_page_is_delivery_with_its_count() {
    let mut page = machine(CALLERS);
    page.remove("focal_entity");
    page.insert("next_cursor".into(), json!("next"));
    page["_kin"] = json!({
        "verdict": {"state": "inconclusive", "safe_to_conclude_absent": false,
            "limiting_factor": "reference_page_partial: this page is not the complete answer"},
        "page": {"has_more": true, "record_offset": 0, "returned_references": 6,
            "total_references": 27},
    });
    let reply = plain_answer(page, "find_references");
    assert_plain(&reply);
    // Its listed leftovers still stand; being a page does not.
    assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT);
    assert!(reply["_kin"]["meaning"]
        .as_str()
        .unwrap()
        .starts_with(&format!(
            "Answer: {EVERY_KIND_SCOPE}this page holds 6 of the 27 rows of this reference answer."
        )));
    assert_eq!(
        reply["_kin"]["delivered"],
        "More rows follow: call again with cursor set to next_cursor."
    );
}

#[test]
fn recorded_rows_complete_never_closes_uncertain_callers_or_requests_more_answer_rows() {
    for known in [0, 7] {
        let page = json!({
            "references": (0..known).map(|index| json!({
                "entity_id": format!("caller-{index}"), "name": format!("caller_{index}"),
                "resolution": "type_resolved",
            })).collect::<Vec<_>>(),
            "call_site_candidates": {"named": [{"caller": "unresolved", "caller_name": "possible",
                "line_in_entity": null, "callee": null, "state": "could_not_match"}]},
            "next_cursor": "proof", "answer_next_cursor": null, "evidence_next_cursor": "proof",
            "_kin": {
                "verdict": {"state": "inconclusive", "safe_to_conclude_absent": false,
                    "limiting_factor": "call_extraction_incomplete: unresolved sites may add callers"},
                "page": {"kind": "references", "has_more": true, "answer_rows_complete": true,
                    "answer_rows_has_more": false, "evidence_has_more": true,
                    "total_references": known, "returned_references": known},
            },
        });
        let reply = plain_answer(page.as_object().unwrap().clone(), "find_references");
        assert_plain(&reply);
        assert_eq!(reply["_kin"]["verdict"], PARTIAL);
        // Plain replies name the transport fact as delivery of recorded rows.
        assert_eq!(reply["_kin"]["page"]["recorded_rows_exhausted"], true);
        assert!(reply["_kin"]["page"].get("answer_rows_complete").is_none());
        assert_eq!(reply["references"].as_array().unwrap().len(), known);
        assert_eq!(
            reply[LEFT_TO_CHECK_KEY][0]["sites"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(reply["evidence_next_cursor"], "proof");
        assert!(reply.get("answer_next_cursor").is_none());
        let delivered = reply["_kin"]["delivered"].as_str().unwrap();
        assert!(
            delivered.contains("Every recorded reference row is shown"),
            "{delivered}"
        );
        assert!(delivered.contains("never a reference row"), "{delivered}");
        assert!(!delivered.contains("may still add callers"), "{delivered}");
        assert!(!delivered.contains("More rows follow"), "{delivered}");
        if known == 0 {
            let meaning = reply["_kin"]["meaning"].as_str().unwrap();
            assert!(
                meaning.contains("Unresolved sites could still add callers"),
                "{meaning}"
            );
            assert!(!meaning.contains("nothing calls"), "{meaning}");
        }
        let mut final_page = page;
        final_page["next_cursor"] = Value::Null;
        final_page["evidence_next_cursor"] = Value::Null;
        final_page["_kin"]["page"]["has_more"] = json!(false);
        final_page["_kin"]["page"]["evidence_has_more"] = json!(false);
        final_page["_kin"]["response"]["bounded"] = json!(true);
        let final_reply = plain_answer(final_page.as_object().unwrap().clone(), "find_references");
        assert_eq!(final_reply["_kin"]["verdict"], PARTIAL);
        let delivered = final_reply["_kin"]["delivered"].as_str().unwrap();
        assert!(
            delivered.contains(
                "No more recorded reference rows or retained evidence pages remain; the verdict \
                 still limits what can be concluded."
            ),
            "{delivered}"
        );
        assert!(!delivered.contains("send ") && !delivered.contains("call again"));
    }
}

#[test]
fn a_reference_page_with_more_recorded_rows_names_the_answer_cursor() {
    let mut page = machine(CALLERS);
    page.remove("focal_entity");
    page.insert("next_cursor".into(), json!("all"));
    page.insert("answer_next_cursor".into(), json!("answer"));
    page.insert("evidence_next_cursor".into(), json!("proof"));
    page["_kin"]["page"] = json!({
        "kind": "references", "has_more": true,
        "answer_rows_complete": false, "answer_rows_has_more": true, "evidence_has_more": true,
        "returned_references": 6, "total_references": 27,
    });
    let reply = plain_answer(page, "find_references");
    assert_plain(&reply);
    assert_eq!(
        reply["_kin"]["delivered"],
        "More recorded reference rows follow: send answer_next_cursor alone as cursor."
    );
    assert_eq!(reply["answer_next_cursor"], "answer");
    assert_eq!(reply["evidence_next_cursor"], "proof");
}

// ---------------------------------------------------------------------------
// Each answer is judged by what its own operation did
// ---------------------------------------------------------------------------

const NAME_MISS: &str = include_str!("testdata/plain_flask_locate_name_miss.json");
const SOURCE_READ: &str = include_str!("testdata/plain_flask_source.json");
const HISTORY: &str = include_str!("testdata/plain_flask_history.json");
const NEIGHBORS: &str = include_str!("testdata/plain_flask_neighbors.json");
const CONTEXT_BY_ID: &str = include_str!("testdata/plain_flask_context_id.json");
const RANKED: &str = include_str!("testdata/plain_flask_locate_desc.json");
const STATUS: &str = include_str!("testdata/plain_flask_status.json");

/// Sentences that belong to a usage analysis alone: who uses an entity, and
/// what could still reach it. No other operation may say them.
const USAGE_WORDS: [&str; 5] = [
    "could not check every place",
    "reached indirectly",
    "refs on one",
    "Run refs",
    "place this could be used",
];

fn meaning_of(reply: &Value) -> &str {
    reply["_kin"]["meaning"].as_str().unwrap_or_default()
}

fn assert_no_usage_words(reply: &Value, tool: &str) {
    let text = reply["_kin"].to_string();
    for words in USAGE_WORDS {
        assert!(!text.contains(words), "{tool} says {words:?}: {text}");
    }
}

/// The name miss the head-to-head saw: `AppContextProxy` is a class declared
/// inside `if t.TYPE_CHECKING:` in `flask/globals.py`, which Kin holds no
/// entity for. The answer is that declaration, listed with where it is, and
/// never a hint to run refs on rows that do not exist.
#[test]
fn a_name_search_miss_lists_the_declaration_kin_holds_no_entity_for() {
    let reply = plain_answer(machine(NAME_MISS), "semantic_search");
    assert_plain(&reply);
    assert_no_usage_words(&reply, "locate");
    assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT);
    let meaning = meaning_of(&reply);
    assert!(
        meaning.starts_with("Answer: no entity Kin holds is named AppContextProxy, but 1 declaration named like it sits where Kin keeps no entity"),
        "{meaning}"
    );
    assert!(meaning.contains("mode literal"), "{meaning}");
    assert_eq!(
        reply["not_held"],
        json!([{"name": "AppContextProxy", "kind": "class", "why": "declared inside a module-level block, such as an if or try block, whose declarations Kin has not admitted as entities"}])
    );
    assert_eq!(reply["results"], json!([]));
    assert!(reply["_kin"]["searched"]
        .as_str()
        .unwrap()
        .contains("Searched recorded declarations and their semantic names."));
}

/// A name nothing declares says so plainly, with the next useful step.
#[test]
fn a_name_search_that_matches_nothing_says_so_and_what_to_do_next() {
    let mut miss = machine(NAME_MISS);
    miss["query"] = json!("NoSuchName");
    let coverage = miss["edge_coverage"]["declarations"]
        .as_object_mut()
        .unwrap();
    coverage.remove("matching_unadmitted");
    coverage.remove("matching_unadmitted_total");
    coverage.insert("state".into(), json!("complete"));
    miss["_kin"]["verdict"] = json!({"state": "certified", "safe_to_conclude_absent": true});
    let reply = plain_answer(miss, "semantic_search");
    assert_plain(&reply);
    assert_no_usage_words(&reply, "locate");
    assert_eq!(reply["_kin"]["verdict"], COMPLETE);
    assert_eq!(
        meaning_of(&reply),
        "Answer: no declaration Kin holds is named NoSuchName. Kin holds this checkout's current code, so a NoSuchName you remember from another version of this project is not in it. To search by what the code does, call locate with mode description; to find the text inside code, call locate with mode literal."
    );
    assert!(reply.get("not_held").is_none());
}

/// A name lookup as the search handler answers one plain name with nothing
/// narrowing it: `rows` held, `not_held` listed by the census, every source
/// unit read whole, and the verdict that follows from those.
fn name_lookup(query: &str, rows: &[&str], not_held: &[&str]) -> Map<String, Value> {
    let mut reply = machine(NAME_MISS);
    reply["query"] = json!(query);
    reply["results"] = rows
        .iter()
        .map(|name| json!({"name": name, "kind": "function"}))
        .collect();
    reply["total_matches"] = json!(rows.len());
    reply["truncated"] = json!(false);
    reply.insert("own_name_rows_complete".into(), json!(true));
    let census = reply["edge_coverage"]["declarations"]
        .as_object_mut()
        .unwrap();
    census.insert("parsed".into(), json!("whole"));
    census.remove("matching_unadmitted");
    census.remove("matching_unadmitted_total");
    census.insert("state".into(), json!("complete"));
    if !not_held.is_empty() {
        census.insert(
            "matching_unadmitted".into(),
            not_held
                .iter()
                .map(|name| {
                    json!({"name": name, "kind": "function", "reason": "nested_in_callable",
                           "owner": "outer"})
                })
                .collect(),
        );
        census.insert("matching_unadmitted_total".into(), json!(not_held.len()));
        census.insert("state".into(), json!("complete_except"));
    }
    reply["_kin"]["verdict"] = if not_held.is_empty() {
        json!({"state": "certified", "safe_to_conclude_absent": false})
    } else {
        json!({"state": "certified_except", "safe_to_conclude_absent": false,
               "limiting_factor": "declarations_unadmitted_listed"})
    };
    reply
}

/// Declarations that read alike once projected, here sixteen nested
/// `validate` callbacks inside `outer` on different lines, are one not_held
/// row with their count, and the sentence still counts sixteen.
#[test]
fn identical_not_held_declarations_are_said_once_with_their_count() {
    let names = ["validate"; 16];
    let reply = plain_answer_with_details(
        name_lookup("validate", &[], &names),
        "semantic_search",
        false,
    );
    assert_plain(&reply);
    // Details lists every declaration as it is.
    let detailed = plain_answer(name_lookup("validate", &[], &names), "semantic_search");
    assert_eq!(detailed["not_held"].as_array().unwrap().len(), 16);
    let rows = reply["not_held"].as_array().expect("not_held rows");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["name"], "validate");
    assert_eq!(rows[0]["inside"], "outer");
    assert_eq!(rows[0]["count"], 16);
    assert!(
        meaning_of(&reply).contains("16 declarations"),
        "{}",
        meaning_of(&reply)
    );
    assert!(
        !meaning_of(&reply).contains("1 of them"),
        "{}",
        meaning_of(&reply)
    );
}

/// The claim that a reply holds every declaration of a name is about the
/// declarations named exactly so. A reply of nothing else leads with it; one
/// that also holds longer names says which of its rows the claim counts; one
/// that holds only longer names says no declaration has the name.
#[test]
fn a_name_search_states_closure_for_the_declarations_named_exactly_so() {
    let reply = plain_answer(
        name_lookup("pop", &["Store.pop", "Queue::pop"], &[]),
        "semantic_search",
    );
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE);
    assert_eq!(
        meaning_of(&reply),
        "Answer: these 2 are every declaration named pop in this repository: Kin read all 1844 declarations in its source, and no other has that name."
    );

    let reply = plain_answer(name_lookup("pop", &["pop"], &[]), "semantic_search");
    assert_eq!(
        meaning_of(&reply),
        "Answer: this one is the only declaration named pop in this repository: Kin read all 1844 declarations in its source, and no other has that name."
    );

    let reply = plain_answer(name_lookup("pop", &[], &["pop"]), "semantic_search");
    assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT);
    assert!(
        meaning_of(&reply).ends_with(
            "The 1 listed under not_held is the only declaration named pop in this repository: Kin read all 1844 declarations in its source, and no other has that name."
        ),
        "{}",
        meaning_of(&reply)
    );

    let reply = plain_answer(
        name_lookup("pop", &["Store.pop", "pop_front"], &["pop", "pop_inner"]),
        "semantic_search",
    );
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT);
    assert_eq!(
        meaning_of(&reply),
        "Answer: these are the 2 declarations Kin holds whose name matches pop. 2 declarations more named like it sit where Kin keeps no entity; not_held lists them. The 2 named exactly pop here are every declaration named pop in this repository: Kin read all 1844 declarations in its source, and no other has that name."
    );

    let reply = plain_answer(name_lookup("pop", &["pop_front"], &[]), "semantic_search");
    assert_eq!(
        meaning_of(&reply),
        "Answer: this is the 1 declaration Kin holds whose name matches pop. None here is named exactly pop, and no declaration in this repository is: Kin read all 1844 declarations in its source."
    );
}

/// Each thing the claim rests on, taken away alone, takes the claim with it,
/// and the reply says where another declaration could be when the answer
/// knows: a search that did not ask for members by their own name, rows the
/// reply did not show, matching declarations the census counted without
/// listing, and source held without being parsed.
#[test]
fn a_name_search_one_condition_short_claims_no_closure() {
    let closed = |reply: &Value| meaning_of(reply).contains("no other has that name");
    let whole = || name_lookup("pop", &["Store.pop"], &["pop"]);
    assert!(closed(&plain_answer(whole(), "semantic_search")));

    let mut unasked = whole();
    unasked.remove("own_name_rows_complete");
    let reply = plain_answer(unasked, "semantic_search");
    assert!(!closed(&reply), "{reply:#}");
    assert_eq!(
        meaning_of(&reply),
        "Answer: this is the 1 declaration Kin holds whose name matches pop. 1 declaration more named like it sits where Kin keeps no entity; not_held lists it."
    );

    let mut cut = whole();
    cut["total_matches"] = json!(4);
    cut["truncated"] = json!(true);
    let reply = plain_answer(cut, "semantic_search");
    assert!(!closed(&reply), "{reply:#}");
    assert!(reply["_kin"]["delivered"]
        .as_str()
        .is_some_and(|delivered| delivered.starts_with("Showing 1 of the 4")));

    let mut unlisted = whole();
    unlisted["edge_coverage"]["declarations"]["matching_unadmitted_total"] = json!(30);
    let reply = plain_answer(unlisted, "semantic_search");
    assert!(!closed(&reply), "{reply:#}");
    assert_eq!(
        meaning_of(&reply),
        "Answer: this is the 1 declaration Kin holds whose name matches pop. 30 declarations more named like it sit where Kin keeps no entity; not_held lists 1 of them. 29 declarations named like it that Kin keeps no entity for are not listed, so another pop can be among them; a narrower name such as module.name lists fewer."
    );

    let mut unparsed = whole();
    unparsed["_kin"]["verdict"]["limiting_factor"] =
        json!("declarations_unadmitted_listed; declarations_unparsed_files_listed");
    let reply = plain_answer(unparsed, "semantic_search");
    assert!(!closed(&reply), "{reply:#}");
    // Source never parsed could declare another: the answer is partial.
    assert_eq!(reply["_kin"]["verdict"], PARTIAL);
    assert!(
        meaning_of(&reply).starts_with("Partial: this is the 1 declaration Kin holds"),
        "{}",
        meaning_of(&reply)
    );
    assert!(
        meaning_of(&reply).ends_with(
            "Part of this repository's source is held as bytes and was never parsed, so another pop could be declared there."
        ),
        "{}",
        meaning_of(&reply)
    );

    let mut older = whole();
    older["edge_coverage"]["declarations"]
        .as_object_mut()
        .unwrap()
        .remove("parsed");
    assert!(!closed(&plain_answer(older, "semantic_search")));
}

/// Source parsed around a syntax error whose unread parts were read back and
/// spell the name nowhere is no place another declaration could hide. A reply
/// that is not closed for another reason names that reason and says nothing
/// about the unread parts; one whose unread parts could not be read says
/// that, and one that recorded nothing about them says they were not read.
#[test]
fn a_name_search_says_unread_parts_are_open_only_when_they_could_hold_the_name() {
    let syntax = "parsed around a syntax error";
    let partly = |unread: Option<&str>| {
        let mut reply = name_lookup("pop", &["Store.pop"], &["pop"]);
        reply["edge_coverage"]["declarations"]["matching_unadmitted_total"] = json!(30);
        let census = reply["edge_coverage"]["declarations"]
            .as_object_mut()
            .unwrap();
        census.insert("parsed".into(), json!("in_part"));
        census.insert("parsed_in_part".into(), json!(2));
        if let Some(unread) = unread {
            census.insert("unread_regions".into(), json!(unread));
        }
        plain_answer(reply, "semantic_search")
    };

    let clear = partly(Some("clear"));
    assert!(
        !meaning_of(&clear).contains(syntax),
        "{}",
        meaning_of(&clear)
    );
    assert!(
        meaning_of(&clear).contains("29 declarations named like it"),
        "the reason the reply is not closed is still named: {}",
        meaning_of(&clear)
    );

    let not_read = partly(Some("not_read"));
    assert!(
        meaning_of(&not_read).contains("Kin could not read back"),
        "{}",
        meaning_of(&not_read)
    );

    let unrecorded = partly(None);
    assert!(
        meaning_of(&unrecorded).contains(syntax),
        "{}",
        meaning_of(&unrecorded)
    );
}

/// A verdict code about uses reaches no answer to another question, whether
/// the machine verdict calls the answer complete except those uses or
/// inconclusive because of them.
#[test]
fn usage_codes_never_qualify_an_answer_to_another_question() {
    let usage = "value_calls_unmeasured; named_leftovers; escape_residue_listed; method_call_resolution_incomplete; binding_unproven; caller_arrival_unresolved; reference_enrichment_unusable";
    let cases: [(&str, &str); 6] = [
        (NAME_MISS, "semantic_search"),
        (SOURCE_READ, "get_entity_source"),
        (HISTORY, "kin_provenance_query"),
        (NEIGHBORS, "graph_neighborhood"),
        (CONTEXT_BY_ID, "get_context_pack"),
        (RANKED, "semantic_locate"),
    ];
    for (fixture, tool) in cases {
        for state in ["certified_except", "inconclusive"] {
            let mut reply = machine(fixture);
            reply["_kin"]["verdict"] = json!({"state": state, "limiting_factor": usage,
                "safe_to_conclude_absent": false});
            if tool == "semantic_search" {
                // Only the name search's own residue may make it an exception.
                reply["edge_coverage"]["declarations"]
                    .as_object_mut()
                    .unwrap()
                    .remove("matching_unadmitted_total");
                reply["edge_coverage"]["declarations"]
                    .as_object_mut()
                    .unwrap()
                    .remove("matching_unadmitted");
            }
            let plain = plain_answer(reply, tool);
            assert_no_usage_words(&plain, tool);
            assert_ne!(
                plain["_kin"]["verdict"], PARTIAL,
                "{tool} {state}: {plain:#}"
            );
            assert_ne!(
                plain["_kin"]["verdict"], COMPLETE_EXCEPT_INDIRECT,
                "{tool} {state}: {plain:#}"
            );
        }
    }
}

/// A history read delivered the requested page of recorded history: its
/// latest change is said once, and where the recorded history begins is
/// disclosed as its scope. It is never told it could not check every use.
#[test]
fn a_history_page_says_what_it_delivered_and_where_the_record_begins() {
    let (reply, bytes) = rendered(HISTORY, "kin_provenance_query");
    assert_plain(&reply);
    assert_no_usage_words(&reply, "history");
    assert!(bytes < HISTORY.len() / 2, "{bytes} of {}", HISTORY.len());
    assert_eq!(reply["_kin"]["verdict"], COMPLETE);
    assert_eq!(
        meaning_of(&reply),
        "Answer: 1 change is recorded for this entity, newest first. No approval is recorded on the latest change."
    );
    assert!(reply.get("latest_change").is_none(), "said once: {reply:#}");
    let change = &reply["changes"][0];
    assert_eq!(
        change["change_id"],
        "13ccfb15830cb8c58c0dd9f9e019fc263d0ad8f280ae231ef039dbc90457d03b"
    );
    assert_eq!(
        change["git_commit"],
        "cd5aab00a2e20fdb77c534044c644aeea009c795"
    );
    assert_eq!(change["first_recorded"], true);
    assert!(change.get("id").is_none());
    let searched = reply["_kin"]["searched"].as_str().unwrap();
    assert!(
        searched.contains("begins at change 13ccfb15830c, which recorded Git commit cd5aab00a2e2"),
        "{searched}"
    );
    for key in [
        "truncated",
        "compact",
        "returned",
        "offset",
        "approvals",
        "recent_audit_events",
    ] {
        assert!(reply.get(key).is_none(), "{key} in {reply:#}");
    }
}

/// A later page keeps the latest change, which is not among its rows, and
/// says how to reach the older ones.
#[test]
fn a_later_history_page_keeps_the_latest_change_it_does_not_hold() {
    let mut page = machine(HISTORY);
    let mut older = page["changes"][0].clone();
    older["id"] = json!("0".repeat(64));
    older["parents"] = json!(["1".repeat(64)]);
    page["changes"] = json!([older]);
    page["change_count"] = json!(30);
    page["offset"] = json!(20);
    page["next_offset"] = json!(21);
    let reply = plain_answer(page, "kin_provenance_query");
    assert_eq!(
        reply["latest_change"]["change_id"],
        "13ccfb15830cb8c58c0dd9f9e019fc263d0ad8f280ae231ef039dbc90457d03b"
    );
    assert!(meaning_of(&reply).starts_with(
        "Answer: 30 changes are recorded for this entity, newest first; this page holds 21 to 21."
    ));
    assert_eq!(
        reply["_kin"]["delivered"],
        "Older changes follow: call history again with offset set to next_offset."
    );
    assert_eq!(reply["next_offset"], 21);
    assert!(!reply["_kin"]["searched"]
        .as_str()
        .unwrap()
        .contains("begins at"));
}

/// An ordinary source read returns the code, who it is, the revision and
/// whether the body is whole, with a compact reference to its write
/// precondition in place of the whole `source_base`.
#[test]
fn a_source_read_serves_its_code_revision_and_a_compact_edit_reference() {
    let machine_reply = machine(SOURCE_READ);
    let base = machine_reply["source_base"].clone();
    let (reply, bytes) = rendered(SOURCE_READ, "get_entity_source");
    assert_plain(&reply);
    assert_no_usage_words(&reply, "source");
    assert_eq!(reply["_kin"]["verdict"], COMPLETE);
    assert_eq!(
        meaning_of(&reply),
        "Answer: this is the exact current source of leak_detector, the whole body."
    );
    assert_eq!(reply["entity_id"], "600c6388-0170-48cc-bde5-2bc591e4e8aa");
    for key in ["file", "line", "start_line", "end_line"] {
        assert!(reply.get(key).is_none(), "{key}");
    }
    assert_eq!(
        reply["revision"],
        json!({"workspace_generation": 3, "tree": "b76abc0deef1"})
    );
    assert_eq!(
        reply["source_base"],
        crate::source_ticket::compact(&base).unwrap()
    );
    // The ticket resolves to exactly the base the machine reply carries.
    let ticket =
        crate::source_ticket::parse(reply["source_base"]["ticket"].as_str().unwrap()).unwrap();
    assert_eq!(
        crate::source_ticket::resolve(&ticket, &json!({"source_base": base})).unwrap(),
        base
    );
    for key in [
        "id",
        "file_path",
        "start_byte",
        "end_byte",
        "span_coherence",
        "language",
    ] {
        assert!(reply.get(key).is_none(), "{key} in {reply:#}");
    }
    let base_bytes = serde_json::to_string(&base).unwrap().len();
    assert!(
        bytes + base_bytes / 2
            < SOURCE_READ
                .len()
                .min(serde_json::to_string(&machine_reply).unwrap().len()),
        "{bytes} bytes"
    );
}

/// A ranking delivered its ranked results: complete as a ranking, and never
/// a claim that every relevant entity was found. What it ranked on is scope.
/// A class of code it left out is said in the answer line itself, beside the
/// verdict, with the call that ranks it: "complete" over a ranking that held
/// no test, with the exclusion only in a trailing line, sent a model that was
/// looking for a test off to run the whole suite.
#[test]
fn a_ranking_is_complete_as_a_ranking_and_claims_nothing_more() {
    let reply = plain_answer(machine(RANKED), "semantic_locate");
    assert_plain(&reply);
    assert_no_usage_words(&reply, "locate");
    assert_eq!(reply["_kin"]["verdict"], COMPLETE);
    let meaning = meaning_of(&reply);
    assert!(
        meaning.starts_with("Answer: these are the 3 entities Kin ranks closest to \"where the application context is pushed and popped\", best first, with test code left out (include_tests true ranks it)."),
        "{meaning}"
    );
    assert!(
        meaning.contains("it does not claim to have found every entity"),
        "{meaning}"
    );
    assert!(
        meaning.contains("None of them is a confident match"),
        "{meaning}"
    );
    let searched = reply["_kin"]["searched"].as_str().unwrap();
    assert!(
        searched.contains("lexical and graph signals; embedding coverage could not be read"),
        "{searched}"
    );
    assert!(!searched.contains("and search vectors"), "{searched}");
    // Said once, in the answer line, and not again here.
    assert!(!searched.contains("include_tests"), "{searched}");

    // A ranking that looked at test code says nothing of the kind.
    let mut with_tests = machine(RANKED);
    with_tests["semantic_coverage"]["limited_by"] = json!(["vector_support_disabled"]);
    let whole = plain_answer(with_tests, "semantic_locate");
    assert!(!meaning_of(&whole).contains("left out"), "{whole}");
    let row = &reply["results"][0];
    assert_eq!(row["entity_id"], "30acb47e-56ec-4536-b310-5d9c40d7188c");
    for key in ["id", "score", "matched"] {
        assert!(row.get(key).is_none(), "{key} in {row}");
    }
    assert_eq!(
        reply["_kin"]["delivered"],
        "More rows follow: call again with cursor set to next_cursor."
    );
}

/// Each literal hit keeps how sure Kin is of where the text is, and a match
/// Kin could not place is given its entity's lines, except file-level previews,
/// which do not locate their text inside that entity at all.
#[test]
fn a_literal_hit_keeps_how_sure_kin_is_of_its_line() {
    let hit = |confidence: &str, line: Value, field: &str| {
        json!({"entity_id": "e", "name": "SessionInterface", "kind": "class",
            "file": "src/flask/sessions.py", "start_line": 140, "end_line": 250,
            "line": line, "line_confidence": confidence, "matched_field": field,
            "excerpt": "SESSION_COOKIE_NAME"})
    };
    let mut page = literal_page(0, 0, None);
    page["hits"] = json!([
        hit("exact", json!(172), "body_preview"),
        hit("declaration", json!(140), "doc_summary"),
        hit("entity_span_only", Value::Null, "file_import_context"),
    ]);
    page["total_matching"] = json!(3);
    let reply = plain_answer(page, "lexical_lookup");
    assert_plain(&reply);
    let hits = reply["hits"].as_array().unwrap();
    assert!(hits[0].get("line_in_entity").is_none());
    assert!(hits[0].get("at").is_none());
    assert_eq!(
        (hits[1].get("line_in_entity"), hits[1]["at"].clone()),
        (None, json!("its declaration"))
    );
    assert_eq!(hits[1]["matched_in"], "doc summary");
    assert!(
        hits[2].get("line").is_none(),
        "an entity span is never a line: {}",
        hits[2]
    );
    assert!(hits[2].get("within_lines").is_none());
    assert_eq!(hits[2]["at"], "recorded context; lexical scope unproven");
    assert!(hits[2].get("about_code").is_none());
    assert_eq!(hits[2]["matched_in"], "recorded import context");
    assert!(reply["_kin"]["searched"]
        .as_str()
        .unwrap()
        .starts_with(LITERAL_SCOPE));
    assert!(reply["_kin"]["searched"]
        .as_str()
        .unwrap()
        .contains("not exact import syntax"));
    for hit in hits {
        assert_eq!(hit["code"], "SESSION_COOKIE_NAME");
    }
}

/// A neighbourhood is an exploration of the hops asked for. Cut to fit, it
/// is complete and says what was left out; its links are records a reader
/// can follow, each naming both ends.
#[test]
fn a_neighbourhood_explores_its_hops_with_readable_links() {
    let (reply, bytes) = rendered(NEIGHBORS, "graph_neighborhood");
    assert_plain(&reply);
    assert_no_usage_words(&reply, "neighbors");
    assert!(bytes < NEIGHBORS.len());
    assert_eq!(reply["_kin"]["verdict"], COMPLETE, "{reply:#}");
    let meaning = meaning_of(&reply);
    assert!(
        meaning.starts_with("Answer: these are 6 entities within 2 hops of AppContext.pop"),
        "{meaning}"
    );
    let delivered = reply["_kin"]["delivered"].as_str().unwrap();
    assert!(delivered.contains("did not fit"));
    assert!(delivered.contains("max_output_bytes"));
    assert!(!delivered.contains("max_chars"));
    assert!(!delivered.contains("max_response_chars"));
    assert!(reply["_kin"]["searched"]
        .as_str()
        .unwrap()
        .contains("not a list of every use"));
    let link = &reply["links"][0];
    assert_eq!(link["from"], "AppContext.pop");
    assert_eq!(link["how"], "references");
    assert!(
        link["to"].is_string() && link["to_id"].is_string(),
        "{link}"
    );
    assert!(reply.get("relations").is_none());
    for row in reply["entities"].as_array().unwrap() {
        assert!(row.get("body_elided").is_none(), "{row}");
        assert!(row["entity_id"].is_string(), "{row}");
    }
}

/// A context delivered its focal evidence first. Neighbours it left out to
/// fit, and calls around it Kin could not match, do not make it sound
/// unfinished; a focal body that did not fit does.
#[test]
fn a_context_is_complete_with_its_focal_evidence_and_partial_without_it() {
    let reply = plain_answer(machine(CONTEXT_BY_ID), "get_context_pack");
    assert_plain(&reply);
    assert_no_usage_words(&reply, "context");
    assert_eq!(reply["_kin"]["verdict"], COMPLETE, "{reply:#}");
    assert!(meaning_of(&reply).starts_with(
        "Answer: this is the context around AppContext.pop: its code first, then related context."
    ));
    assert!(reply["entity"]["body"].is_string());
    // A neighbour is a signature by default, and each row says which view of
    // it was shown rather than leaving a missing body to be inferred.
    let rows = reply["dependencies"].as_array().unwrap();
    assert!(!rows.is_empty(), "{reply:#}");
    for row in rows {
        assert!(row.get("body").is_none(), "{row}");
        let signed = row["signature"]
            .as_str()
            .is_some_and(|text| !text.is_empty());
        assert_eq!(
            row["shown"],
            if signed {
                "signature only"
            } else {
                "identity only"
            },
            "a neighbour is a signature by default: {row}"
        );
    }

    let mut lost = machine(CONTEXT_BY_ID);
    lost["focal_entity"]["body"] = Value::Null;
    lost["focal_entity"]["body_elided"] = json!(["body"]);
    let reply = plain_answer(lost, "get_context_pack");
    assert_eq!(reply["_kin"]["verdict"], PARTIAL);
    assert!(
        meaning_of(&reply).contains("the body of AppContext.pop did not fit in this reply"),
        "{}",
        meaning_of(&reply)
    );
}

/// Status says how fresh its graph is and what the daemon paused, in words,
/// with no verdict and no continuation it cannot serve.
#[test]
fn status_says_how_fresh_the_graph_is_and_what_was_paused() {
    let reply = plain_answer(machine(STATUS), STATUS_TOOL);
    assert_plain(&reply);
    assert!(reply["_kin"].get("verdict").is_none());
    let meaning = meaning_of(&reply);
    assert!(
        meaning.contains("Kin has not recorded ingestion since it started"),
        "{meaning}"
    );
    assert!(meaning.contains("short of memory"), "{meaning}");
    assert!(!meaning.contains("some checks switched off"), "{meaning}");
    assert!(reply.get("next_cursor").is_none());
}

/// Every plain reply names an entity by `entity_id`, never `id`, wherever the
/// entity sits in it.
#[test]
fn every_plain_reply_names_entities_by_entity_id() {
    fn keys(value: &Value, path: &str, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    if key == "id" {
                        out.push(format!("{path}.{key}"));
                    }
                    keys(child, &format!("{path}.{key}"), out);
                }
            }
            Value::Array(rows) => rows.iter().for_each(|row| keys(row, path, out)),
            _ => {}
        }
    }
    for (fixture, tool) in [
        (CALLERS, "find_references"),
        (TRACE, "trace_data_flow"),
        (CONTEXT, "get_context_pack"),
        (SOURCE, "get_entity_source"),
        (NAME_MISS, "semantic_search"),
        (SOURCE_READ, "get_entity_source"),
        (HISTORY, "kin_provenance_query"),
        (NEIGHBORS, "graph_neighborhood"),
        (CONTEXT_BY_ID, "get_context_pack"),
        (RANKED, "semantic_locate"),
    ] {
        let (reply, _) = rendered(fixture, tool);
        let mut found = Vec::new();
        keys(&reply, tool, &mut found);
        assert!(found.is_empty(), "{tool}: {found:?}");
    }
}

/// A ranking's verdict qualifies only its ranked results: what it could rank
/// on is scope, and a reason the build carries no code for does not make the
/// ranked results partial.
#[test]
fn a_ranking_stays_complete_whatever_it_could_rank_on() {
    let mut ranked = machine(RANKED);
    ranked["_kin"]["verdict"] = json!({"state": "inconclusive", "safe_to_conclude_absent": false,
        "limiting_factor": "relevance_floor_unmeasured; coverage_role_filter_withheld; unlisted_clause; retrieval_degraded"});
    let reply = plain_answer(ranked, "semantic_locate");
    assert_eq!(reply["_kin"]["verdict"], COMPLETE, "{reply:#}");

    let mut stale = machine(RANKED);
    stale["_kin"]["verdict"] = json!({"state": "inconclusive", "safe_to_conclude_absent": false,
        "limiting_factor": "tracked_changes_pending"});
    let reply = plain_answer(stale, "semantic_locate");
    assert_eq!(reply["_kin"]["verdict"], PARTIAL, "{reply:#}");
    assert!(meaning_of(&reply).contains("the code has changed since Kin last read it"));
}

/// A language server that failed to start is a reason a usage analysis may be
/// missing uses, and scope for a listing of links.
#[test]
fn a_language_server_that_failed_to_start_is_said_where_it_matters() {
    let mut context = machine(CONTEXT_BY_ID);
    context["_kin"]["verdict"] = json!({"state": "inconclusive", "safe_to_conclude_absent": false,
        "limiting_factor": "response_bounded; reference_enrichment_unusable; method_call_resolution_incomplete; retrieval_degraded"});
    context["_kin"]["response"] = json!({"bounded": true});
    let reply = plain_answer(context, "get_context_pack");
    assert_eq!(reply["_kin"]["verdict"], COMPLETE, "{reply:#}");
    assert!(reply["_kin"]["searched"]
        .as_str()
        .unwrap()
        .contains("failed to start"));

    let mut refs = machine(CALLERS);
    refs["_kin"]["verdict"] = json!({"state": "inconclusive", "safe_to_conclude_absent": false,
        "limiting_factor": "reference_enrichment_unusable; value_calls_unmeasured"});
    let reply = plain_answer(refs, "find_references");
    assert_eq!(reply["_kin"]["verdict"], PARTIAL);
    let meaning = meaning_of(&reply);
    assert!(
        meaning.contains("the language server for this code failed to start"),
        "{meaning}"
    );
    assert!(
        !meaning.contains("could not check every place"),
        "{meaning}"
    );
}

/// When the verdict's clause carries why the server could not start, the
/// plain reply says why instead of only that it failed to start.
#[test]
fn a_language_server_that_failed_to_start_is_said_with_its_reason() {
    let clause = "reference_enrichment_unusable: TypeScript enrichment unavailable: \
                  `typescript-language-server` did not answer initialize: it exited with code \
                  3. Its last stderr: Error: Cannot find module 'typescript', so \
                  reference enrichment for TypeScript is unavailable on this host";
    let mut refs = machine(CALLERS);
    refs["_kin"]["verdict"] = json!({"state": "inconclusive", "safe_to_conclude_absent": false,
        "limiting_factor": format!("{clause}; value_calls_unmeasured")});
    let reply = plain_answer(refs, "find_references");
    let meaning = meaning_of(&reply);
    assert!(
        meaning.contains("did not answer initialize: it exited with code 3")
            && meaning.contains("Cannot find module 'typescript'"),
        "{meaning}"
    );
    assert!(!meaning.contains("failed to start"), "{meaning}");

    let mut context = machine(CONTEXT_BY_ID);
    context["_kin"]["verdict"] = json!({"state": "inconclusive", "safe_to_conclude_absent": false,
    "limiting_factor": format!(
        "response_bounded; {clause}; method_call_resolution_incomplete; retrieval_degraded"
    )});
    context["_kin"]["response"] = json!({"bounded": true});
    let reply = plain_answer(context, "get_context_pack");
    let searched = reply["_kin"]["searched"].as_str().unwrap_or_default();
    assert!(
        searched.contains("TypeScript enrichment unavailable: `typescript-language-server`"),
        "{reply:#}"
    );
}

/// A page the response budget compacted sheds each hit's confidence label and
/// span. A hit that kept a line still says how sure it is, from its matched
/// field, and one without a line is never given one.
#[test]
fn a_compacted_literal_hit_still_says_how_sure_its_line_is() {
    let mut page = literal_page(0, 0, None);
    page["hits"] = json!([
        {"entity_id": "a", "name": "SessionInterface", "kind": "class", "file": "src/flask/sessions.py",
         "line": 172, "matched_field": "body_preview", "excerpt": "x"},
        {"entity_id": "b", "name": "SessionInterface.get_cookie_name", "kind": "method",
         "file": "src/flask/sessions.py", "line": 171, "matched_field": "doc_summary", "excerpt": "x"},
        {"entity_id": "c", "name": "Other", "kind": "class", "file": "src/flask/other.py",
         "line": null, "matched_field": "file_import_context", "excerpt": "x"},
    ]);
    page["total_matching"] = json!(3);
    let reply = plain_answer(page, "lexical_lookup");
    let hits = reply["hits"].as_array().unwrap();
    assert_eq!(
        (hits[0].get("line_in_entity"), hits[0].get("at")),
        (None, None)
    );
    assert_eq!(
        (hits[1].get("line_in_entity"), hits[1]["at"].clone()),
        (None, json!("its declaration"))
    );
    assert!(hits[2].get("line").is_none(), "{}", hits[2]);
    assert_eq!(hits[2]["at"], "recorded context; lexical scope unproven");
    assert!(hits[2].get("within_lines").is_none());
}

#[test]
fn model_slots_observed_identity_survives_plain_presentation() {
    let mut reply = machine(CALLERS);
    let observed = json!([{ "slot": "reranking", "provider": "external",
        "model": "get_context_pack-model", "candidates": 6, "returned": 6,
        "filtered_out": 0 }]);
    reply[crate::envelope::ENVELOPE_KEY]["model_slots"] = observed.clone();
    let (plain, _) = rendered(&serde_json::to_string(&reply).unwrap(), "find_references");
    assert_eq!(
        plain[crate::envelope::ENVELOPE_KEY]["model_slots"],
        observed
    );
}

const REACH_PAGE: &str = include_str!("testdata/plain_react_reach_trace_page.json");
const DEADCODE_BY_ID: &str = include_str!("testdata/plain_flask_deadcode_refs_by_id.json");
const DEADCODE_BY_NAME: &str = include_str!("testdata/plain_flask_deadcode_refs_by_name.json");

/// The page of a callers walk whose chain rides on a later page counts its
/// answer from the reach reading: four proven callers, never "0 steps in all".
/// Captured from React's `advanceTimers` with the 0.8.2 build, whose page one
/// held the reach members and frontier and no chain step.
#[test]
fn a_trace_page_counts_its_answer_from_the_reach_members_not_the_chain() {
    let machine = machine(REACH_PAGE);
    assert_eq!(machine["chain"], json!([]));
    let envelope = machine["_kin"].clone();
    let reply = plain_answer(machine, "trace_data_flow");
    // The reach rows carry the walk's policy words as they always have; the
    // sentences this lead writes carry none.
    assert_plain(&reply["_kin"]);
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.contains("4 functions can end up calling advanceTimers"),
        "{meaning}"
    );
    assert!(
        meaning.contains("reach.members lists every one"),
        "{meaning}"
    );
    assert!(!meaning.contains("0 steps"), "{meaning}");
    assert!(
        !meaning.contains("what calls the entity and what it calls"),
        "a page that does not repeat its direction is still a callers walk: {meaning}"
    );
    assert_eq!(envelope["page"]["total_steps"], 14);
}

/// The lead of a walk with a reach reading and its chain on the same page
/// counts the members, which are the answer, and gives the chain's length as
/// the walk's.
#[test]
fn a_trace_with_reach_members_leads_with_the_members() {
    let mut machine = machine(TRACE);
    machine.insert(
        "reach".into(),
        json!({"members_total": 6, "members": [], "question": "which entities can end up calling the focal"}),
    );
    let lead = trace_lead(&machine, &Value::Null);
    assert_eq!(
        lead,
        "6 functions can end up calling App._find_error_handler within 8 calls. reach.members \
         lists every one, each with the line where it calls the next step toward \
         App._find_error_handler (via); the chain shows the same walk in 7 steps, each naming its \
         parent_step"
    );
}

/// "Is X unused?" asked of a store whose census measured calls and values but
/// not other mentions says what it checked and names what it did not, instead
/// of "could not check every place".
#[test]
fn an_absence_with_only_non_call_mentions_unmeasured_names_them() {
    let reply = plain_answer(machine(DEADCODE_BY_ID), "find_references");
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], PARTIAL);
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        !meaning.contains("could not check every place"),
        "{meaning}"
    );
    assert!(
        meaning.contains(
            "Kin checked the code it holds for every call that spells _make_timedelta, and every \
             place that passes or stores it as a value, and found none."
        ),
        "{meaning}"
    );
    assert!(
        meaning.contains("Left unchecked: Kin did not check the places that read the name"),
        "{meaning}"
    );
    assert!(!meaning.contains("does not record import"), "{meaning}");
    assert!(
        meaning.contains("Only such a place could use it."),
        "{meaning}"
    );
    assert!(reply["_kin"]["searched"]
        .as_str()
        .unwrap()
        .starts_with("Checked the admitted semantic inventory"));
}

/// Each answer of a same-name reference answer says how many files were
/// searched, from the reply around it, as its meaning does.
#[test]
fn every_section_of_a_same_name_answer_qualifies_its_semantic_inventory() {
    let reply = plain_answer(machine(DEADCODE_BY_NAME), "find_references");
    assert_plain(&reply);
    let answers = reply["answers"].as_array().unwrap();
    assert_eq!(answers.len(), 2);
    for answer in answers {
        let searched = answer["_kin"]["searched"].as_str().unwrap();
        assert!(
            searched.starts_with("Checked the admitted semantic inventory"),
            "{searched}"
        );
        assert!(!searched.contains("did not count"), "{searched}");
    }
}

/// A certified absence asked about every use says it is safe to conclude the
/// entity is unused; one asked about calls alone says only that.
#[test]
fn a_certified_absence_says_it_is_safe_to_conclude_unused() {
    let mut every_use = machine(DEADCODE_BY_ID);
    every_use["_kin"]["verdict"] = json!({"state": "certified", "safe_to_conclude_absent": true});
    let reply = plain_answer(every_use.clone(), "find_references");
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE);
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.starts_with(&format!("Answer: {EVERY_KIND_SCOPE}")),
        "{meaning}"
    );
    assert!(
        meaning.contains(
            "so it is safe to conclude _make_timedelta is unused within the selected scope; you \
             can answer from this"
        ),
        "{meaning}"
    );
    let mut calls_only = every_use;
    calls_only["relation_kinds"] = json!(["calls"]);
    let reply = plain_answer(calls_only, "find_references");
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.contains("This answer covers only relation kinds [\"calls\"];"),
        "{meaning}"
    );
    assert!(
        meaning.contains(
            "so it is safe to conclude nothing calls _make_timedelta within the selected scope"
        ),
        "{meaning}"
    );
    assert!(!meaning.contains("unused"), "{meaning}");
}

/// The codes a usage answer most often stops on each say what is open, never
/// the generic sentence.
#[test]
fn usage_codes_that_stop_an_answer_never_fall_back_to_the_generic_reason() {
    for code in [
        "reference_completion_unmeasured",
        "local_binding_outstanding",
    ] {
        let reason = plain_reason(code).unwrap();
        assert_ne!(reason, UNEXPLAINED, "{code}");
        for jargon in JARGON {
            assert!(!reason.contains(jargon), "{code}: {reason}");
        }
    }
}

/// Physical location metadata is not needed to retain an entity or its code.
#[test]
fn semantic_rows_keep_exact_program_text_and_entity_identity_without_coordinates() {
    let code = "from package import thing\nreturn \"src/example.py\"";
    let signature = "def resolve(path=\"src/example.py\")";
    let row = json!({"id":"entity-a", "name":"Owner.resolve", "qualified_name":"package.Owner.resolve",
        "kind":"function", "file":"src/owner.py", "file_path":"src/owner.py",
        "read_path":"/checkout/src/owner.py", "entity_file":"src/owner.py",
        "line":90,"start_line":90,"end_line":92,"start_byte":1024,"end_byte":1096,
        "body":code,"signature":signature, "path":["entity-a", "entity-b"]});
    let plain = plain_row(row);
    assert_eq!(plain["entity_id"], "entity-a");
    assert_eq!(plain["name"], "Owner.resolve");
    assert_eq!(plain["qualified_name"], "package.Owner.resolve");
    assert_eq!(plain["body"], code);
    assert_eq!(plain["signature"], signature);
    assert_eq!(plain["path"], json!(["entity-a", "entity-b"]));
    for key in [
        "file",
        "file_path",
        "read_path",
        "entity_file",
        "line",
        "start_line",
        "end_line",
        "start_byte",
        "end_byte",
    ] {
        assert!(plain.get(key).is_none(), "{key}: {plain}");
    }
}

#[test]
fn path_derived_lookup_context_is_a_semantic_gap_without_an_invented_code_match() {
    let hit = json!({"entity_id":"entity-a", "name":"resolve", "kind":"function",
        "file":"src/private.py", "matched_field":"file_surface_context",
        "excerpt":"src/private.py python", "line":90,"start_line":90});
    let plain = plain_literal_hit(&hit);
    assert_eq!(plain["entity_id"], "entity-a");
    assert_eq!(plain["name"], "resolve");
    assert_eq!(plain["semantic_gap"], "entity_local_match_unavailable");
    assert!(plain.get("code").is_none());
    assert!(plain.get("line").is_none());
    assert!(plain.get("line_in_entity").is_none());
    assert!(!plain.to_string().contains("src/private.py"));
}

#[test]
fn literal_offsets_are_retained_only_when_the_entity_relative_origin_was_recorded() {
    for offset in [None, Some(0), Some(2)] {
        let mut hit = json!({"entity_id":"entity-a", "name":"resolve", "matched_field":"body_preview",
            "excerpt":"resolve()", "line":12,"start_line":10,"line_confidence":"exact"});
        if let Some(offset) = offset {
            hit["line_in_entity"] = json!(offset);
        }
        let plain = plain_literal_hit(&hit);
        assert_eq!(plain.get("line_in_entity").and_then(Value::as_u64), offset);
        assert!(plain.get("line").is_none());
        assert_eq!(plain["code"], "resolve()");
    }
}

#[test]
fn already_projected_literal_gaps_survive_plain_rendering() {
    let plain = plain_literal_hit(&json!({"entity_id":"entity-a", "name":"resolve",
        "semantic_gap":"entity_local_match_unavailable"}));
    assert_eq!(plain["semantic_gap"], "entity_local_match_unavailable");
    assert!(plain.get("code").is_none());
}

#[test]
fn a_semantic_outline_without_a_declaration_header_does_not_claim_one() {
    let reply = plain_answer(machine(&json!({"id":"module-a", "kind":"Module",
        "body_omitted":"container_shape", "members":[{"id":"member-a", "name":"resolve"}],
        "semantic_identity_gap":"A semantic name is not recorded for this entity; use its entity_id.",
        "_kin":{"verdict":{"state":"certified","safe_to_conclude_absent":false}}
    }).to_string()), "get_entity_source");
    assert_eq!(reply["entity_id"], "module-a");
    assert_eq!(reply["members"][0]["entity_id"], "member-a");
    assert!(meaning_of(&reply).contains("outline of this entity"));
    assert!(!meaning_of(&reply).contains("its header"));
    assert!(reply.get("semantic_identity_gap").is_some());
}

/// Outstanding local-name bindings the answer showed unrelated to it are
/// counted beside it: they leave a whole answer complete, say the one way any
/// of them could matter, and never turn an empty answer into an absence.
#[test]
fn bindings_unrelated_to_the_answer_are_counted_and_never_an_absence() {
    for empty in [false, true] {
        let mut machine = machine(CALLERS);
        machine.insert(
            "_kin".into(),
            json!({
                "verdict": {
                    "state": "certified_except",
                    "safe_to_conclude_absent": false,
                    "limiting_factor": "local_binding_unrelated; unkeyed_lookups_counted",
                },
                "source_derivation": {"binding_scope": {
                    "basis": "answer_entities", "languages": ["Python"],
                    "prior_local_binding": "outstanding",
                    "outstanding_local_binding_obligations": 2,
                    "unrelated_obligations": 2,
                }},
            }),
        );
        machine.remove(crate::handlers::entities::CALL_SITE_CANDIDATES_KEY);
        if empty {
            machine.insert("references".into(), json!([]));
            machine.insert("total_upstream".into(), json!(0));
        }
        let reply = plain_answer(machine, "find_references");
        assert_plain(&reply);
        assert_eq!(reply["_kin"]["verdict"], COMPLETE, "{reply:#}");
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert!(
            meaning.contains("2 local names elsewhere in this code are not resolved")
                && meaning.contains("could matter only if"),
            "{meaning}"
        );
        assert!(!meaning.contains("safe to conclude"), "{meaning}");
        assert!(!meaning.contains("unused"), "{meaning}");
        let gap = &reply["coverage_gaps"][0];
        assert_eq!(gap["folded"], true, "{reply:#}");
        assert_eq!(gap["count"], 2);
        assert!(gap.get("resolution_note").is_none());
    }
}

/// The same bindings, still open because one could hide a use, keep the
/// answer partial: a fold is only ever what the answer showed unrelated.
#[test]
fn bindings_the_answer_could_not_rule_out_keep_it_partial() {
    let mut machine = machine(CALLERS);
    machine.insert(
        "_kin".into(),
        json!({"verdict": {
            "state": "inconclusive",
            "safe_to_conclude_absent": false,
            "limiting_factor": "local_binding_outstanding; unkeyed_lookups_counted",
        }}),
    );
    let reply = plain_answer(machine, "find_references");
    assert_eq!(reply["_kin"]["verdict"], PARTIAL);
    assert!(reply["_kin"]["meaning"]
        .as_str()
        .unwrap()
        .contains("Kin has not resolved what some local names in this code point to"));
}

/// A test exclusion the question asked for is scope, not a gap.
#[test]
fn a_requested_test_exclusion_reads_complete_for_non_test_code() {
    for (factor, state, complete) in [
        ("unkeyed_lookups_counted", "certified_except", true),
        ("method_call_resolution_incomplete", "inconclusive", false),
    ] {
        let mut machine = machine(CALLERS);
        machine.insert(
            "_kin".into(),
            json!({"verdict": {"state": state, "safe_to_conclude_absent": false,
                "limiting_factor": factor}}),
        );
        machine.insert(
            "caller_scope".into(),
            json!({"basis": "current_stored_caller_role", "excluded_role": "test",
                "include_tests": false}),
        );
        machine.remove(crate::handlers::entities::CALL_SITE_CANDIDATES_KEY);
        let reply = plain_answer(machine, "find_references");
        assert_plain(&reply);
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert!(
            !meaning.contains("unattributed coverage obligations"),
            "{meaning}"
        );
        if complete {
            assert_eq!(reply["_kin"]["verdict"], COMPLETE);
            assert!(
                meaning.starts_with(
                    "Answer: Complete for non-test code: test code was excluded, as asked."
                ),
                "{meaning}"
            );
        } else {
            assert_eq!(reply["_kin"]["verdict"], PARTIAL);
            assert!(
                meaning.contains("Test code was excluded, as asked; this covers non-test code."),
                "{meaning}"
            );
            assert!(!meaning.contains("Complete for non-test code"), "{meaning}");
        }
    }
}

/// An empty recorded module inventory cannot certify complete membership or
/// prove that the module declares nothing.
#[test]
fn a_module_source_read_keeps_missing_member_coverage_partial() {
    let mut machine = machine(SOURCE);
    machine.insert("kind".into(), json!("module"));
    machine.insert("name".into(), json!("ReactFiberConfigTestHost"));
    machine.insert("members".into(), json!([]));
    machine.remove("body");
    machine.insert("header".into(), json!(""));
    machine.insert(
        "semantic_gap".into(),
        json!(crate::container_shape::MODULE_MEMBER_GAP),
    );
    machine.insert(
        "_kin".into(),
        json!({"verdict": {"state": "inconclusive", "safe_to_conclude_absent": false,
            "limiting_factor": crate::container_shape::MODULE_MEMBER_GAP}}),
    );
    let reply = plain_answer(machine, "get_entity_source");
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], PARTIAL, "{reply:#}");
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(meaning.starts_with("Partial outline: "), "{meaning}");
    assert!(
        meaning.contains("not a finding that it declares nothing"),
        "{meaning}"
    );
}

/// A callers walk counts the places it folded by reason and says how each
/// could matter, beside its members.
#[test]
fn a_trace_says_what_it_folded_and_how_it_could_matter() {
    let mut machine = machine(TRACE);
    let mut reach = json!({"members": [], "members_total": 0, "frontier": [],
        "frontier_total": 0, "frontier_withheld": [], "state": "open"});
    let mut by_reason = Map::new();
    by_reason.insert(crate::trace_reach::FOLD_TEXT_NOT_READ.into(), json!(52657));
    reach[crate::trace_reach::FRONTIER_FOLDED_KEY] = json!({"count": 52657,
        "by_reason": by_reason, "why": crate::trace_reach::FRONTIER_FOLDED_WHY});
    machine.insert("reach".into(), reach);
    let reply = plain_answer(machine, TRACE_TOOL);
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.contains("52657 observations are counted by reason in reach.frontier_folded"),
        "{meaning}"
    );
    assert!(
        !meaning.contains("could add callers are described"),
        "{meaning}"
    );
}

/// A named site whose kind never held a missed caller (its caller is already
/// a row, or its receiver is a value of a type outside the repository) is
/// counted by reason, while a site whose receiver Kin could not identify,
/// such as a test's fixture parameter, stays listed exactly as before.
#[test]
fn settled_named_sites_are_counted_and_unidentified_receivers_stay_listed() {
    let mut machine = machine(CALLERS);
    let row = machine["references"][0]["entity_id"].clone();
    machine.insert(
        "_kin".into(),
        json!({"verdict": {"state": "certified_except", "safe_to_conclude_absent": false,
            "limiting_factor": "named_leftovers; unkeyed_lookups_counted"}}),
    );
    let site = |caller: Value, expr: &str, reason: &str| {
        json!({"callee": "pop", "caller": caller, "caller_name": "c", "expr": expr,
            "line_in_entity": 3, "state": "unresolved", "reason": "callee spells the name",
            "receiver": expr.split('.').next().unwrap(),
            crate::receiver_binding::RECEIVER_BINDING_MISSING_KEY:
                {"step": "binding", "name": "x", "reason": reason}})
    };
    machine.insert(
        crate::handlers::entities::CALL_SITE_CANDIDATES_KEY.into(),
        json!({"named": [
            site(row.clone(), "ctx.pop(exc)", "no_value_origin_fact"),
            site(json!("other-caller"), "stack.pop()", "method_of_external_type"),
            site(json!("fixture-test"), "app.pop()", "no_value_origin_fact"),
        ]}),
    );
    let reply = plain_answer_with_details(machine.clone(), "find_references", false);
    assert_plain(&reply);
    let groups = reply[LEFT_TO_CHECK_KEY].as_array().unwrap();
    let listed: Vec<&Value> = groups
        .iter()
        .filter_map(|group| group.get("sites"))
        .flat_map(|sites| sites.as_array().unwrap())
        .collect();
    assert_eq!(listed.len(), 1, "{reply:#}");
    assert_eq!(listed[0]["code"], "app.pop()");
    // What a library's method returned may be an object of this repository:
    // counted, and still left to check.
    let folded = groups.last().unwrap();
    assert_eq!(folded["folded_entries"], 1);
    assert_eq!(
        folded[GAP_REASONS_KEY],
        json!({"receiver_outside_repository": 1})
    );
    assert!(folded["why"].as_str().unwrap().contains("details:true"));
    // A call whose caller is already a row adds no function: settled.
    let settled = settled_by(&reply, "call site");
    assert_eq!(settled["folded_entries"], 1);
    assert_eq!(settled[GAP_REASONS_KEY], json!({CALLER_ALREADY_A_ROW: 1}));
    assert_eq!(
        settled["why"],
        "1 call site that spells AppContext.pop, proved to add no function to this answer. \
         caller_already_a_row: the caller is already a row of this answer."
    );
    // The listed fixture site keeps the answer complete except it.
    assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT);
    // Details list every site, as before.
    let mut detailed =
        json!({"content":[{"type":"text","text":Value::Object(machine.clone()).to_string()}]});
    present(&mut detailed, "find_references");
    let detailed: Value =
        serde_json::from_str(detailed["content"][0]["text"].as_str().unwrap()).unwrap();
    let all: usize = detailed[LEFT_TO_CHECK_KEY]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|group| group.get("sites"))
        .map(|sites| sites.as_array().unwrap().len())
        .sum();
    assert_eq!(all, 3, "{detailed:#}");
}

/// When every site left is folded, the answer is complete except that count
/// and says the one way the folded sites could matter; an empty answer is
/// still never an absence.
#[test]
fn an_answer_whose_only_sites_are_folded_reads_complete_and_never_absent() {
    for empty in [false, true] {
        let mut machine = machine(CALLERS);
        machine.insert(
            "_kin".into(),
            json!({"verdict": {"state": "certified_except", "safe_to_conclude_absent": false,
                "limiting_factor": "named_leftovers; unkeyed_lookups_counted"}}),
        );
        machine.insert(
            crate::handlers::entities::CALL_SITE_CANDIDATES_KEY.into(),
            json!({"named": [{"callee": "pop", "caller": "lib-caller", "caller_name": "c",
                "expr": "stack.pop()", "line_in_entity": 3, "state": "unresolved",
                "reason": "callee spells the name", "receiver": "stack",
                crate::receiver_binding::RECEIVER_BINDING_MISSING_KEY:
                    {"step": "method", "name": "deque.pop", "reason": "method_of_external_type"}}]}),
        );
        if empty {
            machine.insert("references".into(), json!([]));
            machine.insert("total_upstream".into(), json!(0));
        }
        let reply = plain_answer_with_details(machine, "find_references", false);
        assert_plain(&reply);
        // Complete except the count, never a plain complete and never partial.
        assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT, "{reply:#}");
        let meaning = reply["_kin"]["meaning"].as_str().unwrap();
        assert!(
            meaning.contains("1 more place is counted by reason in left_to_check"),
            "{meaning}"
        );
        assert!(!meaning.contains("Check a listed site"), "{meaning}");
        assert!(!meaning.contains("nothing calls"), "{meaning}");
        assert!(!meaning.contains("safe to conclude"), "{meaning}");
        assert!(!meaning.contains("unused"), "{meaning}");
    }
}

/// A calls-only reference answer leaves the override census out, counting it
/// in coverage, while an answer about every use keeps it listed.
#[test]
fn a_calls_only_answer_leaves_out_the_override_census_and_counts_it() {
    for (kinds, listed) in [
        (json!(["calls"]), false),
        (json!(["calls", "imports", "references"]), true),
    ] {
        let mut machine = machine(CALLERS);
        machine.insert("relation_kinds".into(), kinds.clone());
        machine.insert(
            crate::handlers::overrides::OVERRIDDEN_BY_KEY.into(),
            json!({"count": 0, "proven": 0, "overriders": [],
                "coverage": {"scope": "recorded_overrides_and_implements",
                    "recorded_closure": "complete", "nested_definitions": "focal_file_measured",
                    "repository_completeness": "incomplete"},
                "classes_spelling_base_without_edge": [
                    {"entity_id": "c1", "name": "test_x.<locals>.CustomFlask", "module": "tests.test_x"},
                    {"entity_id": "c2", "name": "test_y.<locals>.MyFlask", "module": "tests.test_y"}]}),
        );
        let reply = plain_answer(machine, "find_references");
        assert_plain(&reply);
        let block = &reply["overridden_by"];
        if listed {
            assert_eq!(
                block["classes_spelling_base_without_edge"]
                    .as_array()
                    .unwrap()
                    .len(),
                2,
                "{kinds}: {block:#}"
            );
        } else {
            assert!(
                block.get("classes_spelling_base_without_edge").is_none(),
                "{kinds}: {block:#}"
            );
            assert_eq!(block["coverage"]["classes_spelling_base_without_edge"], 2);
            // Coverage still reads incomplete: the count is never an absence.
            assert_eq!(block["coverage"]["repository_completeness"], "incomplete");
            assert!(block.get("gaps").is_none(), "{block:#}");
        }
    }
}

/// A walk cut by its own caps says so in plain words, rather than the catch-all
/// that names no cause.
#[test]
fn a_walk_cut_by_its_caps_names_the_cap() {
    for (code, words) in [
        ("trace_walk_truncated", "per-step or total cap"),
        ("trace_walk_degraded", "its own work bounds"),
    ] {
        let reason = plain_reason(code).unwrap();
        assert!(reason.contains(words), "{code}: {reason}");
        assert_ne!(reason, UNEXPLAINED);
        for jargon in JARGON {
            assert!(!reason.contains(jargon), "{code}: {reason}");
        }
    }
}

/// The `flask.json.load` answer a model spent a dozen turns checking: tests
/// pass the standard library's `json.load`, the focal's own body calls
/// `_json.load`, and one test passes `tomllib.load`, bound by a call.
fn outside_imports(focal: &str) -> Map<String, Value> {
    let mut machine = machine(focal);
    machine.insert(
        "_kin".into(),
        json!({"verdict": {"state": "certified_except", "safe_to_conclude_absent": false,
            "limiting_factor": "named_leftovers; value_escape_listed"}}),
    );
    let call = |expr: &str, receiver: &str, state: &str, import: Value| {
        let mut row = json!({"callee": "load", "caller": format!("caller-of-{receiver}"),
            "caller_name": format!("uses_{receiver}"), "expr": expr, "line_in_entity": 3,
            "state": state, "reason": "callee spells the name", "receiver": receiver});
        if !import.is_null() {
            row[crate::site_expression::RECEIVER_IMPORT_KEY] = import;
        }
        row
    };
    let toml = json!({"file": "tests/test_config.py", "line": 47, "kind": "argument",
        "name": "load", "detail": "tomllib", "inside": "test_config_from_file_toml",
        "inside_id": "791b929d-0000-4000-8000-000000000001",
        "expr": "os.path.join(current_dir, \"static\", \"config.toml\"), tomllib.load, text=False"});
    let annotation = json!({"file": "src/flask/config.py", "line": 190, "kind": "annotation",
        "name": "load", "inside": "Config.from_file",
        "inside_id": "d821107e-0000-4000-8000-000000000002",
        "expr": "load: t.Callable[[t.IO[t.Any]], t.Mapping[str, t.Any]],"});
    machine.insert(
        crate::handlers::entities::CALL_SITE_CANDIDATES_KEY.into(),
        json!({
            "named": [
                call("_json.load(fp, **kwargs)", "_json", "unresolved",
                    json!({"module": "json", "outside_repository": true})),
                call("fjson.load(fp)", "fjson", "unresolved",
                    json!({"module": "flask", "outside_repository": false})),
                call("stream.load(fp)", "stream", "unresolved", Value::Null),
            ],
            "census": {"escape": "escapes", "residue": {
                "values": {"count": 1, "sites": [toml.clone()]},
                kin_model::call_site_reading::OUTSIDE_RECEIVERS_KEY: [
                    {"receiver": "json", "module": "json", "name": "load", "reads": 3}],
                "mentions": {"measured": true, "count": 2, "matched": {},
                    "unmatched": {"count": 2, "sites": [toml, annotation]}},
            }},
        }),
    );
    machine
}

fn listed_sites(reply: &Value) -> Vec<&Value> {
    reply[LEFT_TO_CHECK_KEY]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|group| group.get("sites"))
        .flat_map(|sites| sites.as_array().unwrap())
        .collect()
}

fn folded_by(reply: &Value, needle: &str) -> Value {
    counted_group(reply, LEFT_TO_CHECK_KEY, needle)
}

/// The counted group under `settled` whose sentence says `needle`.
fn settled_by(reply: &Value, needle: &str) -> Value {
    counted_group(reply, SETTLED_KEY, needle)
}

fn counted_group(reply: &Value, key: &str, needle: &str) -> Value {
    reply[key]
        .as_array()
        .unwrap_or_else(|| panic!("no {key} in the reply\n{reply:#}"))
        .iter()
        .find(|group| {
            group.get("sites").is_none()
                && group["why"]
                    .as_str()
                    .is_some_and(|why| why.contains(needle))
        })
        .cloned()
        .unwrap_or_else(|| panic!("no counted group under {key} says {needle:?}\n{reply:#}"))
}

/// A call or a read made on a name its file imports from outside the
/// repository is counted with what it imports, a listed call says which
/// module its receiver comes from, and a value use is listed once: under the
/// calls that pass it, never again as a read.
#[test]
fn outside_imports_are_counted_and_a_listed_site_says_what_its_receiver_imports() {
    let reply =
        plain_answer_with_details(outside_imports(DEADCODE_BY_ID), "find_references", false);
    assert_plain(&reply);
    let listed = listed_sites(&reply);
    let codes: Vec<&str> = listed
        .iter()
        .filter_map(|site| site["code"].as_str())
        .collect();
    assert_eq!(
        codes,
        vec![
            "fjson.load(fp)",
            "stream.load(fp)",
            "load: t.Callable[[t.IO[t.Any]], t.Mapping[str, t.Any]],",
        ],
        "{reply:#}"
    );
    assert_eq!(listed[0][RECEIVER_IMPORTED_FROM_KEY], "flask");
    assert!(listed[0].get(RECEIVER_OUTSIDE_KEY).is_none(), "{reply:#}");
    assert!(
        listed[1].get(RECEIVER_IMPORTED_FROM_KEY).is_none(),
        "{reply:#}"
    );

    // The call on `_json`, settled under its reason.
    let calls = settled_by(&reply, "call site");
    assert_eq!(calls["folded_entries"], 1);
    assert_eq!(
        calls[GAP_REASONS_KEY],
        json!({RECEIVER_IMPORTED_FROM_OUTSIDE: 1})
    );
    assert!(
        calls["why"]
            .as_str()
            .unwrap()
            .contains("a module no file of this repository supplies"),
        "{calls}"
    );
    // The three reads of `json.load`, counted with the import they stand on.
    let reads = settled_by(&reply, "reads of json.load");
    assert_eq!(reads["folded_entries"], 3);
    assert_eq!(
        reads["why"],
        "3 reads of json.load, where its file imports json from the module json. No file of \
         this repository supplies that module, so each reads the outside module's own member, \
         not this one. Counted, not listed."
    );
    // The value use is served once.
    let passed = reply[PASSED_TO_CALLS_KEY]["sites"].as_array().unwrap();
    assert_eq!(passed.len(), 1, "{reply:#}");
    assert!(passed[0]["code"].as_str().unwrap().contains("tomllib.load"));
    let text = reply.to_string();
    assert_eq!(text.matches("tomllib.load").count(), 1, "{reply:#}");
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.contains("3 other places could reach it"),
        "{meaning}"
    );
    assert!(
        meaning.contains("4 more places that spell _make_timedelta are settled"),
        "{meaning}"
    );
    assert!(
        !meaning.contains("counted by reason in left_to_check"),
        "nothing open is counted: {meaning}"
    );
    assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT);
}

/// A read, not a call, made on a value proved to take the member from a type
/// outside the repository is counted with that type, beside the reads on
/// outside imports and under the same total.
#[test]
fn reads_on_a_value_of_an_outside_type_are_counted_with_the_type() {
    let with_values = |machine: &mut Map<String, Value>| {
        machine[crate::handlers::entities::CALL_SITE_CANDIDATES_KEY]["census"]["residue"]
            [kin_model::call_site_reading::OUTSIDE_VALUES_KEY] = json!([
            {"receiver": "runner", "owner": "click.testing.CliRunner", "name": "invoke",
                "reads": 1},
            {"receiver": "client", "owner": "werkzeug.test.Client", "name": "get", "reads": 2},
        ]);
    };
    let before =
        plain_answer_with_details(outside_imports(DEADCODE_BY_ID), "find_references", false);
    let mut machine = outside_imports(DEADCODE_BY_ID);
    with_values(&mut machine);
    let reply = plain_answer_with_details(machine, "find_references", false);
    assert_plain(&reply);

    let reads = settled_by(&reply, "reads of client.get");
    assert_eq!(reads["folded_entries"], 3);
    assert_eq!(reads[GAP_REASONS_KEY], json!({MEMBER_OF_OUTSIDE_TYPE: 3}));
    assert_eq!(
        reads["why"],
        "1 read of runner.invoke, where the value runner holds takes invoke from \
         click.testing.CliRunner; 2 reads of client.get, where the value client holds takes get \
         from werkzeug.test.Client. No file of this repository declares those types, so each \
         reads a member declared outside this repository, not this one. Counted, not listed."
    );
    // Counted, never listed: the sites to check are the ones it had.
    assert_eq!(listed_sites(&reply).len(), listed_sites(&before).len());
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(
        meaning.contains("7 more places that spell _make_timedelta are settled"),
        "{meaning}"
    );
    // The detailed form counts them the same way.
    let mut machine = outside_imports(DEADCODE_BY_ID);
    with_values(&mut machine);
    let detailed = plain_answer_with_details(machine, "find_references", true);
    assert_eq!(
        settled_by(&detailed, "reads of client.get")["folded_entries"],
        3
    );
}

/// The answer about `FlaskCliRunner.invoke` once click's own runner is proved:
/// the one place that spelled the name and is not a row is a read of click's
/// member. Nothing is listed and nothing open is counted, so the answer is
/// complete, not "complete except" a list that is empty. A counted call whose
/// reason leaves it open keeps the weaker verdict.
#[test]
fn an_answer_whose_every_leftover_is_settled_is_complete() {
    let with = |named: Value| {
        let mut machine = machine(CALLERS);
        machine.insert(
            "_kin".into(),
            json!({"verdict": {"state": "certified_except", "safe_to_conclude_absent": false,
                "limiting_factor": "unkeyed_lookups_counted"}}),
        );
        machine.insert(
            crate::handlers::entities::CALL_SITE_CANDIDATES_KEY.into(),
            json!({
                "named": named,
                "census": {"escape": "contained", "entities_checked": 1772, "residue": {
                    kin_model::call_site_reading::OUTSIDE_VALUES_KEY: [
                        {"receiver": "runner", "owner": "click.testing.CliRunner",
                            "name": "invoke", "reads": 1}],
                    "unkeyed_lookups": {"count": 14},
                }},
            }),
        );
        machine
    };
    let reply = plain_answer_with_details(with(json!([])), "find_references", false);
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE, "{reply:#}");
    assert!(reply.get(LEFT_TO_CHECK_KEY).is_none(), "{reply:#}");
    let settled = settled_by(&reply, "read of runner.invoke");
    assert_eq!(settled[GAP_REASONS_KEY], json!({MEMBER_OF_OUTSIDE_TYPE: 1}));
    let meaning = reply["_kin"]["meaning"].as_str().unwrap();
    assert!(meaning.contains("you can answer from this"), "{meaning}");
    assert!(
        meaning.contains("1 more place that spells pop is settled"),
        "{meaning}"
    );
    assert!(!meaning.contains("left_to_check"), "{meaning}");

    // What a library's method returned may be one of this repository's own
    // objects: that call is counted, stays under left_to_check, and the
    // answer stays complete except it.
    let open = json!([{"callee": "pop", "caller": "other-caller", "caller_name": "c",
        "expr": "stack.pop()", "line_in_entity": 3, "state": "unresolved",
        "reason": "callee spells the name", "receiver": "stack",
        crate::receiver_binding::RECEIVER_BINDING_MISSING_KEY:
            {"step": "returns", "name": "x", "reason": "method_of_external_type"}}]);
    let mut machine = with(open);
    machine.insert(
        "_kin".into(),
        json!({"verdict": {"state": "certified_except", "safe_to_conclude_absent": false,
            "limiting_factor": "named_leftovers; unkeyed_lookups_counted"}}),
    );
    let reply = plain_answer_with_details(machine, "find_references", false);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE_EXCEPT, "{reply:#}");
    assert_eq!(
        folded_by(&reply, "call site")[GAP_REASONS_KEY],
        json!({"receiver_outside_repository": 1})
    );
    assert!(settled_by(&reply, "read of runner.invoke").is_object());
}

/// A read off an instance of a class that shares no instance with the
/// entity's owner is counted as settled, in a sentence about the entity asked
/// about: it says whose hierarchy the receiver is in and that the owner is
/// not in it, never where the member that is read is declared.
#[test]
fn reads_on_an_instance_of_an_unrelated_class_are_settled_relative_to_the_entity() {
    let mut machine = machine(CALLERS);
    machine.insert(
        "_kin".into(),
        json!({"verdict": {"state": "certified_except", "safe_to_conclude_absent": false,
            "limiting_factor": "unkeyed_lookups_counted"}}),
    );
    machine.insert(
        crate::handlers::entities::CALL_SITE_CANDIDATES_KEY.into(),
        json!({
            "named": [],
            "census": {"escape": "contained", "entities_checked": 1772, "residue": {
                kin_model::call_site_reading::UNRELATED_RECEIVERS_KEY: [
                    {"receiver": "self", "class": "MethodView", "name": "pop", "reads": 1},
                    {"receiver": "self", "class": "SessionMixin", "name": "pop", "reads": 2}],
                "unkeyed_lookups": {"count": 14},
            }},
        }),
    );
    let reply = plain_answer_with_details(machine, "find_references", false);
    assert_plain(&reply);
    assert_eq!(reply["_kin"]["verdict"], COMPLETE, "{reply:#}");
    let settled = settled_by(&reply, "reads of self.pop");
    assert_eq!(settled["folded_entries"], 3);
    assert_eq!(
        settled[GAP_REASONS_KEY],
        json!({RECEIVER_OF_UNRELATED_CLASS: 3})
    );
    assert_eq!(
        settled["why"],
        "1 read of self.pop, where self is an instance of MethodView; 2 reads of self.pop, \
         where self is an instance of SessionMixin. AppContext is not in those classes' \
         hierarchies: no class is, or derives from, both. So each reads a member of that \
         other hierarchy, not this one. Counted, not listed."
    );
    let why = settled["why"].as_str().unwrap();
    assert!(!why.contains("outside"), "{why}");
}

/// A method is reached through a value of its class, which an outside module
/// can hand out, so a call on an outside import stays listed for a method and
/// still says where its receiver comes from. Details list every site.
#[test]
fn a_method_keeps_a_call_on_an_outside_import_and_details_list_every_site() {
    let reply = plain_answer_with_details(outside_imports(CALLERS), "find_references", false);
    let listed = listed_sites(&reply);
    let on_json = listed
        .iter()
        .find(|site| site["code"] == "_json.load(fp, **kwargs)")
        .unwrap_or_else(|| panic!("{reply:#}"));
    assert_eq!(on_json[RECEIVER_IMPORTED_FROM_KEY], "json");
    assert_eq!(on_json[RECEIVER_OUTSIDE_KEY], true);

    let detailed =
        plain_answer_with_details(outside_imports(DEADCODE_BY_ID), "find_references", true);
    let listed = listed_sites(&detailed);
    assert!(
        listed
            .iter()
            .any(|site| site["code"] == "_json.load(fp, **kwargs)"
                && site[RECEIVER_OUTSIDE_KEY] == true),
        "{detailed:#}"
    );
    // The reads are a count either way: the census lists no site for them.
    assert_eq!(
        settled_by(&detailed, "reads of json.load")["folded_entries"],
        3
    );
}

/// A call through a variable says what kind of thing the variable is and what
/// would make it run the focal, instead of that Kin could not pin it down.
#[test]
fn a_call_through_a_variable_says_what_would_make_it_run_the_focal() {
    let mut machine = machine(DEADCODE_BY_ID);
    machine.insert(
        "_kin".into(),
        json!({"verdict": {"state": "certified_except", "safe_to_conclude_absent": false,
            "limiting_factor": "named_leftovers"}}),
    );
    machine.insert(
        crate::handlers::entities::CALL_SITE_CANDIDATES_KEY.into(),
        json!({"named": [{"callee": "load", "caller": "c", "caller_name": "Config.from_file",
            "expr": "load(f)", "line_in_entity": 40, "state": "binding",
            "reason": "callee spells the name"}]}),
    );
    let reply = plain_answer_with_details(machine, "find_references", false);
    assert_plain(&reply);
    let why = reply[LEFT_TO_CHECK_KEY][0]["why"].as_str().unwrap();
    assert_eq!(
        why,
        "Calls through a variable named load: a parameter, a local or an assigned name, not a \
         declaration. One runs _make_timedelta only if that variable was handed _make_timedelta \
         as a value, so check where its caller gets it."
    );
}

/// Every recorded reply in testdata, each as the tool that served it: the
/// plain word is the one its account decides (the renderer also asserts this
/// for every rendering in a debug build), and each reply's standing is pinned,
/// so a change in what a real reply is told shows up as a change to this
/// table. It also measures what `_kin.verdict.answer` costs each machine
/// reply that carries it.
#[test]
fn every_recorded_reply_says_the_word_its_account_decides() {
    let recorded: [(&str, &str, &str); 24] = [
        ("flask callers refs", CALLERS, "find_references"),
        ("flask callers context", CONTEXT, "get_context_pack"),
        ("flask callers source", SOURCE, "get_entity_source"),
        ("flask context by id", CONTEXT_BY_ID, "get_context_pack"),
        ("flask deadcode refs", SAME_NAME, "find_references"),
        (
            "flask deadcode refs by id",
            DEADCODE_BY_ID,
            "find_references",
        ),
        (
            "flask deadcode refs by name",
            DEADCODE_BY_NAME,
            "find_references",
        ),
        (
            "flask find_error_handler trace page",
            include_str!("testdata/plain_flask_find_error_handler_trace_page.json"),
            "trace_data_flow",
        ),
        (
            "flask get_root_path trace page",
            include_str!("testdata/plain_flask_get_root_path_trace_page.json"),
            "trace_data_flow",
        ),
        ("flask history", HISTORY, "kin_provenance_query"),
        ("flask impact refs", IMPACT, "find_references"),
        (
            "flask impact page",
            include_str!("testdata/plain_flask_impact_open_session_page.json"),
            "impact_analysis",
        ),
        ("flask ranked locate", RANKED, "semantic_locate"),
        ("flask name miss", NAME_MISS, "semantic_search"),
        ("flask neighbors", NEIGHBORS, "graph_neighborhood"),
        ("flask reach trace", TRACE, "trace_data_flow"),
        (
            "flask Scaffold.route trace page",
            include_str!("testdata/plain_flask_scaffold_route_trace_page.json"),
            "trace_data_flow",
        ),
        ("flask source", SOURCE_READ, "get_entity_source"),
        ("flask status", STATUS, STATUS_TOOL),
        (
            "react advanceTimers trace page",
            include_str!("testdata/plain_react_advance_timers_trace_page.json"),
            "trace_data_flow",
        ),
        (
            "react commitTextUpdate refs",
            COMMIT_TEXT_UPDATE,
            "find_references",
        ),
        (
            "react scoped reach page",
            include_str!("testdata/plain_react_reach_scoped_trace_page.json"),
            "trace_data_flow",
        ),
        ("react reach page", REACH_PAGE, "trace_data_flow"),
        (
            "react unscoped reach page",
            include_str!("testdata/plain_react_reach_unscoped_trace_page.json"),
            "trace_data_flow",
        ),
    ];
    let mut table = Vec::new();
    for (name, fixture, tool) in recorded {
        let mut reply = machine(fixture);
        let envelope = reply
            .remove(crate::envelope::ENVELOPE_KEY)
            .unwrap_or(Value::Null);
        let account = crate::answer_account::account(tool, &reply, &envelope, None);
        let plain = plain_answer(machine(fixture), tool);
        assert_eq!(
            plain.pointer("/_kin/verdict").and_then(Value::as_str),
            account.word(),
            "{name}: {plain:#}"
        );
        assert!(
            crate::answer_account::breaches(&account, &plain).is_empty(),
            "{name}: {plain:#}"
        );
        // What the machine reply would carry, and whether this reply, as
        // recorded, is delivered whole and so carries it.
        let answer = account.to_value();
        let cost = format!(",\"answer\":{answer}").len();
        let mut payload: Value = serde_json::from_str(fixture).unwrap();
        crate::answer_account::attach_answer(tool, &mut payload);
        let carried = payload.pointer("/_kin/verdict/answer").is_some();
        table.push(format!(
            "{name}: {} ({:?}){}; answer {cost} bytes{}",
            account
                .word()
                .map_or("unjudged", |_| account.standing.wire()),
            account.basis,
            if account.open.is_empty() {
                String::new()
            } else {
                format!(", open {}", account.open.join(", "))
            },
            if carried {
                ", carried"
            } else {
                ", not carried"
            },
        ));
    }
    let table = table.join("\n");
    eprintln!("{table}");
    let expected = include_str!("testdata/recorded_reply_accounts.txt");
    assert_eq!(table.trim(), expected.trim(), "\n{table}");
}

/// A `refs` answer about an unparsed entity leads with what names it in its
/// text, matched by name, and is partial for that reason: with places, with
/// none, and with places withheld or unread.
#[test]
fn what_names_an_unparsed_entity_leads_its_partial_answer() {
    let reply = |rows: usize, total: u64, unread: u64| {
        let mut machine = json!({
            "_kin": {"verdict": {"state": "inconclusive",
                "limiting_factor": crate::verdict::MATCHED_BY_NAME,
                "safe_to_conclude_absent": false}},
            "entity": {"entity_id": "artifact", "name": "schema.sql", "kind": "SQL",
                "held_as": "text"},
            "named_in": (0..rows).map(|row| json!({
                "entity_id": format!("holder-{row}"), "name": format!("holder_{row}"),
                "line": "open(\"schema.sql\")"})).collect::<Vec<_>>(),
            "matched_by": "name",
            "named_in_total": total,
        });
        if unread > 0 {
            machine["named_in_unread"] = json!(unread);
        }
        plain_answer(machine.as_object().unwrap().clone(), "find_references")
    };
    let three = reply(3, 3, 0);
    assert_eq!(three["_kin"]["verdict"], PARTIAL, "{three:#}");
    assert_eq!(
        meaning_of(&three),
        "Partial: these are the 3 entities whose text names schema.sql, matched by name. Kin has not resolved what they use it for, and a name built while code runs, or a pattern, is not followed."
    );
    assert_eq!(
        three["named_in"].as_array().map(Vec::len),
        Some(3),
        "{three:#}"
    );

    let none = reply(0, 0, 0);
    assert_eq!(none["_kin"]["verdict"], PARTIAL, "{none:#}");
    assert!(
        meaning_of(&none).starts_with("Partial: no entity's text names schema.sql by name."),
        "{}",
        meaning_of(&none)
    );

    let cut = reply(2, 60, 1);
    assert!(
        meaning_of(&cut).contains(
            "named_in lists the first 2; 58 more are not listed. Kin could not read the text of 1 entity, so it could also name it."
        ),
        "{}",
        meaning_of(&cut)
    );
}

/// A ranking whose rows sit across packages names the two holding the most
/// and the scope that ranks inside one. Rows in one package, and a ranking
/// already confined to a scope, say nothing of it.
#[test]
fn a_ranking_across_packages_teaches_the_scope_that_ranks_inside_one() {
    let ranked = |modules: &[&str], confined: bool| {
        let mut machine = json!({
            "_kin": {"verdict": {"state": "certified", "limiting_factor": null,
                "safe_to_conclude_absent": false}},
            "query": "serialize a value to json",
            "entities": modules.iter().enumerate().map(|(index, module)| json!({
                "id": format!("row-{index}"), "name": format!("row_{index}"),
                "kind": "function", "module": module, "score": 1.0 - index as f64 / 100.0,
            })).collect::<Vec<_>>(),
        });
        if confined {
            machine["degradations"] = json!([{"component": "caller_order",
                "reason": "rows_in_scope", "detail": "rows confined to flask.json",
                "remediation": "omit scope to rank every module", "severity": "note"}]);
        }
        let reply = plain_answer(machine.as_object().unwrap().clone(), "semantic_locate");
        meaning_of(&reply).to_string()
    };
    let spread = ranked(
        &[
            "flask.json.provider",
            "flask.sansio.app",
            "flask.json",
            "flask.json.tag",
            "flask.sansio.scaffold",
            "flask.app",
            "flask.json.provider",
            "flask.sansio.app",
            "flask.ctx",
            "flask.json",
            "flask.sansio.blueprints",
            "flask.cli",
        ],
        false,
    );
    assert!(
        spread.ends_with(
            "Rows span flask.json (5), flask.sansio (4) and 3 more; scope: \"flask.json\" ranks inside one."
        ),
        "{spread}"
    );
    let two = ranked(&["flask.json", "flask.app"], false);
    assert!(
        two.ends_with(
            "Rows span flask.json (1) and flask.app (1); scope: \"flask.json\" ranks inside one."
        ),
        "{two}"
    );
    assert!(!ranked(&["flask.json.provider", "flask.json"], false).contains("Rows span"));
    assert!(!ranked(&["flask.json", "flask.app"], true).contains("Rows span"));
}

/// Sites a sweep could not decide after repeated passes over unchanged code,
/// in the shape a refs reply carries them: an answer is complete except them
/// only when every one is listed, and partial when the list was cut, could
/// not be read, or the sites are only counted.
#[test]
fn undecided_reference_sites_are_an_exception_only_when_every_one_is_listed() {
    let site = |index: usize| {
        json!({"caller": format!("caller-{index}"), "caller_name": format!("run_{index}"),
            "line_in_entity": 1, "callee": null})
    };
    let reply = |code: &str, block: Value| {
        // The listed code is a residue the answer lists; the counted one
        // leaves the answer inconclusive, as the verdict weighs them.
        let state = if code == crate::answer_account::UNDECIDED_SITES_LISTED {
            "certified_except"
        } else {
            "inconclusive"
        };
        let mut machine = machine(CALLERS);
        machine["_kin"]["verdict"] = json!({"state": state,
            "limiting_factor": code, "safe_to_conclude_absent": false});
        machine.insert("undecided_reference_sites".into(), block);
        machine
    };
    let account_of = |machine: &Map<String, Value>| {
        let mut reply = machine.clone();
        let envelope = reply.remove("_kin").unwrap_or(Value::Null);
        crate::answer_account::account("find_references", &reply, &envelope, None)
    };

    let whole = reply(
        crate::answer_account::UNDECIDED_SITES_LISTED,
        json!({"count": 1, "sites": [site(0)]}),
    );
    assert_eq!(
        account_of(&whole).standing,
        crate::answer_account::AnswerStanding::CompleteExceptListed
    );
    let plain = plain_answer(whole, "find_references");
    assert_eq!(plain["_kin"]["verdict"], COMPLETE_EXCEPT, "{plain:#}");
    assert_eq!(
        plain["undecided_reference_sites"]["sites"][0]["caller_name"],
        "run_0"
    );

    for (code, block) in [
        (
            crate::answer_account::UNDECIDED_SITES_LISTED,
            json!({"count": 60, "sites": (0..50).map(site).collect::<Vec<_>>()}),
        ),
        (
            crate::answer_account::UNDECIDED_SITES_LISTED,
            json!({"count": null, "sites": [], "unreadable": "the sweep record could not be read"}),
        ),
        (
            crate::answer_account::UNDECIDED_SITES,
            json!({"count": 3, "sites": []}),
        ),
    ] {
        let machine = reply(code, block.clone());
        assert_eq!(
            account_of(&machine).standing,
            crate::answer_account::AnswerStanding::Partial,
            "{code} {block}"
        );
        let plain = plain_answer(machine, "find_references");
        assert_eq!(
            plain["_kin"]["verdict"], PARTIAL,
            "{code} {block}: {plain:#}"
        );
        if code == crate::answer_account::UNDECIDED_SITES {
            assert!(
                meaning_of(&plain).contains(
                    "3 reference sites stayed undecided after repeated sweeps of unchanged code, so usages there may be missing"
                ),
                "{}",
                meaning_of(&plain)
            );
        }
    }
}
