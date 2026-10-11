// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Plain replies as compact text, a prototype behind `KIN_MCP_REPLY_FORMAT=text`.
//!
//! A JSON reply spends a large share of its tokens on keys, quotes, escapes
//! and ids. This writes the same plain reply as lines a model reads like the
//! CLI's output: the verdict and its meaning first, then one line per row,
//! then what is left to check, then cursors.
//!
//! ```text
//! Answer (partial): 5 functions can end up calling get_root_path. ...
//! reach.members (5):
//!   flask.sansio.scaffold :: Scaffold.__init__  root_path = get_root_path(self.import_name)
//!   flask.sansio.app :: App.__init__  super().__init__(
//! reach.frontier (2):
//!   tests.test_config :: testapp  testapp = Flask("testapp")  why=calls `Flask`, ... [test]
//! next_cursor: p1c2d3e4
//! ```
//!
//! Rows name entities as `module :: name`, which this connection reads back
//! as that entity's id in any id argument ([`crate::session_handles`]). A
//! name that is not unique, or that has no module, carries `#<handle>`, the
//! id's short handle. Nothing the JSON reply states is dropped: the verdict,
//! meaning, every gap and place to check, every count and cursor are written;
//! only keys whose value is the ordinary one (a `type_resolved` proof, a
//! function's kind, a caller's role) and ids that a row's name already
//! stands for are left unwritten. Section names keep the JSON keys, so a
//! meaning that points at `reach.frontier` still points at its lines.
//!
//! One place is written shorter than its JSON. An entity that refers to
//! reach members without calling them is one line for each way it does,
//! naming the members: `held as a value in <holder>: <members> (<how>)`, or
//! `referenced without a call by <referrer>: <members>`. The line of code at
//! each site spells the same name, and `source` on the holder serves it.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value};

use crate::session_handles::{is_uuid, SessionHandles};

/// The environment variable that picks the plain reply format.
pub const REPLY_FORMAT_ENV: &str = "KIN_MCP_REPLY_FORMAT";

/// How a plain reply is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplyFormat {
    /// JSON, with session handles. The default.
    #[default]
    Json,
    /// Compact text, see this module.
    Text,
}

impl ReplyFormat {
    pub fn from_env() -> Self {
        match std::env::var(REPLY_FORMAT_ENV)
            .ok()
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("text") => Self::Text,
            _ => Self::Json,
        }
    }
}

/// Rewrite every JSON text block of a tool result as text, in place. An error
/// or a block that is not a JSON object keeps its text.
pub fn render_result(result: &mut Value, tool: &str, handles: &SessionHandles) {
    if let Some(blocks) = result.get_mut("content").and_then(Value::as_array_mut) {
        for block in blocks {
            let Some(text) = block.get("text").and_then(Value::as_str) else {
                continue;
            };
            let Ok(Value::Object(payload)) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            block["text"] = Value::String(render(&payload, tool, handles));
        }
    }
    if let Some(structured) = result.get_mut("structuredContent") {
        handles.shorten_value(structured);
    }
}

/// Keys whose rows are the answer, written before everything else.
const ANSWER_KEYS: [&str; 29] = [
    "message",
    "error",
    "entity",
    "body",
    "header",
    "results",
    "hits",
    "references",
    crate::plain_reply::MODULE_CODE_USERS_KEY,
    crate::plain_reply::MODULE_LEVEL_USERS_KEY,
    "answers",
    "reach",
    "chain",
    "route",
    "steps",
    "members",
    "asked_about",
    "entities",
    "links",
    "changes",
    // What a committed write changed, created and removed.
    crate::write_reply::CHANGED,
    crate::write_reply::CREATED,
    crate::write_reply::REMOVED,
    "affected_callers",
    crate::handlers::review::IMPACT_VALUE_USES_KEY,
    "affected_override_callers",
    "affected_overrides",
    crate::impact_sections::INTERFACE_IMPLEMENTATIONS_KEY,
    "affected_dependents",
];

/// Keys that say what is left to check, written right after the answer, and
/// after them the count of places proved to add nothing to it.
const GAP_KEYS: [&str; 11] = [
    "frontier",
    "frontier_withheld",
    "bounds",
    "also_referenced_by",
    "left_to_check",
    "name_only",
    "passed_to_calls",
    "not_held",
    "overridden_by",
    "withheld",
    crate::plain_reply::SETTLED_KEY,
];

/// Envelope notes that describe the JSON shape only; text rows say the same
/// by how they are written.
const JSON_ONLY_NOTES: [&str; 3] = [
    crate::reply_budget::ROW_NOTE_KEY,
    crate::reply_budget::LINKS_NOTE_KEY,
    crate::query_reply::CONTEXT_ROWS_KEY,
];

