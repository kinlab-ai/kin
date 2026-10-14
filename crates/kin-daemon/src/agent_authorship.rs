// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! What agents did in this repository, kept in repository authority.
//!
//! A write through Kin lands as a recorded change, and the change names the
//! session that made it as its author. The change can hold more than that
//! session wrote: work it carried in from the working copy, an edit beside the
//! written entity in the same source unit, and entities whose span only moved.
//! Attributing every entity a change holds to its author would name the agent
//! for edits it never made, and the review that is sent back to that agent
//! would ask it about them.
//!
//! So this keeps, for each change an agent published, the entities its own
//! operations wrote: the entity a patch, an update, a removal or a content
//! replacement named, and the entity a create declared, each only when the
//! change holds a delta for it. Read from the operations at the moment the
//! commit lands, which is the only time anybody knows which of the change's
//! entities they named. Beside those writes it keeps the edits agents made
//! with their own tools, which [`crate::client_edits`] confirms and hands
//! over, and what each agent session did once it ended: the tools it called,
//! the entities each call was about and the changes it committed.
//!
//! All of it is collaboration authority. Each agent session is an actor, and
//! the actor carries the session's [`AgentSessionRecord`]; an agent that
//! edited with its own tools is an actor for the session its client named. A
//! record is held in memory the moment it is known, so the next read shows
//! it, and committed to repository authority as one transaction once the
//! session it belongs to has ended or gone quiet ([`settle`]), the way review
//! flag sightings are: a write is a repository transaction that moves the
//! authority generation, and an agent's own work must not see one start in
//! the middle of it. Only once authority holds a record does the live graph
//! show it, so a daemon that reopens the store reads back exactly what this
//! one wrote. Records hold change and session ids, the agents' names, tool
//! names and entity ids, never a path.
//!
//! Authority has no removal, so the bounds are kept by rewriting: past
//! [`AGENT_WRITES_MAX`] writes or [`AGENT_EDITS_MAX`] edits the oldest are
//! dropped from the records that hold them, and a session that ended outside
//! the window [`crate::ended_sessions`] keeps keeps no calls.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kin_cli::commands::review_guide::{AgentWrite, ClientEdit};
use kin_model::change::EntityDelta;
use kin_model::timestamp::Timestamp;
use kin_model::ProvenanceStore as _;
use kin_model::{
    Actor, ActorId, ActorKind, AgentCall, AgentCallOutcome, AgentEditRecord, AgentSessionRecord,
    AgentSessionSource, AgentWriteRecord, CollaborationDelta, Entity, EntityId, FilePathId,
    Hash256, Keyed, SemanticChange, AGENT_EDITS_MAX, AGENT_ENTITIES_PER_WRITE_MAX,
    AGENT_WRITES_MAX,
};
use sha2::{Digest, Sha256};
use tracing::debug;

use crate::agent_activity::ToolCallEntry;
use crate::ended_sessions::EndedSession;
use crate::DaemonState;

/// The writes through Kin kept. Past it the oldest go.
pub const MAX_AUTHORED_WRITES: usize = AGENT_WRITES_MAX;
/// The entities one write keeps.
pub const MAX_ENTITIES_PER_WRITE: usize = AGENT_ENTITIES_PER_WRITE_MAX;
/// The edits made with an agent's own tools kept. Past it the oldest go.
pub const MAX_CLIENT_EDITS: usize = AGENT_EDITS_MAX;

/// How long a session must have been quiet before what is held for it is
/// written. A session that has ended is written at the next look.
#[cfg(not(test))]
const QUIET_BEFORE_WRITE: Duration = Duration::from_secs(20);
#[cfg(test)]
const QUIET_BEFORE_WRITE: Duration = Duration::from_millis(150);

/// How often the daemon looks at held records ([`crate::daemon`]).
#[cfg(not(test))]
pub(crate) const SETTLE_TICK: Duration = Duration::from_secs(5);
#[cfg(test)]
pub(crate) const SETTLE_TICK: Duration = Duration::from_millis(40);

/// What a transaction that keeps agent records is, and who it is by.
const RECORD_ACTION: &str = "agent_session.kept";
const RECORD_ACTOR: &str = "kin-daemon:agent-sessions";

/// The actor that stands for a session connected through Kin.
pub(crate) fn session_actor_id(session: &str) -> ActorId {
    let mut hasher = Sha256::new();
    hasher.update(b"kin-agent-session-record-v1\0");
    hasher.update(session.trim().to_ascii_lowercase().as_bytes());
    ActorId::from_hash(Hash256::from_bytes(hasher.finalize().into()))
}

/// The actor that stands for a session a client named when it reported edits
/// made with its own tools.
pub(crate) fn client_actor_id(client: &str, session: &str) -> ActorId {
    let mut hasher = Sha256::new();
    hasher.update(b"kin-agent-client-session-record-v1\0");
    hasher.update(client.as_bytes());
    hasher.update(b"\0");
    hasher.update(session.as_bytes());
    ActorId::from_hash(Hash256::from_bytes(hasher.finalize().into()))
}

fn parse_time(at: &str) -> Option<Timestamp> {
    chrono::DateTime::parse_from_rfc3339(at.trim())
        .ok()
        .map(|at| Timestamp(at.with_timezone(&chrono::Utc)))
}

/// The actor that carries `record`.
fn actor_for(actor_id: ActorId, record: AgentSessionRecord) -> Actor {
    let display_name = [record.vendor.as_str(), record.client.as_str()]
        .into_iter()
        .find(|name| !name.is_empty())
        .unwrap_or("agent session")
        .to_string();
    Actor {
        actor_id,
        kind: ActorKind::Assistant,
        display_name,
        external_refs: Vec::new(),
        agent_session: Some(record),
    }
}

/// One record held in memory, not yet in authority.
#[derive(Debug, Clone)]
struct HeldRecord {
    record: AgentSessionRecord,
    /// Moves on every change, so a write that raced a change keeps it held.
    version: u64,
    touched: Instant,
}

