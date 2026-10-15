// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The typed request of every verb an agent is served, and the one place a
//! verb call is checked before it runs.
//!
//! Each verb's arguments are declared once below with
//! [`crate::typed_request!`]. The served input schema on every profile is
//! rendered from that declaration ([`served_schema`]), and every call to the
//! verb is held to it by [`check_call`] before [`crate::verbs::resolve`] routes
//! it, so a call the served schema refuses is refused by the server too, with a
//! sentence that names the fix. What a flat schema cannot carry, which target
//! selector a call names and what a continuation needs, is a
//! [`Rule`](crate::request_schema::Rule) on the same declaration, and the
//! served descriptions say it in words.
//!
//! The arguments a handler reads are the checked request serialized back, with
//! the one served budget name mapped to the spelling the handlers and the
//! daemon have always read ([`handler_budget_name`]).
//!
//! Write operations are held to [`crate::session::OPERATION_FORMS`], the
//! backend's own table of which payload goes with which verb. `mutate` serves
//! them, and `transaction` takes exactly the same operations, from the same
//! type.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::request_schema::{
    check, check_nested, render_nested, ArgType, NestedSpec, Profile, Rule, Ty, TypedRequest,
    OUTPUT_BUDGET,
};
use crate::verbs::{
    CONTEXT, HISTORY, IMPACT, LOCATE, MUTATE, NEIGHBORS, REFS, SESSION, SOURCE, STATUS, TRACE,
    TRANSACTION,
};

crate::arg_enum! {
    /// How `locate` reads its query.
    pub enum LocateMode {
        Auto = "auto",
        Name = "name",
        Description = "description",
        Text = "text",
    }
    full_only []
    synonyms ["literal" => "text"]
    fold_case true
}

crate::arg_enum! {
    /// How `locate` orders the most relevant rows. Relevance is the default;
    /// the others re-sort the leading rows by where each sits in the call
    /// graph (see [`crate::graph_position`]).
    pub enum LocateSort {
        Relevance = "relevance",
        Callers = "callers",
        Hops = "hops",
    }
    full_only []
    synonyms []
    fold_case true
}

crate::arg_enum! {
    /// Which way `neighbors` walks: what the entity depends on, what depends
    /// on it, or both.
    pub enum NeighborDirection {
        In = "in",
        Out = "out",
        Both = "both",
    }
    full_only []
    synonyms [
        "outgoing" => "out", "dependencies" => "out", "depends_on" => "out", "calls" => "out",
        "incoming" => "in", "dependents" => "in", "callers" => "in", "impact" => "in",
        "all" => "both", "" => "both",
    ]
    fold_case true
}

crate::arg_enum! {
    /// Which way `trace` walks, or which end of a route reaches the other.
    pub enum TraceDirection {
        Calls = "calls",
        Callers = "callers",
        Both = "both",
    }
    full_only []
    synonyms [
        "callee" => "calls", "callees" => "calls", "out" => "calls", "outgoing" => "calls",
        "forward" => "calls",
        "caller" => "callers", "in" => "callers", "incoming" => "callers", "reverse" => "callers",
        "all" => "both", "either" => "both", "" => "both",
    ]
    fold_case true
}

crate::arg_enum! {
    /// What a `trace` scope restricts: the walk itself, or only its rows.
    pub enum TraceScopeMode {
        Paths = "paths",
        Rows = "rows",
    }
    full_only []
    synonyms [
        "path" => "paths", "traversal" => "paths", "walk" => "paths", "restrict" => "paths",
        "row" => "rows", "output" => "rows", "filter" => "rows", "results" => "rows",
    ]
    fold_case true
}

crate::arg_enum! {
    /// A reference class `refs` can be narrowed to.
    pub enum ReferenceKind {
        Calls = "calls",
        Imports = "imports",
        References = "references",
    }
    full_only []
    synonyms ["call" => "calls", "import" => "imports", "reference" => "references", "refs" => "references"]
    fold_case true
}