/// A reply as text.
pub fn render(payload: &Map<String, Value>, tool: &str, handles: &SessionHandles) -> String {
    let names = Names::collect(payload, handles);
    let mut out = Writer {
        lines: Vec::new(),
        names: &names,
        handles,
    };
    let envelope = payload.get("_kin").and_then(Value::as_object);
    if tool == "trace_data_flow" {
        if let Some(reach) = crate::trace_reach::answer_view(payload) {
            out.reach(payload, &reach);
            return out.lines.join("\n");
        }
    }
    out.lead(envelope);
    // A reply that is itself one entity (a source read) opens with its row.
    let top_row = payload
        .get("entity_id")
        .and_then(Value::as_str)
        .is_some_and(is_uuid)
        && payload.contains_key("name");
    if top_row {
        let identity: Map<String, Value> = payload
            .iter()
            .filter(|(key, _)| matches!(key.as_str(), "entity_id" | "name" | "module" | "kind"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        out.row(&identity, 0, tool);
    }
    let body = payload.get("body").and_then(Value::as_str);
    let mut keys: Vec<&String> = payload
        .keys()
        .filter(|key| *key != "_kin")
        .filter(|key| !top_row || !matches!(key.as_str(), "entity_id" | "name" | "module" | "kind"))
        // A signature the body already holds is not written twice.
        .filter(|key| {
            *key != "signature"
                || !payload["signature"]
                    .as_str()
                    .zip(body)
                    .is_some_and(|(signature, body)| {
                        crate::lean_reply::body_restates(body, signature)
                    })
        })
        .collect();
    keys.sort_by_key(|key| order(key));
    for key in keys {
        if is_cursor_key(key) {
            continue;
        }
        out.field(key, &payload[key.as_str()], 0, tool);
    }
    out.envelope_notes(envelope, 0, tool);
    for (key, value) in payload.iter().filter(|(key, _)| is_cursor_key(key)) {
        out.field(key, value, 0, tool);
    }
    out.lines.join("\n")
}

/// The proof codes a text row writes: `type` is the ordinary proof, which a
/// row leaves unwritten, so its legend entry explains nothing.
fn written_proof_codes(legend: &str) -> Option<String> {
    let kept: Vec<&str> = legend
        .split("; ")
        .filter(|entry| !entry.starts_with("type:"))
        .collect();
    (!kept.is_empty()).then(|| kept.join("; "))
}

fn order(key: &str) -> usize {
    if let Some(at) = ANSWER_KEYS.iter().position(|known| *known == key) {
        return at;
    }
    if let Some(at) = GAP_KEYS.iter().position(|known| *known == key) {
        return 100 + at;
    }
    200
}

fn is_cursor_key(key: &str) -> bool {
    key.ends_with("cursor")
}

/// Every entity a reply names, by id: the label a row is written under.
struct Names {
    labels: HashMap<String, String>,
}

impl Names {
    fn collect(payload: &Map<String, Value>, handles: &SessionHandles) -> Self {
        // The richest identity recorded for each id anywhere in the reply.
        let mut identity: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
        // The ids whose row says the graph holds another declaration under
        // the same label ([`crate::semantic_owners::NAME_SHARED_KEY`]).
        let mut shared: std::collections::BTreeSet<String> = Default::default();
        fn marked(value: &Value, shared: &mut std::collections::BTreeSet<String>) {
            match value {
                Value::Array(items) => items.iter().for_each(|item| marked(item, shared)),
                Value::Object(fields) => {
                    if fields.get(crate::semantic_owners::NAME_SHARED_KEY)
                        == Some(&Value::Bool(true))
                    {
                        if let Some(id) = ["entity_id", "id"]
                            .iter()
                            .find_map(|key| fields.get(*key).and_then(Value::as_str))
                            .filter(|id| is_uuid(id))
                        {
                            shared.insert(id.to_string());
                        }
                    }
                    fields.values().for_each(|item| marked(item, shared));
                }
                _ => {}
            }
        }
        marked(&Value::Object(payload.clone()), &mut shared);
        fn visit(value: &Value, identity: &mut BTreeMap<String, (String, Option<String>)>) {
            match value {
                Value::Array(items) => items.iter().for_each(|item| visit(item, identity)),
                Value::Object(fields) => {
                    let id = ["entity_id", "id"]
                        .iter()
                        .find_map(|key| fields.get(*key).and_then(Value::as_str))
                        .filter(|id| is_uuid(id));
                    let name = fields
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty());
                    if let (Some(id), Some(name)) = (id, name) {
                        let module = fields
                            .get("module")
                            .and_then(Value::as_str)
                            .filter(|module| !module.is_empty())
                            .map(str::to_string);
                        let entry = identity
                            .entry(id.to_string())
                            .or_insert_with(|| (name.to_string(), None));
                        if entry.1.is_none() && module.is_some() {
                            *entry = (name.to_string(), module);
                        }
                    }
                    // Pairs a row names another entity by: `to`/`to_name`.
                    for (id_key, name_key) in [("to", "to_name"), ("in", "in_name")] {
                        if let (Some(id), Some(name)) = (
                            fields
                                .get(id_key)
                                .and_then(Value::as_str)
                                .filter(|id| is_uuid(id)),
                            fields.get(name_key).and_then(Value::as_str),
                        ) {
                            identity
                                .entry(id.to_string())
                                .or_insert_with(|| (name.to_string(), None));
                        }
                    }
                    for (key, item) in fields {
                        if !matches!(key.as_str(), "body" | "header" | "code") {
                            visit(item, identity);
                        }
                    }
                }
                _ => {}
            }
        }
        visit(&Value::Object(payload.clone()), &mut identity);
        let mut shown: HashMap<String, usize> = HashMap::new();
        let display = |(name, module): &(String, Option<String>)| match module {
            Some(module) => format!("{module} :: {name}"),
            None => name.clone(),
        };
        for named in identity.values() {
            *shown.entry(display(named)).or_default() += 1;
        }
        let mut labels = HashMap::new();
        for (id, named) in &identity {
            let text = display(named);
            let unique = shown.get(&text) == Some(&1);
            // A label the graph holds twice is never one entity's address,
            // though this reply holds only one of the two: written bare, it
            // would be read back as whichever the connection was shown.
            let readable =
                named.1.is_some() && unique && !shared.contains(id) && handles.alias(&text, id);
            let label = if readable {
                text
            } else {
                format!("{text} #{}", handles.handle_for_uuid(id))
            };
            labels.insert(id.clone(), label);
        }
        Self { labels }
    }

    fn label(&self, id: &str) -> Option<&str> {
        self.labels.get(id).map(String::as_str)
    }
}

struct Writer<'a> {
    lines: Vec<String>,
    names: &'a Names,
    handles: &'a SessionHandles,
}

/// Values a row field takes so often that writing them says nothing.
fn ordinary(key: &str, value: &Value) -> bool {
    match (key, value) {
        ("proof", Value::String(proof)) => matches!(
            proof.as_str(),
            "type" | "type_resolved" | "target resolved by type"
        ),
        ("proven_path" | "proven", Value::Bool(true)) => true,
        ("how", Value::String(how)) => how.eq_ignore_ascii_case("calls"),
        ("role", Value::String(role)) => matches!(role.as_str(), "source" | "caller"),
        ("relationship", Value::String(text)) => text == "Contains",
        ("resolution", Value::String(text)) => text == "type_resolved",
        ("kind", Value::String(kind)) => {
            kind.eq_ignore_ascii_case("function") || kind.eq_ignore_ascii_case("method")
        }
        ("distance", _) => true,
        // Said by the row's label, which carries the id.
        (crate::semantic_owners::NAME_SHARED_KEY, _) => true,
        (_, Value::Null) => true,
        (_, Value::Array(items)) => items.is_empty(),
        _ => false,
    }
}

/// Fields a row line writes in its identity or code, never as `key=value`.
const IDENTITY_KEYS: [&str; 7] = [
    "entity_id",
    "id",
    "in",
    "name",
    "module",
    "to_name",
    "in_name",
];
const CODE_KEYS: [&str; 3] = ["code", "call_code", "signature"];
const BLOCK_KEYS: [&str; 2] = ["body", "header"];

/// The first sentence of a meaning, without its verdict word.
fn first_sentence(meaning: &str) -> (&str, &str) {
    let meaning = ["Answer: ", "Partial: ", "Complete: "]
        .iter()
        .find_map(|prefix| meaning.strip_prefix(prefix))
        .unwrap_or(meaning);
    match meaning.find(". ") {
        Some(at) => (&meaning[..=at], meaning[at + 2..].trim()),
        None => (meaning, ""),
    }
}

/// Holders of a member as a value a reach writes one line each for.
const HOLDER_LINES_MAX: usize = 3;

/// A fold's reason in a reader's words, or the reason as recorded.
fn folded_reason(reason: &str) -> &str {
    match reason {
        "call_protocol" => "calling an instance runs `__call__`",
        "constructor_dispatch" => "constructing the class runs `__init__`",
        "caller_text_not_read" => "a caller whose text was not read",
        other => other,
    }
}

/// A `searched` line without the sentence every walk carries.
fn searched_specifics(searched: &str) -> &str {
    searched
        .strip_prefix(
            "Searched recorded entity and relationship evidence; coverage is limited by the verdict.",
        )
        .unwrap_or(searched)
        .trim()
}

