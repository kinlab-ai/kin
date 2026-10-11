// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The write session a connection holds for its client, opened on its first
//! write.
//!
//! A write runs in a session the daemon owns, and `mutate`, `transaction
//! begin` and a toolchain run each name theirs. An agent harness such as
//! `kin agent run` holds one and fills the id in. A client wired straight to
//! the server had to call `session start` first, and so the session tool sat
//! on every agent's list: in one measured run an agent spent a turn starting a
//! session for a question that only read.
//!
//! So a write that names no session runs in this connection's own, which the
//! server opens the first time a write needs one and reuses after that. What it
//! may write and commit is exactly what the store permits the caller:
//! authorization is the daemon's, as it was for a session the caller started,
//! and hiding the session tool from a listing changes what an agent sees, never
//! what it may do. A call that names a session keeps it.
//!
//! Its lifecycle is the connection's. The session names this server's process,
//! which the daemon takes as proof the session is alive for as long as the
//! process runs, and every call made in it refreshes its idle window as any
//! session's call does. It is ended when the client closes the connection, and
//! a server that dies first leaves it to the daemon's idle rule. A session the
//! daemon reaped anyway is opened again once, for a write that started
//! nothing, by [`ConnectionSession::forget`].
//!
//! It declares execution. The toolchain run is gated on a session that opted
//! in to it, and on a connection that serves `exec` the call to it is that
//! opt-in; the command policy that decides what may run is unchanged.

use std::collections::HashMap;
use std::future::Future;

use serde_json::{json, Value};

/// The registered tools that run in a session, and whether a call to one
/// names none of its own, so the connection's session is used.
pub fn needs_connection_session(tool: &str, arguments: &HashMap<String, Value>) -> bool {
    let names_one = arguments
        .get("session_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty());
    if names_one {
        return false;
    }
    match tool {
        // A keyed mutate binds its request to the session its caller will
        // retry under, across restarts; only the caller can hold that.
        "kin_mutate" => arguments.get("request_id").is_none_or(|key| key.is_null()),
        // An intent belongs to the session that registered it, and the
        // connection's own is the one its writes run in.
        "kin_transaction_begin" | "kin_register_intent" | "kin_release_intent" => true,
        _ => false,
    }
}

/// Whether a write's refusal says its session is gone and nothing started, so
/// opening a new session and sending the same call again is safe.
pub fn session_gone_before_start(tool: &str, session_id: &str, result: &Value) -> bool {
    let text = result
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match tool {
        "kin_mutate" => {
            text.starts_with("kin_mutate_not_started:") && text.contains("session_not_found")
        }
        "kin_transaction_begin" => text.starts_with(&format!("Session not found: {session_id}")),
        _ => false,
    }
}

/// Who the client is, as the session it is given names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    pub vendor: String,
    pub client_name: String,
}

impl Default for ClientIdentity {
    fn default() -> Self {
        Self {
            vendor: "mcp-client".into(),
            client_name: "MCP client".into(),
        }
    }
}

impl ClientIdentity {
    /// Read from an `initialize` request's `clientInfo`, where a client names
    /// itself.
    pub fn from_initialize(initialize: &Value) -> Self {
        let name = initialize
            .pointer("/params/clientInfo/name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty());
        match name {
            Some(name) => Self {
                vendor: name.to_ascii_lowercase().replace(' ', "-"),
                client_name: name.to_string(),
            },
            None => Self::default(),
        }
    }
}

/// One connection's write session: its client's identity and, once a write
/// has needed it, the session id.
#[derive(Debug, Default)]
pub struct ConnectionSession {
    client: std::sync::Mutex<ClientIdentity>,
    id: tokio::sync::Mutex<Option<String>>,
    /// Session ids a test hands out in place of the daemon's, in order, and
    /// how many were asked for.
    #[cfg(test)]
    test_ids: std::sync::Mutex<(std::collections::VecDeque<String>, usize)>,
}

impl ConnectionSession {
    /// Name the client the session will be opened for.
    pub fn set_client(&self, client: ClientIdentity) {
        *self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = client;
    }

    /// The arguments `kin_session_start` is sent to open this connection's
    /// session: the client's names, its folder and this process, and every
    /// capability declared, so the store's permissions alone bound what it
    /// writes and commits, and the toolchain run is opted in to.
    pub fn start_arguments(&self, cwd: &str) -> HashMap<String, Value> {
        let client = self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        HashMap::from([
            ("vendor".to_string(), json!(client.vendor)),
            ("client_name".to_string(), json!(client.client_name)),
            ("cwd".to_string(), json!(cwd)),
            ("transport".to_string(), json!("mcp")),
            ("pid".to_string(), json!(std::process::id())),
            (
                "capabilities".to_string(),
                json!({
                    "can_read": true,
                    "can_write": true,
                    "can_commit": true,
                    // The agent surface runs no programs, so its connection
                    // session never declares that it may.
                    "can_execute": false,
                }),
            ),
        ])
    }

    /// The session id, opening the session with `start` the first time. A
    /// failed start is returned and nothing is kept, so the next write tries
    /// again.
    pub async fn id_or_start<F, Fut>(&self, start: F) -> Result<String, String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<String, String>>,
    {
        let mut id = self.id.lock().await;
        if let Some(id) = id.as_ref() {
            return Ok(id.clone());
        }
        let started = start().await?;
        *id = Some(started.clone());
        Ok(started)
    }