crate::arg_enum! {
    /// The relationship question answered by `refs`.
    pub enum ReferenceView {
        References = "references",
        Overrides = "overrides",
    }
    full_only []
    synonyms []
    fold_case false
}

crate::arg_enum! {
    /// The weakest resolution a `refs` headline row may carry.
    pub enum MinResolution {
        NameOnly = "name_only",
        ImportScoped = "import_scoped",
        TypeResolved = "type_resolved",
    }
    full_only []
    synonyms []
    fold_case false
}

crate::arg_enum! {
    /// The ranking pipeline a description search forces.
    pub enum Pipeline {
        Fused = "fused",
        Cosine = "cosine",
    }
    full_only []
    synonyms []
    fold_case false
}

crate::arg_enum! {
    /// What a description search ranks.
    pub enum Granularity {
        Entity = "entity",
    }
    full_only []
    synonyms []
    fold_case false
}

crate::arg_enum! {
    /// What a `session` call does.
    pub enum SessionAction {
        Start = "start",
        Heartbeat = "heartbeat",
        End = "end",
    }
    full_only []
    synonyms []
    fold_case true
}

crate::arg_enum! {
    /// What a `transaction` call does. Staging is `mutate` with a
    /// `transaction_id` on the agent profiles, so `stage` is served in full
    /// only, and taken everywhere.
    pub enum TransactionAction {
        Begin = "begin",
        Commit = "commit",
        Abort = "abort",
    }
    full_only [Stage = "stage"]
    synonyms []
    fold_case true
}

/// One write operation, checked against the backend's own forms and handed to
/// the handler exactly as sent: the backend decodes it into
/// [`crate::session::McpMutationOperation`], and a decode that met a
/// re-serialized `body: null` would refuse a patch that never carried one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Operation(pub Value);

static OPERATION: NestedSpec = NestedSpec {
    render: operation_schema,
    check: check_operation,
};

impl ArgType for Operation {
    fn ty() -> Ty {
        Ty::Nested(&OPERATION)
    }
}

crate::typed_request! {
    /// What a session says it may do. Declaring none takes what the store
    /// permits.
    pub struct Capabilities for "capabilities", example json!({"can_write": true}), rules vec![], {
        can_read: bool { default(true), full() },
        can_write: bool { default(false) },
        can_commit: bool { default(false) },
        can_execute: bool { default(false) },
        can_branch: bool { default(false), full() },
        max_concurrent_intents: u64 { default(1), min(0), full() },
    }
}

static CAPABILITIES: NestedSpec = NestedSpec {
    render: render_nested::<Capabilities>,
    check: check_nested::<Capabilities>,
};

impl ArgType for Capabilities {
    fn ty() -> Ty {
        Ty::Nested(&CAPABILITIES)
    }
}

crate::typed_request! {
    pub struct LocateRequest for LOCATE,
    example json!({"query": "where failed requests are retried"}),
    rules vec![Rule::Select {
        sets: &[&["query"], &["literal"]],
        continuation: &["cursor"],
        several: false,
    }], {
        mode: LocateMode { doc("Auto: name/meaning."), no_agent_doc(), default("auto") },
        query: String { doc("Semantic name, meaning or text."), no_agent_doc() },
        kind: String { doc("`command`: CLI entry.") },
        language: String {},
        // Described once, on `trace`: the same dotted prefix of a row's module.
        scope: String {
            doc("Module prefix; lists the rows inside it. On 1,032 descriptions naming no function, the right package lifted the top 5 from 59% to 80%."),
            no_agent_doc()
        },
        sort: LocateSort {
            doc("Re-sorts the 20 most relevant rows: callers (most proven non-test callers first) or hops (fewest calls from a root first). Relevance is the default and finds described code best: sorting by callers always took the top 5 from 59% to 32%. Use it to ask which of the rows is central."),
            agent_doc("Re-sorts top 20; callers: top-5 59% to 32%.")
        },
        include_tests: bool { doc("Text includes tests unless false."), no_agent_doc() },
        pattern: bool { doc("Text only: a regular expression; false by default."), no_agent_doc() },
        ignore_case: bool { doc("Text only: ignore case; false by default."), no_agent_doc() },
        limit: u64 { range(1, 200), default(20) },
        cursor: String { doc("`next_cursor` alone."), no_agent_doc() },
        details: bool {
            doc("Adds row signatures. MCP cursor inherits; CLI repeats."),
            agent_doc("Adds row signatures."),
            default(false)
        },
        max_output_bytes: u64 { output_budget() },
        literal: String { hidden() },
        compact: bool { default(true), full() },
        queries: Vec<String> { full() },
        page_size: u64 { min(1), full() },
        granularity: Granularity { default("entity"), full() },
        include_snippet: bool { default(true), full() },
        snippet_alias: bool { default(false), full() },
        pipeline: Pipeline { full() },
        explain: bool { default(false), full() },
    }
}

