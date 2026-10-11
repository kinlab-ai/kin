// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A compact reference to the write precondition a source read issues.
//!
//! `get_entity_source` serves an entity's exact body with the `source_base` a
//! guarded edit must carry back: the workspace instant it was read at, and the
//! artifact, byte span and digest of the body. That object is most of a short
//! read's bytes, and a reader who only reads never uses it. Every public MCP read is
//! served a ticket in its place, `{"ticket": "sb2.<entity>.<generation>.<digest>"}`,
//! which a reader who edits copies into `source_base` unchanged, as it would the
//! whole object.
//!
//! The ticket holds no state on the server, so it survives a restart and a
//! one-shot `kin call`. It names the entity, the workspace generation of the
//! read, and a digest of what a base binds an edit to: the repository, the
//! workspace and its selected head, the entity, its artifact, and the length
//! and digest of the body that was read. When a write carries one, the server
//! reads the entity's current base and the ticket resolves when that base
//! still binds the same things: the entity has exactly the bytes the reader
//! was served, in the same repository and workspace and on the same branch.
//! The write then carries the entity's current base, so every check the
//! write path makes on a base it makes on this one.
//!
//! A ticket therefore survives a commit that did not touch its entity. An
//! agent that reads three functions and changes them in three calls sends
//! three tickets from its first reads; each holds until its own entity's body
//! changes. A ticket for a class served whole goes stale when a method inside
//! it changes, because the class's bytes did. A class too large for that is
//! served as its own code with its members elided ([`crate::module_code`]),
//! and its ticket binds that code: it holds through an edit to any member.
//! What the reader saw around the entity, a callee's signature for one, is
//! not part of what a ticket binds.
//!
//! A ticket never carries an old body onto a new one. A body that changed
//! since the read fails to resolve and the write is refused before anything
//! is staged, with the fresh read that fixes it.
//!
//! A ticket an older build minted, `sb1`, bound the whole workspace instant
//! and is still read that way: it resolves only while nothing in the
//! workspace has moved.

use std::collections::HashMap;
use std::future::Future;

use serde_json::{json, Value};

use crate::source_base::EntitySourceBase;

/// The one key of a ticket object.
pub const TICKET_KEY: &str = "ticket";

/// The version a ticket this build mints begins with.
const PREFIX: &str = "sb2";

/// The version an older build minted. Its digest covers the whole base, so
/// it holds only while nothing in the workspace moved.
const WORKSPACE_PREFIX: &str = "sb1";

/// Hex digits of the base digest a ticket carries: 128 bits.
const DIGEST_HEX: usize = 32;

/// The write tools whose operations can carry a `source_base`.
const WRITE_TOOLS: [&str; 4] = [
    "kin_mutate",
    "kin_transaction_stage",
    "kin_transaction_commit",
    crate::handlers::sessions::DURABLE_MUTATE_TOOL,
];

/// Whether `tool` is a write tool, whose operations can carry a
/// `source_base`.
pub(crate) fn is_write_tool(tool: &str) -> bool {
    WRITE_TOOLS.contains(&tool)
}

/// A ticket read back from a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTicket {
    pub entity_id: String,
    generation: u64,
    digest: String,
    /// Whether the ticket binds the whole workspace instant, as an `sb1`
    /// ticket does, and not the entity's body alone.
    whole_workspace: bool,
}

fn digest_hex(bytes: &[u8]) -> String {
    kin_blobs::digest(bytes)
        .to_string()
        .chars()
        .take(DIGEST_HEX)
        .collect()
}

/// The digest of what a base binds an edit to: the repository, the workspace
/// and the selected head it was read on, the entity and its artifact, and
/// the length and digest of the body that was read. Where the body sits in
/// its file, the file's other bytes and the workspace's tree are not in it,
/// so a base whose entity is untouched digests the same after any other
/// commit on its branch. The selected head is a branch by name, which a
/// commit leaves as it was and a switch of branch does not.
fn digest_of(base: &EntitySourceBase) -> Option<String> {
    let bound = json!({
        "schema": base.schema,
        "repository_id": base.context.repository_id,
        "workspace_id": base.context.workspace_id,
        "workspace_head_hash": base.context.workspace_head_hash,
        "entity_id": base.entity_id,
        "artifact_id": base.artifact_id,
        "body_bytes": base.end_byte.checked_sub(base.start_byte)?,
        "body_hash": base.body_hash,
    });
    Some(digest_hex(&serde_json::to_vec(&bound).ok()?))
}

