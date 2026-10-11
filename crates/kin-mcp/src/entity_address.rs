// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! An entity an agent names in an id argument, read from the graph.
//!
//! A reply names an entity three ways: its UUID, the short handle a
//! connection serves for that UUID (`535ce03c`), and, in a text reply, its
//! label (`flask.sansio.app :: App.route`, or `name #535ce03c` when the name
//! alone is not unique). [`crate::session_handles`] reads the short forms
//! back from the table of the connection that served them. That table dies
//! with the connection: a server or daemon restart, a resumed task, a
//! compacted context or a second client leaves an agent holding addresses
//! nothing can read. Measured in model runs, `refs` on a label a fresh
//! connection had not served answered "UUID parsing failed: invalid length:
//! found 71", and `source` on another found no entity until a `locate` for
//! the same name had run. Each failure cost a model turn and a locate.
//!
//! So every argument that takes an entity is also read here, against the
//! graph and nothing else, before the call runs:
//!
//! - a UUID is the exact address and is left as it is;
//! - a short id, alone or after `#`, is the entity whose id starts with it,
//!   when exactly one does;
//! - `module :: name` is the entity of that name in that module, as a row
//!   spells both, when exactly one is;
//! - any other text is a name, read by the strict rule every source read
//!   shares ([`kin_ranking::entity_ranking::resolve_name_strictly`]): one
//!   exact name, one owner's member, or one module-qualified name such as
//!   `flask.app.Flask.route` or `flask.app::Flask.route`.
//!
//! An argument that takes only an id is refused when nothing or more than
//! one thing answers to it, with the candidates spelled so that any one of
//! them can be passed back. An argument whose tool already reads names
//! (`trace`'s `focal`, `context`'s `entities`, `source`'s `entity_id`) is
//! handed on as written when this cannot settle it, so the tool's own answer
//! about the name is unchanged.
//!
//! Cursors and source tickets stay bound to the connection that served them:
//! they stand for a frozen answer or an exact base, which no graph lookup
//! recovers. A short one that reaches a call unread says so. A write's
//! `target` is read by the connection alone for the same reason: the write
//! needs a ticket that connection served, and the source read that serves a
//! new ticket names the entity again.
//!
//! The connection's table stays the fast path: a handle it knows never
//! reaches this module.

use std::collections::HashMap;

use kin_model::graph::EntityFilter;
use kin_model::Entity;
use serde_json::Value;

use crate::session_handles::is_uuid;

/// How an argument takes an entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Takes {
    /// Only an id: text that settles on no one entity is refused.
    Id,
    /// An id, or a name its tool reads itself: text this module cannot
    /// settle is handed on as written.
    Name,
    /// As [`Takes::Name`], for a tool that answers about one entity's own
    /// code. A name that several declarations of one scope share, a
    /// property and its setter or the overloads of a function, is said here
    /// with the address of each: the tool's own list of candidates has no
    /// spelling that tells them apart.
    One,
}

/// The arguments of the registered tool `tool` that take an entity, each a
/// string or a list of strings.
fn entity_arguments(tool: &str) -> &'static [(&'static str, Takes)] {
    match tool {
        "get_entity_source" => &[("entity_id", Takes::One)],
        "get_entity_sources" | "bulk_check_references" => &[("entity_ids", Takes::Name)],
        "trace_data_flow" => &[("focal", Takes::Name), ("target", Takes::Name)],
        crate::handlers::path::TOOL_NAME => &[("from", Takes::Name), ("to", Takes::Name)],
        "get_context_pack" => &[("entity_id", Takes::Id), ("entities", Takes::Name)],
        "find_references" => &[("entity_id", Takes::Id), ("proof_for_caller", Takes::Id)],
        "graph_neighborhood" | "kin_provenance_query" | "get_entity" => &[("entity_id", Takes::Id)],
        "impact_analysis" => &[("entity_ids", Takes::Id)],
        // A tool this table does not know keeps its arguments as written: an
        // `entity_id` of its own may not be an entity's.
        _ => &[],
    }
}

/// The `query` of a reference read that could name an entity Kin holds
/// without parsing it: one name shaped like one (`schema.sql`, `LICENSE`) or
/// a label (`shop :: schema.sql`), on a call that names no `entity_id`.
fn unparsed_query(tool: &str, arguments: &HashMap<String, Value>) -> Option<String> {
    if tool != "find_references" || arguments.contains_key("entity_id") {
        return None;
    }
    let query = arguments.get("query")?.as_str()?.trim();
    let name = query.rsplit_once(" :: ").map_or(query, |(_, name)| name);
    let shaped = !name.is_empty()
        && !name.chars().any(char::is_whitespace)
        && (name.contains('.') || name.chars().any(|c| c.is_ascii_uppercase()));
    shaped.then(|| query.to_string())
}

/// Most candidates a refusal lists before it counts the rest.
const CANDIDATES_SHOWN: usize = 8;

/// Fewest hex digits a short id carries, the length a connection serves.
const SHORT_ID_MIN: usize = 8;

/// Whether `text` is an address this module leaves alone: a UUID, bare or as
/// `entity:<uuid>`, or a symbol outside the repository.
fn exact_address(text: &str) -> bool {
    let text = text.trim();
    is_uuid(text.strip_prefix("entity:").unwrap_or(text))
        || crate::handlers::external_symbols::is_external_address(text)
}

/// `text` as the leading hex digits of an entity id, when it is shaped like
/// them: eight or more hex digits, with hyphens where a UUID places them.
fn short_id(text: &str) -> Option<String> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    (hex.len() >= SHORT_ID_MIN
        && hex.len() <= 32
        && hex.chars().all(|c| c.is_ascii_hexdigit())
        && !text.starts_with('-')
        && !text.ends_with('-'))
    .then(|| hex.to_ascii_lowercase())
}

/// Whether the UUID `id` starts with the hex digits `prefix`, hyphens aside.
pub fn id_starts_with(id: &str, prefix: &str) -> bool {
    let mut digits = id.chars().filter(|c| *c != '-');
    prefix.chars().all(|wanted| {
        digits
            .next()
            .is_some_and(|digit| digit.eq_ignore_ascii_case(&wanted))
    })
}

/// What an entity argument's text is, by its shape alone.
struct Written<'a> {
    /// The leading hex digits of an id: the whole text, or what follows `#`.
    prefix: Option<String>,
    /// The name or label, without a `#` short id after it. Empty when the
    /// text is only a short id.
    name: &'a str,
}

impl<'a> Written<'a> {
    fn of(text: &'a str) -> Self {
        let text = text.trim();
        // `name #535ce03c`, `#535ce03c`. A `#` that no short id follows is
        // part of the name, as in `Model#save`.
        if let Some((label, handle)) = text.rsplit_once('#') {
            if let Some(prefix) = short_id(handle.trim()) {
                return Self {
                    prefix: Some(prefix),
                    name: label.trim(),
                };
            }
        }
        match short_id(text) {
            Some(prefix) => Self {
                prefix: Some(prefix),
                // A hex-shaped word may still be a name no id starts with.
                name: text,
            },
            None => Self {
                prefix: None,
                name: text,
            },
        }
    }

    /// Every `module` and `name` the text could be a label of. A label as a
    /// row writes it, `module :: name`, has one reading. Written without its
    /// spaces, `module::name`, it has one for each `::` it holds, since a
    /// name may hold `::` itself; the graph says which is an entity.
    fn labels(&self) -> Vec<(&'a str, &'a str)> {
        if let Some((module, name)) = self.name.split_once(" :: ") {
            return vec![(module.trim(), name.trim())];
        }
        self.name
            .match_indices("::")
            .map(|(at, separator)| {
                (
                    self.name[..at].trim(),
                    self.name[at + separator.len()..].trim(),
                )
            })
            .filter(|(module, name)| !module.is_empty() && !name.is_empty())
            .collect()
    }

    /// Whether this module could settle the text for an argument that
    /// `takes` an entity this way, so the graph is worth reading for it.
    fn asks(&self, takes: Takes) -> bool {
        matches!(takes, Takes::Id | Takes::One)
            || self.prefix.is_some()
            || !self.labels().is_empty()
    }
}

/// The entity a write operation's `source_base` was read from, by its id:
/// what a ticket names, or what a whole base does.
fn base_entity(operation: &Value) -> Option<String> {
    let payload = operation.get("payload")?.as_object()?;
    payload.iter().find_map(|(variant, value)| {
        let base = match variant.as_str() {
            "EntitySourceBase" => Some(value),
            "EntitySourcePatch" | "EntityCreate" | "EntityRemove" | "EntityContent" => {
                value.get("source_base")
            }
            _ => None,
        }?;
        match base
            .get(crate::source_ticket::TICKET_KEY)
            .and_then(Value::as_str)
        {
            Some(ticket) => crate::source_ticket::parse(ticket)
                .ok()
                .map(|ticket| ticket.entity_id),
            None => base
                .get("entity_id")
                .and_then(Value::as_str)
                .filter(|id| is_uuid(id))
                .map(str::to_string),
        }
    })
}

/// What a write operation's `target` is, read against the entity `expected`
/// that its `source_base` was read from.
#[derive(Debug, PartialEq, Eq)]
enum TargetReading {
    /// It is that entity, written as its id, or as the start of its id the
    /// way a row writes it: `#535ce03c`, `name #535ce03c`,
    /// `module :: name #535ce03c`.
    Names,
    /// It is another entity: another id, or the start of one.
    NamesAnother,
    /// It is a name or a label, which only the graph can read.
    Name,
}

fn read_target(target: &str, expected: &str) -> TargetReading {
    let shown = target.trim();
    let whole = shown.strip_prefix("entity:").unwrap_or(shown);
    if whole.eq_ignore_ascii_case(expected) {
        return TargetReading::Names;
    }
    if is_uuid(whole) {
        return TargetReading::NamesAnother;
    }
    let written = Written::of(shown);
    match &written.prefix {
        Some(prefix) if id_starts_with(expected, prefix) => TargetReading::Names,
        // A short id after a `#` is an address; a hex-shaped word alone may
        // be a name.
        Some(_) if written.name != shown => TargetReading::NamesAnother,
        _ => TargetReading::Name,
    }
}

/// The refusal for a write whose `target` is not the entity its
/// `source_base` was read from. `read_as` names that entity the way a caller
/// can pass it.
fn another_target(shown: &str, read_as: &str) -> String {
    format!(
        "target `{shown}` is not the entity this source_base was read from, which is {read_as}. \
         Pass that as target to change it, or read `{shown}` with source and send the \
         source_base that read returns. Nothing was written."
    )
}

/// Whether a write operation's `target` could be the entity its
/// `source_base` was read from, by what the two texts say alone: `Err` is
/// the refusal for a target that is another entity's id or the start of one.
/// A target that is a name is left for the graph that holds the entity
/// ([`resolve_call`]).
pub fn check_write_target(operation: &Value) -> Result<(), String> {
    let (Some(expected), Some(target)) = (
        base_entity(operation),
        operation.get("target").and_then(Value::as_str),
    ) else {
        return Ok(());
    };
    match read_target(target, &expected) {
        TargetReading::NamesAnother => Err(another_target(
            target.trim(),
            &format!("the entity whose id starts `{}`", &expected[..SHORT_ID_MIN]),
        )),
        _ => Ok(()),
    }
}

/// Whether a write call names an entity some other way than by its whole id.
fn write_targets_unsettled(arguments: &HashMap<String, Value>) -> bool {
    arguments
        .get("operations")
        .and_then(Value::as_array)
        .is_some_and(|operations| {
            operations.iter().any(|operation| {
                match (
                    base_entity(operation),
                    operation.get("target").and_then(Value::as_str),
                ) {
                    (Some(expected), Some(target)) => target != expected,
                    _ => false,
                }
            })
        })
}

