// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use thiserror::Error;

pub type Result<T> = std::result::Result<T, LspError>;

#[derive(Debug, Error)]
pub enum LspError {
    #[error("server not found: {0}")]
    ServerNotFound(String),

    #[error("server failed to start: {0}")]
    ServerStartFailed(String),

    #[error("server initialization failed: {0}")]
    InitializeFailed(String),

    #[error("JSON-RPC error: {0}")]
    JsonRpc(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("timeout waiting for response")]
    Timeout,

    #[error("server shutdown unexpectedly")]
    ServerDied,

    /// A start or handshake failure, carrying whatever the server wrote to its
    /// own stderr before it went.
    ///
    /// The reason is the original failure's message; the tail is bounded and
    /// may be truncated to its last bytes. This variant exists because a server
    /// that dies before it can frame a JSON-RPC reply has nowhere else to say
    /// why, and discarding stderr made those failures unattributable.
    #[error("{reason} (server stderr: {stderr})")]
    ServerFailedWithStderr { reason: String, stderr: String },

    /// A server that started and did not complete the initialize handshake,
    /// with the whole reason: it refused (with its own message), it exited
    /// (with its exit status), or it did not answer within the limit, each
    /// with the end of what it wrote to stderr.
    ///
    /// The sentence is complete on its own, because every surface that
    /// reports a server that cannot start prints it as it is.
    #[error("{0}")]
    HandshakeFailed(String),

    /// The server answered that the request does not apply where it was asked,
    /// in one of the exact method and message pairs [`declined_answer`] knows.
    #[error("{method} declined: {message}")]
    Declined { method: String, message: String },

    /// The server is still running, but its own state is broken: it fails
    /// every question the same way, whatever was asked and where, until it is
    /// restarted. The reason says how that was recognized.
    ///
    /// A server in this state answers quickly, so without this a sweep read
    /// each failure as an answer about one position. On React, tsserver threw
    /// one assertion while updating its project for 1615 files, and every one
    /// of them was recorded as settled with no answer.
    #[error("the server's own state failed and it must restart: {0}")]
    ServerWedged(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// How a request that did not produce an answer ended, in the order every
/// enrichment pass decides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryErrorClass {
    /// The server can no longer answer anything: stop asking it.
    SessionEnded,
    /// No answer arrived in time. Skip this question; several in a row mean
    /// the server has stopped answering.
    TimedOut,
    /// The server answered that the request does not apply at this position.
    /// An answer with nothing in it: skip the position and hold nothing back.
    Declined,
    /// Anything else. The question got no considered answer, so whatever
    /// asked it did not finish, and the file it belongs to is not complete.
    Failed,
}

impl LspError {
    /// Whether the server can no longer answer anything, as opposed to one
    /// request it could not answer or one answer that could not be proven.
    pub fn ends_the_session(&self) -> bool {
        matches!(
            self,
            LspError::ServerDied
                | LspError::Io(_)
                | LspError::ServerNotFound(_)
                | LspError::ServerStartFailed(_)
                | LspError::InitializeFailed(_)
                | LspError::ServerFailedWithStderr { .. }
                | LspError::HandshakeFailed(_)
                | LspError::ServerWedged(_)
        )
    }

    /// Whether the server declined this request as not applying where it was
    /// asked. Only [`JsonRpcClient::request`](crate::client::JsonRpcClient)
    /// makes one, from the pairs [`declined_answer`] knows.
    pub fn is_declined(&self) -> bool {
        matches!(self, LspError::Declined { .. })
    }

