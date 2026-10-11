// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! `kin ask "<question>"`: the front door from a shell.
//!
//! One `ask` call through the same path `kin call` takes, against this
//! repository's daemon, so a shell is answered with the reply an MCP client
//! gets: what Kin ran and what it read the question as, what it did not
//! apply, and each answer, verdict first.
//!
//! `kin ask --hook <client>` is the same answer given before the model's
//! first turn: an agent client runs it on every prompt, hands it the prompt
//! on stdin, and adds what it prints to the model's context. It answers only
//! a prompt that names code and asks something the rules route, says nothing
//! otherwise, and gives the question back to the model when Kin is not ready
//! in time.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Map, Value};

use super::routed_words::{send_to_this_repository, shell_config, Answer};

/// The options `kin ask` takes beside the question.
#[derive(Debug, Clone, Default)]
pub struct AskOptions {
    /// `Some(true)` for `--tests`, `Some(false)` for `--no-tests`.
    pub include_tests: Option<bool>,
    pub scope: Option<String>,
    pub budget: Option<u64>,
    /// Print one JSON object with the answer, its one verdict and its plan.
    pub json: bool,
}

/// The `ask` arguments a question and its options make.
pub fn arguments(question: &str, options: &AskOptions) -> Map<String, Value> {
    let mut arguments = Map::new();
    arguments.insert("question".into(), json!(question));
    if let Some(tests) = options.include_tests {
        arguments.insert("include_tests".into(), json!(tests));
    }
    if let Some(scope) = &options.scope {
        arguments.insert("scope".into(), json!(scope));
    }
    if let Some(budget) = options.budget {
        arguments.insert("max_output_bytes".into(), json!(budget));
    }
    if options.json {
        arguments.insert("explain".into(), json!(true));
    }
    arguments
}

/// The text `kin ask` prints for an answer: the reply as it is, or with
/// `--json` one object holding the reply, its one verdict and its plan.
pub fn printed(answer: &Answer, json_output: bool) -> String {
    if !json_output {
        return answer.text.clone();
    }
    let (reply, plan) = match answer.text.rsplit_once("\n\nPlan: ") {
        Some((reply, plan)) => (reply.to_string(), serde_json::from_str::<Value>(plan).ok()),
        None => (answer.text.clone(), None),
    };
    let mut object = json!({ "answer": reply, "error": answer.is_error });
    if let Some(plan) = plan {
        object["verdict"] = plan["verdict"].clone();
        object["plan"] = plan["plan"].clone();
    }
    serde_json::to_string_pretty(&object).unwrap_or_default()
}

/// The agent clients whose prompt hook `kin ask --hook` answers, by the
/// name the option takes.
pub const HOOK_CLIENTS: [&str; 3] = ["claude", "gemini", "codex"];

/// The most bytes of reply a hook hands its client. Claude Code adds at most
/// 10,000 characters of a hook's context, and a byte is never fewer
/// characters, so the reply and its framing stay inside that.
pub const HOOK_REPLY_BYTES: u64 = 9_000;

/// How long a hook waits for Kin before it leaves the question to the model.
/// A client gives a prompt hook 30 seconds by default.
pub const HOOK_DEADLINE: Duration = Duration::from_secs(20);

/// The prompt a client's hook hands over, and the folder it was asked in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookInput {
    pub prompt: String,
    pub cwd: Option<PathBuf>,
}

/// The prompt and folder in a client's hook input, or `None` when it carries
/// no prompt.
pub fn hook_input(raw: &str) -> Option<HookInput> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let prompt = ["prompt", "user_prompt"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())?
        .to_string();
    let cwd = value.get("cwd").and_then(Value::as_str).map(PathBuf::from);
    Some(HookInput { prompt, cwd })
}

/// Whether a prompt is one the front door answers before the first turn: it
/// names something in code and asks what the rules route. Anything else, a
/// thank-you or a request to run the tests, is left to the model unanswered.
pub fn worth_answering(prompt: &str) -> bool {
    let plan = kin_mcp::ask::plan(prompt);
    plan.intent != kin_mcp::ask::Intent::Other && !plan.targets.is_empty()
}

/// What a hook adds to the model's context around Kin's reply.
pub fn hook_context(reply: &str) -> String {
    format!(
        "Kin ran this question against the repository graph before you started. Its answer:\n\n{reply}\n\nIf this answers the question, answer from it, and claim only the coverage each Answer line states."
    )
}

/// The line a hook adds when Kin was not ready in time.
pub const HOOK_NOT_READY: &str =
    "Kin did not answer this question within 20 seconds; its ask tool runs it.";

/// What `client`'s hook prints for `context`: the JSON its prompt hook reads
/// added context from.
pub fn hook_output(client: &str, context: &str) -> String {
    let event = match client {
        "gemini" => "BeforeAgent",
        _ => "UserPromptSubmit",
    };
    json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": context}})
        .to_string()
}

