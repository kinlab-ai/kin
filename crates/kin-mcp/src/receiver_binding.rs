// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The proof a call through a typed receiver carries, served on its row.
//!
//! When the linker binds `store.containsElement(id)` because the facts of the
//! caller type `store` (a React context read, a pytest fixture, a declared
//! parameter), the edge records the chain it followed as steps, each naming the
//! entity that declares it (see [`kin_index::receiver_origin`]). A reference row
//! serves them as [`RECEIVER_BINDING_KEY`], one object per step with the code it
//! stands on, so a reader sees why the call reaches the focal without reading
//! the caller, the context and the class one by one:
//!
//! ```text
//! receiver_binding: [
//!   {step: "call",     line_in_entity: 6},
//!   {step: "binding",  name: "store", line_in_entity: 1, code: "const store = useContext(StoreContext);"},
//!   {step: "provider", kind: "react_context", name: "StoreContext", entity_id, code: "StoreContext: ReactContext<Store> = …"},
//!   {step: "type",     resolves_to: "Store", via: "declared", entity_id},
//! ]
//! ```
//!
//! A row already names its caller and quotes its sites, so a step inside the
//! caller carries its line there and no id, and the call step points at the
//! site the row quotes. A step declared elsewhere carries its entity id, and
//! is described once per list: many callers typed through one context share
//! its steps, and after the first row that serves one, a later row names it
//! by `step` and `entity_id` alone ([`cap_step_code`]). A reply repeating
//! them would spend an agent's response budget on the same lines, and push an
//! answer that fit into a second page.
//!
//! A row served by name alone, whose chain the linker could not finish, says
//! how far it got and which link it could not establish, as
//! [`RECEIVER_BINDING_KEY`] and [`RECEIVER_BINDING_MISSING_KEY`], from the
//! caller's own facts and the coverage certificate of its file. The row stays
//! the name-only row it was.
//!
//! Every step is addressed by entity id, and a step inside the caller also by
//! its line in the caller, never by a file line. Code is quoted from bodies and
//! signatures the graph holds, cut to one line.
//!
//! A proven row no receiver chain bound, such as one a language server
//! resolved, still has a receiver the caller may bind. When the caller's own
//! value-origin facts record where it binds that name, the row serves the line
//! as [`RECEIVER_BOUND_KEY`]: `{name: "store", line_in_entity: 1, code: "const
//! store = useContext(StoreContext);"}`. It is the line the facts record, never
//! one searched for, and it says nothing about the type the server resolved.
//!
//! A short reply keeps neither list. It serves each proven row's receiver as
//! [`SHORT_RECEIVER_KEY`], at most two lines built from them
//! ([`short_receiver`]): the code that binds the receiver, and the chain it
//! resolves through to its type, so a reader trusts the row without reading
//! the caller's body.

use std::collections::HashMap;

use kin_index::receiver_origin::{MissingStep, OriginStep, OriginStepKind};
use kin_model::entity::Entity;
use kin_model::graph::{GraphStore, RelationLookup};
use kin_model::{FilePathId, Hash256, RepoPath};
use serde_json::{json, Map, Value};

/// The row field holding a receiver chain's steps.
pub const RECEIVER_BINDING_KEY: &str = "receiver_binding";

/// The row field naming the link a name-only row's receiver chain lacks.
pub const RECEIVER_BINDING_MISSING_KEY: &str = "receiver_binding_missing";

/// The row field holding, for a proven row no receiver chain bound, the line
/// at which the caller binds each receiver its calls are made through, as the
/// caller's value-origin facts record it.
pub const RECEIVER_BOUND_KEY: &str = "receiver_bound_in_caller";

/// The most receivers one row's [`RECEIVER_BOUND_KEY`] names.
pub const RECEIVER_BOUND_MAX: usize = 2;

/// The short reply's field for a proven row's receiver.
pub const SHORT_RECEIVER_KEY: &str = "receiver";

/// The most lines [`SHORT_RECEIVER_KEY`] holds.
pub const SHORT_RECEIVER_LINES_MAX: usize = 2;

/// The most characters of one [`SHORT_RECEIVER_KEY`] line, `…` included.
pub const SHORT_RECEIVER_LINE_MAX_CHARS: usize = 160;

/// The longest code a step quotes, in characters.
const STEP_CODE_MAX_CHARS: usize = crate::site_expression::EXPR_MAX_CHARS;

/// Where a chain's call sits in its caller: its line there and its text.
#[derive(Debug, Clone, Copy, Default)]
pub struct CallAt<'a> {
    pub line_in_entity: Option<u32>,
    pub code: Option<&'a str>,
}

