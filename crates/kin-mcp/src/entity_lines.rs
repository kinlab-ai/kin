// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! An entity's source body, with each line marked by its offset in the entity.
//!
//! A reader cites an entity line by its offset: each line starts
//! with `+N` and a tab, where `N` counts from `+0` at the entity's first line.
//! The plus sign marks an offset in the entity, and
//! the note that says what the offsets are sits directly before the body it
//! describes. It does not expose a physical source coordinate.
//!
//! A presentation for an external MCP client and nothing else. It is applied
//! only on a connection that serves no write path and asked for no exact
//! bytes: a connection that can write through Kin sends an entity's whole body
//! back as the base of an edit, and `kin agent run` copies a body byte for byte
//! into the text an edit must match, so both are served the exact bytes the
//! graph holds. `kin agent run` says so when it connects, with the
//! [`EXACT_BODIES_CAPABILITY`] flag.

use serde_json::{Map, Value};

use crate::types::{ContentBlock, ToolCallResult, ToolsListResult};

/// The key the numbering note rides under, directly before `body`.
///
/// Named so that it sorts there. This workspace's `serde_json` keeps an
/// object's keys in sorted order, not insertion order, so a note's place in
/// the answer is decided by its name: `about_body` is read immediately before
/// `body` in every source record the graph serves.
pub const NUMBERING_KEY: &str = "about_body";

/// What the offsets are, in the response that carries them.
pub const NUMBERING_NOTE: &str = "Each body line starts with +N and a tab, where N is the \
line's offset from the entity's first line. Strip that prefix for the exact text.";

/// The initialize capability, under `capabilities.experimental.kin`, with which
/// a client asks for exact entity bodies whatever its profile would present.
pub const EXACT_BODIES_CAPABILITY: &str = "exactEntityBodies";

/// `source`'s short description on a connection that numbers.
///
/// The same length as the exact form, so a numbering profile's listing costs
/// what its measured ceiling says.
pub const NUMBERED_SOURCE_DESCRIPTION: &str = "One entity's source, lines as +N.";

/// Whether `tool` answers with one entity's source body: the registered source
/// tools, and `source`, the verb that serves one of them.
pub fn numbers_this_tool(tool: &str) -> bool {
    matches!(
        tool,
        "get_entity_source" | "get_entity_body" | crate::verbs::SOURCE
    )
}

/// Whether an `initialize` request asks for exact entity bodies.
pub fn client_wants_exact_bodies(initialize: &Value) -> bool {
    initialize
        .pointer("/params/capabilities/experimental/kin")
        .and_then(|kin| kin.get(EXACT_BODIES_CAPABILITY))
        .and_then(Value::as_bool)
        == Some(true)
}

/// Say in a served listing that the source tools number their bodies.
pub fn describe_numbered_bodies(list: &mut ToolsListResult) {
    for tool in &mut list.tools {
        if numbers_this_tool(&tool.name) {
            tool.description = NUMBERED_SOURCE_DESCRIPTION.to_string();
        }
    }
}

/// Mark each line of the `body` of a successful entity source result with its
/// offset, and say what the offsets are directly before it. Anything else, an
/// error, a derived candidate with no body of its own, a payload that is not an
/// object, passes through untouched.
pub fn number_entity_body(result: &mut ToolCallResult) {
    if result.is_error == Some(true) {
        return;
    }
    for block in &mut result.content {
        let ContentBlock::Text { text } = block;
        let Ok(Value::Object(object)) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        let Some(body) = object.get("body").and_then(Value::as_str) else {
            continue;
        };
        // An excerpt's lines keep the offsets they have in the whole body.
        let first = object
            .get(crate::source_pages::EXCERPT_KEY)
            .and_then(|excerpt| excerpt.get("first_line_in_entity"))
            .and_then(Value::as_u64)
            .and_then(|first| usize::try_from(first).ok())
            .unwrap_or(0);
        let numbered = number_lines_from(body, first);
        let note = NUMBERING_NOTE.to_string();
        let mut presented = Map::with_capacity(object.len() + 1);
        for (key, value) in object {
            if key == "body" {
                presented.insert(NUMBERING_KEY.to_string(), Value::String(note.clone()));
                presented.insert(key, Value::String(numbered.clone()));
            } else {
                presented.insert(key, value);
            }
        }
        if let Ok(rendered) = serde_json::to_string_pretty(&Value::Object(presented)) {
            *text = rendered;
        }
    }
}

/// `body` with each line prefixed by `+N` and a tab, `N` its 0-based offset
/// within the body. Line endings are kept as they were, and a final newline
/// starts no line.
pub fn number_lines(body: &str) -> String {
    number_lines_from(body, 0)
}