/// The digest an `sb1` ticket carries: the whole base with its workspace
/// generation set aside.
fn workspace_digest_of(base: &EntitySourceBase) -> Option<String> {
    let mut canonical = base.clone();
    canonical.context.workspace_generation = 0;
    Some(digest_hex(&serde_json::to_vec(&canonical).ok()?))
}

/// The ticket for a `source_base` a read served, or `None` when the value is
/// not a well-formed base.
pub fn mint(base: &Value) -> Option<String> {
    let base: EntitySourceBase = serde_json::from_value(base.clone()).ok()?;
    base.validate().ok()?;
    Some(format!(
        "{PREFIX}.{}.{}.{}",
        base.entity_id,
        base.context.workspace_generation,
        digest_of(&base)?
    ))
}

/// The ticket object every public MCP read serves in place of a `source_base`.
pub fn compact(base: &Value) -> Option<Value> {
    mint(base).map(|ticket| json!({ TICKET_KEY: ticket }))
}

/// The public source precondition. The private write path still parses the
/// complete base after this ticket has been resolved.
pub fn write_base_schema() -> Value {
    json!({
        "type": "object",
        "description": "Copy the source_base ticket returned by source unchanged. Kin resolves it to the exact read base before validating the write.",
        "properties": {
            "ticket": { "type": "string", "minLength": 1 }
        },
        "required": ["ticket"],
        "additionalProperties": false
    })
}

/// Whether `value` is a ticket object: one key, `ticket`, holding a string.
pub fn is_ticket(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.len() == 1 && map.get(TICKET_KEY).is_some_and(Value::is_string))
}

fn operation_ticket(operation: &Value) -> Option<&Value> {
    let payload = operation.get("payload")?.as_object()?;
    payload.iter().find_map(|(variant, value)| {
        let base = match variant.as_str() {
            "EntitySourceBase" => Some(value),
            "EntitySourcePatch" | "EntityCreate" | "EntityRemove" | "EntityContent" => {
                value.get("source_base")
            }
            _ => None,
        }?;
        base.get(TICKET_KEY).map(|_| base)
    })
}

/// Whether an operations array carries a ticket in a supported base slot.
/// Malformed ticket objects count too, so the forwarding check refuses them.
pub fn contains(operations: &Value) -> bool {
    operations
        .as_array()
        .is_some_and(|operations| operations.iter().any(|op| operation_ticket(op).is_some()))
}

/// Validate a ticket-bearing operation's public shape without inventing an
/// internal base. `false` means it has no ticket and needs normal full parsing.
/// The selected daemon still resolves and validates every payload and exact
/// source precondition before admitting an operation.
pub fn check_forward_operation(operation: &Value) -> Result<bool, String> {
    let Some(base) = operation_ticket(operation) else {
        return Ok(false);
    };
    crate::requests::check_operation(operation)?;
    if !is_ticket(base) {
        return Err(UNREADABLE.to_string());
    }
    parse(base[TICKET_KEY].as_str().unwrap_or_default())?;
    // The target is the ticket's entity by its id, by the start of its id as
    // a row prints it, or by a name, which the daemon that holds the graph
    // reads. Only a target that is some other entity is refused here.
    crate::entity_address::check_write_target(operation)?;
    if !operation.get("target").is_some_and(Value::is_string) {
        return Err(
            "A guarded entity operation names its entity as target. Nothing was written.".into(),
        );
    }
    if operation.get("destination").is_some() {
        return Err("A guarded entity operation takes no destination. Nothing was written.".into());
    }
    Ok(true)
}

/// What a caller is told when a ticket does not read back as one Kin minted.
const UNREADABLE: &str = "source_base_ticket_invalid: this source_base ticket was not copied \
     exactly. Send the source_base the source read returned, unchanged, or read the entity again \
     with get_entity_source and send the source_base it returns. Nothing was written.";

