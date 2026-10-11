// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Why a language server did not start, in the sentence every surface prints.
//!
//! A server that will not start is reported by its reason or not at all:
//! "would not start" with no reason sends a reader to a second command to
//! learn what this process already knew. The reason is assembled here from
//! what the launch observed (how the handshake ended, how the process exited,
//! what it reported about its backend, the end of its stderr, and what its
//! adapter found missing) and from where a missing program was looked for.
//!
//! This file only phrases. It decides nothing an answer proves, so it sits
//! outside the enrichment producer identity, and rewording a reason re-asks
//! no stored proof.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::adapters::ServerLaunch;
use crate::error::LspError;
use crate::lifecycle::last_bytes;

/// How much of a server's stderr the reason for a failed start carries: its
/// last bytes, from the start of a line, which are the ones that say why.
pub const START_FAILURE_STDERR: usize = 1024;

/// How a server that started did not complete the initialize handshake.
#[derive(Debug)]
pub(crate) enum HandshakeFailure {
    /// It did not answer within the limit.
    TimedOut(Duration),
    /// It answered with an error. Carries the error's own message.
    Refused(String),
    /// Its connection ended before it answered: it exited, or it said the
    /// backend it fronts did.
    Ended,
    /// Anything else, as the client reported it.
    Other(String),
}

/// The last `max` bytes of a server's stderr, moved forward to the start of a
/// line when that leaves something, and marked when cut.
pub fn stderr_tail_for_reason(stderr: &str, max: usize) -> String {
    let stderr = stderr.trim();
    if stderr.len() <= max {
        return stderr.to_string();
    }
    let tail = last_bytes(stderr, max);
    let tail = match tail.find('\n') {
        Some(newline) if newline + 1 < tail.len() => &tail[newline + 1..],
        _ => tail,
    };
    format!("...{}", tail.trim_start())
}

/// A duration as a reason states it: whole seconds when it is whole seconds.
pub(crate) fn describe_limit(limit: Duration) -> String {
    if limit.subsec_millis() == 0 && limit.as_secs() > 0 {
        format!("{}s", limit.as_secs())
    } else {
        format!("{}ms", limit.as_millis())
    }
}

/// The message a JSON-RPC error response carries, from the error object the
/// client keeps as text. The raw text when it is not one.
pub(crate) fn rpc_error_message(error: &str) -> String {
    serde_json::from_str::<serde_json::Value>(error)
        .ok()
        .and_then(|error| {
            error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(|message| message.trim().to_string())
        })
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| error.trim().to_string())
}

/// The reason a launch that started a process failed, in one sentence a
/// reader can act on: what happened at the handshake, how the process ended,
/// what it reported about its backend, the end of its stderr, and what its
/// adapter found missing.
///
/// `exit` is how the process ended, as the clause a departure carries ("it
/// exited with code 3"), or `None` when it had not exited.
///
/// Every part is read where the process, its stderr and its notifications
/// are all still held. A caller that only receives the error cannot recover
/// any of them, and "server shutdown unexpectedly" was all a reader got of a
/// server that exited during initialize. The sentence never holds the `"; "`
/// that divides verdict clauses, beyond what the server itself wrote.
pub(crate) fn start_failure_reason(
    command: &str,
    failure: &HandshakeFailure,
    exit: Option<&str>,
    backend_exit: Option<&str>,
    stderr: &str,
    toolchain_gap: Option<&str>,
) -> String {
    let ended = matches!(failure, HandshakeFailure::Ended);
    let mut parts: Vec<String> = vec![match failure {
        HandshakeFailure::TimedOut(limit) => format!(
            "`{command}` did not answer initialize within {}",
            describe_limit(*limit)
        ),
        HandshakeFailure::Refused(message) => format!("`{command}` refused initialize: {message}"),
        HandshakeFailure::Ended => match (exit, backend_exit) {
            (Some(exit), _) => format!("`{command}` did not answer initialize: {exit}"),
            (None, Some(report)) => format!(
                "`{command}` did not answer initialize: it reported that its backend exited: \
                 {report}"
            ),
            (None, None) => format!(
                "`{command}` did not answer initialize: it closed its connection, and its \
                 process had not exited"
            ),
        },
        HandshakeFailure::Other(error) => {
            format!("`{command}` did not complete the initialize handshake: {error}")
        }
    }];
    if let (Some(exit), false) = (exit, ended) {
        parts.push(format!("Then {exit}"));
    }
    if let (Some(report), true) = (backend_exit, exit.is_some() || !ended) {
        parts.push(format!("It reported that its backend exited: {report}"));
    }
    if stderr.is_empty() {
        if !matches!(failure, HandshakeFailure::Refused(_)) {
            let last = parts.last_mut().expect("a reason has a first part");
            *last = format!(
                "{}, and it wrote nothing to stderr",
                last.trim_end_matches('.')
            );
        }
    } else {
        parts.push(format!(
            "Its last stderr: {}",
            stderr_tail_for_reason(stderr, START_FAILURE_STDERR)
        ));
    }
    if let Some(gap) = toolchain_gap {
        parts.push(gap.to_string());
    }
    parts
        .iter()
        .map(|part| part.trim().trim_end_matches('.'))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(". ")
}

