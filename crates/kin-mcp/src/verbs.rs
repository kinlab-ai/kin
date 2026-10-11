// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Plain verbs for the agent surface.
//!
//! An agent is served thirteen tools named the way the `kin` CLI names the same
//! capability: `locate`, `refs`, `trace`, `context`, `source`, `neighbors`,
//! `impact`, `status`, `history`, `init`, `mutate`, `session` and
//! `transaction`. Each takes the arguments of the tool it replaces, and a
//! question that used to need a choice between tools is an option on one verb
//! with a default: `locate` reads a query by name, by description or as literal
//! text through `mode`, `trace` walks from one entity or, given `to`, finds the
//! route between two, and `session` and `transaction` take an `action`.
//!
//! The verbs are a naming and routing layer and nothing else. Every verb call is
//! rewritten by [`resolve`], once, at the point a server parses the call, into
//! the registered tool that answers it, with the arguments that tool takes.
//! Everything keyed on a tool name after that point, the belt defaults, the
//! response budget, the negative-evidence spec and the `_kin` envelope, sees
//! the same name it always saw, so a verb's answer is the named tool's answer.
//!
//! The names the verbs replace stay accepted for one version as hidden aliases:
//! a call by the old name reaches the same handler, and no verb-speaking
//! listing advertises it. The `benchmark` and `context-bench` profiles name the
//! old tools explicitly and are served them unchanged, because their bytes are
//! an input to a citable result.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

use crate::types::{ToolAnnotations, ToolDefinition};

pub const LOCATE: &str = "locate";
pub const REFS: &str = "refs";
pub const TRACE: &str = "trace";
pub const CONTEXT: &str = "context";
pub const SOURCE: &str = "source";
pub const NEIGHBORS: &str = "neighbors";
pub const IMPACT: &str = "impact";
pub const STATUS: &str = "status";
pub const HISTORY: &str = "history";
pub const INIT: &str = "init";
pub const MUTATE: &str = "mutate";
/// Retired as a verb: a write opens its connection's own session, and the
/// toolchain run is [`EXEC`]. Accepted as a hidden alias, never listed.
pub const SESSION: &str = "session";
pub const EXEC: &str = "exec";
pub const TRANSACTION: &str = "transaction";

/// Every verb, in name order.
pub const VERBS: [&str; 12] = [
    CONTEXT,
    HISTORY,
    IMPACT,
    LOCATE,
    MUTATE,
    NEIGHBORS,
    REFS,
    SESSION,
    SOURCE,
    STATUS,
    TRACE,
    TRANSACTION,
];

/// Every name a verb replaced, with the verb that serves it now.
///
/// Each one is accepted by `tools/call` for one version and routed to the
/// handler it always reached. None is listed by a profile that speaks verbs.
pub const ALIASES: [(&str, &str); 20] = [
    ("semantic_search", LOCATE),
    ("semantic_locate", LOCATE),
    (crate::handlers::lexical::TOOL_NAME, LOCATE),
    ("find_references", REFS),
    ("trace_data_flow", TRACE),
    (crate::handlers::path::TOOL_NAME, TRACE),
    ("get_context_pack", CONTEXT),
    ("get_entity_source", SOURCE),
    ("graph_neighborhood", NEIGHBORS),
    ("impact_analysis", IMPACT),
    ("kin_graph_status", STATUS),
    ("kin_provenance_query", HISTORY),
    ("kin_mutate", MUTATE),
    // The session verb and the tools it folded are accepted wherever the
    // toolchain run is served, and listed nowhere.
    ("kin_session_start", SESSION),
    ("kin_session_heartbeat", SESSION),
    ("kin_session_end", SESSION),
    ("kin_transaction_begin", TRANSACTION),
    ("kin_transaction_stage", TRANSACTION),
    ("kin_transaction_commit", TRANSACTION),
    ("kin_transaction_abort", TRANSACTION),
];

/// The verbs that answer through exactly one registered tool, with that tool.
const RENAMES: [(&str, &str); 8] = [
    (REFS, "find_references"),
    (CONTEXT, "get_context_pack"),
    (SOURCE, "get_entity_source"),
    (NEIGHBORS, "graph_neighborhood"),
    (IMPACT, "impact_analysis"),
    (STATUS, "kin_graph_status"),
    (HISTORY, "kin_provenance_query"),
    (MUTATE, "kin_mutate"),
];

/// Each verb's title, what a client menu shows a person. Distinct from the
/// title of the tool it replaced, which stays registered beside it.
fn title_of(verb: &str) -> &'static str {
    match verb {
        LOCATE => "Locate code",
        REFS => "References",
        TRACE => "Trace calls and routes",
        CONTEXT => "Context around entities",
        SOURCE => "Entity source",
        NEIGHBORS => "Neighbors",
        IMPACT => "Change impact",
        STATUS => "Status",
        HISTORY => "Change history",
        INIT => "Set up a repository",
        MUTATE => "Mutate entities",
        EXEC => "Toolchain run",
        SESSION => "Agent session",
        _ => "Staged transaction",
    }
}

/// `locate`'s modes, in the order a refusal names them.
pub const LOCATE_MODES: [&str; 4] = ["auto", "name", "description", "text"];

/// `session`'s actions, the default first.
pub const SESSION_ACTIONS: [&str; 3] = ["start", "heartbeat", "end"];

/// `transaction`'s actions, the default first.
pub const TRANSACTION_ACTIONS: [&str; 4] = ["begin", "stage", "commit", "abort"];

/// Whether `name` is one of the verbs.
pub fn is_verb(name: &str) -> bool {
    VERBS.contains(&name)
}

/// Whether `name` is a tool whose name is a plain word, which prose uses
/// without naming the tool: every verb, the retired `session`, and the front
/// door `ask`. "The session it opened" or "ask again in other words" names no
/// tool, so such a name counts as a mention only in backticks. Like a verb,
/// `ask` is a routed command and a registered tool by the same name, and
/// `call` runs it by that name.
pub fn is_plain_word(name: &str) -> bool {
    is_verb(name) || name == SESSION || name == crate::ask::TOOL_NAME
}

/// The verb that serves `name` now, when `name` is a name a verb replaced.
pub fn verb_for(name: &str) -> Option<&'static str> {
    ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map(|(_, verb)| *verb)
}

/// Whether `name` is a hidden alias: accepted, routed, never listed where the
/// verbs are.
pub fn is_alias(name: &str) -> bool {
    verb_for(name).is_some()
}

/// The registered tools one verb answers through.
pub fn handlers_of(verb: &str) -> Vec<&'static str> {
    ALIASES
        .iter()
        .filter(|(_, served_by)| *served_by == verb)
        .map(|(alias, _)| *alias)
        .collect()
}

/// Whether a profile serving `allowed` serves a call to `name`: by its own
/// name, or, for a hidden alias, because the profile serves its verb.
///
/// A profile's name set holds exactly what it lists, so this is the one place
/// an old name is admitted wherever the verb that replaced it is served.
pub fn allows(allowed: &HashSet<String>, name: &str) -> bool {
    allowed.contains(name) || verb_for(name).is_some_and(|verb| allowed.contains(verb))
}

