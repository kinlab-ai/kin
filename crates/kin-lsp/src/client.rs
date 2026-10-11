// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! JSON-RPC 2.0 client over stdio with background reader.
//!
//! Architecture: a dedicated tokio task owns the BufReader<ChildStdout>
//! and reads all messages. A message with a `method` is the server's own
//! request or notification, whatever its id; only a message without one is a
//! response, dispatched by ID via oneshot channels. The server's requests are
//! answered from [`ServerRequestAnswers`]. Notifications are discarded, apart
//! from rust-analyzer's server status and the work a server reports through
//! `$/progress`, which are kept for the lifecycle to read, and a server's
//! report that the backend behind it exited (see [`ServerWatch`]). No mutex on
//! the read path.
//!
//! A request the client stops waiting for, because its own timeout or its
//! caller's ran out, is cancelled with `$/cancelRequest`, so the server does
//! not go on computing an answer nobody will read.
//!
//! A server whose own state has failed keeps answering, each time with the
//! same error. The client tells that apart from a question the server could
//! not answer (see [`JsonRpcClient::wedged`]) and from then on reads the server
//! as gone, so whoever drives it restarts it instead of recording each failure
//! as an answer about one position.
//!
//! A FIFO writer owns stdin. Normal writes retain one shared permit until
//! flushed, including after caller cancellation. Document cleanup can queue
//! synchronously on Drop, before the next pass opens the same document.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot, watch, Mutex, OwnedSemaphorePermit, Semaphore};
use tracing::debug;

use crate::error::{LspError, Result};
use crate::protocol::WorkspaceFolder;

/// Pending response waiters, keyed by request ID.
/// The background reader removes entries and fires the oneshot.
type WaiterMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value>>>>>;

enum Outbound {
    Message {
        body: String,
        ack: oneshot::Sender<Result<()>>,
        _permit: OwnedSemaphorePermit,
    },
    CloseDocuments {
        uris: Vec<String>,
        ack: Option<oneshot::Sender<Result<()>>>,
    },
    /// The answer to a request the server sent. Queued by the reader without a
    /// write permit, so reading never waits on a caller's write.
    Reply { body: String },
    /// `$/cancelRequest` for a request the client stopped waiting for. Queued
    /// without a write permit, from a drop, after the request it cancels.
    Cancel { body: String },
}

/// JSON-RPC's code for a method the receiver does not implement.
const METHOD_NOT_FOUND: i64 = -32601;

/// What this client answers when the server asks it something.
///
/// A server sends requests of its own: pyright asks for its settings, and
/// rust-analyzer asks for a diagnostics refresh numbered from its own counter.
/// Every one of them gets an answer, since a server may wait on it and the
/// protocol asks for one, and an answer never stands in for the response to a
/// request of Kin's.
#[derive(Debug, Clone, Default)]
pub struct ServerRequestAnswers {
    /// The settings `workspace/configuration` is answered from. `None`
    /// answers every section with `null`, which the protocol reads as "the
    /// client has no setting here".
    pub settings: Option<Value>,
    /// The folders `workspace/workspaceFolders` is answered with.
    pub workspace_folders: Vec<WorkspaceFolder>,
}

impl ServerRequestAnswers {
    /// The answer to one server request: a result, or a JSON-RPC error.
    pub fn answer(
        &self,
        method: &str,
        params: Option<&Value>,
    ) -> std::result::Result<Value, (i64, String)> {
        match method {
            "workspace/configuration" => {
                let items = params
                    .and_then(|params| params.get("items"))
                    .and_then(Value::as_array);
                Ok(Value::Array(
                    items
                        .into_iter()
                        .flatten()
                        .map(|item| self.section(item.get("section").and_then(Value::as_str)))
                        .collect(),
                ))
            }
            "workspace/workspaceFolders" => {
                Ok(serde_json::to_value(&self.workspace_folders).unwrap_or(Value::Null))
            }
            // Acknowledged and otherwise ignored: Kin shows no progress,
            // registers nothing dynamically and holds nothing to refresh.
            "window/workDoneProgress/create"
            | "window/showMessageRequest"
            | "client/registerCapability"
            | "client/unregisterCapability"
            | "workspace/diagnostic/refresh"
            | "workspace/semanticTokens/refresh"
            | "workspace/inlayHint/refresh"
            | "workspace/inlineValue/refresh"
            | "workspace/codeLens/refresh"
            | "workspace/foldingRange/refresh" => Ok(Value::Null),
            "workspace/applyEdit" => Ok(serde_json::json!({
                "applied": false,
                "failureReason": "Kin applies no edits a language server proposes",
            })),
            _ => Err((METHOD_NOT_FOUND, format!("Kin does not serve {method}"))),
        }
    }

    /// One configuration section, read the way editors read theirs: a dotted
    /// section is a path into the settings tree (`python.analysis` is
    /// `settings.python.analysis`), and a key spelled with the dots is taken
    /// as it is. A section with no setting is `null`; no section is the whole
    /// tree.
    pub fn section(&self, section: Option<&str>) -> Value {
        let Some(settings) = &self.settings else {
            return Value::Null;
        };
        let Some(section) = section.filter(|section| !section.is_empty()) else {
            return settings.clone();
        };
        section
            .split('.')
            .try_fold(settings, |node, part| node.get(part))
            .or_else(|| settings.get(section))
            .cloned()
            .unwrap_or(Value::Null)
    }
}

/// What the client watches the server's notifications for.
#[derive(Debug, Clone, Default)]
pub struct ServerWatch {
    /// Text an error-level `window/logMessage` or `window/showMessage`
    /// carries when the backend a server fronts has exited while the server
    /// itself runs on.
    ///
    /// typescript-language-server outlives its tsserver: it logs
    /// `[tsserver] Exited. Code: null. Signal: SIGABRT` and from then on
    /// answers every request with an empty result, which reads as nothing
    /// found. On drizzle-orm tsserver ran out of heap partway through a
    /// sweep, and every file after it was recorded as asked and answered
    /// with nothing. Seen, the report ends the connection the way a closed
    /// stdout does.
    pub backend_exit_report: Option<String>,
}

