// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! What the agents working in this repository are doing through Kin, as the
//! daemon saw it.
//!
//! Three things, all in memory and all bounded:
//!
//! - a call log per live session: one entry per tool call the daemon ran for
//!   it, holding the tool's name, the graph entities it was about, how much it
//!   answered and how long it took. Never the question, the arguments or the
//!   answer, and never anything that is or becomes a path;
//! - a `ToolCall` event per entry on the daemon's event bus, throttled so a busy
//!   agent cannot flood the subscribers that share that bus;
//! - the path-free projection `GET /agents` and `GET /session/{id}/calls`
//!   serve, which drops the working directory and process id a session
//!   registered with and every file-shaped intent scope.
//!
//! The log lives and dies with the session: ending or reaping a session drops
//! its log, so `/agents` describes agents that are here now.
//!
//! It also holds, briefly, which sessions were handed a reviewer's open flag
//! ([`AgentActivity::note_flag_sightings`]). Those outlive the session, because
//! they are on their way into the repository: `crate::review_flags` writes
//! them into the flag once the agents that saw it have gone quiet.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use kin_model::ids::{EntityId, SessionId};
use kin_model::session::{AgentSession, IntentScope, LockType, SessionTransport};
use kin_model::timestamp::Timestamp;
use kin_model::EntityStore as _;

use crate::state::DaemonEvent;
use crate::DaemonState;

/// Entries one session's log keeps. Older ones are evicted, and `seq` and
/// `calls_total` still count them.
pub const MAX_CALLS_PER_SESSION: usize = 200;
/// Entries all logs together keep. Past it, the oldest entry anywhere goes.
pub const MAX_CALL_LOG_ENTRIES: usize = 4096;
/// Sessions with a log at once. Past it, the log of the session heard from
/// least recently goes.
pub const MAX_CALL_LOG_SESSIONS: usize = 64;
/// Entity ids one entry keeps.
pub const MAX_ENTITY_IDS_PER_CALL: usize = 16;
/// Largest registered tool name the activity log accepts, in bytes.
pub const MAX_TOOL_NAME_BYTES: usize = 64;
/// Id-shaped strings read out of one call's request and answer before the scan
/// stops. Bounds the graph lookups a single large answer can cost.
pub const MAX_ENTITY_ID_CANDIDATES: usize = 2048;
/// Committed change ids one session keeps.
pub const MAX_CHANGES_PER_SESSION: usize = 16;
/// Flag sightings held before they are written into the repository. Past it,
/// the oldest goes.
pub const MAX_PENDING_FLAG_SIGHTINGS: usize = 1024;

/// At most one `ToolCall` event per session in this window.
pub const TOOL_CALL_EVENT_MIN_INTERVAL: Duration = Duration::from_millis(250);
/// At most this many `ToolCall` events across all sessions per rolling second.
pub const TOOL_CALL_EVENTS_PER_SECOND: usize = 16;
/// How often a held event is retried.
const TOOL_CALL_EVENT_FLUSH_TICK: Duration = Duration::from_millis(50);
const EVENT_WINDOW: Duration = Duration::from_secs(1);

/// A session that made a call or was heard from this recently is active.
pub const ACTIVE_WINDOW: Duration = Duration::from_secs(60);
/// Newest calls `/agents` shows per agent.
pub const AGENT_RECENT_CALLS: usize = 5;
/// Default and largest `limit` for `/session/{id}/calls`.
pub const DEFAULT_CALLS_LIMIT: usize = 50;
/// Transactions, intents, and scopes per intent `/agents` shows per agent.
pub const AGENT_LIST_LIMIT: usize = 16;
/// Characters of an intent's task description `/agents` shows.
pub const TASK_TEXT_LIMIT: usize = 140;

/// One tool call the daemon ran for a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallEntry {
    /// 1-based position of this call in its session.
    pub seq: u64,
    /// When the call finished.
    pub at: Timestamp,
    /// The registered tool the daemon ran.
    pub tool: String,
    /// Graph entities the call was about: the request's targets first, then
    /// entities the answer named, each confirmed present in the graph.
    pub entity_ids: Vec<EntityId>,
    /// Distinct graph entities the answer named.
    pub result_count: usize,
    /// Size of the answer text the daemon returned.
    pub bytes: usize,
    /// Wall time the daemon spent on the call.
    pub duration_ms: u64,
    /// The answer was a refusal or an error.
    pub is_error: bool,
    /// The repository a repo-scoped call (`/repos/{repo_id}/mcp/tools/call`)
    /// named. Absent for a call on the daemon's own repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    /// The change a write committed, as its answer named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    /// The call finished after the agent that made it stopped waiting for its
    /// answer, so what it did is known here and the agent was never told.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub caller_left: bool,
}

/// What a call contributes to its entry before the log numbers and stamps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallDraft {
    pub tool: String,
    pub entity_ids: Vec<EntityId>,
    pub result_count: usize,
    pub bytes: usize,
    pub duration_ms: u64,
    pub is_error: bool,
    pub repo_id: Option<String>,
    /// The change a write committed.
    pub change_id: Option<String>,
    /// The caller stopped waiting before the call finished.
    pub caller_left: bool,
}

/// One entry as the event bus carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallEvent {
    pub session_id: SessionId,
    pub entry: ToolCallEntry,
    /// Earlier calls of this session this event stands for that got no event
    /// of their own.
    pub coalesced: u32,
}

impl ToolCallEvent {
    pub fn into_daemon_event(self) -> DaemonEvent {
        DaemonEvent::ToolCall {
            session_id: self.session_id.to_string(),
            call_seq: self.entry.seq,
            at: self.entry.at,
            tool: self.entry.tool,
            entity_ids: self.entry.entity_ids,
            result_count: self.entry.result_count,
            bytes: self.entry.bytes,
            duration_ms: self.entry.duration_ms,
            is_error: self.entry.is_error,
            repo_id: self.entry.repo_id,
            coalesced: self.coalesced,
        }
    }
}

/// What the event gate decided about one entry.
#[derive(Debug, PartialEq, Eq)]
pub enum EventOffer {
    /// Send this event now.
    Emit(ToolCallEvent),
    /// Held for a later flush. `start_flusher` is true for exactly the offer
    /// that found no flush running.
    Held { start_flusher: bool },
}

#[derive(Debug, Default)]
struct SessionLog {
    /// Entries with the order they were recorded in across all sessions, so
    /// the global bound can find the oldest one.
    calls: VecDeque<(u64, ToolCallEntry)>,
    calls_total: u64,
    last_call_at: Option<Timestamp>,
    changes: VecDeque<String>,
    /// When this session was last heard from, in the log's own order.
    touched: u64,
}

#[derive(Debug, Default)]
struct CallLog {
    sessions: HashMap<SessionId, SessionLog>,
    entries: usize,
    next_order: u64,
}

impl CallLog {
    fn session_mut(&mut self, session_id: &SessionId) -> &mut SessionLog {
        if !self.sessions.contains_key(session_id) && self.sessions.len() >= MAX_CALL_LOG_SESSIONS {
            let stalest = self
                .sessions
                .iter()
                .min_by_key(|(_, log)| log.touched)
                .map(|(id, _)| *id);
            if let Some(stalest) = stalest {
                if let Some(evicted) = self.sessions.remove(&stalest) {
                    self.entries -= evicted.calls.len();
                }
            }
        }
        self.sessions.entry(*session_id).or_default()
    }

    fn record(
        &mut self,
        session_id: &SessionId,
        draft: ToolCallDraft,
        at: Timestamp,
    ) -> ToolCallEntry {
        let order = self.next_order;
        self.next_order += 1;
        let log = self.session_mut(session_id);
        log.calls_total += 1;
        log.touched = order;
        log.last_call_at = Some(at.clone());
        let entry = ToolCallEntry {
            seq: log.calls_total,
            at,
            tool: activity_tool_name(&draft.tool),
            entity_ids: draft.entity_ids,
            result_count: draft.result_count,
            bytes: draft.bytes,
            duration_ms: draft.duration_ms,
            is_error: draft.is_error,
            repo_id: draft.repo_id,
            change_id: draft.change_id,
            caller_left: draft.caller_left,
        };
        log.calls.push_back((order, entry.clone()));
        let mut added = 1;
        if log.calls.len() > MAX_CALLS_PER_SESSION {
            log.calls.pop_front();
            added = 0;
        }
        self.entries += added;
        while self.entries > MAX_CALL_LOG_ENTRIES {
            let oldest = self
                .sessions
                .iter()
                .filter_map(|(id, log)| log.calls.front().map(|(order, _)| (*order, *id)))
                .min_by_key(|(order, _)| *order);
            let Some((_, id)) = oldest else { break };
            if let Some(log) = self.sessions.get_mut(&id) {
                log.calls.pop_front();
                self.entries -= 1;
            }
        }
        entry
    }
}

#[derive(Debug)]
struct Pending {
    entry: ToolCallEntry,
    folded: u32,
    since: Instant,
}

#[derive(Debug, Default)]
struct EventGate {
    last_emit: HashMap<SessionId, Instant>,
    pending: HashMap<SessionId, Pending>,
    window: VecDeque<Instant>,
    flusher_running: bool,
}

