// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! What an agent session did in a repository, as repository authority keeps
//! it past the session's end.
//!
//! An agent that connected through Kin is an [`Actor`](crate::Actor), and the
//! actor that stands for its session carries this record: who the session was,
//! when it worked, the tools it called and the entities each call was about,
//! the changes it committed, and the entities each of those commits wrote. An
//! agent that edited with its own tools, and whose client reported the edits,
//! is an actor of the same kind, holding the edits Kin confirmed. Never the
//! question, the arguments or the answer of a call, and never a directory, a
//! process id or a path.
//!
//! It is collaboration authority, so it travels in a repository transaction
//! like a review or a flag, and a daemon that reopens the store reads it back
//! from there. Every field is persisted positionally, so a field is only ever
//! appended and none is skipped.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::EntityId;
use crate::timestamp::Timestamp;

/// How long a session's calls are kept after it ends.
pub const AGENT_SESSION_KEPT_DAYS: u64 = 30;
/// The ended sessions whose calls are kept: the newest that ended inside
/// [`AGENT_SESSION_KEPT_DAYS`].
pub const AGENT_SESSIONS_KEPT: usize = 32;
/// The calls one session keeps, its newest.
pub const AGENT_SESSION_CALLS_MAX: usize = 100;
/// The changes one session keeps, its newest.
pub const AGENT_SESSION_CHANGES_MAX: usize = 16;
/// The writes through Kin kept across every session, the newest.
pub const AGENT_WRITES_MAX: usize = 512;
/// The edits made with an agent's own tools kept across every session, the
/// newest.
pub const AGENT_EDITS_MAX: usize = 2048;
/// The entities one write or one edit keeps.
pub const AGENT_ENTITIES_PER_WRITE_MAX: usize = 128;

/// How a session reached Kin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum AgentSessionSource {
    /// A session connected through Kin: an MCP server, a wrapper or the CLI.
    Kin,
    /// A session its client named when it reported an edit made with the
    /// client's own tools.
    Client,
}

/// One agent session's record, carried by the actor that stands for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentSessionRecord {
    /// The session: a Kin session id, or the session a client named.
    pub session: String,
    pub source: AgentSessionSource,
    /// The names its connection or its client gave, made safe to show.
    pub vendor: String,
    pub client: String,
    /// How it connected to Kin. Empty for a client's own session.
    pub transport: String,
    pub started_at: Option<Timestamp>,
    /// When it last called Kin, or was last heard from when it never did.
    pub last_activity_at: Option<Timestamp>,
    /// When it ended. Absent while Kin has not seen it end.
    pub ended_at: Option<Timestamp>,
    /// Every call it made, counted, including the ones not kept.
    pub calls_total: u64,
    /// Its newest calls, oldest first.
    pub calls: Vec<AgentCall>,
    /// The changes it committed, newest first.
    pub change_ids: Vec<String>,
    /// What each change it committed through Kin wrote, oldest first.
    pub writes: Vec<AgentWriteRecord>,
    /// The edits it made with its own tools that Kin confirmed, oldest first.
    pub edits: Vec<AgentEditRecord>,
}

impl AgentSessionRecord {
    /// A record that holds nothing yet.
    pub fn new(session: &str, source: AgentSessionSource) -> Self {
        Self {
            session: session.to_string(),
            source,
            vendor: String::new(),
            client: String::new(),
            transport: String::new(),
            started_at: None,
            last_activity_at: None,
            ended_at: None,
            calls_total: 0,
            calls: Vec::new(),
            change_ids: Vec::new(),
            writes: Vec::new(),
            edits: Vec::new(),
        }
    }

    /// Hold every list inside its bound: the newest calls, changes, and the
    /// entities each write and edit names.
    pub fn bound(&mut self) {
        if self.calls.len() > AGENT_SESSION_CALLS_MAX {
            let extra = self.calls.len() - AGENT_SESSION_CALLS_MAX;
            self.calls.drain(..extra);
        }
        self.change_ids.truncate(AGENT_SESSION_CHANGES_MAX);
        for write in &mut self.writes {
            write.entities.truncate(AGENT_ENTITIES_PER_WRITE_MAX);
        }
        for edit in &mut self.edits {
            edit.entities.truncate(AGENT_ENTITIES_PER_WRITE_MAX);
        }
        if self.writes.len() > AGENT_WRITES_MAX {
            let extra = self.writes.len() - AGENT_WRITES_MAX;
            self.writes.drain(..extra);
        }
        if self.edits.len() > AGENT_EDITS_MAX {
            let extra = self.edits.len() - AGENT_EDITS_MAX;
            self.edits.drain(..extra);
        }
    }
}

/// One tool call a session made: the tool, the entities it was about, how
/// much it answered and how long it took.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentCall {
    /// 1-based position of the call in its session.
    pub seq: u64,
    pub at: Timestamp,
    /// The registered tool Kin ran.
    pub tool: String,
    /// Entities the call was about: its targets first, then what the answer
    /// named.
    pub entity_ids: Vec<EntityId>,
    /// Distinct entities the answer named.
    pub result_count: u64,
    /// Size of the answer.
    pub bytes: u64,
    pub duration_ms: u64,
    /// The answer was a refusal or an error.
    pub is_error: bool,
    /// What a write left behind, when there is something to say: the change
    /// it committed, and whether it finished after the agent stopped waiting
    /// for it. Absent on every other call, which keeps those at the bytes
    /// they always had; it is the last field, so an encoding without it reads
    /// back as a call with none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<AgentCallOutcome>,
}