/// The reason a server could not be spawned at all. A program that is not
/// found names where it was looked for.
pub(crate) fn spawn_failure(
    command: &str,
    launch: &ServerLaunch,
    error: &std::io::Error,
) -> LspError {
    if error.kind() != std::io::ErrorKind::NotFound {
        return LspError::ServerStartFailed(format!("`{command}`: {error}"));
    }
    if Path::new(command).components().count() > 1 {
        return LspError::ServerNotFound(format!("`{command}` does not exist ({error})"));
    }
    let path = launch
        .env
        .iter()
        .rev()
        .find(|(name, _)| name == "PATH")
        .map(|(_, value)| std::ffi::OsString::from(value))
        .or_else(|| std::env::var_os("PATH"));
    let home = std::env::var_os("HOME").map(PathBuf::from);
    LspError::ServerNotFound(format!(
        "no `{command}` on PATH (looked in {})",
        describe_search_path(path.as_deref(), home.as_deref())
    ))
}

/// How many directories a reason names from the front of a long `PATH`.
const SEARCHED_FIRST_NAMED: usize = 8;

/// How many directories a reason names from the end of a long `PATH`. Kin
/// appends its own tool directories and the usual per-user install places
/// there, so the end is where a reader checks that they were searched.
const SEARCHED_LAST_NAMED: usize = 3;

/// The directories a `PATH` lookup searched, as a reason names them: in
/// order, each once, the home directory written `~`, and for a long `PATH`
/// the first [`SEARCHED_FIRST_NAMED`] and last [`SEARCHED_LAST_NAMED`] named
/// with the rest counted between them.
///
/// A server that is "not found" is not found somewhere. Naming where is what
/// tells a reader whether the daemon looked where they installed it, which a
/// daemon started from a GUI or a service manager often does not.
pub fn describe_search_path(path: Option<&OsStr>, home: Option<&Path>) -> String {
    let mut directories: Vec<String> = Vec::new();
    for directory in path.map(std::env::split_paths).into_iter().flatten() {
        if directory.as_os_str().is_empty() {
            continue;
        }
        let shown = match home.and_then(|home| directory.strip_prefix(home).ok()) {
            Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Some(rest) => format!("~/{}", rest.display()),
            None => directory.display().to_string(),
        };
        if !directories.contains(&shown) {
            directories.push(shown);
        }
    }
    if directories.is_empty() {
        return "an empty PATH".to_string();
    }
    let named = SEARCHED_FIRST_NAMED + SEARCHED_LAST_NAMED;
    if directories.len() <= named + 1 {
        return directories.join(", ");
    }
    let rest = directories.len() - named;
    let last = directories.split_off(directories.len() - SEARCHED_LAST_NAMED);
    directories.truncate(SEARCHED_FIRST_NAMED);
    format!(
        "{}, {rest} more, {}",
        directories.join(", "),
        last.join(", ")
    )
}

/// [`describe_search_path`] for this process's own `PATH` and home.
pub fn searched_on_this_path() -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    describe_search_path(std::env::var_os("PATH").as_deref(), home.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_path_is_named_in_order_once_with_home_shortened_and_the_rest_counted() {
        let home = Path::new("/home/u");
        let path = std::env::join_paths([
            "/opt/homebrew/bin",
            "/home/u/.cargo/bin",
            "/opt/homebrew/bin",
            "",
            "/home/u",
            "/usr/bin",
        ])
        .unwrap();
        assert_eq!(
            describe_search_path(Some(&path), Some(home)),
            "/opt/homebrew/bin, ~/.cargo/bin, ~, /usr/bin"
        );
        let long = std::env::join_paths((0..14).map(|index| format!("/d{index}"))).unwrap();
        assert_eq!(
            describe_search_path(Some(&long), None),
            "/d0, /d1, /d2, /d3, /d4, /d5, /d6, /d7, 3 more, /d11, /d12, /d13",
            "a long PATH keeps its end, where Kin's own tool directories are"
        );
        let twelve = std::env::join_paths((0..12).map(|index| format!("/d{index}"))).unwrap();
        assert!(describe_search_path(Some(&twelve), None).ends_with("/d10, /d11"));
        assert_eq!(describe_search_path(None, None), "an empty PATH");
    }

    #[test]
    fn a_long_stderr_keeps_its_last_lines_from_a_line_boundary() {
        let stderr = (0..200)
            .map(|line| format!("line {line} of the server's output"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = stderr_tail_for_reason(&stderr, START_FAILURE_STDERR);
        assert!(tail.starts_with("...line "), "{tail}");
        assert!(tail.ends_with("line 199 of the server's output"), "{tail}");
        assert!(tail.len() <= START_FAILURE_STDERR + 3, "{}", tail.len());
        assert_eq!(stderr_tail_for_reason("short", 1024), "short");
    }
}
