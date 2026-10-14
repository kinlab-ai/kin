// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A long entity body served as bounded, lossless excerpts.
//!
//! An agent loop sends every tool result again in every later prompt, so one
//! source read is paid for on every turn after it. One plain read of a 288 KB
//! generated function cost a scored run about 625,000 prompt tokens. A source
//! reply therefore serves at most [`SOURCE_PAGE_BYTES`] of a body by default:
//! the first excerpt, the exact size of the whole body, the digest that
//! identifies it, and a cursor that serves the next excerpt of that same body.
//! Excerpts end at a line end when one falls in the second half of the budget,
//! and at a character boundary otherwise, so a body that is one long line
//! still pages. Joined in order, the excerpts are the body byte for byte.
//!
//! A cursor holds no state on the server: `src1.<entity>.<offset>.<digest>`.
//! A continuation reads the entity's exact body again, the way every source
//! read does, and serves the next excerpt only when that body still has the
//! digest the first excerpt was cut from. A body that changed refuses the
//! cursor by name rather than joining parts of two bodies. Nothing is cached,
//! so a cursor survives a restart and a one-shot `kin call`.
//!
//! `full_body: true` returns the whole body with its base, up to
//! [`FULL_BODY_MAX_BYTES`]. An excerpt of a body that size or smaller is
//! never an edit base: its reply carries no `source_base`, so no guarded
//! change can be written against bytes the reader never saw. A longer body is
//! never served whole, `full_body` included, so it pages like any long body
//! and each excerpt keeps its base for a patch: every anchor of a patch must
//! stand once in the whole body, so one copied from an excerpt changes only
//! what its reader saw. The write path refuses to replace such a body whole.
//! This is presentation only. It runs where a `tools/call` answer is presented to its
//! client, after the handler, so every internal source read keeps the exact
//! whole body: the read a source ticket resolves against, the bases a
//! transaction stages, and the bodies a context pack budgets.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::types::{ContentBlock, ToolCallResult};

/// The most body bytes one default source reply serves.
///
/// 12 KiB is about 3,000 tokens and about 300 lines of ordinary code, so a
/// reply stays small even on the turn it is re-sent the thirtieth time. It
/// leaves nearly every body whole: across the 47,949 Rust functions in this
/// repository's crates, the 99th percentile is 7.4 KB and 0.44 percent are
/// longer than 12 KiB. It sits between the 4 KB a class is served whole up to
/// and the [`FULL_BODY_MAX_BYTES`] `full_body` serves any body whole up to.
pub const SOURCE_PAGE_BYTES: usize = 12 * 1024;

/// The longest body `full_body` serves whole, for every entity alike: a
/// function, a class or object literal, a module's own code or a text Kin
/// holds without parsing it. Nothing finer than a leaf's body exists to read
/// it by, so without a bound `full_body` on a 288 KB generated function is a
/// whole read by another name. A longer body is served in excerpts whether or
/// not `full_body` is asked, and each carries the base a patch needs.
pub const FULL_BODY_MAX_BYTES: usize = 16 * 1024;

/// The field an excerpted reply describes its excerpt in.
pub const EXCERPT_KEY: &str = "body_excerpt";

/// The argument a continuation sends its cursor in.
pub const CURSOR_ARGUMENT: &str = "cursor";

/// Why an excerpt carries no `source_base`.
pub const SOURCE_BASE_UNAVAILABLE_EXCERPT: &str = "source_excerpt: this reply serves part of \
the body, so it carries no source_base and cannot be the base of an edit. Read the entity with \
full_body: true for the whole body and its source_base.";

/// The last sentence of every excerpt note, which says what an edit needs.
const EDIT_SENTENCE: &str = "An excerpt is not an edit base: read the entity with full_body: \
true before changing it.";

/// The last sentence of the note on an excerpt that keeps its base: one of a
/// body longer than [`FULL_BODY_MAX_BYTES`], or of an artifact's text
/// ([`crate::artifact_entity`]). A patch names literal anchors, each of which
/// must stand once in the whole body, so one written against an excerpt
/// changes only what its reader saw, and a long function or changelog changes
/// without being read whole. A whole replacement is refused for a body this
/// long, by the write path.
const PATCH_EDIT_SENTENCE: &str = "This excerpt's source_base carries a patch whose old_text \
is copied from these lines, and never a whole replacement.";

/// The version every cursor this build mints begins with.
const PREFIX: &str = "src1";

/// Hex digits of the body digest a cursor carries: 128 bits.
const DIGEST_HEX: usize = 32;

const CURSOR_INVALID: &str = "source_cursor_invalid: this cursor was not copied exactly. Send the \
next_cursor a source excerpt returned, unchanged, with the entity_id it was returned with.";

const CURSOR_WITH_FULL_BODY: &str = "cursor and full_body cannot be combined: a cursor serves the \
next excerpt, and full_body serves the whole body at once. Send one of them.";

/// Whether `tool` answers with one entity's source body.
pub fn pages_this_tool(tool: &str) -> bool {
    matches!(
        tool,
        "get_entity_source" | "get_entity_body" | crate::verbs::SOURCE
    )
}

/// A cursor read back from a continuation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceCursor {
    entity_id: String,
    offset: usize,
    digest: String,
}

fn mint(entity_id: &str, offset: usize, digest: &str) -> String {
    format!("{PREFIX}.{entity_id}.{offset}.{}", &digest[..DIGEST_HEX])
}

fn parse(text: &str) -> Result<SourceCursor, String> {
    let mut parts = text.trim().split('.');
    let (Some(PREFIX), Some(entity), Some(offset), Some(digest), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(CURSOR_INVALID.to_string());
    };
    let entity = uuid::Uuid::parse_str(entity).map_err(|_| CURSOR_INVALID.to_string())?;
    let offset: usize = offset.parse().map_err(|_| CURSOR_INVALID.to_string())?;
    if offset == 0
        || digest.len() != DIGEST_HEX
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(CURSOR_INVALID.to_string());
    }
    Ok(SourceCursor {
        entity_id: entity.to_string(),
        offset,
        digest: digest.to_string(),
    })
}