crate::typed_request! {
    pub struct RefsRequest for REFS,
    example json!({"query": "parse_config"}),
    rules vec![Rule::Select {
        sets: &[&["entity_id"], &["query"]],
        continuation: &["cursor"],
        several: false,
    }, Rule::Needs { field: "proof_for_caller", needs: "entity_id" },
    Rule::Excludes { field: "proof_for_caller", with: "cursor", why: "receiver proof is a single selected answer; omit cursor." }], {
        entity_id: String { doc("Returned UUID.") },
        query: String { doc("All matches; qualified_name pins one.") },
        proof_for_caller: String { doc("Caller UUID; entity_id is the callee UUID."), max_len(36), from_reply() },
        view: ReferenceView { doc("Overrides requires entity_id.") },
        relation_kinds: Vec<ReferenceKind> {},
        // Described once, on `trace`: the same dotted prefix of a row's module.
        scope: String { doc("Module prefix; lists the callers inside it."), no_agent_doc() },
        include_tests: bool { doc("False excludes current Test-role callers."), no_agent_doc(), default(true) },
        cursor: String {
            doc("Cursor alone inherits caller-role scope."),
            no_agent_doc(),
            max_len(256),
            from_reply()
        },
        details: bool {
            doc("Adds all site lines. MCP cursor inherits; CLI repeats."),
            agent_doc("Adds all site lines."),
            default(false)
        },
        max_output_bytes: u64 { output_budget() },
        answer_only: bool { default(false), full() },
        compact: bool { default(true), full() },
        include_snippets: bool { default(false), full() },
        min_resolution: MinResolution { default("import_scoped"), full() },
        explain: bool { hidden() },
    }
}

crate::typed_request! {
    pub struct TraceRequest for TRACE,
    example json!({"focal": "parse_config", "direction": "callers"}),
    rules vec![
        Rule::Select { sets: &[&["focal"]], continuation: &["cursor"], several: false },
        Rule::Excludes {
            field: "cursor",
            with: "to",
            why: "a route answers in one page. Send cursor alone to continue a walk.",
        },
        Rule::Excludes {
            field: "scope",
            with: "to",
            why: "scope confines a walk; a route between two entities takes none.",
        },
        Rule::Needs { field: "scope_mode", needs: "scope" },
    ], {
        focal: String { doc("UUID, name or mod.name."), no_agent_doc(), alias("from") },
        to: String {},
        scope: String {
            doc("Module prefix; walk stays inside it."),
            agent_doc("Dotted prefix of a row `module`.")
        },
        scope_mode: TraceScopeMode { default("paths"), full() },
        direction: TraceDirection { default("both") },
        depth: u64 { range(1, 8), default(3) },
        include_tests: bool { doc("False leaves test code out."), no_agent_doc(), default(true) },
        include_body: bool { doc("Sources; false: shape."), no_agent_doc(), default(true) },
        limit_per_step: u64 {
            range(1, crate::remediation::TRACE_MAX_LIMIT_PER_STEP as i64),
            default(crate::remediation::TRACE_DEFAULT_LIMIT_PER_STEP as u64)
        },
        cursor: String { doc("`next_cursor` alone."), no_agent_doc(), max_len(256), from_reply() },
        details: bool { doc("Adds proof."), default(false) },
        max_output_bytes: u64 { output_budget() },
        compact: bool { default(false), full() },
        include_type_edges: bool { default(false), full() },
        target: String { full() },
        max_depth: u64 {
            range(1, crate::remediation::PATH_MAX_MAX_DEPTH as i64), default(6), full()
        },
        limit: u64 { range(1, 25), default(3), full() },
    }
}