/// The work a server reported through `$/progress` notifications.
///
/// A server that indexes after it starts, such as gopls loading packages,
/// clangd indexing in the background or rust-analyzer fetching and indexing
/// its crates, reports each piece of work as a `begin`, any number of
/// `report`s and an `end` under one token.
/// Work that has begun and not ended is the server saying it is not done.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerProgress {
    /// The title each piece of work began with, by its token, while it runs.
    pub active: std::collections::BTreeMap<String, String>,
    /// How many pieces of work have begun.
    pub begun: u64,
    /// How many have ended.
    pub ended: u64,
}

impl ServerProgress {
    /// Read one `$/progress` notification's params. Returns whether anything
    /// changed. Progress that is not work done progress, such as a partial
    /// result, has no `kind` and is ignored.
    fn observe(&mut self, params: Option<&Value>) -> bool {
        let Some(params) = params else {
            return false;
        };
        let token = match params.get("token") {
            Some(Value::String(token)) => token.clone(),
            Some(Value::Number(token)) => token.to_string(),
            _ => return false,
        };
        let value = params.get("value");
        match value
            .and_then(|value| value.get("kind"))
            .and_then(Value::as_str)
        {
            Some("begin") => {
                let title = value
                    .and_then(|value| value.get("title"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.begun += 1;
                self.active.insert(token, title);
                true
            }
            Some("end") => {
                if self.active.remove(&token).is_some() {
                    self.ended += 1;
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Whether the server has work it began and has not ended.
    pub fn busy(&self) -> bool {
        !self.active.is_empty()
    }
}

#[cfg(test)]
type AckPause = Arc<std::sync::Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>>;

/// The real timeout registered by one request, observed by the controlled
/// clock in the wire-level integrity tests.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct RequestDeadline {
    pub id: i64,
    pub asked_at: tokio::time::Instant,
    pub at: tokio::time::Instant,
}

/// A JSON-RPC 2.0 client with FIFO writes and async response dispatch.
pub struct JsonRpcClient {
    writer: mpsc::Sender<Outbound>,
    write_slot: Arc<Semaphore>,
    failed: Arc<AtomicBool>,
    writer_handle: tokio::task::JoinHandle<()>,
    #[cfg(test)]
    ack_pause: AckPause,
    #[cfg(test)]
    request_deadline: watch::Sender<std::collections::BTreeMap<i64, RequestDeadline>>,
    waiters: WaiterMap,
    next_id: AtomicI64,
    /// Requests the server answered with a result rather than an error, a
    /// timeout or silence. See [`JsonRpcClient::answered`].
    answered: AtomicU64,
    /// How far this server's answer allowances are stretched now. See
    /// [`crate::pace`].
    pace: crate::pace::AnswerPace,
    /// The latest `experimental/serverStatus` the server sent, if any.
    server_status: watch::Receiver<Option<Value>>,
    /// The work the server reported through `$/progress`.
    progress: watch::Receiver<ServerProgress>,
    /// The server's report that its backend exited, once it made one. See
    /// [`ServerWatch::backend_exit_report`].
    backend_exit: Arc<std::sync::OnceLock<String>>,
    /// Why the server's own state is known to have failed, once it is. See
    /// [`JsonRpcClient::wedged`].
    wedged: std::sync::OnceLock<String>,
    /// The failures the server returned since it last answered anything.
    streak: std::sync::Mutex<FailureStreak>,
    /// Handle to the background reader task (kept alive with the client).
    /// Finished once the server's stdout has closed. See
    /// [`JsonRpcClient::is_disconnected`].
    reader_handle: tokio::task::JoinHandle<()>,
}

impl JsonRpcClient {
    /// A client that answers the server's requests with nothing configured:
    /// `null` for every setting, and no workspace folders.
    pub fn new(stdin: ChildStdin, stdout: tokio::process::ChildStdout) -> Self {
        Self::answering(stdin, stdout, ServerRequestAnswers::default())
    }

    /// A client that answers the server's requests from `answers`.
    pub fn answering(
        stdin: ChildStdin,
        stdout: tokio::process::ChildStdout,
        answers: ServerRequestAnswers,
    ) -> Self {
        Self::watching(stdin, stdout, answers, ServerWatch::default())
    }

    /// A client that answers the server's requests from `answers` and watches
    /// its notifications as `watch` says.
    pub fn watching(
        mut stdin: ChildStdin,
        stdout: tokio::process::ChildStdout,
        answers: ServerRequestAnswers,
        watch: ServerWatch,
    ) -> Self {
        let waiters: WaiterMap = Arc::new(Mutex::new(HashMap::new()));
        let reader_waiters = Arc::clone(&waiters);
        // Only one normal frame can be pending. The bounded queue also admits
        // small cleanup batches when an in-flight write's caller is cancelled.
        let (writer, mut outgoing) = mpsc::channel::<Outbound>(64);
        let write_slot = Arc::new(Semaphore::new(1));
        let failed = Arc::new(AtomicBool::new(false));
        let writer_failed = Arc::clone(&failed);
        let writer_waiters = Arc::clone(&waiters);
        #[cfg(test)]
        let ack_pause: AckPause = Arc::new(std::sync::Mutex::new(None));
        #[cfg(test)]
        let writer_pause = Arc::clone(&ack_pause);
        let writer_handle = tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                if writer_failed.load(Ordering::Acquire) {
                    break;
                }
                let result = match &message {
                    Outbound::Message { body, .. }
                    | Outbound::Reply { body }
                    | Outbound::Cancel { body } => write_frame(&mut stdin, body).await,
                    Outbound::CloseDocuments { uris, .. } => {
                        let mut result = Ok(());
                        for uri in uris {
                            let body = serde_json::json!({
                                "jsonrpc": "2.0", "method": "textDocument/didClose",
                                "params": { "textDocument": { "uri": uri } },
                            })
                            .to_string();
                            if let Err(error) = write_frame(&mut stdin, &body).await {
                                result = Err(error);
                                break;
                            }
                        }
                        result
                    }
                };
                #[cfg(test)]
                if !matches!(message, Outbound::Reply { .. } | Outbound::Cancel { .. }) {
                    let pause = writer_pause.lock().unwrap().take();
                    if let Some((entered, resume)) = pause {
                        entered.notify_one();
                        resume.notified().await;
                    }
                }
                if result.is_err() {
                    writer_failed.store(true, Ordering::Release);
                    fail_waiters(&writer_waiters).await;
                }
                let failed = result.is_err();
                match message {
                    Outbound::Message { ack, .. } => {
                        let _ = ack.send(result);
                    }
                    Outbound::CloseDocuments { ack: Some(ack), .. } => {
                        let _ = ack.send(result);
                    }
                    Outbound::CloseDocuments { ack: None, .. }
                    | Outbound::Reply { .. }
                    | Outbound::Cancel { .. } => {}
                }
                if failed {
                    break;
                }
            }
        });

        // Spawn background reader — owns stdout exclusively, no mutex on reads.
        // It holds the writer weakly, so the writer still ends when the client
        // is dropped rather than when the server closes its output.
        let replies = writer.downgrade();
        let (status_tx, server_status) = watch::channel(None);
        let (progress_tx, progress) = watch::channel(ServerProgress::default());
        let backend_exit = Arc::new(std::sync::OnceLock::new());
        let reported_exit = Arc::clone(&backend_exit);
        let reader_failed = Arc::clone(&failed);
        let reader_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_one_message(&mut reader).await {
                    Ok(msg) => {
                        // A message with a method is the server's own request
                        // or notification, never a response, whatever its id
                        // says. rust-analyzer numbers its requests from its own
                        // counter, so one can share an id with a pending Kin
                        // request; dispatched by id, it answered that request
                        // with `null`, which reads as nothing found.
                        if let Some(method) = msg.get("method").and_then(Value::as_str) {
                            match msg.get("id").filter(|id| !id.is_null()) {
                                Some(id) => answer_server_request(
                                    &replies,
                                    &answers,
                                    method,
                                    id,
                                    msg.get("params"),
                                ),
                                None if method == "experimental/serverStatus" => {
                                    status_tx.send_replace(msg.get("params").cloned());
                                }
                                None if method == "$/progress" => {
                                    progress_tx.send_if_modified(|progress| {
                                        progress.observe(msg.get("params"))
                                    });
                                }
                                None => {
                                    if let Some(report) =
                                        backend_exit_report(&watch, method, msg.get("params"))
                                    {
                                        // Everything waiting was asked of a
                                        // backend that is gone.
                                        if reported_exit.set(report).is_ok() {
                                            fail_waiters(&reader_waiters).await;
                                        }
                                    }
                                }
                            }
                            continue;
                        }
                        // Response (has "id", no "method") → dispatch to waiter.
                        if let Some(id) = msg.get("id").and_then(|v| v.as_i64()) {
                            let mut map = reader_waiters.lock().await;
                            if let Some(tx) = map.remove(&id) {
                                let result = if let Some(error) = msg.get("error") {
                                    Err(LspError::JsonRpc(error.to_string()))
                                } else {
                                    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                                };
                                let _ = tx.send(result); // Receiver may have dropped (timeout)
                            }
                        }
                        // Notification (no "id") → drop silently.
                    }
                    Err(_) => {
                        // Publish the terminal connection state before draining. A
                        // later waiter must not be registered after this drain and
                        // sent to a process that still accepts stdin but has no
                        // response stream left.
                        reader_failed.store(true, Ordering::Release);
                        fail_waiters(&reader_waiters).await;
                        break;
                    }
                }
            }
        });

