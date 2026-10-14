// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use std::collections::HashMap;

use serde_json::{json, Value};

use super::*;
use crate::request_schema::{Field, Visibility};

#[test]
fn context_details_is_strict_optional_metadata_without_a_cursor_contract() {
    let schema = served(CONTEXT);
    assert_eq!(schema["properties"]["details"]["type"], "boolean");
    assert!(schema["properties"]["details"].get("default").is_none());
    assert!(schema["properties"].get("cursor").is_none());
    assert!(
        schema["properties"]["details"]["description"]
            .as_str()
            .unwrap()
            .len()
            <= crate::agent_belt::AGENT_DEFAULT_PROPERTY_DESCRIPTION_BUDGET
    );
    assert_eq!(
        declared_fields(CONTEXT)
            .unwrap()
            .into_iter()
            .find(|field| field.name == "details")
            .unwrap()
            .default,
        Some(json!(false))
    );
    for value in [json!(true), json!(false)] {
        let mut arguments = HashMap::from([
            ("entity_id".into(), json!("held")),
            ("details".into(), value.clone()),
        ]);
        check_call(CONTEXT, &mut arguments).unwrap();
        assert_eq!(arguments["details"], value);
    }
    for value in [Value::Null, json!("true"), json!(1), json!([]), json!({})] {
        let mut arguments = HashMap::from([
            ("entity_id".into(), json!("held")),
            ("details".into(), value),
        ]);
        assert!(check_call(CONTEXT, &mut arguments)
            .unwrap_err()
            .contains("details must be true or false"));
    }
    let mut continuation = HashMap::from([
        ("cursor".into(), json!("opaque")),
        ("details".into(), json!(true)),
    ]);
    assert!(check_call(CONTEXT, &mut continuation).is_err());
}

/// A `details` description names the one thing the toggle adds to a row and
/// promises nothing else. "Full rows; same steps." and "Metadata; same rows."
/// read as a free upgrade: in a model run the agent passed `details: true` on
/// 25 of 34 `locate` calls, 6 of 8 `context`, 5 of 10 `refs` and its one
/// `trace`. No line is longer than the one it replaced, since an agent pays
/// for every schema byte on every turn.
#[test]
fn details_guidance_names_what_a_row_gains_and_invites_nothing() {
    for (tool, guidance, replaced) in [
        (LOCATE, "Adds row signatures.", "Metadata; same rows."),
        (REFS, "Adds all site lines.", "Metadata; same rows."),
        (TRACE, "Adds proof.", "Full rows; same steps."),
        (CONTEXT, "Adds row metadata.", "Metadata; same code."),
    ] {
        let schema = served(tool);
        let served = schema["properties"]["details"]["description"]
            .as_str()
            .unwrap();
        assert_eq!(served, guidance, "{tool}");
        assert!(served.len() <= replaced.len(), "{tool}: {served}");
        let lower = served.to_ascii_lowercase();
        for invitation in ["same", "full", "complete", "whole", "more"] {
            assert!(!lower.contains(invitation), "{tool}: {served}");
        }
    }
}

fn schema_accepts(schema: &Value, instance: &Value) -> bool {
    crate::schema_check::errors(schema, instance).is_empty()
}

/// The agent-default listing, as a client receives it.
fn served_agent_default() -> crate::types::ToolsListResult {
    crate::tools::served_tools_list(
        Some(&crate::tools::name_set(
            crate::tools::agent_default_tool_names(),
        )),
        true,
    )
}

fn served(tool: &str) -> Value {
    served_agent_default()
        .tools
        .into_iter()
        .find(|served| served.name == tool)
        .unwrap_or_else(|| panic!("agent-default serves no {tool}"))
        .input_schema
}

fn arguments(value: Value) -> HashMap<String, Value> {
    serde_json::from_value(value).expect("arguments are an object")
}

#[test]
fn text_search_serves_one_mode_and_explicit_matching_options() {
    for schema in [
        served(LOCATE),
        served_schema(LOCATE, Profile::Full, &|_| None).unwrap(),
        crate::tools::tool_definitions()
            .tools
            .into_iter()
            .find(|tool| tool.name == LOCATE)
            .unwrap()
            .input_schema,
    ] {
        assert_eq!(
            schema["properties"]["mode"]["enum"],
            json!(["auto", "name", "description", "text"])
        );
        assert!(schema["properties"].get("literal").is_none());
        for field in ["pattern", "ignore_case"] {
            assert_eq!(schema["properties"][field]["type"], "boolean");
        }
        // The default depends on mode: text includes tests, description does
        // not. No universal schema default may tell a client otherwise.
        assert!(schema["properties"]["include_tests"]
            .get("default")
            .is_none());
        assert!(schema_accepts(
            &schema,
            &json!({"query": "retry(_after)?", "mode": "text", "pattern": true,
                    "ignore_case": true, "include_tests": false})
        ));
    }
    assert!(
        served_schema(LOCATE, Profile::Full, &|_| None).unwrap()["properties"]["include_tests"]
            ["description"]
            .as_str()
            .unwrap()
            .starts_with("Text includes")
    );
}