/// `text` on one line: whitespace runs collapsed, cut past
/// [`STEP_CODE_MAX_CHARS`] with `…`. `None` for text with nothing in it.
fn one_line(text: &str) -> Option<String> {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() <= STEP_CODE_MAX_CHARS {
        return Some(collapsed);
    }
    let mut cut: String = collapsed.chars().take(STEP_CODE_MAX_CHARS - 1).collect();
    cut.push('\u{2026}');
    Some(cut)
}

/// Line `line` of `body`, counted from 0.
fn body_line(body: &str, line: u32) -> Option<String> {
    one_line(body.lines().nth(usize::try_from(line).ok()?)?)
}

/// What a step's entity is written as: its signature, on one line.
fn declared_as(entity: &Entity) -> Option<String> {
    one_line(entity.signature.lines().next().unwrap_or_default())
}

/// The steps of a receiver chain as a row serves them, the call first.
///
/// `caller_body` is the caller's exact text when the answer holds it, which
/// the binding step quotes at the line the caller's facts record. A step
/// declared elsewhere quotes its entity's signature. The method the chain
/// ends at is the row's focal, or the base it reaches the focal through, and
/// is not repeated.
pub fn served_steps<G: GraphStore + ?Sized>(
    store: &G,
    caller: &Entity,
    caller_body: Option<&str>,
    call: CallAt<'_>,
    steps: &[OriginStep],
) -> Vec<Value> {
    let mut served = Vec::with_capacity(steps.len() + 1);
    let mut first = Map::new();
    first.insert("step".into(), json!("call"));
    match call.line_in_entity {
        // The row's own site at this line quotes the call.
        Some(line) => {
            first.insert("line_in_entity".into(), json!(line));
        }
        None => {
            if let Some(code) = call.code.and_then(one_line) {
                first.insert("code".into(), json!(code));
            }
        }
    }
    served.push(Value::Object(first));
    let caller_facts = kin_parser::value_origins::read_value_origins(caller);
    for step in steps {
        if step.step == OriginStepKind::Method {
            continue;
        }
        served.push(served_step(
            store,
            caller,
            caller_body,
            caller_facts.as_ref(),
            step,
        ));
    }
    served
}

fn served_step<G: GraphStore + ?Sized>(
    store: &G,
    caller: &Entity,
    caller_body: Option<&str>,
    caller_facts: Option<&kin_parser::value_origins::ScopeFacts>,
    step: &OriginStep,
) -> Value {
    let mut row = Map::new();
    row.insert("step".into(), json!(step.step.as_str()));
    match step.step {
        OriginStepKind::Type | OriginStepKind::External => {
            row.insert("resolves_to".into(), json!(step.name));
        }
        _ => {
            row.insert("name".into(), json!(step.name));
        }
    }
    if let Some(kind) = &step.kind {
        row.insert("kind".into(), json!(kind));
    }
    if let Some(via) = &step.via {
        row.insert("via".into(), json!(via));
    }
    let Some(id) = step.entity else {
        return Value::Object(row);
    };
    if id == caller.id {
        // A name the caller binds, addressed inside the row's own caller and
        // quoted from its body.
        let line = caller_facts.and_then(|facts| facts.binding_lines.get(&step.name).copied());
        if let Some(line) = line {
            row.insert("line_in_entity".into(), json!(line));
            if let Some(code) = caller_body.and_then(|body| body_line(body, line)) {
                row.insert("code".into(), json!(code));
            }
        }
        return Value::Object(row);
    }
    row.insert("entity_id".into(), json!(id.to_string()));
    // The class a chain reaches is named and addressed; its declaration line
    // is long, the same on every row the class types, and says nothing the
    // name does not. A member step is addressed by the class that declares
    // the attribute, whose own declaration line is not the attribute's.
    if matches!(step.step, OriginStepKind::Type | OriginStepKind::Member) {
        return Value::Object(row);
    }
    if let Ok(Some(entity)) = store.get_entity(&id) {
        if let Some(code) = declared_as(&entity) {
            row.insert("code".into(), json!(code));
        }
    }
    Value::Object(row)
}

