// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Run the front door's verbs the way a client's own calls run.
//!
//! Each verb is one `tools/call` sent through the server's own message path
//! and presented as an agent profile presents it, which is exactly what `kin
//! call` does for a shell. The front door therefore reads the answer a client
//! would read: the same belt defaults, budgets, pages, refusals and plain
//! verdict. It reads that answer in its plain JSON form, whatever this
//! connection's reply format, and writes its own reply once.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use serde_json::{json, Map, Value};

use super::compose::{answer, AskRequest, Composite, SubCalls, TOOL_NAME};
use super::prompt;
use super::sub_answer::SubAnswer;
use crate::server::{
    present_for_client, process_daemon_message, process_message, McpServerConfig,
    PersistableMcpStore,
};
use crate::session::SessionRegistry;
use crate::session_handles::SessionHandles;
use crate::text_reply::ReplyFormat;
use crate::types::{JsonRpcResponse, ToolCallResult};

/// The configuration each verb runs under: this connection's, answering in
/// plain JSON with every read verb reachable, so the front door reads the
/// same plain answer whatever profile or format this connection serves.
fn verb_config(config: &McpServerConfig) -> McpServerConfig {
    let mut verbs = config.clone();
    verbs.plain_replies = true;
    verbs.citable = false;
    verbs.agent_belt = true;
    verbs.reply_format = ReplyFormat::Json;
    verbs.routed = None;
    verbs.allowed_tools = None;
    verbs
}

fn request_for(verb: &str, arguments: &Map<String, Value>) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": verb, "arguments": arguments},
    })
    .to_string()
}

/// Read one presented response as a sub-answer.
fn read(
    verb: &str,
    arguments: Map<String, Value>,
    response: Option<JsonRpcResponse>,
    started: Instant,
) -> SubAnswer {
    let seconds = started.elapsed().as_secs_f64();
    let Some(response) = response else {
        return SubAnswer {
            verb: verb.into(),
            arguments,
            payload: None,
            text: format!("{verb} gave no answer."),
            is_error: true,
            seconds,
            bytes: 0,
        };
    };
    if let Some(error) = response.error {
        return SubAnswer {
            verb: verb.into(),
            arguments,
            payload: None,
            text: error.message,
            is_error: true,
            seconds,
            bytes: 0,
        };
    }
    let result: ToolCallResult = serde_json::from_value(response.result.unwrap_or_default())
        .unwrap_or_else(|error| {
            ToolCallResult::error(format!(
                "{verb} answered in a shape Kin could not read: {error}"
            ))
        });
    let text = result
        .content
        .iter()
        .map(|block| match block {
            crate::types::ContentBlock::Text { text } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let payload = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| value.as_object().cloned());
    SubAnswer {
        verb: verb.into(),
        arguments,
        bytes: text.len(),
        payload,
        text,
        is_error: result.is_error == Some(true),
        seconds,
    }
}

/// The handles this connection writes rows with, or a table of the front
/// door's own when it serves none.
struct Handles<'a> {
    connection: Option<&'a SessionHandles>,
    own: SessionHandles,
}

impl<'a> Handles<'a> {
    fn of(config: &'a McpServerConfig) -> Self {
        Self {
            connection: config.session_handles.as_deref(),
            own: SessionHandles::default(),
        }
    }

    fn get(&self) -> &SessionHandles {
        self.connection.unwrap_or(&self.own)
    }
}

/// Verbs run against this repository's daemon.
struct DaemonCalls<'a> {
    config: McpServerConfig,
    handles: Handles<'a>,
}

impl SubCalls for DaemonCalls<'_> {
    fn call<'a>(
        &'a mut self,
        verb: &'a str,
        arguments: Map<String, Value>,
    ) -> Pin<Box<dyn Future<Output = SubAnswer> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let message = self.config.expand_handles(request_for(verb, &arguments));
            let response = process_daemon_message(&message, &self.config)
                .await
                .map(|response| present_for_client(response, &message, &self.config));
            read(verb, arguments, response, started)
        })
    }

    fn handles(&self) -> &SessionHandles {
        self.handles.get()
    }
}

/// Verbs run against an in-process store.
struct OfflineCalls<'a, G: PersistableMcpStore> {
    store: &'a G,
    sessions: &'a SessionRegistry,
    config: McpServerConfig,
    handles: Handles<'a>,
}

impl<G: PersistableMcpStore> SubCalls for OfflineCalls<'_, G> {
    fn call<'a>(
        &'a mut self,
        verb: &'a str,
        arguments: Map<String, Value>,
    ) -> Pin<Box<dyn Future<Output = SubAnswer> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let message = self.config.expand_handles(request_for(verb, &arguments));
            let response = process_message(&message, self.store, &self.config, self.sessions)
                .await
                .map(|response| present_for_client(response, &message, &self.config));
            read(verb, arguments, response, started)
        })
    }

    fn handles(&self) -> &SessionHandles {
        self.handles.get()
    }
}

