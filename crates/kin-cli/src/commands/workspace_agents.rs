// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The agents working in this repository, as the workspace page shows them.
//!
//! Everything here is read from the repository daemon's own record of agent
//! sessions: `/agents`, `/session/{id}/calls` and the `ToolCall` events on
//! `/graph/events`. The page is handed names a reader can use. An agent is
//! "Claude Code", never its session id. An entity is its name, never its place
//! on disk. A call is the tool the agent used and the entities it was about,
//! never the words it asked with, which the daemon does not keep.
//!
//! The daemon forgets a session when it ends. This server remembers the
//! sessions it has seen while it runs, with their last call log, so the page
//! can still show what an agent did after it disconnected.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use super::review_ui::UiState;
use crate::daemon_client::DaemonClient;

/// How long one read of the daemon may take before the page is told so.
const READ_TIMEOUT: Duration = Duration::from_secs(8);
/// How long naming one entity may take. A slow one is left unnamed.
const NAME_TIMEOUT: Duration = Duration::from_secs(4);
const NAME_CONCURRENCY: usize = 6;
/// The most entities one answer looks up names for. The rest are counted.
const NAMES_PER_READ: usize = 48;
const NAMES_KEPT: usize = 4096;
/// The most agent sessions this server remembers, live and ended.
const AGENTS_KEPT: usize = 24;
const EARLIER_SHOWN: usize = 8;
/// The most calls remembered for one session, and the most a timeline shows.
const CALLS_KEPT: usize = 200;
const TIMELINE_DEFAULT: usize = 100;
/// The entities of a call that are named for the page; the rest are counted.
const NAMED_PER_CALL: usize = 4;
const IDS_PER_CALL: usize = 16;
const CHANGES_SHOWN: usize = 5;
const CHANGES_KEPT: usize = 16;
const LOOKED_AT_SHOWN: usize = 12;
/// How many of the newest recorded changes are read to title an agent's change.
const CHANGE_LOOKUP: usize = 100;
const CHANGE_ROWS_KEPT: usize = 512;
/// Live streams this server relays at once. Each holds one daemon stream.
const STREAMS_MAX: usize = 4;
const PULSE_EVERY: Duration = Duration::from_millis(400);
/// Entity changes are folded into one pulse per this many ticks.
const ENTITY_PULSE_TICKS: u32 = 3;
/// A comment line keeps a quiet stream open through proxies and browsers.
const KEEPALIVE_TICKS: u32 = 50;
const STREAM_BUFFER_MAX: usize = 1 << 20;
/// What the browser waits before it opens the stream again, in milliseconds.
const STREAM_RETRY_MS: u64 = 5000;

pub(super) fn routes() -> Router<Arc<UiState>> {
    Router::new()
        .route("/api/workspace/agents", get(serve_agents))
        .route("/api/workspace/agents/timeline", get(serve_timeline))
        .route("/api/workspace/events", get(serve_events))
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

/// The name a reader knows an agent by, from the vendor and client name its
/// connection gave. The daemon lowercases the client name and joins its words
/// with hyphens to make the vendor.
pub(super) fn agent_display_name(vendor: &str, client: &str) -> String {
    known_agent(vendor)
        .or_else(|| known_agent(client))
        .map(str::to_string)
        .or_else(|| titled(client))
        .or_else(|| titled(vendor))
        .unwrap_or_else(|| "Unnamed agent".to_string())
}

fn known_agent(raw: &str) -> Option<&'static str> {
    let normalized = raw.trim().to_ascii_lowercase().replace([' ', '_'], "-");
    Some(match normalized.as_str() {
        "claude-code" => "Claude Code",
        "claude-ai" | "claude-desktop" | "claude" => "Claude",
        "codex" | "codex-cli" | "codex-mcp-client" | "openai-codex" => "Codex",
        "cursor" | "cursor-vscode" | "cursor-agent" => "Cursor",
        "windsurf" | "windsurf-client" => "Windsurf",
        "gemini" | "gemini-cli" | "gemini-cli-mcp-client" => "Gemini CLI",
        "visual-studio-code" | "vscode" | "github-copilot" | "copilot" => "GitHub Copilot",
        "zed" => "Zed",
        "cline" => "Cline",
        "goose" => "Goose",
        "opencode" => "OpenCode",
        "continue" => "Continue",
        "grok" | "grok-cli" => "Grok",
        _ => return None,
    })
}

/// A client name a reader can be shown, in title case. Nothing that is a
/// placeholder, an id or a place on disk.
fn titled(raw: &str) -> Option<String> {
    let text = raw.trim();
    if text.is_empty()
        || text.len() > 48
        || matches!(
            text.to_ascii_lowercase().as_str(),
            "unknown" | "unnamed client" | "mcp-client" | "mcp client"
        )
        || uuid::Uuid::parse_str(text).is_ok()
        || path_shaped(text)
        || text.chars().any(char::is_control)
    {
        return None;
    }
    let words: Vec<String> = text
        .split(['-', '_', ' '])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut letters = word.chars();
            match letters.next() {
                Some(first) => first.to_uppercase().chain(letters).collect(),
                None => String::new(),
            }
        })
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

/// A short, stable handle for a session that does not show its id.
pub(super) fn agent_key(session: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"kin-workspace-agent\0");
    digest.update(session.trim().to_ascii_lowercase().as_bytes());
    hex::encode(&digest.finalize()[..8])
}

/// An agent named as a change's author: `{vendor}/{client} <mcp-agent:{session}>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AgentAuthor {
    pub(super) name: String,
    pub(super) session: String,
}

pub(super) fn agent_author(raw: &str) -> Option<AgentAuthor> {
    const MARK: &str = "<mcp-agent:";
    let text = raw.trim();
    let open = text.rfind(MARK)?;
    let session = text[open + MARK.len()..].strip_suffix('>')?.trim();
    if session.is_empty() || session.contains(['<', '>']) {
        return None;
    }
    let label = text[..open].trim();
    let name = if label.is_empty() || label.eq_ignore_ascii_case(session) {
        "Unnamed agent".to_string()
    } else {
        let (vendor, client) = label.split_once('/').unwrap_or((label, ""));
        agent_display_name(vendor, client)
    };
    Some(AgentAuthor {
        name,
        session: session.to_ascii_lowercase(),
    })
}

/// The agent a change came from, as the page carries it.
pub(super) fn agent_ref(raw_author: &str) -> Option<Value> {
    agent_author(raw_author)
        .map(|author| json!({ "name": author.name, "key": agent_key(&author.session) }))
}

/// Whether a name reads as a place on disk rather than as code.
fn path_shaped(text: &str) -> bool {
    if text.contains(['/', '\\']) {
        return true;
    }
    let extension = text
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    matches!(
        extension.as_deref(),
        Some(
            "py" | "pyi"
                | "rs"
                | "ts"
                | "tsx"
                | "js"
                | "jsx"
                | "mjs"
                | "cjs"
                | "go"
                | "java"
                | "kt"
                | "rb"
                | "php"
                | "c"
                | "h"
                | "cc"
                | "cpp"
                | "hpp"
                | "cs"
                | "swift"
                | "md"
                | "rst"
                | "toml"
                | "json"
                | "yaml"
                | "yml"
                | "txt"
                | "cfg"
                | "ini"
                | "lock"
                | "sh"
        )
    )
}