/// Read a ticket back, refusing anything that is not one this build mints.
pub fn parse(text: &str) -> Result<SourceTicket, String> {
    let text = text
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '`');
    let mut parts = text.split('.');
    let (Some(prefix), Some(entity), Some(generation), Some(digest), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(UNREADABLE.to_string());
    };
    let whole_workspace = match prefix {
        PREFIX => false,
        WORKSPACE_PREFIX => true,
        _ => return Err(UNREADABLE.to_string()),
    };
    let entity = uuid::Uuid::parse_str(entity).map_err(|_| UNREADABLE.to_string())?;
    let generation = generation.parse().map_err(|_| UNREADABLE.to_string())?;
    if digest.len() != DIGEST_HEX
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(UNREADABLE.to_string());
    }
    Ok(SourceTicket {
        entity_id: entity.to_string(),
        generation,
        digest: digest.to_string(),
        whole_workspace,
    })
}

/// The base a write carrying `ticket` is checked against, given the entity's
/// current source read, or the refusal that says why the write cannot carry
/// it.
///
/// `current` is a machine `get_entity_source` reply for the ticket's entity.
/// The ticket resolves when that reply's base binds what the ticket's digest
/// binds: the same repository, workspace, selected head, entity and artifact,
/// and a body of the same length and digest as the one the reader was served. The base
/// returned is the reply's own, the entity's current one. An `sb1` ticket
/// resolves the way it was minted to: only when the whole base, at the
/// ticket's generation, is the one the reader was served.
pub fn resolve(ticket: &SourceTicket, current: &Value) -> Result<Value, String> {
    let name = current
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(ticket.entity_id.as_str());
    let reread = format!(
        "read {name} again with get_entity_source and send the source_base it returns. Nothing \
         was written."
    );
    let Some(base) = current.get("source_base").filter(|base| base.is_object()) else {
        let why = current
            .get("source_base_unavailable")
            .and_then(Value::as_str)
            .unwrap_or("Kin cannot currently prove the bytes of its body");
        return Err(format!(
            "source_base_unavailable: this source_base ticket names {name}, which has no edit \
             base now: {why}. {reread}"
        ));
    };
    let mut base: EntitySourceBase = serde_json::from_value(base.clone()).map_err(|_| {
        format!("source_base_unavailable: {name} has no readable edit base now; {reread}")
    })?;
    if base.entity_id.to_string() != ticket.entity_id {
        return Err(format!(
            "source_base_ticket_invalid: this ticket names entity {}, and the current read \
             answered for {}; {reread}",
            ticket.entity_id, base.entity_id
        ));
    }
    let held = if ticket.whole_workspace {
        base.context.workspace_generation = ticket.generation;
        workspace_digest_of(&base)
    } else {
        digest_of(&base)
    };
    if held.as_deref() != Some(ticket.digest.as_str()) {
        let what = if ticket.whole_workspace {
            "Its body or the workspace changed since that read"
        } else {
            "Its body changed since that read"
        };
        return Err(format!(
            "source_base_changed: {name} is not the source this ticket was read from. {what}, \
             so an edit written against it could overwrite work it never saw; {reread}"
        ));
    }
    serde_json::to_value(&base)
        .map_err(|error| format!("source_base_unavailable: {error}; {reread}"))
}

/// Where one ticket sits in a write call's operations.
struct Slot {
    operation: usize,
    variant: String,
    /// `true` when the payload variant is the base itself
    /// (`EntitySourceBase`), `false` when the base is its `source_base` field.
    whole: bool,
    ticket: SourceTicket,
}

/// The base value a slot addresses, mutably.
fn slot_value<'a>(operations: &'a mut [Value], slot: &Slot) -> Option<&'a mut Value> {
    let variant = operations
        .get_mut(slot.operation)?
        .get_mut("payload")?
        .get_mut(&slot.variant)?;
    if slot.whole {
        Some(variant)
    } else {
        variant.get_mut("source_base")
    }
}