/// Set the `target` of every write operation that carries a `source_base`
/// to the id of the entity that base was read from, when the target names
/// that entity another way: the start of its id as a row prints it, its
/// `module :: name` label, or its name.
///
/// The base already says which entity the operation changes, and nothing is
/// read from the target but agreement with it. So a name that several
/// declarations share, a property and its setter, is the one the base was
/// read from, and a target that is some other entity is refused with the
/// address of the one the base names.
///
/// A call that carries its own request id is a call that may be a retry of
/// one already applied, whose entity may since be gone, so its names are not
/// read against today's graph.
fn settle_write_targets<G: crate::server::PersistableMcpStore>(
    store: &G,
    arguments: &mut HashMap<String, Value>,
) -> Result<(), String> {
    let keyed = arguments.contains_key("request_id");
    let Some(operations) = arguments
        .get_mut("operations")
        .and_then(Value::as_array_mut)
    else {
        return Ok(());
    };
    let reader = Reader { store };
    for operation in operations {
        let (Some(expected), Some(target)) = (
            base_entity(operation),
            operation
                .get("target")
                .and_then(Value::as_str)
                .map(str::to_string),
        ) else {
            continue;
        };
        if target == expected {
            continue;
        }
        let shown = target.trim();
        let held = || {
            crate::handlers::common::parse_entity_id(&expected)
                .ok()
                .and_then(|id| store.get_entity(&id).ok().flatten())
        };
        let read_as = |entity: Option<&Entity>| match entity {
            Some(entity) => reader
                .labels_with(std::slice::from_ref(entity), false)
                .pop()
                .map(|label| {
                    format!(
                        "{} #{}`",
                        label.trim_end_matches('`'),
                        short_of(entity, SHORT_ID_MIN)
                    )
                })
                .unwrap_or_else(|| format!("`{}`", entity.id)),
            None => format!("the entity whose id starts `{}`", &expected[..SHORT_ID_MIN]),
        };
        let names = match read_target(shown, &expected) {
            TargetReading::Names => true,
            TargetReading::NamesAnother => {
                return Err(another_target(shown, &read_as(held().as_ref())));
            }
            TargetReading::Name if keyed => continue,
            TargetReading::Name => {
                let Some(entity) = held() else {
                    // The write path says what became of the entity.
                    continue;
                };
                let written = Written::of(shown);
                let module = crate::semantic_owners::entity_module(store, &entity);
                let named = written.name == entity.name
                    || written.labels().iter().any(|(owner, name)| {
                        *name == entity.name && module.as_deref() == Some(*owner)
                    })
                    || module
                        .as_deref()
                        .is_some_and(|module| written.name == format!("{module}.{}", entity.name));
                if !named {
                    return Err(another_target(shown, &read_as(Some(&entity))));
                }
                true
            }
        };
        if names {
            operation["target"] = Value::String(expected);
        }
    }
    Ok(())
}

/// Whether any entity argument of this call needs the graph to be read. A
/// call that passes only UUIDs, which is every call a live connection
/// expands, and a name its own tool reads cost nothing here.
pub fn needs_graph(tool: &str, arguments: &HashMap<String, Value>) -> bool {
    if crate::source_ticket::is_write_tool(tool) {
        return write_targets_unsettled(arguments);
    }
    if unparsed_query(tool, arguments).is_some() {
        return true;
    }
    let asks = |text: &str, takes: Takes| {
        !text.trim().is_empty() && !exact_address(text) && Written::of(text).asks(takes)
    };
    entity_arguments(tool)
        .iter()
        .any(|(key, takes)| match arguments.get(*key) {
            Some(Value::String(text)) => asks(text, *takes),
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .any(|text| asks(text, *takes)),
            _ => false,
        })
}

/// A short cursor or ticket that reached a call unread: the handle of a
/// connection that is gone, which no graph lookup can stand in for.
///
/// Read anywhere in the arguments, since a ticket rides inside a write's
/// payload. A full cursor or ticket is far longer and never matches.
pub fn session_bound_refusal(arguments: &HashMap<String, Value>) -> Option<String> {
    fn handle_shaped(text: &str, letter: char) -> bool {
        let mut chars = text.chars();
        chars.next() == Some(letter)
            && matches!(text.len(), 8 | 11 | 14 | 17)
            && chars.all(|c| c.is_ascii_hexdigit())
    }
    fn find(value: &Value, key: Option<&str>) -> Option<String> {
        match value {
            Value::String(text) => {
                let key = key?;
                let text = text.trim();
                if key.ends_with("cursor") && handle_shaped(text, 'p') {
                    return Some(format!(
                        "{key} `{text}` belongs to an earlier connection, and this one cannot \
                         read it. Repeat the query without {key}."
                    ));
                }
                if key == crate::source_ticket::TICKET_KEY && handle_shaped(text, 't') {
                    return Some(format!(
                        "source_base ticket `{text}` belongs to an earlier connection, and this \
                         one cannot read it. Read the entity with get_entity_source again and \
                         pass the source_base it returns."
                    ));
                }
                None
            }
            Value::Array(items) => items.iter().find_map(|item| find(item, key)),
            Value::Object(fields) => fields
                .iter()
                .find_map(|(field, item)| find(item, Some(field))),
            _ => None,
        }
    }
    arguments
        .iter()
        .find_map(|(key, value)| find(value, Some(key)))
}