#[test]
fn literal_mode_is_an_unadvertised_spelling_of_text() {
    for mode in ["text", "literal", " LITERAL "] {
        let mut call = arguments(json!({"query": "retry", "mode": mode}));
        check_call(LOCATE, &mut call).unwrap();
        assert_eq!(call["mode"], "text");
        for field in ["pattern", "ignore_case", "include_tests"] {
            assert!(
                !call.contains_key(field),
                "{field} must remain a handler default"
            );
        }
    }
    for mode in ["name", "description", "auto"] {
        let mut call = arguments(json!({"query": "retry", "mode": mode}));
        check_call(LOCATE, &mut call).unwrap();
        assert_eq!(call["mode"], mode);
        assert!(!call.contains_key("include_tests"));
    }
}

#[test]
fn text_matching_options_are_strict_booleans_and_cursor_optional() {
    for field in ["pattern", "ignore_case"] {
        for value in [json!(true), json!(false)] {
            let mut call = arguments(json!({"cursor": "held-page"}));
            call.insert(field.into(), value.clone());
            check_call(LOCATE, &mut call).unwrap();
            assert_eq!(call[field], value);
        }
        for value in [Value::Null, json!("true"), json!(0), json!([]), json!({})] {
            let mut call = arguments(json!({"query": "retry", "mode": "text"}));
            call.insert(field.into(), value);
            assert!(check_call(LOCATE, &mut call)
                .unwrap_err()
                .contains(&format!("{field} must be true or false")));
        }
    }
    let mut continuation = arguments(json!({"cursor": "held-page"}));
    check_call(LOCATE, &mut continuation).unwrap();
    assert_eq!(
        continuation.len(),
        1,
        "cursor-only calls inherit every choice"
    );
}

#[test]
fn refs_served_guidance_separates_returned_uuids_from_names() {
    let schema = served(REFS);
    assert_eq!(
        schema["properties"]["entity_id"]["description"],
        "Returned UUID."
    );
    assert_eq!(
        schema["properties"]["query"]["description"],
        "All matches; qualified_name pins one."
    );
    assert_eq!(
        schema["properties"]["proof_for_caller"]["description"],
        "Caller UUID; entity_id is the callee UUID."
    );
    for query in ["Context.pop", "app._make_timedelta"] {
        check_call(REFS, &mut arguments(json!({"query": query}))).unwrap();
    }
}

#[test]
fn query_details_is_advertised_as_a_boolean_and_takes_a_bare_cursor() {
    for tool in [LOCATE, REFS] {
        let schema = served(tool);
        assert_eq!(schema["properties"]["details"]["type"], "boolean");
        // Agent schemas omit false defaults to keep their established budget;
        // the typed contract still declares the minimal view as the default.
        assert!(schema["properties"]["details"].get("default").is_none());
        assert_eq!(
            declared_fields(tool)
                .unwrap()
                .into_iter()
                .find(|field| field.name == "details")
                .unwrap()
                .default,
            Some(json!(false))
        );
        // An agent is told the one thing the toggle adds to a row, and the
        // full profile keeps how a continuation treats it.
        let guidance = schema["properties"]["details"]["description"]
            .as_str()
            .unwrap();
        assert!(guidance.starts_with("Adds "), "{guidance}");
        assert!(guidance.len() <= crate::agent_belt::AGENT_DEFAULT_PROPERTY_DESCRIPTION_BUDGET);
        let full = served_schema(tool, Profile::Full, &|_| None).unwrap();
        let full_guidance = full["properties"]["details"]["description"]
            .as_str()
            .unwrap();
        assert!(
            full_guidance.contains("MCP cursor inherits") && full_guidance.contains("CLI repeats"),
            "{full_guidance}"
        );
        for call in [
            json!({"query":"parse_config","details":true}),
            json!({"cursor":"opaque","details":true}),
            json!({"cursor":"opaque","details":false}),
        ] {
            assert!(schema_accepts(&schema, &call));
            let mut call = arguments(call);
            check_call(tool, &mut call).unwrap();
            assert!(call["details"].is_boolean());
        }
        for value in [json!("true"), json!(1), json!({}), json!([]), Value::Null] {
            let call = json!({"query":"parse_config","details":value});
            assert!(!schema_accepts(&schema, &call));
            let error = check_call(tool, &mut arguments(call)).unwrap_err();
            assert!(error.contains("details must be true or false"), "{error}");
        }
    }
    let query = crate::tools::served_tools_list(
        Some(&crate::tools::name_set(
            crate::tools::agent_query_tool_names(),
        )),
        true,
    );
    assert_eq!(query.tools.len(), 10);
}