#[derive(Debug, Default)]
struct Held {
    records: HashMap<ActorId, HeldRecord>,
    next_version: u64,
    /// The store takes no local write, as a hosted daemon's does not: what is
    /// known is kept in memory only.
    memory_only: bool,
}

/// What agents did here, as authority holds it and as it is held to be
/// written.
#[derive(Debug)]
pub struct AgentAuthorship {
    /// The live graph, which holds what authority holds.
    graph: Arc<kin_db::InMemoryGraph>,
    held: Mutex<Held>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Every record, `held` over what the graph holds, in actor order.
fn records_in(
    graph: &kin_db::InMemoryGraph,
    held: &Held,
) -> BTreeMap<String, (ActorId, AgentSessionRecord)> {
    let mut all = BTreeMap::new();
    for actor in graph.list_actors().unwrap_or_default() {
        if let Some(record) = actor.agent_session {
            all.insert(actor.actor_id.0.to_string(), (actor.actor_id, record));
        }
    }
    for (actor_id, held) in &held.records {
        all.insert(actor_id.0.to_string(), (*actor_id, held.record.clone()));
    }
    all
}

fn call_record(entry: &ToolCallEntry) -> AgentCall {
    AgentCall {
        seq: entry.seq,
        at: entry.at.clone(),
        tool: entry.tool.clone(),
        entity_ids: entry.entity_ids.clone(),
        result_count: u64::try_from(entry.result_count).unwrap_or(u64::MAX),
        bytes: u64::try_from(entry.bytes).unwrap_or(u64::MAX),
        duration_ms: entry.duration_ms,
        is_error: entry.is_error,
        outcome: AgentCallOutcome::of(entry.change_id.clone(), entry.caller_left),
    }
}

fn call_entry(call: &AgentCall) -> ToolCallEntry {
    ToolCallEntry {
        seq: call.seq,
        at: call.at.clone(),
        tool: call.tool.clone(),
        entity_ids: call.entity_ids.clone(),
        result_count: usize::try_from(call.result_count).unwrap_or(usize::MAX),
        bytes: usize::try_from(call.bytes).unwrap_or(usize::MAX),
        duration_ms: call.duration_ms,
        is_error: call.is_error,
        repo_id: None,
        change_id: call
            .outcome
            .as_ref()
            .and_then(|outcome| outcome.change_id.clone()),
        caller_left: call
            .outcome
            .as_ref()
            .is_some_and(|outcome| outcome.caller_left),
    }
}

impl AgentAuthorship {
    /// What agents did, read from what `graph` holds, which is what authority
    /// holds once a daemon has opened the store.
    pub fn new(graph: Arc<kin_db::InMemoryGraph>) -> Self {
        Self {
            graph,
            held: Mutex::new(Held::default()),
        }
    }

    /// Change the record under `actor_id`, starting it with `start` when
    /// there is none. `change` says whether it changed anything; a record that
    /// changed is held until it is written.
    fn update(
        &self,
        actor_id: ActorId,
        start: impl FnOnce() -> AgentSessionRecord,
        change: impl FnOnce(&mut AgentSessionRecord) -> bool,
    ) {
        let mut held = lock(&self.held);
        let mut record = match held.records.get(&actor_id) {
            Some(kept) => kept.record.clone(),
            None => self
                .graph
                .get_actor(&actor_id)
                .ok()
                .flatten()
                .and_then(|actor| actor.agent_session)
                .unwrap_or_else(start),
        };
        if !change(&mut record) {
            return;
        }
        record.bound();
        held.next_version += 1;
        let version = held.next_version;
        held.records.insert(
            actor_id,
            HeldRecord {
                record,
                version,
                touched: Instant::now(),
            },
        );
    }

    /// Every record, as authority holds it with what is held over it.
    fn records(&self) -> Vec<(ActorId, AgentSessionRecord)> {
        let held = lock(&self.held);
        records_in(&self.graph, &held).into_values().collect()
    }

    /// Keep one write. A change already recorded is not recorded twice, as a
    /// resumed commit finalizes the change it already published. A write that
    /// wrote no entity is not kept: there is nothing it could be shown on.
    pub fn record(&self, write: AgentWrite) {
        if write.entities.is_empty() {
            return;
        }
        let known = self.records().iter().any(|(_, record)| {
            record
                .writes
                .iter()
                .any(|kept| kept.change_id.eq_ignore_ascii_case(&write.change_id))
        });
        if known {
            return;
        }
        let session = write.session.trim().to_ascii_lowercase();
        self.update(
            session_actor_id(&session),
            || AgentSessionRecord::new(&session, AgentSessionSource::Kin),
            |record| {
                if record.vendor.is_empty() {
                    record.vendor = write.vendor.clone();
                }
                if record.client.is_empty() {
                    record.client = write.client.clone();
                }
                let mut entities = write.entities.clone();
                entities.truncate(MAX_ENTITIES_PER_WRITE);
                record.writes.push(AgentWriteRecord {
                    change_id: write.change_id.to_ascii_lowercase(),
                    at: parse_time(&write.at).unwrap_or_else(Timestamp::now),
                    entities,
                });
                true
            },
        );
    }

    /// Every write kept, oldest first, the newest [`MAX_AUTHORED_WRITES`].
    pub fn writes(&self) -> Vec<AgentWrite> {
        let mut writes: Vec<(Timestamp, AgentWrite)> = Vec::new();
        for (_, record) in self.records() {
            for write in &record.writes {
                writes.push((
                    write.at.clone(),
                    AgentWrite {
                        change_id: write.change_id.clone(),
                        session: record.session.clone(),
                        vendor: record.vendor.clone(),
                        client: record.client.clone(),
                        at: write.at.to_string(),
                        entities: write.entities.clone(),
                    },
                ));
            }
        }
        writes.sort_by(|left, right| left.0.cmp(&right.0));
        let skip = writes.len().saturating_sub(MAX_AUTHORED_WRITES);
        writes
            .into_iter()
            .skip(skip)
            .map(|(_, write)| write)
            .collect()
    }