/// The most focal entities one context call names.
pub const CONTEXT_ENTITIES_MAX: u64 = 16;

crate::typed_request! {
    pub struct ContextRequest for CONTEXT,
    example json!({"question": "how are failed requests retried"}),
    rules vec![Rule::Select {
        sets: &[&["entity_id"], &["entities"], &["question"]],
        continuation: &[],
        several: true,
    }], {
        entity_id: String { doc("UUID") },
        // Sixteen, since a reader checking the rows of one answer names that
        // many at once: at eight, a ninth name refused the whole call.
        entities: Vec<String> { doc("Names/UUIDs."), items(1, CONTEXT_ENTITIES_MAX) },
        question: String {},
        depth: u64 { range(1, 8), default(2) },
        token_budget: u64 { range(0, 200_000), default(16_000) },
        max_output_bytes: u64 { output_budget() },
        focal_body: bool { doc("False: no focal bodies."), no_agent_doc(), default(true) },
        neighbor_bodies: bool { doc("True: bodies, not signatures."), no_agent_doc(), default(false) },
        details: bool { doc("Adds row metadata."), default(false) },
        include_traffic: bool { default(true), full() },
        compact: bool { default(false), full() },
        max_focals: u64 { range(1, 32), hidden() },
    }
}

crate::typed_request! {
    pub struct SourceRequest for SOURCE,
    example json!({"entity_id": "<entity UUID>"}),
    rules vec![], {
        entity_id: String { doc("UUID"), required() },
        full_body: bool { doc("Whole body, up to 16 KB."), default(false) },
        // Taught by the excerpt that returns it, so no listing pays for it.
        cursor: String { doc("`next_cursor` of an excerpt."), max_len(256), hidden() },
    }
}

crate::typed_request! {
    pub struct NeighborsRequest for NEIGHBORS,
    example json!({"entity_id": "<entity UUID>", "direction": "in"}),
    rules vec![], {
        entity_id: String { doc("UUID"), required() },
        direction: NeighborDirection { doc("`out` deps, `in` dependents."), default("both") },
        depth: u64 { range(1, 8), default(2) },
        limit: u64 { range(1, 2_000), default(30) },
        max_output_bytes: u64 { output_budget() },
        compact: bool { default(true), full() },
    }
}

crate::typed_request! {
    pub struct ImpactRequest for IMPACT,
    example json!({"entity_ids": ["<entity UUID>"]}),
    rules vec![Rule::Select {
        sets: &[&["entity_ids"], &["change_ids"], &["base", "head"]],
        continuation: &["cursor"],
        several: false,
    }], {
        entity_ids: Vec<String> { min_items(1) },
        change_ids: Vec<String> { min_items(1) },
        base: String { doc("Change ID.") },
        head: String { doc("Change ID.") },
        scope: String { doc("Module prefix; lists the entities inside it."), no_agent_doc() },
        include_tests: bool { doc("False leaves test code out."), no_agent_doc(), default(true) },
        dependents: bool { doc("True lists indirect dependents."), no_agent_doc(), default(false) },
        cursor: String { doc("`next_cursor` alone."), no_agent_doc(), max_len(256), from_reply() },
        max_output_bytes: u64 { output_budget() },
        compact: bool { default(true), full() },
        include_traffic: bool { default(true), full() },
        depth: u64 { range(1, 8), hidden() },
    }
}

crate::typed_request! {
    pub struct StatusRequest for STATUS,
    example json!({}),
    rules vec![], {
        cursor: String { doc("`next_cursor` alone."), no_agent_doc(), max_len(512), from_reply() },
        flags_cursor: String { doc("Continue open review flags from `_kin.flags_next_cursor`; separate from the enrichment cursor."), no_agent_doc(), max_len(64), from_reply() },
        max_output_bytes: u64 { output_budget() },
    }
}