/// [`number_lines`] for an excerpt whose first line is line `first` of its
/// entity.
pub fn number_lines_from(body: &str, first: usize) -> String {
    let mut numbered = String::with_capacity(body.len() + body.len() / 8 + 8);
    for (offset, line) in body.split_inclusive('\n').enumerate() {
        numbered.push('+');
        numbered.push_str(&(first + offset).to_string());
        numbered.push('\t');
        numbered.push_str(line);
    }
    numbered
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_of(result: &ToolCallResult) -> Value {
        let ContentBlock::Text { text } = &result.content[0];
        serde_json::from_str(text).expect("JSON payload")
    }

    #[test]
    fn lines_are_offsets_from_the_entity_first_line() {
        assert_eq!(
            number_lines("fn a() {\n    b();\n}\n"),
            "+0\tfn a() {\n+1\t    b();\n+2\t}\n"
        );
        assert_eq!(number_lines("x\ny"), "+0\tx\n+1\ty");
        assert_eq!(number_lines("\r\nz"), "+0\t\r\n+1\tz");
        assert_eq!(number_lines(""), "");
    }

    /// The daemon's record shape: the body is marked, the note sits directly
    /// before it and describes only entity-relative offsets. Raw metadata is
    /// left for the semantic egress projection to remove.
    #[test]
    fn a_daemon_record_gains_only_entity_relative_offsets() {
        let record = json!({
            "id": "e1", "name": "target", "kind": "Function",
            "file_path": "src/lib.rs", "start_line": 120, "end_line": 122,
            "start_byte": 900, "end_byte": 930,
            "signature": "fn target()",
            "body": "fn target() {\n    call();\n}",
            "span_coherence": "verified",
        });
        let mut result = ToolCallResult::text(serde_json::to_string_pretty(&record).unwrap());
        number_entity_body(&mut result);
        let numbered = text_of(&result);
        assert_eq!(
            numbered["body"],
            "+0\tfn target() {\n+1\t    call();\n+2\t}"
        );
        assert_eq!(numbered[NUMBERING_KEY], NUMBERING_NOTE.to_string());
        for key in [
            "file_path",
            "start_line",
            "end_line",
            "start_byte",
            "end_byte",
            "signature",
            "span_coherence",
        ] {
            assert_eq!(numbered[key], record[key], "{key} moved");
        }
        let keys: Vec<&String> = numbered.as_object().unwrap().keys().collect();
        let body_at = keys.iter().position(|key| *key == "body").unwrap();
        assert_eq!(
            keys[body_at - 1],
            NUMBERING_KEY,
            "the note is not next to the body"
        );
        assert_eq!(keys.len(), record.as_object().unwrap().len() + 1);
    }

    /// The offline record's `start_line` can be the declaration line below a
    /// doc comment, so it gets the offsets and no arithmetic.
    #[test]
    fn a_record_without_the_graph_span_gets_no_file_line_arithmetic() {
        let record = json!({"id": "e1", "start_line": 12, "body": "/// doc\nfn a() {}"});
        let mut result = ToolCallResult::text(record.to_string());
        number_entity_body(&mut result);
        let numbered = text_of(&result);
        assert_eq!(numbered[NUMBERING_KEY], NUMBERING_NOTE);
        assert_eq!(numbered["body"], "+0\t/// doc\n+1\tfn a() {}");
    }

    /// The note cannot be read as file line numbers.
    #[test]
    fn the_note_says_the_numbers_are_offsets() {
        let note = NUMBERING_NOTE.to_string();
        assert!(note.contains("offset from the entity's first line"));
        assert!(note.contains("Strip that prefix for the exact text"));
        assert!(!note.contains('\u{2014}'));
        assert!(!note.contains("File line"));
        assert!(!note.contains("start_line"));
        assert_eq!(
            NUMBERED_SOURCE_DESCRIPTION.len(),
            "One entity's exact source, by id.".len(),
            "a numbering listing must cost what the exact one does"
        );
    }

    /// An excerpt's lines are numbered from where they sit in the whole body,
    /// so offsets read across excerpts address one entity.
    #[test]
    fn an_excerpt_is_numbered_from_its_first_line_in_the_entity() {
        let record = json!({
            "id": "e1", "body": "    b();\n}",
            "body_excerpt": {"first_line_in_entity": 41, "byte_offset": 900},
        });
        let mut result = ToolCallResult::text(record.to_string());
        number_entity_body(&mut result);
        assert_eq!(text_of(&result)["body"], "+41\t    b();\n+42\t}");
    }

    /// Nothing that is not a successful body is touched.
    #[test]
    fn errors_and_bodiless_answers_pass_through() {
        let mut error = ToolCallResult::error("Entity not found: e1");
        number_entity_body(&mut error);
        let ContentBlock::Text { text } = &error.content[0];
        assert_eq!(text, "Entity not found: e1");

        let derived = json!({"id": "e2", "body": null, "generator_source": {"body": "x\ny"}});
        let mut result = ToolCallResult::text(derived.to_string());
        number_entity_body(&mut result);
        assert_eq!(text_of(&result), derived);
    }

    #[test]
    fn only_the_entity_source_tools_are_numbered() {
        assert!(numbers_this_tool("get_entity_source"));
        assert!(numbers_this_tool("get_entity_body"));
        for tool in [
            "unregistered_tool",
            "get_entity_sources",
            "get_context_pack",
            "trace_data_flow",
        ] {
            assert!(!numbers_this_tool(tool), "{tool}");
        }
    }

    #[test]
    fn a_client_asks_for_exact_bodies_through_its_initialize_capabilities() {
        let initialize = |capabilities: Value| {
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"protocolVersion": "2024-11-05", "capabilities": capabilities}})
        };
        assert!(client_wants_exact_bodies(&initialize(
            json!({"experimental": {"kin": {"exactEntityBodies": true}}})
        )));
        for capabilities in [
            json!({}),
            json!({"experimental": {}}),
            json!({"experimental": {"kin": {"exactEntityBodies": false}}}),
            json!({"experimental": {"kin": {"exactEntityBodies": "yes"}}}),
            json!({"roots": {"listChanged": true}}),
        ] {
            assert!(
                !client_wants_exact_bodies(&initialize(capabilities.clone())),
                "{capabilities}"
            );
        }
    }
}