        Self {
            writer,
            write_slot,
            failed,
            writer_handle,
            #[cfg(test)]
            ack_pause,
            #[cfg(test)]
            request_deadline: watch::channel(std::collections::BTreeMap::new()).0,
            waiters,
            next_id: AtomicI64::new(1),
            answered: AtomicU64::new(0),
            pace: crate::pace::AnswerPace::default(),
            server_status,
            progress,
            backend_exit,
            wedged: std::sync::OnceLock::new(),
            streak: std::sync::Mutex::new(FailureStreak::default()),
            reader_handle,
        }
    }

    /// The server's report that the backend behind it exited, once it made
    /// one. See [`ServerWatch::backend_exit_report`].
    pub fn backend_exit(&self) -> Option<&str> {
        self.backend_exit.get().map(String::as_str)
    }

    /// The server's latest `experimental/serverStatus`, which rust-analyzer
    /// sends when the client claims the capability. Holds `None` until the
    /// first one arrives.
    pub fn server_status(&self) -> watch::Receiver<Option<Value>> {
        self.server_status.clone()
    }

    /// The work this server has reported through `$/progress`: what it has
    /// begun and not yet ended, and how much it has begun and ended in all.
    /// A server reports work only to a client that claims
    /// `window.workDoneProgress`, which every launch does.
    pub fn progress(&self) -> watch::Receiver<ServerProgress> {
        self.progress.clone()
    }

    #[cfg(test)]
    pub(crate) fn request_deadlines_for_tests(
        &self,
    ) -> watch::Receiver<std::collections::BTreeMap<i64, RequestDeadline>> {
        self.request_deadline.subscribe()
    }

    #[cfg(test)]
    pub(crate) async fn request_ids_for_tests(&self) -> std::collections::BTreeSet<i64> {
        self.waiters.lock().await.keys().copied().collect()
    }

    /// Whether the connection to the server is gone: its stdout closed, or
    /// a write to its stdin failed.
    ///
    /// Either way no request sent from now on can be answered. A server that
    /// exits mid-session fails every later request at once rather than
    /// hanging, which is exactly what lets a caller that never looks at this
    /// ask a dead server about hundreds of files and record each failure as
    /// an answer that did not arrive. A caller that asks first can stop.
    ///
    /// A server that reported its backend exited counts as gone too: it can
    /// still reply, but only with empty answers.
    ///
    /// A server whose own state failed (see [`Self::wedged`]) counts as gone
    /// too: it still replies, but only with the same failure.
    pub fn is_disconnected(&self) -> bool {
        self.failed.load(Ordering::Acquire)
            || self.reader_handle.is_finished()
            || self.backend_exit.get().is_some()
            || self.wedged.get().is_some()
    }

    /// Why the server's own state is known to have failed, once it is.
    ///
    /// Two things say so. An error the server's adapter recognizes as a
    /// failure of the server rather than of a question (see
    /// [`crate::error::server_state_failure`]), and, for any server, one
    /// identical error returned [`WEDGE_REPEATS`] times in a row about at
    /// least [`WEDGE_DOCUMENTS`] documents, with nothing answered in between,
    /// by a server that had answered before. A question a server cannot
    /// answer fails at its own position and the next one is answered; a file
    /// no project holds fails with its own path in the error; a server that
    /// never answered anything is misconfigured rather than broken, and a
    /// restart would not help it. None of those is read as a failed state.
    ///
    /// Once set, every request fails at once with
    /// [`LspError::ServerWedged`] and [`Self::is_disconnected`] holds, so the
    /// server is retired and replaced rather than asked about every
    /// remaining file.
    pub fn wedged(&self) -> Option<&str> {
        self.wedged.get().map(String::as_str)
    }

    /// Record that the server's own state failed, and say why. The first
    /// reason recorded stands.
    fn wedge(&self, reason: String) -> String {
        let _ = self.wedged.set(reason);
        self.wedged.get().cloned().unwrap_or_default()
    }

    /// Read one error the server returned about `document`, and say why its
    /// state has failed when this error shows that it has.
    fn observe_failure(&self, error: &str, document: Option<&str>) -> Option<String> {
        if let Some(reason) = crate::error::server_state_failure(error) {
            return Some(self.wedge(reason));
        }
        let mut streak = self
            .streak
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if streak.error.as_deref() != Some(error) {
            *streak = FailureStreak {
                error: Some(error.to_string()),
                repeats: 0,
                documents: BTreeSet::new(),
                answered_before: self.answered() > 0,
            };
        }
        streak.repeats += 1;
        if let Some(document) = document {
            if streak.documents.len() < WEDGE_DOCUMENTS {
                streak.documents.insert(document.to_string());
            }
        }
        if !(streak.answered_before
            && streak.repeats >= WEDGE_REPEATS
            && streak.documents.len() >= WEDGE_DOCUMENTS)
        {
            return None;
        }
        let reason = format!(
            "it failed {} requests in a row about {} or more documents with one identical \
             error, after answering earlier ones: {}",
            streak.repeats,
            streak.documents.len(),
            last_chars(error, WEDGE_ERROR_IN_REASON)
        );
        drop(streak);
        Some(self.wedge(reason))
    }

    /// Forget the failures since the server last answered: it answered.
    fn observe_answer(&self) {
        let mut streak = self
            .streak
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if streak.error.is_some() {
            *streak = FailureStreak::default();
        }
    }

    /// How far this server's answer allowances are stretched now, for a
    /// caller that budgets several requests at once.
    pub fn pace(&self) -> &crate::pace::AnswerPace {
        &self.pace
    }

    /// How many requests this server has answered with a result.
    ///
    /// A caller that takes this before and after its work on one document
    /// learns whether the server answered anything about it at all. A server
    /// that cannot load a document refuses every request about it, and when
    /// it phrases each refusal the way it phrases an ordinary decline, this
    /// count is what still tells the two apart.
    pub fn answered(&self) -> u64 {
        self.answered.load(Ordering::Relaxed)
    }

    /// Send a request and wait for the response (with 10s timeout).
    ///
    /// A request this stops waiting for is cancelled: when the 10 seconds
    /// run out, and when the caller drops the returned future, as an outer
    /// timeout does. A server otherwise goes on computing the answer. On
    /// drizzle-orm a references query Kin had given up on grew tsserver from
    /// 5.6 GB to its 16 GB ceiling in 70 seconds, and tsserver died of it.
    pub async fn request<P: Serialize>(&self, method: &str, params: P) -> Result<Value> {
        self.request_within(method, params, std::time::Duration::from_secs(10))
            .await
    }

    /// [`Self::request`] with a timeout of the caller's own, for a request
    /// whose answer may legitimately wait on the server loading a project.
    pub async fn request_within<P: Serialize>(
        &self,
        method: &str,
        params: P,
        timeout: std::time::Duration,
    ) -> Result<Value> {
        if self.backend_exit.get().is_some() {
            return Err(LspError::ServerDied);
        }
        if let Some(reason) = self.wedged() {
            return Err(LspError::ServerWedged(reason.to_string()));
        }
        if self.failed.load(Ordering::Acquire) || self.reader_handle.is_finished() {
            return Err(LspError::ServerDied);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

        // Register waiter BEFORE sending (no race with the reader).
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().await.insert(id, tx);
        let mut pending = Pending {
            writer: &self.writer,
            id,
            answered: false,
            #[cfg(test)]
            deadlines: &self.request_deadline,
        };

        // Send the request.
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let document = request_document(&request);
        if let Err(e) = self.send_message(&request).await {
            self.waiters.lock().await.remove(&id);
            return Err(e);
        }

        // Wait for the background reader to dispatch our response, for as
        // long as this server's recent answers say it needs.
        #[cfg(not(test))]
        let asked = std::time::Instant::now();
        #[cfg(test)]
        let asked = tokio::time::Instant::now();
        let allowance = self.pace.allow(timeout);
        #[cfg(test)]
        let deadline = asked + allowance;
        let reply = tokio::time::timeout(allowance, rx);
        #[cfg(test)]
        self.request_deadline.send_modify(|waiting| {
            waiting.insert(
                id,
                RequestDeadline {
                    id,
                    asked_at: asked,
                    at: deadline,
                },
            );
        });
        let reply = reply.await;
        pending.answered = reply.is_ok();
        match &reply {
            Ok(_) => self.pace.answered(asked.elapsed(), timeout),
            Err(_) => self.pace.timed_out(),
        }
        match reply {
            // A reply read after the backend's exit report is the empty
            // answer of a server with nothing behind it.
            Ok(Ok(Ok(_))) if self.backend_exit.get().is_some() => Err(LspError::ServerDied),
            Ok(Ok(result)) => match result {
                Ok(value) => {
                    self.observe_answer();
                    self.answered.fetch_add(1, Ordering::Relaxed);
                    Ok(value)
                }
                // The one place an error answer is read as a decline: here, where
                // the method it answered is known. Every caller then classifies
                // with `LspError::class` and never looks at the code itself.
                // A decline is an answer, so it ends a run of failures. Any
                // other error is read for what it says about the server
                // before it is handed back.
                Err(LspError::JsonRpc(error)) => {
                    match crate::error::declined_answer(method, &error) {
                        Some(message) => {
                            self.observe_answer();
                            Err(LspError::Declined {
                                method: method.to_string(),
                                message,
                            })
                        }
                        None => match self.observe_failure(&error, document.as_deref()) {
                            Some(reason) => {
                                tracing::warn!(
                                    method,
                                    %reason,
                                    "the language server's own state failed; it is read as gone \
                                     from here on, so it is restarted rather than asked again"
                                );
                                Err(LspError::ServerWedged(reason))
                            }
                            None => Err(LspError::JsonRpc(error)),
                        },
                    }
                }
                Err(error) => Err(error),
            },
            Ok(Err(_)) => Err(LspError::ServerDied), // Sender dropped (reader died)
            Err(_) => {
                // Timeout — clean up the waiter.
                self.waiters.lock().await.remove(&id);
                Err(LspError::Timeout)
            }
        }
    }

    /// Send a notification (no response expected).
    pub async fn notify<P: Serialize>(&self, method: &str, params: P) -> Result<()> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.send_message(&notification).await
    }

    /// Synchronously queue all owned-document closes. Waiting for the returned
    /// acknowledgment is optional; enqueue order survives caller cancellation.
    pub(crate) fn close_documents(
        &self,
        uris: Vec<String>,
    ) -> Result<oneshot::Receiver<Result<()>>> {
        if self.failed.load(Ordering::Acquire) {
            return Err(LspError::ServerDied);
        }
        let (ack, done) = oneshot::channel();
        let message = Outbound::CloseDocuments {
            uris,
            ack: Some(ack),
        };
        if self.writer.try_send(message).is_err() {
            self.fail_writer();
            return Err(LspError::ServerDied);
        }
        Ok(done)
    }

    fn fail_writer(&self) {
        if !self.failed.swap(true, Ordering::AcqRel) {
            self.writer_handle.abort();
            let waiters = Arc::clone(&self.waiters);
            tokio::spawn(async move {
                fail_waiters(&waiters).await;
            });
        }
    }

    #[cfg(test)]
    pub(crate) fn pause_next_write_ack(
        &self,
    ) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        *self.ack_pause.lock().unwrap() = Some((entered.clone(), resume.clone()));
        (entered, resume)
    }

    /// Send one frame. The writer owns the permit, so dropping this future
    /// cannot admit another document body while the first is blocked on IO.
    async fn send_message(&self, message: &Value) -> Result<()> {
        let permit = self
            .write_slot
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| LspError::ServerDied)?;
        if self.failed.load(Ordering::Acquire) {
            return Err(LspError::ServerDied);
        }
        let body = serde_json::to_string(message)?;
        let (ack, done) = oneshot::channel();
        if self
            .writer
            .try_send(Outbound::Message {
                body,
                ack,
                _permit: permit,
            })
            .is_err()
        {
            self.fail_writer();
            return Err(LspError::ServerDied);
        }
        done.await.map_err(|_| LspError::ServerDied)??;
        debug!(
            method = message
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("response"),
            "sent message"
        );
        Ok(())
    }
}

