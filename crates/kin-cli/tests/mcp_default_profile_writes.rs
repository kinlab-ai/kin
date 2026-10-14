// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The default MCP profile writes with no session tool.
//!
//! An agent on the default profile used to open a session before its first
//! write, and in one measured run spent a turn doing so for a question it only
//! read. A write that names no session now runs in the connection's own, which
//! the server opens itself, and the profile lists no `session` tool at all.
//!
//! This drives the real `kin mcp start` on its default profile, the one gates
//! and benchmarks run, against a real daemon over a real repository: the
//! listing carries no `session` and no `exec`, a call to `exec` is refused
//! without running anything, and a `mutate` that names no session commits and
//! reads back.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::tempdir;

mod common;

use common::Command;

/// How long one request may take to be answered, including the first write,
/// which opens the connection's session on the daemon.
const ANSWER_BOUND: Duration = Duration::from_secs(120);

fn run_git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", kin_git::empty_global_git_config())
        .current_dir(path)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn seed_repository(repo: &Path) {
    fs::create_dir_all(repo.join("src")).expect("create source directory");
    run_git(repo, &["init", "--initial-branch=main"]);
    run_git(repo, &["config", "user.email", "kin@example.invalid"]);
    run_git(repo, &["config", "user.name", "Kin"]);
    fs::write(
        repo.join("src/storage.rs"),
        b"pub fn open_store() -> u32 {\n    7\n}\n\npub fn close_store() -> u32 {\n    open_store() + 1\n}\n",
    )
    .expect("write entity source");
    run_git(repo, &["add", "--all"]);
    run_git(repo, &["commit", "-m", "storage"]);
}

/// A `kin mcp start` session on the default profile, driven one request at a
/// time.
struct McpSession {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl McpSession {
    fn start(repo: &Path, home: &Path, kin_home: &Path, registry: &Path, port: u16) -> Self {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_kin"))
            .args(["mcp", "start"])
            .current_dir(repo)
            .env_clear()
            // No KIN_MCP_TOOL_PROFILE: the default profile is the one under
            // test. Machine replies, so the payloads can be read.
            .env("KIN_MCP_MACHINE_REPLIES", "1")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("KIN_HOME", kin_home)
            .env("KIN_RUNTIME_MODE", "development")
            .env("TMPDIR", std::env::temp_dir())
            .env("KIN_VFS_DISABLE", "1")
            .env("KIN_REGISTRY_PATH", registry)
            .env("KIN_DAEMON_URL", format!("http://127.0.0.1:{port}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kin mcp start");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("write a request");
        self.stdin.flush().expect("flush a request");
    }

    /// Send one request and wait for the response that carries its id.
    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + ANSWER_BOUND;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(remaining).unwrap_or_else(|_| {
                panic!("{method} (id {id}) was not answered in {ANSWER_BOUND:?}")
            });
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("id") == Some(&json!(id)) {
                return message;
            }
        }
    }