#[test]
fn receiver_proof_selector_is_served_and_requires_an_exact_target_without_cursor() {
    let uuid = "00000000-0000-4000-8000-000000000001";
    let schema = served(REFS);
    assert!(schema["properties"]["proof_for_caller"].is_object());
    let mut valid = arguments(json!({"entity_id": uuid, "proof_for_caller": uuid}));
    check_call(REFS, &mut valid).unwrap();
    for call in [
        json!({"query": "Context.pop", "proof_for_caller": uuid}),
        json!({"entity_id": uuid, "proof_for_caller": uuid, "cursor": "page"}),
    ] {
        assert!(check_call(REFS, &mut arguments(call)).is_err());
    }
}

/// The anchored patch an agent sends, well formed.
fn patch_operation() -> Value {
    json!({
        "verb": "patch",
        "target": "00000000-0000-4000-8000-000000000001",
        "description": "Retry once more",
        "payload": {"EntitySourcePatch": {
            "source_base": {"schema": "kin.entity.source_base.v1"},
            "edits": [{"old_text": "retries = 2", "new_text": "retries = 3"}]
        }}
    })
}

/// Compact source bases are resolved after verb routing, before the backend
/// decodes the full base. Checking a request must preserve that opaque base.
#[test]
fn write_requests_preserve_source_base_tickets_for_resolution() {
    let ticket = json!({
        "ticket": "sb1.00000000-0000-4000-8000-000000000001.7.0123456789abcdef0123456789abcdef"
    });
    let mut patch = patch_operation();
    patch["payload"]["EntitySourcePatch"]["source_base"] = ticket.clone();
    let body = json!({
        "verb": "update",
        "target": "00000000-0000-4000-8000-000000000001",
        "description": "Retry once more",
        "body": "fn retry() {}",
        "payload": {"EntitySourceBase": ticket}
    });
    for operation in [patch, body] {
        for tool in [MUTATE, TRANSACTION] {
            let mut call = arguments(json!({"operations": [operation.clone()]}));
            if tool == TRANSACTION {
                call.insert("action".into(), json!("begin"));
            }
            check_call(tool, &mut call).expect("the source base awaits resolution");
            assert_eq!(call["operations"], json!([operation]));
        }
    }
}

/// One deliberately invalid call, the words its refusal must carry, and
/// whether the served schema can refuse it as well. A selector rule is the
/// server's alone: a flat schema cannot say it without a top-level combinator.
struct Probe {
    tool: &'static str,
    call: Value,
    fix: &'static str,
    schema_refuses: bool,
}