/// Where `caller` binds each of `receivers`, in order and once per name, as
/// [`RECEIVER_BOUND_KEY`] serves it: the name, its line in the caller, and the
/// code at that line when `caller_body` holds it. Only a line `facts` record
/// is served, so a receiver bound more than once, a parameter or module name
/// the adapter recorded no line for, and `self` are left out.
pub fn bound_in_caller<'a>(
    facts: &kin_parser::value_origins::ScopeFacts,
    caller_body: Option<&str>,
    receivers: impl IntoIterator<Item = &'a str>,
) -> Vec<Value> {
    let mut bound: Vec<Value> = Vec::new();
    for receiver in receivers {
        if bound.len() >= RECEIVER_BOUND_MAX {
            break;
        }
        let receiver = receiver.trim();
        if bound.iter().any(|entry| entry["name"] == receiver) {
            continue;
        }
        let Some(&line) = facts.binding_lines.get(receiver) else {
            continue;
        };
        let mut entry = Map::new();
        entry.insert("name".into(), json!(receiver));
        entry.insert("line_in_entity".into(), json!(line));
        if let Some(code) = caller_body.and_then(|body| body_line(body, line)) {
            entry.insert("code".into(), json!(code));
        }
        bound.push(Value::Object(entry));
    }
    bound
}

/// The bytes the quoted code of one list's receiver chains may add.
pub const RECEIVER_BINDING_CODE_BYTES_MAX: usize = 2_000;

/// What [`cap_step_code`] kept and cut.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodeCut {
    /// Steps that kept their code.
    pub quoted: usize,
    /// Steps an earlier row of the list already described, served by
    /// `step` and `entity_id` alone.
    pub repeated: usize,
    /// Steps past the budget, which kept their address and dropped the code.
    pub elided: usize,
}

impl CodeCut {
    /// The disclosure a list carries beside itself when the budget cut code,
    /// or `None` when it cut nothing.
    pub fn disclosure(&self, budget: usize) -> Option<Value> {
        (self.elided > 0).then(|| {
            json!({
                "elided": self.elided,
                "quoted": self.quoted,
                "budget_bytes": budget,
                "detail": format!(
                    "the first {} steps of `{RECEIVER_BINDING_KEY}` and `{RECEIVER_BOUND_KEY}` \
                     quote their code within a {budget}-byte budget; the other {} keep their \
                     entity_id or line, and `source` on that id shows the declaration",
                    self.quoted, self.elided
                ),
            })
        })
    }
}

/// Describe each step declared outside its caller once per list, and keep
/// the code the receiver chains of `rows` quote within `budget` bytes, in the
/// order the rows are served. The lines [`RECEIVER_BOUND_KEY`] quotes are
/// spent from the same budget.
///
/// A step whose entity an earlier row's step already described is served as
/// `step` and `entity_id` alone: the id names what the list showed. A step
/// past the budget keeps everything but its code.
pub fn cap_step_code(rows: &mut [Value], budget: usize) -> CodeCut {
    let mut cut = CodeCut::default();
    let mut described: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    let mut bytes = 0usize;
    for row in rows.iter_mut() {
        let Some(row) = row.as_object_mut() else {
            continue;
        };
        let steps = row
            .iter_mut()
            .filter(|(key, _)| *key == RECEIVER_BINDING_KEY || *key == RECEIVER_BOUND_KEY)
            .filter_map(|(_, value)| value.as_array_mut())
            .flat_map(|steps| steps.iter_mut())
            .filter_map(Value::as_object_mut);
        for step in steps {
            let key = step
                .get("entity_id")
                .and_then(Value::as_str)
                .zip(step.get("step").and_then(Value::as_str))
                .map(|(id, kind)| (id.to_string(), kind.to_string()));
            if let Some(key) = key {
                if described.contains(&key) {
                    step.retain(|field, _| field == "step" || field == "entity_id");
                    cut.repeated += 1;
                    continue;
                }
                described.insert(key);
            }
            let Some(code) = step.get("code").and_then(Value::as_str) else {
                continue;
            };
            let added = code.len() + 10;
            if bytes + added > budget {
                step.remove("code");
                cut.elided += 1;
            } else {
                bytes += added;
                cut.quoted += 1;
            }
        }
    }
    cut
}

/// The missing link of a receiver chain as a row serves it.
pub fn missing_json(missing: &MissingStep) -> Value {
    let mut row = Map::new();
    row.insert("step".into(), json!(missing.step.as_str()));
    row.insert("name".into(), json!(missing.name));
    if let Some(kind) = &missing.kind {
        row.insert("kind".into(), json!(kind));
    }
    row.insert("reason".into(), json!(missing.reason));
    row.insert("detail".into(), json!(reason_detail(&missing.reason)));
    Value::Object(row)
}

