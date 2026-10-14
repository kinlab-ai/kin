// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! `kin agents record-edit`: what an agent client's edit hook runs after the
//! agent edits code with the client's own tools.
//!
//! Most coding agents change code with their own edit tools and never call a
//! Kin write, so Kin sees the edit with no one's name on it. A client that
//! runs paired hooks around each edit hands them the operation id and what
//! the edit will be. This command
//! reads those on stdin and tells the repository's daemon which session made
//! the edit and what it replaced. The daemon attributes the entities the edit
//! changed only once it has confirmed, against the content it admitted, that
//! the edit was all that changed there.
//!
//! It runs after every edit an agent makes, so it does as little as it can.
//! It reaches only a daemon that is already serving the repository, and never
//! starts one. Before execution it waits for one local preparation request;
//! after execution it sends one request and lets attribution finish behind
//! the answer. It prints nothing, and it never fails: a hook that failed would
//! interrupt the agent over something that is not the agent's concern.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

/// The clients whose edit hook this reads, as `kin setup` names them.
pub const EDIT_HOOK_CLIENTS: [&str; 1] = ["claude"];

/// The arguments after the launcher that make a hook Kin's edit hook.
pub const EDIT_HOOK_ARGUMENTS: &str = "agents record-edit --client claude";

/// How long the request may take before the hook gives up on it.
const SEND_BUDGET: Duration = Duration::from_millis(1500);
/// How long a caller that asked for the outcome waits for it.
const WAIT_BUDGET: Duration = Duration::from_secs(20);

/// The name a client's sessions give Kin when they connect, so an edit and a
/// connection of the same client read as one agent.
fn client_name(client: &str) -> &'static str {
    match client {
        "claude" => "claude-code",
        _ => "unknown",
    }
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn replacement(edit: &Value) -> Option<Value> {
    Some(json!({
        "old_text": text(edit, "old_string")?,
        "new_text": text(edit, "new_string")?,
        "replace_all": edit.get("replace_all").and_then(Value::as_bool).unwrap_or(false),
    }))
}

/// What a hook's input says was edited: where, and the claim to send. `Err`
/// says in words why the input is not an edit this reads.
///
/// Claude Code runs paired `PreToolUse` and `PostToolUse` hooks with the
/// tool's name, input and operation id. Its
/// `Edit` replaces one text with another, `MultiEdit` makes several such
/// replacements in order, and `Write` writes the whole content.
pub fn claim_from_hook(client: &str, raw: &str) -> Result<(PathBuf, Value), String> {
    let input: Value = serde_json::from_str(raw)
        .map_err(|error| format!("the hook input is not JSON: {error}"))?;
    if let Some(event) = text(&input, "hook_event_name") {
        if !matches!(event, "PreToolUse" | "PostToolUse") {
            return Err(format!(
                "the hook ran on {event}, not before or after a tool"
            ));
        }
    }
    let tool = text(&input, "tool_name").ok_or("the hook input names no tool")?;
    let tool_input = input
        .get("tool_input")
        .ok_or("the hook input carries no tool input")?;
    let session = text(&input, "session_id")
        .map(str::trim)
        .filter(|session| !session.is_empty())
        .ok_or("the hook input names no session")?;
    let place = text(tool_input, "file_path")
        .map(str::trim)
        .filter(|place| !place.is_empty())
        .ok_or("the tool input names nothing it edited")?;
    let mut unit = PathBuf::from(place);
    if unit.is_relative() {
        let cwd =
            text(&input, "cwd").ok_or("the tool input names a relative place and no folder")?;
        unit = Path::new(cwd).join(unit);
    }
    let mut claim = json!({
        "client": client_name(client),
        "session": session,
        "phase": if text(&input, "hook_event_name") == Some("PreToolUse") { "before" } else { "after" },
        "tool_use_id": text(&input, "tool_use_id"),
        "unit": unit.to_str().ok_or("the edited place is not readable text")?,
    });
    match tool {
        "Edit" => {
            claim["edits"] =
                json!([replacement(tool_input).ok_or("the edit carries no old and new text")?]);
        }
        "MultiEdit" => {
            let edits: Option<Vec<Value>> = tool_input
                .get("edits")
                .and_then(Value::as_array)
                .ok_or("the edit carries no replacements")?
                .iter()
                .map(replacement)
                .collect();
            claim["edits"] =
                Value::Array(edits.ok_or("a replacement carries no old and new text")?);
        }
        "Write" => {
            claim["content"] =
                json!(text(tool_input, "content").ok_or("the write carries no content")?);
        }
        other => return Err(format!("{other} is not a tool that edits code")),
    }
    Ok((unit, claim))
}