fn probes() -> Vec<Probe> {
    let uuid = "00000000-0000-4000-8000-000000000001";
    let mismatched = {
        let mut operation = patch_operation();
        operation["verb"] = json!("update");
        operation["payload"] = json!({"EntityCreate": {"name": "run"}});
        operation
    };
    let two_payloads = {
        let mut operation = patch_operation();
        operation["payload"]["EntityRemove"] = json!({"source_base": {}});
        operation
    };
    let patch_with_body = {
        let mut operation = patch_operation();
        operation["body"] = json!("fn run() {}");
        operation
    };
    let update_without_body = json!({
        "verb": "update", "target": uuid, "description": "Rewrite",
        "payload": {"EntitySourceBase": {"schema": "kin.entity.source_base.v1"}}
    });
    let probe = |tool, call, fix, schema_refuses| Probe {
        tool,
        call,
        fix,
        schema_refuses,
    };
    vec![
        // Budgets.
        probe(
            REFS,
            json!({"query": "run", "max_output_bytes": -5}),
            "max_output_bytes must be from 2000 to 60000",
            true,
        ),
        probe(
            REFS,
            json!({"query": "run", "max_output_bytes": 10}),
            "max_output_bytes must be from 2000 to 60000",
            true,
        ),
        probe(
            LOCATE,
            json!({"query": "run", "max_output_bytes": "big"}),
            "max_output_bytes must be a whole number",
            true,
        ),
        probe(
            CONTEXT,
            json!({"question": "q", "token_budget": -1}),
            "token_budget must be from 0 to 200000",
            true,
        ),
        // Enumerations.
        probe(
            REFS,
            json!({"entity_id": uuid, "view": "subclasses"}),
            "view must be one of references, overrides",
            true,
        ),
        probe(
            NEIGHBORS,
            json!({"entity_id": uuid, "direction": "sideways"}),
            "direction must be one of in, out, both",
            true,
        ),
        probe(
            TRACE,
            json!({"focal": "run", "direction": "up"}),
            "direction must be one of calls, callers, both",
            true,
        ),
        probe(
            REFS,
            json!({"query": "run", "relation_kinds": ["inherits"]}),
            "relation_kinds[0] must be one of calls, imports, references",
            true,
        ),
        probe(
            LOCATE,
            json!({"query": "run", "mode": "fuzzy"}),
            "mode must be one of auto, name, description, text",
            true,
        ),
        // Bounds.
        probe(
            NEIGHBORS,
            json!({"entity_id": uuid, "depth": -1}),
            "depth must be from 1 to 8",
            true,
        ),
        probe(
            NEIGHBORS,
            json!({"entity_id": uuid, "limit": 0}),
            "limit must be from 1 to 2000",
            true,
        ),
        probe(
            TRACE,
            json!({"focal": "run", "depth": 99}),
            "depth must be from 1 to 8",
            true,
        ),
        probe(
            HISTORY,
            json!({"entity_id": uuid, "offset": -1}),
            "offset must be at least 0",
            true,
        ),
        probe(CONTEXT, json!({"entities": []}), "entities is empty", true),
        // Selectors.
        probe(
            REFS,
            json!({"entity_id": uuid, "query": "run"}),
            "Send only one of entity_id, or query",
            false,
        ),
        probe(
            REFS,
            json!({}),
            "needs entity_id, or query, or cursor to continue a page",
            false,
        ),
        probe(
            CONTEXT,
            json!({"depth": 2}),
            "needs entity_id, entities, or question",
            false,
        ),
        probe(
            IMPACT,
            json!({"entity_ids": [uuid], "change_ids": ["abc"]}),
            "Send only one of entity_ids, change_ids, or base and head",
            false,
        ),
        probe(
            IMPACT,
            json!({"base": "abc"}),
            "base needs head as well",
            false,
        ),
        probe(
            IMPACT,
            json!({}),
            "needs entity_ids, change_ids, or base and head, or cursor to continue a page",
            false,
        ),
        probe(
            TRACE,
            json!({"direction": "calls"}),
            "needs focal, or cursor to continue a page",
            false,
        ),
        probe(
            TRACE,
            json!({"cursor": "abc", "to": "main"}),
            "cursor and to cannot be combined",
            false,
        ),
        probe(SOURCE, json!({}), "needs entity_id", true),
        // Values copied back from a reply. The server holds their length; an
        // agent profile does not advertise it, since no caller chooses it.
        probe(
            REFS,
            json!({"cursor": "c".repeat(257)}),
            "cursor is 257 bytes, over its 256-byte limit",
            false,
        ),
        probe(
            REFS,
            json!({"entity_id": uuid, "proof_for_caller": format!("{uuid}-x")}),
            "proof_for_caller is 38 bytes, over its 36-byte limit",
            false,
        ),
        // Aliases that disagree.
        probe(
            REFS,
            json!({"query": "run", "max_chars": 3000, "max_output_bytes": 4000}),
            "Send only max_output_bytes",
            false,
        ),
        // Writes.
        probe(
            MUTATE,
            json!({"operations": []}),
            "operations is empty",
            true,
        ),
        probe(
            MUTATE,
            json!({"operations": [mismatched]}),
            "payload EntityCreate goes with verb create",
            true,
        ),
        probe(
            MUTATE,
            json!({"operations": [two_payloads]}),
            "payload takes exactly one of",
            true,
        ),
        probe(
            MUTATE,
            json!({"operations": [patch_with_body]}),
            "EntitySourcePatch takes no body",
            false,
        ),
        probe(
            MUTATE,
            json!({"operations": [update_without_body]}),
            "needs body",
            true,
        ),
        probe(
            MUTATE,
            json!({"operations": [{"verb": "patch", "target": uuid, "payload": {}}]}),
            "each operation needs description",
            true,
        ),
        probe(
            MUTATE,
            json!({"operations": [patch_operation()], "transaction_id": "t", "summary": "s"}),
            "transaction_id and summary cannot be combined",
            false,
        ),
        probe(
            TRANSACTION,
            json!({"action": "stage", "transaction_id": "t", "operations": [mismatched]}),
            "payload EntityCreate goes with verb create",
            false,
        ),
    ]
}

