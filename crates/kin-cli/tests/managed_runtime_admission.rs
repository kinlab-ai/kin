// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! CLI commands that build their own daemon requests still carry the runtime
//! admission a release daemon requires.
//!
//! A managed daemon refuses a local request that lacks the caller's runtime
//! identity and the repository it means, before any handler runs. A
//! development daemon checks those claims when a request carries them but does
//! not demand them, so a command that sends a request on an HTTP client of its
//! own, without the claims, passed every development run and was refused on the
//! first release build: `kin commit` answered `HTTP 409 Conflict: managed
//! semantic requests require the exact runtime identity header`.
//!
//! `KIN_DAEMON_REQUIRE_RUNTIME_IDENTITY` holds a development daemon to the
//! managed contract. Under it, a bare request is the positive control that the
//! daemon really refuses, and the commands that build their own requests are
//! what must still be answered.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use tempfile::tempdir;

mod common;

use common::Command;

const STRICT_ADMISSION: &str = "KIN_DAEMON_REQUIRE_RUNTIME_IDENTITY";

fn run_git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", kin_git::empty_global_git_config())
        .current_dir(repo)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_kin(
    runtime: &common::IsolatedDaemonRuntime,
    repo: &Path,
    args: &[&str],
) -> std::process::Output {
    runtime
        .kin_command()
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", kin_git::empty_global_git_config())
        .env("KIN_DAEMON_BIN", runtime.daemon_bin())
        .env("KIN_DAEMON_DISABLE_LSP", "1")
        .env(STRICT_ADMISSION, "1")
        .current_dir(repo)
        .output()
        .expect("run kin")
}

fn require_kin(
    runtime: &common::IsolatedDaemonRuntime,
    repo: &Path,
    args: &[&str],
) -> std::process::Output {
    let output = run_kin(runtime, repo, args);
    assert!(
        output.status.success(),
        "kin {args:?} failed under strict runtime admission: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// One HTTP/1.1 request to the daemon with its bearer token and nothing else:
/// no runtime identity and no repository claim. Answers the status line.
fn bare_request(port: u16, token: &str, path: &str) -> String {
    let body = br#"{"json":true}"#;
    let mut stream =
        std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect to the daemon");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .expect("bound the read");
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).expect("send the head");
    stream.write_all(body).expect("send the body");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

#[test]
fn commands_with_their_own_requests_are_admitted_by_a_daemon_that_requires_runtime_identity() {
    let root = tempdir().expect("temp root");
    let repo = root.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("create source directory");
    run_git(&repo, &["init", "--initial-branch=main"]);
    run_git(&repo, &["config", "commit.gpgsign", "false"]);
    run_git(&repo, &["config", "user.name", "Ada Lovelace"]);
    run_git(&repo, &["config", "user.email", "ada@example.com"]);
    fs::write(repo.join("src/lib.rs"), b"pub fn shipped() -> u8 { 1 }\n").expect("write source");
    run_git(&repo, &["add", "--all"]);
    run_git(&repo, &["commit", "-m", "first commit"]);

    let runtime = common::IsolatedDaemonRuntime::new(&repo);
    require_kin(&runtime, &repo, &["init", ".", "--json"]);
    // `kin doctor --drift` reaches the daemon through the shared client and
    // starts one under strict admission when none is serving.
    require_kin(&runtime, &repo, &["doctor", "--drift", "--json"]);

    // The positive control: this daemon really refuses a request that carries
    // its token and none of the runtime claims. Without it, every assertion
    // below would also pass against a daemon that checks nothing.
    let layout = runtime.layout_for(&repo);
    let port: u16 = fs::read_to_string(layout.root().join("daemon.port"))
        .expect("a daemon serves the repository")
        .trim()
        .parse()
        .expect("the daemon records its port");
    let token = fs::read_to_string(layout.root().join("daemon.token"))
        .expect("the daemon records its token")
        .trim()
        .to_string();
    let refused = bare_request(port, &token, "/commands/drift");
    assert!(
        refused.starts_with("HTTP/1.1 409"),
        "a request without runtime claims must be refused before its handler: {refused}"
    );
    assert!(
        refused.contains("runtime identity header"),
        "the refusal must be the runtime admission one: {refused}"
    );

    // `kin commit` sends its request on a client of its own, for a reply with
    // no deadline. It was the command a release build refused.
    fs::write(repo.join("src/lib.rs"), b"pub fn shipped() -> u8 { 2 }\n").expect("edit source");
    let committed = require_kin(
        &runtime,
        &repo,
        &["commit", "-m", "an edit under strict runtime admission"],
    );
    let said = String::from_utf8_lossy(&committed.stdout);
    assert!(
        !said.trim().is_empty(),
        "the commit must report the change it landed"
    );
    let log = require_kin(&runtime, &repo, &["log", "--json", "--count", "5"]);
    let log: serde_json::Value = serde_json::from_slice(&log.stdout).expect("kin log emits JSON");
    assert!(
        log["entries"]
            .as_array()
            .expect("log reports its entries")
            .iter()
            .any(|entry| entry
                .to_string()
                .contains("an edit under strict runtime admission")),
        "the commit must have landed: {log}"
    );

    // The intent commands built bare requests too.
    require_kin(&runtime, &repo, &["intent", "list"]);
}