impl EventGate {
    fn admits(&mut self, session_id: &SessionId, now: Instant) -> bool {
        while self
            .window
            .front()
            .is_some_and(|sent| now.saturating_duration_since(*sent) >= EVENT_WINDOW)
        {
            self.window.pop_front();
        }
        let session_clear = self.last_emit.get(session_id).is_none_or(|sent| {
            now.saturating_duration_since(*sent) >= TOOL_CALL_EVENT_MIN_INTERVAL
        });
        session_clear && self.window.len() < TOOL_CALL_EVENTS_PER_SECOND
    }

    fn mark_sent(&mut self, session_id: &SessionId, now: Instant) {
        if self.last_emit.len() > MAX_CALL_LOG_SESSIONS {
            // Only a send inside the interval constrains the next one.
            self.last_emit.retain(|_, sent| {
                now.saturating_duration_since(*sent) < TOOL_CALL_EVENT_MIN_INTERVAL
            });
        }
        self.last_emit.insert(*session_id, now);
        self.window.push_back(now);
    }

    fn offer(&mut self, session_id: SessionId, entry: ToolCallEntry, now: Instant) -> EventOffer {
        if let Some(pending) = self.pending.get_mut(&session_id) {
            // A held event is already waiting for this session, and a flush is
            // running for it: this call replaces it and the one it replaced is
            // folded in.
            pending.folded = pending.folded.saturating_add(1);
            pending.entry = entry;
            return EventOffer::Held {
                start_flusher: false,
            };
        }
        if self.admits(&session_id, now) {
            self.mark_sent(&session_id, now);
            return EventOffer::Emit(ToolCallEvent {
                session_id,
                entry,
                coalesced: 0,
            });
        }
        self.pending.insert(
            session_id,
            Pending {
                entry,
                folded: 0,
                since: now,
            },
        );
        let start_flusher = !self.flusher_running;
        self.flusher_running = true;
        EventOffer::Held { start_flusher }
    }

    /// The held events that may go now, oldest first, and whether any remain.
    /// A drain that leaves nothing held ends the flush, under the same lock an
    /// offer takes, so an offer after it starts a new one.
    fn drain(&mut self, now: Instant) -> (Vec<ToolCallEvent>, bool) {
        let mut waiting: Vec<(Instant, SessionId)> = self
            .pending
            .iter()
            .map(|(id, pending)| (pending.since, *id))
            .collect();
        waiting.sort_by_key(|(since, id)| (*since, id.0));
        let mut ready = Vec::new();
        for (_, session_id) in waiting {
            if !self.admits(&session_id, now) {
                continue;
            }
            if let Some(pending) = self.pending.remove(&session_id) {
                self.mark_sent(&session_id, now);
                ready.push(ToolCallEvent {
                    session_id,
                    entry: pending.entry,
                    coalesced: pending.folded,
                });
            }
        }
        let more = !self.pending.is_empty();
        if !more {
            self.flusher_running = false;
        }
        (ready, more)
    }

    fn forget(&mut self, session_id: &SessionId) {
        self.last_emit.remove(session_id);
        self.pending.remove(session_id);
    }
}

/// One session that was handed one open review flag, not yet written into
/// the flag.
pub type FlagSighting = (kin_model::AnnotationId, kin_model::ReviewFlagSighting);

#[derive(Debug, Default)]
struct FlagSightings {
    pending: VecDeque<FlagSighting>,
    /// A task is running that will write what is pending.
    settling: bool,
}

/// What one session's log held when the session ended.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EndedLog {
    pub calls_total: u64,
    /// Its newest calls, oldest first.
    pub calls: Vec<ToolCallEntry>,
    /// The changes it committed, newest first.
    pub changes: Vec<String>,
    pub last_call_at: Option<Timestamp>,
}