/// What a reason code on a [`MissingStep`] means, for a reader.
pub fn reason_detail(reason: &str) -> &'static str {
    match reason {
        "receiver_not_a_name" => {
            "the call is made on an expression, not on a name or own field the scope binds or \
             an attribute read off one"
        }
        "no_value_origin_fact" => {
            "the adapter recorded no single binding for this receiver in its scope: it is bound \
             more than once, bound where its value cannot be read, or not bound there at all"
        }
        "local_unbound" => "the local the binding reads is not bound once in the same scope",
        "type_not_located" => {
            "no class the file can reach is named this: it is not declared there, not imported \
             from a repository file, or imported as a default export the module does not name"
        }
        "callee_not_located" => "the called name reaches no function or class of the repository",
        "callee_declares_no_return" => "the function declares no return type and supplies nothing",
        "callee_returns_receiver" => "the function declares it returns its receiver, and has none",
        "method_not_found" => "the class, its bases and its members declare no method of this name",
        "method_declares_no_return" => "the method declares no return type",
        "method_of_external_type" => {
            "the value is of a type outside the repository, whose methods' returns are not read"
        }
        "provider_not_found" => "no provider of this name is reachable from the scope",
        "provider_value_unknown" => {
            "the provider is found but declares no single type for its value (for a React \
             context, `createContext` has no type argument and no `Context<T>` annotation)"
        }
        "provider_file_unreadable" => "a file the provider lookup walks has no facts this reading holds",
        "provider_ambiguous" => "two providers of the name sit in one file, and order decides",
        "provider_lookup_blocked" => "a hook on the lookup path can replace the provided value",
        "provider_lookup_open" => "a file on the lookup path can provide any name",
        "provider_owner_open" => {
            "the owner class has a repository base or subclass that could bind the value differently"
        }
        "self_call" => "the method the chain reaches is the caller itself",
        "language_boundary" => "the method the chain reaches is in a language the caller cannot call by name",
        "chain_too_deep" => "the chain nests deeper than the linker follows",
        "member_not_declared" => {
            "Kin holds no declared type for this attribute on the class or a base the \
             repository declares; an adapter that records none for the language leaves it open"
        }
        "member_type_unknown" => {
            "the class declares the attribute, but not as one type, or two declarations disagree"
        }
        "member_of_external_type" => {
            "the value is of a type outside the repository, whose attributes are not read"
        }
        "provider_withheld" => {
            "the test the lookup is made for parametrizes this name directly, so no fixture \
             supplies it"
        }
        "imported_provider_not_located" => {
            "the provider is a name the file imports from a module the repository does not hold"
        }
        "imported_provider_not_found" => {
            "the module the import reaches holds no provider of this name declared as imported"
        }
        _ => "the chain stopped here",
    }
}

/// The missing link as a listed call site carries it: the step, the name it
/// had to resolve, its namespace and the reason code, without the words.
pub fn missing_brief(missing: &MissingStep) -> Value {
    let mut row = Map::new();
    row.insert("step".into(), json!(missing.step.as_str()));
    row.insert("name".into(), json!(missing.name));
    if let Some(kind) = &missing.kind {
        row.insert("kind".into(), json!(kind));
    }
    row.insert("reason".into(), json!(missing.reason));
    Value::Object(row)
}

/// Each step a list of reference rows describes, by its kind and entity id.
/// [`cap_step_code`] serves a step an earlier row described as `step` and
/// `entity_id` alone, and a reader of a later row reads it as described here.
pub type DescribedSteps = HashMap<(String, String), Map<String, Value>>;

/// The steps `rows` describe, the first description of each kept.
pub fn described_steps<'a>(rows: impl IntoIterator<Item = &'a Value>) -> DescribedSteps {
    let mut described = DescribedSteps::new();
    let steps = rows
        .into_iter()
        .filter_map(|row| row.get(RECEIVER_BINDING_KEY).and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_object);
    for step in steps {
        let (Some(kind), Some(id)) = (
            step.get("step").and_then(Value::as_str),
            step.get("entity_id").and_then(Value::as_str),
        ) else {
            continue;
        };
        if step_name(step).is_some() {
            described
                .entry((kind.to_string(), id.to_string()))
                .or_insert_with(|| step.clone());
        }
    }
    described
}

/// What a served step names: the binding, provider, function or method, or
/// the type it resolves to.
fn step_name(step: &Map<String, Value>) -> Option<&str> {
    step.get("name")
        .or_else(|| step.get("resolves_to"))
        .and_then(Value::as_str)
}