    /// Drop `session_id` if it is still the one held, so the next write opens
    /// a new session. A different id means another call already replaced it.
    pub async fn forget(&self, session_id: &str) {
        let mut id = self.id.lock().await;
        if id.as_deref() == Some(session_id) {
            *id = None;
        }
    }

    /// Hand out `ids`, in order, in place of the daemon's sessions.
    #[cfg(test)]
    pub(crate) fn open_with(&self, ids: &[&str]) {
        let mut test_ids = self.test_ids.lock().unwrap();
        test_ids.0 = ids.iter().map(|id| id.to_string()).collect();
        test_ids.1 = 0;
    }

    /// The next test session id, when a test set some.
    #[cfg(test)]
    pub(crate) fn next_test_id(&self) -> Option<String> {
        let mut test_ids = self.test_ids.lock().unwrap();
        let next = test_ids.0.pop_front()?;
        test_ids.1 += 1;
        Some(next)
    }

    /// How many sessions a test's ids were asked for.
    #[cfg(test)]
    pub(crate) fn test_opens(&self) -> usize {
        self.test_ids.lock().unwrap().1
    }

    /// The session held now, if any.
    pub async fn current(&self) -> Option<String> {
        self.id.lock().await.clone()
    }
}

/// The session id a `kin_session_start` answer carries.
pub fn started_session_id(answer: &Value) -> Option<String> {
    let text = answer.pointer("/content/0/text").and_then(Value::as_str)?;
    let payload: Value = serde_json::from_str(text).ok()?;
    payload
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(value: Value) -> HashMap<String, Value> {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn only_a_write_that_names_no_session_takes_the_connections() {
        assert!(needs_connection_session(
            "kin_mutate",
            &arguments(json!({"operations": []}))
        ));
        assert!(!needs_connection_session(
            "kin_mutate",
            &arguments(json!({"session_id": "s"}))
        ));
        assert!(
            !needs_connection_session("kin_mutate", &arguments(json!({"request_id": "k"}))),
            "a keyed mutate keeps the session its caller retries under"
        );
        assert!(needs_connection_session(
            "kin_transaction_begin",
            &HashMap::new()
        ));
        assert!(!needs_connection_session(
            "find_references",
            &HashMap::new()
        ));
        assert!(!needs_connection_session(
            "kin_transaction_commit",
            &HashMap::new()
        ));
    }

    #[tokio::test]
    async fn the_session_is_opened_once_and_reopened_after_it_is_forgotten() {
        let session = ConnectionSession::default();
        let first = session
            .id_or_start(|| async { Ok("one".to_string()) })
            .await
            .unwrap();
        let again = session
            .id_or_start(|| async { panic!("an open session is reused") })
            .await
            .unwrap();
        assert_eq!((first.as_str(), again.as_str()), ("one", "one"));
        session.forget("other").await;
        assert_eq!(session.current().await.as_deref(), Some("one"));
        session.forget("one").await;
        let failed = session
            .id_or_start(|| async { Err("daemon down".to_string()) })
            .await;
        assert!(failed.is_err());
        assert_eq!(
            session.current().await,
            None,
            "a failed start keeps nothing"
        );
        let reopened = session
            .id_or_start(|| async { Ok("two".to_string()) })
            .await
            .unwrap();
        assert_eq!(reopened, "two");
    }

    #[test]
    fn the_session_names_the_client_its_process_and_its_capabilities() {
        let session = ConnectionSession::default();
        session.set_client(ClientIdentity::from_initialize(&json!({
            "params": {"clientInfo": {"name": "Claude Code", "version": "2"}}
        })));
        let start = session.start_arguments("/repo");
        assert_eq!(start["vendor"], "claude-code");
        assert_eq!(start["client_name"], "Claude Code");
        assert_eq!(start["cwd"], "/repo");
        assert_eq!(start["pid"], std::process::id());
        // A write and a commit are still bounded by the store: the daemon
        // grants each only where the store permits it.
        assert_eq!(
            start["capabilities"],
            json!({"can_read": true, "can_write": true, "can_commit": true, "can_execute": false})
        );
    }

    #[test]
    fn a_reaped_session_is_recognized_only_where_nothing_started() {
        let refusal =
            |text: &str| json!({"content": [{"type": "text", "text": text}], "isError": true});
        assert!(session_gone_before_start(
            "kin_mutate",
            "s",
            &refusal("kin_mutate_not_started: {\"refusal\":\"session_not_found\"}\nSession not found: s.")
        ));
        assert!(session_gone_before_start(
            "kin_transaction_begin",
            "s",
            &refusal("Session not found: s. Start a session with kin_session_start first.")
        ));
        assert!(!session_gone_before_start(
            "kin_mutate",
            "s",
            &refusal("commit refused: source_base_required")
        ));
        assert_eq!(
            started_session_id(
                &json!({"content": [{"type": "text", "text": "{\"session_id\":\"abc\"}"}]})
            ),
            Some("abc".to_string())
        );
    }
}