/// Answer a client's prompt hook: the JSON to print, or `None` to print
/// nothing. Never an error: a hook that fails would interrupt the person, and
/// a prompt Kin cannot answer is one the model answers as it would have. Why
/// it printed nothing goes to stderr, which a client does not show for a hook
/// that succeeded.
pub async fn hook(client: &str, raw: &str) -> Option<String> {
    match hook_answer(client, raw).await {
        Ok(printed) => Some(printed),
        Err(why) => {
            eprintln!("kin ask --hook: {why}");
            None
        }
    }
}

async fn hook_answer(client: &str, raw: &str) -> std::result::Result<String, String> {
    let input = hook_input(raw).ok_or("the hook input carries no prompt")?;
    if !worth_answering(&input.prompt) {
        return Err("the prompt asks nothing Kin routes".into());
    }
    if let Some(cwd) = &input.cwd {
        std::env::set_current_dir(cwd)
            .map_err(|error| format!("cannot work in {}: {error}", cwd.display()))?;
    }
    crate::commands::require_repository_layout().map_err(|error| error.to_string())?;
    let options = AskOptions {
        budget: Some(HOOK_REPLY_BYTES),
        ..AskOptions::default()
    };
    match tokio::time::timeout(HOOK_DEADLINE, run(&input.prompt, &options)).await {
        Ok(Ok(answer)) if !answer.is_error => Ok(hook_output(client, &hook_context(&answer.text))),
        Ok(Ok(answer)) => Err(format!("Kin refused the question: {}", answer.text)),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Ok(hook_output(client, HOOK_NOT_READY)),
    }
}

/// Ask this repository's daemon, starting it when none is running.
pub async fn run(question: &str, options: &AskOptions) -> Result<Answer> {
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": kin_mcp::ask::TOOL_NAME, "arguments": arguments(question, options)},
    });
    // `ask` is its own tool, not a routed command, so the shell's surface
    // serves it as `full` does.
    let mut config = shell_config();
    config.routed = None;
    config.allowed_tools = None;
    send_to_this_repository(&request, &config, "kin ask").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_become_the_ask_arguments_and_nothing_else() {
        let none = arguments("who calls run?", &AskOptions::default());
        assert_eq!(Value::Object(none), json!({"question": "who calls run?"}));
        let all = arguments(
            "who calls run?",
            &AskOptions {
                include_tests: Some(false),
                scope: Some("flask".into()),
                budget: Some(8000),
                json: true,
            },
        );
        assert_eq!(
            Value::Object(all),
            json!({"question": "who calls run?", "include_tests": false, "scope": "flask",
                   "max_output_bytes": 8000, "explain": true})
        );
    }

    #[test]
    fn a_hook_reads_the_prompt_and_folder_each_client_sends() {
        let input = hook_input(
            r#"{"session_id":"s","cwd":"/work/app","hook_event_name":"UserPromptSubmit","prompt":" What calls `run`? "}"#,
        )
        .unwrap();
        assert_eq!(input.prompt, "What calls `run`?");
        assert_eq!(input.cwd, Some(PathBuf::from("/work/app")));
        assert!(hook_input(r#"{"prompt":"   "}"#).is_none());
        assert!(hook_input("not json").is_none());
    }

    #[test]
    fn only_a_prompt_that_names_code_and_asks_something_routed_is_answered() {
        assert!(worth_answering("What calls `dispatch`?"));
        assert!(worth_answering("Is make_timedelta used anywhere?"));
        for prompt in [
            "thanks!",
            "Run the test suite please.",
            "What is this repository about?",
        ] {
            assert!(!worth_answering(prompt), "{prompt}");
        }
    }

    #[test]
    fn each_client_reads_added_context_from_its_own_event() {
        for (client, event) in [
            ("claude", "UserPromptSubmit"),
            ("codex", "UserPromptSubmit"),
            ("gemini", "BeforeAgent"),
        ] {
            let printed: Value = serde_json::from_str(&hook_output(client, "c")).unwrap();
            assert_eq!(printed["hookSpecificOutput"]["hookEventName"], event);
            assert_eq!(printed["hookSpecificOutput"]["additionalContext"], "c");
        }
        let context = hook_context("Kin ran: x.");
        assert!(context.starts_with("Kin ran this question against the repository graph"));
        assert!(context.contains("\n\nKin ran: x.\n\n"));
        // The reply budget and the framing stay inside the 10,000 characters
        // a client adds.
        assert!(HOOK_REPLY_BYTES as usize + hook_context("").len() < 10_000);
    }

    #[test]
    fn json_output_splits_the_plan_line_from_the_reply() {
        let answer = Answer {
            text: "Kin ran: x.\nAnswer (complete): y\n\nPlan: {\"verdict\":\"complete\",\"plan\":{\"intent\":\"callers_direct\"}}".into(),
            is_error: false,
        };
        let printed: Value = serde_json::from_str(&printed(&answer, true)).unwrap();
        assert_eq!(printed["answer"], "Kin ran: x.\nAnswer (complete): y");
        assert_eq!(printed["verdict"], "complete");
        assert_eq!(printed["plan"]["intent"], "callers_direct");
        assert_eq!(super::printed(&answer, false), answer.text);
    }
}