    /// Whether this failure refuses the question for good: the server
    /// answered, and the answer is one this build cannot prove or decode, so
    /// asking the same server about the same bytes returns it again.
    ///
    /// A prepared call hierarchy item that is not the queried entity, a range
    /// that names no position in the admitted text, a declaration whose name
    /// its own line does not spell, a site in a file the graph does not
    /// admit, and an answer whose shape does not decode are all refusals. The
    /// entity they are about is unprovable by this server, which settles it:
    /// retrying costs a query and learns nothing.
    ///
    /// The one error a server returns itself that is a refusal is a
    /// TypeScript compiler's internal assertion about the position asked (see
    /// [`typescript_internal_assertion`]): the compiler asserts on the same
    /// bytes the same way every time. An assertion thrown while tsserver
    /// brings its project up to date is not about the position at all, and
    /// is a server failure instead (see [`server_state_failure`]).
    ///
    /// Not a refusal: a timeout, a server that stopped answering, and any
    /// other error the server returned itself, which may be load, a restart
    /// or a request it cancelled. Those may answer if asked again, so their
    /// work stays owed. A decline is its own class and never reaches here.
    pub fn is_refusal(&self) -> bool {
        match self {
            LspError::Protocol(_) | LspError::Json(_) => true,
            LspError::JsonRpc(error) => {
                typescript_internal_assertion(error) && server_state_failure(error).is_none()
            }
            _ => false,
        }
    }

    /// Whether retrying can recover without changing the admitted source.
    /// Session loss and timeouts need another attempt. The protocol also
    /// explicitly asks for one with RequestCancelled, ContentModified and
    /// ServerCancelled. Other errors retain their existing failure policy;
    /// declines and deterministic refusals never become retry debt here.
    pub fn is_retryable(&self) -> bool {
        if self.is_declined() || self.is_refusal() {
            return false;
        }
        if self.ends_the_session() || matches!(self, Self::Timeout) {
            return true;
        }
        let Self::JsonRpc(error) = self else {
            return false;
        };
        typescript_lost_the_open_file(error)
            || serde_json::from_str::<serde_json::Value>(error)
                .ok()
                .and_then(|error| error.get("code").and_then(serde_json::Value::as_i64))
                .is_some_and(|code| matches!(code, -32802..=-32800))
    }

    /// Whether the server may answer this question if asked it with nothing
    /// else in flight: it timed out, or the server asked for it again with
    /// RequestCancelled, ContentModified or ServerCancelled. Those are what a
    /// server does when it falls behind, so a pass that asked beside other
    /// questions backs off and asks again alone before it counts one as
    /// unanswered. A server that stopped answering is not behind, and an
    /// answer this build refuses is the same answer asked alone.
    pub fn under_load(&self) -> bool {
        self.is_retryable() && !self.ends_the_session()
    }