/// How many requests in a row must fail with one identical error, with
/// nothing answered between them, before a server that had answered earlier is
/// read as having failed itself (see [`JsonRpcClient::wedged`]).
pub const WEDGE_REPEATS: usize = 16;

/// How many different documents those requests must be about. One document's
/// questions can all fail for a reason of that document's own; the same
/// failure about several says the server, not the documents, is at fault.
pub const WEDGE_DOCUMENTS: usize = 3;

/// How much of the repeated error a wedge's reason carries.
const WEDGE_ERROR_IN_REASON: usize = 512;

/// The errors a server returned since it last answered anything, while they
/// are all the same error.
#[derive(Debug, Default)]
struct FailureStreak {
    /// The error every request in the run failed with, exactly as received.
    error: Option<String>,
    repeats: usize,
    /// The documents those requests were about, up to [`WEDGE_DOCUMENTS`].
    documents: BTreeSet<String>,
    /// Whether the server had answered any request before the run began.
    answered_before: bool,
}

/// The document a request is about: its `textDocument`, or the `item` a call
/// or type hierarchy request carries.
fn request_document(request: &Value) -> Option<String> {
    let params = request.get("params")?;
    params
        .get("textDocument")
        .or_else(|| params.get("item"))
        .and_then(|document| document.get("uri"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The last `max` characters of `text`, marked when cut.
fn last_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let tail: String = text.chars().skip(count - max).collect();
    format!("...{tail}")
}

/// A request in flight. Dropped before its reply arrived, it cancels the
/// request with the server.
struct Pending<'a> {
    writer: &'a mpsc::Sender<Outbound>,
    id: i64,
    answered: bool,
    #[cfg(test)]
    deadlines: &'a watch::Sender<std::collections::BTreeMap<i64, RequestDeadline>>,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        #[cfg(test)]
        self.deadlines
            .send_if_modified(|waiting| waiting.remove(&self.id).is_some());
        if self.answered {
            return;
        }
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "$/cancelRequest",
            "params": { "id": self.id },
        })
        .to_string();
        // Best effort: a server that never sees the cancellation answers a
        // reply nobody reads, as it did before. The writer's queue is FIFO,
        // so the cancellation follows the request it names.
        match self.writer.try_send(Outbound::Cancel { body }) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(message)) => {
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    let writer = self.writer.clone();
                    runtime.spawn(async move {
                        let _ = writer.send(message).await;
                    });
                }
            }
        }
        debug!(
            id = self.id,
            "cancelled a request the client stopped waiting for"
        );
    }
}