crate::typed_request! {
    pub struct HistoryRequest for HISTORY,
    example json!({"entity_id": "<entity UUID>"}),
    rules vec![], {
        entity_id: String { doc("UUID"), required() },
        limit: u64 { range(1, 200), default(20) },
        offset: u64 { doc("next_offset"), no_agent_doc(), min(0), default(0) },
        compact: bool { default(true), full() },
    }
}

crate::typed_request! {
    pub struct MutateRequest for MUTATE,
    example json!({"operations": [{
        "verb": "patch", "target": "<entity UUID>", "description": "Retry once more",
        "payload": {"EntitySourcePatch": {"source_base": "<source_base from source>",
            "edits": [{"old_text": "retries = 2", "new_text": "retries = 3"}]}}
    }]}),
    rules vec![
        Rule::Excludes {
            field: "transaction_id",
            with: "summary",
            why: "staging commits nothing. Give the message to transaction commit.",
        },
        Rule::Excludes {
            field: "transaction_id",
            with: "request_id",
            why: "a keyed mutate commits in one call. Drop transaction_id to commit now.",
        },
    ], {
        operations: Vec<Operation> { doc("Applied atomically."), no_agent_doc(), min_items(1), required() },
        summary: String { doc("One sentence for the history."), no_agent_doc() },
        transaction_id: String { doc("Stage into this open transaction instead."), no_agent_doc() },
        session_id: String { full() },
        scope: String { full() },
        request_id: String { non_empty(), max_len(256), full() },
    }
}

crate::typed_request! {
    /// The retired session verb, accepted as a hidden alias: start, keep and
    /// end a named session, and run the toolchain in it.
    pub struct SessionRequest for SESSION,
    example json!({"action": "end", "session_id": "<session_id>"}),
    rules vec![], {
        action: SessionAction {},
        session_id: String {},
        vendor: String {},
        client_name: String {},
        capabilities: Capabilities {},
        transport: String { default("mcp") },
        pid: u64 { min(0) },
    }
}

crate::typed_request! {
    pub struct TransactionRequest for TRANSACTION,
    example json!({"action": "commit", "transaction_id": "<transaction_id>"}),
    rules vec![], {
        action: TransactionAction { default("begin") },
        scope: String { doc("Label, never a path.") },
        transaction_id: String { doc("UUID"), no_agent_doc() },
        message: String { doc("One sentence for the history."), no_agent_doc() },
        session_id: String { full() },
        operations: Vec<Operation> { full() },
    }
}

