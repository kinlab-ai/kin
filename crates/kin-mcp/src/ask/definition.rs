// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The `ask` tool as the registry lists it.

use serde_json::json;

use super::compose::{DEFAULT_REPLY_BYTES, MAX_REPLY_BYTES, MIN_REPLY_BYTES, TOOL_NAME};
use crate::types::{ToolAnnotations, ToolDefinition};

/// The long description the `full` profile serves.
const DESCRIPTION: &str = "Ask a question about this repository in plain words, the way a developer would: who calls X, what can end up calling X, what has to change if X changes, is X used anywhere, which modules declare X, where is X, show X's code. Kin reads the question, runs the graph query that answers it (`refs`, `trace`, `impact`, `locate`, `source` or `context`), and answers in one reply. The reply's first line says what Kin ran and which words of the question it read that from; then it names anything the question asked for that the query did not apply, such as a path, which Kin never takes as an address, or a package it could not map to a module; then each answer follows in its own words, verdict first. A name several declarations share is listed whole. A change question and a question about whether it is used are answered for each declaration, up to eight. The words of the question decide whether test code counts and which package the answer is limited to; `include_tests` and `scope` override them.";

/// The registered definition of `ask`.
pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: TOOL_NAME.into(),
        description: DESCRIPTION.into(),
        annotations: ToolAnnotations {
            title: "Ask Kin".into(),
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: false,
        },
        input_schema: json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question in plain words. Name the function, method or class it is about."
                },
                "include_tests": {
                    "type": "boolean",
                    "description": "Whether test code counts. Overrides what the question says; when neither says, it counts."
                },
                "scope": {
                    "type": "string",
                    "description": "Dotted prefix of a row `module` the answer is limited to. Overrides a package the question names."
                },
                "max_output_bytes": {
                    "type": "integer",
                    "minimum": MIN_REPLY_BYTES,
                    "maximum": MAX_REPLY_BYTES,
                    "default": DEFAULT_REPLY_BYTES
                },
                "explain": {
                    "type": "boolean",
                    "description": "Adds a last line with the plan and every call Kin made."
                }
            },
            "required": ["question"]
        }),
    }
}
