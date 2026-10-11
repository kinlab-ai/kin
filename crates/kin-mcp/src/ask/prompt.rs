// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! `ask` as an MCP prompt.
//!
//! A client that surfaces a server's prompts as commands, as Claude Code does
//! with `/mcp__kin__ask`, VS Code with `/mcp.kin.ask` and Zed and Gemini CLI
//! with their own slash commands, lets a person type a question and send the
//! model Kin's answer as their own turn. The model starts from the answer
//! rather than from a tool call, which is the front door's answer-first form
//! for a client with no prompt hook.
//!
//! The prompt runs the same composer as the `ask` tool and is served on the
//! same profiles: one that lists `ask`, or `full`.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::compose::TOOL_NAME;

/// The prompt's name, the tool's own.
pub const PROMPT_NAME: &str = TOOL_NAME;

/// The prompt as `prompts/list` lists it.
pub fn listed() -> Value {
    json!({
        "name": PROMPT_NAME,
        "title": "Ask Kin",
        "description": "Ask a question about this repository in plain words. Kin runs the graph query that answers it and sends its answer as your message.",
        "arguments": [{
            "name": "question",
            "description": "The question, naming the function, method or class it is about.",
            "required": true,
        }],
    })
}

/// The `ask` arguments a `prompts/get` request makes, or the sentence that
/// says what is wrong with it.
pub fn arguments_of(params: &Value) -> Result<HashMap<String, Value>, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if name != PROMPT_NAME {
        return Err(format!(
            "There is no prompt named '{name}'; this server serves the prompt '{PROMPT_NAME}'."
        ));
    }
    let question = params
        .get("arguments")
        .and_then(|arguments| arguments.get("question"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|question| !question.is_empty())
        .ok_or_else(|| {
            "The ask prompt needs a question in plain words, such as \"what calls parse_config?\"."
                .to_string()
        })?;
    Ok(HashMap::from([("question".to_string(), json!(question))]))
}

/// The `prompts/get` result for `question` and Kin's reply: one user message
/// with the question and the answer Kin already ran.
pub fn result(question: &str, reply: &str) -> Value {
    json!({
        "description": "Kin's answer to the question, run against the repository graph.",
        "messages": [{
            "role": "user",
            "content": {
                "type": "text",
                "text": format!(
                    "{question}\n\nKin ran this question against the repository graph before you started. Its answer:\n\n{reply}\n\nIf this answers the question, answer from it, and claim only the coverage each Answer line states."
                ),
            },
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_takes_one_required_question() {
        let listed = listed();
        assert_eq!(listed["name"], "ask");
        assert_eq!(listed["arguments"][0]["name"], "question");
        assert_eq!(listed["arguments"][0]["required"], true);
        let arguments =
            arguments_of(&json!({"name": "ask", "arguments": {"question": " who calls run? "}}))
                .unwrap();
        assert_eq!(arguments["question"], "who calls run?");
        assert!(arguments_of(&json!({"name": "ask", "arguments": {}})).is_err());
        assert!(arguments_of(&json!({"name": "other", "arguments": {"question": "x"}})).is_err());
    }

    #[test]
    fn the_result_is_one_user_message_with_the_question_and_kins_answer() {
        let result = result("who calls run?", "Kin ran: direct callers of app :: run.");
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        let text = messages[0]["content"]["text"].as_str().unwrap();
        assert!(text.starts_with("who calls run?\n\nKin ran this question"));
        assert!(text.contains("Kin ran: direct callers of app :: run."));
    }
}