/// Serve one source reply as an excerpt when its body is over the budget, or
/// the excerpt a continuation's cursor names. Anything else is left exactly as
/// the handler answered: an error, a body within the budget, a body asked for
/// with `full_body`, an outline or an inventory.
pub fn present(tool: &str, arguments: &HashMap<String, Value>, result: &mut ToolCallResult) {
    present_within(tool, arguments, result, SOURCE_PAGE_BYTES);
}

pub(crate) fn present_within(
    tool: &str,
    arguments: &HashMap<String, Value>,
    result: &mut ToolCallResult,
    budget: usize,
) {
    if !pages_this_tool(tool) {
        return;
    }
    let budget = budget.max(4);
    let cursor = match arguments.get(CURSOR_ARGUMENT) {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => match parse(text) {
            Ok(cursor) => Some(cursor),
            Err(refusal) => {
                *result = ToolCallResult::error(refusal);
                return;
            }
        },
        Some(_) => {
            *result = ToolCallResult::error(CURSOR_INVALID);
            return;
        }
    };
    let full_body = arguments.get("full_body").and_then(Value::as_bool) == Some(true);
    if cursor.is_some() && full_body {
        *result = ToolCallResult::error(CURSOR_WITH_FULL_BODY);
        return;
    }
    // The handler's own refusal, such as an entity that no longer exists,
    // already says why nothing was served.
    if result.is_error == Some(true) {
        return;
    }
    for block in &mut result.content {
        let ContentBlock::Text { text } = block;
        let Ok(Value::Object(mut reply)) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        // A text held with parts is served whole when it is short and as its
        // outline when it is not, and is never paged as a whole.
        match crate::artifact_entity::parts::present(&mut reply, full_body, cursor.is_some()) {
            Err(refusal) => {
                *result = ToolCallResult::error(refusal);
                return;
            }
            Ok(true) => {
                let presented = Value::Object(reply);
                *text = crate::budget::render_in_format_of(&presented, text)
                    .unwrap_or_else(|_| presented.to_string());
                return;
            }
            Ok(false) => {}
        }
        // An entity held as bytes is described and has no body: its bytes
        // asked for whole are refused, and the description is never paged.
        if crate::artifact_entity::is_bytes_reply(&reply) && full_body {
            *result = ToolCallResult::error(crate::artifact_entity::inline_bytes_refusal(
                &subject(&reply),
            ));
            return;
        }
        let Some(body) = reply.get("body").and_then(Value::as_str).map(str::to_owned) else {
            if cursor.is_some() {
                *result = ToolCallResult::error(no_body_refusal(&reply));
            }
            return;
        };
        let artifact = crate::artifact_entity::is_artifact_reply(&reply);
        // A body is served whole up to a bound, and `full_body` asked of a
        // longer one is served its first excerpt, as a long body always is.
        let whole_asked = full_body && body.len() <= FULL_BODY_MAX_BYTES;
        let start = match &cursor {
            None if whole_asked || body.len() <= budget => return,
            None => 0,
            Some(cursor) => match continues(cursor, &reply, &body) {
                Ok(()) => cursor.offset,
                Err(refusal) => {
                    *result = ToolCallResult::error(refusal);
                    return;
                }
            },
        };
        let Some(entity_id) = reply_entity_id(&reply) else {
            // Without an id no cursor can name the rest, and a reply that
            // cannot be continued is served whole rather than cut.
            return;
        };
        excerpt(&mut reply, &entity_id, &body, start, budget, artifact);
        let excerpted = Value::Object(reply);
        *text = crate::budget::render_in_format_of(&excerpted, text)
            .unwrap_or_else(|_| excerpted.to_string());
        return;
    }
    if cursor.is_some() {
        *result = ToolCallResult::error(no_body_refusal(&Map::new()));
    }
}

fn reply_entity_id(reply: &Map<String, Value>) -> Option<String> {
    reply
        .get("id")
        .or_else(|| reply.get("entity_id"))
        .and_then(Value::as_str)
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
        .map(|id| id.to_string())
}

fn subject(reply: &Map<String, Value>) -> String {
    reply
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("this entity")
        .to_string()
}

fn reread(reply: &Map<String, Value>) -> String {
    match reply_entity_id(reply) {
        Some(id) => format!(
            "Read it again with get_entity_source {{\"entity_id\":\"{id}\"}} and start from its \
             first excerpt."
        ),
        None => {
            "Read it again with get_entity_source and its entity_id, without a cursor.".to_string()
        }
    }
}

fn no_body_refusal(reply: &Map<String, Value>) -> String {
    format!(
        "source_cursor_stale: {} is no longer served as a body, so this cursor continues nothing. \
         {}",
        subject(reply),
        reread(reply)
    )
}

/// Whether `cursor` continues the body this reply read: the same entity, the
/// same exact bytes, and an offset inside them.
fn continues(cursor: &SourceCursor, reply: &Map<String, Value>, body: &str) -> Result<(), String> {
    let id = reply_entity_id(reply);
    if id.as_deref() != Some(cursor.entity_id.as_str()) {
        return Err(format!(
            "source_cursor_invalid: this cursor continues entity {}, and this call read {}. Send \
             the cursor with the entity_id it was returned with.",
            cursor.entity_id,
            id.as_deref().unwrap_or("an entity with no id")
        ));
    }
    if !kin_blobs::digest(body.as_bytes())
        .to_string()
        .starts_with(&cursor.digest)
    {
        return Err(format!(
            "source_cursor_stale: the body of {} changed after this excerpt was read, so the rest \
             of it cannot be joined to the excerpts already served. {}",
            subject(reply),
            reread(reply)
        ));
    }
    if cursor.offset >= body.len() || !body.is_char_boundary(cursor.offset) {
        return Err(CURSOR_INVALID.to_string());
    }
    Ok(())
}