/// Replace every ticket in a write call's operations, or a draft creation's
/// `original_source_base`, with the exact base it was minted from.
///
/// `read_source` supplies a private machine source reply for the selected
/// authority. For a new write it reads the current source. An authenticated
/// replay must instead supply the retained original base after matching the
/// saved request or draft identity; reading today's source cannot recover an
/// original base that a successful write has already replaced. Resolution
/// still checks the ticket's exact digest and never refreshes its generation.
///
/// A call that is not a write, or carries no ticket, is left as it is. The
/// first ticket that does not resolve refuses the whole call, before anything
/// is staged or forwarded.
pub async fn resolve_call<F, Fut>(
    tool: &str,
    arguments: &mut HashMap<String, Value>,
    read_source: F,
) -> Result<(), String>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    if tool == "kin_draft_create" {
        let Some(candidate) = arguments.get("original_source_base") else {
            return Ok(());
        };
        if candidate.get(TICKET_KEY).is_none() {
            return Ok(());
        }
        if !is_ticket(candidate) {
            return Err(UNREADABLE.to_string());
        }
        let ticket = parse(candidate[TICKET_KEY].as_str().unwrap_or_default())?;
        let original = read_source(ticket.entity_id.clone())
            .await
            .map_err(|error| {
                format!(
                    "source_base_unavailable: Kin could not verify this draft's original \
                 source_base ticket: {error}. Nothing was saved."
                )
            })?;
        let resolved = resolve(&ticket, &original)?;
        arguments.insert("original_source_base".to_string(), resolved);
        return Ok(());
    }
    if !WRITE_TOOLS.contains(&tool) {
        return Ok(());
    }
    let Some(operations) = arguments
        .get_mut("operations")
        .and_then(Value::as_array_mut)
    else {
        return Ok(());
    };
    let mut slots = Vec::new();
    for (operation, element) in operations.iter().enumerate() {
        let Some(payload) = element.get("payload").and_then(Value::as_object) else {
            continue;
        };
        for (variant, value) in payload {
            let (whole, candidate) = if variant == "EntitySourceBase" {
                (true, Some(value))
            } else {
                (false, value.get("source_base"))
            };
            let Some(candidate) = candidate.filter(|candidate| is_ticket(candidate)) else {
                continue;
            };
            let text = candidate[TICKET_KEY].as_str().unwrap_or_default();
            slots.push(Slot {
                operation,
                variant: variant.clone(),
                whole,
                ticket: parse(text)?,
            });
        }
    }
    if slots.is_empty() {
        return Ok(());
    }
    let mut reads: HashMap<String, Value> = HashMap::new();
    for slot in &slots {
        if reads.contains_key(&slot.ticket.entity_id) {
            continue;
        }
        let current = read_source(slot.ticket.entity_id.clone())
            .await
            .map_err(|error| {
                format!(
                    "source_base_unavailable: Kin could not read entity {} to check this \
                     source_base ticket: {error}. Read it again with get_entity_source and send \
                     the source_base it returns. Nothing was written.",
                    slot.ticket.entity_id
                )
            })?;
        reads.insert(slot.ticket.entity_id.clone(), current);
    }
    let mut resolved = Vec::with_capacity(slots.len());
    for slot in &slots {
        resolved.push(resolve(&slot.ticket, &reads[&slot.ticket.entity_id])?);
    }
    for (slot, base) in slots.iter().zip(resolved) {
        if let Some(value) = slot_value(operations, slot) {
            *value = base;
        }
    }
    Ok(())
}

/// The machine reply of a `get_entity_source` call, as JSON, or why there is
/// none: an error result's own words.
pub fn source_reply(result: &crate::types::ToolCallResult) -> Result<Value, String> {
    let text = result
        .content
        .iter()
        .map(|block| match block {
            crate::types::ContentBlock::Text { text } => text.as_str(),
        })
        .next()
        .unwrap_or_default();
    if result.is_error == Some(true) {
        return Err(text.to_string());
    }
    serde_json::from_str(text).map_err(|error| error.to_string())
}