/// The call logs and the event gate, owned by the session coordinator.
#[derive(Debug, Default)]
pub struct AgentActivity {
    log: Mutex<CallLog>,
    events: Mutex<EventGate>,
    flag_sightings: Mutex<FlagSightings>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl AgentActivity {
    /// Record a call for `session_id` if `is_live` says the session exists,
    /// checked while the log is locked so a session ended concurrently cannot
    /// be left with a log after [`AgentActivity::forget`].
    pub fn record_call_if(
        &self,
        session_id: &SessionId,
        draft: ToolCallDraft,
        at: Timestamp,
        is_live: impl FnOnce() -> bool,
    ) -> Option<ToolCallEntry> {
        let mut log = lock(&self.log);
        if !is_live() {
            return None;
        }
        Some(log.record(session_id, draft, at))
    }

    /// Record a call for `session_id` unconditionally.
    pub fn record_call(
        &self,
        session_id: &SessionId,
        draft: ToolCallDraft,
        at: Timestamp,
    ) -> ToolCallEntry {
        lock(&self.log).record(session_id, draft, at)
    }

    /// The session's total call count, the entries retained, and the newest
    /// `limit` of them, newest first.
    pub fn recent_calls(
        &self,
        session_id: &SessionId,
        limit: usize,
    ) -> (u64, usize, Vec<ToolCallEntry>) {
        let log = lock(&self.log);
        let Some(session) = log.sessions.get(session_id) else {
            return (0, 0, Vec::new());
        };
        let calls = session
            .calls
            .iter()
            .rev()
            .take(limit)
            .map(|(_, entry)| entry.clone())
            .collect();
        (session.calls_total, session.calls.len(), calls)
    }

    /// When the session's last recorded call finished.
    pub fn last_call_at(&self, session_id: &SessionId) -> Option<Timestamp> {
        lock(&self.log)
            .sessions
            .get(session_id)
            .and_then(|session| session.last_call_at.clone())
    }

    /// Remember a change the session committed.
    pub fn record_change(&self, session_id: &SessionId, change_id: String) {
        let mut log = lock(&self.log);
        let order = log.next_order;
        log.next_order += 1;
        let session = log.session_mut(session_id);
        session.touched = order;
        session.changes.retain(|known| known != &change_id);
        session.changes.push_front(change_id);
        session.changes.truncate(MAX_CHANGES_PER_SESSION);
    }

    /// Changes the session committed, newest first.
    pub fn recent_changes(&self, session_id: &SessionId) -> Vec<String> {
        lock(&self.log)
            .sessions
            .get(session_id)
            .map(|session| session.changes.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Drop everything held for a session that ended, and hand back what its
    /// log held so the session can be kept past its end
    /// ([`crate::ended_sessions`]).
    pub fn forget(&self, session_id: &SessionId) -> EndedLog {
        let ended = {
            let mut log = lock(&self.log);
            match log.sessions.remove(session_id) {
                Some(evicted) => {
                    log.entries -= evicted.calls.len();
                    EndedLog {
                        calls_total: evicted.calls_total,
                        calls: evicted.calls.into_iter().map(|(_, entry)| entry).collect(),
                        changes: evicted.changes.into_iter().collect(),
                        last_call_at: evicted.last_call_at,
                    }
                }
                None => EndedLog::default(),
            }
        };
        lock(&self.events).forget(session_id);
        ended
    }

    /// Hold that `sighting`'s session was handed each flag in `flags`, until
    /// it is written into the repository. A flag the session is already held
    /// for is not held twice. Returns whether the caller must start the task
    /// that writes them: true for exactly the call that found none running.
    pub fn note_flag_sightings(
        &self,
        flags: &[kin_model::AnnotationId],
        sighting: &kin_model::ReviewFlagSighting,
    ) -> bool {
        let mut held = lock(&self.flag_sightings);
        let mut added = false;
        for flag in flags {
            let known = held
                .pending
                .iter()
                .any(|(pending, seen)| pending == flag && seen.session == sighting.session);
            if !known {
                held.pending.push_back((*flag, sighting.clone()));
                added = true;
            }
        }
        while held.pending.len() > MAX_PENDING_FLAG_SIGHTINGS {
            held.pending.pop_front();
        }
        let start = added && !held.settling;
        if start {
            held.settling = true;
        }
        start
    }

    /// The sightings held, oldest first.
    pub fn pending_flag_sightings(&self) -> Vec<FlagSighting> {
        lock(&self.flag_sightings).pending.iter().cloned().collect()
    }

    /// Take the held sightings `ready` accepts, oldest first, leaving the
    /// rest held.
    pub fn take_flag_sightings(&self, ready: impl Fn(&FlagSighting) -> bool) -> Vec<FlagSighting> {
        let mut held = lock(&self.flag_sightings);
        let (taken, kept): (VecDeque<_>, VecDeque<_>) = std::mem::take(&mut held.pending)
            .into_iter()
            .partition(ready);
        held.pending = kept;
        taken.into()
    }

    /// Hold sightings again that could not be written, ahead of any noted
    /// since they were taken. Returns whether the caller must start the task
    /// that writes them, as [`Self::note_flag_sightings`] does.
    pub fn return_flag_sightings(&self, unwritten: Vec<FlagSighting>) -> bool {
        let mut held = lock(&self.flag_sightings);
        if unwritten.is_empty() {
            return false;
        }
        for sighting in unwritten.into_iter().rev() {
            held.pending.push_front(sighting);
        }
        while held.pending.len() > MAX_PENDING_FLAG_SIGHTINGS {
            held.pending.pop_front();
        }
        let start = !held.settling;
        held.settling = true;
        start
    }

    /// Whether the task that writes sightings may stop: nothing is held. It
    /// is marked stopped under the lock a new sighting takes, so one noted
    /// after this starts a new task.
    pub fn flag_sightings_settled(&self) -> bool {
        let mut held = lock(&self.flag_sightings);
        if held.pending.is_empty() {
            held.settling = false;
        }
        held.pending.is_empty()
    }

    /// Entries held across every log.
    pub fn retained_entries(&self) -> usize {
        lock(&self.log).entries
    }

    /// Sessions with a log.
    pub fn logged_sessions(&self) -> usize {
        lock(&self.log).sessions.len()
    }

    /// Offer one entry to the event gate.
    pub fn offer_event(
        &self,
        session_id: SessionId,
        entry: ToolCallEntry,
        now: Instant,
    ) -> EventOffer {
        lock(&self.events).offer(session_id, entry, now)
    }

    /// Take the held events that may go now, and whether any remain held.
    pub fn drain_events(&self, now: Instant) -> (Vec<ToolCallEvent>, bool) {
        lock(&self.events).drain(now)
    }
}

// ---------------------------------------------------------------------------
// Reading a call
// ---------------------------------------------------------------------------

fn is_hex(byte: u8) -> bool {
    byte.is_ascii_hexdigit()
}

/// Every hyphenated UUID in `text`, in order, up to `cap`. A UUID inside a
/// longer run of hex digits and hyphens is not one.
pub fn uuid_candidates(text: &str, cap: usize, out: &mut Vec<Uuid>) {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    let bytes = text.as_bytes();
    let mut index = 0;
    while index + 36 <= bytes.len() && out.len() < cap {
        let starts_clean = index == 0 || !(is_hex(bytes[index - 1]) || bytes[index - 1] == b'-');
        if !starts_clean || !is_hex(bytes[index]) {
            index += 1;
            continue;
        }
        let mut cursor = index;
        let mut shaped = true;
        for (group, width) in GROUPS.iter().enumerate() {
            if group > 0 {
                if bytes[cursor] != b'-' {
                    shaped = false;
                    break;
                }
                cursor += 1;
            }
            if !bytes[cursor..cursor + width]
                .iter()
                .all(|byte| is_hex(*byte))
            {
                shaped = false;
                break;
            }
            cursor += width;
        }
        let ends_clean = cursor == bytes.len() || !(is_hex(bytes[cursor]) || bytes[cursor] == b'-');
        if shaped && ends_clean {
            if let Ok(uuid) = Uuid::parse_str(&text[index..cursor]) {
                out.push(uuid);
            }
            index = cursor;
        } else {
            index += 1;
        }
    }
}

fn collect_argument_uuids(value: &serde_json::Value, cap: usize, out: &mut Vec<Uuid>) {
    if out.len() >= cap {
        return;
    }
    match value {
        serde_json::Value::String(text) => {
            if let Ok(uuid) = Uuid::parse_str(text.trim()) {
                out.push(uuid);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_argument_uuids(item, cap, out);
            }
        }
        serde_json::Value::Object(fields) => {
            for item in fields.values() {
                collect_argument_uuids(item, cap, out);
            }
        }
        _ => {}
    }
}

/// Every argument value that is, as a whole, a UUID: the only part of a
/// request the call log may keep, and only once the graph confirms it names an
/// entity. Read before the call runs, because the call consumes its arguments.
pub fn argument_uuids(arguments: &HashMap<String, serde_json::Value>) -> Vec<Uuid> {
    let mut targets = Vec::new();
    for value in arguments.values() {
        collect_argument_uuids(value, MAX_ENTITY_ID_CANDIDATES, &mut targets);
    }
    targets
}

/// Only registered tool names belong in the shared activity log. Refused
/// names are caller-controlled text, so retain a fixed label rather than a
/// path, question, control sequence or arbitrarily large string.
fn activity_tool_name(tool: &str) -> String {
    static REGISTERED_NAMES: OnceLock<std::collections::HashSet<String>> = OnceLock::new();
    // A keyed mutate reaches the daemon under its versioned internal name,
    // which is no tool an agent sees: it called `kin_mutate`.
    if tool == crate::mcp_mutate::TOOL {
        return "kin_mutate".to_string();
    }
    if tool.len() <= MAX_TOOL_NAME_BYTES
        && REGISTERED_NAMES
            .get_or_init(|| {
                kin_mcp::tools::tool_definitions()
                    .tools
                    .into_iter()
                    .map(|definition| definition.name)
                    .collect()
            })
            .contains(tool)
    {
        tool.to_string()
    } else {
        "unrecognized_tool".to_string()
    }
}

/// The entry fields a finished call contributes. `targets` are the request's
/// whole-UUID argument values ([`argument_uuids`]); the answer contributes
/// every UUID that stands alone in its text. Each id is kept only when
/// `is_entity` confirms the graph holds it, so nothing a caller typed reaches
/// the log as text.
pub fn draft_for_call(
    tool: &str,
    targets: &[Uuid],
    result: &kin_mcp::ToolCallResult,
    duration: Duration,
    is_entity: impl Fn(&EntityId) -> bool,
) -> ToolCallDraft {
    let mut named = Vec::new();
    let mut bytes = 0usize;
    for block in &result.content {
        let kin_mcp::ContentBlock::Text { text } = block;
        bytes += text.len();
        uuid_candidates(text, MAX_ENTITY_ID_CANDIDATES, &mut named);
    }
    let mut seen = std::collections::HashSet::new();
    let mut entity_ids = Vec::new();
    for uuid in targets {
        let id = EntityId(*uuid);
        if entity_ids.len() < MAX_ENTITY_IDS_PER_CALL && seen.insert(*uuid) && is_entity(&id) {
            entity_ids.push(id);
        }
    }
    let mut checked = std::collections::HashSet::new();
    let mut answered = 0usize;
    for uuid in named {
        let id = EntityId(uuid);
        if !checked.insert(uuid) || !is_entity(&id) {
            continue;
        }
        answered += 1;
        if entity_ids.len() < MAX_ENTITY_IDS_PER_CALL && seen.insert(uuid) {
            entity_ids.push(id);
        }
    }
    ToolCallDraft {
        tool: activity_tool_name(tool),
        entity_ids,
        result_count: answered,
        bytes,
        duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        is_error: result.is_error == Some(true),
        repo_id: None,
        change_id: None,
        caller_left: false,
    }
}

/// Whether a call writes: it opens, stages, commits or drops a change, or
/// mutates in one call. A write lands whether or not its caller is still
/// waiting when it ends, so its record must too; see [`record_tool_call`].
pub fn is_write_tool(tool: &str) -> bool {
    tool.starts_with("kin_transaction")
        || matches!(
            tool,
            "kin_mutate" | "mutate" | "transaction" | "kin_entity_lease"
        )
        || tool == crate::mcp_mutate::TOOL
}

/// Tools whose calls are session lifecycle rather than work.
pub(crate) fn is_lifecycle_tool(tool: &str) -> bool {
    matches!(
        tool,
        "kin_session_start" | "kin_session_heartbeat" | "kin_session_end" | "register_session"
    )
}

/// How one finished call met the session that made it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CallMeeting {
    /// The session was live when the call began.
    pub began_live: bool,
    /// The caller stopped waiting before the call finished.
    pub caller_left: bool,
}

/// Record one finished `/mcp/tools/call` for the session that made it, and
/// put it on the event bus within the gate's bound. Returns whether the daemon
/// still holds that session; a call naming one it never held is not recorded.
///
/// A call whose session ended while it ran, as one does when its agent gives
/// up on a slow commit and closes its connection, is kept on what was kept of
/// the session ([`crate::session_registry::SessionCoordinator::record_finished_call`]),
/// so what the agent did is never lost to how long Kin took to do it. A write
/// says the change it committed, and every call says whether its caller had
/// stopped waiting.
pub(crate) async fn record_tool_call(
    state: &Arc<DaemonState>,
    session_id: SessionId,
    tool: &str,
    targets: &[Uuid],
    result: &kin_mcp::ToolCallResult,
    duration: Duration,
    meeting: CallMeeting,
) -> bool {
    let coordinator = &state.coordinator;
    let is_live = || {
        coordinator
            .get_session(&session_id)
            .ok()
            .flatten()
            .is_some()
    };
    if tool.is_empty() || is_lifecycle_tool(tool) {
        return is_live();
    }
    let graph = state.graph_for_request(Some(&session_id)).await;
    let mut draft = draft_for_call(tool, targets, result, duration, |id| {
        graph.get_entity(id).ok().flatten().is_some()
    });
    if is_write_tool(tool) {
        draft.change_id = committed_change_id(result);
    }
    draft.caller_left = meeting.caller_left;
    match coordinator.record_finished_call(&session_id, draft, Timestamp::now(), meeting.began_live)
    {
        crate::session_registry::FinishedCall::Live(entry) => {
            publish_tool_call(state, session_id, entry);
            true
        }
        crate::session_registry::FinishedCall::AfterEnd
        | crate::session_registry::FinishedCall::Unknown => false,
    }
}

/// Record one finished `/repos/{repo_id}/mcp/tools/call` for the session its
/// `X-Kin-Session` names, the way [`record_tool_call`] records a call on the
/// daemon's own repository, and hand the response back.
///
/// The route answers with a finished response, so its body is read here and
/// put back unchanged. Entity ids are confirmed against that repository's own
/// graph, never this daemon's: ids carry no repository component, so the same
/// id can name different things in two repositories. A session the daemon
/// does not hold gets the same `X-Kin-Session-Status: unknown` answer.
pub(crate) async fn record_repo_scoped_call(
    state: &Arc<DaemonState>,
    repo_id: &str,
    session_id: SessionId,
    tool: &str,
    targets: &[Uuid],
    response: axum::response::Response,
    duration: Duration,
) -> axum::response::Response {
    let (mut parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::debug!(%error, "repo-scoped answer could not be read for the call log");
            return axum::response::Response::from_parts(parts, axum::body::Body::empty());
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let failed = !parts.status.is_success()
        || serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .pointer("/result/isError")
                    .and_then(serde_json::Value::as_bool)
            })
            .unwrap_or(false);
    let mut answer = kin_mcp::ToolCallResult::text(text.into_owned());
    answer.is_error = failed.then_some(true);
    let graph = crate::api::cached_repo_scoped_graph(state, repo_id).await;
    let mut draft = draft_for_call(tool, targets, &answer, duration, |id| {
        graph
            .as_ref()
            .is_some_and(|graph| graph.get_entity(id).ok().flatten().is_some())
    });
    draft.repo_id = Some(display_name(repo_id, "repository"));
    let coordinator = &state.coordinator;
    let held = if tool.is_empty() || is_lifecycle_tool(tool) {
        coordinator
            .get_session(&session_id)
            .ok()
            .flatten()
            .is_some()
    } else {
        match coordinator
            .activity()
            .record_call_if(&session_id, draft, Timestamp::now(), || {
                coordinator
                    .get_session(&session_id)
                    .ok()
                    .flatten()
                    .is_some()
            }) {
            Some(entry) => {
                publish_tool_call(state, session_id, entry);
                true
            }
            None => false,
        }
    };
    if !held {
        parts.headers.insert(
            axum::http::HeaderName::from_static("x-kin-session-status"),
            axum::http::HeaderValue::from_static("unknown"),
        );
    }
    axum::response::Response::from_parts(parts, axum::body::Body::from(bytes))
}

/// Put one entry on the event bus now, or hold it and make sure a flush runs.
pub(crate) fn publish_tool_call(
    state: &Arc<DaemonState>,
    session_id: SessionId,
    entry: ToolCallEntry,
) {
    match state
        .coordinator
        .activity()
        .offer_event(session_id, entry, Instant::now())
    {
        EventOffer::Emit(event) => state.emit_event(event.into_daemon_event()),
        EventOffer::Held {
            start_flusher: true,
        } => {
            let state = Arc::clone(state);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(TOOL_CALL_EVENT_FLUSH_TICK).await;
                    let (ready, more) = state.coordinator.activity().drain_events(Instant::now());
                    for event in ready {
                        state.emit_event(event.into_daemon_event());
                    }
                    if !more {
                        break;
                    }
                }
            });
        }
        EventOffer::Held {
            start_flusher: false,
        } => {}
    }
}

