// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! `kin agents`: the AI agents working in this repository and what they do
//! through Kin, as the repository's daemon sees them.

use anyhow::Result;

/// `kin agents [--json]`.
pub async fn run(json: bool) -> Result<()> {
    let layout = crate::commands::require_repository_layout()?;
    let client = crate::daemon_client::DaemonClient::connect_for_command("agents", &layout).await?;
    let payload = client.agents().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    print!("{}", render(&payload));
    Ok(())
}

/// One line per agent, active first, as the daemon ordered them.
fn render(payload: &serde_json::Value) -> String {
    let agents = payload
        .get("agents")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if agents.is_empty() {
        return "No agents are working in this repository.\n".to_string();
    }
    let mut out = String::new();
    for agent in agents {
        let text = |field: &str| {
            agent
                .get(field)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("-")
                .to_string()
        };
        let count = |field: &str| {
            agent
                .get(field)
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len)
        };
        let calls = agent
            .get("calls_total")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let last = agent
            .pointer("/recent_calls/0/tool")
            .and_then(serde_json::Value::as_str)
            .map(|tool| format!(", last {tool}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "{client} ({state}, {transport}) {session}: {calls} call{plural}{last}, \
             {transactions} open transaction{tx_plural}, {intents} intent{intent_plural}, \
             {changes} change{change_plural}\n",
            client = text("client"),
            state = text("state"),
            transport = text("transport"),
            session = text("session_id"),
            plural = if calls == 1 { "" } else { "s" },
            transactions = count("open_transactions"),
            tx_plural = if count("open_transactions") == 1 {
                ""
            } else {
                "s"
            },
            intents = count("intents"),
            intent_plural = if count("intents") == 1 { "" } else { "s" },
            changes = count("recent_change_ids"),
            change_plural = if count("recent_change_ids") == 1 {
                ""
            } else {
                "s"
            },
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_agents_says_so() {
        assert_eq!(
            render(&json!({"agents": []})),
            "No agents are working in this repository.\n"
        );
    }

    #[test]
    fn an_agent_is_one_line() {
        let line = render(&json!({"agents": [{
            "session_id": "s-1",
            "client": "claude-code",
            "transport": "mcp",
            "state": "active",
            "calls_total": 3,
            "recent_calls": [{"tool": "impact_analysis"}],
            "open_transactions": [{}],
            "intents": [],
            "recent_change_ids": ["a", "b"],
        }]}));
        assert_eq!(
            line,
            "claude-code (active, mcp) s-1: 3 calls, last impact_analysis, \
             1 open transaction, 0 intents, 2 changes\n"
        );
    }
}