/// The read a ticket resolves against: one entity's source, by id.
pub fn source_arguments(entity_id: &str) -> HashMap<String, Value> {
    HashMap::from([("entity_id".to_string(), json!(entity_id))])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(generation: u64, body_hash: &str) -> Value {
        json!({
            "schema": "kin.entity.source_base.v1",
            "context": {
                "repository_id": "58e38c1d-03c0-4130-adda-cdd5d8f8559b",
                "workspace_id": "d5a19365-150e-4ea8-974d-d4166572e0db",
                "workspace_generation": generation,
                "workspace_head_hash": "a".repeat(64),
                "workspace_tree_hash": "b".repeat(64),
            },
            "entity_id": "600c6388-0170-48cc-bde5-2bc591e4e8aa",
            "artifact_id": "41d36491-c153-59b6-b7f8-1a41c7dda283",
            "source_blob_hash": "c".repeat(64),
            "start_byte": 1339,
            "end_byte": 1669,
            "body_hash": body_hash,
        })
    }

    fn reply(base: Value) -> Value {
        json!({"name": "leak_detector", "source_base": base})
    }

    fn ticket_of(base: &Value) -> SourceTicket {
        parse(&mint(base).expect("mints")).expect("parses")
    }

    #[test]
    fn a_ticket_is_short_and_names_its_entity() {
        let served = base(3, &"d".repeat(64));
        let ticket = mint(&served).unwrap();
        assert!(ticket.starts_with("sb2.600c6388-0170-48cc-bde5-2bc591e4e8aa.3."));
        assert!(ticket.len() < 90, "{ticket}");
        assert!(
            serde_json::to_string(&compact(&served).unwrap())
                .unwrap()
                .len()
                * 4
                < serde_json::to_string(&served).unwrap().len(),
            "a ticket is a fraction of the base it stands for"
        );
    }

    #[test]
    fn a_ticket_resolves_to_exactly_the_base_that_was_served() {
        let served = base(3, &"d".repeat(64));
        let resolved = resolve(&ticket_of(&served), &reply(served.clone())).unwrap();
        assert_eq!(resolved, served);
    }

    /// A commit that did not touch the entity moves the workspace's head and
    /// tree, the file's blob, the generation and perhaps the entity's place
    /// in its file. The ticket holds through all of it, and the write
    /// carries the entity's current base.
    #[test]
    fn a_ticket_holds_while_its_entity_s_body_is_unchanged() {
        let served = base(3, &"d".repeat(64));
        let mut moved = base(9, &"d".repeat(64));
        moved["context"]["workspace_tree_hash"] = json!("2".repeat(64));
        moved["source_blob_hash"] = json!("3".repeat(64));
        moved["start_byte"] = json!(1500);
        moved["end_byte"] = json!(1830);
        let resolved = resolve(&ticket_of(&served), &reply(moved.clone())).unwrap();
        assert_eq!(resolved, moved, "the write carries the current base");
    }

    /// What a ticket binds still refuses: another workspace or repository,
    /// another branch, another artifact, and a body of another length.
    #[test]
    fn a_ticket_does_not_hold_past_what_it_binds() {
        let served = base(3, &"d".repeat(64));
        for (pointer, value) in [
            (
                "/context/workspace_id",
                json!("0e1d2c3b-150e-4ea8-974d-d4166572e0db"),
            ),
            (
                "/context/repository_id",
                json!("0e1d2c3b-03c0-4130-adda-cdd5d8f8559b"),
            ),
            ("/context/workspace_head_hash", json!("1".repeat(64))),
            (
                "/artifact_id",
                json!("0e1d2c3b-c153-59b6-b7f8-1a41c7dda283"),
            ),
            ("/end_byte", json!(1700)),
        ] {
            let mut other = served.clone();
            *other.pointer_mut(pointer).unwrap() = value;
            let error = resolve(&ticket_of(&served), &reply(other)).unwrap_err();
            assert!(
                error.starts_with("source_base_changed:"),
                "{pointer}: {error}"
            );
        }
    }

    /// A ticket an older build minted bound the whole workspace instant, and
    /// still does.
    #[test]
    fn an_older_ticket_keeps_its_whole_workspace_meaning() {
        let served = base(3, &"d".repeat(64));
        let parsed: EntitySourceBase = serde_json::from_value(served.clone()).unwrap();
        let older = parse(&format!(
            "sb1.{}.3.{}",
            parsed.entity_id,
            workspace_digest_of(&parsed).unwrap()
        ))
        .unwrap();
        // The same instant, at a later generation, resolves to the base as
        // it was served.
        assert_eq!(
            resolve(&older, &reply(base(9, &"d".repeat(64)))).unwrap(),
            served
        );
        let mut moved = served.clone();
        moved["context"]["workspace_tree_hash"] = json!("f".repeat(64));
        let error = resolve(&older, &reply(moved)).unwrap_err();
        assert!(error.starts_with("source_base_changed:"), "{error}");
        assert!(error.contains("or the workspace changed"), "{error}");
    }

    #[test]
    fn a_changed_body_refuses_rather_than_refreshing_the_base() {
        let served = base(3, &"d".repeat(64));
        let error = resolve(&ticket_of(&served), &reply(base(3, &"e".repeat(64)))).unwrap_err();
        assert!(error.starts_with("source_base_changed:"), "{error}");
        assert!(error.contains("get_entity_source"), "{error}");
        assert!(error.contains("Nothing was written"), "{error}");
    }

    #[test]
    fn another_entity_s_base_never_resolves_a_ticket() {
        let served = base(3, &"d".repeat(64));
        let mut other = served.clone();
        other["entity_id"] = json!("4bf5cda3-6b30-4361-96cb-b9f72736d582");
        let error = resolve(&ticket_of(&served), &reply(other)).unwrap_err();
        assert!(error.starts_with("source_base_ticket_invalid:"), "{error}");
    }

    #[test]
    fn a_body_that_lost_its_base_refuses_with_the_reason() {
        let served = base(3, &"d".repeat(64));
        let current = json!({"name": "leak_detector", "source_base": null,
            "source_base_unavailable": "the span is not verified"});
        let error = resolve(&ticket_of(&served), &current).unwrap_err();
        assert!(error.contains("the span is not verified"), "{error}");
    }

    #[test]
    fn a_ticket_that_was_not_copied_exactly_is_refused() {
        let ticket = mint(&base(3, &"d".repeat(64))).unwrap();
        for broken in [
            ticket.replace("sb2", "sb3"),
            ticket[..ticket.len() - 1].to_string(),
            ticket.replacen('.', "", 1),
            format!("{ticket}.0"),
            ticket.to_uppercase(),
        ] {
            assert!(parse(&broken).is_err(), "{broken}");
        }
        assert!(
            parse(&format!("\"{ticket}\"")).is_ok(),
            "quotes a copy added are ignored"
        );
    }

    fn call(operations: Value) -> HashMap<String, Value> {
        HashMap::from([
            ("session_id".to_string(), json!("s")),
            ("operations".to_string(), operations),
        ])
    }

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn every_ticket_in_a_write_resolves_where_it_sits() {
        let served = base(3, &"d".repeat(64));
        let ticket = compact(&served).unwrap();
        let mut arguments = call(json!([
            {"verb": "patch", "target": "600c6388-0170-48cc-bde5-2bc591e4e8aa",
             "description": "yield nothing",
             "payload": {"EntitySourcePatch": {"source_base": ticket, "edits": [
                 {"old_text": "yield", "new_text": "yield None"}]}}},
            {"verb": "update", "target": "600c6388-0170-48cc-bde5-2bc591e4e8aa",
             "description": "replace the body",
             "body": "def leak_detector(): pass",
             "payload": {"EntitySourceBase": ticket}},
        ]));
        let reads = std::cell::Cell::new(0);
        block_on(resolve_call("kin_mutate", &mut arguments, |_| {
            reads.set(reads.get() + 1);
            let current = reply(served.clone());
            async move { Ok(current) }
        }))
        .unwrap();
        let operations = &arguments["operations"];
        assert_eq!(
            operations[0]["payload"]["EntitySourcePatch"]["source_base"],
            served
        );
        assert_eq!(operations[1]["payload"]["EntitySourceBase"], served);
        assert_eq!(reads.get(), 1, "each entity is read once");
        // What the write path parses is the base itself.
        crate::session::parse_staged_operations(operations).expect("the resolved call parses");
    }

    #[test]
    fn a_write_with_a_stale_ticket_is_refused_whole_and_left_unchanged() {
        let served = base(3, &"d".repeat(64));
        let ticket = compact(&served).unwrap();
        let operations = json!([{"verb": "remove", "target": "600c6388-0170-48cc-bde5-2bc591e4e8aa",
            "payload": {"EntityRemove": {"source_base": ticket}}}]);
        let mut arguments = call(operations.clone());
        let error = block_on(resolve_call(
            "kin_transaction_stage",
            &mut arguments,
            |_| {
                let current = reply(base(4, &"e".repeat(64)));
                async move { Ok(current) }
            },
        ))
        .unwrap_err();
        assert!(error.starts_with("source_base_changed:"), "{error}");
        assert_eq!(arguments["operations"], operations);
    }

    #[test]
    fn a_full_base_and_a_read_are_left_alone() {
        let served = base(3, &"d".repeat(64));
        let operations = json!([{"verb": "remove", "target": "600c6388-0170-48cc-bde5-2bc591e4e8aa",
            "payload": {"EntityRemove": {"source_base": served}}}]);
        let mut arguments = call(operations.clone());
        block_on(resolve_call("kin_mutate", &mut arguments, |_| async {
            Err::<Value, String>("never read".into())
        }))
        .unwrap();
        assert_eq!(arguments["operations"], operations);
        let mut read = call(
            json!([{"payload": {"EntityRemove": {"source_base": compact(&served).unwrap()}}}]),
        );
        block_on(resolve_call("get_entity_source", &mut read, |_| async {
            Err::<Value, String>("never read".into())
        }))
        .unwrap();
    }

    fn draft_call(original_source_base: Value) -> HashMap<String, Value> {
        HashMap::from([
            (
                "draft_id".into(),
                json!("cf8a87f2-6791-4499-bc05-4d22c072438e"),
            ),
            ("original_source_base".into(), original_source_base),
            (
                "original_body".into(),
                json!("def load(): return 'data/input.json'\n"),
            ),
            ("body".into(), json!("")),
        ])
    }

    #[test]
    fn a_draft_ticket_expands_only_its_original_base() {
        let served = base(3, &"d".repeat(64));
        let mut arguments = draft_call(compact(&served).unwrap());
        let expected = draft_call(served.clone());
        let reads = std::cell::Cell::new(0);
        block_on(resolve_call("kin_draft_create", &mut arguments, |entity| {
            assert_eq!(entity, served["entity_id"].as_str().unwrap());
            reads.set(reads.get() + 1);
            let original = reply(served.clone());
            async move { Ok(original) }
        }))
        .unwrap();
        assert_eq!(arguments, expected);
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn a_draft_full_base_needs_no_ticket_read() {
        let mut arguments = draft_call(base(3, &"d".repeat(64)));
        let expected = arguments.clone();
        block_on(resolve_call(
            "kin_draft_create",
            &mut arguments,
            |_| async {
                Err::<Value, String>("a full base must not invoke the ticket reader".into())
            },
        ))
        .unwrap();
        assert_eq!(arguments, expected);
    }

    #[test]
    fn an_unreadable_draft_ticket_is_refused_without_read_or_mutation() {
        for malformed in [
            json!({"ticket": 7}),
            json!({"ticket": "broken"}),
            json!({"ticket": mint(&base(3, &"d".repeat(64))).unwrap(), "extra": true}),
        ] {
            let mut arguments = draft_call(malformed);
            let expected = arguments.clone();
            let reads = std::cell::Cell::new(0);
            let error = block_on(resolve_call(
                "kin_draft_create",
                &mut arguments,
                |_| async {
                    reads.set(reads.get() + 1);
                    Err::<Value, String>("malformed tickets must be rejected before reading".into())
                },
            ))
            .unwrap_err();
            assert!(error.starts_with("source_base_ticket_invalid:"));
            assert_eq!(reads.get(), 0);
            assert_eq!(arguments, expected);
        }
    }

    #[test]
    fn a_draft_changed_base_refuses_without_refresh_or_mutation() {
        let served = base(3, &"d".repeat(64));
        let mut arguments = draft_call(compact(&served).unwrap());
        let expected = arguments.clone();
        let error = block_on(resolve_call(
            "kin_draft_create",
            &mut arguments,
            |_| async { Ok(reply(base(4, &"e".repeat(64)))) },
        ))
        .unwrap_err();
        assert!(error.starts_with("source_base_changed:"));
        assert_eq!(arguments, expected);
    }

    #[test]
    fn a_draft_replay_can_resolve_the_retained_original_without_reading_current_source() {
        let served = base(3, &"d".repeat(64));
        let mut arguments = draft_call(compact(&served).unwrap());
        // The authenticated draft dispatcher supplies the stored original.
        // Current source may have advanced; it is not this ticket's authority.
        let retained = reply(served.clone());
        block_on(resolve_call("kin_draft_create", &mut arguments, |_| {
            let retained = retained.clone();
            async move { Ok(retained) }
        }))
        .unwrap();
        assert_eq!(arguments, draft_call(served));
    }

    #[test]
    fn a_failed_draft_base_read_leaves_all_arguments_unchanged() {
        let mut arguments = draft_call(compact(&base(3, &"d".repeat(64))).unwrap());
        let expected = arguments.clone();
        let error = block_on(resolve_call(
            "kin_draft_create",
            &mut arguments,
            |_| async { Err("retained authority unavailable".into()) },
        ))
        .unwrap_err();
        assert!(error.contains("Nothing was saved"));
        assert_eq!(arguments, expected);
    }

    #[test]
    fn draft_creation_advertises_the_same_ticket_contract_as_guarded_writes() {
        let tool = crate::entity_drafts::tool_definitions()
            .into_iter()
            .find(|tool| tool.name == "kin_draft_create")
            .unwrap();
        assert_eq!(
            tool.input_schema["properties"]["original_source_base"],
            write_base_schema()
        );
        let schema = write_base_schema();
        assert_eq!(schema["required"], json!(["ticket"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"].as_object().unwrap().len(), 1);
        for physical in ["artifact_id", "start_byte", "end_byte", "source_blob_hash"] {
            assert!(!schema.to_string().contains(physical));
        }
    }

    #[test]
    fn forwarding_checks_ticket_forms_without_synthesizing_an_internal_base() {
        let served = base(3, &"d".repeat(64));
        let ticket = compact(&served).unwrap();
        for (verb, variant, payload) in [
            ("update", "EntitySourceBase", ticket.clone()),
            (
                "patch",
                "EntitySourcePatch",
                json!({"source_base":ticket,"edits":[{"old_text":"a","new_text":"b"}]}),
            ),
            ("remove", "EntityRemove", json!({"source_base":ticket})),
            (
                "create",
                "EntityCreate",
                json!({"source_base":ticket,"placement":"sibling_after","name":"fresh","kind":"function","body":"def fresh(): pass"}),
            ),
        ] {
            let mut operation = json!({"verb":verb,"target":served["entity_id"],"description":"change one entity","payload":{variant:payload}});
            if variant == "EntitySourceBase" {
                operation["body"] = json!("def replaced(): pass");
            }
            let before = operation.clone();
            assert!(contains(&json!([operation])));
            assert!(check_forward_operation(&operation).unwrap());
            assert_eq!(operation, before);
            assert!(
                crate::session::parse_staged_operations(&json!([operation])).is_err(),
                "ticket shape checking must not create a parseable private base"
            );
        }
    }

    #[test]
    fn forwarding_refuses_malformed_ticket_operation_shapes() {
        let served = base(3, &"d".repeat(64));
        let valid = json!({"verb":"update","target":served["entity_id"],"description":"replace","body":"def replaced(): pass","payload":{"EntitySourceBase":compact(&served).unwrap()}});
        let mut cases = Vec::new();
        for (key, value) in [
            ("verb", json!("remove")),
            // Another entity, by its id and by the start of one.
            ("target", json!("00000000-0000-4000-8000-00000000beef")),
            ("target", json!("replaced #0badc0de")),
            ("target", Value::Null),
            ("body", Value::Null),
            ("destination", Value::Null),
            ("content", json!("wrong field")),
        ] {
            let mut wrong = valid.clone();
            wrong[key] = value;
            cases.push(wrong);
        }
        let mut malformed = valid.clone();
        malformed["payload"]["EntitySourceBase"]["extra"] = json!(true);
        cases.push(malformed);
        for operation in cases {
            assert!(contains(&json!([operation])));
            assert!(check_forward_operation(&operation).is_err(), "{operation}");
        }
        // The ticket's entity is named by the start of its id, the way a row
        // prints it, or by a name, which the daemon that holds the graph
        // reads against the ticket: neither is refused here.
        let id = served["entity_id"].as_str().unwrap();
        for target in [
            format!("#{}", &id[..8]),
            format!("pkg.app :: replaced #{}", &id[..8]),
            "pkg.app :: replaced".to_string(),
            "replaced".to_string(),
        ] {
            let mut named = valid.clone();
            named["target"] = json!(target);
            assert_eq!(check_forward_operation(&named), Ok(true), "{target}");
        }
        assert!(!contains(
            &json!([{"payload":{"Relation":{"ticket":"program data"}}}])
        ));
    }
}