/// Where the excerpt that starts at `start` ends: at the last line end in the
/// budget when that keeps at least half of it, otherwise at the last character
/// boundary in it. Always past `start`, since a character is at most four bytes.
fn excerpt_end(body: &str, start: usize, budget: usize) -> usize {
    if body.len() - start <= budget {
        return body.len();
    }
    let mut limit = start + budget;
    while !body.is_char_boundary(limit) {
        limit -= 1;
    }
    match body[start..limit].rfind('\n') {
        Some(at) if at + 1 >= budget / 2 => start + at + 1,
        _ => limit,
    }
}

/// Line ends in `bytes`. Counted on bytes, since an excerpt can end inside
/// the last line it serves and the byte before its end need not start a
/// character.
fn newlines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| **byte == b'\n').count()
}

/// Lines the way an entity's lines are numbered: a final newline starts none.
fn line_count(body: &str) -> usize {
    newlines(body.as_bytes()) + usize::from(!body.is_empty() && !body.ends_with('\n'))
}

fn excerpt(
    reply: &mut Map<String, Value>,
    entity_id: &str,
    body: &str,
    start: usize,
    budget: usize,
    artifact: bool,
) {
    let end = excerpt_end(body, start, budget);
    let digest = kin_blobs::digest(body.as_bytes()).to_string();
    let first = newlines(&body.as_bytes()[..start]);
    let last = newlines(&body.as_bytes()[..end - 1]);
    let lines = line_count(body);
    let starts_mid_line = start > 0 && body.as_bytes()[start - 1] != b'\n';
    let ends_mid_line = end < body.len() && body.as_bytes()[end - 1] != b'\n';
    let remaining_bytes = body.len() - end;
    let remaining_lines = if remaining_bytes == 0 {
        0
    } else {
        lines - last - usize::from(!ends_mid_line)
    };
    let mut fields = json!({
        "first_line_in_entity": first,
        "last_line_in_entity": last,
        "byte_offset": start,
        "bytes": end - start,
        "body_bytes": body.len(),
        "body_lines": lines,
        "body_digest": digest,
        "remaining_bytes": remaining_bytes,
        "remaining_lines": remaining_lines,
    });
    if starts_mid_line {
        fields["starts_mid_line"] = json!(true);
    }
    if ends_mid_line {
        fields["ends_mid_line"] = json!(true);
    }
    let cursor = (remaining_bytes > 0).then(|| mint(entity_id, end, &digest));
    // The read's revision stays visible once its base is withheld.
    if let Some(context) = reply
        .get("source_base")
        .and_then(|base| base.get("context"))
        .cloned()
    {
        for key in ["workspace_generation", "workspace_tree_hash"] {
            if let Some(value) = context.get(key) {
                reply.entry(key).or_insert_with(|| value.clone());
            }
        }
    }
    // An excerpt keeps its base, for a patch, where no read serves the body
    // whole, and an artifact's text always keeps it.
    let patchable = keeps_base(artifact, body.len());
    if !patchable {
        reply.remove("source_base");
        reply
            .entry("source_base_unavailable")
            .or_insert_with(|| json!(SOURCE_BASE_UNAVAILABLE_EXCERPT));
    }
    let note = machine_note(&fields, entity_id, cursor.as_deref(), patchable);
    reply.insert("body".into(), json!(&body[start..end]));
    reply.insert("body_complete".into(), json!(false));
    reply.insert(EXCERPT_KEY.into(), fields);
    match cursor {
        Some(cursor) => reply.insert("next_cursor".into(), json!(cursor)),
        None => reply.remove("next_cursor"),
    };
    let note = match reply.get("note").and_then(Value::as_str) {
        Some(earlier) => format!("{earlier} {note}"),
        None => note,
    };
    reply.insert("note".into(), json!(note));
}

/// Whether an excerpt of a body `bytes` long keeps its base: when no read
/// serves that body whole, and always for an artifact's text, which changes
/// by patch at any length.
fn keeps_base(artifact: bool, bytes: usize) -> bool {
    artifact || bytes > FULL_BODY_MAX_BYTES
}

fn count(fields: &Value, key: &str) -> u64 {
    fields.get(key).and_then(Value::as_u64).unwrap_or_default()
}

/// What the excerpt is, in one clause: its lines and bytes out of the whole.
fn served_clause(fields: &Value) -> String {
    format!(
        "lines +{} to +{} of {} ({} of {} bytes)",
        count(fields, "first_line_in_entity"),
        count(fields, "last_line_in_entity"),
        count(fields, "body_lines"),
        count(fields, "bytes"),
        count(fields, "body_bytes"),
    )
}

/// How much is left and the call that serves it, spelled with `tool`.
/// `full_body` is offered for a body it serves whole, one of at most
/// [`FULL_BODY_MAX_BYTES`].
fn remaining_sentence(fields: &Value, entity_id: &str, cursor: Option<&str>, tool: &str) -> String {
    let whole_offered = count(fields, "body_bytes") as usize <= FULL_BODY_MAX_BYTES;
    match cursor {
        Some(cursor) if whole_offered => format!(
            "{} bytes ({} lines) remain. Continue with {tool} {{\"entity_id\":\"{entity_id}\",\
             \"cursor\":\"{cursor}\"}}, or pass full_body: true for the whole body in one reply.",
            count(fields, "remaining_bytes"),
            count(fields, "remaining_lines"),
        ),
        Some(cursor) => format!(
            "{} bytes ({} lines) remain. Continue with {tool} {{\"entity_id\":\"{entity_id}\",\
             \"cursor\":\"{cursor}\"}}; a body over {} KB is never served whole.",
            count(fields, "remaining_bytes"),
            count(fields, "remaining_lines"),
            FULL_BODY_MAX_BYTES / 1024,
        ),
        None => "Nothing remains: the excerpts joined in order are the whole body.".to_string(),
    }
}

/// What an excerpt's note and clause end with: what an edit needs.
fn edit_sentence(patchable: bool) -> &'static str {
    if patchable {
        PATCH_EDIT_SENTENCE
    } else {
        EDIT_SENTENCE
    }
}