    /// Keep one edit an agent made with its own tools. One that changed no
    /// entity is not kept.
    pub fn record_edit(&self, edit: ClientEdit) {
        if edit.entities.is_empty() {
            return;
        }
        self.update(
            client_actor_id(&edit.client, &edit.session),
            || {
                let mut record = AgentSessionRecord::new(&edit.session, AgentSessionSource::Client);
                record.client = edit.client.clone();
                record
            },
            |record| {
                record.edits.push(edit_record(&edit));
                true
            },
        );
    }

    /// Every edit kept, oldest first, the newest [`MAX_CLIENT_EDITS`].
    pub fn edits(&self) -> Vec<ClientEdit> {
        edits_of(&self.records())
    }

    /// Rewrite the edits kept with `seal`, which returns whether it changed
    /// any, and keep the result. An edit left with no entity is dropped.
    pub fn reseal_edits(&self, seal: impl FnOnce(&mut Vec<ClientEdit>) -> bool) {
        let mut held = lock(&self.held);
        let records: Vec<(ActorId, AgentSessionRecord)> =
            records_in(&self.graph, &held).into_values().collect();
        let mut edits = edits_of(&records);
        if !seal(&mut edits) {
            return;
        }
        edits.retain(|edit| !edit.entities.is_empty());
        let mut regrouped: HashMap<ActorId, Vec<AgentEditRecord>> = HashMap::new();
        for edit in &edits {
            regrouped
                .entry(client_actor_id(&edit.client, &edit.session))
                .or_default()
                .push(edit_record(edit));
        }
        for (actor_id, record) in records {
            if record.source != AgentSessionSource::Client {
                continue;
            }
            let now_held = regrouped.remove(&actor_id).unwrap_or_default();
            if now_held == record.edits {
                continue;
            }
            let mut record = record;
            record.edits = now_held;
            record.bound();
            held.next_version += 1;
            let version = held.next_version;
            held.records.insert(
                actor_id,
                HeldRecord {
                    record,
                    version,
                    touched: Instant::now(),
                },
            );
        }
    }

    /// Keep what one session's log held when it ended.
    pub fn hold_ended(&self, ended: EndedSession) {
        let session = ended.session_id.trim().to_ascii_lowercase();
        self.update(
            session_actor_id(&session),
            || AgentSessionRecord::new(&session, AgentSessionSource::Kin),
            |record| {
                record.vendor = ended.vendor.clone();
                record.client = ended.client.clone();
                record.transport = ended.transport.clone();
                record.started_at = Some(ended.started_at.clone());
                record.last_activity_at = Some(ended.last_activity_at.clone());
                record.ended_at = Some(ended.ended_at.clone());
                record.calls_total = ended.calls_total;
                record.calls = ended.calls.iter().map(call_record).collect();
                let mut changes = ended.change_ids.clone();
                for change in &record.change_ids {
                    if !changes.contains(change) {
                        changes.push(change.clone());
                    }
                }
                record.change_ids = changes;
                true
            },
        );
    }

    /// Keep one call a session made that finished after the session ended:
    /// a commit that ran on after its agent stopped waiting and closed its
    /// connection. It goes after the calls the session's log held when it
    /// ended, numbered after the last of them, and the change it committed
    /// joins the session's changes. Returns whether a session ended here to
    /// keep it on; nothing is started for one that was never kept.
    pub fn record_after_end(
        &self,
        session: &str,
        draft: &crate::agent_activity::ToolCallDraft,
        at: &Timestamp,
    ) -> bool {
        let session = session.trim().to_ascii_lowercase();
        let mut kept = false;
        self.update(
            session_actor_id(&session),
            || AgentSessionRecord::new(&session, AgentSessionSource::Kin),
            |record| {
                if record.source != AgentSessionSource::Kin || record.ended_at.is_none() {
                    return false;
                }
                record.calls_total += 1;
                record.calls.push(AgentCall {
                    seq: record.calls_total,
                    at: at.clone(),
                    tool: draft.tool.clone(),
                    entity_ids: draft.entity_ids.clone(),
                    result_count: u64::try_from(draft.result_count).unwrap_or(u64::MAX),
                    bytes: u64::try_from(draft.bytes).unwrap_or(u64::MAX),
                    duration_ms: draft.duration_ms,
                    is_error: draft.is_error,
                    outcome: AgentCallOutcome::of(draft.change_id.clone(), draft.caller_left),
                });
                if record
                    .last_activity_at
                    .as_ref()
                    .is_none_or(|last| last < at)
                {
                    record.last_activity_at = Some(at.clone());
                }
                if let Some(change) = &draft.change_id {
                    keep_change(record, change);
                }
                kept = true;
                true
            },
        );
        kept
    }

    /// Keep a change a session committed after it ended on what was kept of
    /// the session. Nothing is started for a session that was never kept.
    pub fn record_change_after_end(&self, session: &str, change_id: &str) {
        let session = session.trim().to_ascii_lowercase();
        self.update(
            session_actor_id(&session),
            || AgentSessionRecord::new(&session, AgentSessionSource::Kin),
            |record| {
                if record.source != AgentSessionSource::Kin || record.ended_at.is_none() {
                    return false;
                }
                keep_change(record, change_id)
            },
        );
    }