impl Writer<'_> {
    /// An answered callers reach: the verdict and count, one line per member
    /// with the line where it calls toward the focal, then every place the
    /// walk did not follow, then how to go on. Every gap the reply holds is
    /// written: listed and withheld frontier places, counted observations by
    /// reason, blind spots, bounds, entities that refer without calling, and
    /// what the policy does not follow.
    fn reach(&mut self, payload: &Map<String, Value>, reach: &Map<String, Value>) {
        let envelope = payload.get("_kin").and_then(Value::as_object);
        let verdict = envelope
            .and_then(|kin| kin.get("verdict"))
            .and_then(Value::as_str);
        let meaning = envelope
            .and_then(|kin| kin.get("meaning"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let (lead, _) = first_sentence(meaning);
        // A walk asked to leave tests out says so on its answer line: the
        // exclusion is the answer's scope, and what it left out is a count.
        let excluded = crate::plain_reply::tests_excluded_sentence(
            reach,
            verdict.is_some_and(|verdict| verdict == crate::plain_reply::COMPLETE),
        )
        .map(|sentence| format!(" {sentence}"))
        .unwrap_or_default();
        match verdict {
            Some(verdict) => self.push(0, format!("Answer ({verdict}): {lead}{excluded}")),
            None => self.push(0, format!("{lead}{excluded}")),
        }
        let rows = |key: &str| {
            reach
                .get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        // A reach that is not answered whole says how far each package was
        // read, as a heading over that package's members.
        let packages: Vec<Value> = reach
            .get(crate::trace_reach::PACKAGES_KEY)
            .and_then(Value::as_array)
            .filter(|_| !verdict.is_some_and(|verdict| verdict.starts_with("complete")))
            .cloned()
            .unwrap_or_default();
        let package_of = |row: &Map<String, Value>| -> Option<usize> {
            let module = row.get("module").and_then(Value::as_str)?;
            packages
                .iter()
                .enumerate()
                .filter(|(_, package)| {
                    package
                        .get("module")
                        .and_then(Value::as_str)
                        .is_some_and(|package| {
                            module == package
                                || module
                                    .strip_prefix(package)
                                    .is_some_and(|rest| rest.starts_with('.'))
                        })
                })
                .max_by_key(|(_, package)| package["module"].as_str().map_or(0, str::len))
                .map(|(at, _)| at)
        };
        let mut headed: Option<usize> = None;
        for member in rows("members") {
            if let Some(fields) = member.as_object() {
                if let Some(at) = package_of(fields).filter(|at| headed != Some(*at)) {
                    let package = &packages[at];
                    self.push(
                        0,
                        format!(
                            "{} ({}{}), {}:",
                            package["module"].as_str().unwrap_or_default(),
                            package["members"].as_u64().unwrap_or(0),
                            if package.get("role").and_then(Value::as_str) == Some("test") {
                                ", test code"
                            } else {
                                ""
                            },
                            crate::plain_reply::reach_package_state(package)
                        ),
                    );
                    headed = Some(at);
                }
                self.row(fields, 1, "trace_data_flow");
            }
        }
        // Possible callers: reached only through a call matched by a bare
        // name. The heading says what the proof fields of each row would.
        let by_name = rows(crate::trace_reach::NAME_ONLY_KEY);
        if !by_name.is_empty() {
            let total = reach
                .get(crate::trace_reach::NAME_ONLY_TOTAL_KEY)
                .and_then(Value::as_u64)
                .unwrap_or(by_name.len() as u64);
            self.push(
                0,
                format!("Matched by name alone, not proven callers ({total}):"),
            );
            for row in by_name {
                if let Some(fields) = row.as_object() {
                    let mut fields = fields.clone();
                    fields.remove("proof");
                    fields.remove("proven_path");
                    self.row(&fields, 1, "trace_data_flow");
                }
            }
        }
        let mut gaps: Vec<String> = Vec::new();
        for place in rows("frontier") {
            let Some(fields) = place.as_object() else {
                continue;
            };
            let at = fields
                .get("in")
                .and_then(Value::as_str)
                .map(|id| self.id(id))
                .or_else(|| {
                    fields
                        .get("in_name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let code = fields
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let why = fields.get("why").and_then(Value::as_str).unwrap_or("");
            let test = if fields.get("in_role").and_then(Value::as_str) == Some("test") {
                " [test]"
            } else {
                ""
            };
            let mut line = at;
            if !code.is_empty() {
                line.push_str(&format!("  {code}"));
            }
            if !why.is_empty() {
                line.push_str(&format!("  ({why})"));
            }
            gaps.push(format!("{line}{test}"));
        }
        if let Some(tests) = reach
            .get("frontier_in_test_code")
            .and_then(Value::as_object)
        {
            let sites = tests.get("sites").and_then(Value::as_u64).unwrap_or(0);
            let callers = self.scalar("callers", tests.get("callers").unwrap_or(&Value::Null));
            gaps.push(format!(
                "{sites} call sites in test code ({callers}) [test]"
            ));
        }
        if let Some(unspelled) = reach
            .get(crate::reply_budget::FRONTIER_RUN_UNSPELLED_KEY)
            .and_then(Value::as_object)
        {
            let count = unspelled.get("count").and_then(Value::as_u64).unwrap_or(0);
            let members: Vec<&str> = unspelled
                .get("members")
                .and_then(Value::as_array)
                .map(|members| {
                    members
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|name| name.rsplit(['.', ':']).next().unwrap_or(name))
                        .collect()
                })
                .unwrap_or_default();
            gaps.push(format!(
                "{count} {} the language can run without a call that spells {}, through a \
                 decorator or a protocol, and no record lists where: {}",
                if count == 1 { "member" } else { "members" },
                if count == 1 {
                    "its name"
                } else {
                    "their names"
                },
                members.join(", ")
            ));
        }
        let withheld = rows("frontier_withheld");
        if !withheld.is_empty() {
            let named: Vec<String> = withheld.iter().map(|row| self.inline(row)).collect();
            gaps.push(format!(
                "{} more places not listed here: {}",
                withheld.len(),
                named.join("; ")
            ));
        }
        if let Some(folded) = reach.get("frontier_folded").and_then(Value::as_object) {
            let count = folded.get("count").and_then(Value::as_u64).unwrap_or(0);
            if count > 0 {
                let unit = folded
                    .get("unit")
                    .and_then(Value::as_str)
                    .unwrap_or("observations");
                let unit = match count {
                    1 => unit.strip_suffix('s').unwrap_or(unit),
                    _ => unit,
                };
                let reasons: Vec<String> = folded
                    .get("by_reason")
                    .and_then(Value::as_object)
                    .map(|reasons| {
                        reasons
                            .iter()
                            .map(|(reason, n)| format!("{} {n}", folded_reason(reason)))
                            .collect()
                    })
                    .unwrap_or_default();
                gaps.push(format!(
                    "{count} {unit} counted by reason, not listed: {}",
                    reasons.join(", ")
                ));
            }
        }
        if let Some(blind) = reach.get("blind_spots").and_then(Value::as_object) {
            let mut counted = Vec::new();
            for (key, words) in [
                ("unkeyed_lookups", "lookups by a name built at run time"),
                ("calls_through_values", "calls through values"),
            ] {
                if let Some(n) = blind.get(key).and_then(Value::as_u64).filter(|n| *n > 0) {
                    counted.push(format!("{n} {words}"));
                }
            }
            if let Some(holders) = blind.get("value_holders").and_then(Value::as_object) {
                let mut holders = holders.clone();
                holders.remove("why");
                // A member that holds another as a value is a place, written
                // as one; what is left of the census is counts.
                if let Some(Value::Array(members)) = holders.remove("in_members") {
                    for place in &members {
                        gaps.extend(self.held_in(place));
                    }
                }
                // What the census leaves beside its listed holders, in
                // words: holders no row names, or a census that is not whole.
                let unlisted = holders
                    .remove("unlisted")
                    .and_then(|unlisted| unlisted.as_u64())
                    .unwrap_or(0);
                let incomplete =
                    holders.remove("complete").and_then(|whole| whole.as_bool()) == Some(false);
                if unlisted > 0 {
                    gaps.push(format!(
                        "{unlisted} more {} a member as a value and {} not listed",
                        if unlisted == 1 {
                            "place holds"
                        } else {
                            "places hold"
                        },
                        if unlisted == 1 { "is" } else { "are" },
                    ));
                } else if incomplete {
                    gaps.push(
                        "where a member is held as a value was not measured for every member"
                            .to_string(),
                    );
                }
                holders.remove("rows");
                if !holders.is_empty() {
                    counted.push(format!(
                        "value holders: {}",
                        self.inline(&Value::Object(holders))
                    ));
                }
            }
            for (key, value) in blind {
                if !matches!(
                    key.as_str(),
                    "unkeyed_lookups" | "calls_through_values" | "value_holders" | "why"
                ) {
                    counted.push(format!("{key} {}", self.scalar(key, value)));
                }
            }
            if !counted.is_empty() {
                gaps.push(format!("counted, not followed: {}", counted.join("; ")));
            }
        }
        for bound in rows("bounds") {
            gaps.push(self.bound(&bound));
        }
        // A holder of a member as a value is one line, for the first few.
        // Past that they are one kind of place, and the rest are counted.
        let referrers = rows("also_referenced_by");
        let mut holders: Vec<String> = Vec::new();
        let mut named: Vec<String> = Vec::new();
        for referrer in &referrers {
            for line in self.referrer(referrer) {
                if line.starts_with("held as a value in ") {
                    holders.push(line);
                } else {
                    named.push(line);
                }
            }
        }
        let uncounted = holders.len().saturating_sub(HOLDER_LINES_MAX);
        holders.truncate(HOLDER_LINES_MAX);
        gaps.extend(holders);
        if uncounted > 0 {
            gaps.push(format!(
                "{uncounted} more {} a member as a value",
                if uncounted == 1 {
                    "place holds"
                } else {
                    "places hold"
                }
            ));
        }
        gaps.extend(named);
        let referenced_total = reach
            .get("also_referenced_total")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if referenced_total > referrers.len() as u64 {
            gaps.push(format!(
                "{} more referrers counted, not listed",
                referenced_total - referrers.len() as u64
            ));
        }
        if let Some(imported) = reach
            .get("also_imported_by")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
        {
            gaps.push(format!("imported without a call by {imported} modules"));
        }
        // What the walk never follows by design explains the places listed
        // above it. With no place listed it explains nothing: under a
        // complete answer it was the one line left to wonder about.
        if let Some(rule) = reach
            .get("not_followed")
            .or_else(|| {
                reach
                    .get("policy")
                    .and_then(|policy| policy.get("not_followed"))
            })
            .and_then(Value::as_str)
            .filter(|_| !gaps.is_empty())
        {
            gaps.push(format!("by design: {rule}"));
        }
        if let Some(searched) = envelope
            .and_then(|kin| kin.get("searched"))
            .and_then(Value::as_str)
            .map(searched_specifics)
            .filter(|searched| !searched.is_empty())
        {
            gaps.push(searched.to_string());
        }
        if !gaps.is_empty() {
            self.push(0, "Not followed:");
            for gap in gaps {
                self.push(1, gap);
            }
        }
        // Everything else the reply states, written as any reply is.
        for (key, value) in payload {
            if matches!(
                key.as_str(),
                "_kin" | "reach" | "direction" | "focal" | "target"
            ) || is_cursor_key(key)
                // The answer line names the focal, the scope and the walk; a
                // text reach ends on its rows and the places left.
                || crate::reply_budget::ANSWERED_REACH_LEAVES_OUT.contains(&key.as_str())
            {
                continue;
            }
            if key == "readings" {
                let others: Vec<Value> = value
                    .as_array()
                    .map(|readings| {
                        readings
                            .iter()
                            .filter(|reading| {
                                !reading
                                    .get("key")
                                    .and_then(Value::as_str)
                                    .is_some_and(|key| {
                                        matches!(key, "reach" | "direction")
                                            || crate::reply_budget::ANSWERED_REACH_LEAVES_OUT
                                                .contains(&key)
                                    })
                            })
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                if !others.is_empty() {
                    self.field(key, &Value::Array(others), 0, "trace_data_flow");
                }
                continue;
            }
            self.field(key, value, 0, "trace_data_flow");
        }
        // A page that holds every row of the reach invites no further call:
        // its cursor would serve display detail, and the notes about the
        // chain and how to narrow it describe something the reader was not
        // shown. A page missing rows keeps its delivery note and cursor.
        let whole = crate::trace_reach::rows_delivered(reach);
        if let Some(envelope) = envelope {
            for (key, value) in envelope {
                if matches!(key.as_str(), "verdict" | "meaning" | "searched")
                    || JSON_ONLY_NOTES.contains(&key.as_str())
                    || matches!(
                        key.as_str(),
                        crate::reply_budget::CHAIN_LEFT_OUT_KEY | crate::reply_budget::NARROW_KEY
                    )
                    || (whole && key == "delivered")
                {
                    continue;
                }
                self.field(key, value, 0, "trace_data_flow");
            }
        }
        if !whole {
            for (key, value) in payload.iter().filter(|(key, _)| is_cursor_key(key)) {
                self.field(key, value, 0, "trace_data_flow");
            }
        }
    }

    /// A bound as one plain line: what stopped the reading and what it left
    /// unread, in the bound's own sentence, then where it stopped. The
    /// sentence already says which limit it was and how much was left.
    fn bound(&self, bound: &Value) -> String {
        let Some(detail) = bound.get("detail").and_then(Value::as_str) else {
            return format!("walk bound: {}", self.inline(bound));
        };
        let mut rest = bound.as_object().cloned().unwrap_or_default();
        for key in ["detail", "left", "which", "limit"] {
            rest.remove(key);
        }
        rest.retain(|_, value| {
            !value.is_null() && value.as_array().is_none_or(|rows| !rows.is_empty())
        });
        if rest.is_empty() {
            format!("walk bound: {detail}")
        } else {
            format!(
                "walk bound: {detail} ({})",
                self.inline(&Value::Object(rest))
            )
        }
    }

    /// One entity that refers to reach members without calling them, as one
    /// short line for each way it does: the members it holds as a value,
    /// which a call through that value could run, and the ones it only
    /// names. A site's line of code, which spells the same name, is left to
    /// `source` on the referrer.
    fn referrer(&self, row: &Value) -> Vec<String> {
        let whole = || {
            vec![format!(
                "referenced without a call by: {}",
                self.inline(row)
            )]
        };
        let Some(fields) = row.as_object() else {
            return whole();
        };
        let mut who = match ["entity_id", "id"]
            .iter()
            .find_map(|key| fields.get(*key).and_then(Value::as_str))
        {
            Some(id) => self.id(id),
            None => fields
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        };
        let refs: Vec<&Map<String, Value>> = fields
            .get("refs")
            .and_then(Value::as_array)
            .map(|refs| refs.iter().filter_map(Value::as_object).collect())
            .unwrap_or_default();
        if who.is_empty() || refs.is_empty() {
            return whole();
        }
        if let Some(kind) = fields.get("kind").filter(|kind| !ordinary("kind", kind)) {
            who.push_str(&format!(" ({})", self.scalar("kind", kind)));
        }
        let (held, named): (Vec<_>, Vec<_>) = refs
            .into_iter()
            .partition(|reference| reference.get("value").is_some_and(Value::is_string));
        let mut lines = Vec::new();
        if !held.is_empty() {
            lines.push(format!(
                "held as a value in {who}: {}",
                self.referred(&held)
            ));
        }
        if !named.is_empty() {
            lines.push(format!(
                "referenced without a call by {who}: {}",
                self.referred(&named)
            ));
        }
        lines
    }

    /// A member that holds another as a value, from the value-holder census.
    fn held_in(&self, place: &Value) -> Option<String> {
        let fields = place.as_object()?;
        let who = match fields.get("id").and_then(Value::as_str) {
            Some(id) => self.id(id),
            None => fields.get("name").and_then(Value::as_str)?.to_string(),
        };
        Some(format!(
            "held as a value in {who}: {}",
            self.referred(&[fields])
        ))
    }

    /// The entities some refs point at, each once: by the name the reply
    /// states, or by its label when two of them share that name. A ref that
    /// holds its entity as a value says how, once after the names when every
    /// ref holds its entity the same way.
    fn referred(&self, refs: &[&Map<String, Value>]) -> String {
        // (the entity's id, or its name when the ref has no id; name; how)
        let mut entries: Vec<(String, String, Option<String>)> = Vec::new();
        for reference in refs {
            let id = reference.get("to").and_then(Value::as_str);
            let name = reference
                .get("to_name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| id.map(|id| self.id(id)))
                .unwrap_or_default();
            let key = id.map_or_else(|| name.clone(), str::to_string);
            let how = reference
                .get("value")
                .and_then(Value::as_str)
                .map(str::to_string);
            if !entries
                .iter()
                .any(|(seen, _, seen_how)| *seen == key && *seen_how == how)
            {
                entries.push((key, name, how));
            }
        }
        let shared_how = entries
            .first()
            .and_then(|(_, _, how)| how.clone())
            .filter(|how| {
                entries
                    .iter()
                    .all(|(_, _, other)| other.as_ref() == Some(how))
            });
        let written: Vec<String> = entries
            .iter()
            .map(|(key, name, how)| {
                let shared_name = entries
                    .iter()
                    .any(|(other, other_name, _)| other != key && other_name == name);
                let mut text = if shared_name && is_uuid(key) {
                    self.id(key)
                } else {
                    name.clone()
                };
                if let (None, Some(how)) = (&shared_how, how) {
                    text.push_str(&format!(" ({how})"));
                }
                text
            })
            .collect();
        match shared_how {
            Some(how) => format!("{} ({how})", written.join(", ")),
            None => written.join(", "),
        }
    }

    /// A small object on one line: its label or name, then its other fields.
    fn inline(&self, value: &Value) -> String {
        let Some(fields) = value.as_object() else {
            return self.scalar("", value);
        };
        let id = ["entity_id", "id"]
            .iter()
            .find_map(|key| fields.get(*key).and_then(Value::as_str));
        let mut parts = Vec::new();
        if let Some(id) = id {
            parts.push(self.id(id));
        } else if let Some(name) = fields.get("name").and_then(Value::as_str) {
            parts.push(name.to_string());
        }
        for (key, value) in fields {
            if matches!(
                key.as_str(),
                "entity_id" | "id" | "name" | "to_name" | "in_name"
            ) || ordinary(key, value)
            {
                continue;
            }
            match value {
                Value::Array(items) if items.iter().any(Value::is_object) => {
                    let inner: Vec<String> = items.iter().map(|item| self.inline(item)).collect();
                    parts.push(format!("{key}: {}", inner.join(", ")));
                }
                Value::Object(_) => parts.push(format!("{key}: {}", self.inline(value))),
                _ => parts.push(format!("{key}={}", self.scalar(key, value))),
            }
        }
        parts.join(" ")
    }

    fn push(&mut self, depth: usize, text: impl AsRef<str>) {
        let text = text.as_ref();
        self.lines.push(format!("{}{text}", "  ".repeat(depth)));
    }

    fn lead(&mut self, envelope: Option<&Map<String, Value>>) {
        self.lead_at(envelope, 0);
    }

    fn lead_at(&mut self, envelope: Option<&Map<String, Value>>, depth: usize) {
        let Some(envelope) = envelope else {
            return;
        };
        let verdict = envelope.get("verdict").and_then(Value::as_str);
        let meaning = envelope
            .get("meaning")
            .and_then(Value::as_str)
            .unwrap_or("");
        let meaning = ["Answer: ", "Partial: ", "Complete: "]
            .iter()
            .find_map(|prefix| meaning.strip_prefix(prefix))
            .unwrap_or(meaning);
        match verdict {
            Some(verdict) => self.push(depth, format!("Answer ({verdict}): {meaning}")),
            None if !meaning.is_empty() => self.push(depth, meaning),
            None => {}
        }
    }

    /// What an envelope says besides its verdict and meaning, which the lead
    /// line wrote, and the notes on the JSON shape only.
    fn envelope_notes(&mut self, envelope: Option<&Map<String, Value>>, depth: usize, tool: &str) {
        let Some(envelope) = envelope else {
            return;
        };
        for (key, value) in envelope {
            if matches!(key.as_str(), "verdict" | "meaning")
                || JSON_ONLY_NOTES.contains(&key.as_str())
            {
                continue;
            }
            if key == crate::query_reply::PROOF_CODES_KEY {
                if let Some(legend) = value.as_str().and_then(written_proof_codes) {
                    self.push(depth, format!("{key}: {legend}"));
                }
                continue;
            }
            self.field(key, value, depth, tool);
        }
    }

    /// One answer of a reply that answers several, such as a name several
    /// entities share: its own verdict line, then its fields beneath it.
    fn section(&mut self, fields: &Map<String, Value>, depth: usize, tool: &str) {
        let envelope = fields.get("_kin").and_then(Value::as_object);
        self.lead_at(envelope, depth);
        let mut keys: Vec<&String> = fields
            .keys()
            .filter(|key| *key != "_kin" && !is_cursor_key(key))
            .collect();
        keys.sort_by_key(|key| order(key));
        for key in keys {
            self.field(key, &fields[key.as_str()], depth + 1, tool);
        }
        self.envelope_notes(envelope, depth + 1, tool);
        for (key, value) in fields.iter().filter(|(key, _)| is_cursor_key(key)) {
            self.field(key, value, depth + 1, tool);
        }
    }

    /// One id written as its row label, or its short handle.
    fn id(&self, id: &str) -> String {
        match self.names.label(id) {
            Some(label) => label.to_string(),
            None if is_uuid(id) => self.handles.handle_for_uuid(id),
            None => id.to_string(),
        }
    }

    fn scalar(&self, key: &str, value: &Value) -> String {
        match value {
            Value::String(text) if is_uuid(text) => self.id(text),
            Value::String(text) => {
                // Cursors, tickets and change ids under their short handles.
                let mut wrapped = Value::Object(Map::from_iter([(key.to_string(), value.clone())]));
                self.handles.shorten_value(&mut wrapped);
                wrapped[key].as_str().unwrap_or(text).to_string()
            }
            Value::Array(items)
                if items
                    .iter()
                    .all(|item| !item.is_object() && !item.is_array()) =>
            {
                items
                    .iter()
                    .map(|item| self.scalar(key, item))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
            other => {
                let mut compact = other.clone();
                self.handles.shorten_value(&mut compact);
                compact.to_string()
            }
        }
    }

    fn is_row(value: &Value) -> bool {
        value.as_object().is_some_and(|fields| {
            fields.contains_key("entity_id")
                || fields.contains_key("id")
                || fields.contains_key("name")
                || fields.contains_key("code")
                || fields.contains_key("in")
        })
    }

    /// A field at `depth`: a scalar as `key: value`, rows one per line under
    /// `key (n):`, an object as its scalars on one line and its lists below.
    fn field(&mut self, key: &str, value: &Value, depth: usize, tool: &str) {
        match value {
            Value::Null => {}
            Value::Array(items) if items.is_empty() => self.push(depth, format!("{key}: none")),
            Value::Array(items)
                if items
                    .iter()
                    .all(|item| !item.is_object() && !item.is_array()) =>
            {
                let line = self.scalar(key, value);
                self.push(depth, format!("{key}: {line}"));
            }
            Value::Array(items)
                if key == "answers"
                    && items
                        .iter()
                        .all(|item| item.get("_kin").is_some_and(Value::is_object)) =>
            {
                self.push(depth, format!("{key} ({}):", items.len()));
                for item in items {
                    self.section(
                        item.as_object().expect("an answer is an object"),
                        depth + 1,
                        tool,
                    );
                }
            }
            Value::Array(items) if key == "readings" => {
                for reading in items {
                    match (
                        reading.get("key").and_then(Value::as_str),
                        reading.get("value"),
                    ) {
                        (Some(name), Some(value)) => self.field(name, value, depth, tool),
                        _ => self.push(depth, self.scalar(key, reading)),
                    }
                }
            }
            Value::Array(items) => {
                self.push(depth, format!("{key} ({}):", items.len()));
                for item in items {
                    if Self::is_row(item) {
                        self.row(
                            item.as_object().expect("a row is an object"),
                            depth + 1,
                            tool,
                        );
                    } else if let Some(fields) = item.as_object() {
                        self.object(None, fields, depth + 1, tool);
                    } else {
                        self.push(depth + 1, self.scalar(key, item));
                    }
                }
            }
            Value::Object(fields) if Self::is_row(value) && key != "reach" => {
                self.push(depth, format!("{key}:"));
                self.row(fields, depth + 1, tool);
            }
            Value::Object(fields) => self.object(Some(key), fields, depth, tool),
            Value::String(text) if text.contains('\n') => {
                self.push(depth, format!("{key}:"));
                for line in text.lines() {
                    self.push(depth + 1, line);
                }
            }
            scalar => {
                let line = self.scalar(key, scalar);
                self.push(depth, format!("{key}: {line}"));
            }
        }
    }

    /// An object: its scalars as `key=value` on one line under its name, then
    /// its lists and objects, each under `name.key`.
    fn object(
        &mut self,
        name: Option<&str>,
        fields: &Map<String, Value>,
        depth: usize,
        tool: &str,
    ) {
        // An empty list among an object's scalars would read `key=`, which
        // says nothing; a list's own line says `none` where it is the answer.
        let scalars: Vec<String> = fields
            .iter()
            .filter(|(_, value)| {
                !value.is_object()
                    && !value.as_array().is_some_and(|items| {
                        items.is_empty()
                            || items.iter().any(|item| item.is_object() || item.is_array())
                    })
            })
            .filter(|(_, value)| !value.is_null())
            .map(|(key, value)| format!("{key}={}", self.scalar(key, value)))
            .collect();
        match name {
            Some(name) if !scalars.is_empty() => {
                self.push(depth, format!("{name}: {}", scalars.join(" ")))
            }
            Some(_) => {}
            None if !scalars.is_empty() => self.push(depth, scalars.join(" ")),
            None => {}
        }
        let mut keys: Vec<&String> = fields.keys().collect();
        keys.sort_by_key(|key| order(key));
        for key in keys {
            let value = &fields[key.as_str()];
            let nested = value.is_object()
                || value.as_array().is_some_and(|items| {
                    items.iter().any(|item| item.is_object() || item.is_array())
                });
            if nested {
                let path = match name {
                    Some(name) => format!("{name}.{key}"),
                    None => key.clone(),
                };
                self.field(&path, value, depth, tool);
            }
        }
    }

    /// One row: its identity, its code, then what else it states.
    fn row(&mut self, fields: &Map<String, Value>, depth: usize, tool: &str) {
        let id = ["entity_id", "id", "in"]
            .iter()
            .find_map(|key| fields.get(*key).and_then(Value::as_str))
            .filter(|id| is_uuid(id) || !fields.contains_key("in"));
        let mut parts: Vec<String> = Vec::new();
        let identity = match id {
            Some(id) if is_uuid(id) => Some(self.id(id)),
            Some(id) => Some(id.to_string()),
            None => fields.get("name").and_then(Value::as_str).map(|name| {
                match fields.get("module").and_then(Value::as_str) {
                    Some(module) => format!("{module} :: {name}"),
                    None => name.to_string(),
                }
            }),
        };
        if let Some(identity) = identity {
            parts.push(identity);
        }
        if let Some(kind) = fields.get("kind").filter(|kind| !ordinary("kind", kind)) {
            parts.push(format!("({})", self.scalar("kind", kind)));
        }
        let mut code_written = false;
        for key in CODE_KEYS {
            if let Some(code) = fields.get(key).and_then(Value::as_str) {
                if key == "signature" && code_written {
                    continue;
                }
                if !code.trim().is_empty() && !code.contains('\n') {
                    parts.push(code.trim().to_string());
                    code_written = true;
                }
            }
            // Every other call the row makes follows its first, each written
            // the way the first is.
            if key == "call_code" {
                for code in fields
                    .get(crate::query_reply::MORE_CALL_CODE_KEY)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    parts.push(code.trim().to_string());
                }
            }
        }
        // `via` is the next step toward the focal; the code line usually
        // spells it, and the label is written only when it does not.
        let via = fields.get("via").and_then(Value::as_str);
        for (key, value) in fields {
            if IDENTITY_KEYS.contains(&key.as_str())
                || CODE_KEYS.contains(&key.as_str())
                    && value.as_str().is_some_and(|code| !code.contains('\n'))
                || BLOCK_KEYS.contains(&key.as_str())
                || key == "kind"
                || key == crate::query_reply::MORE_CALL_CODE_KEY
                || ordinary(key, value)
            {
                continue;
            }
            if key == "via" {
                if let Some(via) = via {
                    let label = self.id(via);
                    let short = label
                        .split(" :: ")
                        .last()
                        .and_then(|name| name.split(" #").next())
                        .and_then(|name| name.rsplit('.').next())
                        .unwrap_or_default()
                        .to_string();
                    let spelled = !short.is_empty()
                        && fields
                            .get("code")
                            .and_then(Value::as_str)
                            .is_some_and(|code| code.contains(&short));
                    if !spelled {
                        parts.push(format!("via={label}"));
                    }
                }
                continue;
            }
            if key == "in_role" || key == "role" {
                parts.push(format!("[{}]", self.scalar(key, value)));
                continue;
            }
            if value.is_object()
                || value
                    .as_array()
                    .is_some_and(|items| items.iter().any(Value::is_object))
            {
                continue;
            }
            parts.push(format!("{key}={}", self.scalar(key, value)));
        }
        self.push(depth, parts.join("  "));
        for key in BLOCK_KEYS.iter().chain(CODE_KEYS.iter()) {
            if let Some(text) = fields.get(*key).and_then(Value::as_str) {
                if text.contains('\n') || BLOCK_KEYS.contains(key) {
                    for line in text.lines() {
                        self.push(depth + 1, line);
                    }
                }
            }
        }
        for (key, value) in fields {
            if value.is_object()
                || value
                    .as_array()
                    .is_some_and(|items| items.iter().any(Value::is_object))
            {
                self.field(key, value, depth + 1, tool);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FOCAL: &str = "b29b1f12-be53-4b09-9111-aa98a3c332e4";
    const MEMBER: &str = "535ce03c-ed5e-46fa-8603-8de10f52f6d4";
    const OTHER: &str = "7c49126d-6e98-4fc4-9f61-ccec9c5ba038";
    const TEST: &str = "60075ec4-42a1-4452-b310-9e64469fe43e";

    fn reach() -> Map<String, Value> {
        json!({
            "_kin": {"verdict": "complete except the sites listed below",
                     "meaning": "Answer: 2 functions can end up calling isFiberSuspenseAndTimedOut. reach.frontier lists 1 place.",
                     "rows": "Each row names its entity_id; ...",
                     "searched": "Searched recorded entity and relationship evidence; coverage is limited by the verdict. None of the 3 places that build a name at run time spells it."},
            "direction": "callers",
            "next_cursor": "eyJ2IjoxLCJzbmFwc2hvdCI6IjliNjM0YjgxLTkwNTEtNGQzNy05YTYzLWYzOWJjMzNiZTY0YSJ9",
            "reach": {
                "members": [
                    {"id": MEMBER, "name": "collectScopedNodes", "module": "react-reconciler.ReactFiberScope",
                     "code": "if (isFiberSuspenseAndTimedOut(node)) {", "via": FOCAL, "proof": "type_resolved",
                     "proven_path": true, "distance": 1},
                    {"id": OTHER, "name": "collectScopedNodesFromChildren", "module": "react-reconciler.ReactFiberScope",
                     "code": "collectScopedNodes(child, fn, scopedNodes);", "via": MEMBER, "proof": "name_only",
                     "proven_path": true, "distance": 2},
                ],
                "frontier": [
                    {"in": TEST, "in_name": "test", "in_role": "test", "code": "scope.DO_NOT_USE_queryAllNodes(fn)",
                     "to": OTHER, "to_name": "collectScopedNodesFromChildren", "why": "a call through a value", "kind": "value_call"},
                ],
            },
            "readings": [{"key": "reach", "value": {"members_total": 2, "frontier_total": 1, "state": "open",
                "bounds": [], "also_referenced_total": 0,
                "target": {"entity_id": FOCAL, "name": "isFiberSuspenseAndTimedOut"},
                "frontier_folded": {"by_reason": {"constructor_dispatch": 5}, "count": 5, "unit": "observations",
                    "why": "a long glossary"},
                "policy": {"follows": ["Calls:type_resolved"], "not_followed": "unproved dispatch"}}}],
        })
        .as_object()
        .unwrap()
        .clone()
    }

    /// A bound is one plain line: its own sentence, which already says which
    /// limit it was and how much it left, then where it stopped. No
    /// `left=3 which=time_budget` for a reader to decode.
    #[test]
    fn a_walk_bound_is_one_plain_sentence_and_where_it_stopped() {
        let mut reply = reach();
        reply["readings"][0]["value"]["bounds"] = json!([{
            "which": "frontier_reads_max", "limit": 24, "left": 2,
            "at": [{"id": MEMBER, "name": "collectScopedNodes"}],
            "detail": "the closure reads the unresolved calls of at most 24 members, so 2 more \
                       were not read and calls that could reach them are not in frontier",
        }]);
        reply["readings"][0]["value"]["state"] = json!("bounded");
        let text = render(&reply, "trace_data_flow", &SessionHandles::default());
        let line = text
            .lines()
            .find(|line| line.trim_start().starts_with("walk bound:"))
            .unwrap_or_else(|| panic!("{text}"));
        assert_eq!(
            line.trim_start(),
            "walk bound: the closure reads the unresolved calls of at most 24 members, so 2 more \
             were not read and calls that could reach them are not in frontier (at: \
             react-reconciler.ReactFiberScope :: collectScopedNodes)"
        );
        assert!(!text.contains("which="), "{text}");
        assert!(!text.contains("left="), "{text}");
    }

    #[test]
    fn a_reach_reads_as_a_verdict_line_rows_and_the_places_left() {
        let handles = SessionHandles::default();
        let text = render(&reach(), "trace_data_flow", &handles);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "Answer (complete except the sites listed below): 2 functions can end up calling isFiberSuspenseAndTimedOut."
        );
        assert_eq!(
            lines[1],
            "  react-reconciler.ReactFiberScope :: collectScopedNodes  if (isFiberSuspenseAndTimedOut(node)) {"
        );
        assert_eq!(
            lines[2],
            "  react-reconciler.ReactFiberScope :: collectScopedNodesFromChildren  collectScopedNodes(child, fn, scopedNodes);  proof=name_only",
            "an unusual proof is written, a via the code spells is not"
        );
        assert_eq!(lines[3], "Not followed:");
        assert_eq!(
            lines[4],
            "  test #60075ec4  scope.DO_NOT_USE_queryAllNodes(fn)  (a call through a value) [test]"
        );
        assert!(
            text.contains(
                "  5 observations counted by reason, not listed: constructing the class runs \
                 `__init__` 5"
            ),
            "{text}"
        );
        assert!(text.contains("  by design: unproved dispatch"), "{text}");
        assert!(
            !text.contains("Searched recorded entity"),
            "the boilerplate goes: {text}"
        );
        assert!(
            text.contains("  None of the 3 places that build a name at run time spells it."),
            "{text}"
        );
        assert!(!text.contains("Each row names its entity_id"), "{text}");
        assert!(!text.contains(MEMBER), "no full id is written: {text}");
        // The page holds every row, so it writes no cursor to call again.
        assert!(!text.contains("next_cursor"), "{text}");
        // A page that is missing rows does.
        let mut missing = reach();
        missing["readings"][0]["value"]["members_total"] = json!(3);
        let text = render(&missing, "trace_data_flow", &handles);
        let lines: Vec<&str> = text.lines().collect();
        let cursor_line = lines.last().unwrap();
        assert!(cursor_line.starts_with("next_cursor: p"), "{cursor_line}");

        // Every label and cursor the reply wrote reads back as its value.
        let page = cursor_line.trim_start_matches("next_cursor: ");
        let mut call = json!({"entity_id": "react-reconciler.ReactFiberScope :: collectScopedNodes", "cursor": page});
        assert!(handles.expand_arguments(&mut call));
        assert_eq!(call["entity_id"], MEMBER);
        assert!(call["cursor"].as_str().unwrap().starts_with("eyJ"));
        let mut call = json!({"focal": "test #60075ec4"});
        assert!(handles.expand_arguments(&mut call));
        assert_eq!(call["focal"], TEST);
    }

    /// Each entity that refers to members without calling them is one short
    /// line for each way it does, and a member that holds another as a value
    /// is a place of its own, never a field of a count.
    #[test]
    fn referrers_and_value_holders_read_one_short_line_each() {
        const HOLDER: &str = "5b2f692b-0d1e-4f2a-8b3c-9d0e1f2a3b08";
        const MODULE: &str = "42c0b4be-8b83-470f-a99b-700ec9577019";
        let handles = SessionHandles::default();
        let mut payload = reach();
        payload["reach"]["also_referenced_by"] = json!([
            {"id": MODULE, "name": "ReactFiberScope", "kind": "Module",
             "refs": [{"how": "reference", "to": FOCAL, "to_name": "isFiberSuspenseAndTimedOut",
                       "code": "import {isFiberSuspenseAndTimedOut} from './ReactFiberTreeReflection';"}]},
            {"id": HOLDER, "name": "createScopeInstance", "kind": "Function", "refs": [
                {"how": "reference", "to": MEMBER, "to_name": "collectScopedNodes",
                 "code": "collectScopedNodes,", "value": "collection"},
                {"how": "reference", "to": OTHER, "to_name": "collectScopedNodesFromChildren",
                 "code": "return collectScopedNodesFromChildren;", "value": "return"},
                {"how": "reference", "to": FOCAL, "to_name": "isFiberSuspenseAndTimedOut",
                 "code": "typeof isFiberSuspenseAndTimedOut"}]},
        ]);
        payload["readings"][0]["value"]["also_referenced_total"] = json!(2);
        payload["readings"][0]["value"]["blind_spots"] = json!({
            "calls_through_values": 17,
            "value_holders": {"rows": 1, "complete": false, "unlisted": 2, "why": "a long glossary",
                "in_members": [{"id": OTHER, "name": "collectScopedNodesFromChildren",
                    "to": MEMBER, "to_name": "collectScopedNodes", "line": 12}]}});
        let text = render(&payload, "trace_data_flow", &handles);
        for line in [
            "  referenced without a call by ReactFiberScope #42c0b4be (Module): isFiberSuspenseAndTimedOut",
            // How each member is held is said per name when it differs.
            "  held as a value in createScopeInstance #5b2f692b: collectScopedNodes (collection), collectScopedNodesFromChildren (return)",
            "  referenced without a call by createScopeInstance #5b2f692b: isFiberSuspenseAndTimedOut",
            "  held as a value in react-reconciler.ReactFiberScope :: collectScopedNodesFromChildren: collectScopedNodes",
            // What is left of the census is said in words, and a count
            // stays a count.
            "  2 more places hold a member as a value and are not listed",
            "  counted, not followed: 17 calls through values",
        ] {
            assert!(text.lines().any(|written| written == line), "{line}\n{text}");
        }
        assert!(!text.contains("a long glossary"), "{text}");
        assert!(
            !text.contains("typeof"),
            "a site's code is left to source: {text}"
        );

        // The same member held twice the same way is named once, and two
        // members that share a name are told apart by their labels.
        const TWIN: &str = "9a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";
        payload["reach"]["also_referenced_by"] = json!([
            {"id": HOLDER, "name": "createScopeInstance", "refs": [
                {"how": "reference", "to": MEMBER, "to_name": "collectScopedNodes", "value": "collection"},
                {"how": "reference", "to": MEMBER, "to_name": "collectScopedNodes", "value": "collection"},
                {"how": "reference", "to": TWIN, "to_name": "collectScopedNodes", "value": "collection"}]},
        ]);
        payload["readings"][0]["value"]["also_referenced_total"] = json!(1);
        let text = render(&payload, "trace_data_flow", &handles);
        assert!(
            text.contains("  held as a value in createScopeInstance #5b2f692b: react-reconciler.ReactFiberScope :: collectScopedNodes, collectScopedNodes #9a1b2c3d (collection)\n"),
            "{text}"
        );

        // A census that names only members holding a value writes no count.
        payload["readings"][0]["value"]["blind_spots"] = json!({"value_holders": {
            "in_members": [{"id": OTHER, "name": "collectScopedNodesFromChildren",
                "to": MEMBER, "to_name": "collectScopedNodes", "line": 12}]}});
        let text = render(&payload, "trace_data_flow", &handles);
        assert!(!text.contains("counted, not followed"), "{text}");
        assert!(
            text.contains("  held as a value in react-reconciler.ReactFiberScope :: collectScopedNodesFromChildren: collectScopedNodes\n"),
            "{text}"
        );
    }

    #[test]
    fn a_walk_that_is_not_an_answered_reach_is_written_field_by_field() {
        let handles = SessionHandles::default();
        let payload = json!({
            "_kin": {"verdict": "complete", "meaning": "Answer: this is the route from a to b."},
            "chain": [{"step": 1, "parent_step": 0, "entity_id": MEMBER, "name": "a", "module": "m",
                       "how": "Calls", "code": "b()"}],
            "direction": "calls",
        });
        let text = render(payload.as_object().unwrap(), "trace_data_flow", &handles);
        assert!(
            text.starts_with(
                "Answer (complete): this is the route from a to b.\nchain (1):\n  m :: a  b()"
            ),
            "{text}"
        );
        assert!(text.contains("step=1"), "{text}");
    }

    #[test]
    fn a_name_without_a_module_or_shared_by_two_ids_carries_its_handle() {
        let handles = SessionHandles::default();
        let payload = json!({
            "_kin": {"verdict": "complete", "meaning": "Answer: two."},
            "results": [
                {"entity_id": MEMBER, "name": "dumps", "module": "flask.json"},
                {"entity_id": OTHER, "name": "dumps", "module": "flask.json"},
                {"entity_id": TEST, "name": "loads"},
            ],
        });
        let text = render(payload.as_object().unwrap(), "semantic_locate", &handles);
        assert!(text.contains("flask.json :: dumps #535ce03c"), "{text}");
        assert!(text.contains("flask.json :: dumps #7c49126d"), "{text}");
        assert!(text.contains("loads #60075ec4"), "{text}");
        let mut call = json!({"entity_id": "flask.json :: dumps"});
        assert!(
            !handles.expand_arguments(&mut call),
            "a shared name names neither"
        );
        let mut call = json!({"entity_id": "7c49126d"});
        assert!(handles.expand_arguments(&mut call));
        assert_eq!(call["entity_id"], OTHER);
    }

    /// A row the graph marks as one of several declarations under its label
    /// carries its id although the reply holds none of the others, and the
    /// connection does not learn the bare label as that one.
    #[test]
    fn a_name_the_graph_holds_twice_carries_its_handle_though_the_reply_holds_one() {
        let handles = SessionHandles::default();
        let payload = json!({
            "_kin": {"verdict": "complete", "meaning": "Answer: two."},
            "results": [
                {"entity_id": MEMBER, "name": "App.debug", "module": "flask.sansio.app",
                 "name_shared": true},
                {"entity_id": OTHER, "name": "App.run", "module": "flask.sansio.app"},
            ],
        });
        let text = render(payload.as_object().unwrap(), "semantic_locate", &handles);
        assert!(
            text.contains("flask.sansio.app :: App.debug #535ce03c"),
            "{text}"
        );
        assert!(!text.contains("name_shared"), "{text}");
        let mut call = json!({"entity_id": "flask.sansio.app :: App.debug"});
        assert!(
            !handles.expand_arguments(&mut call),
            "a label the graph holds twice names neither"
        );
        let mut call = json!({"entity_id": "flask.sansio.app :: App.debug #535ce03c"});
        assert!(handles.expand_arguments(&mut call));
        assert_eq!(call["entity_id"], MEMBER);
        // A name declared once is written bare and read back by its label.
        assert!(text.contains("\n  flask.sansio.app :: App.run"), "{text}");
        assert!(!text.contains("App.run #"), "{text}");
        let mut call = json!({"entity_id": "flask.sansio.app :: App.run"});
        assert!(handles.expand_arguments(&mut call));
        assert_eq!(call["entity_id"], OTHER);
    }

    #[test]
    fn each_answer_of_a_same_name_reply_reads_under_its_own_verdict_line() {
        let handles = SessionHandles::default();
        let payload = json!({
            "_kin": {"verdict": "complete", "meaning": "Answer: 2 entities are named pushSegmentFinale."},
            "answers": [
                {"_kin": {"verdict": "complete", "meaning": "Answer: these 4 functions call pushSegmentFinale.",
                          "proof_codes": "imports: target resolved through imports; type: target resolved by type"},
                 "entity": {"entity_id": MEMBER, "name": "pushSegmentFinale", "module": "ReactFizzConfigDOM"},
                 "references": [{"entity_id": OTHER, "name": "renderPreamble", "module": "react-server.ReactFizzServer",
                                 "call_code": "pushSegmentFinale(chunks)", "proof": "imports"}]},
                {"_kin": {"verdict": "complete", "meaning": "Answer: nothing calls pushSegmentFinale."},
                 "entity": {"entity_id": TEST, "name": "pushSegmentFinale", "module": "react-markup.ReactFizzConfigMarkup"},
                 "references": [],
                 "left_to_check": [{"why": "Places that assign something named pushSegmentFinale to another name.",
                     "sites": [{}, {"in_module": "react-server.ReactFizzConfig.custom", "kind": "variable",
                                    "name": "pushSegmentFinale"}]}]},
            ],
        });
        let text = render(payload.as_object().unwrap(), "find_references", &handles);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "Answer (complete): 2 entities are named pushSegmentFinale."
        );
        assert_eq!(lines[1], "answers (2):");
        assert_eq!(
            lines[2], "  Answer (complete): these 4 functions call pushSegmentFinale.",
            "{text}"
        );
        assert!(
            text.contains("      react-server.ReactFizzServer :: renderPreamble  pushSegmentFinale(chunks)  proof=imports"),
            "{text}"
        );
        // The ordinary `type` proof is never written on a row, so its legend
        // entry goes; the others stay.
        assert!(
            text.contains("    proof_codes: imports: target resolved through imports\n"),
            "{text}"
        );
        assert!(!text.contains("type: target resolved by type"), "{text}");
        assert!(
            text.contains("  Answer (complete): nothing calls pushSegmentFinale.\n    entity:"),
            "{text}"
        );
        assert!(text.contains("    references: none"), "{text}");
        // No line is empty, and an empty list is never written as `key=`.
        assert!(lines.iter().all(|line| !line.trim().is_empty()), "{text}");
        assert!(!text.contains("references="), "{text}");
        assert!(
            text.contains("in_module=react-server.ReactFizzConfig.custom"),
            "{text}"
        );
    }

    #[test]
    fn a_source_body_is_written_whole_under_its_row() {
        let handles = SessionHandles::default();
        let body = "def route(self, rule):\n    return decorator";
        let payload = json!({
            "_kin": {"verdict": "complete", "meaning": "Answer: this is the exact current source of Scaffold.route, the whole body."},
            "entity_id": MEMBER, "name": "Scaffold.route", "module": "flask.sansio.scaffold", "kind": "Method",
            "body": body, "signature": "def route(self, rule)",
            "source_base": {"ticket": "sb1.cfaa9527-8673-4daa-8d27-d8a5ac89d37c.8.1c976b444644a10a5ab5f51ccee2ec30"},
        });
        let text = render(payload.as_object().unwrap(), "get_entity_source", &handles);
        assert!(
            text.contains("body:\n  def route(self, rule):\n      return decorator"),
            "{text}"
        );
        assert!(text.contains("source_base: ticket=t"), "{text}");
        assert!(
            text.starts_with("Answer (complete): this is the exact current source"),
            "{text}"
        );
        assert_eq!(
            text.lines().nth(1).unwrap(),
            "flask.sansio.scaffold :: Scaffold.route",
            "{text}"
        );
        assert!(
            !text.contains("signature"),
            "the body holds the signature: {text}"
        );
    }
}