/// An entity as the page names it: `Signer.sign` for a method of a class,
/// otherwise its own name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Named {
    label: String,
    kind: String,
}

fn named_from(source: &Value) -> Option<Named> {
    let name = source["name"].as_str()?.trim();
    if name.is_empty() || path_shaped(name) {
        return None;
    }
    let kind = super::workspace_explore::kind_label(source["kind"].as_str().unwrap_or_default());
    let owner = source["semantic_owners"]
        .as_array()
        .and_then(|owners| owners.first());
    let label = match owner {
        Some(owner) if is_member(&kind) && !name.contains('.') && !name.contains("::") => {
            let owner_name = owner["name"].as_str().unwrap_or_default().trim();
            let owner_kind =
                super::workspace_explore::kind_label(owner["kind"].as_str().unwrap_or_default());
            if !owner_name.is_empty() && !path_shaped(owner_name) && is_type_like(&owner_kind) {
                format!("{owner_name}.{name}")
            } else {
                name.to_string()
            }
        }
        _ => name.to_string(),
    };
    Some(Named { label, kind })
}

fn is_member(kind: &str) -> bool {
    matches!(
        kind,
        "method" | "function" | "property" | "field" | "constant" | "attribute" | "variable"
    )
}

fn is_type_like(kind: &str) -> bool {
    matches!(
        kind,
        "class" | "struct" | "trait" | "trait_def" | "interface" | "enum" | "impl" | "type"
    )
}

// ---------------------------------------------------------------------------
// What this server remembers
// ---------------------------------------------------------------------------

/// The agent sessions this page server has seen, with the names it has looked
/// up. Held for the life of the server; bounded in every dimension.
#[derive(Default)]
pub(crate) struct AgentMemory {
    inner: Mutex<Memory>,
    streams: AtomicUsize,
}

#[derive(Default)]
struct Memory {
    names: HashMap<String, Option<Named>>,
    agents: HashMap<String, Seen>,
    /// Which session committed a change, from `/agents` and from authors.
    change_owner: HashMap<String, String>,
    /// The name an author line gives a session the daemon no longer lists.
    author_names: HashMap<String, String>,
    change_rows: HashMap<String, Value>,
}

#[derive(Debug, Clone)]
struct Seen {
    session: String,
    name: String,
    transport: String,
    started_at: String,
    last_activity_at: String,
    live: bool,
    last_listed: Instant,
    calls_total: u64,
    /// Newest first, by the call's position in its session.
    calls: BTreeMap<u64, Value>,
    /// Newest first.
    changes: Vec<String>,
    /// The daemon's last answer for this session while it was live.
    snapshot: Value,
}