/// The report in one server notification that the backend behind the server
/// exited, when `watch` names one and the notification is an error-level
/// message carrying it.
fn backend_exit_report(
    watch: &ServerWatch,
    method: &str,
    params: Option<&Value>,
) -> Option<String> {
    let marker = watch.backend_exit_report.as_deref()?;
    if !matches!(method, "window/logMessage" | "window/showMessage") {
        return None;
    }
    let params = params?;
    // MessageType.Error is 1.
    if params.get("type").and_then(Value::as_i64) != Some(1) {
        return None;
    }
    let message = params.get("message").and_then(Value::as_str)?;
    message.contains(marker).then(|| message.trim().to_string())
}

/// Answer one request the server sent, from `answers`.
///
/// The reply is queued without waiting, since the reader must never block on
/// the writer. A full queue hands the reply to a task that waits for room, so a
/// reply is never dropped while the server may be waiting on it.
fn answer_server_request(
    replies: &mpsc::WeakSender<Outbound>,
    answers: &ServerRequestAnswers,
    method: &str,
    id: &Value,
    params: Option<&Value>,
) {
    let reply = match answers.answer(method, params) {
        Ok(result) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }),
    };
    let message = Outbound::Reply {
        body: reply.to_string(),
    };
    let Some(writer) = replies.upgrade() else {
        return;
    };
    match writer.try_send(message) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(message)) => {
            tokio::spawn(async move {
                let _ = writer.send(message).await;
            });
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {}
    }
    debug!(method, "answered a request the server sent");
}