/// Whether a listing of `allowed` speaks verbs. Every profile does except one
/// that names an old tool explicitly, which only the citable profiles do.
pub fn speaks_verbs(allowed: Option<&HashSet<String>>) -> bool {
    allowed.is_none_or(|allowed| !allowed.iter().any(|name| is_alias(name)))
}

/// Rewrite one call by a verb into the registered call that answers it.
///
/// Any other name is left alone, except the declaration filter's old served
/// name, which [`crate::agent_belt::canonicalize_tool_name`] maps as before.
/// Called once, where a server parses a `tools/call`, so everything keyed on a
/// tool name after it sees the registered one. A verb call that names no valid
/// form is refused with a sentence that says what the valid form is.
pub fn resolve(name: &mut String, arguments: &mut HashMap<String, Value>) -> Result<(), String> {
    crate::agent_belt::canonicalize_tool_name(name);
    if let Some(message) = crate::tools::retired_file_operation(name) {
        return Err(message.into());
    }
    if name == SESSION
        && (arguments.contains_key("argv")
            || arguments
                .get("action")
                .and_then(Value::as_str)
                .is_some_and(|action| action.eq_ignore_ascii_case("exec")))
    {
        return Err(crate::tools::retired_file_operation(EXEC).unwrap().into());
    }
    // Every verb call is held to the typed request its schema is served from,
    // before anything routes it, and reaches its handler as that request.
    crate::requests::check_call(name, arguments)?;
    let target = match name.as_str() {
        LOCATE => resolve_locate(arguments)?,
        TRACE => resolve_trace(arguments)?,
        SESSION => resolve_session(arguments)?,
        TRANSACTION => resolve_transaction(arguments)?,
        MUTATE if supplied(arguments, "transaction_id") => "kin_transaction_stage",
        other => match RENAMES.iter().find(|(verb, _)| *verb == other) {
            Some((_, tool)) => tool,
            None => return Ok(()),
        },
    };
    name.clear();
    name.push_str(target);
    Ok(())
}

/// The registered tool a verb call would reach, without rewriting anything.
/// For a check that only needs the name, such as whether a call reads the
/// graph; any other name is returned unchanged.
pub fn handler_name(name: &str, arguments: &HashMap<String, Value>) -> String {
    let mut name = name.to_string();
    let mut arguments = arguments.clone();
    let _ = resolve(&mut name, &mut arguments);
    name
}

/// The refusal a malformed verb call gets: what was wrong, and one call that
/// works.
fn refusal(verb: &str, problem: &str, example: Value) -> String {
    format!("{verb}: {problem} Valid form: {verb} {example}")
}

fn supplied(arguments: &HashMap<String, Value>, field: &str) -> bool {
    arguments.get(field).is_some_and(|value| !value.is_null())
}

/// Remove an option field and read it as a string, `None` when absent or null.
fn take_option(
    verb: &str,
    field: &str,
    arguments: &mut HashMap<String, Value>,
    valid: &[&str],
    example: Value,
) -> Result<Option<String>, String> {
    match arguments.remove(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if valid.contains(&text.trim()) => {
            Ok(Some(text.trim().to_string()))
        }
        Some(other) => Err(refusal(
            verb,
            &format!(
                "{field} must be one of {}, and it was {other}.",
                valid.join(", ")
            ),
            example,
        )),
    }
}

/// Whether a query reads as a name rather than a description: one identifier,
/// or identifiers joined by `.` or `::`, as in `parse_config`,
/// `app.run` or `Scheduler::advance_timers`.
pub fn looks_like_a_name(query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return false;
    }
    let is_identifier = |segment: &str| {
        let mut characters = segment.chars();
        characters
            .next()
            .is_some_and(|first| first.is_alphabetic() || first == '_' || first == '$')
            && characters.all(|rest| rest.is_alphanumeric() || rest == '_' || rest == '$')
    };
    query
        .split("::")
        .flat_map(|part| part.split('.'))
        .all(is_identifier)
}

/// Whether `cursor` is the page token a literal lookup hands back.
///
/// Only a description page and a literal page have a cursor, a name search
/// does not page, so a cursor sent without a mode continues the lookup whose
/// token it is.
fn is_literal_page_token(cursor: &str) -> bool {
    crate::handlers::lexical::is_literal_cursor(cursor)
}

fn resolve_locate(arguments: &mut HashMap<String, Value>) -> Result<&'static str, String> {
    let example = json!({"query": "where failed requests are retried"});
    let mode = take_option(
        LOCATE,
        "mode",
        arguments,
        &LOCATE_MODES,
        json!({"query": "parse_config", "mode": "name"}),
    )?;
    // The old literal field starts the same exact text search. A continuation
    // keeps the cursor's held-text or stored-field scope, including old pages.
    if !supplied(arguments, "query") {
        if let Some(literal) = arguments.remove("literal").filter(|value| !value.is_null()) {
            arguments.insert("query".into(), literal);
            if mode.is_none() && !supplied(arguments, "cursor") {
                return finish_text(arguments);
            }
        }
    }
    let query = arguments
        .get("query")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| refusal(LOCATE, "query must be a string.", example.clone()))
        })
        .transpose()?;
    let cursor = arguments
        .get("cursor")
        .and_then(Value::as_str)
        .map(str::to_string);
    if query.is_none() && cursor.is_none() {
        return Err(refusal(
            LOCATE,
            "needs query, or cursor to continue a page.",
            example,
        ));
    }
    let handler = match mode.as_deref().unwrap_or("auto") {
        "name" => {
            if cursor.is_some() {
                return Err(refusal(
                    LOCATE,
                    "a name search returns one page and takes no cursor; raise limit instead.",
                    json!({"query": "parse_config", "mode": "name", "limit": 50}),
                ));
            }
            if query.is_none() {
                return Err(refusal(
                    LOCATE,
                    "mode name needs query.",
                    json!({"query": "parse_config", "mode": "name"}),
                ));
            }
            Ok("semantic_search")
        }
        "description" => Ok("semantic_locate"),
        "text" => finish_text(arguments),
        _ => match (&cursor, &query) {
            (Some(cursor), _) if is_literal_page_token(cursor) => finish_literal(arguments),
            (Some(_), _) => Ok("semantic_locate"),
            (None, Some(query))
                if looks_like_a_name(query)
                    || crate::file_coordinate_query::looks_like_coordinate(query) =>
            {
                Ok("semantic_search")
            }
            _ => Ok("semantic_locate"),
        },
    }?;
    if handler != crate::handlers::lexical::TOOL_NAME {
        if let Some(field) = ["pattern", "ignore_case"]
            .into_iter()
            .find(|field| supplied(arguments, field))
        {
            return Err(refusal(
                LOCATE,
                &format!("{field} is only for mode text."),
                json!({"query": "retry", "mode": "text"}),
            ));
        }
    }
    // Text rows are every match, not a ranking, so they have no relevance
    // order to re-sort.
    if handler == crate::handlers::lexical::TOOL_NAME
        && supplied(arguments, crate::graph_position::SORT_ARGUMENT)
    {
        return Err(refusal(
            LOCATE,
            "sort re-orders a ranking; mode text lists every match. Use scope or kind to narrow it.",
            json!({"query": "retry", "mode": "text", "scope": "flask.sessions"}),
        ));
    }
    // A continuation pages the rows its search chose, in the order it chose.
    if cursor.is_some() {
        if let Some(field) = [
            crate::trace_scope::SCOPE_ARGUMENT,
            crate::graph_position::SORT_ARGUMENT,
        ]
        .into_iter()
        .find(|field| supplied(arguments, field))
        {
            return Err(refusal(
                LOCATE,
                &format!(
                    "a cursor keeps the {field} its search began with; send cursor alone, or start a new search."
                ),
                json!({"cursor": "<next_cursor>"}),
            ));
        }
    }
    Ok(handler)
}