/// Every tool served from a typed request, with its request's name.
pub const TYPED_TOOLS: [&str; 12] = [
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

/// Whether `tool` is served from a typed request.
pub fn is_typed(tool: &str) -> bool {
    TYPED_TOOLS.contains(&tool)
}

/// The input schema `tool` is served on `profile`, rendered from its typed
/// request, or `None` for a tool with none. `describe` supplies the long
/// description a registered tool carries for a field, for the `full` profile.
pub fn served_schema(
    tool: &str,
    profile: Profile,
    describe: &dyn Fn(&str) -> Option<String>,
) -> Option<Value> {
    Some(match tool {
        LOCATE => LocateRequest::schema(profile, describe),
        REFS => RefsRequest::schema(profile, describe),
        TRACE => TraceRequest::schema(profile, describe),
        CONTEXT => ContextRequest::schema(profile, describe),
        SOURCE => SourceRequest::schema(profile, describe),
        NEIGHBORS => NeighborsRequest::schema(profile, describe),
        IMPACT => ImpactRequest::schema(profile, describe),
        STATUS => StatusRequest::schema(profile, describe),
        HISTORY => HistoryRequest::schema(profile, describe),
        MUTATE => MutateRequest::schema(profile, describe),
        SESSION => SessionRequest::schema(profile, describe),
        TRANSACTION => TransactionRequest::schema(profile, describe),
        _ => return None,
    })
}

/// Every field `tool`'s typed request declares, served or not.
pub fn declared_fields(tool: &str) -> Option<Vec<crate::request_schema::Field>> {
    Some(match tool {
        LOCATE => LocateRequest::fields(),
        REFS => RefsRequest::fields(),
        TRACE => TraceRequest::fields(),
        CONTEXT => ContextRequest::fields(),
        SOURCE => SourceRequest::fields(),
        NEIGHBORS => NeighborsRequest::fields(),
        IMPACT => ImpactRequest::fields(),
        STATUS => StatusRequest::fields(),
        HISTORY => HistoryRequest::fields(),
        MUTATE => MutateRequest::fields(),
        SESSION => SessionRequest::fields(),
        TRANSACTION => TransactionRequest::fields(),
        _ => return None,
    })
}

fn checked<R: TypedRequest>(
    arguments: &HashMap<String, Value>,
) -> Result<HashMap<String, Value>, String> {
    check::<R>(arguments).map(|(_, handled)| handled)
}

/// Hold one call to `tool`'s typed request, and rewrite `arguments` into what
/// its handler reads. A tool with no typed request is left alone.
pub fn check_call(tool: &str, arguments: &mut HashMap<String, Value>) -> Result<(), String> {
    // Text options inherit their value on cursor continuation. An explicit
    // value must be a boolean; null must not silently reset the search.
    if tool == LOCATE {
        for field in ["pattern", "ignore_case"] {
            if arguments
                .get(field)
                .is_some_and(|value| !value.is_boolean())
            {
                return Err(format!("{field} must be true or false."));
            }
        }
    }
    // These cursor-inherited choices have no nullable spelling: only an
    // explicit boolean may override detail or caller-role scope. Other
    // optional fields keep the convention that null means absent.
    if matches!(tool, LOCATE | REFS | CONTEXT | TRACE)
        && arguments
            .get("details")
            .is_some_and(|value| !value.is_boolean())
    {
        return Err("details must be true or false.".into());
    }
    if matches!(tool, REFS | TRACE | IMPACT)
        && arguments
            .get("include_tests")
            .is_some_and(|value| !value.is_boolean())
    {
        return Err("include_tests must be true or false.".into());
    }
    let handled = match tool {
        LOCATE => checked::<LocateRequest>(arguments)?,
        REFS => checked::<RefsRequest>(arguments)?,
        TRACE => checked::<TraceRequest>(arguments)?,
        CONTEXT => checked::<ContextRequest>(arguments)?,
        SOURCE => checked::<SourceRequest>(arguments)?,
        NEIGHBORS => checked::<NeighborsRequest>(arguments)?,
        IMPACT => checked::<ImpactRequest>(arguments)?,
        STATUS => checked::<StatusRequest>(arguments)?,
        HISTORY => checked::<HistoryRequest>(arguments)?,
        MUTATE => checked::<MutateRequest>(arguments)?,
        SESSION => checked::<SessionRequest>(arguments)?,
        TRANSACTION => checked::<TransactionRequest>(arguments)?,
        _ => return Ok(()),
    };
    *arguments = handled;
    if let Some(internal) = handler_budget_name(tool, arguments) {
        if let Some(budget) = arguments.remove(OUTPUT_BUDGET) {
            arguments.insert(internal.into(), budget);
        }
    }
    Ok(())
}

/// The spelling the handlers behind `tool`, and the daemon, read its reply
/// budget under: `max_response_chars` for the trace walk, which bounds itself
/// by it, and `max_chars` everywhere else, the route a trace with `to` takes
/// among them. `None` for a verb with no reply
/// budget, `session` among them, whose `max_output_bytes` is the bytes a
/// toolchain run keeps of each stream and is left alone.
pub fn handler_budget_name(tool: &str, arguments: &HashMap<String, Value>) -> Option<&'static str> {
    let fields = declared_fields(tool)?;
    let takes_budget = fields
        .iter()
        .any(|field| field.name == OUTPUT_BUDGET && field.aliases.contains(&"max_chars"));
    if !takes_budget {
        return None;
    }
    // A trace with `to` is answered by the route tool, which reads `max_chars`
    // like every other tool; only the walk reads `max_response_chars`.
    let walk = tool == TRACE && arguments.get("to").is_none_or(|to| to.is_null());
    Some(if walk {
        "max_response_chars"
    } else {
        "max_chars"
    })
}