/// Tell the daemon serving the repository that holds `unit` about `claim`.
/// `Ok(None)` when it was sent and no outcome was asked for.
async fn send(unit: &Path, claim: &Value, wait: bool) -> Result<Option<Value>, String> {
    let folder = unit.parent().ok_or("the edit names no folder")?;
    let layout = kin_core::KinLayout::discover(folder).ok_or("the edit is in no Kin repository")?;
    if !kin_core::layout::store_daemon_is_running(layout.root()) {
        return Err("no Kin daemon is serving this repository".into());
    }
    let port = kin_daemon_spawn::read_reported_port(layout.root())
        .ok_or("the repository's daemon has published no port")?;
    let client = crate::daemon_client::DaemonClient::from_base_url_for_layout(
        format!("http://127.0.0.1:{port}"),
        &layout,
    )
    .map_err(|error| format!("{error:#}"))?;
    let before = claim["phase"] == "before";
    let budget = if wait && !before {
        WAIT_BUDGET
    } else {
        SEND_BUDGET
    };
    match tokio::time::timeout(budget, client.record_agent_edit(claim, wait || before)).await {
        Ok(Ok(outcome)) => Ok(wait.then_some(outcome)),
        Ok(Err(error)) => Err(format!("{error:#}")),
        Err(_) => Err("the daemon did not take the edit in time".into()),
    }
}

/// Run one edit hook: read its input and tell the daemon. Returns what to
/// print, which is nothing unless `wait` asked for the outcome. Why it did
/// nothing goes to stderr, which a client does not show for a hook that
/// succeeded.
pub async fn record_edit(client: &str, raw: &str, wait: bool) -> Option<String> {
    let sent = match claim_from_hook(client, raw) {
        Ok((unit, claim)) => send(&unit, &claim, wait).await,
        Err(why) => Err(why),
    };
    match sent {
        Ok(outcome) => outcome.map(|outcome| outcome.to_string()),
        Err(why) => {
            eprintln!("kin agents record-edit: {why}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_a_multi_edit_and_a_write_become_claims() {
        let (unit, claim) = claim_from_hook(
            "claude",
            r#"{"session_id":"s-1","tool_use_id":"tool-1","cwd":"/work/app","hook_event_name":"PostToolUse",
                "tool_name":"Edit","tool_input":{"file_path":"/work/app/shapes.py",
                "old_string":"return w * h","new_string":"return abs(w * h)"},
                "tool_response":{"filePath":"/work/app/shapes.py"}}"#,
        )
        .unwrap();
        assert_eq!(unit, PathBuf::from("/work/app/shapes.py"));
        assert_eq!(
            claim,
            json!({
                "client": "claude-code",
                "session": "s-1",
                "phase": "after",
                "tool_use_id": "tool-1",
                "unit": "/work/app/shapes.py",
                "edits": [{
                    "old_text": "return w * h",
                    "new_text": "return abs(w * h)",
                    "replace_all": false,
                }],
            })
        );

        let (_, many) = claim_from_hook(
            "claude",
            r#"{"session_id":"s-1","cwd":"/work/app","tool_name":"MultiEdit",
                "tool_input":{"file_path":"shapes.py","edits":[
                {"old_string":"a","new_string":"b"},
                {"old_string":"c","new_string":"d","replace_all":true}]}}"#,
        )
        .unwrap();
        assert_eq!(many["unit"], "/work/app/shapes.py", "placed in its folder");
        assert_eq!(many["edits"].as_array().unwrap().len(), 2);
        assert_eq!(many["edits"][1]["replace_all"], true);

        let (_, whole) = claim_from_hook(
            "claude",
            r#"{"session_id":"s-1","tool_name":"Write",
                "tool_input":{"file_path":"/work/app/new.py","content":"x = 1\n"}}"#,
        )
        .unwrap();
        assert_eq!(whole["content"], "x = 1\n");
        assert!(whole.get("edits").is_none());
    }

    #[test]
    fn anything_that_is_not_an_edit_is_refused_in_words() {
        for (raw, why) in [
            ("not json", "not JSON"),
            (
                r#"{"session_id":"s","tool_name":"Bash","tool_input":{"file_path":"/a","command":"ls"}}"#,
                "not a tool that edits code",
            ),
            (
                r#"{"session_id":"s","hook_event_name":"PostToolUseFailure","tool_name":"Edit","tool_input":{}}"#,
                "not before or after a tool",
            ),
            (
                r#"{"tool_name":"Edit","tool_input":{"file_path":"/a","old_string":"a","new_string":"b"}}"#,
                "no session",
            ),
            (
                r#"{"session_id":"s","tool_name":"Edit","tool_input":{"file_path":"/a"}}"#,
                "no old and new text",
            ),
            (
                r#"{"session_id":"s","tool_name":"Edit","tool_input":{"file_path":"a.py","old_string":"a","new_string":"b"}}"#,
                "no folder",
            ),
        ] {
            let refused = claim_from_hook("claude", raw).unwrap_err();
            assert!(refused.contains(why), "{refused}");
        }
    }
}