    /// The one classification every pass applies, in its one order:
    /// session-ending, then timeout, then decline, then failure.
    pub fn class(&self) -> QueryErrorClass {
        if self.ends_the_session() {
            QueryErrorClass::SessionEnded
        } else if matches!(self, LspError::Timeout) {
            QueryErrorClass::TimedOut
        } else if self.is_declined() {
            QueryErrorClass::Declined
        } else {
            QueryErrorClass::Failed
        }
    }
}

/// Whether a JSON-RPC error is TypeScript's compiler failing one of its own
/// assertions: typescript-language-server relays it as `TypeScript Server
/// Error (<version>)` with a `Debug Failure.` line, from `Debug.fail` and
/// `Debug.assert` inside the compiler.
///
/// Such an assertion is a property of the program and the position, not of
/// load: on drizzle-orm TypeScript 5.6.3 failed 514 definition queries in
/// `getTextOfPropertyName`, the same ones in every sweep, and holding their
/// 230 files owed asked them again for nothing.
pub fn typescript_internal_assertion(error: &str) -> bool {
    let Ok(error) = serde_json::from_str::<serde_json::Value>(error) else {
        return false;
    };
    let Some(message) = error.get("message").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let mut lines = message.lines();
    lines
        .next()
        .is_some_and(|first| first.contains("TypeScript Server Error"))
        && lines.any(|line| line.trim_start().starts_with("Debug Failure."))
}

/// Whether tsserver answered that it holds no source file for the document
/// the question was about. The sweep opens every file before asking about it,
/// so this is tsserver's own state having failed, not an answer about the
/// file: on React it answered every question about one test file this way,
/// in every sweep, as its project update began asserting, and the file was
/// held owed to a later pass that answered it. Asked again once the file is
/// reopened, on a server that is answering, it is answered.
///
/// Asked again on the same server, it was not: on React 11,737 definition
/// questions about one file got this answer over 28 seconds, the file was
/// held owed, and a fresh server a later pass started answered it in ten. So
/// it is also a server failure (see [`server_state_failure`]): the server is
/// retired and the file asked again on its replacement, inside the pass.
pub fn typescript_lost_the_open_file(error: &str) -> bool {
    let Ok(error) = serde_json::from_str::<serde_json::Value>(error) else {
        return false;
    };
    let Some(message) = error.get("message").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let mut lines = message.lines();
    lines
        .next()
        .is_some_and(|first| first.contains("TypeScript Server Error"))
        && lines.any(|line| line.trim_start().starts_with("Could not find source file"))
}

/// The frames of tsserver's stack that bring a project up to date before any
/// request is answered: the program is rebuilt and the module resolution
/// cache is synchronized and watched. Every request passes through them first,
/// so an assertion raised inside them says nothing about the position asked.
const TYPESCRIPT_PROJECT_UPDATE_FRAMES: &[&str] = &[
    "updateGraph",
    "updateProjectIfDirty",
    "CachingPerDirectoryResolution",
    "FailedLookupLocation",
];

/// Why a JSON-RPC error a server returned says its own state has failed,
/// rather than that one question could not be answered, or `None`.
///
/// The one known today is a TypeScript internal assertion thrown while
/// tsserver brings its project up to date (see
/// [`TYPESCRIPT_PROJECT_UPDATE_FRAMES`]). TypeScript 5.7.3 asserted in
/// `watchFailedLookupLocationOfResolution`, under `updateGraph`, for every
/// definition query on React for two and a half minutes, until tsserver was
/// restarted. The assertion is the same whatever was asked, which is how it
/// differs from one about a position (`getTextOfPropertyName` on
/// drizzle-orm), which stays a refusal. Other servers are covered by
/// [`crate::client`]'s repeated-failure rule, which needs no recognizer.
pub fn server_state_failure(error: &str) -> Option<String> {
    if typescript_lost_the_open_file(error) {
        return Some(
            "TypeScript answered that it holds no source file for a document it was handed, so \
             its own project state has failed and it fails that document until it restarts"
                .to_string(),
        );
    }
    if !typescript_internal_assertion(error) {
        return None;
    }
    let message = serde_json::from_str::<serde_json::Value>(error)
        .ok()?
        .get("message")?
        .as_str()?
        .to_string();
    let frame = message
        .lines()
        .map(str::trim_start)
        .filter_map(|line| line.strip_prefix("at "))
        .find(|frame| {
            let function = frame.split([' ', '(']).next().unwrap_or("");
            TYPESCRIPT_PROJECT_UPDATE_FRAMES
                .iter()
                .any(|update| function.contains(update))
        })?;
    let function = frame.split([' ', '(']).next().unwrap_or(frame);
    Some(format!(
        "TypeScript asserted while updating its project, in `{function}`, so it fails every \
         question until it restarts"
    ))
}

/// How a known declining message is matched.
enum Wording {
    Exact(&'static str),
    Prefix(&'static str),
    Suffix(&'static str),
}

/// The answers a server gives when a request does not apply where it was
/// asked, as exact method and message pairs, never a bare error code.
///
/// Each was observed from gopls v0.22.0 on the GitHub CLI and is a property of
/// the source at that position, so asking again returns it again. gopls sends
/// every handler error with code 0, including "no package metadata for file"
/// for a package it could not load, so the code alone says nothing: a pair
/// that is not listed here is a failure, and the file it belongs to is retried.
const DECLINED_ANSWERS: &[(&str, Wording)] = &[
    // A keyword, a package clause or a declaration's own line.
    (
        "textDocument/typeDefinition",
        Wording::Exact("no enclosing expression has a type"),
    ),
    // A package name, a builtin, or a value of an unnamed type.
    (
        "textDocument/typeDefinition",
        Wording::Prefix("cannot find type name(s) from type "),
    ),
    // A method, field or value asked for its type hierarchy.
    (
        "textDocument/prepareTypeHierarchy",
        Wording::Exact("not a type name"),
    ),
    // A type, field, constant or variable asked for its call hierarchy.
    (
        "textDocument/prepareCallHierarchy",
        Wording::Suffix(" is not a function"),
    ),
    // A package clause asked for its call hierarchy.
    (
        "textDocument/prepareCallHierarchy",
        Wording::Exact("no symbol here"),
    ),
    (
        "textDocument/prepareCallHierarchy",
        Wording::Exact("identifier not found"),
    ),
    // A position on a keyword asked for its references.
    (
        "textDocument/references",
        Wording::Exact("no identifier found"),
    ),
];

/// The declining message, when this JSON-RPC error answering `method` is one
/// of [`DECLINED_ANSWERS`].
///
/// `error` is the JSON-RPC error object as the client received it. Code 0 is
/// required as well as the wording, because that is how gopls sends them.
pub fn declined_answer(method: &str, error: &str) -> Option<String> {
    let error: serde_json::Value = serde_json::from_str(error).ok()?;
    if error.get("code").and_then(serde_json::Value::as_i64) != Some(0) {
        return None;
    }
    let message = error.get("message").and_then(serde_json::Value::as_str)?;
    DECLINED_ANSWERS
        .iter()
        .filter(|(declining, _)| *declining == method)
        .any(|(_, wording)| match wording {
            Wording::Exact(text) => message == *text,
            Wording::Prefix(text) => message.starts_with(text),
            Wording::Suffix(text) => message.ends_with(text) && message.len() > text.len(),
        })
        .then(|| message.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{declined_answer, LspError, QueryErrorClass};

    fn rpc(code: i64, message: &str) -> String {
        serde_json::json!({"code": code, "message": message}).to_string()
    }

    /// Only the observed method and message pairs decline, and only with the
    /// code gopls sends them with.
    #[test]
    fn only_a_known_answer_to_its_own_method_is_a_decline() {
        let declined = |method: &str, message: &str| declined_answer(method, &rpc(0, message));
        assert!(declined(
            "textDocument/typeDefinition",
            "no enclosing expression has a type"
        )
        .is_some());
        assert!(declined(
            "textDocument/typeDefinition",
            "cannot find type name(s) from type invalid type"
        )
        .is_some());
        assert!(declined("textDocument/prepareTypeHierarchy", "not a type name").is_some());
        assert!(declined(
            "textDocument/prepareCallHierarchy",
            "Issue is not a function"
        )
        .is_some());
        assert!(declined("textDocument/prepareCallHierarchy", "no symbol here").is_some());
        assert!(declined("textDocument/references", "no identifier found").is_some());

        // The wording belongs to its method.
        assert!(declined("textDocument/definition", "no identifier found").is_none());
        assert!(declined("callHierarchy/outgoingCalls", "no symbol here").is_none());
        assert!(declined("typeHierarchy/supertypes", "not a type name").is_none());
        assert!(declined("textDocument/prepareCallHierarchy", " is not a function").is_none());
        // gopls cannot load the file's package: a failure, not a decline.
        assert!(declined(
            "textDocument/typeDefinition",
            "no package metadata for file file:///w/main.go"
        )
        .is_none());
        assert!(declined("textDocument/references", "no package metadata for file").is_none());
        // The wording with any other code is not gopls declining.
        assert!(declined_answer(
            "textDocument/prepareTypeHierarchy",
            &rpc(-32603, "not a type name")
        )
        .is_none());
        assert!(declined_answer(
            "textDocument/references",
            &rpc(-32803, "no identifier found")
        )
        .is_none());
        assert!(declined_answer("textDocument/references", "not json").is_none());
    }

    /// Only an answer this build cannot prove or decode refuses a question
    /// for good; everything that may answer if asked again does not.
    #[test]
    fn only_an_unprovable_answer_is_a_refusal() {
        assert!(LspError::Protocol(
            "prepared call hierarchy source is not the queried entity".into()
        )
        .is_refusal());
        let undecodable = serde_json::from_str::<Vec<u32>>("{}").unwrap_err();
        assert!(LspError::Json(undecodable).is_refusal());
        for unanswered in [
            LspError::Timeout,
            LspError::ServerDied,
            LspError::JsonRpc(rpc(-32801, "content modified")),
            LspError::Io(std::io::Error::other("broken pipe")),
            LspError::Declined {
                method: "textDocument/references".into(),
                message: "no identifier found".into(),
            },
        ] {
            assert!(!unanswered.is_refusal(), "{unanswered}");
        }
    }

    /// TypeScript answers a question its own compiler asserts on with the
    /// assertion, and asks of the same bytes assert the same way every time:
    /// on drizzle-orm TypeScript 5.6.3 failed 514 definition queries in
    /// `getTextOfPropertyName`, identically in every sweep. That is a
    /// refusal. Any other error tsserver returns, and one that only mentions
    /// the words, is not.
    #[test]
    fn a_typescript_internal_assertion_is_a_refusal() {
        let assertion = rpc(
            1,
            "<main> TypeScript Server Error (5.6.3)\nDebug Failure.\nError: Debug Failure.\n    \
             at getTextOfPropertyName (/repo/node_modules/typescript/lib/typescript.js:17277:16)",
        );
        assert!(LspError::JsonRpc(assertion).is_refusal());
        let false_expression = rpc(
            1,
            "<semantic> TypeScript Server Error (5.9.3)\nDebug Failure. False expression: \
             Expected node to be a declaration\nError: Debug Failure. False expression",
        );
        assert!(LspError::JsonRpc(false_expression).is_refusal());
        for other in [
            rpc(1, "<main> TypeScript Server Error (5.6.3)\nNo Project."),
            rpc(
                1,
                "<main> TypeScript Server Error (5.6.3)\nCould not find source file",
            ),
            rpc(0, "Debug Failure."),
            rpc(-32603, "the request mentions a Debug Failure. somewhere"),
            "not json".to_string(),
        ] {
            assert!(!LspError::JsonRpc(other.clone()).is_refusal(), "{other}");
        }
    }

    #[test]
    fn a_tsserver_that_lost_the_open_file_is_asked_again() {
        let lost = rpc(
            1,
            "<main> TypeScript Server Error (5.7.3)\nCould not find source file: \
             '/repo/src/events-test.js'.\nError: Could not find source file: \
             '/repo/src/events-test.js'.\n    at getValidSourceFile (/env/typescript.js:1:1)",
        );
        // A server failure: the client reads the server as gone, so the
        // file is asked again on a fresh one rather than on this one.
        let reason = super::server_state_failure(&lost).expect("a server failure");
        assert!(reason.contains("no source file"), "{reason}");
        let error = LspError::JsonRpc(lost);
        assert!(!error.is_refusal());
        assert!(error.is_retryable());
        assert!(error.under_load());
        assert_eq!(error.class(), QueryErrorClass::Failed);
        // Other errors tsserver returns keep their policy.
        let no_project = LspError::JsonRpc(rpc(
            1,
            "<main> TypeScript Server Error (5.6.3)\nNo Project.",
        ));
        assert!(!no_project.is_retryable());
        let elsewhere = LspError::JsonRpc(rpc(1, "Could not find source file"));
        assert!(!elsewhere.is_retryable());
        assert!(super::server_state_failure(&rpc(1, "Could not find source file")).is_none());
    }

    /// The assertion tsserver threw on React for every definition query for
    /// two and a half minutes, as typescript-language-server relayed it.
    pub(crate) fn project_update_assertion() -> String {
        let at = |function: &str| {
            format!("\n    at {function} (/env/node_modules/typescript/lib/typescript.js:1:1)")
        };
        rpc(
            1,
            &format!(
                "<main> TypeScript Server Error (5.7.3)\nDebug Failure. False expression.\n\
                 Error: Debug Failure. False expression.{}\n    at Set.forEach (<anonymous>){}{}{}{}{}{}",
                at("watchFailedLookupLocationOfResolution"),
                at("watchFailedLookupLocationOfNonRelativeModuleResolutions"),
                at("Object.startCachingPerDirectoryResolution"),
                at("InferredProject2.updateGraphWorker"),
                at("InferredProject2.updateGraph"),
                at("updateProjectIfDirty"),
                at("IpcIOSession.getDefinition"),
            ),
        )
    }

    /// An assertion thrown while tsserver brings its project up to date is
    /// about the server, not the position: it fails every question the same
    /// way until tsserver restarts, so it ends the session and is retried,
    /// and is never a refusal that settles the position. Falsify by reading
    /// every `Debug Failure.` as a refusal: on React 1615 files were settled
    /// with no answer while tsserver was in this state.
    #[test]
    fn a_typescript_assertion_while_updating_the_project_is_a_server_failure() {
        let wedged = project_update_assertion();
        let reason = super::server_state_failure(&wedged).expect("recognized");
        assert!(
            reason.contains("watchFailedLookupLocationOfResolution"),
            "{reason}"
        );
        assert!(!LspError::JsonRpc(wedged.clone()).is_refusal());
        for function in [
            "InferredProject2.updateGraph",
            "ConfiguredProject2.updateGraphWorker",
            "updateProjectIfDirty",
            "Object.finishCachingPerDirectoryResolution",
        ] {
            let error = rpc(
                1,
                &format!(
                    "<main> TypeScript Server Error (5.7.3)\nDebug Failure.\n    at {function} \
                     (/x/typescript.js:1:1)\n    at IpcIOSession.getDefinition (/x/typescript.js:2:2)"
                ),
            );
            assert!(super::server_state_failure(&error).is_some(), "{function}");
        }
        // An assertion about the position stays a refusal.
        let position = rpc(
            1,
            "<main> TypeScript Server Error (5.6.3)\nDebug Failure.\nError: Debug Failure.\n    \
             at getTextOfPropertyName (/repo/node_modules/typescript/lib/typescript.js:17277:16)\n    \
             at IpcIOSession.getDefinition (/x/typescript.js:2:2)",
        );
        assert!(super::server_state_failure(&position).is_none());
        assert!(LspError::JsonRpc(position).is_refusal());
        // Only an internal assertion is read this way, whatever else names
        // the frames.
        assert!(super::server_state_failure(&rpc(
            1,
            "<main> TypeScript Server Error (5.7.3)\nNo Project.\n    at updateGraph (x)"
        ))
        .is_none());

        let failed = LspError::ServerWedged(reason);
        assert!(failed.ends_the_session());
        assert!(failed.is_retryable());
        assert!(!failed.is_refusal());
        assert_eq!(failed.class(), QueryErrorClass::SessionEnded);
    }

    #[test]
    fn retryability_recognizes_protocol_cancellation_without_retrying_refusals() {
        for code in [-32800, -32801, -32802] {
            let error = LspError::JsonRpc(rpc(code, "ask again"));
            assert!(error.is_retryable(), "{error}");
            assert!(!error.is_refusal());
            assert_eq!(error.class(), QueryErrorClass::Failed);
        }
        for error in [
            LspError::Timeout,
            LspError::ServerDied,
            LspError::Io(std::io::Error::other("broken pipe")),
        ] {
            assert!(error.is_retryable(), "{error}");
        }
        for error in [
            LspError::Declined {
                method: "textDocument/references".into(),
                message: "no identifier found".into(),
            },
            LspError::Protocol("a declaration cannot be proved".into()),
            LspError::JsonRpc(rpc(-32603, "internal error")),
            LspError::JsonRpc(rpc(-32803, "request failed")),
            LspError::JsonRpc("not json: -32801".into()),
            LspError::JsonRpc(r#"{"code":"-32801","message":"content modified"}"#.into()),
            LspError::JsonRpc(rpc(
                -32800,
                "TypeScript Server Error (5.6.3)\nDebug Failure.",
            )),
        ] {
            assert!(!error.is_retryable(), "{error}");
        }
    }

    /// The one order: session-ending, then timeout, then decline, then failure.
    #[test]
    fn every_error_has_one_class() {
        let declined = LspError::Declined {
            method: "textDocument/references".into(),
            message: "no identifier found".into(),
        };
        assert_eq!(declined.class(), QueryErrorClass::Declined);
        assert_eq!(LspError::Timeout.class(), QueryErrorClass::TimedOut);
        assert_eq!(LspError::ServerDied.class(), QueryErrorClass::SessionEnded);
        assert_eq!(
            LspError::JsonRpc(rpc(0, "no package metadata for file")).class(),
            QueryErrorClass::Failed
        );
        assert_eq!(
            LspError::Protocol("ambiguous".into()).class(),
            QueryErrorClass::Failed
        );
    }
}