#[test]
fn every_invalid_probe_is_refused_with_the_fix() {
    let mut problems = Vec::new();
    for probe in probes() {
        let mut name = probe.tool.to_string();
        let mut call = arguments(probe.call.clone());
        match crate::verbs::resolve(&mut name, &mut call) {
            Ok(()) => problems.push(format!("{} {} was accepted", probe.tool, probe.call)),
            Err(refusal) => {
                if !refusal.contains(probe.fix) {
                    problems.push(format!(
                        "{} {}: the refusal does not say `{}`: {refusal}",
                        probe.tool, probe.call, probe.fix
                    ));
                }
                if !refusal.contains("Valid form:") {
                    problems.push(format!(
                        "{} {}: the refusal ends with no call that works: {refusal}",
                        probe.tool, probe.call
                    ));
                }
            }
        }
        // The profile that serves the verb must refuse it too wherever a flat
        // schema can say the rule. `transaction` stages on the full profile.
        let schema = if probe.tool == TRANSACTION {
            served_schema(TRANSACTION, Profile::Full, &|_| None).unwrap()
        } else {
            served(probe.tool)
        };
        let accepted = schema_accepts(&schema, &probe.call);
        if probe.schema_refuses && accepted {
            problems.push(format!(
                "{} {}: the served schema accepts it",
                probe.tool, probe.call
            ));
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn every_valid_call_the_probes_mirror_is_accepted() {
    let uuid = "00000000-0000-4000-8000-000000000001";
    for (tool, call) in [
        (REFS, json!({"query": "run", "max_output_bytes": 4000})),
        (REFS, json!({"query": "run", "max_chars": 4000})),
        (
            REFS,
            json!({"query": "run", "max_chars": 4000, "max_output_bytes": 4000}),
        ),
        (
            REFS,
            json!({"query": "run", "relation_kinds": ["Calls", "refs"]}),
        ),
        (REFS, json!({"cursor": "abc"})),
        (REFS, json!({"cursor": "abc", "query": "run"})),
        (
            NEIGHBORS,
            json!({"entity_id": uuid, "direction": "dependents"}),
        ),
        (
            TRACE,
            json!({"focal": "run", "direction": "callee", "max_response_chars": 5000}),
        ),
        (TRACE, json!({"cursor": "abc"})),
        (TRACE, json!({"from": "run", "to": "main"})),
        (IMPACT, json!({"cursor": "abc"})),
        (IMPACT, json!({"base": "a", "head": "b"})),
        (
            CONTEXT,
            json!({"entities": ["run"], "question": "how does it retry"}),
        ),
        (LOCATE, json!({"literal": "retry_after"})),
        (LOCATE, json!({"cursor": "abc"})),
        (STATUS, json!({})),
        (MUTATE, json!({"operations": [patch_operation()]})),
        (
            MUTATE,
            json!({"operations": [patch_operation()], "transaction_id": "t"}),
        ),
        (
            TRANSACTION,
            json!({"action": "stage", "transaction_id": "t", "operations": [patch_operation()]}),
        ),
        (SESSION, json!({"action": "end", "session_id": "s"})),
    ] {
        let mut name = tool.to_string();
        let mut call_arguments = arguments(call.clone());
        crate::verbs::resolve(&mut name, &mut call_arguments)
            .unwrap_or_else(|refusal| panic!("{tool} {call} was refused: {refusal}"));
    }
}

#[test]
fn a_checked_call_reaches_its_handler_under_the_names_the_handler_reads() {
    let resolved = |tool: &str, call: Value| {
        let mut name = tool.to_string();
        let mut call = arguments(call);
        crate::verbs::resolve(&mut name, &mut call).unwrap();
        (name, call)
    };
    let (name, call) = resolved(REFS, json!({"query": "run", "max_output_bytes": 4000}));
    assert_eq!(name, "find_references");
    assert_eq!(call["max_chars"], 4000);
    assert!(!call.contains_key("max_output_bytes"));

    let (name, call) = resolved(
        TRACE,
        json!({"focal": "run", "max_chars": 5000, "direction": "Outgoing"}),
    );
    assert_eq!(name, "trace_data_flow");
    assert_eq!(
        call["max_response_chars"], 5000,
        "the walk reads this spelling"
    );
    assert_eq!(call["direction"], "calls");

    let (name, call) = resolved(
        NEIGHBORS,
        json!({"entity_id": "x", "direction": " DEPENDENTS "}),
    );
    assert_eq!(name, "graph_neighborhood");
    assert_eq!(call["direction"], "in");

    let (name, call) = resolved(
        MUTATE,
        json!({"operations": [patch_operation()], "transaction_id": "t"}),
    );
    assert_eq!(
        name, "kin_transaction_stage",
        "a transaction_id stages instead of committing"
    );
    assert_eq!(
        call["operations"][0],
        patch_operation(),
        "operations pass through as sent"
    );

    let mut retired = "exec".to_string();
    assert!(crate::verbs::resolve(&mut retired, &mut arguments(json!({"argv":["go"]}))).is_err());
}

/// A value of `field`'s type that keeps its bounds.
fn sample(field: &Field) -> Value {
    fn of(ty: &Ty, field: &Field) -> Value {
        match ty {
            Ty::String => json!("x".repeat(field.min_length.unwrap_or(1).max(1) as usize)),
            Ty::Integer => json!(field.minimum.unwrap_or(1).max(0)),
            Ty::Boolean => json!(true),
            Ty::Object => json!({}),
            Ty::StringMap => json!({"KEY": "value"}),
            Ty::Enum(spec) => json!(spec.values[0]),
            Ty::Nested(_) if field.name == "capabilities" => json!({"can_write": true}),
            Ty::Nested(_) => patch_operation(),
            Ty::Any => json!({"any": "value"}),
            Ty::Array(items) => {
                let count = field.min_items.unwrap_or(1).max(1) as usize;
                Value::Array((0..count).map(|_| of(items, field)).collect())
            }
        }
    }
    of(&field.ty, field)
}

#[test]
fn every_declared_field_is_a_field_of_the_request_type() {
    // The struct and the field list come from one declaration, and this holds
    // the part a macro cannot: that each field's served type is one its Rust
    // type decodes, and that the decoded request serializes back unchanged.
    fn round_trip<R: TypedRequest>() {
        let fields = R::fields();
        let fixture: serde_json::Map<String, Value> = fields
            .iter()
            .map(|field| (field.name.to_string(), sample(field)))
            .collect();
        let request: R = serde_json::from_value(Value::Object(fixture.clone()))
            .unwrap_or_else(|error| panic!("{}: the fixture does not decode: {error}", R::NAME));
        let back = serde_json::to_value(&request).unwrap();
        assert_eq!(back, Value::Object(fixture), "{}", R::NAME);
    }
    round_trip::<LocateRequest>();
    round_trip::<RefsRequest>();
    round_trip::<TraceRequest>();
    round_trip::<ContextRequest>();
    round_trip::<SourceRequest>();
    round_trip::<NeighborsRequest>();
    round_trip::<ImpactRequest>();
    round_trip::<StatusRequest>();
    round_trip::<HistoryRequest>();
    round_trip::<MutateRequest>();
    round_trip::<SessionRequest>();
    round_trip::<TransactionRequest>();
    round_trip::<Capabilities>();
}

/// Every served description, at any depth, removed.
fn without_descriptions(mut schema: Value) -> Value {
    fn strip(value: &mut Value) {
        match value {
            Value::Object(object) => {
                object.remove("description");
                object.values_mut().for_each(strip);
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    strip(&mut schema);
    schema
}

#[test]
fn every_served_verb_schema_is_generated_from_its_request_type() {
    let profiles: [(&str, Option<&[&str]>, bool); 4] = [
        (
            "agent-default",
            Some(crate::tools::agent_default_tool_names()),
            true,
        ),
        (
            "agent-query",
            Some(crate::tools::agent_query_tool_names()),
            true,
        ),
        (
            "agent-search",
            Some(crate::tools::agent_search_tool_names()),
            true,
        ),
        ("full", None, false),
    ];
    let mut checked = 0;
    for (profile, names, belt) in profiles {
        let allowed = names.map(crate::tools::name_set);
        let listing = crate::tools::served_tools_list(allowed.as_ref(), belt);
        for tool in listing.tools {
            if !crate::verbs::is_verb(&tool.name) {
                continue;
            }
            assert!(
                is_typed(&tool.name),
                "{profile}: {} has no typed request",
                tool.name
            );
            if belt {
                let mut expected = served_schema(&tool.name, Profile::Agent, &|_| None).unwrap();
                crate::agent_belt::apply_belt_schema_defaults(&tool.name, &mut expected);
                assert_eq!(tool.input_schema, expected, "{profile}: {}", tool.name);
            } else {
                let expected = served_schema(&tool.name, Profile::Full, &|_| None).unwrap();
                assert_eq!(
                    without_descriptions(tool.input_schema.clone()),
                    without_descriptions(expected),
                    "{profile}: {}",
                    tool.name
                );
            }
            checked += 1;
        }
    }
    assert!(
        checked >= 12 * 2,
        "only {checked} served verbs were checked"
    );
}

#[test]
fn every_agent_field_is_served_and_every_other_is_accepted() {
    for tool in TYPED_TOOLS {
        let agent = served_schema(tool, Profile::Agent, &|_| None).unwrap();
        let full = served_schema(tool, Profile::Full, &|_| None).unwrap();
        for field in declared_fields(tool).unwrap() {
            let on_agent = agent["properties"].get(field.name).is_some();
            let on_full = full["properties"].get(field.name).is_some();
            match field.visibility {
                Visibility::Agent => assert!(on_agent && on_full, "{tool}.{}", field.name),
                Visibility::Full => assert!(!on_agent && on_full, "{tool}.{}", field.name),
                Visibility::Hidden => assert!(!on_agent && !on_full, "{tool}.{}", field.name),
            }
        }
    }
}

#[test]
fn every_payload_variant_is_a_form_or_named_uncommittable() {
    use crate::session::{operation_form, McpMutationPayload, OPERATION_FORMS, PAYLOAD_VARIANTS};
    for variant in PAYLOAD_VARIANTS {
        assert!(
            operation_form(variant).is_some() || variant == "Blob",
            "{variant} is neither a served form nor the one payload no route commits"
        );
    }
    for form in OPERATION_FORMS {
        assert!(PAYLOAD_VARIANTS.contains(&form.payload), "{}", form.payload);
    }
    // The wire name of a decoded payload is the one the forms table keys on.
    let blob: McpMutationPayload = serde_json::from_value(json!({"Blob": [1]})).unwrap();
    assert_eq!(blob.variant(), "Blob");
}

#[test]
fn the_registered_operation_contract_pairs_each_payload_with_the_forms_verbs() {
    // The full profile serves the registered contract, written out with every
    // nested base schema. Its branches must pair each payload with exactly the
    // verbs the backend's forms table does.
    let registered = crate::tools::transaction_operation_schema();
    let branches = registered["oneOf"].as_array().unwrap();
    let carried = |branch: &Value| -> Vec<String> {
        let payload = &branch["properties"]["payload"];
        let mut names: Vec<String> = payload["properties"]
            .as_object()
            .map(|object| object.keys().cloned().collect())
            .unwrap_or_default();
        for alternative in payload["oneOf"].as_array().into_iter().flatten() {
            names.extend(
                alternative["properties"]
                    .as_object()
                    .into_iter()
                    .flat_map(|object| object.keys().cloned()),
            );
        }
        names
    };
    for form in crate::session::OPERATION_FORMS {
        let branch = branches
            .iter()
            .find(|branch| {
                branch["properties"]["payload"]["properties"]
                    .get(form.payload)
                    .is_some()
                    || branch["properties"]["payload"]["oneOf"]
                        .as_array()
                        .is_some_and(|alternatives| {
                            alternatives.iter().any(|alternative| {
                                alternative["properties"].get(form.payload).is_some()
                            })
                        })
            })
            .unwrap_or_else(|| panic!("no registered branch carries {}", form.payload));
        let verbs: Vec<&str> = branch["properties"]["verb"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            verbs.contains(&form.verbs[0]),
            "{}: the served verb {} is not in {verbs:?}",
            form.payload,
            form.verbs[0]
        );
        // A branch that carries several payloads offers the union of their
        // verbs; none of them may be one the backend refuses for all of them.
        let payloads = carried(branch);
        for verb in verbs {
            assert!(
                crate::session::OPERATION_FORMS
                    .iter()
                    .filter(|other| payloads.iter().any(|name| name == other.payload))
                    .any(|other| other.verbs.contains(&verb)),
                "{}: the registered contract offers {verb}, which the backend refuses",
                form.payload
            );
        }
    }
}

#[test]
fn no_served_verb_schema_opens_with_a_combinator() {
    for tool in TYPED_TOOLS {
        for profile in [Profile::Agent, Profile::Full] {
            let schema = served_schema(tool, profile, &|_| None).unwrap();
            assert!(
                crate::input_contract::top_level_combinators(&schema).is_empty(),
                "{tool}: a top-level combinator drops the tool on provider APIs"
            );
            assert_eq!(schema["type"], "object", "{tool}");
        }
    }
}

/// A value a caller copies back from a reply loses only its length bound, and
/// only on the agent profiles: every call is still held to it, the `full`
/// profile still serves it, and every bound a caller's own choice can break is
/// served as before.
#[test]
fn only_values_copied_from_a_reply_drop_their_bound_from_the_agent_listing() {
    let mut copied = 0;
    for tool in TYPED_TOOLS {
        let agent = served_schema(tool, Profile::Agent, &|_| None).unwrap();
        let full = served_schema(tool, Profile::Full, &|_| None).unwrap();
        for field in declared_fields(tool).unwrap() {
            let Some(maximum) = field.max_length else {
                continue;
            };
            if field.visibility == Visibility::Full {
                assert_eq!(full["properties"][field.name]["maxLength"], maximum);
            }
            if field.visibility != Visibility::Agent {
                continue;
            }
            let served = &agent["properties"][field.name];
            assert_eq!(full["properties"][field.name]["maxLength"], maximum);
            if field.from_reply {
                copied += 1;
                // Only a handle or a returned UUID: a value a caller writes
                // keeps its bound wherever it is served.
                assert!(
                    field.name == "cursor"
                        || field.name.ends_with("_cursor")
                        || field.doc.contains("UUID"),
                    "{tool}.{} is marked as copied from a reply and is neither a cursor nor \
                     a returned UUID",
                    field.name
                );
                assert!(served.get("maxLength").is_none(), "{tool}.{}", field.name);
            } else {
                assert_eq!(served["maxLength"], maximum, "{tool}.{}", field.name);
            }
        }
    }
    // impact, refs, status and trace cursors, status's review-flags cursor,
    // and the refs receiver UUID.
    assert_eq!(copied, 6, "the sweep did not reach the copied values");
}

/// `trace`'s `scope` says what to pass where an agent fills it in: a dotted
/// module prefix as reply rows spell `module`, never a path. Agents under-use
/// it, so its guidance is held rather than left to a later cut.
///
/// The whole schema is held too. The description grew to say that one call
/// returns every direct and indirect caller, and `details` and `scope` gave
/// up the same number of characters, so a turn costs no more than before.
#[test]
fn trace_scope_is_served_with_the_form_it_takes() {
    let listing = served_agent_default();
    let trace = listing
        .tools
        .iter()
        .find(|tool| tool.name == TRACE)
        .expect("agent-default serves trace");
    assert!(trace.description.contains("scope"), "{}", trace.description);
    let said = trace.input_schema["properties"]["scope"]["description"]
        .as_str()
        .expect("scope is served with its form");
    assert!(
        said.contains("Dotted") && said.contains("`module`"),
        "scope must say it takes a dotted module prefix as rows spell it: {said}"
    );
    for word in ["path", "file", "/"] {
        assert!(
            !said.contains(word),
            "scope must not steer toward a path: {said}"
        );
    }
    let details = trace.input_schema["properties"]["details"]["description"]
        .as_str()
        .unwrap();
    let before = "X's call chain/reach? to: route; scope: in module.".len()
        + "Full rows; same steps.".len()
        + "Dotted module prefix from a row's `module`.".len();
    assert!(
        trace.description.len() + details.len() + said.len() <= before,
        "{} + {} + {} > {before}",
        trace.description.len(),
        details.len(),
        said.len()
    );
    check_call(
        TRACE,
        &mut arguments(json!({"focal": "run", "scope": "flask.sansio"})),
    )
    .unwrap();
}

/// A context call names up to sixteen entities. One more is refused with
/// the number it takes, so the next call fits.
#[test]
fn context_takes_sixteen_entities_and_says_so_past_them() {
    let named = |count: usize| -> HashMap<String, Value> {
        let names: Vec<String> = (0..count).map(|index| format!("entity_{index}")).collect();
        HashMap::from([("entities".to_string(), json!(names))])
    };
    for count in [1, 9, CONTEXT_ENTITIES_MAX as usize] {
        let mut arguments = named(count);
        check_call(CONTEXT, &mut arguments).unwrap_or_else(|refusal| panic!("{count}: {refusal}"));
        assert_eq!(arguments["entities"].as_array().unwrap().len(), count);
    }
    let refusal = check_call(CONTEXT, &mut named(17)).unwrap_err();
    assert!(
        refusal.contains("entities has 17; it takes at most 16."),
        "{refusal}"
    );
}