/// Whether this connection serves `ask`.
pub fn served(config: &McpServerConfig) -> bool {
    config
        .allowed_tools
        .as_ref()
        .is_none_or(|allowed| allowed.contains(TOOL_NAME))
}

/// The composed answer as the tool result every connection writes: the reply
/// as text, whose every answer carries its own verdict line. Text is the
/// form the front door was measured in, and a client's presentation leaves
/// prose as written, so the reply reaches the client as composed. With
/// `explain`, a last line carries the reply's one verdict and its plan.
pub fn tool_result(composite: &Composite, request: &AskRequest) -> ToolCallResult {
    let mut text = composite.text.clone();
    if request.explain {
        let plan = json!({"verdict": composite.verdict, "plan": composite.record});
        text.push_str(&format!("\n\nPlan: {plan}"));
    }
    let mut result = ToolCallResult::text(text);
    if composite.is_error {
        result.is_error = Some(true);
    }
    result
}

fn refusal(message: String) -> ToolCallResult {
    ToolCallResult::error(message)
}

/// Answer an `ask` call on a daemon-backed connection.
pub async fn answer_daemon(
    arguments: &HashMap<String, Value>,
    config: &McpServerConfig,
) -> ToolCallResult {
    let request = match AskRequest::from_arguments(arguments) {
        Ok(request) => request,
        Err(message) => return refusal(message),
    };
    let mut calls = DaemonCalls {
        config: verb_config(config),
        handles: Handles::of(config),
    };
    let composite = answer(&request, &mut calls).await;
    tool_result(&composite, &request)
}

/// Answer an `ask` call on a connection with an in-process store.
pub async fn answer_offline<G: PersistableMcpStore>(
    arguments: &HashMap<String, Value>,
    store: &G,
    sessions: &SessionRegistry,
    config: &McpServerConfig,
) -> ToolCallResult {
    let request = match AskRequest::from_arguments(arguments) {
        Ok(request) => request,
        Err(message) => return refusal(message),
    };
    let mut calls = OfflineCalls {
        store,
        sessions,
        config: verb_config(config),
        handles: Handles::of(config),
    };
    let composite = answer(&request, &mut calls).await;
    tool_result(&composite, &request)
}

/// Whether this connection serves the `ask` prompt: wherever it serves the
/// tool, and on a routed surface, whose `ask` command runs it.
pub fn serves_prompt(config: &McpServerConfig) -> bool {
    served(config) || config.routed.is_some()
}

/// The `prompts/list` result: the `ask` prompt where it is served.
pub fn prompts_list(config: &McpServerConfig) -> Value {
    let prompts: Vec<Value> = if serves_prompt(config) {
        vec![prompt::listed()]
    } else {
        Vec::new()
    };
    json!({ "prompts": prompts })
}

fn prompt_result(
    arguments: &HashMap<String, Value>,
    result: &ToolCallResult,
) -> Result<Value, String> {
    let reply = result
        .content
        .iter()
        .map(|block| match block {
            crate::types::ContentBlock::Text { text } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    if result.is_error == Some(true) {
        return Err(reply);
    }
    let question = arguments
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(prompt::result(question, &reply))
}

/// The `prompts/get` result for the `ask` prompt on a daemon-backed
/// connection, or the sentence that refuses it.
pub async fn prompt_get_daemon(params: &Value, config: &McpServerConfig) -> Result<Value, String> {
    if !serves_prompt(config) {
        return Err("This connection serves no prompts.".into());
    }
    let arguments = prompt::arguments_of(params)?;
    let result = answer_daemon(&arguments, &verb_free_config(config)).await;
    prompt_result(&arguments, &result)
}

/// The `prompts/get` result for the `ask` prompt on a connection with an
/// in-process store, or the sentence that refuses it.
pub async fn prompt_get_offline<G: PersistableMcpStore>(
    params: &Value,
    store: &G,
    sessions: &SessionRegistry,
    config: &McpServerConfig,
) -> Result<Value, String> {
    if !serves_prompt(config) {
        return Err("This connection serves no prompts.".into());
    }
    let arguments = prompt::arguments_of(params)?;
    let result = answer_offline(&arguments, store, sessions, &verb_free_config(config)).await;
    prompt_result(&arguments, &result)
}

/// The connection's configuration for a prompt's answer: the reply is text
/// on every connection, and a routed surface's `ask` is the named tool.
fn verb_free_config(config: &McpServerConfig) -> McpServerConfig {
    let mut config = config.clone();
    config.routed = None;
    config
}