fn step_kind(step: &Map<String, Value>) -> &str {
    step.get("step").and_then(Value::as_str).unwrap_or_default()
}

/// `line` within [`SHORT_RECEIVER_LINE_MAX_CHARS`], keeping its start and
/// marking the cut with `…`.
fn short_line(line: &str) -> String {
    if line.chars().count() <= SHORT_RECEIVER_LINE_MAX_CHARS {
        return line.to_string();
    }
    let mut cut: String = line
        .chars()
        .take(SHORT_RECEIVER_LINE_MAX_CHARS - 1)
        .collect();
    cut.push('\u{2026}');
    cut
}

/// The receiver of one proven reference row as a short reply serves it, at
/// most [`SHORT_RECEIVER_LINES_MAX`] lines, or `None` when the row records
/// nothing to build one from.
///
/// A row bound through a receiver chain serves the code that binds the
/// receiver, when its caller's facts record the line, and the chain from the
/// receiver to the type it resolves to: `ctx: pytest_fixture app -> Flask(...)
/// -> Flask.test_request_context() -> AppContext`. A chain with a link settled
/// by name alone proves nothing about the receiver and is not served. A row no
/// chain bound serves the code at each line [`RECEIVER_BOUND_KEY`] records.
/// Every word comes from the row and the steps `described` holds; a link this
/// list never described reads `…`, and a chain whose type it never described
/// is left out.
pub fn short_receiver(row: &Value, described: &DescribedSteps) -> Option<Value> {
    let mut lines = row
        .get(RECEIVER_BINDING_KEY)
        .and_then(Value::as_array)
        .map(|steps| chain_lines(steps, described))
        .unwrap_or_default();
    if lines.is_empty() {
        lines = row
            .get(RECEIVER_BOUND_KEY)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("code").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
    }
    lines.truncate(SHORT_RECEIVER_LINES_MAX);
    (!lines.is_empty()).then(|| {
        Value::Array(
            lines
                .iter()
                .map(|line| Value::from(short_line(line)))
                .collect(),
        )
    })
}

/// The binding code and the chain line of one served receiver chain.
fn chain_lines(steps: &[Value], described: &DescribedSteps) -> Vec<String> {
    let steps: Vec<Map<String, Value>> = steps
        .iter()
        .filter_map(Value::as_object)
        .map(|step| {
            if step_name(step).is_some() {
                return step.clone();
            }
            step.get("entity_id")
                .and_then(Value::as_str)
                .and_then(|id| described.get(&(step_kind(step).to_string(), id.to_string())))
                .cloned()
                .unwrap_or_else(|| step.clone())
        })
        .collect();
    // The chain opens at the row's own call, which the row already quotes.
    let links = match steps.split_first() {
        Some((call, rest)) if step_kind(call) == "call" && step_name(call).is_none() => rest,
        _ => &steps[..],
    };
    let Some((receiver, links)) = links.split_first() else {
        return Vec::new();
    };
    if step_kind(receiver) != "binding" || links.iter().any(|step| step_kind(step) == "guess") {
        return Vec::new();
    }
    let Some(name) = step_name(receiver) else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    if let Some(code) = receiver.get("code").and_then(Value::as_str) {
        lines.push(code.to_string());
    }
    if let Some(chain) = chain_line(name, links) {
        lines.push(chain);
    }
    lines
}

/// `receiver: link -> link -> Type`, from the links after the receiver's own
/// binding, or `None` when they name no type.
fn chain_line(receiver: &str, links: &[Map<String, Value>]) -> Option<String> {
    let resolved = links
        .iter()
        .rev()
        .find(|step| matches!(step_kind(step), "type" | "external"))
        .and_then(step_name)?;
    let supplied_beside = |step: Option<&Map<String, Value>>, name: Option<&str>| {
        step.is_some_and(|step| {
            matches!(step_kind(step), "provider" | "fallback") && step_name(step) == name
        })
    };
    let mut tokens: Vec<String> = Vec::new();
    for (index, step) in links.iter().enumerate() {
        let name = step_name(step);
        let kind = step.get("kind").and_then(Value::as_str);
        let token = match step_kind(step) {
            // A name a provider supplies is said once, as the provider.
            "binding"
                if supplied_beside(index.checked_sub(1).and_then(|i| links.get(i)), name)
                    || supplied_beside(links.get(index + 1), name) =>
            {
                continue;
            }
            "provider" => name.map(|name| match kind {
                Some(kind) => format!("{kind} {name}"),
                None => name.to_string(),
            }),
            "fallback" => name.map(|name| match kind {
                Some(kind) => format!("{kind} {name} (built in)"),
                None => format!("{name} (built in)"),
            }),
            "type" if step.get("via").and_then(Value::as_str) == Some("constructed") => {
                name.map(|name| format!("{name}(...)"))
            }
            "call" | "returns" => name.map(|name| format!("{name}()")),
            _ => name.map(str::to_string),
        }
        .unwrap_or_else(|| "\u{2026}".to_string());
        if tokens.last() != Some(&token) {
            tokens.push(token);
        }
    }
    // A chain ending at a method that returns its receiver names the type it
    // already reached.
    if !links
        .last()
        .is_some_and(|step| matches!(step_kind(step), "type" | "external"))
    {
        tokens.push(resolved.to_string());
    }
    Some(format!("{receiver}: {}", tokens.join(" -> ")))
}