/// A text search is the literal lookup reading held module text: it takes
/// its text as `literal` and says it reads held text.
fn finish_text(arguments: &mut HashMap<String, Value>) -> Result<&'static str, String> {
    arguments.insert(
        crate::handlers::lexical::HELD_TEXT_KEY.into(),
        Value::Bool(true),
    );
    finish_literal(arguments)
}

/// A literal lookup takes its text as `literal`.
fn finish_literal(arguments: &mut HashMap<String, Value>) -> Result<&'static str, String> {
    if let Some(query) = arguments.remove("query") {
        arguments.insert("literal".into(), query);
    }
    Ok(crate::handlers::lexical::TOOL_NAME)
}

fn resolve_trace(arguments: &mut HashMap<String, Value>) -> Result<&'static str, String> {
    // The route tool's own spelling of the starting end, read as `focal`.
    if !supplied(arguments, "focal") {
        if let Some(from) = arguments.remove("from").filter(|value| !value.is_null()) {
            arguments.insert("focal".into(), from);
        }
    }
    if !supplied(arguments, "focal") && !supplied(arguments, "cursor") {
        return Err(refusal(
            TRACE,
            "needs focal, the entity to start from; add to for the route to another.",
            json!({"focal": "parse_config", "to": "main"}),
        ));
    }
    if !supplied(arguments, "to") {
        arguments.remove("to");
        return Ok("trace_data_flow");
    }
    let Some(focal) = arguments.remove("focal") else {
        return Err(refusal(
            TRACE,
            "a route needs focal as well as to.",
            json!({"focal": "parse_config", "to": "main"}),
        ));
    };
    arguments.insert("from".into(), focal);
    if let Some(direction) = arguments.get_mut("direction") {
        let routed = match direction.as_str() {
            Some("calls") => Some("forward"),
            Some("callers") => Some("reverse"),
            Some("both") => Some("either"),
            _ => None,
        };
        if let Some(routed) = routed {
            *direction = json!(routed);
        }
    }
    if let Some(depth) = arguments.remove("depth") {
        arguments.entry("max_depth".into()).or_insert(depth);
    }
    for walk_only in [
        "target",
        "limit_per_step",
        "include_body",
        "compact",
        "cursor",
    ] {
        arguments.remove(walk_only);
    }
    Ok(crate::handlers::path::TOOL_NAME)
}

/// The fields each action needs, the way the registered tool requires them.
fn require(
    verb: &str,
    action: &str,
    fields: &[&str],
    arguments: &HashMap<String, Value>,
    example: Value,
) -> Result<(), String> {
    let missing: Vec<&str> = fields
        .iter()
        .copied()
        .filter(|field| !supplied(arguments, field))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(refusal(
            verb,
            &format!("action {action} needs {}.", missing.join(" and ")),
            example,
        ))
    }
}

fn resolve_session(arguments: &mut HashMap<String, Value>) -> Result<&'static str, String> {
    let start_example = json!({"vendor": "claude-code", "client_name": "Claude Code", "capabilities": {"can_write": true, "can_commit": true}});
    let action = take_option(
        SESSION,
        "action",
        arguments,
        &SESSION_ACTIONS,
        start_example.clone(),
    )?;
    let session = json!("<session_id>");
    match action.as_deref() {
        None | Some("start") => {
            if action.is_none()
                && supplied(arguments, "session_id")
                && !supplied(arguments, "vendor")
            {
                return Err(refusal(
                    SESSION,
                    "a call naming a session_id needs action heartbeat or end; start opens a new one.",
                    json!({"action": "end", "session_id": session}),
                ));
            }
            require(
                SESSION,
                "start",
                &["vendor", "client_name"],
                arguments,
                start_example,
            )?;
            Ok("kin_session_start")
        }
        Some("heartbeat") => {
            require(
                SESSION,
                "heartbeat",
                &["session_id"],
                arguments,
                json!({"action": "heartbeat", "session_id": session}),
            )?;
            Ok("kin_session_heartbeat")
        }
        Some("end") => {
            require(
                SESSION,
                "end",
                &["session_id"],
                arguments,
                json!({"action": "end", "session_id": session}),
            )?;
            Ok("kin_session_end")
        }
        _ => unreachable!("validated session action"),
    }
}

fn resolve_transaction(arguments: &mut HashMap<String, Value>) -> Result<&'static str, String> {
    let begin_example = json!({"scope": "repository"});
    let action = take_option(
        TRANSACTION,
        "action",
        arguments,
        &TRANSACTION_ACTIONS,
        begin_example.clone(),
    )?;
    let transaction = json!("<transaction_id>");
    match action.as_deref() {
        None | Some("begin") => {
            if action.is_none() && supplied(arguments, "transaction_id") {
                return Err(refusal(
                    TRANSACTION,
                    "a call naming a transaction_id needs action stage, commit or abort; begin opens a new one.",
                    json!({"action": "commit", "transaction_id": transaction}),
                ));
            }
            require(TRANSACTION, "begin", &["scope"], arguments, begin_example)?;
            Ok("kin_transaction_begin")
        }
        Some("stage") => {
            require(
                TRANSACTION,
                "stage",
                &["transaction_id", "operations"],
                arguments,
                json!({"action": "stage", "transaction_id": transaction, "operations": ["<as mutate takes them>"]}),
            )?;
            Ok("kin_transaction_stage")
        }
        Some("commit") => {
            require(
                TRANSACTION,
                "commit",
                &["transaction_id"],
                arguments,
                json!({"action": "commit", "transaction_id": transaction, "message": "one sentence for the history"}),
            )?;
            Ok("kin_transaction_commit")
        }
        _ => {
            require(
                TRANSACTION,
                "abort",
                &["transaction_id"],
                arguments,
                json!({"action": "abort", "transaction_id": transaction}),
            )?;
            Ok("kin_transaction_abort")
        }
    }
}