// ---------------------------------------------------------------------------
// Commit attribution
// ---------------------------------------------------------------------------

/// Changes larger than this are not replayed entity by entity on the stream.
/// The `GraphRootChanged` every commit already emits tells a renderer to
/// re-export, which is cheaper than a burst that would lag every subscriber.
pub const MAX_ATTRIBUTED_ENTITY_EVENTS: usize = 256;

/// The change id a successful `kin_transaction_commit` answer names.
pub fn committed_change_id(result: &kin_mcp::ToolCallResult) -> Option<String> {
    if result.is_error == Some(true) {
        return None;
    }
    result.content.iter().find_map(|block| {
        let kin_mcp::ContentBlock::Text { text } = block;
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let id = value.get("change_id")?.as_str()?;
        (id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| id.to_ascii_lowercase())
    })
}

/// Attribute a committed MCP transaction to the session that owns it: remember
/// the change for `/agents`, and emit its entity and relation changes with
/// that session on them.
pub(crate) fn attribute_commit(state: &DaemonState, owner: Option<&str>, change_id: &str) {
    let Some(owner) = owner.and_then(|owner| Uuid::parse_str(owner.trim()).ok()) else {
        return;
    };
    let session_id = SessionId(owner);
    state
        .coordinator
        .record_committed_change(&session_id, change_id.to_string());
    let Ok(hash) = kin_model::Hash256::from_hex(change_id) else {
        return;
    };
    let change_key = kin_model::SemanticChangeId::from_hash(hash);
    let Ok(Some(change)) = kin_model::ChangeStore::get_change(state.graph.as_ref(), &change_key)
    else {
        return;
    };
    if change.entity_deltas.len() > MAX_ATTRIBUTED_ENTITY_EVENTS {
        return;
    }
    let session = session_id.to_string();
    let mut pass = crate::state::PassDelta::default();
    for delta in &change.entity_deltas {
        let (entity, change_type, node) = match delta {
            kin_model::EntityDelta::Added { new } => {
                pass.nodes_added += 1;
                (
                    new,
                    crate::state::ChangeType::Created,
                    crate::state::graph_node_summary(&state.graph, &new.id),
                )
            }
            kin_model::EntityDelta::Modified { new, .. } => {
                pass.nodes_modified += 1;
                (
                    new,
                    crate::state::ChangeType::Modified,
                    crate::state::graph_node_summary(&state.graph, &new.id),
                )
            }
            kin_model::EntityDelta::Removed { old } => {
                pass.nodes_removed += 1;
                (old, crate::state::ChangeType::Deleted, None)
            }
        };
        state.emit_event(DaemonEvent::EntityChanged {
            entity_id: entity.id,
            change_type,
            file_path: entity.file_origin.as_ref().map(|file| file.0.clone()),
            session_id: Some(session.clone()),
            node,
        });
    }
    let relations = kin_model::TransactionDelta {
        entity_deltas: Vec::new(),
        relation_deltas: change.relation_deltas.clone(),
        tree_deltas: Vec::new(),
        admission_policy_delta: None,
        external_reference_deltas: Vec::new(),
        resolution_record_deltas: Vec::new(),
    };
    for event in crate::state::relation_change_events(&relations, None) {
        pass.count(&event);
        state.emit_event(event);
    }
    if let Some(boundary) = pass.into_event() {
        state.emit_event(boundary);
    }
}

// ---------------------------------------------------------------------------
// The path-free projection
// ---------------------------------------------------------------------------

/// Whether a string reads as a filesystem path or a file name, which nothing
/// this module serves may carry.
pub fn is_path_like(text: &str) -> bool {
    const FILE_SUFFIXES: &[&str] = &[
        ".rs", ".py", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".go", ".java", ".kt",
        ".swift", ".c", ".h", ".cc", ".cpp", ".hpp", ".cs", ".rb", ".php", ".scala", ".md", ".txt",
        ".json", ".toml", ".yaml", ".yml", ".lock", ".sh", ".zsh", ".bash", ".html", ".css",
        ".sql", ".xml", ".cfg", ".ini", ".env",
    ];
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.starts_with('~')
        || trimmed.starts_with('.')
        || (trimmed.len() > 1
            && trimmed.as_bytes()[1] == b':'
            && trimmed.as_bytes()[0].is_ascii_alphabetic())
        || FILE_SUFFIXES.iter().any(|suffix| lower.ends_with(suffix))
}

/// A client-supplied name fit to show: trimmed, at most 64 characters, and
/// `fallback` when it is empty or path-shaped.
pub fn display_name(text: &str, fallback: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() || is_path_like(trimmed) {
        return fallback.to_string();
    }
    trimmed.chars().take(64).collect()
}