/// What one write call of a session left behind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentCallOutcome {
    /// The change the call committed, when it committed one.
    pub change_id: Option<String>,
    /// The call finished after the agent that made it stopped waiting for its
    /// answer, so the agent was never told how it ended.
    pub caller_left: bool,
}

impl AgentCallOutcome {
    /// The outcome worth keeping for a call that committed `change_id`, if
    /// any, and whose caller did or did not wait for it: `None` when there is
    /// nothing to say.
    pub fn of(change_id: Option<String>, caller_left: bool) -> Option<Self> {
        (change_id.is_some() || caller_left).then_some(Self {
            change_id,
            caller_left,
        })
    }
}

/// What one change a session committed through Kin wrote: the entities its
/// own operations named or created, never one that only came along.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentWriteRecord {
    pub change_id: String,
    pub at: Timestamp,
    pub entities: Vec<EntityId>,
}

/// One edit a session made with its own tools, as Kin confirmed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentEditRecord {
    /// The change the working copy stood on when the edit was admitted.
    pub base: Option<String>,
    /// The change that recorded the edit, once one did.
    pub change_id: Option<String>,
    /// The Kin session the same agent was connected through, when one was.
    pub kin_session: Option<String>,
    pub at: Timestamp,
    pub entities: Vec<EntityId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_stays_inside_its_bounds_keeping_the_newest() {
        let mut record = AgentSessionRecord::new("s", AgentSessionSource::Kin);
        let call = |seq| AgentCall {
            seq,
            at: Timestamp::now(),
            tool: "semantic_locate".into(),
            entity_ids: vec![EntityId::new()],
            result_count: 1,
            bytes: 1,
            duration_ms: 1,
            is_error: false,
            outcome: None,
        };
        let many = u64::try_from(AGENT_SESSION_CALLS_MAX).unwrap() + 7;
        record.calls = (1..=many).map(call).collect();
        record.change_ids = (0..AGENT_SESSION_CHANGES_MAX + 3)
            .map(|index| format!("{index:064x}"))
            .collect();
        record.writes = vec![AgentWriteRecord {
            change_id: "a".repeat(64),
            at: Timestamp::now(),
            entities: (0..AGENT_ENTITIES_PER_WRITE_MAX + 5)
                .map(|_| EntityId::new())
                .collect(),
        }];
        record.bound();
        assert_eq!(record.calls.len(), AGENT_SESSION_CALLS_MAX);
        assert_eq!(record.calls.last().unwrap().seq, many);
        assert_eq!(record.change_ids.len(), AGENT_SESSION_CHANGES_MAX);
        assert_eq!(record.change_ids[0], format!("{:064x}", 0));
        assert_eq!(
            record.writes[0].entities.len(),
            AGENT_ENTITIES_PER_WRITE_MAX
        );
    }

    #[test]
    fn a_record_reads_back_from_the_positional_encoding_the_store_keeps() {
        let mut record = AgentSessionRecord::new("s", AgentSessionSource::Client);
        record.edits.push(AgentEditRecord {
            base: None,
            change_id: Some("b".repeat(64)),
            kin_session: None,
            at: Timestamp::now(),
            entities: vec![EntityId::new()],
        });
        let read: AgentSessionRecord =
            rmp_serde::from_slice(&rmp_serde::to_vec(&record).unwrap()).unwrap();
        assert_eq!(read, record);
    }

    /// The call as it was persisted before a call could carry an outcome.
    #[derive(Serialize)]
    struct CallBeforeOutcomes<'a> {
        seq: u64,
        at: &'a Timestamp,
        tool: &'a str,
        entity_ids: &'a [EntityId],
        result_count: u64,
        bytes: u64,
        duration_ms: u64,
        is_error: bool,
    }

    /// A call with nothing to say keeps the bytes it always had, an encoding
    /// written before outcomes reads back as a call with none, and a commit
    /// that finished after its agent stopped waiting reads back whole.
    #[test]
    fn a_call_outcome_is_appended_without_moving_any_stored_call() {
        let plain = AgentCall {
            seq: 3,
            at: Timestamp::now(),
            tool: "kin_transaction_commit".into(),
            entity_ids: vec![EntityId::new()],
            result_count: 1,
            bytes: 40,
            duration_ms: 460_000,
            is_error: false,
            outcome: None,
        };
        let before = CallBeforeOutcomes {
            seq: plain.seq,
            at: &plain.at,
            tool: &plain.tool,
            entity_ids: &plain.entity_ids,
            result_count: plain.result_count,
            bytes: plain.bytes,
            duration_ms: plain.duration_ms,
            is_error: plain.is_error,
        };
        let stored = rmp_serde::to_vec(&before).unwrap();
        assert_eq!(rmp_serde::to_vec(&plain).unwrap(), stored);
        let read: AgentCall = rmp_serde::from_slice(&stored).unwrap();
        assert_eq!(read, plain);

        assert_eq!(AgentCallOutcome::of(None, false), None);
        let mut late = plain.clone();
        late.outcome = AgentCallOutcome::of(Some("c".repeat(64)), true);
        let mut record = AgentSessionRecord::new("s", AgentSessionSource::Kin);
        record.calls = vec![plain, late];
        let read: AgentSessionRecord =
            rmp_serde::from_slice(&rmp_serde::to_vec(&record).unwrap()).unwrap();
        assert_eq!(read, record);
        let read: AgentSessionRecord =
            serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
        assert_eq!(read, record);
    }
}