/// How far the chain typing `receiver` in `caller` got, for a call of
/// `method` the linker could not bind through it, and the link it lacks.
///
/// A receiver the caller's facts do not bind lacks its binding: a name, or
/// an own field, reads as bound nowhere in the scope, and an attribute path
/// whose first name the facts do not bind reads as no name at all. One they
/// do bind (`app` of `app.json`) is looked up in the coverage certificate of
/// the caller's file, which records every such call whose chain stopped,
/// with the steps it took. `None` when neither says anything: a certificate
/// from another body, or one written before these records existed.
pub fn chain_gap<G: GraphStore + ?Sized>(
    store: &G,
    caller: &Entity,
    receiver: &str,
    method: &str,
) -> Option<(Vec<OriginStep>, MissingStep)> {
    let receiver = receiver.trim();
    let unbound = |reason: &str| {
        Some((
            Vec::new(),
            MissingStep {
                step: OriginStepKind::Binding,
                name: receiver.to_string(),
                reason: reason.into(),
                kind: None,
                file: caller.file_origin.as_ref().map(|path| path.0.clone()),
            },
        ))
    };
    let Some(key) = kin_index::receiver_origin::receiver_key(receiver) else {
        return unbound("receiver_not_a_name");
    };
    let bound = kin_parser::value_origins::read_value_origins(caller)
        .is_some_and(|facts| facts.bindings.contains_key(key));
    if !bound {
        return unbound(if key == receiver {
            "no_value_origin_fact"
        } else {
            "receiver_not_a_name"
        });
    }
    let span = caller.span.as_ref()?;
    let FilePathId(file) = caller.file_origin.as_ref()?;
    let body = caller
        .metadata
        .extra
        .get("blob_hash")
        .and_then(Value::as_str)
        .and_then(|hex| Hash256::from_hex(hex).ok())?;
    let artifact = store.artifact_id_at_path(&RepoPath::from_utf8(file).ok()?)?;
    let RelationLookup::Present(certificate) = store
        .lookup_relation_by_id(&kin_index::parse_coverage_relation_id(artifact))
        .ok()?
    else {
        return None;
    };
    kin_index::receiver_origin_gaps(&certificate, file, artifact, body)
        .into_iter()
        .find(|gap| {
            gap.receiver == receiver
                && gap.method == method
                && gap.span.start_byte >= span.start_byte
                && gap.span.end_byte <= span.end_byte
        })
        .map(|gap| (gap.steps, gap.missing))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_one_line_and_bounded() {
        assert_eq!(
            one_line("  const store =\n    useContext(StoreContext);  ").as_deref(),
            Some("const store = useContext(StoreContext);")
        );
        assert_eq!(one_line(" \n\t"), None);
        let long = "x".repeat(STEP_CODE_MAX_CHARS + 20);
        let cut = one_line(&long).unwrap();
        assert_eq!(cut.chars().count(), STEP_CODE_MAX_CHARS);
        assert!(cut.ends_with('\u{2026}'));
    }

    #[test]
    fn a_name_an_own_field_or_an_attribute_path_has_a_bindable_key() {
        let key = kin_index::receiver_origin::receiver_key;
        for (receiver, bound) in [
            ("store", "store"),
            ("this.store", "this.store"),
            ("self.client", "self.client"),
            ("$this->store", "$this->store"),
            ("props.store", "props"),
            ("this.props.store", "this.props"),
            ("app.json", "app"),
        ] {
            assert_eq!(key(receiver), Some(bound), "{receiver}");
        }
        for not in ["getStore()", "a[b]", ""] {
            assert_eq!(key(not), None, "{not}");
        }
    }

    #[test]
    fn a_missing_step_names_its_reason_in_words() {
        let value = missing_json(&MissingStep {
            step: OriginStepKind::Provider,
            name: "LooseContext".into(),
            reason: "provider_value_unknown".into(),
            kind: Some("react_context".into()),
            file: None,
        });
        assert_eq!(value["step"], "provider");
        assert_eq!(value["kind"], "react_context");
        assert_eq!(value["reason"], "provider_value_unknown");
        assert!(value["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("createContext")));
    }

    /// The steps the linker records for `ctx.pop()` in a pytest test whose
    /// `ctx` is what the `app` fixture's `test_request_context()` returns, as
    /// a row serves them: the first row describes every step, and a later
    /// row of the same list names each step declared elsewhere by its id.
    fn fixture_chain(described: bool, binding_code: Option<&str>) -> Value {
        let mut binding = json!({"step": "binding", "name": "ctx", "line_in_entity": 1});
        if let Some(code) = binding_code {
            binding["code"] = json!(code);
        }
        let mut steps = vec![json!({"step": "call", "line_in_entity": 2}), binding];
        if described {
            steps.extend([
                json!({"step": "binding", "name": "app"}),
                json!({"step": "provider", "name": "app", "kind": "pytest_fixture",
                    "entity_id": "fixture", "code": "@fixture def app()"}),
                json!({"step": "binding", "name": "app", "entity_id": "fixture"}),
                json!({"step": "type", "resolves_to": "Flask", "via": "constructed",
                    "entity_id": "flask"}),
                json!({"step": "returns", "name": "Flask.test_request_context",
                    "entity_id": "trc", "code": "def test_request_context(self) -> AppContext"}),
                json!({"step": "type", "resolves_to": "AppContext", "via": "returned",
                    "entity_id": "appctx"}),
            ]);
        } else {
            steps.extend([
                json!({"step": "binding", "name": "app"}),
                json!({"step": "provider", "entity_id": "fixture"}),
                json!({"step": "binding", "entity_id": "fixture"}),
                json!({"step": "type", "entity_id": "flask"}),
                json!({"step": "returns", "entity_id": "trc"}),
                json!({"step": "type", "entity_id": "appctx"}),
            ]);
        }
        json!({"resolution": "type_resolved", RECEIVER_BINDING_KEY: steps})
    }

    const FIXTURE_CHAIN: &str =
        "ctx: pytest_fixture app -> Flask(...) -> Flask.test_request_context() -> AppContext";

    #[test]
    fn a_chained_row_serves_its_binding_code_and_the_chain_to_its_type() {
        let row = fixture_chain(true, Some("ctx = app.test_request_context()"));
        let described = described_steps([&row]);
        assert_eq!(
            short_receiver(&row, &described),
            Some(json!(["ctx = app.test_request_context()", FIXTURE_CHAIN]))
        );
        // A caller whose facts record no line for the binding still names its
        // receiver and the type it resolves to.
        let row = fixture_chain(true, None);
        assert_eq!(
            short_receiver(&row, &described_steps([&row])),
            Some(json!([FIXTURE_CHAIN]))
        );
    }

    #[test]
    fn a_later_row_reads_each_repeated_step_as_the_list_described_it() {
        let first = fixture_chain(true, None);
        let later = fixture_chain(false, None);
        let described = described_steps([&first, &later]);
        assert_eq!(
            short_receiver(&later, &described),
            Some(json!([FIXTURE_CHAIN]))
        );
        // A page that never saw the type's description serves no chain rather
        // than a guessed type.
        assert_eq!(short_receiver(&later, &described_steps([&later])), None);
    }

    /// `app.json.dumps(...)` through the `app` fixture: the attribute read
    /// off the fixture's `Flask` is a link of its own, named by the class
    /// that declares it, before the type its declaration names.
    #[test]
    fn a_chain_through_a_declared_attribute_names_the_attribute() {
        let row = json!({"resolution": "type_resolved", RECEIVER_BINDING_KEY: [
            {"step": "call", "line_in_entity": 2},
            {"step": "binding", "name": "app"},
            {"step": "provider", "name": "app", "kind": "pytest_fixture", "entity_id": "fixture"},
            {"step": "binding", "name": "app", "entity_id": "fixture"},
            {"step": "type", "resolves_to": "Flask", "via": "constructed", "entity_id": "flask"},
            {"step": "member", "name": "App.json", "entity_id": "app_class"},
            {"step": "type", "resolves_to": "JSONProvider", "via": "declared", "entity_id": "provider"},
        ]});
        assert_eq!(
            short_receiver(&row, &described_steps([&row])),
            Some(json!([
                "app: pytest_fixture app -> Flask(...) -> App.json -> JSONProvider"
            ]))
        );
    }

    #[test]
    fn a_row_with_nothing_recorded_has_no_short_receiver() {
        let none = DescribedSteps::new();
        for row in [
            json!({"resolution": "type_resolved"}),
            json!({"resolution": "type_resolved", RECEIVER_BINDING_KEY: []}),
            // A chain reaching no type, and a binding with no recorded line.
            json!({RECEIVER_BINDING_KEY: [{"step": "call"}, {"step": "binding", "name": "ctx"}]}),
            json!({RECEIVER_BOUND_KEY: [{"name": "store", "line_in_entity": 1}]}),
        ] {
            assert_eq!(short_receiver(&row, &none), None, "{row}");
        }
        // A link settled by name alone proves nothing about the receiver.
        let guessed = json!({RECEIVER_BINDING_KEY: [
            {"step": "call", "line_in_entity": 2},
            {"step": "binding", "name": "ctx", "code": "ctx = make()"},
            {"step": "guess", "name": "Factory.make"},
            {"step": "type", "resolves_to": "Context"},
        ]});
        assert_eq!(short_receiver(&guessed, &none), None);
    }

    #[test]
    fn an_unchained_row_serves_the_line_that_binds_its_receiver() {
        let row = json!({"resolution": "type_resolved", RECEIVER_BOUND_KEY: [
            {"name": "store", "line_in_entity": 1, "code": "const store = useContext(StoreContext);"},
            {"name": "other", "line_in_entity": 2},
        ]});
        assert_eq!(
            short_receiver(&row, &DescribedSteps::new()),
            Some(json!(["const store = useContext(StoreContext);"]))
        );
    }

    #[test]
    fn a_short_receiver_keeps_two_lines_each_within_its_limit() {
        let long = format!("value = {}", "x".repeat(300));
        let row = json!({RECEIVER_BOUND_KEY: [
            {"name": "a", "code": long},
            {"name": "b", "code": "b = make()"},
            {"name": "c", "code": "c = make()"},
        ]});
        let lines = short_receiver(&row, &DescribedSteps::new()).unwrap();
        let lines = lines.as_array().unwrap();
        assert_eq!(lines.len(), SHORT_RECEIVER_LINES_MAX);
        let cut = lines[0].as_str().unwrap();
        assert_eq!(cut.chars().count(), SHORT_RECEIVER_LINE_MAX_CHARS);
        assert!(cut.starts_with("value = xxx") && cut.ends_with('\u{2026}'));
        assert_eq!(lines[1], "b = make()");
    }

    #[test]
    fn a_receiver_is_bound_in_its_caller_only_where_the_facts_record_a_line() {
        let facts = kin_parser::value_origins::ScopeFacts {
            binding_lines: [("store".to_string(), 1), ("this.cache".to_string(), 2)].into(),
            ..Default::default()
        };
        let body = "function check(id) {\n  const store = openStore();\n  this.cache = new Map();\n  return store.has(id);\n}";
        let bound = bound_in_caller(
            &facts,
            Some(body),
            ["other", "store", "store", "self", "this.cache", "late"],
        );
        assert_eq!(
            bound,
            vec![
                json!({"name": "store", "line_in_entity": 1, "code": "const store = openStore();"}),
                json!({"name": "this.cache", "line_in_entity": 2, "code": "this.cache = new Map();"}),
            ]
        );
        // Without the body the line still addresses the binding.
        assert_eq!(
            bound_in_caller(&facts, None, ["store"]),
            vec![json!({"name": "store", "line_in_entity": 1})]
        );
    }

    #[test]
    fn a_receiver_bound_line_spends_the_chain_code_budget() {
        let mut rows = vec![
            json!({RECEIVER_BOUND_KEY: [{"name": "a", "line_in_entity": 1, "code": "a = make_a()"}]}),
            json!({RECEIVER_BOUND_KEY: [{"name": "b", "line_in_entity": 1, "code": "x".repeat(80)}]}),
        ];
        let cut = cap_step_code(&mut rows, 40);
        assert_eq!(
            cut,
            CodeCut {
                quoted: 1,
                repeated: 0,
                elided: 1
            }
        );
        assert_eq!(rows[0][RECEIVER_BOUND_KEY][0]["code"], "a = make_a()");
        assert_eq!(
            rows[1][RECEIVER_BOUND_KEY][0],
            json!({"name": "b", "line_in_entity": 1})
        );
    }
}