async fn fail_waiters(waiters: &WaiterMap) {
    for (_, waiter) in waiters.lock().await.drain() {
        let _ = waiter.send(Err(LspError::ServerDied));
    }
}

async fn write_frame(stdin: &mut ChildStdin, body: &str) -> Result<()> {
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    stdin.write_all(header.as_bytes()).await?;
    stdin.write_all(body.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

/// Read one JSON-RPC message from a BufReader (Content-Length delimited).
/// This is a free function — no &self, no mutex. Called only by the reader task.
async fn read_one_message(
    reader: &mut BufReader<tokio::process::ChildStdout>,
) -> std::result::Result<Value, LspError> {
    // Read headers until blank line.
    let mut content_length: Option<usize> = None;
    let mut header_line = String::new();
    loop {
        header_line.clear();
        let bytes_read = reader.read_line(&mut header_line).await?;
        if bytes_read == 0 {
            return Err(LspError::ServerDied);
        }
        let trimmed = header_line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(len_str) = trimmed.strip_prefix("Content-Length: ") {
            content_length = len_str.parse().ok();
        }
    }

    let length = content_length
        .ok_or_else(|| LspError::Protocol("missing Content-Length header".to_string()))?;

    // Read exactly `length` bytes of body.
    let mut body = vec![0u8; length];
    tokio::io::AsyncReadExt::read_exact(reader, &mut body).await?;

    let value: Value = serde_json::from_slice(&body)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    #[test]
    fn json_rpc_request_format() {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {},
        });
        let body = serde_json::to_string(&request).unwrap();
        assert!(body.contains("\"jsonrpc\":\"2.0\""));
        assert!(body.contains("\"method\":\"initialize\""));
    }

    /// EOF is observed independently of request registration. The peer keeps
    /// stdin open, so a successful write cannot stand in for a response stream.
    /// Polling once after the reader joined checks immediate failure without a
    /// timing threshold or waiting out a request timeout.
    #[tokio::test]
    async fn a_request_after_stdout_closed_fails_before_sending() {
        use std::future::Future;
        use std::task::Poll;

        let mut server = crate::lifecycle::LspServer::scripted_for_tests(
            "import os, sys\nos.close(1)\nsys.stdin.read()\n",
            serde_json::json!({}),
        );
        (&mut server.client.reader_handle).await.unwrap();
        {
            let mut request = Box::pin(server.client.request("initialize", serde_json::json!({})));
            std::future::poll_fn(|context| match request.as_mut().poll(context) {
                Poll::Ready(Err(crate::error::LspError::ServerDied)) => Poll::Ready(()),
                other => panic!("a closed response stream must fail immediately, not {other:?}"),
            })
            .await;
        }
        assert!(server.client.waiters.lock().await.is_empty());
        assert_eq!(
            server
                .client
                .next_id
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        server.shutdown().await.unwrap();
    }

    /// The reader must wake a pending request even while the writer has not
    /// acknowledged its flushed frame. The EOF and ack barriers establish the
    /// ordering directly, with no sleep or elapsed-time assumption.
    #[tokio::test]
    async fn a_pending_request_fails_when_stdout_closes_before_its_write_ack() {
        let server = crate::lifecycle::LspServer::scripted_for_tests(
            "import os, sys\nsys.stdin.buffer.readline()\nos.close(1)\nsys.stdin.read()\n",
            serde_json::json!({}),
        );
        let (entered, resume) = server.client.pause_next_write_ack();
        let mut status = server.client.server_status();
        let eof_before_ack = async {
            entered.notified().await;
            assert!(
                status.changed().await.is_err(),
                "EOF closes the reader's observations"
            );
            resume.notify_one();
        };
        let (answer, ()) = tokio::join!(
            server.client.request("initialize", serde_json::json!({})),
            eof_before_ack,
        );
        assert!(
            matches!(answer, Err(crate::error::LspError::ServerDied)),
            "{answer:?}"
        );
        assert!(server.client.is_disconnected());
        assert!(server.client.waiters.lock().await.is_empty());
        drop(status);
        server.shutdown().await.unwrap();
    }

    /// A peer that, asked anything, first asks the client something of its own
    /// under the very id the client is waiting on, the way rust-analyzer numbers
    /// `workspace/diagnostic/refresh` from its own counter. It sends a
    /// notification too, reads the client's reply to its request, and only then
    /// answers, returning that reply inside its result.
    const ASKING_PEER: &str = r#"
import json, os, sys

def read():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b"\n", b"\r\n"):
            break
        name, value = line.decode().split(":", 1)
        headers[name.lower()] = value.strip()
    return json.loads(sys.stdin.buffer.read(int(headers["content-length"])))

def write(message):
    payload = json.dumps(message).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(payload) + payload)
    sys.stdout.buffer.flush()

asks = json.loads(os.environ["KIN_LSP_TEST_RESPONSES"])
while True:
    message = read()
    if "id" not in message:
        continue
    ask = asks.get(message["method"], {"method": "workspace/diagnostic/refresh"})
    write({"jsonrpc": "2.0", "id": message["id"], **ask})
    write({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 3, "message": "x"}})
    reply = read()
    write({"jsonrpc": "2.0", "id": message["id"], "result": {"asked": message["method"], "reply": reply}})