    /// Call one tool and return its result.
    fn call(&mut self, id: u64, tool: &str, arguments: Value) -> Value {
        let response = self.request(
            id,
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
        );
        response
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("{tool} answered with no result: {response}"))
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The JSON a tool result's first text block carries.
fn payload(result: &Value, what: &str) -> Value {
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{what} answered with no text: {result}"));
    serde_json::from_str(text).unwrap_or_else(|_| panic!("{what} answered with no JSON: {text}"))
}

/// The id of the entity named `name` in a name search's rows.
fn entity_id_named(search: &Value, name: &str) -> Option<String> {
    ["results", "hits"]
        .iter()
        .filter_map(|key| search[*key].as_array())
        .flatten()
        .find(|row| row["name"] == name)
        .and_then(|row| row.get("id").or_else(|| row.get("entity_id")))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn wait_until_serving(kin_root: &Path, daemon: &mut common::RuntimeOwnedChild) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(status) = daemon.try_wait().expect("inspect the daemon") {
            panic!("the daemon exited before it served: {status}");
        }
        if let Some(port) = fs::read_to_string(kin_root.join("daemon.port"))
            .ok()
            .and_then(|value| value.trim().parse::<u16>().ok())
        {
            let address = SocketAddr::from(([127, 0, 0, 1], port));
            if TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_ok() {
                return port;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the daemon did not start serving"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_default_profile_lists_no_session_and_a_write_needs_none() {
    let root = tempdir().expect("temp root");
    let home = root.path().join("home");
    let repo = root.path().join("repo");
    fs::create_dir_all(&home).expect("create home");
    fs::create_dir_all(&repo).expect("create repo");
    let repo = repo.canonicalize().expect("resolve the repository path");
    let home = home.canonicalize().expect("resolve the home path");
    seed_repository(&repo);

    let runtime = common::IsolatedDaemonRuntime::new(&repo);
    let kin_home = std::path::PathBuf::from(
        runtime
            .kin_command()
            .configured_env_for_test(std::ffi::OsStr::new("KIN_HOME"))
            .flatten()
            .expect("owned Kin home"),
    );
    let init = runtime
        .kin_command()
        .args(["init", ".", "--json"])
        .current_dir(&repo)
        .output()
        .expect("run kin init");
    assert!(
        init.status.success(),
        "kin init failed: stdout={} stderr={}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );

    let mut daemon = runtime
        .daemon_command()
        .arg("--repo")
        .arg(&repo)
        .arg("--port")
        .arg("0")
        .env("KIN_DAEMON_DISABLE_LSP", "1")
        .env("KIN_DAEMON_IDLE_TIMEOUT_SECS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn_owned()
        .expect("spawn the daemon");
    let port = wait_until_serving(runtime.layout().root(), &mut daemon);

    let mut mcp = McpSession::start(&repo, &home, &kin_home, runtime.registry_path(), port);
    let initialized = mcp.request(
        1,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {},
               "clientInfo": {"name": "kin-default-profile-test", "version": "0"}}),
    );
    assert_eq!(
        initialized["result"]["protocolVersion"], "2025-06-18",
        "{initialized}"
    );
    mcp.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    let listed = mcp.request(2, "tools/list", json!({}));
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("a tool list")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"session"),
        "the default profile lists a session tool: {names:?}"
    );
    for served in ["mutate", "transaction", "locate", "source"] {
        assert!(
            names.contains(&served),
            "the default profile lost {served}: {names:?}"
        );
    }
    // Running a program and setting a folder up are not semantic agent work,
    // so the default profile lists neither, and a call to the retired run is
    // refused by either name before its program runs.
    for retired in ["exec", "kin_session_exec", "init", "kin_init"] {
        assert!(
            !names.contains(&retired),
            "the default profile lists {retired}: {names:?}"
        );
    }
    let marker = repo.join("exec-ran");
    for (id, tool) in [(10, "exec"), (11, "kin_session_exec")] {
        let response = mcp.request(
            id,
            "tools/call",
            json!({"name": tool, "arguments": {
                "argv": ["touch", marker.to_str().expect("a UTF-8 marker path")]
            }}),
        );
        assert!(
            response.get("error").is_some() || response["result"]["isError"] == json!(true),
            "the default profile ran {tool}: {response}"
        );
    }
    assert!(!marker.exists(), "a refused exec call ran its program");

    let search = mcp.call(3, "locate", json!({"query": "open_store", "mode": "name"}));
    let focal = entity_id_named(&payload(&search, "locate"), "open_store")
        .unwrap_or_else(|| panic!("locate found no open_store: {search}"));
    let source = payload(
        &mcp.call(4, "source", json!({"entity_id": focal})),
        "source",
    );
    let base = source
        .get("source_base")
        .cloned()
        .unwrap_or_else(|| panic!("source served no source_base: {source}"));

    // No session was opened, named or declared by this client.
    let mutated = mcp.call(
        5,
        "mutate",
        json!({
            "operations": [{
                "verb": "patch",
                "target": focal,
                "description": "raise the store handle",
                "payload": {"EntitySourcePatch": {
                    "source_base": base,
                    "edits": [{"old_text": "    7", "new_text": "    8"}]
                }}
            }],
            "summary": "raise the store handle"
        }),
    );
    assert_ne!(
        mutated["isError"],
        json!(true),
        "a mutate that named no session was refused: {mutated}"
    );
    let receipt = payload(&mutated, "mutate");
    assert_eq!(receipt["status"], "committed", "{receipt}");
    assert_eq!(
        mutated.get("structuredContent"),
        Some(&receipt),
        "a client that negotiated structured output gets the reply restated"
    );

    let after = payload(
        &mcp.call(6, "source", json!({"entity_id": focal})),
        "source",
    );
    assert!(
        after["body"]
            .as_str()
            .is_some_and(|body| body.contains("    8")),
        "the committed body did not read back: {after}"
    );
    let on_disk = fs::read_to_string(repo.join("src/storage.rs")).expect("read the source");
    assert!(
        on_disk.contains("    8"),
        "the working copy was not written: {on_disk}"
    );

    drop(mcp);
    let _ = daemon.kill();
    let _ = daemon.wait();
}