/// The compact payload schema an agent profile serves for one operation form.
///
/// The anchored patch, the path a targeted change takes, is served as
/// structure; the others by the fields each takes, since the refusal a caller
/// gets when it sends one wrong spells out every form
/// ([`crate::session::ACCEPTED_OPERATION_SHAPES`]).
fn compact_payload_schema(payload: &str) -> Value {
    match payload {
        "EntitySourcePatch" => json!({
            "type": "object",
            "properties": {
                "source_base": {"type": "object"},
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Each old_text occurs once in the original body.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_text": {"type": "string"},
                            "new_text": {"type": "string"}
                        },
                        "required": ["old_text", "new_text"]
                    }
                }
            },
            "required": ["source_base", "edits"]
        }),
        "EntitySourceBase" => json!({
            "type": "object",
            "description": "The source_base, for update with body."
        }),
        "EntityCreate" => json!({
            "type": "object",
            "description": "To a unit, as in an empty repository: {repository_base, unit: {language, package, name, role}, name, kind, body, imports}, target the name. Beside a declaration: {source_base, placement, name, kind, body}, target its UUID. A method: placement sibling_after a method, or member of its class."
        }),
        "UnitImports" => json!({
            "type": "object",
            "description": "{repository_base, unit, add, remove}, target the package."
        }),
        "EntityRemove" => json!({"type": "object", "description": "{source_base}"}),
        "EntityContent" => json!({
            "type": "object",
            "description": "Content Kin holds, for an unparsed entity. replace takes {source_base, content_of, at}; create, beside the target, {source_base, name, content_of or text}."
        }),
        _ => json!({"type": "object"}),
    }
}

/// One operation's served schema. The `full` profile serves the registered
/// contract, every form with its nested base schemas; an agent profile serves
/// the forms [`crate::session::OPERATION_FORMS`] marks for agents, with the
/// payload held to exactly one variant and each variant to its verb.
pub fn operation_schema(profile: Profile) -> Value {
    if profile == Profile::Full {
        return crate::tools::transaction_operation_schema();
    }
    let forms: Vec<&crate::session::OperationForm> = crate::session::OPERATION_FORMS
        .iter()
        .filter(|form| form.agent)
        .collect();
    // A form is paired with its first verb, the others being its aliases,
    // except one whose verbs each do something of their own.
    let paired = |form: &crate::session::OperationForm| -> Vec<&'static str> {
        if form.payload == "EntityContent" {
            form.verbs.to_vec()
        } else {
            vec![form.verbs[0]]
        }
    };
    let mut verbs: Vec<&str> = Vec::new();
    for form in &forms {
        for verb in paired(form) {
            if !verbs.contains(&verb) {
                verbs.push(verb);
            }
        }
    }
    let payloads: Map<String, Value> = forms
        .iter()
        .map(|form| {
            (
                form.payload.to_string(),
                compact_payload_schema(form.payload),
            )
        })
        .collect();
    let pairings: Vec<Value> = forms
        .iter()
        .map(|form| {
            let verbs = paired(form);
            let verb = match verbs.as_slice() {
                [one] => json!({"const": one}),
                several => json!({"enum": several}),
            };
            let mut pairing = json!({
                "properties": {
                    "verb": verb,
                    "payload": {"required": [form.payload]}
                }
            });
            if form.body {
                pairing["required"] = json!(["body"]);
            }
            pairing
        })
        .collect();
    json!({
        "type": "object",
        "properties": {
            "verb": {"type": "string", "enum": verbs},
            "target": {
                "type": "string",
                "minLength": 1,
                "description": "Entity UUID, or the name a create declares."
            },
            "body": {"type": "string", "description": "Complete new source, for update."},
            "payload": {
                "type": "object",
                "description": "Copy source_base from the source tool and repository_base from status or the last mutate, unchanged.",
                "properties": payloads,
                "minProperties": 1,
                "maxProperties": 1,
                "additionalProperties": false
            },
            "description": {"type": "string"}
        },
        "required": ["verb", "target", "payload", "description"],
        "anyOf": pairings
    })
}