impl AgentMemory {
    fn lock(&self) -> std::sync::MutexGuard<'_, Memory> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

fn seq(call: &Value) -> u64 {
    call["seq"].as_u64().unwrap_or(0)
}

/// One call as the page may see it: what tool, about which entities, how many
/// entities the answer named, and whether it was refused. Nothing else of the
/// daemon's entry is passed on.
fn call_json(call: &Value) -> Option<Value> {
    let seq = call["seq"].as_u64()?;
    let tool = tool_name(text(&call["tool"]));
    let ids: Vec<&str> = call["entity_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .take(IDS_PER_CALL)
        .collect();
    Some(json!({
        "seq": seq,
        "at": text(&call["at"]),
        "tool": tool,
        "entity_ids": ids,
        "result_count": call["result_count"].as_u64().unwrap_or(0),
        "duration_ms": call["duration_ms"].as_u64().unwrap_or(0),
        "is_error": call["is_error"].as_bool().unwrap_or(false),
    }))
}

/// A registered tool's name, which is an identifier; anything else is not
/// passed on.
fn tool_name(raw: &str) -> &str {
    if !raw.is_empty()
        && raw.len() <= 64
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        raw
    } else {
        "unknown"
    }
}

fn remember_calls(seen: &mut Seen, calls: &[Value]) {
    for call in calls.iter().filter_map(call_json) {
        seen.calls.insert(seq(&call), call);
    }
    while seen.calls.len() > CALLS_KEPT {
        let oldest = *seen.calls.keys().next().expect("non-empty");
        seen.calls.remove(&oldest);
    }
}

fn remember_changes(memory: &mut Memory, session: &str, changes: &[String]) {
    if let Some(seen) = memory.agents.get_mut(session) {
        let mut merged: Vec<String> = changes.to_vec();
        for change in &seen.changes {
            if !merged.contains(change) {
                merged.push(change.clone());
            }
        }
        merged.truncate(CHANGES_KEPT);
        seen.changes = merged;
    }
    for change in changes {
        memory
            .change_owner
            .insert(change.clone(), session.to_string());
    }
}

/// Attribution follows the history this server still holds, rather than
/// accumulating every change and author it has ever seen.
fn prune_attribution(memory: &mut Memory) {
    let changes: HashSet<&String> = memory
        .agents
        .values()
        .flat_map(|seen| seen.changes.iter())
        .chain(memory.change_rows.keys())
        .collect();
    memory
        .change_owner
        .retain(|change, _| changes.contains(change));
    let sessions: HashSet<&String> = memory
        .agents
        .keys()
        .chain(memory.change_owner.values())
        .collect();
    memory
        .author_names
        .retain(|session, _| sessions.contains(session));
}

/// Take in one `/agents` answer. Returns the live sessions in the daemon's
/// order.
fn observe(memory: &AgentMemory, payload: &Value) -> Vec<String> {
    let mut memory = memory.lock();
    let now = Instant::now();
    let mut live = Vec::new();
    for agent in payload["agents"].as_array().into_iter().flatten() {
        let Some(session) = agent["session_id"].as_str().map(str::to_ascii_lowercase) else {
            continue;
        };
        let name = agent_display_name(text(&agent["vendor"]), text(&agent["client"]));
        let seen = memory
            .agents
            .entry(session.clone())
            .or_insert_with(|| Seen {
                session: session.clone(),
                name: name.clone(),
                transport: String::new(),
                started_at: String::new(),
                last_activity_at: String::new(),
                live: true,
                last_listed: now,
                calls_total: 0,
                calls: BTreeMap::new(),
                changes: Vec::new(),
                snapshot: Value::Null,
            });
        seen.name = name;
        seen.transport = text(&agent["transport"]).to_string();
        seen.started_at = text(&agent["started_at"]).to_string();
        seen.last_activity_at = text(&agent["last_activity_at"]).to_string();
        seen.live = true;
        seen.last_listed = now;
        seen.calls_total = agent["calls_total"].as_u64().unwrap_or(seen.calls_total);
        remember_calls(
            seen,
            agent["recent_calls"]
                .as_array()
                .map_or(&[][..], Vec::as_slice),
        );
        seen.snapshot = json!({
            "state": agent["state"],
            "calls_in_flight": agent["calls_in_flight"],
            "open_transactions": agent["open_transactions"],
            "intents": agent["intents"],
        });
        let changes: Vec<String> = agent["recent_change_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|id| is_change_id(id))
            .map(str::to_ascii_lowercase)
            .collect();
        remember_changes(&mut memory, &session, &changes);
        live.push(session);
    }
    for (session, seen) in memory.agents.iter_mut() {
        if !live.contains(session) {
            seen.live = false;
        }
    }
    while memory.agents.len() > AGENTS_KEPT {
        let Some(oldest) = memory
            .agents
            .values()
            .filter(|seen| !seen.live)
            .min_by_key(|seen| seen.last_listed)
            .map(|seen| seen.session.clone())
        else {
            break;
        };
        memory.agents.remove(&oldest);
    }
    prune_attribution(&mut memory);
    live
}

fn is_change_id(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------------
// Reading names and changes
// ---------------------------------------------------------------------------

/// Names for the entities a page will show, looked up once each and then
/// remembered. An entity whose read fails or that has no name a reader can be
/// shown is left out, and the page counts it instead of naming it.
async fn names_for(memory: &AgentMemory, client: &DaemonClient, wanted: &[String]) -> Value {
    let mut order = Vec::new();
    let mut unique = HashSet::new();
    for id in wanted {
        if unique.insert(id.clone()) {
            order.push(id.clone());
        }
    }
    let missing: Vec<String> = {
        let memory = memory.lock();
        order
            .iter()
            .filter(|id| !memory.names.contains_key(*id))
            .take(NAMES_PER_READ)
            .cloned()
            .collect()
    };
    let gate = Arc::new(tokio::sync::Semaphore::new(NAME_CONCURRENCY));
    let mut reads = tokio::task::JoinSet::new();
    for id in missing {
        let client = client.clone();
        let gate = gate.clone();
        reads.spawn(async move {
            let _permit = gate.acquire_owned().await.ok()?;
            match tokio::time::timeout(NAME_TIMEOUT, client.workspace_entity_source(&id)).await {
                Ok(Ok(source)) => Some((id, named_from(&source))),
                // A read that answered with a refusal names nothing to show.
                Ok(Err(_)) => Some((id, None)),
                // A slow read is asked again next time.
                Err(_) => None,
            }
        });
    }
    let mut found = Vec::new();
    while let Some(read) = reads.join_next().await {
        if let Ok(Some(entry)) = read {
            found.push(entry);
        }
    }
    let mut memory = memory.lock();
    if memory.names.len() + found.len() > NAMES_KEPT {
        memory.names.clear();
    }
    for (id, named) in found {
        memory.names.insert(id, named);
    }
    let mut names = Map::new();
    for id in order {
        if let Some(Some(named)) = memory.names.get(&id) {
            names.insert(id, json!({ "label": named.label, "kind": named.kind }));
        }
    }
    Value::Object(names)
}

/// Rows for changes an agent made: title, time and how many entities moved.
/// Read from the newest recorded changes, once; a change's row never changes.
async fn change_rows(memory: &AgentMemory, client: &DaemonClient, ids: &[String]) -> Vec<Value> {
    let missing = {
        let memory = memory.lock();
        ids.iter().any(|id| !memory.change_rows.contains_key(id))
    };
    if missing {
        let request = super::log::LogRequest {
            count: CHANGE_LOOKUP,
            skip: 0,
        };
        if let Ok(Ok(response)) = tokio::time::timeout(READ_TIMEOUT, client.log(&request)).await {
            if let Some(report) = response.report {
                let mut memory = memory.lock();
                if memory.change_rows.len() > CHANGE_ROWS_KEPT {
                    memory.change_rows.clear();
                }
                for entry in report.entries {
                    let id = entry.change_id.to_string().to_ascii_lowercase();
                    let author = entry.author.to_string();
                    if let Some(agent) = agent_author(&author) {
                        memory
                            .change_owner
                            .entry(id.clone())
                            .or_insert_with(|| agent.session.clone());
                        memory
                            .author_names
                            .entry(agent.session.clone())
                            .or_insert(agent.name);
                    }
                    let row = json!({
                        "id": id,
                        "title": entry.message.lines().next().unwrap_or_default().trim(),
                        "time": entry.timestamp.to_string(),
                        "entities_changed": entry.entity_delta_count,
                    });
                    memory.change_rows.insert(id, row);
                }
                prune_attribution(&mut memory);
            }
        }
    }
    let memory = memory.lock();
    ids.iter()
        .filter_map(|id| memory.change_rows.get(id).cloned())
        .collect()
}

// ---------------------------------------------------------------------------
// The agents as the page shows them
// ---------------------------------------------------------------------------

fn state_of(seen: &Seen) -> &'static str {
    if !seen.live {
        "ended"
    } else if seen.snapshot["state"] == "active" {
        "active"
    } else {
        "idle"
    }
}

fn open_changes(snapshot: &Value) -> Vec<Value> {
    snapshot["open_transactions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|transaction| {
            json!({
                "staged_edits": transaction["staged_operations"].as_u64().unwrap_or(0),
                "age_secs": transaction["age_secs"],
            })
        })
        .collect()
}

/// What an agent said it is working on, with the entities it holds. A task is
/// the agent's own description, already cut short and cleared of places on
/// disk by the daemon; this checks again.
fn intents(snapshot: &Value, scope_names: &mut Map<String, Value>) -> Vec<Value> {
    snapshot["intents"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|intent| {
            let mut ids = Vec::new();
            let mut others = intent["withheld_scopes"].as_u64().unwrap_or(0);
            for scope in intent["scopes"].as_array().into_iter().flatten() {
                let id = text(&scope["id"]);
                if scope["kind"] == "entity" && uuid::Uuid::parse_str(id).is_ok() {
                    ids.push(id.to_string());
                    let name = text(&scope["name"]);
                    if !name.is_empty() && !path_shaped(name) && !scope_names.contains_key(id) {
                        scope_names.insert(id.to_string(), json!({ "label": name, "kind": "" }));
                    }
                } else {
                    others += 1;
                }
            }
            let task: String = text(&intent["task"])
                .split_whitespace()
                .map(|word| if path_shaped(word) { "\u{2026}" } else { word })
                .collect::<Vec<_>>()
                .join(" ");
            json!({
                "task": super::workspace_explore::plain(&task),
                "entity_ids": ids,
                "other_scopes": others,
            })
        })
        .collect()
}

fn agent_json(seen: &Seen, calls_shown: usize, changes: Vec<Value>) -> Value {
    let live = seen.live;
    let mut scope_names = Map::new();
    let intents = if live {
        intents(&seen.snapshot, &mut scope_names)
    } else {
        Vec::new()
    };
    json!({
        "key": agent_key(&seen.session),
        "name": seen.name,
        "state": state_of(seen),
        "busy": live && seen.snapshot["calls_in_flight"].as_u64().unwrap_or(0) > 0,
        "through": match seen.transport.as_str() {
            "mcp" => "mcp",
            "cli" | "wrapper" => "terminal",
            _ => "other",
        },
        "started_at": seen.started_at,
        "last_activity_at": seen.last_activity_at,
        "calls_total": seen.calls_total,
        "recent_calls": seen.calls.values().rev().take(calls_shown).cloned().collect::<Vec<_>>(),
        "open_changes": if live { open_changes(&seen.snapshot) } else { Vec::new() },
        "intents": intents,
        "scope_names": scope_names,
        "changes": changes,
        "changes_total": seen.changes.len(),
    })
}

/// The entities of the shown calls and intents that the page names.
fn wanted_ids(agent: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    for call in agent["recent_calls"].as_array().into_iter().flatten() {
        for id in call["entity_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .take(NAMED_PER_CALL)
        {
            ids.push(text(id).to_string());
        }
    }
    for intent in agent["intents"].as_array().into_iter().flatten() {
        for id in intent["entity_ids"].as_array().into_iter().flatten() {
            ids.push(text(id).to_string());
        }
    }
    ids
}

/// Names from intent scopes fill in where a lookup found none.
fn merge_names(names: &mut Value, agents: &[Value]) {
    let Some(names) = names.as_object_mut() else {
        return;
    };
    for agent in agents {
        if let Some(scopes) = agent["scope_names"].as_object() {
            for (id, named) in scopes {
                names.entry(id.clone()).or_insert_with(|| named.clone());
            }
        }
    }
}

fn unavailable(status: StatusCode, message: &str) -> Response {
    let mut response = Json(json!({ "error": message })).into_response();
    *response.status_mut() = status;
    response
}

fn read_at() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

async fn read_agents(state: &UiState, client: &DaemonClient) -> Result<(Value, Vec<String>)> {
    let payload = tokio::time::timeout(READ_TIMEOUT, client.agents())
        .await
        .context("Kin took too long to say which agents are connected")??;
    anyhow::ensure!(
        payload["agents"].is_array(),
        "Kin's answer about connected agents was not readable"
    );
    let live = observe(&state.agents, &payload);
    Ok((payload, live))
}

async fn serve_agents(State(state): State<Arc<UiState>>) -> Response {
    let answer = async {
        let client = super::review_ui::daemon(&state).await?;
        let (payload, live) = read_agents(&state, &client).await?;
        let (live_seen, earlier_seen): (Vec<Seen>, Vec<Seen>) = {
            let memory = state.agents.lock();
            let live_seen = live
                .iter()
                .filter_map(|session| memory.agents.get(session).cloned())
                .collect();
            let mut earlier: Vec<Seen> = memory
                .agents
                .values()
                .filter(|seen| !seen.live)
                .cloned()
                .collect();
            earlier.sort_by(|a, b| b.last_listed.cmp(&a.last_listed));
            earlier.truncate(EARLIER_SHOWN);
            (live_seen, earlier)
        };
        let mut agents = Vec::new();
        for seen in &live_seen {
            let shown: Vec<String> = seen.changes.iter().take(CHANGES_SHOWN).cloned().collect();
            let rows = change_rows(&state.agents, &client, &shown).await;
            agents.push(agent_json(seen, 5, rows));
        }
        let mut earlier = Vec::new();
        for seen in &earlier_seen {
            let shown: Vec<String> = seen.changes.iter().take(CHANGES_SHOWN).cloned().collect();
            let rows = change_rows(&state.agents, &client, &shown).await;
            earlier.push(agent_json(seen, 3, rows));
        }
        let wanted: Vec<String> = agents
            .iter()
            .chain(earlier.iter())
            .flat_map(wanted_ids)
            .collect();
        let mut names = names_for(&state.agents, &client, &wanted).await;
        merge_names(&mut names, &agents);
        Ok::<_, anyhow::Error>(json!({
            "schema": "kin.workspace.agents.v1",
            "read_at": read_at(),
            "active_window_secs": payload["active_window_secs"],
            "agents": agents,
            "earlier": earlier,
            "names": names,
        }))
    }
    .await;
    match answer {
        Ok(body) => Json(body).into_response(),
        Err(error) => unavailable(
            StatusCode::BAD_GATEWAY,
            &super::workspace_explore::plain(&format!(
                "Couldn't read which agents are connected: {error:#}"
            )),
        ),
    }
}

#[derive(Deserialize)]
struct TimelineQuery {
    agent: String,
    #[serde(default)]
    limit: Option<usize>,
}

fn session_for_key(memory: &AgentMemory, key: &str) -> Option<Seen> {
    memory
        .lock()
        .agents
        .values()
        .find(|seen| agent_key(&seen.session) == key)
        .cloned()
}

fn valid_key(key: &str) -> bool {
    key.len() == 16 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn serve_timeline(
    State(state): State<Arc<UiState>>,
    Query(query): Query<TimelineQuery>,
) -> Response {
    if !valid_key(&query.agent) {
        return unavailable(
            StatusCode::BAD_REQUEST,
            "That is not an agent this page knows.",
        );
    }
    let limit = query.limit.unwrap_or(TIMELINE_DEFAULT).clamp(1, CALLS_KEPT);
    let client = match super::review_ui::daemon(&state).await {
        Ok(client) => client,
        Err(error) => {
            return unavailable(
                StatusCode::BAD_GATEWAY,
                &format!("Couldn't reach this repository: {error:#}"),
            )
        }
    };
    // The agents read refreshes what this server remembers; when it fails the
    // timeline is still drawn from what was remembered.
    let listed = read_agents(&state, &client).await.is_ok();
    let Some(seen) = session_for_key(&state.agents, &query.agent) else {
        return unavailable(
            StatusCode::NOT_FOUND,
            "Kin has no record of this agent any more. Its session ended before this page saw it.",
        );
    };
    if seen.live {
        if let Ok(Ok(calls)) = tokio::time::timeout(
            READ_TIMEOUT,
            client.session_calls(&seen.session, Some(limit)),
        )
        .await
        {
            let mut memory = state.agents.lock();
            if let Some(kept) = memory.agents.get_mut(&seen.session) {
                kept.calls_total = calls["calls_total"].as_u64().unwrap_or(kept.calls_total);
                remember_calls(
                    kept,
                    calls["calls"].as_array().map_or(&[][..], Vec::as_slice),
                );
            }
        }
    }
    let seen = session_for_key(&state.agents, &query.agent).unwrap_or(seen);
    let rows = change_rows(&state.agents, &client, &seen.changes).await;
    let mut agent = agent_json(&seen, limit, rows);
    let calls = agent["recent_calls"].take();
    let oldest_kept = seen.calls.keys().next().copied();
    let mut wanted = Vec::new();
    for call in calls.as_array().into_iter().flatten() {
        for id in call["entity_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .take(NAMED_PER_CALL)
        {
            wanted.push(text(id).to_string());
        }
    }
    wanted.extend(wanted_ids(&agent));
    let mut names = names_for(&state.agents, &client, &wanted).await;
    merge_names(&mut names, std::slice::from_ref(&agent));
    Json(json!({
        "schema": "kin.workspace.agent-timeline.v1",
        "read_at": read_at(),
        "listed": listed,
        "agent": agent,
        "calls": calls,
        "kept": seen.calls.len(),
        "earliest_kept_from_start": oldest_kept.is_some_and(|seq| seq <= 1),
        "names": names,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// The agent behind a change, for the guided review
// ---------------------------------------------------------------------------

/// The entities an agent read before it committed a change: every entity its
/// read calls named after its previous commit and before this one.
#[derive(Debug, PartialEq, Eq)]
struct LookedAt {
    ids: Vec<String>,
    /// Whether the remembered calls reach back to that previous commit, or to
    /// the session's first call. When they do not, the count is a floor.
    complete: bool,
}

fn is_read(tool: &str) -> bool {
    !(tool.starts_with("kin_transaction")
        || tool.starts_with("kin_session")
        || matches!(
            tool,
            "kin_mutate" | "mutate" | "transaction" | "unknown" | "unrecognized_tool"
        ))
}

fn looked_at(calls: &BTreeMap<u64, Value>, changes: &[String], change: &str) -> Option<LookedAt> {
    let index = changes.iter().position(|known| known == change)?;
    let commits: Vec<u64> = calls
        .values()
        .rev()
        .filter(|call| call["tool"] == "kin_transaction_commit" && call["is_error"] != true)
        .map(seq)
        .collect();
    let commit = *commits.get(index)?;
    let previous = commits.get(index + 1).copied();
    let oldest = calls.keys().next().copied().unwrap_or(commit);
    let complete = previous.is_some() || oldest <= 1;
    // What each call was about comes first, in the order the agent asked;
    // then the other entities its answers named.
    let reads: Vec<&Value> = calls
        .range(previous.map_or(0, |p| p + 1)..commit)
        .map(|(_, call)| call)
        .filter(|call| is_read(text(&call["tool"])))
        .collect();
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    let ids_of = |call: &Value| -> Vec<String> {
        call["entity_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|id| text(id).to_string())
            .collect()
    };
    for id in reads
        .iter()
        .filter_map(|call| ids_of(call).into_iter().next())
    {
        if seen.insert(id.clone()) {
            ids.push(id);
        }
    }
    for id in reads.iter().flat_map(|call| ids_of(call)) {
        if seen.insert(id.clone()) {
            ids.push(id);
        }
    }
    Some(LookedAt { ids, complete })
}

#[derive(Deserialize)]
pub(super) struct ChangeAgentQuery {
    change: String,
}

/// `GET /api/change-agent?change=<id>`: which agent made a change and what it
/// read first, when this server or the daemon still holds that session.
pub(super) async fn serve_change_agent(
    State(state): State<Arc<UiState>>,
    Query(query): Query<ChangeAgentQuery>,
) -> Response {
    let change = query.change.trim().to_ascii_lowercase();
    if !is_change_id(&change) {
        return unavailable(StatusCode::BAD_REQUEST, "That is not a recorded change.");
    }
    let client = match super::review_ui::daemon(&state).await {
        Ok(client) => client,
        Err(error) => {
            return unavailable(
                StatusCode::BAD_GATEWAY,
                &format!("Couldn't reach this repository: {error:#}"),
            )
        }
    };
    let _ = read_agents(&state, &client).await;
    let mut owner = state.agents.lock().change_owner.get(&change).cloned();
    if owner.is_none() {
        let _ = change_rows(&state.agents, &client, std::slice::from_ref(&change)).await;
        owner = state.agents.lock().change_owner.get(&change).cloned();
    }
    let Some(session) = owner else {
        return Json(json!({ "schema": "kin.workspace.change-agent.v1", "agent": null }))
            .into_response();
    };
    let seen = state.agents.lock().agents.get(&session).cloned();
    let Some(seen) = seen else {
        let name = state
            .agents
            .lock()
            .author_names
            .get(&session)
            .cloned()
            .unwrap_or_else(|| "Unnamed agent".to_string());
        return Json(json!({
            "schema": "kin.workspace.change-agent.v1",
            "agent": { "name": name, "key": agent_key(&session), "state": "unknown" },
            "looked_at": null,
        }))
        .into_response();
    };
    if seen.live {
        if let Ok(Ok(calls)) = tokio::time::timeout(
            READ_TIMEOUT,
            client.session_calls(&session, Some(CALLS_KEPT)),
        )
        .await
        {
            let mut memory = state.agents.lock();
            if let Some(kept) = memory.agents.get_mut(&session) {
                remember_calls(
                    kept,
                    calls["calls"].as_array().map_or(&[][..], Vec::as_slice),
                );
            }
        }
    }
    let seen = state
        .agents
        .lock()
        .agents
        .get(&session)
        .cloned()
        .unwrap_or(seen);
    let looked = looked_at(&seen.calls, &seen.changes, &change);
    let (looked_json, names) = match looked {
        Some(looked) => {
            let shown: Vec<String> = looked.ids.iter().take(LOOKED_AT_SHOWN).cloned().collect();
            let names = names_for(&state.agents, &client, &shown).await;
            (
                json!({ "count": looked.ids.len(), "complete": looked.complete, "entity_ids": shown }),
                names,
            )
        }
        None => (Value::Null, json!({})),
    };
    Json(json!({
        "schema": "kin.workspace.change-agent.v1",
        "agent": { "name": seen.name, "key": agent_key(&session), "state": state_of(&seen) },
        "looked_at": looked_json,
        "names": names,
    }))
    .into_response()
}

/// The guide's change, attributed for the page: an agent's change names the
/// agent as the reader knows it, and its session id stays off the page.
pub(super) fn attribute_change(change: &mut Value) {
    let recorded = change["author_recorded"]
        .as_str()
        .or_else(|| change["author"].as_str())
        .map(str::to_string);
    let Some(author) = recorded.as_deref().and_then(agent_author) else {
        return;
    };
    change["author"] = Value::String(author.name.clone());
    change["author_recorded"] = Value::Null;
    change["agent"] = json!({ "name": author.name, "key": agent_key(&author.session) });
    // An agent's commit records the transaction it came from on its own line.
    // That id means nothing to a reader, so the page's message leaves it out.
    if let Some(message) = change["message"].as_str() {
        let kept: Vec<&str> = message
            .lines()
            .filter(|line| !is_transaction_line(line))
            .collect();
        change["message"] = Value::String(kept.join("\n").trim_end().to_string());
    }
}

fn is_transaction_line(line: &str) -> bool {
    line.trim()
        .strip_prefix("MCP transaction ")
        .is_some_and(|id| uuid::Uuid::parse_str(id.trim()).is_ok())
}

// ---------------------------------------------------------------------------
// The live stream
// ---------------------------------------------------------------------------

/// Hands back its stream slot when the relay ends, however it ends.
struct StreamSlot(Arc<UiState>);

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.0.agents.streams.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The daemon's stream, cut down to what the page may see and slowed to what
/// it needs: a pulse that an agent did something, one when an agent committed
/// a change, and one when entities changed.
struct Relay {
    response: reqwest::Response,
    buffer: Vec<u8>,
    out: std::collections::VecDeque<String>,
    pending_agent: Option<Value>,
    pending_entities: Option<(u64, Option<String>)>,
    ticker: tokio::time::Interval,
    ticks: u32,
    ended: bool,
    _slot: StreamSlot,
}

fn frame(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// The page's form of one daemon event, or nothing when the page has no use
/// for it. Session ids become agent keys; no place on disk is passed on.
enum Translated {
    Agent(Value),
    Change(Value),
    Entities(Option<String>),
    Resync,
    Nothing,
}

fn translate(payload: &Value) -> Translated {
    let key_of = |session: &Value| session.as_str().map(agent_key);
    match text(&payload["type"]) {
        "ToolCall" => Translated::Agent(json!({
            "agent": key_of(&payload["session_id"]),
            "tool": tool_name(text(&payload["tool"])),
            "at": text(&payload["at"]),
        })),
        "Coordination" => {
            let event = &payload["event"];
            let agent = key_of(&event["session_id"]);
            if event["event"] == "transaction_outcome" && event["outcome"] == "committed" {
                let change = text(&event["change_id"]).to_ascii_lowercase();
                Translated::Change(json!({
                    "agent": agent,
                    "change": is_change_id(&change).then_some(change),
                }))
            } else {
                Translated::Agent(json!({ "agent": agent }))
            }
        }
        "EntityChanged" => Translated::Entities(key_of(&payload["session_id"])),
        "lagged" => Translated::Resync,
        _ => Translated::Nothing,
    }
}

impl Relay {
    fn take(&mut self, payload: &str) {
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            return;
        };
        match translate(&value) {
            Translated::Agent(data) => self.pending_agent = Some(data),
            Translated::Change(data) => {
                if let Some(agent) = self.pending_agent.take() {
                    self.out.push_back(frame("agent", &agent));
                }
                self.out.push_back(frame("change", &data));
            }
            Translated::Entities(agent) => {
                let (count, first) = self.pending_entities.take().unwrap_or((0, None));
                self.pending_entities = Some((count + 1, first.or(agent)));
            }
            Translated::Resync => self.out.push_back(frame("resync", &json!({}))),
            Translated::Nothing => {}
        }
    }

    fn tick(&mut self) {
        self.ticks = self.ticks.wrapping_add(1);
        if let Some(agent) = self.pending_agent.take() {
            self.out.push_back(frame("agent", &agent));
        }
        if self.ticks % ENTITY_PULSE_TICKS == 0 {
            if let Some((count, agent)) = self.pending_entities.take() {
                self.out.push_back(frame(
                    "entities",
                    &json!({ "count": count, "agent": agent }),
                ));
            }
        }
        if self.out.is_empty() && self.ticks % KEEPALIVE_TICKS == 0 {
            self.out.push_back(": still here\n\n".to_string());
        }
    }

    /// Split complete frames off the buffer and take in their data lines.
    fn drain(&mut self) {
        while let Some(end) = self.buffer.windows(2).position(|pair| pair == b"\n\n") {
            let block: Vec<u8> = self.buffer.drain(..end + 2).collect();
            let block = String::from_utf8_lossy(&block);
            let data: Vec<&str> = block
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect();
            if !data.is_empty() {
                self.take(&data.join("\n"));
            }
        }
    }

    async fn next_frame(mut self) -> Option<(Result<String, Infallible>, Self)> {
        loop {
            if let Some(frame) = self.out.pop_front() {
                return Some((Ok(frame), self));
            }
            if self.ended {
                return None;
            }
            tokio::select! {
                chunk = self.response.chunk() => match chunk {
                    Ok(Some(bytes)) => {
                        self.buffer.extend_from_slice(&bytes);
                        if self.buffer.len() > STREAM_BUFFER_MAX {
                            self.buffer.clear();
                            self.out.push_back(frame("resync", &json!({})));
                        }
                        self.drain();
                    }
                    Ok(None) | Err(_) => {
                        self.out.push_back(frame("closed", &json!({})));
                        self.ended = true;
                    }
                },
                _ = self.ticker.tick() => self.tick(),
            }
        }
    }
}

async fn serve_events(State(state): State<Arc<UiState>>) -> Response {
    if state.agents.streams.fetch_add(1, Ordering::SeqCst) >= STREAMS_MAX {
        state.agents.streams.fetch_sub(1, Ordering::SeqCst);
        return unavailable(
            StatusCode::TOO_MANY_REQUESTS,
            "This page is already following Kin in other windows. It reads again on its own instead.",
        );
    }
    let slot = StreamSlot(state.clone());
    let opened = async {
        let client = super::review_ui::daemon(&state).await?;
        tokio::time::timeout(
            READ_TIMEOUT,
            client.graph_events("types=ToolCall,Coordination,EntityChanged"),
        )
        .await
        .context("Kin took too long to start the live feed")?
    }
    .await;
    let response = match opened {
        Ok(response) => response,
        Err(error) => {
            drop(slot);
            return unavailable(
                StatusCode::BAD_GATEWAY,
                &format!("Couldn't follow this repository live: {error:#}"),
            );
        }
    };
    let mut ticker = tokio::time::interval(PULSE_EVERY);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut relay = Relay {
        response,
        buffer: Vec::new(),
        out: std::collections::VecDeque::new(),
        pending_agent: None,
        pending_entities: None,
        ticker,
        ticks: 0,
        ended: false,
        _slot: slot,
    };
    relay.out.push_back(format!(
        "retry: {STREAM_RETRY_MS}\nevent: ready\ndata: {{}}\n\n"
    ));
    let stream = futures_util::stream::unfold(relay, Relay::next_frame);
    let mut response = Response::new(Body::from_stream(stream));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_are_named_as_a_reader_knows_them() {
        assert_eq!(
            agent_display_name("claude-code", "claude-code"),
            "Claude Code"
        );
        assert_eq!(
            agent_display_name("cursor-vscode", "cursor-vscode"),
            "Cursor"
        );
        assert_eq!(
            agent_display_name("codex-mcp-client", "codex-mcp-client"),
            "Codex"
        );
        assert_eq!(agent_display_name("my-helper", "my-helper"), "My Helper");
        assert_eq!(
            agent_display_name("unknown", "unnamed client"),
            "Unnamed agent"
        );
        assert_eq!(agent_display_name("mcp-client", ""), "Unnamed agent");
        assert_eq!(agent_display_name("x", "/Users/someone/bin/agent"), "X");
        assert_eq!(agent_display_name("", "src/agent.py"), "Unnamed agent");
    }

    #[test]
    fn a_change_authored_by_an_agent_names_the_agent_and_hides_the_session() {
        let session = "0B6F8C55-3F0E-4A8E-9A31-5D1F2F0C9E11";
        let author =
            agent_author(&format!("claude-code/claude-code <mcp-agent:{session}>")).unwrap();
        assert_eq!(author.name, "Claude Code");
        assert_eq!(author.session, session.to_ascii_lowercase());
        let unnamed = agent_author(&format!("{session} <mcp-agent:{session}>")).unwrap();
        assert_eq!(unnamed.name, "Unnamed agent");
        assert_eq!(agent_author("Dana Reader <dana@example.invalid>"), None);
        assert_eq!(
            agent_author("David Lord <davidism@gmail.com> 1749933342 -0700"),
            None
        );

        let key = agent_key(session);
        assert_eq!(key.len(), 16);
        assert_eq!(key, agent_key(&session.to_ascii_lowercase()));
        assert!(!key.contains(&session[..8].to_ascii_lowercase()));

        let mut change = json!({
            "author": "claude-code/claude-code",
            "author_recorded": format!("claude-code/claude-code <mcp-agent:{session}>"),
        });
        change["message"] = json!(format!(
            "Explain how want_bytes treats text\n\nMCP transaction d5ee7ddb-eed1-4b51-a643-85f216ec1769"
        ));
        attribute_change(&mut change);
        assert_eq!(change["message"], "Explain how want_bytes treats text");
        assert_eq!(change["author"], "Claude Code");
        assert_eq!(change["author_recorded"], Value::Null);
        assert_eq!(change["agent"]["key"], key);
        assert!(!change.to_string().to_ascii_lowercase().contains("0b6f8c55"));

        let mut person = json!({ "author": "Dana Reader", "author_recorded": "Dana Reader <d@example.invalid>" });
        attribute_change(&mut person);
        assert_eq!(person["author"], "Dana Reader");
        assert!(person.get("agent").is_none());
    }

    #[test]
    fn entity_names_never_carry_a_place_on_disk() {
        let method = json!({ "name": "sign", "kind": "Method",
            "semantic_owners": [{ "entity_id": "a", "name": "Signer", "kind": "Class" }] });
        assert_eq!(named_from(&method).unwrap().label, "Signer.sign");
        let function = json!({ "name": "want_bytes", "kind": "Function",
            "semantic_owners": [{ "entity_id": "b", "name": "encoding", "kind": "Module" }] });
        assert_eq!(named_from(&function).unwrap().label, "want_bytes");
        assert_eq!(
            named_from(&json!({ "name": "src/itsdangerous/signer.py", "kind": "Module" })),
            None
        );
        assert_eq!(
            named_from(&json!({ "name": "signer.py", "kind": "File" })),
            None
        );
        let owner_path = json!({ "name": "sign", "kind": "Method",
            "semantic_owners": [{ "entity_id": "a", "name": "signer.py", "kind": "Class" }] });
        assert_eq!(named_from(&owner_path).unwrap().label, "sign");
    }

    fn call(seq: u64, tool: &str, ids: &[&str]) -> Value {
        json!({ "seq": seq, "at": "2026-10-10T20:41:05.990Z", "tool": tool, "entity_ids": ids,
                "result_count": ids.len(), "bytes": 10, "duration_ms": 3, "is_error": false })
    }

    const A: &str = "5a1c7a4e-8d55-4d0b-a7a4-0b1f1b8f6d10";
    const B: &str = "9e2b7a4e-8d55-4d0b-a7a4-0b1f1b8f6d10";
    const C: &str = "0c3d7a4e-8d55-4d0b-a7a4-0b1f1b8f6d10";

    fn seen_with(calls: &[Value], changes: &[&str]) -> Seen {
        let mut seen = Seen {
            session: "s".into(),
            name: "Claude Code".into(),
            transport: "mcp".into(),
            started_at: String::new(),
            last_activity_at: String::new(),
            live: true,
            last_listed: Instant::now(),
            calls_total: calls.len() as u64,
            calls: BTreeMap::new(),
            changes: changes.iter().map(|c| c.to_string()).collect(),
            snapshot: json!({ "state": "active" }),
        };
        remember_calls(&mut seen, calls);
        seen
    }

    #[test]
    fn what_an_agent_looked_at_is_read_from_its_calls_before_each_commit() {
        let first = "a".repeat(64);
        let second = "b".repeat(64);
        let seen = seen_with(
            &[
                call(1, "semantic_locate", &[A, B]),
                call(2, "get_entity_source", &[A]),
                call(3, "kin_transaction_begin", &[]),
                call(4, "kin_transaction_commit", &[A]),
                call(5, "impact_analysis", &[C, A]),
                call(6, "kin_transaction_begin", &[]),
                call(7, "kin_transaction_commit", &[C]),
            ],
            &[&second, &first],
        );
        let older = looked_at(&seen.calls, &seen.changes, &first).unwrap();
        assert_eq!(older.ids, vec![A.to_string(), B.to_string()]);
        let mut wide = seen_with(
            &[
                call(1, "semantic_locate", &[B, C]),
                call(2, "get_entity_source", &[A, B]),
                call(3, "kin_transaction_commit", &[A]),
            ],
            &[&first],
        );
        wide.changes = vec![first.clone()];
        let ordered = looked_at(&wide.calls, &wide.changes, &first).unwrap();
        assert_eq!(
            ordered.ids,
            vec![B.to_string(), A.to_string(), C.to_string()],
            "what each call was about leads, then what the answers named"
        );
        assert!(older.complete);
        let newer = looked_at(&seen.calls, &seen.changes, &second).unwrap();
        assert_eq!(newer.ids, vec![C.to_string(), A.to_string()]);
        assert!(newer.complete);
        assert_eq!(looked_at(&seen.calls, &seen.changes, &"c".repeat(64)), None);

        // Calls before the remembered ones were dropped: the count is a floor.
        let trimmed = seen_with(
            &[
                call(40, "find_references", &[B]),
                call(41, "kin_transaction_commit", &[A]),
            ],
            &[&first],
        );
        let floor = looked_at(&trimmed.calls, &trimmed.changes, &first).unwrap();
        assert_eq!(floor.ids, vec![B.to_string()]);
        assert!(!floor.complete);
    }

    #[test]
    fn refused_unrecognized_calls_do_not_count_as_entities_read_before_a_commit() {
        let change = "a".repeat(64);
        let mut refused = call(2, "unrecognized_tool", &[C]);
        refused["is_error"] = json!(true);
        let seen = seen_with(
            &[
                call(1, "get_entity_source", &[A]),
                refused,
                call(3, "kin_transaction_commit", &[A]),
            ],
            &[&change],
        );
        let looked = looked_at(&seen.calls, &seen.changes, &change).unwrap();
        assert_eq!(looked.ids, vec![A.to_string()]);
        assert!(looked.complete);
    }

    #[test]
    fn remembered_calls_keep_only_what_the_page_may_show_and_stay_bounded() {
        let mut seen = seen_with(&[], &[]);
        let noisy = json!({ "seq": 1, "at": "t", "tool": "find_references",
            "entity_ids": [A, "not-an-id", "/Users/x/y.py"], "result_count": 2, "bytes": 99,
            "duration_ms": 4, "is_error": false, "query": "secret words", "repo_id": "r" });
        remember_calls(&mut seen, &[noisy]);
        let kept = seen.calls.values().next().unwrap();
        assert_eq!(kept["entity_ids"], json!([A]));
        assert!(kept.get("query").is_none() && kept.get("bytes").is_none());
        let many: Vec<Value> = (2..=400)
            .map(|n| call(n, "get_entity_source", &[A]))
            .collect();
        remember_calls(&mut seen, &many);
        assert_eq!(seen.calls.len(), CALLS_KEPT);
        assert_eq!(
            *seen.calls.keys().next().unwrap(),
            400 - CALLS_KEPT as u64 + 1
        );
    }

    #[test]
    fn the_agents_answer_is_remembered_and_ended_sessions_stay_earlier() {
        let memory = AgentMemory::default();
        let session = "dbecbbcd-c382-4559-93ba-9f08bbf28ab6";
        let change = "7a484793eb222d07dc925f52d65f98e0cea1f4d4d376315588e90bc20082be07";
        let payload = json!({ "schema": "kin.agents.v1", "agents": [{
            "session_id": session, "client": "claude-code", "vendor": "claude-code", "transport": "mcp",
            "started_at": "2026-10-10T22:09:31Z", "last_activity_at": "2026-10-10T22:09:49Z",
            "state": "active", "calls_in_flight": 0, "calls_total": 66,
            "recent_calls": [call(66, "kin_transaction_commit", &[A]), call(65, "kin_transaction_begin", &[])],
            "open_transactions": [{ "transaction_id": "tx-1", "state": "active", "staged_operations": 2, "age_secs": 9 }],
            "intents": [{ "intent_id": "i", "lock_type": "hard",
                "scopes": [{ "kind": "entity", "id": A, "name": "dumps" }, { "kind": "contract", "id": "c" }],
                "withheld_scopes": 1, "task": "Tighten src/itsdangerous/signer.py defaults" }],
            "recent_change_ids": [change],
        }]});
        assert_eq!(observe(&memory, &payload), vec![session.to_string()]);
        let seen = memory.lock().agents.get(session).cloned().unwrap();
        let shown = agent_json(&seen, 5, Vec::new());
        assert_eq!(shown["name"], "Claude Code");
        assert_eq!(shown["state"], "active");
        assert_eq!(
            shown["open_changes"],
            json!([{ "staged_edits": 2, "age_secs": 9 }])
        );
        assert_eq!(shown["intents"][0]["task"], "Tighten \u{2026} defaults");
        assert_eq!(shown["intents"][0]["other_scopes"], 2);
        assert_eq!(shown["scope_names"][A]["label"], "dumps");
        let text = shown.to_string();
        assert!(!text.contains(session) && !text.contains("tx-1") && !text.contains("signer.py"));
        assert_eq!(
            memory.lock().change_owner.get(change).map(String::as_str),
            Some(session)
        );

        observe(&memory, &json!({ "agents": [] }));
        let ended = memory.lock().agents.get(session).cloned().unwrap();
        assert_eq!(state_of(&ended), "ended");
        let shown = agent_json(&ended, 3, Vec::new());
        assert_eq!(shown["open_changes"], json!([]));
        assert_eq!(shown["recent_calls"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn attribution_history_stays_bounded_through_agent_and_commit_churn() {
        let memory = AgentMemory::default();
        let historical_change = "e".repeat(64);
        let historical_session = "historical-session";
        {
            let mut kept = memory.lock();
            kept.change_rows.insert(
                historical_change.clone(),
                json!({ "id": historical_change }),
            );
            kept.change_owner
                .insert(historical_change.clone(), historical_session.into());
            kept.author_names
                .insert(historical_session.into(), "Historical Agent".into());
        }
        for round in 0..AGENTS_KEPT * 8 {
            let session = format!("00000000-0000-4000-8000-{round:012x}");
            let changes: Vec<String> = (0..CHANGES_KEPT * 2)
                .map(|change| format!("{:064x}", round * CHANGES_KEPT * 2 + change))
                .collect();
            memory
                .lock()
                .author_names
                .insert(session.clone(), "Codex".into());
            observe(
                &memory,
                &json!({ "agents": [{
                "session_id": session, "client": "codex", "vendor": "codex",
                "recent_change_ids": changes,
            }] }),
            );
            let kept = memory.lock();
            assert!(kept.agents.len() <= AGENTS_KEPT);
            assert!(kept.change_owner.len() <= kept.agents.len() * CHANGES_KEPT + 1);
            assert!(kept.author_names.len() <= kept.agents.len() + 1);
            assert_eq!(kept.change_owner.get(&changes[0]), Some(&session));
            assert!(!kept.change_owner.contains_key(&changes[CHANGES_KEPT]));
            assert_eq!(
                kept.change_owner
                    .get(&historical_change)
                    .map(String::as_str),
                Some(historical_session)
            );
            assert_eq!(
                kept.author_names
                    .get(historical_session)
                    .map(String::as_str),
                Some("Historical Agent")
            );
        }
        // Clearing refreshed rows no longer leaves their attribution behind.
        memory.lock().change_rows.clear();
        observe(&memory, &json!({ "agents": [] }));
        let kept = memory.lock();
        assert!(!kept.change_owner.contains_key(&historical_change));
        assert!(!kept.author_names.contains_key(historical_session));
        assert!(kept.change_owner.len() <= AGENTS_KEPT * CHANGES_KEPT);
        assert!(kept.author_names.len() <= AGENTS_KEPT);
    }

    #[test]
    fn attribution_pruning_preserves_current_rows_and_latest_agent_ownership() {
        let memory = AgentMemory::default();
        let current = "a".repeat(64);
        let fallback = "b".repeat(64);
        let session = "00000000-0000-4000-8000-000000000001";
        {
            let mut kept = memory.lock();
            for change in [&current, &fallback] {
                kept.change_rows
                    .insert(change.clone(), json!({ "id": change }));
                kept.change_owner
                    .insert(change.clone(), "earlier-session".into());
            }
            kept.author_names
                .insert("earlier-session".into(), "Claude Code".into());
        }
        observe(
            &memory,
            &json!({ "agents": [{
            "session_id": session, "client": "codex", "vendor": "codex",
            "recent_change_ids": [current],
        }] }),
        );
        let mut kept = memory.lock();
        assert_eq!(
            kept.change_owner.get(&current).map(String::as_str),
            Some(session)
        );
        assert_eq!(
            kept.change_owner.get(&fallback).map(String::as_str),
            Some("earlier-session")
        );
        assert_eq!(
            kept.author_names.get("earlier-session").map(String::as_str),
            Some("Claude Code")
        );
        // A later row refresh retains the daemon's latest observed owner.
        kept.change_owner
            .entry(current.clone())
            .or_insert_with(|| "earlier-session".into());
        prune_attribution(&mut kept);
        assert_eq!(
            kept.change_owner.get(&current).map(String::as_str),
            Some(session)
        );
    }

    #[test]
    fn the_live_feed_passes_on_pulses_without_sessions_or_places() {
        let session = "dbecbbcd-c382-4559-93ba-9f08bbf28ab6";
        let tool = json!({ "type": "ToolCall", "seq": 5, "session_id": session, "call_seq": 12,
            "coalesced": 7, "at": "2026-10-10T22:09:37Z", "tool": "get_entity_source",
            "entity_ids": [A], "result_count": 2, "bytes": 3490, "duration_ms": 8, "is_error": false });
        let Translated::Agent(data) = translate(&tool) else {
            panic!("a tool call is an agent pulse")
        };
        assert_eq!(data["agent"], agent_key(session));
        assert_eq!(data["tool"], "get_entity_source");
        assert!(!data.to_string().contains(session));

        let committed = json!({ "type": "Coordination", "event": { "event": "transaction_outcome",
            "outcome": "committed", "session_id": session, "transaction_id": "t", "change_id": "A".repeat(64) } });
        let Translated::Change(data) = translate(&committed) else {
            panic!("a commit is a change pulse")
        };
        assert_eq!(data["change"], "a".repeat(64));

        let changed = json!({ "type": "EntityChanged", "entity_id": A, "change_type": "modified",
            "session_id": session, "file_path": "src/itsdangerous/signer.py" });
        assert!(matches!(translate(&changed), Translated::Entities(Some(_))));
        assert!(matches!(
            translate(&json!({ "type": "GraphRootChanged" })),
            Translated::Nothing
        ));

        // Coordination events carry raw intent scopes; none of them is passed on.
        let intent = json!({ "type": "Coordination", "event": { "event": "intent_registered",
            "session_id": session, "scopes": ["file:src/itsdangerous/signer.py", { "Entity": A }],
            "task": "Tighten src/itsdangerous/signer.py" } });
        let Translated::Agent(data) = translate(&intent) else {
            panic!("an intent is an agent pulse")
        };
        assert_eq!(data, json!({ "agent": agent_key(session) }));
    }
}