/// An intent's description fit to show: path-shaped words replaced by `…`,
/// whitespace collapsed, truncated to `limit` characters.
pub fn scrub_task(text: &str, limit: usize) -> String {
    let words: Vec<&str> = text
        .split_whitespace()
        .map(|word| {
            let bare = word.trim_matches(|c: char| {
                matches!(c, '"' | '\'' | '`' | '(' | ')' | ',' | ';' | ':')
            });
            if is_path_like(bare) {
                "…"
            } else {
                word
            }
        })
        .collect();
    let joined = words.join(" ");
    if joined.chars().count() <= limit {
        return joined;
    }
    let mut cut: String = joined.chars().take(limit.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

pub(crate) fn transport_name(transport: SessionTransport) -> &'static str {
    match transport {
        SessionTransport::Mcp => "mcp",
        SessionTransport::Cli => "cli",
        SessionTransport::Wrapper => "wrapper",
        SessionTransport::Ui => "ui",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentsResponse {
    pub schema: String,
    pub generated_at: Timestamp,
    pub active_window_secs: u64,
    pub agents: Vec<AgentView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentView {
    pub session_id: String,
    pub client: String,
    pub vendor: String,
    pub transport: String,
    pub started_at: Timestamp,
    pub last_activity_at: Timestamp,
    pub state: String,
    pub calls_in_flight: usize,
    pub calls_total: u64,
    pub recent_calls: Vec<ToolCallEntry>,
    pub open_transactions: Vec<TransactionView>,
    pub intents: Vec<IntentView>,
    pub recent_change_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransactionView {
    pub transaction_id: String,
    pub state: String,
    pub staged_operations: usize,
    pub opened_at: Option<Timestamp>,
    pub age_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IntentView {
    pub intent_id: String,
    pub lock_type: String,
    pub scopes: Vec<ScopeView>,
    pub withheld_scopes: usize,
    pub task: String,
    pub registered_at: Timestamp,
    pub expires_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeView {
    pub kind: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionCallsResponse {
    pub schema: String,
    pub session_id: String,
    pub calls_total: u64,
    pub retained: usize,
    pub calls: Vec<ToolCallEntry>,
    /// When the session ended, for one the daemon keeps past its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
}

/// The agents that worked in this repository and are gone, as
/// `/agents/ended` shows them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EndedAgentsResponse {
    pub schema: String,
    pub generated_at: Timestamp,
    /// How long Kin keeps a session after it ends.
    pub kept_days: u64,
    /// How many ended sessions Kin keeps at most, the newest.
    pub kept_sessions: usize,
    /// Newest activity first.
    pub agents: Vec<EndedAgentView>,
}

/// One agent session that is gone: who it was, when it worked, what it read,
/// what it wrote and the changes it committed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EndedAgentView {
    pub session_id: String,
    pub client: String,
    pub vendor: String,
    /// How it connected. Empty when Kin kept only what it wrote.
    pub transport: String,
    pub started_at: Timestamp,
    pub last_activity_at: Timestamp,
    pub ended_at: Timestamp,
    /// Whether Kin kept the session's calls. When it did not, what Kin kept
    /// is what the session wrote, and what it read is not known.
    pub calls_kept: bool,
    pub calls_total: u64,
    pub calls_retained: usize,
    /// Its newest calls, newest first.
    pub recent_calls: Vec<ToolCallEntry>,
    /// The entities its kept reads were about, newest first, each once: what
    /// each read asked about first, then what its answers named.
    pub read_entity_ids: Vec<EntityId>,
    /// How many distinct entities its kept reads were about.
    pub read_entities: usize,
    /// The entities it wrote, through Kin or with its own tools, newest
    /// first, each once.
    pub edited_entity_ids: Vec<EntityId>,
    pub edited_entities: usize,
    /// The changes it committed, newest first.
    pub change_ids: Vec<String>,
}

/// Entities an ended agent's read or write list names at most.
pub const ENDED_ENTITIES_SHOWN: usize = 24;

fn seconds_between(earlier: &Timestamp, later: &Timestamp) -> u64 {
    later
        .0
        .signed_duration_since(earlier.0)
        .num_seconds()
        .max(0)
        .unsigned_abs()
}

/// What the projection reads for one session besides the session itself.
pub struct AgentFacts<'a> {
    pub activity: &'a AgentActivity,
    pub calls_in_flight: usize,
    pub intents: Vec<kin_model::session::Intent>,
    pub transactions: Vec<kin_mcp::McpTransaction>,
    pub entity_name: &'a dyn Fn(&EntityId) -> Option<String>,
}

/// One session as `/agents` shows it. Reads nothing a caller could have made a
/// path: the working directory and process id the session registered with are
/// not read at all, and names are checked before they are shown.
pub fn agent_view(session: &AgentSession, facts: &AgentFacts<'_>, now: &Timestamp) -> AgentView {
    let (calls_total, _, recent_calls) = facts
        .activity
        .recent_calls(&session.session_id, AGENT_RECENT_CALLS);
    let mut last_activity_at = session.started_at.clone();
    if session.last_heartbeat > last_activity_at {
        last_activity_at = session.last_heartbeat.clone();
    }
    if let Some(at) = facts.activity.last_call_at(&session.session_id) {
        if at > last_activity_at {
            last_activity_at = at;
        }
    }
    let active = facts.calls_in_flight > 0
        || seconds_between(&last_activity_at, now) <= ACTIVE_WINDOW.as_secs();

    let open_transactions = facts
        .transactions
        .iter()
        .filter(|transaction| !matches!(transaction.state.as_str(), "committed" | "aborted"))
        .take(AGENT_LIST_LIMIT)
        .map(|transaction| TransactionView {
            transaction_id: display_name(&transaction.transaction_id, "transaction"),
            state: transaction.state.clone(),
            staged_operations: transaction.staged_operations.len(),
            opened_at: transaction.created_at.clone(),
            age_secs: transaction
                .created_at
                .as_ref()
                .map(|created| seconds_between(created, now)),
        })
        .collect();

    let intents = facts
        .intents
        .iter()
        .take(AGENT_LIST_LIMIT)
        .map(|intent| {
            let mut withheld_scopes = 0;
            let mut scopes = Vec::new();
            for scope in &intent.scopes {
                match scope {
                    IntentScope::Entity(id) => scopes.push(ScopeView {
                        kind: "entity".to_string(),
                        id: id.to_string(),
                        name: (facts.entity_name)(id).filter(|name| !is_path_like(name)),
                    }),
                    IntentScope::Contract(id) => scopes.push(ScopeView {
                        kind: "contract".to_string(),
                        id: id.to_string(),
                        name: None,
                    }),
                    IntentScope::Artifact(_) => withheld_scopes += 1,
                }
            }
            if scopes.len() > AGENT_LIST_LIMIT {
                withheld_scopes += scopes.len() - AGENT_LIST_LIMIT;
                scopes.truncate(AGENT_LIST_LIMIT);
            }
            IntentView {
                intent_id: intent.intent_id.to_string(),
                lock_type: match intent.lock_type {
                    LockType::Soft => "soft",
                    LockType::Hard => "hard",
                }
                .to_string(),
                scopes,
                withheld_scopes,
                task: scrub_task(&intent.task_description, TASK_TEXT_LIMIT),
                registered_at: intent.registered_at.clone(),
                expires_at: intent.expires_at.clone(),
            }
        })
        .collect();

    AgentView {
        session_id: session.session_id.to_string(),
        client: display_name(&session.client_name, "unnamed client"),
        vendor: display_name(&session.vendor, "unknown"),
        transport: transport_name(session.transport).to_string(),
        started_at: session.started_at.clone(),
        last_activity_at,
        state: if active { "active" } else { "idle" }.to_string(),
        calls_in_flight: facts.calls_in_flight,
        calls_total,
        recent_calls,
        open_transactions,
        intents,
        recent_change_ids: facts.activity.recent_changes(&session.session_id),
    }
}

fn internal(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

/// The vendor the daemon registers its own reconcile session under. That
/// session is the daemon, not an agent, so `/agents` leaves it out, as the
/// daemon's idle rule does when it counts attached clients.
pub const DAEMON_OWN_VENDOR: &str = "kin-daemon";

/// Every live agent session as `/agents` shows it, active first, newest
/// activity first within each.
pub(crate) fn agents_response(state: &DaemonState) -> Result<AgentsResponse, (StatusCode, String)> {
    let now = Timestamp::now();
    let coordinator = &state.coordinator;
    let sessions = coordinator.list_sessions().map_err(internal)?;
    let transactions: Vec<kin_mcp::McpTransaction> = state
        .mcp_transactions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .cloned()
        .collect();
    let entity_name = |id: &EntityId| -> Option<String> {
        state
            .graph
            .get_entity(id)
            .ok()
            .flatten()
            .filter(|entity| !kin_model::is_file_module_surface(entity))
            .map(|entity| entity.name)
    };
    let mut agents = Vec::with_capacity(sessions.len());
    // A session whose process is gone is an agent that left, even in the
    // moments before the sweep reaps it, so it is not shown as here.
    for session in sessions
        .iter()
        .filter(|session| session.vendor != DAEMON_OWN_VENDOR && !coordinator.process_gone(session))
    {
        let owner = session.session_id.to_string().to_ascii_lowercase();
        let mut owned: Vec<kin_mcp::McpTransaction> = transactions
            .iter()
            .filter(|transaction| transaction.session_id.to_ascii_lowercase() == owner)
            .cloned()
            .collect();
        owned.sort_by(|left, right| left.transaction_id.cmp(&right.transaction_id));
        let mut intents = coordinator
            .list_intents(&session.session_id)
            .map_err(internal)?;
        intents.sort_by(|left, right| left.registered_at.cmp(&right.registered_at));
        let facts = AgentFacts {
            activity: coordinator.activity(),
            calls_in_flight: coordinator.active_call_count(&session.session_id),
            intents,
            transactions: owned,
            entity_name: &entity_name,
        };
        agents.push(agent_view(session, &facts, &now));
    }
    agents.sort_by(|left, right| {
        (right.state == "active")
            .cmp(&(left.state == "active"))
            .then_with(|| right.last_activity_at.cmp(&left.last_activity_at))
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    Ok(AgentsResponse {
        schema: "kin.agents.v1".to_string(),
        generated_at: now,
        active_window_secs: ACTIVE_WINDOW.as_secs(),
        agents,
    })
}

/// GET /agents — the agents working in this repository, path-free.
pub(crate) async fn agents_route(
    State(state): State<Arc<DaemonState>>,
) -> Result<Json<AgentsResponse>, (StatusCode, String)> {
    tokio::task::spawn_blocking(move || agents_response(&state))
        .await
        .map_err(internal)?
        .map(Json)
}

#[derive(Debug, Deserialize)]
pub(crate) struct CallsParams {
    #[serde(default)]
    limit: Option<usize>,
}

/// GET /session/{session_id}/calls — one session's call log, newest first:
/// a live session's own, or what was kept of one that ended.
pub(crate) async fn session_calls_route(
    Path(session_id): Path<String>,
    Query(params): Query<CallsParams>,
    State(state): State<Arc<DaemonState>>,
) -> Result<Json<SessionCallsResponse>, (StatusCode, String)> {
    let uuid = Uuid::parse_str(session_id.trim()).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid session UUID: {session_id}"),
        )
    })?;
    let session_id = SessionId(uuid);
    let limit = params
        .limit
        .unwrap_or(DEFAULT_CALLS_LIMIT)
        .clamp(1, MAX_CALLS_PER_SESSION);
    if state
        .coordinator
        .get_session(&session_id)
        .map_err(internal)?
        .is_none()
    {
        // A session that ended answers from what was kept of it.
        let Some(ended) = state
            .agent_authorship
            .ended_session(&session_id.to_string(), &Timestamp::now())
        else {
            return Err((
                StatusCode::NOT_FOUND,
                format!("session not found: {session_id}"),
            ));
        };
        let retained = ended.calls.len();
        return Ok(Json(SessionCallsResponse {
            schema: "kin.agent-calls.v1".to_string(),
            session_id: session_id.to_string(),
            calls_total: ended.calls_total,
            retained,
            calls: ended.calls.into_iter().rev().take(limit).collect(),
            ended_at: Some(ended.ended_at),
        }));
    }
    let (calls_total, retained, calls) = state
        .coordinator
        .activity()
        .recent_calls(&session_id, limit);
    Ok(Json(SessionCallsResponse {
        schema: "kin.agent-calls.v1".to_string(),
        session_id: session_id.to_string(),
        calls_total,
        retained,
        calls,
        ended_at: None,
    }))
}

/// Whether a logged call read the graph, rather than opening, staging or
/// committing a change or managing its session.
pub fn is_read_tool(tool: &str) -> bool {
    !(is_lifecycle_tool(tool)
        || tool.starts_with("kin_transaction")
        || tool.starts_with("kin_session")
        || matches!(
            tool,
            "kin_mutate" | "mutate" | "transaction" | "unrecognized_tool"
        ))
}

/// What a session's kept calls read: the entity each read asked about first,
/// newest first, then the others their answers named. At most `shown`, with
/// the count of every distinct one.
pub fn read_entities(calls_newest_first: &[ToolCallEntry], shown: usize) -> (Vec<EntityId>, usize) {
    let reads: Vec<&ToolCallEntry> = calls_newest_first
        .iter()
        .filter(|call| is_read_tool(&call.tool) && !call.is_error)
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut ids = Vec::new();
    for id in reads.iter().filter_map(|call| call.entity_ids.first()) {
        if seen.insert(*id) {
            ids.push(*id);
        }
    }
    for id in reads.iter().flat_map(|call| call.entity_ids.iter()) {
        if seen.insert(*id) {
            ids.push(*id);
        }
    }
    let total = ids.len();
    ids.truncate(shown);
    (ids, total)
}

fn parse_time(at: &str) -> Option<Timestamp> {
    chrono::DateTime::parse_from_rfc3339(at.trim())
        .ok()
        .map(|at| Timestamp(at.with_timezone(&chrono::Utc)))
}

/// What a session wrote, newest first, each once, and the changes its writes
/// were published as, newest first, from the record of what agents wrote.
fn written_by(
    session: &str,
    writes: &[kin_cli::commands::review_guide::AgentWrite],
    edits: &[kin_cli::commands::review_guide::ClientEdit],
) -> (Vec<EntityId>, Vec<String>) {
    let mut entities = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut changes = Vec::new();
    let mut dated: Vec<(&str, &[EntityId], Option<&str>)> = writes
        .iter()
        .filter(|write| write.session.eq_ignore_ascii_case(session))
        .map(|write| {
            (
                write.at.as_str(),
                write.entities.as_slice(),
                Some(write.change_id.as_str()),
            )
        })
        .chain(
            edits
                .iter()
                .filter(|edit| {
                    edit.kin_session
                        .as_deref()
                        .is_some_and(|kin| kin.eq_ignore_ascii_case(session))
                })
                .map(|edit| (edit.at.as_str(), edit.entities.as_slice(), None)),
        )
        .collect();
    dated.sort_by(|left, right| {
        let at = |text: &str| parse_time(text);
        at(right.0).cmp(&at(left.0))
    });
    for (_, written, change) in dated {
        for id in written {
            if seen.insert(*id) {
                entities.push(*id);
            }
        }
        if let Some(change) = change {
            let change = change.to_ascii_lowercase();
            if !changes.contains(&change) {
                changes.push(change);
            }
        }
    }
    (entities, changes)
}

/// The agents that worked here and are gone, newest activity first: each
/// session the daemon kept past its end, each one still listed whose process
/// is gone, and each session the record of what agents wrote names that
/// neither holds. That last kind ended before its calls were kept, so only
/// what it wrote is known. All of them inside the window ended sessions are
/// kept for, and no more of them than are kept.
pub(crate) fn ended_agents_response(
    state: &DaemonState,
    calls_shown: usize,
) -> Result<EndedAgentsResponse, (StatusCode, String)> {
    use crate::ended_sessions::{within_window, ENDED_KEPT_DAYS, MAX_ENDED_SESSIONS};

    let now = Timestamp::now();
    let coordinator = &state.coordinator;
    let writes = state.agent_authorship.writes();
    let edits = state.agent_authorship.edits();
    let sessions = coordinator.list_sessions().map_err(internal)?;
    let mut agents: Vec<EndedAgentView> = Vec::new();
    let mut known: std::collections::HashSet<String> = std::collections::HashSet::new();

    let view = |session_id: &str,
                vendor: &str,
                client: &str,
                transport: &str,
                started_at: Timestamp,
                last_activity_at: Timestamp,
                ended_at: Timestamp,
                calls: Option<(u64, Vec<ToolCallEntry>)>,
                changes: Vec<String>|
     -> EndedAgentView {
        let (written, written_changes) = written_by(session_id, &writes, &edits);
        let mut change_ids = changes;
        for change in written_changes {
            if !change_ids.contains(&change) {
                change_ids.push(change);
            }
        }
        change_ids.truncate(crate::ended_sessions::MAX_ENDED_CHANGES);
        let calls_kept = calls.is_some();
        let (calls_total, newest_first) = calls.unwrap_or_default();
        let (read_entity_ids, read_entities) = read_entities(&newest_first, ENDED_ENTITIES_SHOWN);
        let edited_entities = written.len();
        EndedAgentView {
            session_id: session_id.to_ascii_lowercase(),
            client: client.to_string(),
            vendor: vendor.to_string(),
            transport: transport.to_string(),
            started_at,
            last_activity_at,
            ended_at,
            calls_kept,
            calls_total,
            calls_retained: newest_first.len(),
            recent_calls: newest_first.into_iter().take(calls_shown).collect(),
            read_entity_ids,
            read_entities,
            edited_entity_ids: written.into_iter().take(ENDED_ENTITIES_SHOWN).collect(),
            edited_entities,
            change_ids,
        }
    };

    // Still listed, but its process is gone: it left, and the next sweep
    // reaps it. Its log is still the live one.
    for session in sessions
        .iter()
        .filter(|session| session.vendor != DAEMON_OWN_VENDOR && coordinator.process_gone(session))
    {
        let (calls_total, _, newest_first) = coordinator
            .activity()
            .recent_calls(&session.session_id, MAX_CALLS_PER_SESSION);
        let changes = coordinator.activity().recent_changes(&session.session_id);
        if calls_total == 0 && changes.is_empty() {
            continue;
        }
        let mut last = session.started_at.clone();
        if session.last_heartbeat > last {
            last = session.last_heartbeat.clone();
        }
        let last = coordinator
            .activity()
            .last_call_at(&session.session_id)
            .unwrap_or(last);
        let id = session.session_id.to_string();
        known.insert(id.to_ascii_lowercase());
        agents.push(view(
            &id,
            &display_name(&session.vendor, "unknown"),
            &display_name(&session.client_name, "unnamed client"),
            transport_name(session.transport),
            session.started_at.clone(),
            last.clone(),
            last,
            Some((calls_total, newest_first)),
            changes,
        ));
    }
    // Every session still listed is here now, or was just shown as gone.
    for session in &sessions {
        known.insert(session.session_id.to_string().to_ascii_lowercase());
    }

    for ended in state.agent_authorship.ended_sessions(&now) {
        let id = ended.session_id.to_ascii_lowercase();
        if !known.insert(id.clone()) {
            continue;
        }
        let mut newest_first = ended.calls;
        newest_first.reverse();
        agents.push(view(
            &id,
            &ended.vendor,
            &ended.client,
            &ended.transport,
            ended.started_at,
            ended.last_activity_at,
            ended.ended_at,
            Some((ended.calls_total, newest_first)),
            ended.change_ids,
        ));
    }

    // Sessions known only by what they wrote: the newest write names them.
    let mut by_session: Vec<(String, Timestamp, Timestamp, String, String)> = Vec::new();
    for write in writes.iter().rev() {
        let id = write.session.trim().to_ascii_lowercase();
        if known.contains(&id) || !within_window(&write.at, &now) {
            continue;
        }
        let Some(at) = parse_time(&write.at) else {
            continue;
        };
        match by_session.iter_mut().find(|(session, ..)| *session == id) {
            Some((_, first, last, ..)) => {
                if at < *first {
                    *first = at;
                } else if at > *last {
                    *last = at;
                }
            }
            None => by_session.push((
                id,
                at.clone(),
                at,
                write.vendor.clone(),
                write.client.clone(),
            )),
        }
    }
    for (id, first, last, vendor, client) in by_session {
        if vendor == DAEMON_OWN_VENDOR {
            continue;
        }
        agents.push(view(
            &id,
            &display_name(&vendor, "unknown"),
            &display_name(&client, "unnamed client"),
            "",
            first,
            last.clone(),
            last,
            None,
            Vec::new(),
        ));
    }

    agents.sort_by(|left, right| {
        right
            .last_activity_at
            .cmp(&left.last_activity_at)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    agents.truncate(MAX_ENDED_SESSIONS);
    Ok(EndedAgentsResponse {
        schema: "kin.agents-ended.v1".to_string(),
        generated_at: now,
        kept_days: ENDED_KEPT_DAYS,
        kept_sessions: MAX_ENDED_SESSIONS,
        agents,
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct EndedParams {
    /// The newest calls shown for each agent.
    #[serde(default)]
    calls: Option<usize>,
}

/// GET /agents/ended — the agents that worked here and are gone, path-free.
pub(crate) async fn ended_agents_route(
    Query(params): Query<EndedParams>,
    State(state): State<Arc<DaemonState>>,
) -> Result<Json<EndedAgentsResponse>, (StatusCode, String)> {
    let shown = params
        .calls
        .unwrap_or(AGENT_RECENT_CALLS)
        .min(crate::ended_sessions::MAX_ENDED_CALLS);
    tokio::task::spawn_blocking(move || ended_agents_response(&state, shown))
        .await
        .map_err(internal)?
        .map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::session::{Intent, SessionCapabilities};

    fn draft(tool: &str) -> ToolCallDraft {
        ToolCallDraft {
            tool: tool.to_string(),
            entity_ids: vec![EntityId::new()],
            result_count: 1,
            bytes: 10,
            duration_ms: 3,
            is_error: false,
            repo_id: None,
            change_id: None,
            caller_left: false,
        }
    }

    fn entry(seq: u64) -> ToolCallEntry {
        ToolCallEntry {
            seq,
            at: Timestamp::now(),
            tool: "semantic_locate".to_string(),
            entity_ids: Vec::new(),
            result_count: 0,
            bytes: 0,
            duration_ms: 0,
            is_error: false,
            repo_id: None,
            change_id: None,
            caller_left: false,
        }
    }

    #[test]
    fn activity_preserves_every_registered_tool_name() {
        for definition in kin_mcp::tools::tool_definitions().tools {
            assert!(definition.name.len() <= MAX_TOOL_NAME_BYTES);
            assert!(definition
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'));
            assert_eq!(activity_tool_name(&definition.name), definition.name);
        }
    }

    #[test]
    fn refused_tool_names_never_reach_activity_text_or_retained_logs() {
        let activity = AgentActivity::default();
        let session = SessionId::new();
        let oversized = "x".repeat(4 * 1024 * 1024);
        let unicode = "界".repeat(MAX_TOOL_NAME_BYTES);
        let at_limit = "x".repeat(MAX_TOOL_NAME_BYTES);
        let beyond_limit = "x".repeat(MAX_TOOL_NAME_BYTES + 1);
        for tool in [
            oversized.as_str(),
            unicode.as_str(),
            at_limit.as_str(),
            beyond_limit.as_str(),
            "/private/repository/query",
            "C:\\private\\repository",
            "private\r\nquery",
            "unknown_tool",
            "locate\u{1b}[31m",
        ] {
            let result = kin_mcp::ToolCallResult::error("refused".to_string());
            let call = draft_for_call(tool, &[], &result, Duration::ZERO, |_| false);
            assert_eq!(call.tool, "unrecognized_tool");
            let entry = activity.record_call(&session, draft(tool), Timestamp::now());
            assert_eq!(entry.tool, "unrecognized_tool");
            assert!(entry.tool.len() <= MAX_TOOL_NAME_BYTES);
            let DaemonEvent::ToolCall {
                tool: event_tool, ..
            } = ToolCallEvent {
                session_id: session,
                entry,
                coalesced: 0,
            }
            .into_daemon_event()
            else {
                panic!("expected a tool call event");
            };
            assert_eq!(event_tool, "unrecognized_tool");
        }
        let (_, retained, calls) = activity.recent_calls(&session, 10);
        assert_eq!(retained, 9);
        assert!(calls.iter().all(|call| call.tool == "unrecognized_tool"));
    }

    #[test]
    fn one_session_keeps_its_newest_calls_and_counts_every_one() {
        let activity = AgentActivity::default();
        let session = SessionId::new();
        for _ in 0..(MAX_CALLS_PER_SESSION + 37) {
            activity.record_call(&session, draft("find_references"), Timestamp::now());
        }
        let (total, retained, newest) = activity.recent_calls(&session, 3);
        assert_eq!(total, (MAX_CALLS_PER_SESSION + 37) as u64);
        assert_eq!(retained, MAX_CALLS_PER_SESSION);
        assert_eq!(
            newest.iter().map(|call| call.seq).collect::<Vec<_>>(),
            vec![total, total - 1, total - 2],
            "newest first, numbered by position in the session"
        );
        assert_eq!(activity.retained_entries(), MAX_CALLS_PER_SESSION);
    }

    #[test]
    fn all_logs_together_stay_under_the_global_bound() {
        let activity = AgentActivity::default();
        let sessions: Vec<SessionId> = (0..40).map(|_| SessionId::new()).collect();
        for round in 0..MAX_CALLS_PER_SESSION {
            for session in &sessions {
                activity.record_call(session, draft("semantic_locate"), Timestamp::now());
            }
            assert!(
                activity.retained_entries() <= MAX_CALL_LOG_ENTRIES,
                "round {round}: {} entries",
                activity.retained_entries()
            );
        }
        assert_eq!(activity.retained_entries(), MAX_CALL_LOG_ENTRIES);
        // The oldest entries went first, so every session still holds its
        // newest calls and its full count.
        for session in &sessions {
            let (total, retained, newest) = activity.recent_calls(session, 1);
            assert_eq!(total, MAX_CALLS_PER_SESSION as u64);
            assert!(retained >= MAX_CALL_LOG_ENTRIES / sessions.len() - 1);
            assert_eq!(newest[0].seq, total);
        }
    }

    #[test]
    fn the_number_of_logged_sessions_is_bounded() {
        let activity = AgentActivity::default();
        let first = SessionId::new();
        activity.record_call(&first, draft("semantic_locate"), Timestamp::now());
        for _ in 0..MAX_CALL_LOG_SESSIONS {
            activity.record_call(
                &SessionId::new(),
                draft("semantic_locate"),
                Timestamp::now(),
            );
        }
        assert_eq!(activity.logged_sessions(), MAX_CALL_LOG_SESSIONS);
        assert_eq!(
            activity.recent_calls(&first, 10).0,
            0,
            "the session heard from least recently lost its log"
        );
    }

    #[test]
    fn an_ended_session_leaves_nothing_behind() {
        let activity = AgentActivity::default();
        let session = SessionId::new();
        activity.record_call(&session, draft("semantic_locate"), Timestamp::now());
        activity.record_change(&session, "ab".repeat(32));
        activity.forget(&session);
        assert_eq!(activity.retained_entries(), 0);
        assert_eq!(activity.recent_calls(&session, 10), (0, 0, Vec::new()));
        assert!(activity.recent_changes(&session).is_empty());
        assert!(activity
            .record_call_if(&session, draft("semantic_locate"), Timestamp::now(), || {
                false
            })
            .is_none());
        assert_eq!(activity.logged_sessions(), 0);
    }

    #[test]
    fn changes_are_kept_newest_first_and_bounded() {
        let activity = AgentActivity::default();
        let session = SessionId::new();
        for index in 0..(MAX_CHANGES_PER_SESSION + 4) {
            activity.record_change(&session, format!("{index:064x}"));
        }
        let changes = activity.recent_changes(&session);
        assert_eq!(changes.len(), MAX_CHANGES_PER_SESSION);
        assert_eq!(changes[0], format!("{:064x}", MAX_CHANGES_PER_SESSION + 3));
    }

    #[test]
    fn a_burst_from_one_session_becomes_bounded_events_and_the_last_call_is_delivered() {
        let gate = AgentActivity::default();
        let session = SessionId::new();
        let start = Instant::now();
        let mut emitted = Vec::new();
        let mut flushers = 0;
        // 100 calls inside 100 ms.
        for index in 0..100u64 {
            match gate.offer_event(
                session,
                entry(index + 1),
                start + Duration::from_millis(index),
            ) {
                EventOffer::Emit(event) => emitted.push(event),
                EventOffer::Held { start_flusher } => flushers += usize::from(start_flusher),
            }
        }
        assert_eq!(emitted.len(), 1, "the first call goes out at once");
        assert_eq!(flushers, 1, "exactly one flush is started for the burst");
        // Nothing may go before the session's interval has passed.
        let (early, more) = gate.drain_events(start + Duration::from_millis(120));
        assert!(early.is_empty() && more);
        let (late, more) =
            gate.drain_events(start + TOOL_CALL_EVENT_MIN_INTERVAL + Duration::from_millis(1));
        assert!(!more, "the flush ends once nothing is held");
        assert_eq!(late.len(), 1);
        assert_eq!(
            late[0].entry.seq, 100,
            "the newest call of the burst is the one delivered"
        );
        assert_eq!(
            late[0].coalesced, 98,
            "it stands for every call that got no event"
        );
        let delivered: u64 = emitted
            .iter()
            .chain(late.iter())
            .map(|event| 1 + u64::from(event.coalesced))
            .sum();
        assert_eq!(delivered, 100, "every call is accounted for by some event");
    }

    #[test]
    fn many_sessions_together_stay_under_the_global_event_rate() {
        let gate = AgentActivity::default();
        let sessions: Vec<SessionId> = (0..50).map(|_| SessionId::new()).collect();
        let start = Instant::now();
        let mut sent: Vec<Instant> = Vec::new();
        // Every session calls every 10 ms for two seconds; a flush drains every
        // 50 ms, as the running one does.
        for tick in 0..200u64 {
            let now = start + Duration::from_millis(tick * 10);
            for (index, session) in sessions.iter().enumerate() {
                if let EventOffer::Emit(_) =
                    gate.offer_event(*session, entry(tick * 100 + index as u64), now)
                {
                    sent.push(now);
                }
            }
            if tick % 5 == 0 {
                let (ready, _) = gate.drain_events(now);
                sent.extend(ready.iter().map(|_| now));
            }
        }
        for (index, at) in sent.iter().enumerate() {
            let in_window = sent[index..]
                .iter()
                .take_while(|later| later.saturating_duration_since(*at) < EVENT_WINDOW)
                .count();
            assert!(
                in_window <= TOOL_CALL_EVENTS_PER_SECOND,
                "{in_window} events inside one second"
            );
        }
        assert!(!sent.is_empty());
    }

    #[test]
    fn ids_are_read_only_where_a_uuid_stands_alone_and_the_graph_holds_it() {
        let known = EntityId::new();
        let also_known = EntityId::new();
        let unknown = EntityId::new();
        let arguments: HashMap<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "entity_id": known.to_string(),
                "query": format!("where is {} used /srv/repo/src/app.py", unknown),
                "limit": 5,
            }))
            .unwrap();
        let answer = serde_json::json!({
            "results": [
                {"entity_id": also_known.to_string(), "name": "dumps"},
                {"entity_id": unknown.to_string()},
                {"entity_id": also_known.to_string()},
            ],
            "glued": format!("{}{}", known, "-abc"),
        });
        let result = kin_mcp::ToolCallResult::text(answer.to_string());
        let draft = draft_for_call(
            "find_references",
            &argument_uuids(&arguments),
            &result,
            Duration::from_millis(12),
            |id| *id == known || *id == also_known,
        );
        assert_eq!(draft.entity_ids, vec![known, also_known]);
        assert_eq!(
            draft.result_count, 1,
            "the answer named one entity the graph holds"
        );
        assert_eq!(draft.bytes, answer.to_string().len());
        assert_eq!(draft.duration_ms, 12);
        let serialized = serde_json::to_string(&draft.entity_ids).unwrap();
        assert!(!serialized.contains("srv") && !serialized.contains("where is"));
    }

    #[test]
    fn the_id_scan_is_bounded() {
        let ids: Vec<String> = (0..(MAX_ENTITY_ID_CANDIDATES + 50))
            .map(|_| Uuid::new_v4().to_string())
            .collect();
        let mut found = Vec::new();
        uuid_candidates(&ids.join(","), MAX_ENTITY_ID_CANDIDATES, &mut found);
        assert_eq!(found.len(), MAX_ENTITY_ID_CANDIDATES);
        let result = kin_mcp::ToolCallResult::text(ids.join(" "));
        let draft = draft_for_call("semantic_search", &[], &result, Duration::ZERO, |_| true);
        assert_eq!(draft.entity_ids.len(), MAX_ENTITY_IDS_PER_CALL);
        assert_eq!(draft.result_count, MAX_ENTITY_ID_CANDIDATES);
    }

    fn session_named(client_name: &str, vendor: &str) -> AgentSession {
        AgentSession {
            session_id: SessionId::new(),
            vendor: vendor.to_string(),
            client_name: client_name.to_string(),
            transport: SessionTransport::Mcp,
            pid: Some(4_000_000_123),
            cwd: std::path::PathBuf::from("/srv/private-checkout"),
            started_at: Timestamp::now(),
            last_heartbeat: Timestamp::now(),
            capabilities: SessionCapabilities::default(),
        }
    }

    fn every_string(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::String(text) => out.push(text.clone()),
            serde_json::Value::Array(items) => {
                items.iter().for_each(|item| every_string(item, out))
            }
            serde_json::Value::Object(fields) => {
                for (key, item) in fields {
                    out.push(key.clone());
                    every_string(item, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn the_projection_carries_no_directory_process_or_path() {
        let activity = AgentActivity::default();
        let session = session_named("claude-code", "claude-code");
        let target = EntityId::new();
        let mut call = draft("impact_analysis");
        call.entity_ids = vec![target];
        activity.record_call(&session.session_id, call, Timestamp::now());
        activity.record_change(&session.session_id, "c".repeat(64));
        let intents = vec![Intent {
            intent_id: kin_model::ids::IntentId::new(),
            session_id: session.session_id,
            scopes: vec![
                IntentScope::Entity(target),
                IntentScope::Artifact(kin_model::FilePathId("src/itsdangerous/signer.py".into())),
            ],
            lock_type: LockType::Hard,
            task_description:
                "Tighten defaults in /srv/private-checkout/src/signer.py and signer.py now"
                    .to_string(),
            registered_at: Timestamp::now(),
            expires_at: None,
        }];
        let transactions = vec![kin_mcp::McpTransaction {
            transaction_id: "tx-1".to_string(),
            session_id: session.session_id.to_string(),
            scope: "src/itsdangerous/signer.py".to_string(),
            state: "active".to_string(),
            staged_operations: Vec::new(),
            created_at: Some(Timestamp::now()),
            commit_payload_hash: None,
            last_activity_at: Timestamp::now(),
        }];
        let name = |id: &EntityId| (*id == target).then(|| "Signer.sign".to_string());
        let facts = AgentFacts {
            activity: &activity,
            calls_in_flight: 0,
            intents,
            transactions,
            entity_name: &name,
        };
        let view = agent_view(&session, &facts, &Timestamp::now());
        assert_eq!(view.state, "active");
        assert_eq!(view.calls_total, 1);
        assert_eq!(view.intents[0].withheld_scopes, 1);
        assert_eq!(view.intents[0].scopes.len(), 1);
        assert_eq!(
            view.intents[0].scopes[0].name.as_deref(),
            Some("Signer.sign")
        );
        assert_eq!(view.open_transactions[0].staged_operations, 0);

        let value = serde_json::to_value(&view).unwrap();
        let mut strings = Vec::new();
        every_string(&value, &mut strings);
        for text in &strings {
            assert!(
                !text.contains('/') && !text.contains('\\'),
                "path-shaped: {text}"
            );
            assert!(
                !text.contains("private-checkout") && !text.contains("srv"),
                "{text}"
            );
            assert!(!text.ends_with(".py"), "file-shaped: {text}");
        }
        assert!(value.get("cwd").is_none() && value.get("pid").is_none());
        // Ten digits, which no timestamp's digit run reaches.
        assert!(
            !value.to_string().contains("4000000123"),
            "the process id is not served"
        );
    }

    #[test]
    fn a_path_shaped_client_name_is_withheld() {
        let activity = AgentActivity::default();
        let session = session_named("/opt/tools/bin/agent", "C:\\agents\\bot");
        let none = |_: &EntityId| None;
        let facts = AgentFacts {
            activity: &activity,
            calls_in_flight: 0,
            intents: Vec::new(),
            transactions: Vec::new(),
            entity_name: &none,
        };
        let view = agent_view(&session, &facts, &Timestamp::now());
        assert_eq!(view.client, "unnamed client");
        assert_eq!(view.vendor, "unknown");
    }

    #[test]
    fn an_old_quiet_session_reads_idle_and_a_busy_one_active() {
        let activity = AgentActivity::default();
        let mut session = session_named("codex", "codex");
        let long_ago = Timestamp(chrono::Utc::now() - chrono::TimeDelta::try_seconds(600).unwrap());
        session.started_at = long_ago.clone();
        session.last_heartbeat = long_ago;
        let none = |_: &EntityId| None;
        let quiet = AgentFacts {
            activity: &activity,
            calls_in_flight: 0,
            intents: Vec::new(),
            transactions: Vec::new(),
            entity_name: &none,
        };
        assert_eq!(
            agent_view(&session, &quiet, &Timestamp::now()).state,
            "idle"
        );
        let busy = AgentFacts {
            calls_in_flight: 1,
            ..quiet
        };
        assert_eq!(
            agent_view(&session, &busy, &Timestamp::now()).state,
            "active"
        );
    }

    #[test]
    fn scrubbing_keeps_words_and_drops_paths() {
        assert_eq!(
            scrub_task("rename `dumps` in src/app.py (see ~/notes)", 140),
            "rename `dumps` in … (see …"
        );
        assert_eq!(scrub_task(&"word ".repeat(100), 20).chars().count(), 20);
        assert!(!is_path_like("Signer.sign"));
        assert!(is_path_like("signer.py"));
        assert!(is_path_like("C:\\repo"));
    }

    #[test]
    fn the_change_id_comes_only_from_a_successful_commit_answer() {
        let id = "a".repeat(64);
        let ok = kin_mcp::ToolCallResult::text(serde_json::json!({"change_id": id}).to_string());
        assert_eq!(committed_change_id(&ok), Some(id.clone()));
        let mut failed = ok.clone();
        failed.is_error = Some(true);
        assert_eq!(committed_change_id(&failed), None);
        let odd = kin_mcp::ToolCallResult::text(r#"{"change_id":"../../etc"}"#);
        assert_eq!(committed_change_id(&odd), None);
    }
}