    /// Every session kept that ended inside the window, newest end first,
    /// the newest [`crate::ended_sessions::MAX_ENDED_SESSIONS`].
    pub fn ended_sessions(&self, now: &Timestamp) -> Vec<EndedSession> {
        let mut ended: Vec<EndedSession> = self
            .records()
            .into_iter()
            .filter(|(_, record)| record.source == AgentSessionSource::Kin)
            .filter_map(|(_, record)| {
                let ended_at = record.ended_at.clone()?;
                let last = record
                    .last_activity_at
                    .clone()
                    .unwrap_or_else(|| ended_at.clone());
                Some(EndedSession {
                    session_id: record.session.clone(),
                    vendor: record.vendor.clone(),
                    client: record.client.clone(),
                    transport: record.transport.clone(),
                    started_at: record.started_at.clone().unwrap_or_else(|| last.clone()),
                    last_activity_at: last,
                    ended_at,
                    calls_total: record.calls_total,
                    calls: record.calls.iter().map(call_entry).collect(),
                    change_ids: record.change_ids.clone(),
                })
            })
            .filter(|session| !crate::ended_sessions::expired(&session.ended_at, now))
            .collect();
        ended.sort_by(|left, right| {
            right
                .ended_at
                .cmp(&left.ended_at)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        ended.truncate(crate::ended_sessions::MAX_ENDED_SESSIONS);
        ended
    }

    /// One session kept that ended inside the window.
    pub fn ended_session(&self, session_id: &str, now: &Timestamp) -> Option<EndedSession> {
        self.ended_sessions(now)
            .into_iter()
            .find(|session| session.session_id.eq_ignore_ascii_case(session_id.trim()))
    }

    /// Whether anything is held that authority does not hold yet.
    pub fn holds_unwritten(&self) -> bool {
        let held = lock(&self.held);
        !held.memory_only && !held.records.is_empty()
    }
}

/// Put `change_id` first among a session's changes, which are newest first,
/// unless it is already there. Returns whether the record changed.
fn keep_change(record: &mut AgentSessionRecord, change_id: &str) -> bool {
    let change_id = change_id.to_ascii_lowercase();
    if record.change_ids.contains(&change_id) {
        return false;
    }
    record.change_ids.insert(0, change_id);
    true
}

fn edit_record(edit: &ClientEdit) -> AgentEditRecord {
    let mut entities = edit.entities.clone();
    entities.truncate(MAX_ENTITIES_PER_WRITE);
    AgentEditRecord {
        base: edit.base.clone(),
        change_id: edit.change_id.clone(),
        kin_session: edit.kin_session.clone(),
        at: parse_time(&edit.at).unwrap_or_else(Timestamp::now),
        entities,
    }
}

/// The edits `records` hold, oldest first, the newest [`MAX_CLIENT_EDITS`].
fn edits_of(records: &[(ActorId, AgentSessionRecord)]) -> Vec<ClientEdit> {
    let mut edits: Vec<(Timestamp, ClientEdit)> = Vec::new();
    for (_, record) in records
        .iter()
        .filter(|(_, record)| record.source == AgentSessionSource::Client)
    {
        for edit in &record.edits {
            edits.push((
                edit.at.clone(),
                ClientEdit {
                    base: edit.base.clone(),
                    change_id: edit.change_id.clone(),
                    client: record.client.clone(),
                    session: record.session.clone(),
                    kin_session: edit.kin_session.clone(),
                    at: edit.at.to_string(),
                    entities: edit.entities.clone(),
                },
            ));
        }
    }
    // Stable, so edits made at one moment keep the order they were kept in.
    edits.sort_by(|left, right| left.0.cmp(&right.0));
    let skip = edits.len().saturating_sub(MAX_CLIENT_EDITS);
    edits.into_iter().skip(skip).map(|(_, edit)| edit).collect()
}

/// The records past a bound, rewritten inside it: the oldest writes and
/// edits past what is kept, and the calls of every session that ended outside
/// the window ended sessions are kept for.
fn bounded(
    records: &[(ActorId, AgentSessionRecord)],
    now: &Timestamp,
) -> HashMap<ActorId, AgentSessionRecord> {
    fn take<'a>(
        rewritten: &'a mut HashMap<ActorId, AgentSessionRecord>,
        records: &[(ActorId, AgentSessionRecord)],
        actor_id: ActorId,
    ) -> &'a mut AgentSessionRecord {
        rewritten.entry(actor_id).or_insert_with(|| {
            records
                .iter()
                .find(|(id, _)| *id == actor_id)
                .map(|(_, record)| record.clone())
                .expect("a rewritten record is one of the records")
        })
    }
    let mut rewritten: HashMap<ActorId, AgentSessionRecord> = HashMap::new();

    let mut ended: Vec<(&Timestamp, ActorId)> = records
        .iter()
        .filter_map(|(id, record)| record.ended_at.as_ref().map(|at| (at, *id)))
        .collect();
    ended.sort_by(|left, right| right.0.cmp(left.0));
    for (rank, (at, actor_id)) in ended.iter().enumerate() {
        let kept = rank < crate::ended_sessions::MAX_ENDED_SESSIONS
            && !crate::ended_sessions::expired(at, now);
        let holds_calls = records
            .iter()
            .any(|(id, record)| id == actor_id && !record.calls.is_empty());
        if !kept && holds_calls {
            take(&mut rewritten, records, *actor_id).calls.clear();
        }
    }

    let mut writes: Vec<(Timestamp, String, ActorId)> = records
        .iter()
        .flat_map(|(id, record)| {
            record
                .writes
                .iter()
                .map(move |write| (write.at.clone(), write.change_id.clone(), *id))
        })
        .collect();
    writes.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
    let past = writes.len().saturating_sub(MAX_AUTHORED_WRITES);
    for (_, change_id, actor_id) in writes.into_iter().take(past) {
        take(&mut rewritten, records, actor_id)
            .writes
            .retain(|write| write.change_id != change_id);
    }