/// How a verb-speaking description names a tool a verb replaced.
///
/// The verb alone where the verb's default reaches the old tool, and the verb
/// with the option that reaches it otherwise.
fn spoken(alias: &str) -> Option<(&'static str, &'static str)> {
    Some(match alias {
        "semantic_search" => (LOCATE, " (mode name)"),
        "semantic_locate" => (LOCATE, ""),
        "lexical_lookup" => (LOCATE, " (mode text)"),
        "trace_data_flow" => (TRACE, ""),
        "trace_path" => (TRACE, " (with to)"),
        "kin_session_exec" => return None,
        // Retired with the session verb and listed nowhere, so prose keeps
        // naming them as they are rather than as the toolchain run.
        "kin_session_start" => (SESSION, " (action start)"),
        "kin_session_heartbeat" => (SESSION, " (action heartbeat)"),
        "kin_session_end" => (SESSION, " (action end)"),
        SESSION => return None,
        "kin_transaction_begin" => (TRANSACTION, ""),
        "kin_transaction_stage" => (TRANSACTION, " (action stage)"),
        "kin_transaction_commit" => (TRANSACTION, " (action commit)"),
        "kin_transaction_abort" => (TRANSACTION, " (action abort)"),
        other => (verb_for(other)?, ""),
    })
}

/// Pairs of old names prose joins with "or" that one verb now answers, read as
/// that verb once.
const JOINED: [(&str, &str); 4] = [
    ("semantic_search or semantic_locate", "locate"),
    ("semantic_locate or semantic_search", "locate"),
    ("`semantic_search` or `semantic_locate`", "`locate`"),
    ("`semantic_locate` or `semantic_search`", "`locate`"),
];

/// Rewrite every whole-word mention of a replaced tool name in `text` to the
/// verb that serves it, with the option that reaches it where that is not the
/// verb's default. A name in backticks keeps its backticks around the verb.
pub fn speak_verbs(text: &str) -> String {
    let mut text = text.to_string();
    for (joined, verb) in JOINED {
        if text.contains(joined) {
            text = text.replace(joined, verb);
        }
    }
    if !ALIASES
        .iter()
        .any(|(alias, _)| crate::tools::names_tool_as_a_word(&text, alias))
        && !text.contains(crate::agent_belt::DECLARATION_FILTER_ALIAS)
    {
        return text;
    }
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while !rest.is_empty() {
        let start = rest.find(is_word).unwrap_or(rest.len());
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        if rest.is_empty() {
            break;
        }
        let end = rest.find(|c: char| !is_word(c)).unwrap_or(rest.len());
        let word = &rest[..end];
        let word = if word == crate::agent_belt::DECLARATION_FILTER_ALIAS {
            "semantic_search"
        } else {
            word
        };
        match spoken(word) {
            Some((verb, option)) => {
                let quoted = out.ends_with('`') && rest[end..].starts_with('`');
                out.push_str(verb);
                if quoted && !option.is_empty() {
                    out.push('`');
                    out.push_str(option);
                    rest = &rest[end + 1..];
                    continue;
                }
                out.push_str(option);
            }
            None => out.push_str(word),
        }
        rest = &rest[end..];
    }
    out
}

