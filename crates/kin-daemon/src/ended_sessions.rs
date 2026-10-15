// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The agent sessions that ended, and the window they are kept for.
//!
//! A live session's call log lives in memory and goes with the session (see
//! [`crate::agent_activity`]). Someone who looks at a repository after its
//! agent finished still needs to see who worked there, what it read and what
//! it changed, so when a session ends or is reaped the daemon keeps what its
//! log held: the tools it called, the entities each call was about, and the
//! changes it committed. Never the question, the arguments or the answer, and
//! never a directory, a process id or a path, the same as the live log.
//!
//! What is kept is repository authority, on the actor that stands for the
//! session ([`kin_model::AgentSessionRecord`]), written by
//! [`crate::agent_authorship`]. It is bounded twice: a session's calls are
//! kept for [`ENDED_KEPT_DAYS`] days after it ends, and only for the newest
//! [`MAX_ENDED_SESSIONS`]. Each keeps its newest [`MAX_ENDED_CALLS`] calls.

use kin_model::timestamp::Timestamp;

use crate::agent_activity::ToolCallEntry;

/// How long a session is kept after it ends.
pub const ENDED_KEPT_DAYS: u64 = kin_model::AGENT_SESSION_KEPT_DAYS;
/// The ended sessions kept, the newest.
pub const MAX_ENDED_SESSIONS: usize = kin_model::AGENT_SESSIONS_KEPT;
/// The calls kept for one ended session, its newest.
pub const MAX_ENDED_CALLS: usize = kin_model::AGENT_SESSION_CALLS_MAX;
/// The changes kept for one ended session, its newest.
pub const MAX_ENDED_CHANGES: usize = kin_model::AGENT_SESSION_CHANGES_MAX;

/// One session that ended, as its log stood when it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndedSession {
    pub session_id: String,
    /// The names its connection gave, already made safe to show.
    pub vendor: String,
    pub client: String,
    pub transport: String,
    pub started_at: Timestamp,
    /// When it last called Kin, or was last heard from when it never did.
    pub last_activity_at: Timestamp,
    pub ended_at: Timestamp,
    /// Every call it made, counted, including the ones not kept.
    pub calls_total: u64,
    /// Its newest calls, oldest first.
    pub calls: Vec<ToolCallEntry>,
    /// The changes it committed, newest first.
    pub change_ids: Vec<String>,
}

/// Whether `at` is older than the window a session is kept for, seen from
/// `now`.
pub fn expired(at: &Timestamp, now: &Timestamp) -> bool {
    let window = chrono::Duration::days(i64::try_from(ENDED_KEPT_DAYS).unwrap_or(30));
    now.0.signed_duration_since(at.0) > window
}

/// Whether a write made at `at`, as its record spells the time, falls inside
/// the window a session is kept for, seen from `now`. A time that does not
/// read is outside it.
pub fn within_window(at: &str, now: &Timestamp) -> bool {
    chrono::DateTime::parse_from_rfc3339(at.trim())
        .map(|at| !expired(&Timestamp(at.with_timezone(&chrono::Utc)), now))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_is_inside_the_window_only_when_it_reads_and_is_recent() {
        let now = Timestamp::now();
        let recent = Timestamp(chrono::Utc::now() - chrono::Duration::minutes(10));
        assert!(within_window(&recent.to_string(), &now));
        assert!(!within_window("not a time", &now));
        let days = i64::try_from(ENDED_KEPT_DAYS).unwrap();
        let old = Timestamp(chrono::Utc::now() - chrono::Duration::days(days + 1));
        assert!(!within_window(&old.to_string(), &now));
        assert!(expired(&old, &now));
        assert!(!expired(&recent, &now));
    }
}