/// Hold one operation to the backend's forms: the fields it may carry, one
/// payload variant, and the verb and body that variant goes with.
///
/// The payload's own fields are the backend's to decode, and its refusal
/// spells out every form, so this stops at the shape a served schema states.
/// An operation with no payload is left to the backend too, which answers each
/// payload-less form by name.
pub fn check_operation(operation: &Value) -> Result<(), String> {
    use crate::session::{operation_form, FormTarget, OPERATION_FIELDS, OPERATION_FORMS};
    let Some(fields) = operation.as_object() else {
        return Err("each operation must be an object.".into());
    };
    let unknown: Vec<&str> = fields
        .keys()
        .map(String::as_str)
        .filter(|key| !OPERATION_FIELDS.contains(key))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "an operation takes only {}, and this one also names {}. Full new source goes in \
             body; anchored edits in payload.EntitySourcePatch.edits.",
            OPERATION_FIELDS.join(", "),
            unknown.join(", ")
        ));
    }
    for field in ["verb", "target", "description"] {
        match fields.get(field) {
            None | Some(Value::Null) => {
                return Err(format!(
                    "each operation needs {field}{}.",
                    if field == "description" {
                        ": one sentence saying what it changes"
                    } else {
                        ""
                    }
                ))
            }
            Some(Value::String(_)) => {}
            Some(_) => return Err(format!("an operation's {field} must be a string.")),
        }
    }
    let verb = fields
        .get("verb")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let Some(payload) = fields.get("payload").filter(|payload| !payload.is_null()) else {
        return Ok(());
    };
    let variants: Vec<&str> = OPERATION_FORMS.iter().map(|form| form.payload).collect();
    let Some(payload) = payload.as_object() else {
        return Err(format!(
            "payload must be an object with exactly one of {}.",
            variants.join(", ")
        ));
    };
    let keys: Vec<&str> = payload.keys().map(String::as_str).collect();
    let [variant] = keys.as_slice() else {
        return Err(format!(
            "payload takes exactly one of {}, and this one {}.",
            variants.join(", "),
            if keys.is_empty() {
                "is empty".to_string()
            } else {
                format!("names {}", keys.join(" and "))
            }
        ));
    };
    let Some(form) = operation_form(variant) else {
        return Err(if *variant == "Blob" {
            "a Blob payload is not committable. Change source with EntitySourcePatch, or \
             EntitySourceBase with the whole new body."
                .into()
        } else {
            format!(
                "payload {variant} is not an operation Kin takes; it takes one of {}.",
                variants.join(", ")
            )
        });
    };
    if !form.verbs.contains(&verb.as_str()) {
        return Err(format!(
            "payload {variant} goes with verb {}, and this operation says {}.",
            form.verbs.join(" or "),
            fields.get("verb").cloned().unwrap_or_default()
        ));
    }
    let has_body = fields.contains_key("body");
    if form.body {
        let empty = fields
            .get("body")
            .and_then(Value::as_str)
            .is_none_or(|body| body.trim().is_empty());
        if !has_body || empty {
            return Err(format!(
                "verb {verb} with {variant} needs body: the entity's whole new source."
            ));
        }
    } else if has_body {
        return Err(format!(
            "{variant} takes no body, not even null; {}",
            if *variant == "EntitySourcePatch" {
                "its edits carry the change."
            } else {
                "the payload carries the change."
            }
        ));
    }
    let target = fields
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if form.target != FormTarget::EntityOrEmpty && target.trim().is_empty() {
        return Err(format!(
            "{variant} needs target: {}.",
            match form.target {
                FormTarget::EntityId => "the entity UUID its source_base names",
                FormTarget::DeclaredName => "the declared name, or the anchor UUID",
                _ => "the unit's package name",
            }
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