/// Rewrite every description in a schema, at any depth, to speak verbs.
pub fn speak_verbs_in_schema(schema: &mut Value) {
    match schema {
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                match value {
                    Value::String(text) if key == "description" => {
                        *text = speak_verbs(text);
                    }
                    _ => speak_verbs_in_schema(value),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(speak_verbs_in_schema),
        _ => {}
    }
}

/// Every verb's registered definition, built from the registered tools it
/// answers through, which `registered` must hold.
///
/// A verb that answers through one tool is that tool under its verb, with its
/// description speaking verbs. A verb that folds several takes every property
/// any of them takes, adds the option that picks between them, and carries
/// each tool's description under the option value that reaches it, so the
/// `full` profile still serves every word.
pub fn definitions(registered: &[ToolDefinition]) -> Vec<ToolDefinition> {
    let find = |name: &str| registered.iter().find(|tool| tool.name == name);
    let mut verbs = Vec::new();
    for (verb, tool) in RENAMES {
        if let Some(definition) = find(tool) {
            verbs.push(renamed(verb, definition));
        }
    }
    if let Some(locate) = locate_definition(&find) {
        verbs.push(locate);
    }
    if let Some(trace) = trace_definition(&find) {
        verbs.push(trace);
    }
    if let Some(session) = session_definition(&find) {
        verbs.push(session);
    }
    if let Some(transaction) = transaction_definition(&find) {
        verbs.push(transaction);
    }
    for verb in &mut verbs {
        typed_input_schema(verb);
    }
    verbs
}

/// Serve `verb`'s input schema from its typed request.
///
/// The fold above decides the words: every field the typed request serves in
/// full keeps the long description the registered tool it came from carries.
/// The typed request decides everything else, the fields, their types, enums,
/// bounds and defaults, so what the `full` profile advertises is what
/// [`crate::requests::check_call`] holds a call to.
fn typed_input_schema(verb: &mut ToolDefinition) {
    let folded = verb
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let long = |field: &str| {
        folded
            .get(field)
            .and_then(|property| property.get("description"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let describe = |field: &str| {
        // A continuation needs only its handle, which the registered prose
        // predates: it told a caller to repeat the query beside the cursor.
        if field == "cursor" {
            return None;
        }
        if field != crate::request_schema::OUTPUT_BUDGET || verb.name == SESSION {
            return long(field);
        }
        let prose = long("max_chars")
            .filter(|prose| !prose.starts_with("Alias"))
            .or_else(|| long("max_response_chars"))?;
        let prose = prose
            .replace(
                "Repeat the same query with cursor to resume.",
                "Send next_cursor alone as cursor to resume.",
            )
            .replace(" max_chars is an alias.", "");
        Some(format!(
            "{} max_chars and max_response_chars are accepted as older names for it.",
            prose.trim_end()
        ))
    };
    if let Some(schema) =
        crate::requests::served_schema(&verb.name, crate::request_schema::Profile::Full, &describe)
    {
        verb.input_schema = schema;
    }
}

fn renamed(verb: &str, definition: &ToolDefinition) -> ToolDefinition {
    let mut schema = definition.input_schema.clone();
    speak_verbs_in_schema(&mut schema);
    ToolDefinition {
        name: verb.into(),
        description: speak_verbs(&definition.description),
        annotations: ToolAnnotations {
            title: title_of(verb).into(),
            ..definition.annotations.clone()
        },
        input_schema: schema,
    }
}

fn properties_of(definition: &ToolDefinition) -> Map<String, Value> {
    definition
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Fold several tools' properties into one map: the first tool to declare a
/// property decides its schema.
fn folded_properties(tools: &[&ToolDefinition]) -> Map<String, Value> {
    let mut properties = Map::new();
    for tool in tools {
        for (name, property) in properties_of(tool) {
            properties.entry(name).or_insert(property);
        }
    }
    properties
}

/// The strongest hints of several tools: a folded verb writes if any of them
/// writes, destroys if any destroys, and is idempotent only if all are.
fn folded_annotations(title: &str, tools: &[&ToolDefinition]) -> ToolAnnotations {
    ToolAnnotations {
        title: title.into(),
        read_only_hint: tools.iter().all(|tool| tool.annotations.read_only_hint),
        destructive_hint: tools.iter().any(|tool| tool.annotations.destructive_hint),
        idempotent_hint: tools.iter().all(|tool| tool.annotations.idempotent_hint),
        open_world_hint: tools.iter().any(|tool| tool.annotations.open_world_hint),
    }
}

fn folded_description(headline: &str, sections: &[(&str, &ToolDefinition)]) -> String {
    let mut description = headline.to_string();
    for (when, tool) in sections {
        description.push_str("\n\n");
        description.push_str(when);
        description.push_str(": ");
        description.push_str(&speak_verbs(&tool.description));
    }
    description
}

fn folded_schema(mut properties: Map<String, Value>, first: (&str, Value)) -> Value {
    for property in properties.values_mut() {
        speak_verbs_in_schema(property);
    }
    let mut ordered = Map::new();
    ordered.insert(first.0.to_string(), first.1);
    ordered.extend(properties);
    json!({ "type": "object", "properties": ordered })
}

fn locate_definition<'a>(
    find: &impl Fn(&str) -> Option<&'a ToolDefinition>,
) -> Option<ToolDefinition> {
    let name = find("semantic_search")?;
    let description = find("semantic_locate")?;
    let literal = find(crate::handlers::lexical::TOOL_NAME)?;
    let mut properties = folded_properties(&[description, name, literal]);
    properties.remove("literal");
    properties.insert(
        "query".into(),
        json!({
            "type": "string",
            "description": "A semantic name (`parse_config`, `app.run`), a description, or exact text, read as mode says. Optional only for cursor continuation."
        }),
    );
    properties.insert(
        "kind".into(),
        json!({
            "type": "string",
            "description": format!(
                "Entity kind filter, for mode name and text. {}",
                crate::handlers::common::SEMANTIC_SEARCH_KIND_GUIDANCE
            )
        }),
    );
    properties.insert(
        "include_tests".into(),
        json!({
            "type": "boolean",
            "description": "Text includes test-role entities by default. Description ranking keeps its usual test scope unless this option overrides it. Cursor-only calls inherit the scope."
        }),
    );
    properties.insert(
        "cursor".into(),
        json!({
            "type": "string",
            "description": "`next_cursor` from a prior description or text page: the next page of the same answer. Name searches return one page. Older literal cursors retain their original search."
        }),
    );
    let mode = json!({
        "type": "string",
        "enum": LOCATE_MODES,
        "default": "auto",
        "description": "auto: identifiers and qualified names select declarations; other text is a description. name: declarations. description: ranked behavior. text: exact, case-sensitive text in held code and unparsed text, including tests. pattern true reads a regular expression; ignore_case true ignores case."
    });
    Some(ToolDefinition {
        name: LOCATE.into(),
        description: folded_description(
            "Find code in the semantic graph with one query, read the way mode says. The \
             default, auto, reads an identifier or a dotted name (`parse_config`, `app.run`, \
             `Scheduler::advance`) as a name, and other text as a description. Read a returned \
             module UUID with source for its recorded member inventory. mode text finds exact, \
             case-sensitive text in held code and unparsed text, including tests. pattern true \
             reads a regular expression; ignore_case true ignores case. A text match is not a \
             resolved reference and does not re-verify an answer `refs` or `trace` certified.",
            &[("mode name", name), ("mode description", description)],
        ),
        annotations: folded_annotations(title_of(LOCATE), &[name, description, literal]),
        input_schema: folded_schema(properties, ("mode", mode)),
    })
}

fn trace_definition<'a>(
    find: &impl Fn(&str) -> Option<&'a ToolDefinition>,
) -> Option<ToolDefinition> {
    let walk = find("trace_data_flow")?;
    let route = find(crate::handlers::path::TOOL_NAME)?;
    let mut properties = properties_of(walk);
    let route_properties = properties_of(route);
    for field in ["max_depth", "limit"] {
        if let Some(property) = route_properties.get(field) {
            properties.insert(field.to_string(), property.clone());
        }
    }
    if let Some(property) = properties.get_mut("max_depth") {
        if let Some(object) = property.as_object_mut() {
            object.insert(
                "description".into(),
                json!("With to: hops walked between the two ends (default 6, ceiling 12). depth is read as this when it is not given."),
            );
        }
    }
    if let Some(property) = properties.get_mut("limit") {
        if let Some(object) = property.as_object_mut() {
            object.insert(
                "description".into(),
                json!("With to: routes returned, shortest first (default 3, ceiling 25)."),
            );
        }
    }
    let mut schema = folded_schema(
        properties,
        (
            "to",
            json!({
                "type": "string",
                "description": "Another entity, in the forms focal takes. With it, trace returns the routes from focal to it as hops, shortest first; direction calls means focal reaches to, callers means to reaches focal, both tries calls first and says which held."
            }),
        ),
    );
    if let Some(object) = schema.as_object_mut() {
        object.insert("required".into(), json!(["focal"]));
    }
    Some(ToolDefinition {
        name: TRACE.into(),
        description: folded_description(
            "Follow how code connects. From one entity, trace walks its call chain: what it \
             calls, what calls it, or both. Given to as well, it returns how focal reaches to, \
             as hops. Given scope, a module path prefix, the walk stays inside that module and \
             says what it left out.",
            &[("Without to", walk), ("With to", route)],
        ),
        annotations: folded_annotations(title_of(TRACE), &[walk, route]),
        input_schema: schema,
    })
}

fn session_definition<'a>(
    find: &impl Fn(&str) -> Option<&'a ToolDefinition>,
) -> Option<ToolDefinition> {
    let start = find("kin_session_start")?;
    let heartbeat = find("kin_session_heartbeat")?;
    let end = find("kin_session_end")?;
    let mut properties = folded_properties(&[start, heartbeat, end]);
    properties.insert(
        "session_id".into(),
        json!({
            "type": "string",
            "description": "The session: the id action start returned, for heartbeat and end. On start, an optional caller-allocated UUID for authenticated daemon registration."
        }),
    );
    let action = json!({
        "type": "string",
        "enum": SESSION_ACTIONS,
        "default": "start",
        "description": "start (default) opens a session and returns session_id. heartbeat keeps it alive and end closes it."
    });
    Some(ToolDefinition {
        name: SESSION.into(),
        description: folded_description(
            "The agent session Kin's writes run in, by action: start (default) opens one and \
             returns its session_id, heartbeat keeps it alive and end closes it.",
            &[
                ("action start", start),
                ("action heartbeat", heartbeat),
                ("action end", end),
            ],
        ),
        annotations: folded_annotations(title_of(SESSION), &[start, heartbeat, end]),
        input_schema: folded_schema(properties, ("action", action)),
    })
}

fn transaction_definition<'a>(
    find: &impl Fn(&str) -> Option<&'a ToolDefinition>,
) -> Option<ToolDefinition> {
    let begin = find("kin_transaction_begin")?;
    let stage = find("kin_transaction_stage")?;
    let commit = find("kin_transaction_commit")?;
    let abort = find("kin_transaction_abort")?;
    let mut properties = folded_properties(&[begin, commit, stage, abort]);
    properties.insert(
        "session_id".into(),
        json!({
            "type": "string",
            "description": "The owning session, for begin. On the other actions an optional mirror; when present in enforce mode it must match the authenticated caller and transaction owner."
        }),
    );
    properties.insert(
        "operations".into(),
        json!({
            "type": "array",
            "description": "For stage, and optionally commit: the exact mutation operations, as mutate takes them.",
            "items": crate::tools::transaction_operation_schema()
        }),
    );
    let action = json!({
        "type": "string",
        "enum": TRANSACTION_ACTIONS,
        "default": "begin",
        "description": "begin (default) opens a transaction and returns transaction_id. stage adds operations, commit publishes everything staged atomically, abort discards it."
    });
    Some(ToolDefinition {
        name: TRANSACTION.into(),
        description: folded_description(
            "A staged repository mutation, by action: begin (default) opens one, stage adds \
             operations, commit publishes them atomically and abort discards them. mutate does \
             all of it in one call.",
            &[
                ("action begin", begin),
                ("action stage", stage),
                ("action commit", commit),
                ("action abort", abort),
            ],
        ),
        annotations: folded_annotations(title_of(TRANSACTION), &[begin, stage, commit, abort]),
        input_schema: folded_schema(properties, ("action", action)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: Value) -> Result<(String, HashMap<String, Value>), String> {
        let mut name = name.to_string();
        let mut arguments: HashMap<String, Value> =
            serde_json::from_value(arguments).expect("arguments are an object");
        resolve(&mut name, &mut arguments)?;
        Ok((name, arguments))
    }

    fn routed_to(name: &str, arguments: Value) -> String {
        call(name, arguments).expect("the call is valid").0
    }

    #[test]
    fn every_alias_names_a_verb_and_every_verb_has_a_handler() {
        for (alias, verb) in ALIASES {
            assert!(is_verb(verb), "{alias} maps to {verb}, which is not a verb");
            assert!(!is_verb(alias), "{alias} is both an alias and a verb");
        }
        for verb in VERBS {
            assert!(
                !handlers_of(verb).is_empty(),
                "{verb} answers through no registered tool"
            );
        }
        let registered: HashSet<String> = crate::tools::tool_definitions()
            .tools
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        for (alias, _) in ALIASES {
            assert!(registered.contains(alias), "{alias} is not registered");
        }
        for verb in VERBS {
            assert!(registered.contains(verb), "{verb} is not registered");
        }
    }

    #[test]
    fn auto_reads_identifiers_and_dotted_names_as_names() {
        for query in [
            "parse_config",
            "Scheduler",
            "app.run",
            "Scheduler.advanceTimers",
            "std::collections::HashMap",
            "$scope",
            "  spaced_name  ",
        ] {
            assert!(looks_like_a_name(query), "{query}");
            assert_eq!(
                routed_to(LOCATE, json!({"query": query})),
                "semantic_search",
                "{query}"
            );
        }
        for query in [
            "where failed requests are retried",
            "parse config",
            "retry-after",
            "fetch()",
            "a.",
            ".b",
            "9lives",
            "",
        ] {
            assert!(!looks_like_a_name(query), "{query}");
        }
        for query in ["where failed requests are retried", "retry-after header"] {
            assert_eq!(
                routed_to(LOCATE, json!({"query": query})),
                "semantic_locate"
            );
        }
    }

    #[test]
    fn locate_modes_route_to_their_tools_and_keep_their_options() {
        let (name, arguments) = call(
            LOCATE,
            json!({"query": "where retries happen", "mode": "name", "kind": "function", "language": "rust", "limit": 5, "max_chars": 9000}),
        )
        .unwrap();
        assert_eq!(name, "semantic_search");
        assert!(!arguments.contains_key("mode"));
        for kept in ["query", "kind", "language", "limit", "max_chars"] {
            assert!(arguments.contains_key(kept), "{kept}");
        }

        let (name, arguments) = call(
            LOCATE,
            json!({"query": "parse_config", "mode": "description", "include_tests": true}),
        )
        .unwrap();
        assert_eq!(name, "semantic_locate");
        assert_eq!(arguments["include_tests"], json!(true));

        let (name, arguments) = call(
            LOCATE,
            json!({"query": "retry_after(", "mode": "literal", "kind": "test"}),
        )
        .unwrap();
        assert_eq!(name, crate::handlers::lexical::TOOL_NAME);
        assert_eq!(arguments["literal"], json!("retry_after("));
        assert!(!arguments.contains_key("query"));
        assert_eq!(arguments["kind"], json!("test"));
        assert_eq!(arguments[crate::handlers::lexical::HELD_TEXT_KEY], true);

        // The literal lookup's old field starts the same text query.
        let (name, arguments) = call(LOCATE, json!({"literal": "x-request-id"})).unwrap();
        assert_eq!(name, crate::handlers::lexical::TOOL_NAME);
        assert_eq!(arguments["literal"], json!("x-request-id"));
        assert_eq!(arguments[crate::handlers::lexical::HELD_TEXT_KEY], true);
    }

    #[test]
    fn text_and_its_hidden_aliases_start_the_same_held_search() {
        for arguments in [
            json!({"query": "retry(_after)?", "mode": "text"}),
            json!({"query": "retry(_after)?", "mode": "literal"}),
            json!({"literal": "retry(_after)?"}),
        ] {
            let (name, arguments) = call(LOCATE, arguments).unwrap();
            assert_eq!(name, crate::handlers::lexical::TOOL_NAME);
            assert_eq!(arguments["literal"], "retry(_after)?");
            assert_eq!(arguments[crate::handlers::lexical::HELD_TEXT_KEY], true);
            assert!(!arguments.contains_key("pattern"));
            assert!(!arguments.contains_key("ignore_case"));
            assert!(!arguments.contains_key("include_tests"));
        }
        let (_, arguments) = call(
            LOCATE,
            json!({"query": "retry(_after)?", "mode": "literal", "pattern": true,
                    "ignore_case": true, "include_tests": false}),
        )
        .unwrap();
        assert_eq!(arguments["pattern"], true);
        assert_eq!(arguments["ignore_case"], true);
        assert_eq!(arguments["include_tests"], false);
    }

    #[test]
    fn matching_options_refuse_nontext_routes_instead_of_being_ignored() {
        for field in ["pattern", "ignore_case"] {
            for value in [false, true] {
                for mut arguments in [
                    json!({"query": "retry", "mode": "name"}),
                    json!({"query": "retry", "mode": "description"}),
                    json!({"query": "retry"}),
                    json!({"query": "where retries happen"}),
                    json!({"cursor": "description-page.1"}),
                ] {
                    arguments[field] = json!(value);
                    let error = call(LOCATE, arguments).unwrap_err();
                    assert!(
                        error.contains(&format!("{field} is only for mode text")),
                        "{error}"
                    );
                }
            }
        }
    }

    #[test]
    fn lexical_cursor_routes_preserve_the_search_the_handler_will_inherit() {
        // Routing recognizes each family; checksum and population checks
        // remain the handler's job. It must not overwrite a stored-field
        // cursor's scope with the new held-text default.
        for cursor in ["lit.saved-page", "txt.saved-page", "txt2.saved-page"] {
            for mut arguments in [
                json!({"cursor": cursor}),
                json!({"cursor": cursor, "query": "retry"}),
                json!({"cursor": cursor, "literal": "retry"}),
            ] {
                arguments["pattern"] = json!(false);
                let (name, arguments) = call(LOCATE, arguments).unwrap();
                assert_eq!(name, crate::handlers::lexical::TOOL_NAME);
                assert_eq!(arguments["cursor"], cursor);
                assert_eq!(arguments["pattern"], false);
                assert!(!arguments.contains_key(crate::handlers::lexical::HELD_TEXT_KEY));
                assert!(!arguments.contains_key("include_tests"));
            }
        }
        let (name, arguments) = call(
            crate::handlers::lexical::TOOL_NAME,
            json!({"literal": "retry", "cursor": "lit.saved-page"}),
        )
        .unwrap();
        assert_eq!(name, crate::handlers::lexical::TOOL_NAME);
        assert!(!arguments.contains_key(crate::handlers::lexical::HELD_TEXT_KEY));
    }

    #[test]
    fn a_locate_cursor_continues_the_page_it_came_from() {
        use base64::Engine as _;
        let literal_token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            json!({"v": 1, "l": "retry", "k": null, "o": 20, "t": 40, "s": "x"}).to_string(),
        );
        let (name, arguments) = call(LOCATE, json!({"cursor": literal_token})).unwrap();
        assert_eq!(name, crate::handlers::lexical::TOOL_NAME);
        assert!(arguments.contains_key("cursor"));
        assert_eq!(
            routed_to(LOCATE, json!({"cursor": "3f9a0c.1"})),
            "semantic_locate"
        );
        // A name-shaped query repeated with its description page's cursor stays
        // on the description page.
        assert_eq!(
            routed_to(
                LOCATE,
                json!({"query": "parse_config", "cursor": "3f9a0c.1"})
            ),
            "semantic_locate"
        );
    }

    #[test]
    fn a_malformed_locate_names_the_valid_form() {
        let error = call(LOCATE, json!({"query": "x", "mode": "fuzzy"})).unwrap_err();
        assert!(
            error.contains("mode must be one of auto, name, description, text"),
            "{error}"
        );
        assert!(error.contains("Valid form: locate {"), "{error}");
        let error = call(LOCATE, json!({})).unwrap_err();
        assert!(error.contains("needs query, or cursor"), "{error}");
        assert!(error.contains("Valid form: locate {\"query\""), "{error}");
        let error = call(
            LOCATE,
            json!({"query": "x", "mode": "name", "cursor": "a.1"}),
        )
        .unwrap_err();
        assert!(error.contains("takes no cursor"), "{error}");
        let error = call(LOCATE, json!({"query": 7})).unwrap_err();
        assert!(error.contains("query must be a string"), "{error}");
    }

    #[test]
    fn trace_routes_to_the_route_tool_only_when_to_is_given() {
        let (name, arguments) = call(
            TRACE,
            json!({"focal": "parse_config", "direction": "callers", "depth": 4, "include_body": false}),
        )
        .unwrap();
        assert_eq!(name, "trace_data_flow");
        assert_eq!(arguments["focal"], json!("parse_config"));
        assert_eq!(arguments["direction"], json!("callers"));

        let (name, arguments) = call(
            TRACE,
            json!({"focal": "main", "to": "parse_config", "direction": "calls", "depth": 5, "include_body": false, "limit_per_step": 4, "max_chars": 9000}),
        )
        .unwrap();
        assert_eq!(name, crate::handlers::path::TOOL_NAME);
        assert_eq!(arguments["from"], json!("main"));
        assert_eq!(arguments["to"], json!("parse_config"));
        assert_eq!(arguments["direction"], json!("forward"));
        assert_eq!(arguments["max_depth"], json!(5));
        assert_eq!(arguments["max_chars"], json!(9000));
        for gone in ["focal", "depth", "include_body", "limit_per_step"] {
            assert!(
                !arguments.contains_key(gone),
                "{gone} reached the route tool"
            );
        }
        for (walk, route) in [("callers", "reverse"), ("both", "either")] {
            let (_, arguments) =
                call(TRACE, json!({"focal": "a", "to": "b", "direction": walk})).unwrap();
            assert_eq!(arguments["direction"], json!(route));
        }
        // The route tool's own spelling of the start is read as focal.
        let (name, arguments) = call(TRACE, json!({"from": "a", "to": "b"})).unwrap();
        assert_eq!(name, crate::handlers::path::TOOL_NAME);
        assert_eq!(arguments["from"], json!("a"));
        // A null to is no route.
        assert_eq!(
            routed_to(TRACE, json!({"focal": "a", "to": null})),
            "trace_data_flow"
        );
        let error = call(TRACE, json!({"to": "b"})).unwrap_err();
        assert!(error.contains("needs focal"), "{error}");
        assert!(error.contains("Valid form: trace {"), "{error}");
    }

    #[test]
    fn session_and_transaction_actions_route_to_their_tools() {
        let start = json!({"vendor": "codex", "client_name": "Codex CLI"});
        assert_eq!(routed_to(SESSION, start.clone()), "kin_session_start");
        let (_, arguments) = call(SESSION, {
            let mut start = start.clone();
            start["action"] = json!("start");
            start
        })
        .unwrap();
        assert!(!arguments.contains_key("action"));
        assert_eq!(
            routed_to(SESSION, json!({"action": "heartbeat", "session_id": "s"})),
            "kin_session_heartbeat"
        );
        assert_eq!(
            routed_to(SESSION, json!({"action": "end", "session_id": "s"})),
            "kin_session_end"
        );
        assert!(call(SESSION, json!({"action":"exec", "argv":["go","test"]})).is_err());

        assert_eq!(
            routed_to(
                TRANSACTION,
                json!({"session_id": "s", "scope": "repository"})
            ),
            "kin_transaction_begin"
        );
        assert_eq!(
            routed_to(
                TRANSACTION,
                json!({"action": "stage", "transaction_id": "t", "operations": []})
            ),
            "kin_transaction_stage"
        );
        assert_eq!(
            routed_to(
                TRANSACTION,
                json!({"action": "commit", "transaction_id": "t"})
            ),
            "kin_transaction_commit"
        );
        assert_eq!(
            routed_to(
                TRANSACTION,
                json!({"action": "abort", "transaction_id": "t"})
            ),
            "kin_transaction_abort"
        );
    }

    #[test]
    fn a_malformed_action_names_the_valid_form() {
        let error = call(SESSION, json!({"action": "close", "session_id": "s"})).unwrap_err();
        assert!(
            error.contains("action must be one of start, heartbeat, end"),
            "{error}"
        );
        assert!(error.contains("Valid form: session {"), "{error}");
        let error = call(SESSION, json!({"action": "end"})).unwrap_err();
        assert!(error.contains("action end needs session_id"), "{error}");
        assert!(error.contains("\"action\":\"end\""), "{error}");
        let error = call(SESSION, json!({"session_id": "s"})).unwrap_err();
        assert!(error.contains("heartbeat or end"), "{error}");
        let error = call(SESSION, json!({"action": "exec", "session_id": "s"})).unwrap_err();
        assert!(
            error.contains("unavailable on the semantic agent surface"),
            "{error}"
        );
        let error = call(
            TRANSACTION,
            json!({"action": "stage", "transaction_id": "t"}),
        )
        .unwrap_err();
        assert!(error.contains("action stage needs operations"), "{error}");
        let error = call(TRANSACTION, json!({"transaction_id": "t"})).unwrap_err();
        assert!(error.contains("stage, commit or abort"), "{error}");
        assert!(error.contains("Valid form: transaction {"), "{error}");
        // The agent profiles stage through mutate, so stage is named last.
        let error = call(TRANSACTION, json!({"action": 3})).unwrap_err();
        assert!(error.contains("begin, commit, abort, stage"), "{error}");
    }

    #[test]
    fn a_one_tool_verb_is_its_tool_under_the_verb() {
        for (verb, tool) in RENAMES {
            // Each verb's own target, as its typed request takes it.
            let target = match verb {
                IMPACT => json!({"entity_ids": ["e"], "entity_id": "e"}),
                EXEC => json!({"argv": ["go", "test"], "entity_id": "e"}),
                MUTATE => json!({"entity_id": "e", "operations": [{
                    "verb": "remove", "target": "e", "description": "Drop it",
                    "payload": {"EntityRemove": {"source_base": {}}}
                }]}),
                _ => json!({"entity_id": "e"}),
            };
            let (name, arguments) = call(verb, target).unwrap();
            assert_eq!(name, tool, "{verb}");
            assert_eq!(arguments["entity_id"], json!("e"));
        }
    }

    #[test]
    fn every_old_name_and_every_other_tool_passes_through_unchanged() {
        for (alias, _) in ALIASES {
            // The retired session verb routes by its action, as it did.
            if alias == SESSION {
                continue;
            }
            let (name, arguments) = call(alias, json!({"query": "x"})).unwrap();
            assert_eq!(name, alias);
            assert_eq!(arguments["query"], json!("x"));
        }
        assert_eq!(routed_to("dead_code", json!({})), "dead_code");
        assert_eq!(
            routed_to(
                crate::agent_belt::DECLARATION_FILTER_ALIAS,
                json!({"query": "x"})
            ),
            "semantic_search"
        );
    }

    #[test]
    fn an_old_name_is_served_wherever_its_verb_is() {
        let allowed = crate::tools::name_set(&[LOCATE, TRACE]);
        for name in [
            "semantic_search",
            "semantic_locate",
            "lexical_lookup",
            "trace_data_flow",
            "trace_path",
            LOCATE,
        ] {
            assert!(allows(&allowed, name), "{name}");
        }
        assert!(!allows(&allowed, "find_references"));
        assert!(!allows(&allowed, "kin_mutate"));
        assert!(speaks_verbs(Some(&allowed)));
        assert!(speaks_verbs(None));
        assert!(!speaks_verbs(Some(&crate::tools::name_set(
            crate::tools::benchmark_tool_names()
        ))));
    }

    #[test]
    fn prose_speaks_verbs_with_the_option_that_reaches_the_old_tool() {
        assert_eq!(
            speak_verbs("Know the exact name? semantic_search."),
            "Know the exact name? locate (mode name)."
        );
        assert_eq!(
            speak_verbs("Chains? trace_data_flow. Two endpoints? `trace_path`."),
            "Chains? trace. Two endpoints? `trace` (with to)."
        );
        assert_eq!(
            speak_verbs("An `entity_id` from semantic_locate or semantic_search is exact."),
            "An `entity_id` from locate is exact."
        );
        assert_eq!(
            speak_verbs("find_references' rows and kin_session_exec's run"),
            "refs' rows and kin_session_exec's run"
        );
        // The retired session tools have no verb to be spoken as.
        assert_eq!(
            speak_verbs("the id kin_session_start returned"),
            "the id session (action start) returned"
        );
        // A longer word that only contains a name is not a mention of it.
        assert_eq!(
            speak_verbs("mcp__kin__find_references and find_references_v2"),
            "mcp__kin__find_references and find_references_v2"
        );
        assert_eq!(speak_verbs("no tool here"), "no tool here");
    }

    #[test]
    fn every_verb_definition_folds_every_property_its_tools_take() {
        let registry = crate::tools::tool_definitions();
        let definition = |name: &str| {
            registry
                .tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("{name} is registered"))
                .clone()
        };
        for verb in VERBS {
            let served = definition(verb);
            let properties = properties_of(&served);
            // The retired session tools are accepted wherever `exec` is served,
            // and `exec` answers only through the toolchain run.
            let retired = |tool: &str| {
                verb == EXEC
                    && matches!(
                        tool,
                        SESSION | "kin_session_start" | "kin_session_heartbeat" | "kin_session_end"
                    )
            };
            for tool in handlers_of(verb).into_iter().filter(|tool| !retired(tool)) {
                for property in properties_of(&definition(tool)).keys() {
                    let spelled = match (verb, property.as_str()) {
                        (LOCATE, "literal") => "query",
                        (TRACE, "from") => "focal",
                        (TRACE, "from_file" | "to_file") => continue,
                        // One served name for every reply budget.
                        (_, "max_chars" | "max_response_chars") => {
                            crate::request_schema::OUTPUT_BUDGET
                        }
                        _ => property.as_str(),
                    };
                    assert!(
                        properties.contains_key(spelled),
                        "{verb} dropped {tool}'s {property}"
                    );
                }
                assert!(
                    served.annotations.read_only_hint
                        <= definition(tool).annotations.read_only_hint,
                    "{verb} claims read-only over {tool}, which writes"
                );
                assert!(
                    served.annotations.destructive_hint
                        >= definition(tool).annotations.destructive_hint,
                    "{verb} hides {tool}'s destructive hint"
                );
            }
            // A plain-word name, such as the retired `session`, is a mention
            // only in backticks: "the connection's own session" names no tool.
            assert!(
                !ALIASES
                    .iter()
                    .any(|(alias, _)| crate::tools::names_tool_in(&served.description, alias)),
                "{verb}'s description still names a replaced tool"
            );
        }
    }
}