"#;

    /// The defect: a request the server sends carries its own id, and the
    /// reader took any message with an integer id for a response. A server
    /// request numbered like a pending Kin request answered that request with
    /// `null`, which reads as "nothing found", and the server's own request was
    /// never answered.
    #[tokio::test]
    async fn a_server_request_sharing_a_pending_id_is_answered_and_never_taken_for_the_response() {
        let server =
            crate::lifecycle::LspServer::scripted_for_tests(ASKING_PEER, serde_json::json!({}));
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            server
                .client
                .request("textDocument/definition", serde_json::json!({})),
        )
        .await
        .expect("the peer answers once the client has replied to its own request")
        .expect("the request succeeds");

        assert_eq!(
            answer["asked"], "textDocument/definition",
            "the pending request was resolved by the server's own request: {answer}"
        );
        let reply = &answer["reply"];
        assert_eq!(
            reply["id"], 1,
            "the reply answers the server's request id: {reply}"
        );
        assert!(reply.get("method").is_none(), "{reply}");
        assert!(
            reply.get("result").is_some_and(serde_json::Value::is_null),
            "a refresh request is acknowledged with a null result: {reply}"
        );
        server.shutdown().await.unwrap();
    }

    /// pyright's settings request, answered on the wire from the settings the
    /// client was started with, one answer per section.
    #[tokio::test]
    async fn workspace_configuration_is_answered_from_the_settings() {
        let answers = super::ServerRequestAnswers {
            settings: Some(serde_json::json!({
                "python": {"pythonPath": "/env/bin/python3", "analysis": {"extraPaths": ["/repo/src"]}},
            })),
            workspace_folders: Vec::new(),
        };
        let asks = serde_json::json!({
            "textDocument/hover": {
                "method": "workspace/configuration",
                "params": {"items": [
                    {"section": "python"},
                    {"section": "python.analysis"},
                    {"section": "pyright"},
                ]},
            },
        });
        let server =
            crate::lifecycle::LspServer::scripted_for_tests_answering(ASKING_PEER, asks, answers);
        let answer = server
            .client
            .request("textDocument/hover", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            answer["reply"]["result"],
            serde_json::json!([
                {"pythonPath": "/env/bin/python3", "analysis": {"extraPaths": ["/repo/src"]}},
                {"extraPaths": ["/repo/src"]},
                null,
            ])
        );
        server.shutdown().await.unwrap();
    }

    /// A request Kin does not serve is refused with MethodNotFound rather than
    /// left unanswered.
    #[tokio::test]
    async fn an_unserved_server_request_is_refused_with_method_not_found() {
        let asks = serde_json::json!({
            "textDocument/hover": {"method": "workspace/showDocument", "params": {}},
        });
        let server = crate::lifecycle::LspServer::scripted_for_tests(ASKING_PEER, asks);
        let answer = server
            .client
            .request("textDocument/hover", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(answer["reply"]["error"]["code"], super::METHOD_NOT_FOUND);
        server.shutdown().await.unwrap();
    }

    #[test]
    fn sections_are_read_as_paths_or_flat_keys_and_absent_ones_are_null() {
        let answers = super::ServerRequestAnswers {
            settings: Some(serde_json::json!({
                "gopls": {"buildFlags": ["-tags=integration"]},
                "flat.key": 1,
            })),
            workspace_folders: Vec::new(),
        };
        assert_eq!(
            answers.section(Some("gopls")),
            serde_json::json!({"buildFlags": ["-tags=integration"]})
        );
        assert_eq!(answers.section(Some("flat.key")), serde_json::json!(1));
        assert_eq!(
            answers.section(Some("gopls.missing")),
            serde_json::Value::Null
        );
        assert_eq!(answers.section(None), answers.settings.clone().unwrap());
        assert_eq!(
            super::ServerRequestAnswers::default().section(Some("python")),
            serde_json::Value::Null
        );
    }

    /// rust-analyzer's status notification is kept, and no other
    /// notification disturbs it.
    #[tokio::test]
    async fn the_latest_server_status_is_kept() {
        const STATUS_PEER: &str = r#"
import json, sys
def write(message):
    payload = json.dumps(message).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(payload) + payload)
    sys.stdout.buffer.flush()
write({"jsonrpc": "2.0", "method": "experimental/serverStatus", "params": {"health": "ok", "quiescent": False}})
write({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 3, "message": "x"}})
write({"jsonrpc": "2.0", "method": "experimental/serverStatus", "params": {"health": "ok", "quiescent": True}})
"#;
        // Status delivery is the behavior under test; use the framed peer
        // afterwards so ordinary shutdown is answered rather than timing out.
        let script = format!("{STATUS_PEER}\n{PEER}");
        let server =
            crate::lifecycle::LspServer::scripted_for_tests(&script, serde_json::json!({}));
        let mut status = server.client.server_status();
        let reported = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            status.wait_for(|status| {
                status
                    .as_ref()
                    .is_some_and(|status| status["quiescent"] == true)
            }),
        )
        .await
        .expect("the status arrives")
        .expect("the reader is alive")
        .clone();
        assert_eq!(reported.unwrap()["health"], "ok");
        drop(status);
        server.shutdown().await.unwrap();
    }

    const PEER: &str = include_str!("enrichment_test_peer.py");

    /// A definition request at `column` in `document`.
    async fn ask(
        server: &crate::lifecycle::LspServer,
        document: &str,
        column: u32,
    ) -> crate::error::Result<serde_json::Value> {
        server
            .client
            .request(
                "textDocument/definition",
                serde_json::json!({
                    "textDocument": {"uri": document},
                    "position": {"line": 0, "character": column},
                }),
            )
            .await
    }

    /// tsserver asserting while it updates its project fails every question
    /// the same way until it restarts. The first such error reads the server
    /// as gone, and every later request fails at once without being sent, so
    /// the sweep retires the server rather than asking it about every
    /// remaining file. Falsify by handing the error back as it came: on
    /// React 881,402 queries over 1615 files each got the same assertion.
    #[tokio::test]
    async fn a_server_asserting_while_updating_its_project_is_read_as_gone() {
        let assertion: serde_json::Value =
            serde_json::from_str(&crate::error::tests::project_update_assertion()).unwrap();
        let server = crate::lifecycle::LspServer::scripted_for_tests(
            PEER,
            serde_json::json!({
                "textDocument/definition@file:///w/a.js#1": {"result": []},
                "textDocument/definition": {"error": assertion},
            }),
        );
        ask(&server, "file:///w/a.js", 1).await.expect("answered");
        assert!(!server.client.is_disconnected());
        let error = ask(&server, "file:///w/b.js", 0).await.unwrap_err();
        let crate::error::LspError::ServerWedged(reason) = &error else {
            panic!("a server failure, not {error:?}");
        };
        assert!(reason.contains("updating its project"), "{reason}");
        assert!(server.client.is_disconnected());
        assert_eq!(server.client.wedged(), Some(reason.as_str()));
        // Even a question it would answer is not asked again.
        let again = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            ask(&server, "file:///w/a.js", 1),
        )
        .await
        .expect("fails at once");
        assert!(matches!(
            again,
            Err(crate::error::LspError::ServerWedged(_))
        ));
        server.shutdown().await.unwrap();
    }

    /// tsserver answering that it holds no source file for a document it was
    /// handed has lost its own project state for that document, and asked
    /// again it gives the same answer: on React 11,737 questions about one
    /// file did, over 28 seconds. The first such answer reads the server as
    /// gone, so the sweep retires it and asks again on a fresh one.
    #[tokio::test]
    async fn a_server_that_lost_an_open_document_is_read_as_gone() {
        let lost = serde_json::json!({
            "code": 1,
            "message": "<main> TypeScript Server Error (5.7.3)\nCould not find source file: \
                '/w/b.js'.\nError: Could not find source file: '/w/b.js'.\n    at \
                getValidSourceFile (/env/typescript.js:1:1)",
        });
        let server = crate::lifecycle::LspServer::scripted_for_tests(
            PEER,
            serde_json::json!({
                "textDocument/definition@file:///w/a.js#1": {"result": []},
                "textDocument/definition": {"error": lost},
            }),
        );
        ask(&server, "file:///w/a.js", 1).await.expect("answered");
        assert!(!server.client.is_disconnected());
        let error = ask(&server, "file:///w/b.js", 0).await.unwrap_err();
        let crate::error::LspError::ServerWedged(reason) = &error else {
            panic!("a server failure, not {error:?}");
        };
        assert!(reason.contains("no source file"), "{reason}");
        assert!(server.client.is_disconnected());
        server.shutdown().await.unwrap();
    }

    /// Any server that answered, then returns one identical error to every
    /// question about several documents with nothing answered in between, is
    /// read as failed. A run too short, about too few documents, broken by an
    /// answer or a decline, of errors that differ, or from a server that never
    /// answered at all, is not.
    #[tokio::test]
    async fn one_error_repeated_across_documents_after_answers_is_a_failed_server() {
        use super::{WEDGE_DOCUMENTS, WEDGE_REPEATS};
        let broken = serde_json::json!({"code": -32603, "message": "internal error: state lost"});
        let responses = serde_json::json!({
            "textDocument/definition@file:///w/ok.rs#1": {"result": []},
            "textDocument/references@file:///w/declined.rs#1": {"error": {"code": 0, "message": "no identifier found"}},
            "textDocument/definition": {"error": broken},
        });
        let document = |n: usize| format!("file:///w/{n}.rs");

        // Never answered: a misconfigured server, which a restart would not
        // cure. Its errors are handed back as they are.
        let server = crate::lifecycle::LspServer::scripted_for_tests(PEER, responses.clone());
        for n in 0..WEDGE_REPEATS * 2 {
            let error = ask(&server, &document(n % (WEDGE_DOCUMENTS + 2)), 0)
                .await
                .unwrap_err();
            assert!(
                matches!(error, crate::error::LspError::JsonRpc(_)),
                "{error:?}"
            );
        }
        assert!(!server.client.is_disconnected());

        // Answers break every run, and so does a decline, which is an answer.
        ask(&server, "file:///w/ok.rs", 1).await.unwrap();
        for round in 0..3 {
            for n in 0..WEDGE_REPEATS - 1 {
                assert!(matches!(
                    ask(&server, &document(n), 0).await,
                    Err(crate::error::LspError::JsonRpc(_))
                ));
            }
            if round % 2 == 0 {
                ask(&server, "file:///w/ok.rs", 1).await.unwrap();
            } else {
                let declined = server
                    .client
                    .request(
                        "textDocument/references",
                        serde_json::json!({"textDocument": {"uri": "file:///w/declined.rs"}, "position": {"line": 0, "character": 1}}),
                    )
                    .await;
                assert!(
                    matches!(declined, Err(crate::error::LspError::Declined { .. })),
                    "{declined:?}"
                );
            }
        }
        assert!(!server.client.is_disconnected());

        // A long run about too few documents is those documents' own.
        for n in 0..WEDGE_REPEATS * 2 {
            assert!(matches!(
                ask(&server, &document(n % (WEDGE_DOCUMENTS - 1)), 0).await,
                Err(crate::error::LspError::JsonRpc(_))
            ));
        }
        assert!(!server.client.is_disconnected());

        // The same error across enough documents is the server's.
        let mut failed = None;
        for n in 0..WEDGE_REPEATS * 2 {
            match ask(&server, &document(n), 0).await {
                Err(crate::error::LspError::ServerWedged(reason)) => {
                    failed = Some((n, reason));
                    break;
                }
                other => assert!(matches!(other, Err(crate::error::LspError::JsonRpc(_)))),
            }
        }
        let (at, reason) = failed.expect("the repeated error reads as a failed server");
        assert!(at < WEDGE_REPEATS * 2, "{at}");
        assert!(reason.contains("state lost"), "{reason}");
        assert!(server.client.is_disconnected());
        server.shutdown().await.unwrap();

        // Errors that name their own document are not one error.
        let mut per_document = serde_json::json!({
            "textDocument/definition@file:///w/ok.rs#1": {"result": []},
        });
        for n in 0..WEDGE_REPEATS * 2 {
            per_document[format!("textDocument/definition@{}#0", document(n))] = serde_json::json!(
                {"error": {"code": 0, "message": format!("no package metadata for file {}", document(n))}}
            );
        }
        let server = crate::lifecycle::LspServer::scripted_for_tests(PEER, per_document);
        ask(&server, "file:///w/ok.rs", 1).await.unwrap();
        for n in 0..WEDGE_REPEATS * 2 {
            for _ in 0..WEDGE_REPEATS {
                assert!(matches!(
                    ask(&server, &document(n), 0).await,
                    Err(crate::error::LspError::JsonRpc(_))
                ));
            }
        }
        assert!(!server.client.is_disconnected());
        server.shutdown().await.unwrap();
    }
}