/// Read every entity argument of a call to the registered tool `tool`
/// against `store`, in place: each text that settles on one entity becomes
/// that entity's UUID.
///
/// `Err` is the refusal for an id argument that names no entity or more than
/// one, or for a cursor or ticket of a connection that is gone.
pub fn resolve_call<G: crate::server::PersistableMcpStore>(
    store: &G,
    tool: &str,
    arguments: &mut HashMap<String, Value>,
) -> Result<(), String> {
    if let Some(refusal) = session_bound_refusal(arguments) {
        return Err(refusal);
    }
    if !needs_graph(tool, arguments) {
        return Ok(());
    }
    if crate::source_ticket::is_write_tool(tool) {
        return settle_write_targets(store, arguments);
    }
    // A reference query that names no declaration and one entity Kin holds
    // without parsing it asks about that entity, by its id.
    if let Some(query) = unparsed_query(tool, arguments) {
        use kin_ranking::entity_ranking::{CandidateReason, StrictNameResolution};
        let misses = matches!(
            kin_ranking::entity_ranking::resolve_name_strictly(store, &query),
            Ok(StrictNameResolution::Missing
                | StrictNameResolution::Candidates(CandidateReason::PartialName, _))
        );
        if let Some(id) = misses
            .then(|| crate::artifact_entity::address_in_tree(store, &query))
            .flatten()
        {
            arguments.remove("query");
            arguments.insert("entity_id".into(), Value::String(id));
            return Ok(());
        }
    }
    let reader = Reader { store };
    for (key, takes) in entity_arguments(tool) {
        let Some(value) = arguments.get_mut(*key) else {
            continue;
        };
        match value {
            Value::String(text) => {
                if let Some(id) = reader.read(key, text, *takes)? {
                    *text = id;
                }
            }
            Value::Array(items) => {
                for item in items.iter_mut() {
                    if let Value::String(text) = item {
                        if let Some(id) = reader.read(key, text, *takes)? {
                            *text = id;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

struct Reader<'s, G: crate::server::PersistableMcpStore> {
    store: &'s G,
}

impl<G: crate::server::PersistableMcpStore> Reader<'_, G> {
    /// The UUID `text` settles on, `None` to leave it as written, or the
    /// refusal for an id argument it does not settle.
    fn read(&self, key: &str, text: &str, takes: Takes) -> Result<Option<String>, String> {
        let shown = text.trim();
        if shown.is_empty() || exact_address(shown) {
            return Ok(None);
        }
        let written = Written::of(shown);
        if !written.asks(takes) {
            return Ok(None);
        }
        if let Some(prefix) = &written.prefix {
            match self.store.entity_ids_with_prefix(prefix)?.as_slice() {
                [one] => return Ok(Some(one.clone())),
                // No id starts with it: the name written beside it, if any,
                // is read below.
                [] => {}
                // Handed on as written, no reader of names could tell
                // them apart either, and would answer that nothing is named.
                several => {
                    return Err(format!(
                        "{key} `{shown}` is the start of {} entity ids. Pass one whole: {}.",
                        several.len(),
                        listed(several.iter().map(|id| format!("`{id}`")).collect())
                    ));
                }
            }
        }
        let mut labelled: Vec<Entity> = Vec::new();
        for (module, name) in written.labels() {
            for entity in self.in_module(module, name)? {
                if !labelled.iter().any(|held| held.id == entity.id) {
                    labelled.push(entity);
                }
            }
        }
        match labelled.as_slice() {
            [one] => return Ok(Some(one.id.to_string())),
            [] => {}
            // One label for several declarations: a property and its setter,
            // the overloads of one function. A reader of names is handed
            // the label and finds nothing, so the candidates are said here,
            // each by the address that reads back as it alone.
            several => return Err(self.ambiguous(key, shown, several)),
        }
        // A label without its spaces is a module-qualified name, which every
        // reader of names takes: `flask.app::route`.
        let qualified = written.name.replace(" :: ", "::");
        if takes == Takes::One && !written.name.is_empty() {
            // Declarations of one scope under one name: no longer name, and
            // no owner, tells them apart, so each is said by its address.
            use kin_ranking::entity_ranking::{CandidateReason, StrictNameResolution};
            // Only entities the name names exactly: a partial name, or a
            // member name several owners share, has candidates the tool
            // lists by names that already differ.
            if let StrictNameResolution::Candidates(
                CandidateReason::SameName | CandidateReason::SameQualifiedName,
                several,
            ) = kin_ranking::entity_ranking::resolve_name_strictly(self.store, &qualified)
                .map_err(|error| format!("graph read failed: {error}"))?
            {
                let label = |entity: &Entity| {
                    crate::semantic_owners::entity_module(self.store, entity)
                        .map(|module| (module, entity.name.clone()))
                };
                let first = several.first().and_then(label);
                if several.len() > 1
                    && first.is_some()
                    && several.iter().all(|entity| label(entity) == first)
                {
                    return Err(self.ambiguous(key, shown, &several));
                }
            }
        }
        if matches!(takes, Takes::Name | Takes::One) {
            // The tool reads the name itself. It is handed the qualified
            // spelling of a label no module answered to exactly, and the
            // name alone when a short id beside it named nothing.
            return Ok((qualified != shown && !qualified.is_empty()).then_some(qualified));
        }
        // No entity of code answers to it: an entity Kin holds without
        // parsing it may.
        let unparsed = || crate::artifact_entity::address_in_tree(self.store, shown);
        if written.name.is_empty() {
            return unparsed().map(Some).ok_or_else(|| absent(key, shown));
        }
        match kin_ranking::entity_ranking::resolve_name_strictly(self.store, &qualified)
            .map_err(|error| format!("graph read failed: {error}"))?
        {
            kin_ranking::entity_ranking::StrictNameResolution::One(entity) => {
                Ok(Some(entity.id.to_string()))
            }
            kin_ranking::entity_ranking::StrictNameResolution::Candidates(
                kin_ranking::entity_ranking::CandidateReason::PartialName,
                near,
            ) => match unparsed() {
                Some(id) => Ok(Some(id)),
                None => Err(format!(
                    "{} Nearest by name: {}.",
                    absent(key, shown),
                    listed(self.labels(&near))
                )),
            },
            kin_ranking::entity_ranking::StrictNameResolution::Candidates(_, several) => {
                Err(self.ambiguous(key, shown, &several))
            }
            kin_ranking::entity_ranking::StrictNameResolution::Missing => {
                unparsed().map(Some).ok_or_else(|| absent(key, shown))
            }
        }
    }

    /// The entities named exactly `name` that the graph places in `module`,
    /// as a row's `module` spells it.
    fn in_module(&self, module: &str, name: &str) -> Result<Vec<Entity>, String> {
        if module.is_empty() || name.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self
            .store
            .query_entities(&EntityFilter {
                name_pattern: Some(name.to_string()),
                ..Default::default()
            })
            .map_err(|error| format!("graph read failed: {error}"))?
            .into_iter()
            .filter(|entity| entity.name == name)
            .filter(|entity| {
                crate::semantic_owners::entity_module(self.store, entity).as_deref() == Some(module)
            })
            .collect())
    }

    /// Each candidate as an address that reads back as it alone: its
    /// `module :: name` label when no other candidate shares it, and the
    /// label with the candidate's short id after it otherwise, the way a row
    /// writes a name two entities share. `told_apart` adds, to candidates
    /// that share a label, the signature that tells them apart.
    fn labels_with(&self, candidates: &[Entity], told_apart: bool) -> Vec<String> {
        let labels: Vec<Option<String>> = candidates
            .iter()
            .map(|entity| {
                crate::semantic_owners::entity_module(self.store, entity)
                    .map(|module| format!("{module} :: {}", entity.name))
            })
            .collect();
        // As many id digits as tell every candidate from the others.
        let digits = (SHORT_ID_MIN..=32)
            .step_by(4)
            .find(|digits| {
                let shorts: std::collections::BTreeSet<String> = candidates
                    .iter()
                    .map(|entity| short_of(entity, *digits))
                    .collect();
                shorts.len() == candidates.len()
            })
            .unwrap_or(32);
        candidates
            .iter()
            .zip(&labels)
            .map(|(entity, label)| {
                let alone = label.as_ref().filter(|label| {
                    labels
                        .iter()
                        .flatten()
                        .filter(|other| other == label)
                        .count()
                        == 1
                });
                match (alone, label) {
                    (Some(label), _) => format!("`{label}`"),
                    (None, label) => {
                        let name = label.as_deref().unwrap_or(entity.name.as_str());
                        let address = format!("`{name} #{}`", short_of(entity, digits));
                        // A module and a function it holds share one label
                        // when they share a name, and only the kind tells
                        // them apart without another read.
                        let kinds_differ =
                            candidates.iter().zip(&labels).any(|(other, other_label)| {
                                other_label == label && other.kind != entity.kind
                            });
                        let kind = kinds_differ.then(|| crate::module_code::kind_word(entity.kind));
                        let signature = entity.signature.trim();
                        let signature = (told_apart
                            && !signature.is_empty()
                            && !signature.contains('`'))
                        .then(|| {
                            let shown: String = signature.chars().take(SIGNATURE_SHOWN).collect();
                            format!("`{}`", shown.trim_end())
                        });
                        match (kind, signature) {
                            (Some(kind), Some(signature)) => {
                                format!("{address} ({kind} {signature})")
                            }
                            (Some(kind), None) => format!("{address} ({kind})"),
                            (None, Some(signature)) => format!("{address} ({signature})"),
                            (None, None) => address,
                        }
                    }
                }
            })
            .collect()
    }

    fn labels(&self, candidates: &[Entity]) -> Vec<String> {
        self.labels_with(candidates, false)
    }

    fn ambiguous(&self, key: &str, shown: &str, candidates: &[Entity]) -> String {
        // In the order their source declares them, so the list reads the
        // same on every call: the getter before its setter.
        let mut candidates = candidates.to_vec();
        candidates.sort_by(|left, right| {
            let place = |entity: &Entity| {
                (
                    entity.file_origin.as_ref().map(|origin| origin.0.clone()),
                    entity.span.as_ref().map(|span| span.start_byte),
                )
            };
            place(left).cmp(&place(right)).then(left.id.cmp(&right.id))
        });
        let candidates = candidates.as_slice();
        let said = |told_apart: bool| {
            format!(
                "{key} `{shown}` names {} entities, and this argument takes one. Pass one of: {}.",
                candidates.len(),
                listed(self.labels_with(candidates, told_apart))
            )
        };
        // A signature that spells a path would have the whole sentence
        // withheld at the agent surface, and the addresses matter more.
        Some(said(true))
            .filter(|said| crate::semantic_surface::is_semantic(said))
            .unwrap_or_else(|| said(false))
    }
}

/// The first `digits` hex digits of an entity's id.
fn short_of(entity: &Entity, digits: usize) -> String {
    entity
        .id
        .to_string()
        .chars()
        .filter(|c| *c != '-')
        .take(digits)
        .collect()
}

/// The most characters of a signature a list of candidates shows for one.
const SIGNATURE_SHOWN: usize = 100;

fn absent(key: &str, shown: &str) -> String {
    format!(
        "{key} `{shown}` names no entity this repository holds. Find it with semantic_locate, \
         then pass the id or the `module :: name` its row shows."
    )
}

/// Up to [`CANDIDATES_SHOWN`] spellings, then how many more there are.
fn listed(mut spellings: Vec<String>) -> String {
    let more = spellings.len().saturating_sub(CANDIDATES_SHOWN);
    spellings.truncate(CANDIDATES_SHOWN);
    let shown = spellings.join(", ");
    match more {
        0 => shown,
        more => format!("{shown}, and {more} more"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_db::InMemoryGraph;
    use kin_model::entity::{
        Entity, EntityKind, EntityMetadata, EntityRole, FingerprintAlgorithm, SemanticFingerprint,
        Visibility,
    };
    use kin_model::graph::EntityStore as _;
    use kin_model::ids::{EntityId, FilePathId, Hash256, LanguageId, RelationId};
    use kin_model::relation::{Relation, RelationKind, RelationOrigin};
    use kin_model::GraphNodeId;
    use serde_json::json;

    fn entity(kind: EntityKind, name: &str, file: &str) -> Entity {
        Entity {
            id: EntityId::new(),
            kind,
            name: name.to_string(),
            language: LanguageId::Python,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([0; 32]),
                signature_hash: Hash256::from_bytes([0; 32]),
                behavior_hash: Hash256::from_bytes([0; 32]),
                equivalence_hash: Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(FilePathId::new(file)),
            span: None,
            signature: format!("def {name}()"),
            visibility: Visibility::Public,
            role: EntityRole::Source,
            doc_summary: None,
            metadata: EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    /// A small package as an adapter records one: `flask/app.py` declares
    /// `route` and `serve`, which calls it, and two more modules each declare
    /// a `dumps`.
    struct Fixture {
        graph: InMemoryGraph,
        route: Entity,
        serve: Entity,
        dumps: [Entity; 2],
    }

    fn fixture() -> Fixture {
        let graph = InMemoryGraph::default();
        for (name, file) in [
            ("flask", "flask/__init__.py"),
            ("app", "flask/app.py"),
            ("json", "flask/json.py"),
            ("provider", "flask/provider.py"),
        ] {
            graph
                .upsert_entity(&entity(EntityKind::Module, name, file))
                .unwrap();
        }
        let route = entity(EntityKind::Function, "route", "flask/app.py");
        let serve = entity(EntityKind::Function, "serve", "flask/app.py");
        let dumps = [
            entity(EntityKind::Function, "dumps", "flask/json.py"),
            entity(EntityKind::Function, "dumps", "flask/provider.py"),
        ];
        for declared in [&route, &serve, &dumps[0], &dumps[1]] {
            graph.upsert_entity(declared).unwrap();
        }
        graph
            .upsert_relation(&Relation {
                id: RelationId::new(),
                kind: RelationKind::Calls,
                src: GraphNodeId::Entity(serve.id),
                dst: GraphNodeId::Entity(route.id),
                confidence: 1.0,
                origin: RelationOrigin::Parsed,
                created_in: None,
                import_source: None,
                evidence: Vec::new(),
            })
            .unwrap();
        Fixture {
            graph,
            route,
            serve,
            dumps,
        }
    }

    fn short(entity: &Entity) -> String {
        entity.id.to_string()[..8].to_string()
    }

    fn resolved(fixture: &Fixture, tool: &str, arguments: Value) -> Result<Value, String> {
        let mut arguments: HashMap<String, Value> = serde_json::from_value(arguments).unwrap();
        resolve_call(&fixture.graph, tool, &mut arguments)?;
        Ok(serde_json::to_value(arguments).unwrap())
    }

    /// Every spelling a reply serves for an entity reads back as its UUID
    /// against the graph alone: no table of any connection is consulted.
    #[test]
    fn every_served_spelling_of_an_entity_reads_back_from_the_graph() {
        let fixture = fixture();
        let id = fixture.route.id.to_string();
        for spelling in [
            "flask.app :: route".to_string(),
            short(&fixture.route),
            format!("#{}", short(&fixture.route)),
            format!("route #{}", short(&fixture.route)),
            format!("flask.app :: route #{}", short(&fixture.route)),
            id[..13].to_string(),
            // What a model types from memory of a label, and a plain name.
            "flask.app::route".to_string(),
            "flask.app.route".to_string(),
            "route".to_string(),
            format!("entity:{id}"),
            id.clone(),
        ] {
            let call = resolved(&fixture, "find_references", json!({"entity_id": spelling}))
                .unwrap_or_else(|refusal| panic!("{spelling}: {refusal}"));
            let read = call["entity_id"].as_str().unwrap();
            assert_eq!(
                read.strip_prefix("entity:").unwrap_or(read),
                id,
                "{spelling}"
            );
        }
        // A list is read item by item, and a second id argument beside the
        // first.
        let call = resolved(
            &fixture,
            "impact_analysis",
            json!({"entity_ids": ["flask.app :: route", short(&fixture.serve)]}),
        )
        .unwrap();
        assert_eq!(
            call["entity_ids"],
            json!([id, fixture.serve.id.to_string()])
        );
        let call = resolved(
            &fixture,
            "find_references",
            json!({"entity_id": "flask.app :: route", "proof_for_caller": "flask.app :: serve"}),
        )
        .unwrap();
        assert_eq!(call["proof_for_caller"], fixture.serve.id.to_string());
    }

    /// An argument that takes one id says what it could not settle, with
    /// addresses that each read back as one entity.
    #[test]
    fn an_id_argument_that_names_none_or_several_is_refused_with_the_candidates() {
        let fixture = fixture();
        let refusal = resolved(
            &fixture,
            "graph_neighborhood",
            json!({"entity_id": "dumps"}),
        )
        .unwrap_err();
        assert_eq!(
            refusal,
            "entity_id `dumps` names 2 entities, and this argument takes one. Pass one of: \
             `flask.json :: dumps`, `flask.provider :: dumps`."
        );
        for candidate in ["flask.json :: dumps", "flask.provider :: dumps"] {
            let call = resolved(
                &fixture,
                "graph_neighborhood",
                json!({"entity_id": candidate}),
            )
            .unwrap();
            assert!(fixture
                .dumps
                .iter()
                .any(|entity| call["entity_id"] == entity.id.to_string()));
        }
        let refusal = resolved(
            &fixture,
            "find_references",
            json!({"entity_id": "flask.app :: no_such_function"}),
        )
        .unwrap_err();
        assert_eq!(
            refusal,
            "entity_id `flask.app :: no_such_function` names no entity this repository holds. \
             Find it with semantic_locate, then pass the id or the `module :: name` its row shows."
        );
        // A short id of another repository, or of an entity since removed.
        let refusal = resolved(
            &fixture,
            "impact_analysis",
            json!({"entity_ids": ["0badc0de"]}),
        )
        .unwrap_err();
        assert!(
            refusal.starts_with("entity_ids `0badc0de` names no entity"),
            "{refusal}"
        );
        // A name near others lists them rather than choosing one.
        let refusal =
            resolved(&fixture, "find_references", json!({"entity_id": "dump"})).unwrap_err();
        assert!(
            refusal.contains("names no entity this repository holds")
                && refusal.contains("Nearest by name: `flask.json :: dumps`"),
            "{refusal}"
        );
    }

    /// A tool that reads names itself keeps its own answer about one: only
    /// an exact address is settled here, and a name is handed on as written.
    #[test]
    fn a_name_its_own_tool_reads_is_handed_on_as_written() {
        let fixture = fixture();
        let id = fixture.route.id.to_string();
        for (tool, key) in [
            ("trace_data_flow", "focal"),
            ("get_entity_source", "entity_id"),
            (crate::handlers::path::TOOL_NAME, "from"),
        ] {
            for (spelling, read) in [
                ("flask.app :: route".to_string(), id.clone()),
                // The same label without its spaces.
                ("flask.app::route".to_string(), id.clone()),
                (short(&fixture.route), id.clone()),
                (format!("route #{}", short(&fixture.route)), id.clone()),
                // Names, the ambiguous and the absent ones included.
                ("route".to_string(), "route".to_string()),
                ("dumps".to_string(), "dumps".to_string()),
                ("flask.app.route".to_string(), "flask.app.route".to_string()),
                (
                    "no_such_function".to_string(),
                    "no_such_function".to_string(),
                ),
                ("deadbeef".to_string(), "deadbeef".to_string()),
                ("Model#save".to_string(), "Model#save".to_string()),
                // A label no module answers to exactly, and a name beside a
                // short id that names nothing, in the spelling the tool reads.
                ("app :: route".to_string(), "app::route".to_string()),
                ("route #0badc0de".to_string(), "route".to_string()),
            ] {
                let call = resolved(&fixture, tool, json!({ key: spelling })).unwrap();
                assert_eq!(call[key], read, "{tool} {spelling}");
            }
        }
        let call = resolved(
            &fixture,
            "get_context_pack",
            json!({"entities": ["flask.app :: route", "dumps", short(&fixture.serve)]}),
        )
        .unwrap();
        assert_eq!(
            call["entities"],
            json!([id, "dumps", fixture.serve.id.to_string()])
        );
        // A name costs no graph read at all, and neither does a UUID.
        assert!(!needs_graph(
            "trace_data_flow",
            &HashMap::from([("focal".to_string(), json!("route"))])
        ));
        assert!(!needs_graph(
            "find_references",
            &HashMap::from([("entity_id".to_string(), json!(id))])
        ));
        assert!(needs_graph(
            "trace_data_flow",
            &HashMap::from([("focal".to_string(), json!("flask.app :: route"))])
        ));
    }

    /// The fixture with a property and its setter in it: two methods of one
    /// class under one name.
    fn twins() -> (Fixture, [Entity; 2]) {
        let fixture = fixture();
        let mut getter = entity(EntityKind::Method, "App.debug", "flask/app.py");
        getter.signature = "@property def debug(self)".into();
        let mut setter = entity(EntityKind::Method, "App.debug", "flask/app.py");
        setter.signature = "@setter def debug(self, value)".into();
        for declared in [&getter, &setter] {
            fixture.graph.upsert_entity(declared).unwrap();
        }
        (fixture, [getter, setter])
    }

    /// A module and a function it declares under the module's own name share
    /// one label, so each is listed with its kind, and a caller picks the one
    /// it means without reading either.
    #[test]
    fn a_module_and_its_namesake_function_are_told_apart_by_kind() {
        let fixture = fixture();
        let function = entity(EntityKind::Function, "app", "flask/app.py");
        fixture.graph.upsert_entity(&function).unwrap();
        let refusal =
            resolved(&fixture, "get_entity_source", json!({"entity_id": "app"})).unwrap_err();
        assert!(refusal.contains("names 2 entities"), "{refusal}");
        assert!(
            refusal.contains(&format!(
                "`flask.app :: app #{}` (function",
                short(&function)
            )),
            "{refusal}"
        );
        assert!(refusal.contains("` (module"), "{refusal}");
    }

    /// A name two declarations of one scope share is answered with both and
    /// the address of each, whether it was written as a label or alone, and
    /// each address reads back as that declaration.
    #[test]
    fn a_name_declared_twice_in_one_scope_is_answered_with_each_address() {
        let (fixture, [getter, setter]) = twins();
        // The fixture records no span, so the two are listed by id.
        let mut ordered = [&getter, &setter];
        ordered.sort_by_key(|entity| entity.id);
        let listed = format!(
            "names 2 entities, and this argument takes one. Pass one of: `flask.app :: \
             App.debug #{}` (`{}`), `flask.app :: App.debug #{}` (`{}`).",
            short(ordered[0]),
            ordered[0].signature,
            short(ordered[1]),
            ordered[1].signature
        );
        for spelling in [
            "flask.app :: App.debug",
            "flask.app::App.debug",
            "App.debug",
        ] {
            assert_eq!(
                resolved(
                    &fixture,
                    "get_entity_source",
                    json!({"entity_id": spelling})
                )
                .unwrap_err(),
                format!("entity_id `{spelling}` {listed}"),
            );
        }
        // The label is refused the same way wherever one entity is asked for.
        assert_eq!(
            resolved(
                &fixture,
                "trace_data_flow",
                json!({"focal": "flask.app :: App.debug"})
            )
            .unwrap_err(),
            format!("focal `flask.app :: App.debug` {listed}"),
        );
        assert_eq!(
            resolved(
                &fixture,
                "find_references",
                json!({"entity_id": "flask.app :: App.debug"})
            )
            .unwrap_err(),
            format!("entity_id `flask.app :: App.debug` {listed}"),
        );
        for twin in [&getter, &setter] {
            for spelling in [
                format!("flask.app :: App.debug #{}", short(twin)),
                format!("App.debug #{}", short(twin)),
                format!("#{}", short(twin)),
            ] {
                let call = resolved(
                    &fixture,
                    "get_entity_source",
                    json!({"entity_id": spelling}),
                )
                .unwrap();
                assert_eq!(call["entity_id"], twin.id.to_string(), "{spelling}");
            }
        }
        // Names that different scopes hold keep the tool's own answer, and
        // so does a name that only comes near one.
        for name in ["dumps", "dump", "rout", "no_such_function"] {
            let call = resolved(&fixture, "get_entity_source", json!({"entity_id": name})).unwrap();
            assert_eq!(call["entity_id"], name);
        }
        // A signature that spells a path is left out, not the addresses.
        let mut pathy = entity(EntityKind::Function, "load", "flask/app.py");
        pathy.signature = "def load(path=\"conf/app.toml\")".into();
        let twin = entity(EntityKind::Function, "load", "flask/app.py");
        for declared in [&pathy, &twin] {
            fixture.graph.upsert_entity(declared).unwrap();
        }
        let refusal =
            resolved(&fixture, "get_entity_source", json!({"entity_id": "load"})).unwrap_err();
        assert!(crate::semantic_surface::is_semantic(&refusal), "{refusal}");
        assert!(
            refusal.contains(&format!("`flask.app :: load #{}`", short(&pathy)))
                && !refusal.contains("conf/app.toml"),
            "{refusal}"
        );
    }

    /// A write names its entity twice, by its base and by its target. The
    /// base decides; the target only has to agree with it, in any spelling a
    /// reply serves for that entity.
    #[test]
    fn a_write_target_is_settled_on_the_entity_its_base_was_read_from() {
        let (fixture, [getter, setter]) = twins();
        let ticket = |entity: &Entity| format!("sb2.{}.3.{}", entity.id, "d".repeat(32));
        let operation = |target: &str, entity: &Entity| {
            json!({"verb": "patch", "target": target, "description": "change it",
                "payload": {"EntitySourcePatch": {
                    "source_base": {"ticket": ticket(entity)},
                    "edits": [{"old_text": "a", "new_text": "b"}]}}})
        };
        let settle = |operations: Value, keyed: bool| {
            let mut arguments = json!({"operations": operations});
            if keyed {
                arguments["request_id"] = json!("retry-1");
            }
            resolved(&fixture, "kin_transaction_commit", arguments)
        };
        let id = setter.id.to_string();
        for target in [
            id.clone(),
            format!("entity:{id}"),
            format!("#{}", short(&setter)),
            format!("App.debug #{}", short(&setter)),
            format!("flask.app :: App.debug #{}", short(&setter)),
            // A name the entity shares with its twin: the base says which.
            "flask.app :: App.debug".to_string(),
            "flask.app::App.debug".to_string(),
            "flask.app.App.debug".to_string(),
            "App.debug".to_string(),
        ] {
            let call = settle(json!([operation(&target, &setter)]), false)
                .unwrap_or_else(|refusal| panic!("{target}: {refusal}"));
            assert_eq!(call["operations"][0]["target"], id, "{target}");
        }
        // Another entity, by the start of its id, by its whole id and by name.
        let read_as = format!("`flask.app :: App.debug #{}`", short(&setter));
        for target in [
            format!("flask.app :: App.debug #{}", short(&getter)),
            getter.id.to_string(),
            "flask.app :: route".to_string(),
            "route".to_string(),
        ] {
            assert_eq!(
                settle(json!([operation(&target, &setter)]), false).unwrap_err(),
                format!(
                    "target `{target}` is not the entity this source_base was read from, which \
                     is {read_as}. Pass that as target to change it, or read `{target}` with \
                     source and send the source_base that read returns. Nothing was written."
                ),
            );
        }
        // Every base slot is read: an update's, a removal's, a creation's.
        for payload in [
            json!({"EntitySourceBase": {"ticket": ticket(&setter)}}),
            json!({"EntityRemove": {"source_base": {"ticket": ticket(&setter)}}}),
            json!({"EntityCreate": {"source_base": {"ticket": ticket(&setter)},
                "placement": "sibling_after", "name": "added", "kind": "method", "body": "x"}}),
        ] {
            let call = settle(
                json!([{"verb": "update", "target": format!("#{}", short(&setter)),
                    "description": "write", "payload": payload}]),
                false,
            )
            .unwrap();
            assert_eq!(call["operations"][0]["target"], id);
        }
        // A call that may be a retry is not read against today's graph: the
        // start of the id still settles, and a name is left as it was sent.
        let call = settle(
            json!([
                operation(&format!("#{}", short(&setter)), &setter),
                operation("App.debug", &setter)
            ]),
            true,
        )
        .unwrap();
        assert_eq!(call["operations"][0]["target"], id);
        assert_eq!(call["operations"][1]["target"], "App.debug");
        // An operation with no base, and one whose target is its base's
        // entity already, cost no graph read.
        assert!(!needs_graph(
            "kin_transaction_commit",
            &serde_json::from_value(json!({"operations": [
                operation(&id, &setter),
                {"verb": "create", "target": "pkg/new.py", "description": "new", "body": "x"}
            ]}))
            .unwrap()
        ));
        // What the forwarding check decides without a graph.
        assert_eq!(check_write_target(&operation("App.debug", &setter)), Ok(()));
        assert_eq!(
            check_write_target(&operation(&format!("#{}", short(&getter)), &setter)).unwrap_err(),
            format!(
                "target `#{}` is not the entity this source_base was read from, which is the \
                 entity whose id starts `{}`. Pass that as target to change it, or read `#{}` \
                 with source and send the source_base that read returns. Nothing was written.",
                short(&getter),
                short(&setter),
                short(&getter)
            )
        );
    }

    /// A cursor or a ticket stands for a frozen answer or an exact base of
    /// the connection that served it, and says so when that one is gone.
    #[test]
    fn a_cursor_or_ticket_of_a_gone_connection_says_where_it_belongs() {
        let fixture = fixture();
        let refusal =
            resolved(&fixture, "find_references", json!({"cursor": "p97a18f5"})).unwrap_err();
        assert_eq!(
            refusal,
            "cursor `p97a18f5` belongs to an earlier connection, and this one cannot read it. \
             Repeat the query without cursor."
        );
        let refusal = resolved(
            &fixture,
            "kin_mutate",
            json!({"operations": [{"verb": "patch", "target": fixture.route.id.to_string(),
                "payload": {"EntitySourcePatch": {"source_base": {"ticket": "t2758ab7"},
                    "edits": []}}}]}),
        )
        .unwrap_err();
        assert!(
            refusal.starts_with("source_base ticket `t2758ab7` belongs to an earlier connection"),
            "{refusal}"
        );
        // A whole cursor, and text that only looks short, pass.
        for cursor in [
            "eyJ2IjoxLCJzbmFwc2hvdCI6IjJiOWRmZTJlLWE1NWItNDg1YS1iNTgxLWRiZDJiYWYyZWM2ZCJ9",
            "page-two",
            "p97a18f",
        ] {
            assert!(
                session_bound_refusal(&HashMap::from([("cursor".to_string(), json!(cursor))]))
                    .is_none()
            );
        }
        assert!(
            session_bound_refusal(&HashMap::from([("query".to_string(), json!("p97a18f5"))]))
                .is_none()
        );
    }

    /// The six read tools, each called on a connection that has served
    /// nothing, by a label and by a short id: every call reaches its entity.
    #[tokio::test]
    async fn a_fresh_connection_reads_labels_and_short_ids_in_every_read_tool() {
        use crate::server::{process_message, McpServerConfig, SessionAuthorityMode};
        use crate::session::SessionRegistry;
        let fixture = fixture();
        let id = fixture.route.id.to_string();
        let call = |tool: &'static str, arguments: Value| {
            let graph = &fixture.graph;
            async move {
                // A connection of its own for every call: nothing is remembered.
                let config = McpServerConfig {
                    session_authority_mode: SessionAuthorityMode::OfflineFallback,
                    ..Default::default()
                };
                let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": tool, "arguments": arguments}})
                .to_string();
                let response =
                    process_message(&message, graph, &config, &SessionRegistry::empty_for_test())
                        .await
                        .expect("response");
                assert!(response.error.is_none(), "{tool}: {:?}", response.error);
                let result = response.result.expect("result");
                (
                    result["isError"].as_bool().unwrap_or(false),
                    result["content"][0]["text"].as_str().unwrap().to_string(),
                )
            }
        };
        for spelling in ["flask.app :: route".to_string(), short(&fixture.route)] {
            for (tool, arguments) in [
                ("refs", json!({"entity_id": spelling})),
                ("source", json!({"entity_id": spelling})),
                // `entities` is read the same way, above; this in-process
                // route answers it only through a daemon.
                ("context", json!({"entity_id": spelling})),
                ("trace", json!({"focal": spelling, "direction": "callers"})),
                ("impact", json!({"entity_ids": [spelling]})),
                ("neighbors", json!({"entity_id": spelling})),
            ] {
                let (_, text) = call(tool, arguments).await;
                for failure in [
                    "UUID parsing failed",
                    "invalid entity_id",
                    "no entity found matching",
                    "names no entity",
                    "takes an entity UUID",
                    "no such entity",
                ] {
                    assert!(!text.contains(failure), "{tool} {spelling}: {text}");
                }
                assert!(
                    text.contains(&id) || text.contains("\"route\""),
                    "{tool} {spelling}: {text}"
                );
            }
        }
        // refs and neighbors name the caller, which only the right entity has.
        for tool in ["refs", "neighbors"] {
            let (failed, text) = call(tool, json!({"entity_id": "flask.app :: route"})).await;
            assert!(!failed && text.contains("serve"), "{tool}: {text}");
        }
        // An id argument that names two entities is refused before anything
        // runs, as an error the agent can act on.
        let (failed, text) = call("refs", json!({"entity_id": "dumps"})).await;
        assert!(failed, "{text}");
        assert!(text.contains("names 2 entities"), "{text}");
        let (failed, text) = call("refs", json!({"cursor": "p97a18f5"})).await;
        assert!(failed, "{text}");
        assert!(text.contains("belongs to an earlier connection"), "{text}");
    }

    /// Two JavaScript packages as an adapter records them. `react-reconciler`
    /// has an `index` file at its root, which names it. `react-dom-bindings`
    /// has none: every source file sits under `src/`, and only the manifest
    /// the tree holds says the directory is a package. `commitRoot`, in the
    /// first, and `setInitialProperties`, in the second, each call
    /// `getStylesObjectFromElement`.
    struct Packages {
        graph: InMemoryGraph,
        styles: Entity,
        setter: Entity,
        commit: Entity,
    }

    const BINDINGS_FILE: &str = "packages/react-dom-bindings/src/client/ReactDOMComponent.js";
    const BINDINGS_MANIFEST: &str = "packages/react-dom-bindings/package.json";
    const RECONCILER_FILE: &str = "packages/react-reconciler/src/ReactFiberWorkLoop.js";

    /// The two packages in a store whose tree is admitted with `tree`.
    fn packages(tree: &[&str]) -> Packages {
        use kin_model::{
            ArtifactId, LocatedEntry, RepoPath, TransactionDelta, TreeDelta, TreeEntry,
        };
        let script = |kind: EntityKind, name: &str, file: &str| {
            let mut entity = entity(kind, name, file);
            entity.language = LanguageId::JavaScript;
            entity.signature = format!("function {name}()");
            entity
        };
        let graph = InMemoryGraph::default();
        for (name, file) in [
            ("ReactDOMComponent", BINDINGS_FILE),
            ("react-reconciler", "packages/react-reconciler/index.js"),
            ("ReactFiberWorkLoop", RECONCILER_FILE),
        ] {
            graph
                .upsert_entity(&script(EntityKind::Module, name, file))
                .unwrap();
        }
        let styles = script(
            EntityKind::Function,
            "getStylesObjectFromElement",
            BINDINGS_FILE,
        );
        let setter = script(EntityKind::Function, "setInitialProperties", BINDINGS_FILE);
        let commit = script(EntityKind::Function, "commitRoot", RECONCILER_FILE);
        for declared in [&styles, &setter, &commit] {
            graph.upsert_entity(declared).unwrap();
        }
        for (caller, callee) in [(&setter, &styles), (&commit, &styles)] {
            graph
                .upsert_relation(&Relation {
                    id: RelationId::new(),
                    kind: RelationKind::Calls,
                    src: GraphNodeId::Entity(caller.id),
                    dst: GraphNodeId::Entity(callee.id),
                    confidence: 1.0,
                    origin: RelationOrigin::Parsed,
                    created_in: None,
                    import_source: None,
                    evidence: Vec::new(),
                })
                .unwrap();
        }
        graph
            .apply_transaction_delta(&TransactionDelta {
                tree_deltas: tree
                    .iter()
                    .enumerate()
                    .map(|(index, path)| TreeDelta::Added {
                        artifact_id: ArtifactId::new(),
                        new: LocatedEntry::new(
                            RepoPath::from_utf8(*path).unwrap(),
                            TreeEntry::blob(Hash256::from_bytes([index as u8 + 1; 32]), false),
                        ),
                    })
                    .collect(),
                ..Default::default()
            })
            .unwrap();
        Packages {
            graph,
            styles,
            setter,
            commit,
        }
    }

    /// One call on a connection of its own, answered with the machine reply.
    async fn asked(graph: &InMemoryGraph, tool: &str, arguments: Value) -> String {
        use crate::server::{process_message, McpServerConfig, SessionAuthorityMode};
        use crate::session::SessionRegistry;
        let config = McpServerConfig {
            session_authority_mode: SessionAuthorityMode::OfflineFallback,
            ..Default::default()
        };
        let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}})
        .to_string();
        let response =
            process_message(&message, graph, &config, &SessionRegistry::empty_for_test())
                .await
                .expect("response");
        assert!(response.error.is_none(), "{tool}: {:?}", response.error);
        response.result.expect("result")["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// A JavaScript package with no `index` file at its root is named by the
    /// manifest its tree holds, so a function in it is placed in the package
    /// (`react-dom-bindings.ReactDOMComponent`) and a question scoped to the
    /// package is answered. Every spelling of the label reads back in every
    /// read tool: the one a reply prints now, and the one a reply printed
    /// before the package was a level.
    #[tokio::test]
    async fn a_package_manifest_in_the_tree_names_its_modules_in_every_read_tool() {
        let fixture = packages(&[
            "package.json",
            BINDINGS_MANIFEST,
            BINDINGS_FILE,
            "packages/react-reconciler/package.json",
            "packages/react-reconciler/index.js",
            RECONCILER_FILE,
        ]);
        let graph = &fixture.graph;
        // The manifest is an artifact of the tree the graph holds, which is
        // all the rule reads.
        assert!(graph
            .artifact_id_at_path(&kin_model::RepoPath::from_utf8(BINDINGS_MANIFEST).unwrap())
            .is_some());
        let module = |entity: &Entity| crate::semantic_owners::entity_module(graph, entity);
        assert_eq!(
            module(&fixture.styles).as_deref(),
            Some("react-dom-bindings.ReactDOMComponent")
        );
        // A package an `index` file already names is named once.
        assert_eq!(
            module(&fixture.commit).as_deref(),
            Some("react-reconciler.ReactFiberWorkLoop")
        );
        // The index every qualified name is matched against agrees with the
        // label a row prints.
        let index = kin_ranking::module_qualifier::ModuleIndex::load(graph).unwrap();
        for entity in [&fixture.styles, &fixture.setter, &fixture.commit] {
            assert_eq!(Some(index.path(entity).join(".")), module(entity));
        }

        let id = fixture.styles.id.to_string();
        for spelling in [
            "react-dom-bindings.ReactDOMComponent :: getStylesObjectFromElement",
            "react-dom-bindings.ReactDOMComponent::getStylesObjectFromElement",
            "react-dom-bindings.ReactDOMComponent.getStylesObjectFromElement",
            // What a reply printed before the package named a level.
            "ReactDOMComponent :: getStylesObjectFromElement",
            "ReactDOMComponent::getStylesObjectFromElement",
            "ReactDOMComponent.getStylesObjectFromElement",
        ] {
            for (tool, arguments) in [
                ("refs", json!({"entity_id": spelling})),
                ("source", json!({"entity_id": spelling})),
                ("context", json!({"entity_id": spelling})),
                ("trace", json!({"focal": spelling, "direction": "callers"})),
                ("impact", json!({"entity_ids": [spelling]})),
                ("neighbors", json!({"entity_id": spelling})),
            ] {
                let text = asked(graph, tool, arguments).await;
                for failure in [
                    "UUID parsing failed",
                    "invalid entity_id",
                    "no entity found matching",
                    "names no entity",
                    "takes an entity UUID",
                    "no such entity",
                    "entities, and this argument takes one",
                ] {
                    assert!(!text.contains(failure), "{tool} {spelling}: {text}");
                }
                assert!(
                    text.contains(&id) || text.contains("\"getStylesObjectFromElement\""),
                    "{tool} {spelling}: {text}"
                );
            }
            // refs names both callers, which only the right entity has.
            let text = asked(graph, "refs", json!({"entity_id": spelling})).await;
            assert!(
                text.contains("setInitialProperties") && text.contains("commitRoot"),
                "{spelling}: {text}"
            );
        }

        // The package is a scope: the walk keeps the caller inside it and
        // counts the one in the other package as outside.
        let scoped: Value = serde_json::from_str(
            &asked(
                graph,
                "trace",
                json!({"focal": id, "direction": "callers", "scope": "react-dom-bindings"}),
            )
            .await,
        )
        .unwrap();
        assert_eq!(scoped["scope"]["module"], "react-dom-bindings", "{scoped}");
        assert_eq!(scoped["scope"]["outside"], 1, "{scoped}");
        assert_eq!(
            scoped["reach"]["members"]
                .as_array()
                .unwrap()
                .iter()
                .map(|member| (member["module"].as_str(), member["id"].as_str()))
                .collect::<Vec<_>>(),
            vec![(
                Some("react-dom-bindings.ReactDOMComponent"),
                Some(fixture.setter.id.to_string().as_str())
            )],
            "{scoped}"
        );
        assert_eq!(
            scoped["scope"]["outside_sample"][0]["module"], "react-reconciler.ReactFiberWorkLoop",
            "{scoped}"
        );
        // A locate for the label a row printed reads the same module path:
        // the package keeps the file, and another package's name rules it out.
        let located = |query: &str| {
            let read = crate::module_name_query::ModuleNameQuery::load(graph, query)
                .unwrap()
                .expect("a module-qualified spelling");
            (
                read.files().map(str::to_string).collect::<Vec<_>>(),
                read.ruled_out_files()
                    .map(str::to_string)
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            located("react-dom-bindings.ReactDOMComponent.getStylesObjectFromElement"),
            (vec![BINDINGS_FILE.to_string()], Vec::new())
        );
        assert_eq!(
            located("react-reconciler.ReactDOMComponent.getStylesObjectFromElement"),
            (Vec::new(), vec![BINDINGS_FILE.to_string()])
        );
    }

    /// A reference answer confined to a module path lists the callers inside
    /// it and counts the rest, so "which functions in this package use it"
    /// is one call. The scope is the one a walk takes: matched against the
    /// graph's module records, refused when it names none, and never an
    /// absence about the rest of the repository.
    #[tokio::test]
    async fn a_reference_answer_confined_to_a_package_lists_its_callers_and_counts_the_rest() {
        use crate::server::{
            present_for_client, process_message, McpServerConfig, SessionAuthorityMode,
        };
        use crate::session::SessionRegistry;
        let fixture = packages(&[
            BINDINGS_MANIFEST,
            BINDINGS_FILE,
            "packages/react-reconciler/package.json",
            "packages/react-reconciler/index.js",
            RECONCILER_FILE,
        ]);
        let graph = &fixture.graph;
        let id = fixture.styles.id.to_string();
        let callers = |reply: &Value| -> Vec<String> {
            reply["references"]
                .as_array()
                .unwrap_or_else(|| panic!("{reply}"))
                .iter()
                .map(|row| row["name"].as_str().unwrap().to_string())
                .collect()
        };
        let whole: Value =
            serde_json::from_str(&asked(graph, "refs", json!({"entity_id": id})).await).unwrap();
        let mut both = callers(&whole);
        both.sort();
        assert_eq!(both, ["commitRoot", "setInitialProperties"]);
        assert!(whole.get("caller_scope").is_none(), "{whole}");

        for (scope, inside) in [
            ("react-dom-bindings", "setInitialProperties"),
            // Case and an inner module path resolve as they do for a walk.
            (
                "React-DOM-Bindings.ReactDOMComponent",
                "setInitialProperties",
            ),
            ("react-reconciler", "commitRoot"),
        ] {
            let reply: Value = serde_json::from_str(
                &asked(graph, "refs", json!({"entity_id": id, "scope": scope})).await,
            )
            .unwrap();
            assert_eq!(callers(&reply), [inside], "{scope}: {reply}");
            assert_eq!(reply["total_upstream"], 1, "{scope}: {reply}");
            assert_eq!(
                reply["caller_scope"]["outside_module"], 1,
                "{scope}: {reply}"
            );
            assert!(
                scope.to_lowercase().starts_with(
                    &reply["caller_scope"]["module"]
                        .as_str()
                        .unwrap()
                        .to_lowercase()
                ),
                "{scope}: {reply}"
            );
        }

        // A scope that names no module path is refused, never answered as
        // though nothing in it used the entity, and so is one that cannot
        // narrow the answer asked for.
        for (arguments, said) in [
            (
                json!({"entity_id": id, "scope": "react-native"}),
                "scope 'react-native' names no module path",
            ),
            (
                json!({"entity_id": id, "scope": "react-dom-bindings", "view": "overrides"}),
                "cannot be combined with",
            ),
        ] {
            let text = asked(graph, "refs", arguments).await;
            assert!(text.contains(said), "{said}: {text}");
        }

        // What an agent reads: the scope and the count beside the rows, and
        // an empty scope said as that scope's alone.
        let agent = |arguments: Value| {
            let graph = &fixture.graph;
            async move {
                let config = McpServerConfig {
                    session_authority_mode: SessionAuthorityMode::OfflineFallback,
                    plain_replies: true,
                    agent_belt: true,
                    ..Default::default()
                };
                let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": "refs", "arguments": arguments}})
                .to_string();
                let response =
                    process_message(&message, graph, &config, &SessionRegistry::empty_for_test())
                        .await
                        .expect("response");
                let mut result = present_for_client(response, &message, &config)
                    .result
                    .unwrap();
                crate::lean_reply::lean_result(&mut result);
                let handles = crate::session_handles::SessionHandles::default();
                crate::text_reply::render_result(&mut result, "find_references", &handles);
                result["content"][0]["text"].as_str().unwrap().to_string()
            }
        };
        let text = agent(json!({"entity_id": id, "scope": "react-dom-bindings"})).await;
        let lead = text.lines().next().unwrap();
        assert!(
            lead.contains(
                "Only what sits inside react-dom-bindings is listed, as asked; 1 recorded row \
                 outside it is not listed."
            ),
            "{text}"
        );
        assert!(text.contains("setInitialProperties"), "{text}");
        assert!(!text.contains("commitRoot"), "{text}");

        let empty = agent(json!({"entity_id": fixture.setter.id.to_string(),
            "scope": "react-reconciler"}))
        .await;
        let lead = empty.lines().next().unwrap();
        assert!(
            lead.contains("no recorded reference to setInitialProperties inside react-reconciler"),
            "{empty}"
        );
        for claim in [
            "unused",
            "safe to conclude",
            "nothing uses setInitialProperties",
        ] {
            assert!(!lead.contains(claim), "{claim}: {empty}");
        }
    }

    /// A store whose tree holds no manifest for a directory names its modules
    /// as it did before, and says a scope naming the directory names nothing:
    /// the rule does not fire, and nothing fails for want of it.
    #[tokio::test]
    async fn a_tree_without_the_manifest_names_modules_as_before() {
        for tree in [
            &[BINDINGS_FILE, RECONCILER_FILE][..],
            // No tree at all, as a store that never admitted one holds.
            &[][..],
        ] {
            let fixture = packages(tree);
            let graph = &fixture.graph;
            assert_eq!(
                crate::semantic_owners::entity_module(graph, &fixture.styles).as_deref(),
                Some("ReactDOMComponent")
            );
            assert_eq!(
                crate::semantic_owners::entity_module(graph, &fixture.commit).as_deref(),
                Some("react-reconciler.ReactFiberWorkLoop")
            );
            let text = asked(
                graph,
                "refs",
                json!({"entity_id": "ReactDOMComponent :: getStylesObjectFromElement"}),
            )
            .await;
            assert!(text.contains("setInitialProperties"), "{text}");
            let refused = asked(
                graph,
                "trace",
                json!({"focal": fixture.styles.id.to_string(), "direction": "callers",
                    "scope": "react-dom-bindings"}),
            )
            .await;
            assert!(refused.contains("scope 'react-dom-bindings'"), "{refused}");
        }
    }

    /// A name search lists the declaration that is named so ahead of the
    /// ones that only hold the name, source code before test code, in the
    /// same order on every call. Measured, the method a reader asked for sat
    /// behind ten nested test helpers and off the page. Every declaration the
    /// reply has room for is shown. When rows short of the limit were cut to
    /// fit the reply, the reply says that, names the budget that would show
    /// more (a larger limit shows the same rows again), and names the
    /// declarations it did not show.
    #[tokio::test]
    async fn a_name_search_leads_with_the_declaration_named_so_and_says_why_rows_are_cut() {
        use crate::server::{
            present_for_client, process_message, McpServerConfig, SessionAuthorityMode,
        };
        use crate::session::SessionRegistry;
        let graph = InMemoryGraph::default();
        for (name, file) in [
            ("test_templating", "tests/test_templating.py"),
            ("app", "flask/app.py"),
        ] {
            graph
                .upsert_entity(&entity(EntityKind::Module, name, file))
                .unwrap();
        }
        for index in 0..24 {
            let mut helper = entity(
                EntityKind::Function,
                &format!("test_add_template_filter_with_name_{index}.<locals>.my_reverse"),
                "tests/test_templating.py",
            );
            helper.role = EntityRole::Test;
            graph.upsert_entity(&helper).unwrap();
        }
        let mut test = entity(
            EntityKind::Function,
            "test_add_template_filter",
            "tests/test_templating.py",
        );
        test.role = EntityRole::Test;
        graph.upsert_entity(&test).unwrap();
        graph
            .upsert_entity(&entity(
                EntityKind::Method,
                "App.add_template_filter",
                "flask/app.py",
            ))
            .unwrap();
        let asked_within = |limit: u64, budget: Option<u64>| {
            let graph = &graph;
            async move {
                let config = McpServerConfig {
                    session_authority_mode: SessionAuthorityMode::OfflineFallback,
                    plain_replies: true,
                    agent_belt: true,
                    ..Default::default()
                };
                let mut arguments =
                    json!({"mode": "name", "query": "add_template_filter", "limit": limit});
                if let Some(budget) = budget {
                    arguments["max_output_bytes"] = json!(budget);
                }
                let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": "locate", "arguments": arguments}})
                .to_string();
                let response =
                    process_message(&message, graph, &config, &SessionRegistry::empty_for_test())
                        .await
                        .expect("response");
                let mut result = present_for_client(response, &message, &config)
                    .result
                    .unwrap();
                crate::lean_reply::lean_result(&mut result);
                let handles = crate::session_handles::SessionHandles::default();
                crate::text_reply::render_result(&mut result, "semantic_search", &handles);
                result["content"][0]["text"].as_str().unwrap().to_string()
            }
        };
        let rows = |text: &str| -> Vec<String> {
            text.lines()
                .filter(|line| line.starts_with("  ") && line.contains(" :: "))
                .map(|line| line.trim().to_string())
                .collect()
        };

        let asked = |limit: u64| asked_within(limit, None);
        // A limit above every match: the reply has room for all 26 rows.
        let text = asked(50).await;
        let listed = rows(&text);
        assert_eq!(listed.len(), 26, "every declaration fits and shows: {text}");
        assert!(!text.contains("delivered:"), "{text}");
        assert!(
            listed[0].starts_with("flask.app :: App.add_template_filter"),
            "{text}"
        );
        assert!(
            listed[1].starts_with("tests.test_templating :: test_add_template_filter"),
            "{text}"
        );
        assert!(
            listed[2].contains("test_add_template_filter_with_name_0.<locals>"),
            "{text}"
        );
        // The same rows in the same order on a second call.
        assert_eq!(rows(&asked(50).await), listed);

        // The same question in a reply too small for all 26 rows.
        let text = asked_within(50, Some(2000)).await;
        let shown = rows(&text);
        assert!(
            shown.len() > 1 && shown.len() < 26,
            "the fixture must outgrow this reply: {text}"
        );
        // The rows that fit are the closest ones, each without its extras.
        let label = |row: &String| row.split("  ").next().unwrap_or_default().to_string();
        assert_eq!(
            shown.iter().map(label).collect::<Vec<_>>(),
            listed[..shown.len()].iter().map(label).collect::<Vec<_>>(),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "delivered: Showing {} of the 26, the closest matches first; the rest did not \
                 fit in this reply. Ask again with a larger max_output_bytes, up to 60000, or a \
                 narrower query such as module.name.",
                shown.len()
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "Not shown, by module: tests.test_templating ({}).",
                26 - shown.len()
            )),
            "{text}"
        );
        assert!(!text.contains("larger limit"), "{text}");

        // Cut by the limit itself, a larger limit is the advice.
        let text = asked(3).await;
        assert_eq!(rows(&text), listed[..3], "{text}");
        assert!(
            text.contains(
                "delivered: Showing 3 of the 26; ask again with a larger limit to see the rest."
            ),
            "{text}"
        );
    }

    /// A `locate` reply exactly as the stdio server writes it to an agent:
    /// plain, in text, with this connection's handles. Returns the machine
    /// answer's size beside it, which is the size the budget used to be
    /// charged against.
    async fn locate_as_delivered(graph: &InMemoryGraph, arguments: Value) -> (String, usize) {
        use crate::server::{
            present_for_client, process_message, McpServerConfig, SessionAuthorityMode,
        };
        use crate::session::SessionRegistry;
        let config = McpServerConfig {
            session_authority_mode: SessionAuthorityMode::OfflineFallback,
            plain_replies: true,
            agent_belt: true,
            reply_format: crate::text_reply::ReplyFormat::Text,
            session_handles: Some(std::sync::Arc::new(
                crate::session_handles::SessionHandles::default(),
            )),
            ..Default::default()
        };
        let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "locate", "arguments": arguments}})
        .to_string();
        let response =
            process_message(&message, graph, &config, &SessionRegistry::empty_for_test())
                .await
                .expect("response");
        let machine = response.result.as_ref().unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .len();
        let result = present_for_client(response, &message, &config)
            .result
            .unwrap();
        (
            result["content"][0]["text"].as_str().unwrap().to_string(),
            machine,
        )
    }

    /// The shape measured on React for `measureInstance`: renderers that each
    /// declare the function, a host config that declares it as a member of an
    /// object literal, and a fork that binds the name to a shim. Kin holds an
    /// entity for the functions and none for the member or the binding.
    const MEASURE_INSTANCE: [(&str, &str); 4] = [
        (
            "packages/react-art/src/ReactFiberConfigART.js",
            "export function measureInstance(instance) {\n  return null;\n}\n",
        ),
        (
            "packages/react-dom-bindings/src/client/ReactFiberConfigDOM.js",
            "export function measureInstance(instance) {\n  return instance.getBoundingClientRect();\n}\n",
        ),
        (
            "packages/react-noop-renderer/src/createReactNoop.js",
            "export const hostConfig = {\n  measureInstance(instance) {\n    return null;\n  },\n};\n",
        ),
        (
            "packages/react-reconciler/src/ReactFiberConfigWithNoMutation.js",
            "function shim() {\n  throw new Error('unsupported');\n}\nexport const measureInstance = shim;\n",
        ),
    ];

    /// A source unit whose last line no parse can read.
    const PARTLY_PARSED: (&str, &str) = (
        "packages/react-native-renderer/src/ReactFiberConfigFabric.js",
        "export function getPublicInstance(instance) {\n  return instance;\n}\nexport function (((\n",
    );

    fn admit_measure_instance(graph: &InMemoryGraph) {
        for (path, source) in MEASURE_INSTANCE {
            crate::declaration_coverage::fixtures::admit(graph, path, source, false);
        }
    }

    /// A name lookup as an agent reads it when a daemon answered: the search
    /// handler's answer under the daemon's envelope and the one verdict, then
    /// the reply the stdio server writes for it, plain and in text. A server
    /// with no daemon behind it certifies nothing, so its reply could not
    /// show what a certified lookup says.
    fn name_lookup_as_a_daemon_delivers(graph: &InMemoryGraph, query: &str) -> String {
        name_lookup_delivered_holding(graph, query, &[])
    }

    /// [`name_lookup_as_a_daemon_delivers`] with the content of `held`
    /// sources readable, as a daemon reads it from what the repository holds.
    fn name_lookup_delivered_holding(graph: &InMemoryGraph, query: &str, held: &[&str]) -> String {
        use crate::server::{present_for_client, McpServerConfig};
        let machine = crate::declaration_coverage::fixtures::search_holding(graph, query, held);
        let config = McpServerConfig {
            plain_replies: true,
            agent_belt: true,
            reply_format: crate::text_reply::ReplyFormat::Text,
            session_handles: Some(std::sync::Arc::new(
                crate::session_handles::SessionHandles::default(),
            )),
            ..Default::default()
        };
        let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "locate", "arguments": {"query": query, "mode": "name"}}})
        .to_string();
        let response = crate::types::JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            result: Some(json!({"content": [{"type": "text", "text": machine.to_string()}]})),
            error: None,
        };
        let result = present_for_client(response, &message, &config)
            .result
            .unwrap();
        result["content"][0]["text"].as_str().unwrap().to_string()
    }

    /// A reader shown the declarations of a name asks next whether they are all
    /// of them, and on React spent its remaining turns on that one question
    /// while Kin had the answer: every source unit carried a current record of
    /// its declarations, so the entities and the declarations Kin keeps no
    /// entity for were every declaration of the name. The answer line says so.
    #[test]
    fn a_name_lookup_says_when_its_reply_holds_every_declaration_of_the_name() {
        let _host = crate::edge_coverage::test_support::scoped_language_servers(&[]);
        let graph = InMemoryGraph::default();
        admit_measure_instance(&graph);

        let text = name_lookup_as_a_daemon_delivers(&graph, "measureInstance");
        assert_eq!(delivered_rows(&text).len(), 2, "{text}");
        assert!(
            text.starts_with(
                "Answer (complete except the sites listed below): these 2, with the 2 listed \
                 under not_held, are every declaration named measureInstance in this \
                 repository: Kin read all 6 declarations in its source, and no other has that \
                 name. Kin keeps no entity for the ones under not_held.\n"
            ),
            "{text}"
        );
        assert!(!text.contains("could be declared there"), "{text}");
    }

    /// One source unit whose record of declarations is not current could
    /// declare another, so the reply claims no such thing and says what is
    /// open.
    #[test]
    fn a_name_lookup_with_source_of_no_current_record_says_what_could_hold_another() {
        let _host = crate::edge_coverage::test_support::scoped_language_servers(&[]);
        let graph = InMemoryGraph::default();
        admit_measure_instance(&graph);
        crate::declaration_coverage::fixtures::admit(
            &graph,
            "packages/react-native-renderer/src/ReactFiberConfigFabric.js",
            "export function getPublicInstance(instance) {\n  return instance;\n}\n",
            true,
        );

        let text = name_lookup_as_a_daemon_delivers(&graph, "measureInstance");
        assert_eq!(delivered_rows(&text).len(), 2, "{text}");
        assert!(!text.contains("every declaration named"), "{text}");
        assert!(!text.contains("no other has that name"), "{text}");
        assert!(
            text.lines().next().unwrap().ends_with(
                "Kin has no current record of the declarations in part of this repository's \
                 source, so another measureInstance could be declared there."
            ),
            "{text}"
        );
    }

    /// A source unit parsed around a syntax error has a current record of the
    /// declarations its grammar shows, and the verdict certifies the lookup.
    /// What the parse could not read can still declare the name. Measured on
    /// React: 174 source units are parsed so, which left no lookup able to
    /// say it held every declaration. The search reads those regions back
    /// from held content: none spelling the name means none declares it, and
    /// the claim stands.
    #[test]
    fn a_name_lookup_over_source_parsed_in_part_states_closure_when_the_unread_part_is_clear() {
        let _host = crate::edge_coverage::test_support::scoped_language_servers(&[]);
        let graph = InMemoryGraph::default();
        admit_measure_instance(&graph);
        crate::declaration_coverage::fixtures::admit(
            &graph,
            PARTLY_PARSED.0,
            PARTLY_PARSED.1,
            false,
        );

        let text = name_lookup_delivered_holding(&graph, "measureInstance", &[PARTLY_PARSED.1]);
        assert_eq!(delivered_rows(&text).len(), 2, "{text}");
        assert!(
            text.starts_with(
                "Answer (complete except the sites listed below): these 2, with the 2 listed \
                 under not_held, are every declaration named measureInstance in this \
                 repository: Kin read all 7 declarations in its source, the name is spelled \
                 nowhere in the part of 1 source unit it could not parse, and no other has \
                 that name."
            ),
            "{text}"
        );
    }

    /// The same unit with the name spelled where the parse could not read:
    /// another declaration of it could be there, so the reply claims nothing
    /// and names the module to look in.
    #[test]
    fn a_name_lookup_names_the_module_whose_unread_part_spells_the_name() {
        let _host = crate::edge_coverage::test_support::scoped_language_servers(&[]);
        let graph = InMemoryGraph::default();
        admit_measure_instance(&graph);
        let spelling = "export function getPublicInstance(instance) {\n  return instance;\n}\n\
                        export function measureInstance(((\n";
        crate::declaration_coverage::fixtures::admit(&graph, PARTLY_PARSED.0, spelling, false);

        let text = name_lookup_delivered_holding(&graph, "measureInstance", &[spelling]);
        assert!(!text.contains("every declaration named"), "{text}");
        assert!(
            text.lines().next().unwrap().ends_with(
                "measureInstance is spelled inside a part of ReactFiberConfigFabric that Kin \
                 could not parse, so another declaration of it could be there."
            ),
            "{text}"
        );
    }

    /// With no held content to read the unread part back from, the reply
    /// says that, and claims nothing.
    #[test]
    fn a_name_lookup_over_source_parsed_in_part_claims_no_closure_it_cannot_read_back() {
        let _host = crate::edge_coverage::test_support::scoped_language_servers(&[]);
        let graph = InMemoryGraph::default();
        admit_measure_instance(&graph);
        // Content no other test admits: the search keeps what it read back
        // by digest for the life of the process.
        crate::declaration_coverage::fixtures::admit(
            &graph,
            PARTLY_PARSED.0,
            "export function getPublicInstanceOnly(instance) {\n  return instance;\n}\nexport function (((\n",
            false,
        );

        let text = name_lookup_as_a_daemon_delivers(&graph, "measureInstance");
        assert_eq!(delivered_rows(&text).len(), 2, "{text}");
        assert!(!text.contains("every declaration named"), "{text}");
        assert!(
            text.lines().next().unwrap().ends_with(
                "Kin could not read back 1 source unit it parsed only in part, so a \
                 declaration named measureInstance inside what it could not parse there would \
                 not be in this answer."
            ),
            "{text}"
        );
    }

    /// The store answers a name many declarations share words with by the
    /// names equal to it alone, which left a member of that name out of the
    /// rows while the reply stood certified. The rows hold every declaration
    /// whose own name is the query, and the claim counts them.
    #[test]
    fn a_name_lookup_holds_every_member_of_the_name_however_many_share_its_words() {
        let _host = crate::edge_coverage::test_support::scoped_language_servers(&[]);
        let graph = InMemoryGraph::default();
        let mut crowd = String::from("export function measure() {\n  return 0;\n}\n");
        for index in 0..40 {
            crowd.push_str(&format!(
                "export function measure_{index}() {{\n  return {index};\n}}\n"
            ));
        }
        crate::declaration_coverage::fixtures::admit(&graph, "src/sizes.js", &crowd, false);
        crate::declaration_coverage::fixtures::admit(
            &graph,
            "src/renderer.js",
            "export class Renderer {\n  measure() {\n    return 1;\n  }\n}\n",
            false,
        );

        let text = name_lookup_as_a_daemon_delivers(&graph, "measure");
        let rows = delivered_rows(&text);
        assert_eq!(rows.len(), 2, "{text}");
        assert!(rows[0].starts_with("sizes :: measure"), "{text}");
        assert!(
            rows[1].starts_with("renderer :: Renderer.measure"),
            "{text}"
        );
        assert!(
            text.starts_with(
                "Answer (complete): these 2 are every declaration named measure in this \
                 repository: Kin read all 43 declarations in its source, and no other has that \
                 name.\n"
            ),
            "{text}"
        );
    }

    fn delivered_rows(text: &str) -> Vec<String> {
        text.lines()
            .skip_while(|line| !line.starts_with("results"))
            .skip(1)
            .take_while(|line| line.starts_with("  "))
            .map(|line| line.trim().to_string())
            .collect()
    }

    /// The budget is for the reply the caller reads. Measured on React: two
    /// declarations named `advanceTimers` made a machine answer of 17,842
    /// characters and a delivered reply of 1,418, and under the 12,000 default
    /// the reply showed one of the two and said the other did not fit. The
    /// reader took the one row for the only declaration. A name lookup shows
    /// every declaration its delivered reply has room for, whatever its
    /// machine answer weighs.
    #[tokio::test]
    async fn a_name_lookup_shows_every_declaration_its_delivered_reply_has_room_for() {
        let graph = InMemoryGraph::default();
        for (module, file) in [
            (
                "SchedulerMock",
                "packages/scheduler/src/forks/SchedulerMock.js",
            ),
            ("Scheduler", "packages/scheduler/src/forks/Scheduler.js"),
            (
                "ReactFiberConfigART",
                "packages/react-art/src/ReactFiberConfigART.js",
            ),
            (
                "ReactFiberConfigDOM",
                "packages/react-dom-bindings/src/client/ReactFiberConfigDOM.js",
            ),
            (
                "ReactFiberConfigFabric",
                "packages/react-native-renderer/src/ReactFiberConfigFabric.js",
            ),
            (
                "ReactFiberConfigTestHost",
                "packages/react-test-renderer/src/ReactFiberConfigTestHost.js",
            ),
        ] {
            graph
                .upsert_entity(&entity(EntityKind::Module, module, file))
                .unwrap();
            let name = if module.starts_with("Scheduler") {
                "advanceTimers"
            } else {
                "commitTextUpdate"
            };
            graph
                .upsert_entity(&entity(EntityKind::Function, name, file))
                .unwrap();
        }
        // The smallest budget a call can name: the machine answer is over it
        // and the reply is not.
        for (name, declarations) in [("advanceTimers", 2), ("commitTextUpdate", 4)] {
            let (text, machine) = locate_as_delivered(
                &graph,
                json!({"mode": "name", "query": name, "max_output_bytes": 2000}),
            )
            .await;
            assert!(
                machine > 2000,
                "the fixture's machine answer must outgrow the budget: {machine}"
            );
            assert!(text.len() <= 2000, "{} bytes: {text}", text.len());
            let rows = delivered_rows(&text);
            assert_eq!(rows.len(), declarations, "every declaration shows: {text}");
            assert!(rows.iter().all(|row| row.contains(name)), "{text}");
            assert!(
                !text.contains("did not fit") && !text.contains("Showing"),
                "nothing was withheld, so nothing says so: {text}"
            );
            // And under the profile's default, the same.
            let (default, _) =
                locate_as_delivered(&graph, json!({"mode": "name", "query": name})).await;
            assert_eq!(delivered_rows(&default), rows, "{default}");
        }
    }

    /// A name lookup that truly cannot fit says how many declarations there
    /// are and names the ones it did not show, so the rows on the page are
    /// never read as the only ones; and it sheds a row's extras before it
    /// withholds a row.
    #[tokio::test]
    async fn a_name_lookup_too_large_for_its_reply_names_what_it_did_not_show() {
        let graph = InMemoryGraph::default();
        for index in 0..40 {
            let file = format!("packages/renderer_{index:02}/src/HostConfig.js");
            graph
                .upsert_entity(&entity(EntityKind::Module, "HostConfig", &file))
                .unwrap();
            graph
                .upsert_entity(&entity(EntityKind::Function, "commitTextUpdate", &file))
                .unwrap();
        }
        let (text, _) = locate_as_delivered(
            &graph,
            json!({"mode": "name", "query": "commitTextUpdate", "limit": 50,
                "details": true, "max_output_bytes": 2000}),
        )
        .await;
        assert!(text.len() <= 2000, "{} bytes: {text}", text.len());
        let rows = delivered_rows(&text);
        assert!(
            rows.len() > 1 && rows.len() < 40,
            "some rows fit and some do not: {text}"
        );
        assert!(
            text.contains(&format!("Showing {} of the 40", rows.len())),
            "{text}"
        );
        let not_shown = text
            .lines()
            .find_map(|line| line.split_once("Not shown, by module: "))
            .map(|(_, named)| named)
            .unwrap_or_else(|| panic!("the reply names what it did not show: {text}"));
        let named = crate::server::NAMED_NOT_SHOWN;
        assert_eq!(
            not_shown.matches("HostConfig").count(),
            (40 - rows.len()).min(named),
            "{text}"
        );
        assert_eq!(
            not_shown.contains(&format!(
                "and {} more.",
                (40 - rows.len()).saturating_sub(named)
            )),
            40 - rows.len() > named,
            "{text}"
        );
        // A row's signature goes before any row does.
        assert!(!text.contains("def commitTextUpdate()"), "{text}");
        assert!(
            text.contains("Detail was left out to fit this reply"),
            "{text}"
        );
        // One more row would not have fit.
        let (roomier, _) = locate_as_delivered(
            &graph,
            json!({"mode": "name", "query": "commitTextUpdate", "limit": 50,
                "details": true, "max_output_bytes": 2200}),
        )
        .await;
        assert!(delivered_rows(&roomier).len() > rows.len(), "{roomier}");
    }

    /// A description's page is sized the same way. Measured on Flask before
    /// this: a default call that asked for 20 rows was shown 11 of them, on
    /// React one that asked for 3 was shown 1, and each reply was under 1,500
    /// bytes of a 12,000 budget. A recorded machine answer, widened to 40
    /// rows, goes through the route that writes it to an agent.
    #[test]
    fn a_description_page_shows_every_row_its_delivered_reply_has_room_for() {
        use crate::server::{present_for_client, McpServerConfig};
        use crate::types::JsonRpcResponse;
        let mut machine: Value =
            serde_json::from_str(include_str!("testdata/plain_flask_locate_desc.json")).unwrap();
        let row = machine["entities"][0].clone();
        machine["entities"] = (0..40)
            .map(|index| {
                let mut row = row.clone();
                row["id"] = json!(format!("30acb47e-56ec-4536-b310-5d9c40d718{index:02}"));
                row["name"] = json!(format!("AppContext.handler_{index:02}"));
                row
            })
            .collect();
        let machine_text = serde_json::to_string(&machine).unwrap();
        assert!(
            machine_text.len() > 12_000,
            "the machine answer must outgrow the default budget: {}",
            machine_text.len()
        );
        let delivered = |arguments: Value| {
            let config = McpServerConfig {
                plain_replies: true,
                agent_belt: true,
                reply_format: crate::text_reply::ReplyFormat::Text,
                session_handles: Some(std::sync::Arc::new(
                    crate::session_handles::SessionHandles::default(),
                )),
                ..Default::default()
            };
            let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "locate", "arguments": arguments}})
            .to_string();
            let response = JsonRpcResponse::success(
                Some(json!(1)),
                serde_json::to_value(crate::ToolCallResult::text(machine_text.clone())).unwrap(),
            );
            present_for_client(response, &message, &config)
                .result
                .unwrap()["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let query = "tear down the application context";
        let text = delivered(json!({"query": query, "limit": 40}));
        assert!(text.len() <= 12_000, "{} bytes", text.len());
        assert_eq!(delivered_rows(&text).len(), 40, "every row shows: {text}");
        assert!(text.contains("More rows follow"), "{text}");

        // In a reply too small for 40 rows, the closest ones show and the
        // cursor reaches the rest.
        let small = delivered(json!({"query": query, "limit": 40, "max_output_bytes": 2000}));
        assert!(small.len() <= 2000, "{} bytes: {small}", small.len());
        let rows = delivered_rows(&small);
        assert!(rows.len() > 1 && rows.len() < 40, "{small}");
        assert_eq!(rows, delivered_rows(&text)[..rows.len()], "{small}");
        assert!(small.contains("More rows follow"), "{small}");
        assert!(small.contains("next_cursor:"), "{small}");
    }

    /// A ranking that left test code out says so in the first line an agent
    /// reads, beside the verdict, and names the call that ranks it.
    #[test]
    fn a_ranking_that_left_tests_out_says_so_in_its_answer_line() {
        use crate::server::{present_for_client, McpServerConfig};
        use crate::types::JsonRpcResponse;
        let config = McpServerConfig {
            plain_replies: true,
            agent_belt: true,
            reply_format: crate::text_reply::ReplyFormat::Text,
            session_handles: Some(std::sync::Arc::new(
                crate::session_handles::SessionHandles::default(),
            )),
            ..Default::default()
        };
        let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "locate", "arguments":
                {"query": "where the application context is pushed and popped"}}})
        .to_string();
        let response = JsonRpcResponse::success(
            Some(json!(1)),
            serde_json::to_value(crate::ToolCallResult::text(include_str!(
                "testdata/plain_flask_locate_desc.json"
            )))
            .unwrap(),
        );
        let result = present_for_client(response, &message, &config)
            .result
            .unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        let answer = text.lines().next().unwrap_or_default();
        assert!(
            answer.starts_with("Answer (complete): these are the 3 entities")
                && answer
                    .contains("best first, with test code left out (include_tests true ranks it)."),
            "{text}"
        );
        assert_eq!(text.matches("include_tests").count(), 1, "{text}");
    }

    /// A ranking the question put tests into says so in its answer line.
    #[test]
    fn a_ranking_says_when_the_question_put_tests_in_scope() {
        use crate::server::{present_for_client, McpServerConfig};
        use crate::types::JsonRpcResponse;
        let mut machine: Value =
            serde_json::from_str(include_str!("testdata/plain_flask_locate_desc.json")).unwrap();
        let delivered = |machine: &Value| {
            let config = McpServerConfig {
                plain_replies: true,
                agent_belt: true,
                reply_format: crate::text_reply::ReplyFormat::Text,
                session_handles: Some(std::sync::Arc::new(
                    crate::session_handles::SessionHandles::default(),
                )),
                ..Default::default()
            };
            let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "locate", "arguments":
                    {"query": "tests for tearing down the application context"}}})
            .to_string();
            let response = JsonRpcResponse::success(
                Some(json!(1)),
                serde_json::to_value(crate::ToolCallResult::text(machine.to_string())).unwrap(),
            );
            present_for_client(response, &message, &config)
                .result
                .unwrap()["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let sentence = "The question asks about tests, so test code is ranked.";
        assert!(!delivered(&machine).contains(sentence));
        machine["degradations"]
            .as_array_mut()
            .expect("the recorded answer carries its notes")
            .push(json!({
                "component": crate::negative::QUESTION_SCOPE_COMPONENT,
                "reason": crate::negative::TESTS_RANKED_FOR_QUESTION_REASON,
                "severity": "note",
            }));
        let text = delivered(&machine);
        let answer = text.lines().next().unwrap_or_default();
        assert!(answer.starts_with("Answer"), "{text}");
        assert!(answer.contains(sentence), "{text}");
    }

    /// A module is named by its own dotted path, the way a row's label
    /// writes it, and asked about imports it says what it imports itself:
    /// each name its import statements bind, with the module it comes from
    /// and whether the repository holds that module.
    #[tokio::test]
    async fn a_module_named_by_its_path_says_what_it_imports() {
        use crate::server::{process_message, McpServerConfig, SessionAuthorityMode};
        use crate::session::SessionRegistry;
        use kin_model::{
            ArtifactId, FileEscapeRecord, LocatedEntry, MentionForm, ModuleScope,
            OccurrenceBinding, OccurrenceContext, RepoPath, TransactionDelta, TreeDelta, TreeEntry,
            ValueOccurrence, VALUE_ESCAPE_RECORD_KEY,
        };
        let graph = InMemoryGraph::default();
        let blob = Hash256::from_bytes([0x71; 32]);
        let bound = |line: u32, module: &str, form: MentionForm| ValueOccurrence {
            line,
            context: OccurrenceContext::Mention,
            binding: OccurrenceBinding::Import,
            receiver: None,
            receiver_kind: None,
            aliases: Vec::new(),
            module: Some(module.into()),
            import: Some((0, 11)),
            imported: None,
            target_line: None,
            form: Some(form),
        };
        let mut record = FileEscapeRecord::new(ModuleScope::Open);
        record.lists_mentions = true;
        record.values.insert(
            "json".into(),
            vec![bound(30, "json", MentionForm::Receiver)],
        );
        record
            .values
            .insert("Flask".into(), vec![bound(2, "flask", MentionForm::Import)]);
        let mut module = entity(EntityKind::Module, "test_config", "tests/test_config.py");
        module
            .metadata
            .extra
            .insert("blob_hash".into(), json!(blob.to_string()));
        module
            .metadata
            .extra
            .insert(VALUE_ESCAPE_RECORD_KEY.into(), record.to_metadata());
        graph.upsert_entity(&module).unwrap();
        graph
            .upsert_entity(&entity(
                EntityKind::Module,
                "flask",
                "src/flask/__init__.py",
            ))
            .unwrap();
        graph
            .apply_transaction_delta(&TransactionDelta {
                tree_deltas: [
                    ("tests/test_config.py", blob),
                    ("src/flask/__init__.py", Hash256::from_bytes([0x72; 32])),
                    (
                        "src/flask/json/__init__.py",
                        Hash256::from_bytes([0x73; 32]),
                    ),
                ]
                .into_iter()
                .map(|(path, hash)| TreeDelta::Added {
                    artifact_id: ArtifactId::new(),
                    new: LocatedEntry::new(
                        RepoPath::from_utf8(path).unwrap(),
                        TreeEntry::blob(hash, false),
                    ),
                })
                .collect(),
                ..Default::default()
            })
            .unwrap();
        let call = |arguments: Value| {
            let graph = &graph;
            async move {
                // The reply as an agent profile's connection hands it over.
                let config = McpServerConfig {
                    session_authority_mode: SessionAuthorityMode::OfflineFallback,
                    plain_replies: true,
                    agent_belt: true,
                    ..Default::default()
                };
                let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": "refs", "arguments": arguments}})
                .to_string();
                let response =
                    process_message(&message, graph, &config, &SessionRegistry::empty_for_test())
                        .await
                        .expect("response");
                let response = crate::server::present_for_client(response, &message, &config);
                let result = response.result.expect("result");
                (
                    result["isError"].as_bool().unwrap_or(false),
                    result["content"][0]["text"].as_str().unwrap().to_string(),
                )
            }
        };
        for spelling in [
            "tests.test_config",
            "tests.test_config :: test_config",
            "tests.test_config::test_config",
        ] {
            let (failed, text) =
                call(json!({"entity_id": spelling, "relation_kinds": ["imports"]})).await;
            assert!(!failed, "{spelling}: {text}");
            let reply: Value = serde_json::from_str(&text)
                .unwrap_or_else(|error| panic!("{spelling}: {error}\n{text}"));
            assert_eq!(
                reply["imports"],
                json!([
                    {"name": "Flask", "from": "flask"},
                    {"name": "json", "from": "json", "outside_repository": true},
                ]),
                "{spelling}: {text}"
            );
            let meaning = reply["_kin"]["meaning"].as_str().unwrap();
            assert!(
                meaning.contains(
                    "What test_config itself imports is in imports: 2 names its import \
                     statements bind, each with the module it comes from; 1 of them come from \
                     a module no file of this repository supplies, marked outside_repository."
                ),
                "{meaning}"
            );
        }
        // Asked only about calls, the module's imports are not the question.
        let (_, text) =
            call(json!({"entity_id": "tests.test_config", "relation_kinds": ["calls"]})).await;
        let reply: Value = serde_json::from_str(&text).unwrap();
        assert!(reply.get("imports").is_none(), "{text}");
        // As text, each import is one line.
        let (_, json_reply) =
            call(json!({"entity_id": "tests.test_config", "relation_kinds": ["imports"]})).await;
        let mut result = json!({"content": [{"type": "text", "text": json_reply}]});
        crate::lean_reply::lean_result(&mut result);
        let handles = crate::session_handles::SessionHandles::default();
        crate::text_reply::render_result(&mut result, "find_references", &handles);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains(
                "imports (2):\n  Flask  from=flask\n  json  from=json  outside_repository=true"
            ),
            "{text}"
        );
    }
}