    // In record order, so of edits made at one moment the first kept goes
    // first, as `edits_of` reads them.
    let mut edits: Vec<(Timestamp, ActorId, usize)> = records
        .iter()
        .flat_map(|(id, record)| {
            record
                .edits
                .iter()
                .enumerate()
                .map(move |(index, edit)| (edit.at.clone(), *id, index))
        })
        .collect();
    edits.sort_by(|left, right| left.0.cmp(&right.0));
    let past = edits.len().saturating_sub(MAX_CLIENT_EDITS);
    let mut dropped: HashMap<ActorId, Vec<usize>> = HashMap::new();
    for (_, actor_id, index) in edits.into_iter().take(past) {
        dropped.entry(actor_id).or_default().push(index);
    }
    for (actor_id, indexes) in dropped {
        let record = take(&mut rewritten, records, actor_id);
        let mut index = 0;
        record.edits.retain(|_| {
            let keep = !indexes.contains(&index);
            index += 1;
            keep
        });
    }
    rewritten
}

/// Whether what is held for `record` may be written now: its session has
/// ended, or it has been quiet for [`QUIET_BEFORE_WRITE`].
fn is_quiet(state: &DaemonState, record: &HeldRecord) -> bool {
    if record.touched.elapsed() < QUIET_BEFORE_WRITE {
        return record.record.ended_at.is_some();
    }
    let Ok(session) = uuid::Uuid::parse_str(record.record.session.trim()) else {
        return true;
    };
    let session = kin_model::SessionId(session);
    let coordinator = &state.coordinator;
    if record.record.source != AgentSessionSource::Kin
        || coordinator.get_session(&session).ok().flatten().is_none()
    {
        return true;
    }
    if coordinator.active_call_count(&session) > 0 {
        return false;
    }
    coordinator
        .activity()
        .last_call_at(&session)
        .is_none_or(|at| {
            Timestamp::now()
                .0
                .signed_duration_since(at.0)
                .to_std()
                .is_ok_and(|idle| idle >= QUIET_BEFORE_WRITE)
        })
}

/// Commit to repository authority, as one transaction, every held record
/// whose session has ended or gone quiet, or every held record with `all`,
/// together with the records the bounds rewrite. Returns how many records
/// it wrote.
pub(crate) async fn settle(state: &Arc<DaemonState>, all: bool) -> usize {
    let ledger = &state.agent_authorship;
    if !ledger.holds_unwritten() {
        return 0;
    }
    let ready_now = {
        let held = lock(&ledger.held);
        held.records
            .values()
            .any(|record| all || is_quiet(state, record))
    };
    if !ready_now {
        return 0;
    }
    let _coordinated = state.coordination_gate.lock().await;
    if state.prepared_publication.ensure_serving().is_err() {
        // Not serving writes now. What is held stays held for the next look.
        return 0;
    }
    let _mutation = state.begin_graph_authority_mutation();
    let (batch, records, held_ids) = {
        let held = lock(&ledger.held);
        let batch: Vec<(ActorId, u64)> = held
            .records
            .iter()
            .filter(|(_, record)| all || is_quiet(state, record))
            .map(|(actor_id, record)| (*actor_id, record.version))
            .collect();
        let held_ids: Vec<ActorId> = held.records.keys().copied().collect();
        let records: Vec<(ActorId, AgentSessionRecord)> =
            records_in(&ledger.graph, &held).into_values().collect();
        (batch, records, held_ids)
    };
    if batch.is_empty() {
        return 0;
    }
    let in_batch = |actor_id: &ActorId| batch.iter().any(|(id, _)| id == actor_id);
    let mut writing: BTreeMap<String, Actor> = BTreeMap::new();
    for (actor_id, record) in records.iter().filter(|(id, _)| in_batch(id)) {
        writing.insert(actor_id.0.to_string(), actor_for(*actor_id, record.clone()));
    }
    // A record still held and not written now is not rewritten either: it is
    // written whole, inside its bounds, once it is ready.
    for (actor_id, record) in bounded(&records, &Timestamp::now()) {
        if in_batch(&actor_id) || !held_ids.contains(&actor_id) {
            writing.insert(actor_id.0.to_string(), actor_for(actor_id, record));
        }
    }
    let actors: Vec<Actor> = writing.into_values().collect();
    let written = kin_model::in_canonical_key_order(
        actors
            .iter()
            .map(|actor| Keyed::new(actor.actor_id, actor.clone()))
            .collect(),
    )
    .map_err(|error| error.to_string())
    .and_then(|keyed| {
        let delta = CollaborationDelta {
            actors: keyed,
            ..CollaborationDelta::default()
        };
        delta.validate().map_err(|error| error.to_string())?;
        crate::review_write::commit_collaboration_records(
            state,
            delta,
            RECORD_ACTION,
            RECORD_ACTOR,
            |graph| {
                for actor in &actors {
                    graph
                        .create_actor(actor)
                        .map_err(|error| error.to_string())?;
                }
                Ok(())
            },
        )
        .map_err(|refusal| match refusal {
            crate::review_write::ReviewWriteRefusal::Hosted => {
                lock(&ledger.held).memory_only = true;
                refusal.to_string()
            }
            other => other.to_string(),
        })
    });
    match written {
        Ok(generation) => {
            tracing::info!(
                records = actors.len(),
                generation,
                "kept what agents did in repository authority"
            );
            let mut held = lock(&ledger.held);
            for (actor_id, version) in &batch {
                if held
                    .records
                    .get(actor_id)
                    .is_some_and(|record| record.version == *version)
                {
                    held.records.remove(actor_id);
                }
            }
            actors.len()
        }
        Err(error) => {
            // Held again for the next look: a record of what an agent did is
            // a record of something that happened, and one failed write does
            // not unmake it.
            debug!(%error, "agent records could not be written to repository authority");
            0
        }
    }
}

/// Whether `entity` is the declaration a create named `name`. A member may be
/// recorded under its container's name, as `Signer.sign`.
fn declares(entity: &Entity, name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && (entity.name == name
            || entity
                .name
                .strip_suffix(name)
                .is_some_and(|head| head.ends_with('.') || head.ends_with("::")))
}

/// The entities `operations` wrote in `change`: for each operation, the entity
/// it names or the one it created, kept only where the change holds a delta
/// for it. An edit the change carried in, an entity whose span only moved
/// because a line above it changed, and a source unit's own container are
/// never among them, because no operation named them.
///
/// A create names its entity by the name it declares. It is the change's one
/// added entity of that name, or, where the change added more than one, the
/// one beside the entity it was anchored to; one it cannot tell apart is left
/// out rather than guessed. `origin_of` says where an entity the change does
/// not hold lives, for an anchor the change left as it was.
pub(crate) fn written_entities(
    operations: &[kin_mcp::McpMutationOperation],
    change: &SemanticChange,
    origin_of: impl Fn(&EntityId) -> Option<FilePathId>,
) -> Vec<EntityId> {
    use kin_mcp::McpMutationPayload as Payload;

    let mut held: HashMap<EntityId, &Entity> = HashMap::new();
    let mut added: Vec<&Entity> = Vec::new();
    for delta in &change.entity_deltas {
        match delta {
            EntityDelta::Added { new } => {
                held.insert(new.id, new);
                added.push(new);
            }
            EntityDelta::Modified { new, .. } => {
                held.insert(new.id, new);
            }
            EntityDelta::Removed { old } => {
                held.insert(old.id, old);
            }
        }
    }
    let origin = |id: &EntityId| {
        held.get(id)
            .and_then(|entity| entity.file_origin.clone())
            .or_else(|| origin_of(id))
    };
    let created = |name: &str, anchor: Option<&EntityId>| -> Option<EntityId> {
        let named: Vec<&&Entity> = added
            .iter()
            .filter(|entity| declares(entity, name))
            .collect();
        if let [only] = named.as_slice() {
            return Some(only.id);
        }
        let place = origin(anchor?)?;
        let beside: Vec<&&&Entity> = named
            .iter()
            .filter(|entity| entity.file_origin.as_ref() == Some(&place))
            .collect();
        match beside.as_slice() {
            [only] => Some(only.id),
            _ => None,
        }
    };

    let mut written: Vec<EntityId> = Vec::new();
    for operation in operations {
        if written.len() >= MAX_ENTITIES_PER_WRITE {
            break;
        }
        let mut push = |id: Option<EntityId>| {
            if let Some(id) = id {
                if held.contains_key(&id) && !written.contains(&id) {
                    written.push(id);
                }
            }
        };
        let verb = operation.verb.trim().to_ascii_lowercase();
        match operation.payload.as_ref() {
            Some(Payload::EntitySourceBase(base)) => push(Some(base.entity_id)),
            Some(Payload::EntitySourcePatch(patch)) => push(Some(patch.source_base.entity_id)),
            Some(Payload::EntityRemove(remove)) => push(Some(remove.source_base.entity_id)),
            Some(Payload::EntityContent(content)) if verb == "create" => push(
                content
                    .name
                    .as_deref()
                    .and_then(|name| created(name, Some(&content.source_base.entity_id))),
            ),
            Some(Payload::EntityContent(content)) => push(Some(content.source_base.entity_id)),
            Some(Payload::EntityCreate(create)) => push(created(
                &create.name,
                create.source_base.as_ref().map(|base| &base.entity_id),
            )),
            Some(Payload::Entity(entity)) => push(Some(entity.id)),
            // An import list and a relation write no declaration of their own,
            // and bytes name no entity.
            Some(Payload::UnitImports(_) | Payload::Relation { .. } | Payload::Blob(_)) => {}
            None => push(
                uuid::Uuid::parse_str(operation.target.trim())
                    .ok()
                    .map(EntityId),
            ),
        }
    }
    written
}

/// Keep what one committed agent transaction wrote. `vendor` and `client` are
/// the names its session gave, already made safe to record.
pub(crate) fn record_commit(
    authorship: &AgentAuthorship,
    session_id: &str,
    vendor: &str,
    client: &str,
    operations: &[kin_mcp::McpMutationOperation],
    change: &SemanticChange,
    graph: &kin_db::InMemoryGraph,
) {
    use kin_model::EntityStore as _;

    let Ok(session) = uuid::Uuid::parse_str(session_id.trim()) else {
        return;
    };
    let entities = written_entities(operations, change, |id| {
        graph
            .get_entity(id)
            .ok()
            .flatten()
            .and_then(|entity| entity.file_origin)
    });
    authorship.record(AgentWrite {
        change_id: change.id.to_string(),
        session: session.to_string(),
        vendor: crate::agent_activity::display_name(vendor, ""),
        client: crate::agent_activity::display_name(client, ""),
        at: change.timestamp.to_string(),
        entities,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_mcp::source_base::{
        EntitySourceBase, EntitySourcePatch, SourceBaseContext, SourceBaseSchema,
    };

    fn entity(name: &str, file: &str) -> Entity {
        let place = FilePathId::new(file);
        Entity {
            id: EntityId::new(),
            kind: kin_model::EntityKind::Function,
            name: name.to_string(),
            language: kin_model::LanguageId::Python,
            fingerprint: kin_model::SemanticFingerprint {
                algorithm: kin_model::FingerprintAlgorithm::V1TreeSitter,
                ast_hash: kin_model::Hash256::from_bytes([1; 32]),
                signature_hash: kin_model::Hash256::from_bytes([2; 32]),
                behavior_hash: kin_model::Hash256::from_bytes([3; 32]),
                equivalence_hash: kin_model::Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(place.clone()),
            span: Some(kin_model::SourceSpan {
                file: place,
                start_byte: 0,
                end_byte: 1,
                start_line: 1,
                start_col: 0,
                end_line: 2,
                end_col: 0,
            }),
            signature: format!("def {name}()"),
            visibility: kin_model::Visibility::Public,
            role: kin_model::EntityRole::Source,
            doc_summary: None,
            metadata: kin_model::EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    fn base_of(entity: &Entity) -> EntitySourceBase {
        EntitySourceBase {
            schema: SourceBaseSchema::V1,
            context: SourceBaseContext {
                repository_id: "authorship-test".into(),
                workspace_id: uuid::Uuid::new_v4().to_string(),
                workspace_generation: 1,
                workspace_head_hash: "a".repeat(64),
                workspace_tree_hash: "b".repeat(64),
            },
            entity_id: entity.id,
            artifact_id: kin_model::ArtifactId::new(),
            source_blob_hash: "c".repeat(64),
            start_byte: 0,
            end_byte: 1,
            body_hash: "d".repeat(64),
        }
    }

    fn operation(
        verb: &str,
        payload: kin_mcp::McpMutationPayload,
    ) -> kin_mcp::McpMutationOperation {
        kin_mcp::McpMutationOperation {
            verb: verb.to_string(),
            target: String::new(),
            payload: Some(payload),
            body: None,
            destination: None,
            description: "edit".to_string(),
        }
    }

    fn patch(target: &Entity) -> kin_mcp::McpMutationOperation {
        operation(
            "patch",
            kin_mcp::McpMutationPayload::EntitySourcePatch(EntitySourcePatch {
                source_base: base_of(target),
                edits: Vec::new(),
            }),
        )
    }

    fn create_beside(anchor: &Entity, name: &str) -> kin_mcp::McpMutationOperation {
        let create: kin_mcp::entity_lifecycle::EntityCreate =
            serde_json::from_value(serde_json::json!({
                "source_base": base_of(anchor),
                "placement": "sibling_after",
                "name": name,
                "kind": "function",
                "body": format!("def {name}():\n    pass"),
            }))
            .expect("an anchored create");
        operation("create", kin_mcp::McpMutationPayload::EntityCreate(create))
    }

    fn change_of(entity_deltas: Vec<EntityDelta>) -> SemanticChange {
        SemanticChange {
            id: kin_model::SemanticChangeId::from_hash(kin_model::Hash256::from_bytes([9; 32])),
            parents: Vec::new(),
            origin: kin_model::ChangeOrigin::Native,
            timestamp: kin_model::Timestamp::now(),
            author: kin_model::AuthorId::new("codex/codex <mcp-agent:s>"),
            message: "MCP transaction".to_string(),
            entity_deltas,
            relation_deltas: Vec::new(),
            tree_deltas: Vec::new(),
            projected_files: Vec::new(),
            spec_link: None,
            evidence: Vec::new(),
            risk_summary: None,
            admission_policy_delta: None,
            external_reference_deltas: Vec::new(),
            resolution_record_deltas: Vec::new(),
        }
    }

    fn modified(entity: &Entity) -> EntityDelta {
        EntityDelta::Modified {
            old: entity.clone(),
            new: entity.clone(),
        }
    }

    /// The agent patched one function. The same change also holds the user's
    /// edit to its neighbour in the same source unit, and a third function
    /// whose span only moved. Only the patched one is the agent's.
    #[test]
    fn only_the_entity_an_operation_named_is_written() {
        let written = entity("double", "util.py");
        let neighbour = entity("clamp", "util.py");
        let moved = entity("area", "util.py");
        let change = change_of(vec![
            modified(&neighbour),
            modified(&written),
            modified(&moved),
        ]);
        assert_eq!(
            written_entities(&[patch(&written)], &change, |_| None),
            vec![written.id]
        );
    }

    /// An operation whose entity the change holds no delta for wrote nothing
    /// there.
    #[test]
    fn an_operation_is_kept_only_where_the_change_holds_its_entity() {
        let named = entity("double", "util.py");
        let other = entity("clamp", "util.py");
        let change = change_of(vec![modified(&other)]);
        assert!(written_entities(&[patch(&named)], &change, |_| None).is_empty());
    }

    /// A create is the added entity of the name it declared, told apart by
    /// the unit it was anchored in when another unit added the same name, and
    /// left out when nothing tells them apart.
    #[test]
    fn a_create_is_the_entity_it_declared_beside_its_anchor() {
        let anchor = entity("area", "shapes.py");
        let made = entity("scale", "shapes.py");
        let elsewhere = entity("scale", "other.py");
        let alone = change_of(vec![EntityDelta::Added { new: made.clone() }]);
        assert_eq!(
            written_entities(&[create_beside(&anchor, "scale")], &alone, |_| None),
            vec![made.id]
        );
        let twice = change_of(vec![
            EntityDelta::Added {
                new: elsewhere.clone(),
            },
            EntityDelta::Added { new: made.clone() },
        ]);
        let origin = anchor.file_origin.clone();
        assert_eq!(
            written_entities(&[create_beside(&anchor, "scale")], &twice, |id| {
                (*id == anchor.id).then(|| origin.clone()).flatten()
            }),
            vec![made.id],
            "beside the anchor, which the change left as it was"
        );
        assert!(
            written_entities(&[create_beside(&anchor, "scale")], &twice, |_| None).is_empty(),
            "two of the name and no anchor to place them: no guess"
        );
    }

    fn write(change: u8, entities: Vec<EntityId>) -> AgentWrite {
        AgentWrite {
            change_id: format!("{change:02x}").repeat(32),
            session: uuid::Uuid::new_v4().to_string(),
            vendor: "codex".to_string(),
            client: "codex".to_string(),
            at: kin_model::Timestamp::now().to_string(),
            entities,
        }
    }

    /// A daemon over a real store, as one that opened it.
    fn daemon(dir: &std::path::Path) -> Arc<DaemonState> {
        kin_core::test_env::install_test_registry();
        let layout = kin_core::init(dir).unwrap().layout;
        reopen(layout)
    }

    fn reopen(layout: kin_core::KinLayout) -> Arc<DaemonState> {
        let state = Arc::new(DaemonState::open(layout).unwrap());
        state
            .is_initialized
            .store(true, std::sync::atomic::Ordering::Relaxed);
        state
    }

    fn edit(session: &str, entities: Vec<EntityId>) -> ClientEdit {
        ClientEdit {
            base: None,
            change_id: None,
            client: "claude-code".to_string(),
            session: session.to_string(),
            kin_session: None,
            at: Timestamp::now().to_string(),
            entities,
        }
    }

    fn ended(session: &str, calls: u64, minutes_ago: i64) -> EndedSession {
        let at = |minutes: i64| Timestamp(chrono::Utc::now() - chrono::Duration::minutes(minutes));
        EndedSession {
            session_id: session.to_string(),
            vendor: "claude-code".to_string(),
            client: "claude-code".to_string(),
            transport: "mcp".to_string(),
            started_at: at(minutes_ago + 10),
            last_activity_at: at(minutes_ago + 1),
            ended_at: at(minutes_ago),
            calls_total: calls,
            calls: (1..=calls)
                .map(|seq| ToolCallEntry {
                    seq,
                    at: at(minutes_ago + 2),
                    tool: "semantic_locate".to_string(),
                    entity_ids: vec![EntityId::new()],
                    result_count: 1,
                    bytes: 10,
                    duration_ms: 3,
                    is_error: false,
                    repo_id: None,
                    change_id: None,
                    caller_left: false,
                })
                .collect(),
            change_ids: vec!["a".repeat(64)],
        }
    }

    /// What agents did is shown the moment it is known, is repository
    /// authority once written, and a daemon that reopens the store reads it
    /// back from there: writes, edits made with an agent's own tools, and the
    /// sessions that ended with their calls. Nothing is kept beside the store
    /// as a record of its own.
    #[tokio::test]
    async fn what_agents_did_reopens_from_repository_authority() {
        let dir = tempfile::tempdir().unwrap();
        let state = daemon(dir.path());
        let first = write(1, vec![EntityId::new()]);
        state.agent_authorship.record(first.clone());
        state.agent_authorship.record(first.clone());
        state.agent_authorship.record(write(2, Vec::new()));
        let edited = edit("client-session-1", vec![EntityId::new()]);
        state.agent_authorship.record_edit(edited.clone());
        let session = uuid::Uuid::new_v4().to_string();
        state.agent_authorship.hold_ended(ended(&session, 4, 3));
        assert_eq!(state.agent_authorship.writes(), vec![first.clone()]);
        assert_eq!(state.agent_authorship.edits(), vec![edited.clone()]);
        let now = Timestamp::now();
        assert_eq!(state.agent_authorship.ended_sessions(&now).len(), 1);

        let generation = state
            .snapshot_generation
            .load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(settle(&state, true).await, 3);
        assert!(!state.agent_authorship.holds_unwritten());
        assert!(
            state
                .snapshot_generation
                .load(std::sync::atomic::Ordering::SeqCst)
                > generation,
            "the records were committed to repository authority"
        );
        assert!(
            !state.layout.root().join("agent_authorship.json").exists()
                && !state.layout.root().join("agent_sessions.json").exists(),
            "no record is kept beside the store"
        );
        let reopened = reopen(state.layout.clone());
        drop(state);
        assert_eq!(reopened.agent_authorship.writes(), vec![first]);
        assert_eq!(reopened.agent_authorship.edits(), vec![edited]);
        let kept = reopened.agent_authorship.ended_sessions(&Timestamp::now());
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].session_id, session);
        assert_eq!(kept[0].calls_total, 4);
        assert_eq!(kept[0].calls.len(), 4);
        assert_eq!(kept[0].change_ids, vec!["a".repeat(64)]);
    }

    /// The bounds hold in authority, which has no removal: past the writes
    /// kept the oldest go from the record that held them, and a session that
    /// ended outside the window keeps no calls.
    #[tokio::test]
    async fn the_records_stay_inside_their_bounds_in_authority() {
        let dir = tempfile::tempdir().unwrap();
        let state = daemon(dir.path());
        let first = write(1, vec![EntityId::new()]);
        state.agent_authorship.record(first.clone());
        for index in 0..MAX_AUTHORED_WRITES {
            state.agent_authorship.record(AgentWrite {
                change_id: format!("{index:064x}"),
                ..write(3, vec![EntityId::new()])
            });
        }
        let days = i64::try_from(crate::ended_sessions::ENDED_KEPT_DAYS).unwrap();
        let old = uuid::Uuid::new_v4().to_string();
        state
            .agent_authorship
            .hold_ended(ended(&old, 2, days * 24 * 60 + 5));
        settle(&state, true).await;
        let reopened = reopen(state.layout.clone());
        drop(state);
        let kept = reopened.agent_authorship.writes();
        assert_eq!(kept.len(), MAX_AUTHORED_WRITES);
        assert!(
            !kept.contains(&first),
            "the oldest write went past the bound"
        );
        let held: Vec<Actor> = reopened
            .graph
            .list_actors()
            .unwrap()
            .into_iter()
            .filter(|actor| actor.agent_session.is_some())
            .collect();
        let writes: usize = held
            .iter()
            .map(|actor| actor.agent_session.as_ref().unwrap().writes.len())
            .sum();
        assert_eq!(
            writes, MAX_AUTHORED_WRITES,
            "authority holds no more than is kept"
        );
        let outside = held
            .iter()
            .find(|actor| actor.actor_id == session_actor_id(&old))
            .expect("the session outside the window is still an actor");
        assert!(
            outside.agent_session.as_ref().unwrap().calls.is_empty(),
            "outside the window a session keeps no calls"
        );
        assert!(reopened
            .agent_authorship
            .ended_sessions(&Timestamp::now())
            .is_empty());
    }

    /// A record is not written while its session is at work, and is once the
    /// session has gone quiet.
    #[tokio::test]
    async fn a_record_waits_for_its_session_to_go_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let state = daemon(dir.path());
        state
            .agent_authorship
            .record(write(1, vec![EntityId::new()]));
        assert_eq!(settle(&state, false).await, 0, "written just now: held");
        assert!(state.agent_authorship.holds_unwritten());
        tokio::time::sleep(QUIET_BEFORE_WRITE + Duration::from_millis(50)).await;
        assert_eq!(settle(&state, false).await, 1);
        assert!(!state.agent_authorship.holds_unwritten());
    }
}