fn machine_note(fields: &Value, entity_id: &str, cursor: Option<&str>, patchable: bool) -> String {
    format!(
        "This is an excerpt, not the whole body: {}. {} {}",
        served_clause(fields),
        remaining_sentence(fields, entity_id, cursor, "get_entity_source"),
        edit_sentence(patchable),
    )
}

/// Whether `note` is the note an excerpt was served with.
pub fn is_excerpt_note(note: &str) -> bool {
    note.ends_with(EDIT_SENTENCE) || note.ends_with(PATCH_EDIT_SENTENCE)
}

/// The plain reply's account of an excerpt, after the lead that names the
/// entity: what it serves, what remains and the one `source` call that
/// continues it. `None` for a reply that is not an excerpt.
pub fn plain_clause(reply: &Map<String, Value>) -> Option<String> {
    let fields = reply.get(EXCERPT_KEY)?;
    let entity_id = reply_entity_id(reply)?;
    let cursor = reply.get("next_cursor").and_then(Value::as_str);
    let artifact = crate::artifact_entity::is_artifact_reply(reply);
    let patchable = keeps_base(artifact, count(fields, "body_bytes") as usize);
    Some(format!(
        "{}, not the whole body. {} {}",
        served_clause(fields),
        remaining_sentence(fields, &entity_id, cursor, crate::verbs::SOURCE),
        edit_sentence(patchable),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "600c6388-0170-48cc-bde5-2bc591e4e8aa";

    fn base(body: &str) -> Value {
        json!({
            "schema": "kin.entity.source_base.v1",
            "context": {
                "repository_id": "58e38c1d-03c0-4130-adda-cdd5d8f8559b",
                "workspace_id": "d5a19365-150e-4ea8-974d-d4166572e0db",
                "workspace_generation": 7,
                "workspace_head_hash": "a".repeat(64),
                "workspace_tree_hash": "b".repeat(64),
            },
            "entity_id": ID,
            "artifact_id": "41d36491-c153-59b6-b7f8-1a41c7dda283",
            "source_blob_hash": "c".repeat(64),
            "start_byte": 100,
            "end_byte": 100 + body.len(),
            "body_hash": kin_blobs::digest(body.as_bytes()).to_string(),
        })
    }

    /// A source reply the way the handlers answer one, pretty-printed.
    fn read(body: &str) -> ToolCallResult {
        let reply = json!({
            "id": ID, "name": "renderTree", "kind": "Function", "language": "TypeScript",
            "signature": "function renderTree(node)", "start_line": 10,
            "end_line": 10 + line_count(body), "span_coherence": "digest_verified",
            "source": "graph", "source_base": base(body), "body": body,
        });
        ToolCallResult::text(serde_json::to_string_pretty(&reply).unwrap())
    }

    fn text(result: &ToolCallResult) -> &str {
        let ContentBlock::Text { text } = &result.content[0];
        text
    }

    fn json_of(result: &ToolCallResult) -> Value {
        serde_json::from_str(text(result)).unwrap()
    }

    fn args(pairs: Value) -> HashMap<String, Value> {
        serde_json::from_value(pairs).unwrap()
    }

    fn paged(body: &str, arguments: &HashMap<String, Value>, budget: usize) -> ToolCallResult {
        let mut result = read(body);
        present_within("get_entity_source", arguments, &mut result, budget);
        result
    }

    /// A function of about 280 KB: a long switch over generated cases with a
    /// non-ASCII string on every line, the shape a compiled bundle hands Kin.
    fn huge_function() -> String {
        let mut body = String::from("function renderTree(node) {\n  switch (node.tag) {\n");
        let mut case = 0;
        while body.len() < 280_000 {
            body.push_str(&format!(
                "    case {case}: return mount(node, \"élément-{case}\", {{ depth: {} }});\n",
                case % 17
            ));
            case += 1;
        }
        body.push_str("  }\n}");
        body
    }

    /// A long function holding nested function definitions, each its own
    /// entity in the graph and none of them a reason to outline the outer one.
    fn nested_function() -> String {
        let mut body = String::from("function outer(input) {\n");
        for index in 0..400 {
            body.push_str(&format!(
                "  function helper{index}(value) {{\n    const scaled = value * {index};\n    return scaled + input;\n  }}\n"
            ));
        }
        body.push_str("  return helper0(1);\n}\n");
        body
    }

    /// The source reply of an artifact Kin holds as text, the way
    /// [`crate::artifact_entity`] answers one.
    fn artifact_read(body: &str) -> ToolCallResult {
        let mut expected = base(body);
        expected["start_byte"] = json!(0);
        expected["end_byte"] = json!(body.len());
        let reply = json!({
            "id": ID, "name": "CHANGES.rst", "module": "flask", "kind": "reStructuredText",
            "held_as": "text", "projected_at": "CHANGES.rst", "source": "graph",
            "source_base": expected, "body": body,
            crate::artifact_entity::REPLY_KEY: {"level": "text", "bytes": body.len()},
        });
        ToolCallResult::text(serde_json::to_string_pretty(&reply).unwrap())
    }

    /// An excerpt of an artifact's text keeps the base its read carried, so
    /// a patch can be written against the lines a reader saw, and says so in
    /// place of the sentence that sends a reader of code to the whole body.
    #[test]
    fn an_excerpt_of_an_artifacts_text_keeps_its_base_for_a_patch() {
        let body: String = (0..2_000)
            .map(|entry| format!("- Entry {entry:04}: one line of a changelog.\n"))
            .collect();
        assert!(body.len() > FULL_BODY_MAX_BYTES);
        let mut first = artifact_read(&body);
        present("get_entity_source", &HashMap::new(), &mut first);
        assert_ne!(first.is_error, Some(true), "{}", text(&first));
        let page = json_of(&first);
        assert!(page["body"].as_str().unwrap().len() <= SOURCE_PAGE_BYTES);
        assert_eq!(page["source_base"]["end_byte"], body.len(), "{page}");
        assert!(page.get("source_base_unavailable").is_none(), "{page}");
        let note = page["note"].as_str().unwrap();
        assert!(is_excerpt_note(note), "{note}");
        assert!(note.ends_with(PATCH_EDIT_SENTENCE), "{note}");
        // The whole text is longer than a reply carries, so it is not offered.
        assert!(!note.contains("full_body"), "{note}");
        let clause = plain_clause(page.as_object().unwrap()).unwrap();
        assert!(clause.ends_with(PATCH_EDIT_SENTENCE), "{clause}");
        assert!(!clause.contains("full_body"), "{clause}");

        // Its excerpts join to the text, each with the base.
        let mut joined = page["body"].as_str().unwrap().to_string();
        let mut cursor = page["next_cursor"].as_str().map(str::to_string);
        while let Some(next) = cursor {
            let mut result = artifact_read(&body);
            present(
                "get_entity_source",
                &args(json!({"entity_id": ID, "cursor": next})),
                &mut result,
            );
            assert_ne!(result.is_error, Some(true), "{}", text(&result));
            let page = json_of(&result);
            assert!(page["source_base"].is_object(), "{page}");
            joined.push_str(page["body"].as_str().unwrap());
            cursor = page["next_cursor"].as_str().map(str::to_string);
        }
        assert_eq!(joined, body);
    }

    /// `full_body` serves a body whole up to its bound, code and text alike.
    /// Past it the body pages like any long body, `full_body` or not: the
    /// same first excerpt, which keeps its base for a patch and never offers
    /// the whole. Within it, an excerpt of code withholds its base and offers
    /// the whole, which carries the base.
    #[test]
    fn full_body_serves_a_body_whole_only_up_to_its_bound() {
        let whole = args(json!({"entity_id": ID, "full_body": true}));
        let long: String = "x\n".repeat(FULL_BODY_MAX_BYTES / 2 + 1);
        let fits: String = long[..FULL_BODY_MAX_BYTES].to_string();
        assert!(fits.len() > SOURCE_PAGE_BYTES);
        let readers: [(&str, fn(&str) -> ToolCallResult); 2] =
            [("code", read), ("text", artifact_read)];
        for (held, reader) in readers {
            let mut asked = reader(&long);
            present("get_entity_source", &whole, &mut asked);
            assert_ne!(asked.is_error, Some(true), "{held}: {}", text(&asked));
            let mut default = reader(&long);
            present("get_entity_source", &HashMap::new(), &mut default);
            assert_eq!(
                text(&asked),
                text(&default),
                "{held}: pages like any long body"
            );
            let page = json_of(&asked);
            assert_eq!(page["body_complete"], false, "{held}");
            assert!(page["body"].as_str().unwrap().len() <= SOURCE_PAGE_BYTES);
            assert_eq!(
                page["source_base"]["end_byte"],
                json_of(&reader(&long))["source_base"]["end_byte"]
            );
            assert!(
                page.get("source_base_unavailable").is_none(),
                "{held}: {page}"
            );
            let note = page["note"].as_str().unwrap();
            assert!(note.ends_with(PATCH_EDIT_SENTENCE), "{held}: {note}");
            assert!(!note.contains("full_body"), "{held}: {note}");
            assert!(
                note.contains("a body over 16 KB is never served whole."),
                "{note}"
            );
            let clause = plain_clause(page.as_object().unwrap()).unwrap();
            assert!(clause.ends_with(PATCH_EDIT_SENTENCE), "{held}: {clause}");
            assert!(!clause.contains("full_body"), "{held}: {clause}");

            // Within the bound, the whole body and its base.
            let mut served = reader(&fits);
            present("get_entity_source", &whole, &mut served);
            assert_eq!(text(&served), text(&reader(&fits)), "{held}: untouched");

            // By default it is an excerpt that offers the whole.
            let mut excerpted = reader(&fits);
            present("get_entity_source", &HashMap::new(), &mut excerpted);
            let page = json_of(&excerpted);
            let note = page["note"].as_str().unwrap();
            assert!(note.contains("full_body: true"), "{held}: {note}");
            match held {
                "code" => {
                    assert!(page.get("source_base").is_none(), "{page}");
                    assert!(note.ends_with(EDIT_SENTENCE), "{note}");
                }
                _ => assert!(page["source_base"].is_object(), "{page}"),
            }
        }
    }

    /// Every excerpt from the first, in order, by following each cursor.
    fn walk(body: &str, budget: usize) -> Vec<Value> {
        let mut pages = vec![json_of(&paged(body, &HashMap::new(), budget))];
        while let Some(cursor) = pages.last().unwrap().get("next_cursor").cloned() {
            let next = paged(
                body,
                &args(json!({"entity_id": ID, "cursor": cursor})),
                budget,
            );
            assert_ne!(next.is_error, Some(true), "{}", text(&next));
            pages.push(json_of(&next));
            assert!(pages.len() < 10_000, "the walk does not end");
        }
        pages
    }

    #[test]
    fn a_huge_function_is_served_within_the_budget_and_its_excerpts_join_to_it() {
        for body in [huge_function(), nested_function()] {
            let first = paged(&body, &HashMap::new(), SOURCE_PAGE_BYTES);
            let page = json_of(&first);
            let served = page["body"].as_str().unwrap();
            assert!(served.len() <= SOURCE_PAGE_BYTES, "{}", served.len());
            assert!(served.len() >= SOURCE_PAGE_BYTES / 2);
            assert!(body.starts_with(served));
            // The whole reply is the excerpt and a small envelope around it.
            assert!(
                text(&first).len() <= serde_json::to_string(served).unwrap().len() + 4_096,
                "{}",
                text(&first).len()
            );
            assert_eq!(page["body_complete"], false);
            let fields = &page[EXCERPT_KEY];
            assert_eq!(fields["body_bytes"], body.len());
            assert_eq!(fields["body_lines"], line_count(&body));
            assert_eq!(
                fields["body_digest"],
                kin_blobs::digest(body.as_bytes()).to_string()
            );
            assert_eq!(fields["first_line_in_entity"], 0);
            assert_eq!(fields["byte_offset"], 0);
            assert_eq!(fields["remaining_bytes"], body.len() - served.len());
            assert!(page["next_cursor"].as_str().unwrap().len() < 100);
            // No read serves it whole, so the excerpt keeps the base a patch
            // needs, and the note says a patch is all it carries.
            assert_eq!(page["source_base"], base(&body), "{page}");
            assert!(page.get("source_base_unavailable").is_none(), "{page}");
            assert_eq!(page["workspace_generation"], 7);
            // The note says it is an excerpt, how much remains and the call.
            let note = page["note"].as_str().unwrap();
            assert!(note.starts_with("This is an excerpt, not the whole body"));
            assert!(note.contains(&format!("{} bytes", body.len() - served.len())));
            assert!(note.contains(page["next_cursor"].as_str().unwrap()));
            assert!(note.contains(ID));
            assert!(note.ends_with(PATCH_EDIT_SENTENCE), "{note}");
            assert!(!note.contains("full_body"), "{note}");
            assert!(is_excerpt_note(note));

            let pages = walk(&body, SOURCE_PAGE_BYTES);
            assert!(pages.len() > 1);
            let joined: String = pages
                .iter()
                .map(|page| page["body"].as_str().unwrap())
                .collect();
            assert_eq!(joined, body, "the excerpts are the body byte for byte");
            let mut offset = 0;
            for page in &pages {
                let fields = &page[EXCERPT_KEY];
                assert_eq!(fields["byte_offset"], offset);
                assert_eq!(fields["body_digest"], pages[0][EXCERPT_KEY]["body_digest"]);
                assert!(page["body"].as_str().unwrap().len() <= SOURCE_PAGE_BYTES);
                offset += fields["bytes"].as_u64().unwrap() as usize;
                assert_eq!(fields["remaining_bytes"], body.len() - offset);
            }
            let last = pages.last().unwrap();
            assert!(last.get("next_cursor").is_none());
            assert_eq!(last[EXCERPT_KEY]["remaining_lines"], 0);
            assert_eq!(
                last[EXCERPT_KEY]["last_line_in_entity"],
                line_count(&body) - 1
            );
            assert!(last["note"]
                .as_str()
                .unwrap()
                .contains("Nothing remains: the excerpts joined in order are the whole body."));
        }
    }

    #[test]
    fn excerpts_end_at_a_line_end_and_number_their_lines_from_the_entity() {
        let body = nested_function();
        let pages = walk(&body, 1_000);
        for (index, page) in pages.iter().enumerate() {
            let fields = &page[EXCERPT_KEY];
            let served = page["body"].as_str().unwrap();
            let start = fields["byte_offset"].as_u64().unwrap() as usize;
            assert_eq!(
                fields["first_line_in_entity"],
                newlines(&body.as_bytes()[..start]),
                "page {index}"
            );
            if index + 1 < pages.len() {
                assert!(served.ends_with('\n'), "page {index} ends mid-line");
                assert!(fields.get("ends_mid_line").is_none());
            }
            assert!(fields.get("starts_mid_line").is_none());
            // The last line it serves is the first line of the next one, less one.
            if let Some(next) = pages.get(index + 1) {
                assert_eq!(
                    next[EXCERPT_KEY]["first_line_in_entity"].as_u64().unwrap(),
                    fields["last_line_in_entity"].as_u64().unwrap() + 1
                );
            }
        }
    }

    /// A body that is one long line, as minified code is, still pages, cut at
    /// character boundaries and never inside a multi-byte character.
    #[test]
    fn a_body_of_one_long_line_pages_at_character_boundaries() {
        let body = format!("const table = \"{}\";", "日本語のテキスト".repeat(4_000));
        let pages = walk(&body, 1_001);
        let joined: String = pages
            .iter()
            .map(|page| page["body"].as_str().unwrap())
            .collect();
        assert_eq!(joined, body);
        assert_eq!(pages[0][EXCERPT_KEY]["ends_mid_line"], true);
        assert_eq!(pages[1][EXCERPT_KEY]["starts_mid_line"], true);
        assert_eq!(pages[1][EXCERPT_KEY]["first_line_in_entity"], 0);
        assert_eq!(pages[0][EXCERPT_KEY]["remaining_lines"], 1);
        for page in &pages {
            assert!(page["body"].as_str().unwrap().len() <= 1_001);
        }
    }

    #[test]
    fn a_stale_cursor_is_refused_by_name() {
        let body = huge_function();
        let first = json_of(&paged(&body, &HashMap::new(), SOURCE_PAGE_BYTES));
        let cursor = first["next_cursor"].clone();
        let changed = body.replacen("case 7:", "case 70:", 1);
        let stale = paged(
            &changed,
            &args(json!({"entity_id": ID, "cursor": cursor})),
            SOURCE_PAGE_BYTES,
        );
        assert_eq!(stale.is_error, Some(true));
        let refusal = text(&stale);
        assert!(refusal.starts_with("source_cursor_stale:"), "{refusal}");
        assert!(refusal.contains("renderTree"), "{refusal}");
        assert!(refusal.contains(ID), "{refusal}");
        assert!(!refusal.contains("case 7"), "{refusal}");
    }

    #[test]
    fn a_cursor_that_was_not_copied_exactly_or_names_another_entity_is_refused() {
        let body = huge_function();
        let first = json_of(&paged(&body, &HashMap::new(), SOURCE_PAGE_BYTES));
        let cursor = first["next_cursor"].as_str().unwrap().to_string();
        let digest = cursor.rsplit('.').next().unwrap().to_string();
        let other = "9f0c6388-0170-48cc-bde5-2bc591e4e8aa";
        for (sent, code) in [
            (json!(42), "source_cursor_invalid"),
            (json!("src1.not-a-uuid.10.ab"), "source_cursor_invalid"),
            (
                json!(format!("src1.{ID}.0.{digest}")),
                "source_cursor_invalid",
            ),
            (
                json!(format!("src2.{ID}.10.{digest}")),
                "source_cursor_invalid",
            ),
            (
                json!(format!("src1.{ID}.{}.{digest}", body.len())),
                "source_cursor_invalid",
            ),
            (
                json!(format!("src1.{other}.10.{digest}")),
                "continues entity 9f0c6388",
            ),
        ] {
            let refused = paged(
                &body,
                &args(json!({"entity_id": ID, "cursor": sent})),
                SOURCE_PAGE_BYTES,
            );
            assert_eq!(refused.is_error, Some(true), "{sent}");
            assert!(text(&refused).contains(code), "{sent}: {}", text(&refused));
        }
        let both = paged(
            &body,
            &args(json!({"entity_id": ID, "cursor": cursor, "full_body": true})),
            SOURCE_PAGE_BYTES,
        );
        assert!(text(&both).starts_with("cursor and full_body cannot be combined"));
    }

    /// `full_body` serves a body within its bound, and a body within the
    /// budget is served, exactly as the handler answered it, base included.
    #[test]
    fn full_body_and_small_bodies_are_untouched() {
        let body = nested_function()[..FULL_BODY_MAX_BYTES].to_string();
        let whole = paged(
            &body,
            &args(json!({"entity_id": ID, "full_body": true})),
            SOURCE_PAGE_BYTES,
        );
        assert_eq!(text(&whole), text(&read(&body)));
        assert_eq!(json_of(&whole)["source_base"], base(&body));

        for small in [
            "function a() {}".to_string(),
            "x".repeat(SOURCE_PAGE_BYTES),
            nested_function()[..SOURCE_PAGE_BYTES].to_string(),
        ] {
            let served = paged(&small, &HashMap::new(), SOURCE_PAGE_BYTES);
            assert_eq!(text(&served), text(&read(&small)), "byte for byte");
        }
    }

    /// Errors, outlines, inventories and other tools pass through, and a
    /// cursor sent to a reply with no body says it continues nothing.
    #[test]
    fn replies_without_a_body_pass_through_and_refuse_a_cursor() {
        let error = ToolCallResult::error("Entity not found: e1");
        let mut served = error.clone();
        present("get_entity_source", &HashMap::new(), &mut served);
        assert_eq!(text(&served), text(&error));

        let outline = ToolCallResult::text(
            json!({"id": ID, "name": "Store", "body_omitted": "container_shape", "members": []})
                .to_string(),
        );
        let mut served = outline.clone();
        present("get_entity_source", &HashMap::new(), &mut served);
        assert_eq!(text(&served), text(&outline));

        let first = json_of(&paged(&huge_function(), &HashMap::new(), SOURCE_PAGE_BYTES));
        let mut continued = outline.clone();
        present(
            "get_entity_source",
            &args(json!({"entity_id": ID, "cursor": first["next_cursor"]})),
            &mut continued,
        );
        assert!(text(&continued).starts_with("source_cursor_stale: Store is no longer served"));

        let mut other = read(&huge_function());
        let before = text(&other).to_string();
        present("get_context_pack", &HashMap::new(), &mut other);
        assert_eq!(text(&other), before);
    }

    /// A compact reply stays compact, so a reply's format is the handler's.
    #[test]
    fn an_excerpt_keeps_the_format_it_was_answered_in() {
        let body = huge_function();
        let reply = json!({"id": ID, "name": "renderTree", "body": body});
        let mut compact = ToolCallResult::text(reply.to_string());
        present("get_entity_source", &HashMap::new(), &mut compact);
        assert!(!text(&compact).contains('\n'), "{}", text(&compact));
        assert_eq!(json_of(&compact)["body_complete"], false);
    }

    const FORWARDING_CHILD: &str = "source_pages::tests::source_excerpts_forwarding_child";

    /// One plain read of a long function on the path `kin mcp start` serves:
    /// the daemon answers every source call with the whole body, as it always
    /// has, and the client is served excerpts that it follows to the end. Run
    /// in a separate process so the stand-in endpoint and the delegate's
    /// process-global state reach no other test.
    #[test]
    fn a_long_source_read_through_the_daemon_is_served_as_excerpts() {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        if std::env::var("SOURCE_EXCERPTS_TEST_PARENT").as_deref() != Ok("1") {
            let home = tempfile::tempdir().unwrap();
            let repo = home.path().join("repo");
            std::fs::create_dir(&repo).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "source_pages::tests::a_long_source_read_through_the_daemon_is_served_as_excerpts",
                    "--nocapture",
                ])
                .current_dir(&repo)
                .env("HOME", home.path().join("private-home"))
                .env("KIN_HOME", home.path().join("kin-home"))
                .env("KIN_REGISTRY_PATH", home.path().join("kin-home/registry.toml"))
                .env("KIN_NO_DAEMON", "1")
                .env("SOURCE_EXCERPTS_TEST_PARENT", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "the fixture process ran nothing: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        let repo = std::env::current_dir().unwrap();
        let runtime = crate::daemon_delegate::RuntimeDaemonFixture::new(&repo);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(Mutex::new(Vec::<Value>::new()));
        let answer = serde_json::to_string(&read(&huge_function())).unwrap();
        let server = {
            let (stop, calls) = (Arc::clone(&stop), Arc::clone(&calls));
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let mut socket = match listener.accept() {
                        Ok((socket, _)) => socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(error) => panic!("fixture listener: {error}"),
                    };
                    socket.set_nonblocking(false).unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut request = Vec::new();
                    let (head, body) = loop {
                        let mut chunk = [0; 4096];
                        let received = socket.read(&mut chunk).unwrap_or(0);
                        if received == 0 {
                            break (String::new(), Vec::new());
                        }
                        request.extend_from_slice(&chunk[..received]);
                        let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                        else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&request[..end]).to_string();
                        let length: usize = head
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break (head, request[end + 4..end + 4 + length].to_vec());
                        }
                    };
                    let target = head
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or_default();
                    let (status, reply) = if let Some(reply) = runtime.route_response(target) {
                        ("200 OK", reply)
                    } else if head.starts_with("POST /mcp/tools/call ") {
                        calls
                            .lock()
                            .unwrap()
                            .push(serde_json::from_slice(&body).unwrap());
                        ("200 OK", answer.clone())
                    } else {
                        ("404 Not Found", "{}".to_string())
                    };
                    let _ = write!(
                        socket,
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: \
                         {}\r\nconnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                }
            })
        };
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", FORWARDING_CHILD, "--ignored", "--nocapture"])
            .current_dir(&repo)
            .env("KIN_DAEMON_URL", format!("http://{address}"))
            .env("KIN_DAEMON_AUTH_TOKEN", "isolated-fixture-token")
            .env("KIN_NO_DAEMON", "1")
            .env("SOURCE_EXCERPTS_TEST_CHILD", "1")
            .output()
            .unwrap();
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "the child ran nothing: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        // The daemon was asked for the registered tool every time, and the
        // cursor travelled with each continuation without changing its read.
        let calls = calls.lock().unwrap();
        assert!(calls.len() > 4, "{calls:?}");
        for call in calls.iter() {
            assert_eq!(call["name"], "get_entity_source", "{call}");
            assert_eq!(call["arguments"]["entity_id"], ID, "{call}");
        }
        assert!(calls[0]["arguments"].get(CURSOR_ARGUMENT).is_none());
        assert!(calls[1]["arguments"][CURSOR_ARGUMENT].is_string());
    }

    /// The child half: an agent profile that numbers lines and answers in
    /// plain words, then the full profile's machine reply by the registered
    /// name, each walked from the first excerpt to the last.
    #[tokio::test]
    #[ignore = "subprocess body invoked by a_long_source_read_through_the_daemon_is_served_as_excerpts"]
    async fn source_excerpts_forwarding_child() {
        use crate::server::{present_for_client, process_daemon_message, McpServerConfig};
        assert_eq!(
            std::env::var("SOURCE_EXCERPTS_TEST_CHILD").as_deref(),
            Ok("1")
        );
        crate::daemon_delegate::RuntimeDaemonFixture::install_registrar();
        let body = huge_function();
        let agent = McpServerConfig {
            allowed_tools: Some(crate::tools::name_set(
                crate::tools::agent_query_tool_names(),
            )),
            agent_belt: true,
            plain_replies: true,
            number_entity_lines: true,
            ..McpServerConfig::default()
        };
        for (config, tool, numbered) in [
            (agent, crate::verbs::SOURCE, true),
            (McpServerConfig::default(), "get_entity_source", false),
        ] {
            let mut cursor: Option<String> = None;
            let mut joined = String::new();
            let mut next_line = 0u64;
            loop {
                let mut arguments = json!({"entity_id": ID});
                if let Some(cursor) = &cursor {
                    arguments[CURSOR_ARGUMENT] = json!(cursor);
                }
                let message = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": tool, "arguments": arguments}})
                .to_string();
                let response = process_daemon_message(&message, &config).await.unwrap();
                let response = present_for_client(response, &message, &config);
                let result = response
                    .result
                    .unwrap_or_else(|| panic!("{:?}", response.error));
                assert_ne!(result["isError"], true, "{result}");
                let text = result["content"][0]["text"].as_str().unwrap();
                let reply: Value = serde_json::from_str(text).unwrap();
                let served = reply["body"].as_str().unwrap();
                assert!(
                    text.len() <= serde_json::to_string(served).unwrap().len() + 4_096,
                    "a reply is its excerpt and a small envelope: {} bytes",
                    text.len()
                );
                if numbered {
                    for line in served.split_inclusive('\n') {
                        let (number, rest) = line.split_once('\t').unwrap();
                        assert_eq!(number, format!("+{next_line}"), "{reply}");
                        next_line += 1;
                        joined.push_str(rest);
                    }
                } else {
                    joined.push_str(served);
                }
                let said = if numbered {
                    reply["_kin"]["meaning"].as_str().unwrap()
                } else {
                    reply["note"].as_str().unwrap()
                };
                assert!(said.contains("not the whole body"), "{reply}");
                assert!(said.ends_with(PATCH_EDIT_SENTENCE), "{reply}");
                assert!(reply.get("source_base").is_some(), "{reply}");
                cursor = reply
                    .get("next_cursor")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                match &cursor {
                    Some(cursor) => assert!(
                        said.contains(&format!(
                            "{} {{\"entity_id\":\"{ID}\",\"cursor\":\"{cursor}\"}}",
                            if numbered {
                                "source"
                            } else {
                                "get_entity_source"
                            }
                        )),
                        "{reply}"
                    ),
                    None => break,
                }
            }
            assert_eq!(joined, body, "{tool}: the excerpts are the body");
        }
    }

    #[test]
    fn the_plain_clause_names_the_one_source_call_that_continues() {
        let body = huge_function();
        let page = json_of(&paged(&body, &HashMap::new(), SOURCE_PAGE_BYTES));
        let clause = plain_clause(page.as_object().unwrap()).unwrap();
        let cursor = page["next_cursor"].as_str().unwrap();
        assert!(clause.starts_with("lines +0 to +"), "{clause}");
        assert!(clause.contains("not the whole body"), "{clause}");
        assert!(clause.contains(&format!(
            "Continue with source {{\"entity_id\":\"{ID}\",\"cursor\":\"{cursor}\"}}"
        )));
        assert!(clause.ends_with(PATCH_EDIT_SENTENCE), "{clause}");
        assert!(clause.contains("a body over 16 KB is never served whole."));
        assert!(!clause.contains('\u{2014}'));
        assert!(plain_clause(json_of(&read("fn a() {}")).as_object().unwrap()).is_none());
    }
}
