// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! `kin ask` through the shipped binaries, against a live daemon.
//!
//! The front door composes its reply on the client side of the daemon, from
//! verbs it sends the daemon one call at a time. This runs that route as a
//! person does: `kin init` on a real Git repository, a daemon serving it, and
//! `kin ask` with a question in plain words.

use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use tempfile::tempdir;

mod common;

struct IsolatedDaemon {
    child: Option<common::RuntimeOwnedChild>,
}

impl IsolatedDaemon {
    fn spawn(repo: &Path, runtime: &common::IsolatedDaemonRuntime) -> Self {
        let mut command = runtime.daemon_command();
        let child = command
            .arg("--repo")
            .arg(repo)
            .arg("--port")
            .arg("0")
            .env("KIN_DAEMON_DISABLE_LSP", "1")
            .env("KIN_DAEMON_IDLE_TIMEOUT_SECS", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn_owned()
            .expect("spawn isolated kin-daemon");
        Self { child: Some(child) }
    }

    fn wait_until_serving(&mut self, kin_root: &Path) -> u16 {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let child = self.child.as_mut().expect("daemon child exists");
            if let Some(status) = child.try_wait().expect("inspect daemon child") {
                panic!("isolated daemon exited before readiness: {status}");
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
                "isolated daemon did not become ready"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for IsolatedDaemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn run_git(repo: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", kin_git::empty_global_git_config())
        .current_dir(repo)
        .output()
        .expect("run git");
    assert!(output.status.success(), "git {args:?} failed");
}

fn seed(repo: &Path) {
    let write = |path: &str, content: &str| {
        let path = repo.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    };
    run_git(repo, &["init", "--initial-branch=main"]);
    run_git(repo, &["config", "user.email", "kin@example.invalid"]);
    run_git(repo, &["config", "user.name", "Kin"]);
    write("shop/__init__.py", "");
    write(
        "shop/helpers.py",
        "def render(request):\n    return str(request)\n",
    );
    write(
        "shop/app.py",
        "from shop.helpers import render\n\n\n\
         def dispatch(request):\n    return render(request)\n\n\n\
         def handle(request):\n    return dispatch(request)\n",
    );
    write(
        "tests/test_app.py",
        "from shop.app import handle\n\n\n\
         def test_handle():\n    assert handle(\"x\")\n",
    );
    run_git(repo, &["add", "--all"]);
    run_git(repo, &["commit", "-m", "a small shop"]);
}

#[test]
fn kin_ask_answers_from_a_live_daemon_as_an_mcp_client_is_answered() {
    let root = tempdir().expect("temp root");
    let repo = root.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo");
    seed(&repo);

    let runtime = common::IsolatedDaemonRuntime::new(&repo);
    let init = runtime
        .kin_command()
        .args(["init", ".", "--json"])
        .current_dir(&repo)
        .output()
        .expect("run kin init");
    assert!(
        init.status.success(),
        "init stdout={} stderr={}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );
    let layout = runtime.layout_for(&repo);
    let mut daemon = IsolatedDaemon::spawn(&repo, &runtime);
    let port = daemon.wait_until_serving(layout.root());
    let url = format!("http://127.0.0.1:{port}");

    let asked = runtime
        .kin_command()
        .args(["ask", "What calls `dispatch`? Skip tests."])
        .env("KIN_DAEMON_URL", &url)
        .current_dir(&repo)
        .output()
        .expect("run kin ask");
    let stdout = String::from_utf8_lossy(&asked.stdout).to_string();
    assert!(
        asked.status.success(),
        "ask stdout={stdout} stderr={}",
        String::from_utf8_lossy(&asked.stderr)
    );
    assert!(
        stdout.starts_with(
            "Kin ran: direct callers of shop.app :: dispatch, test code left out \
             [read as \"who calls it\" from \"calls\"]."
        ),
        "{stdout}"
    );
    assert!(stdout.contains("shop.app :: handle"), "{stdout}");
    assert!(!stdout.contains("test_handle"), "{stdout}");

    let explained = runtime
        .kin_command()
        .args(["ask", "--json", "--tests", "What", "calls", "`handle`?"])
        .env("KIN_DAEMON_URL", &url)
        .current_dir(&repo)
        .output()
        .expect("run kin ask --json");
    let stdout = String::from_utf8_lossy(&explained.stdout).to_string();
    assert!(explained.status.success(), "{stdout}");
    let printed: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON object");
    assert!(
        printed["answer"]
            .as_str()
            .is_some_and(|answer| answer.contains("tests.test_app :: test_handle")),
        "{printed}"
    );
    assert_eq!(printed["plan"]["intent"], "callers_direct", "{printed}");
    assert_eq!(printed["plan"]["include_tests"], true, "{printed}");
    assert!(printed["verdict"].is_string(), "{printed}");

    // A client's prompt hook: the prompt arrives on stdin, and what Kin
    // prints is the context the client adds before the model's first turn.
    let hooked = |client: &str, prompt: &str| {
        let input = root.path().join(format!("hook-{client}.json"));
        fs::write(
            &input,
            serde_json::json!({"session_id": "s", "cwd": repo, "hook_event_name": "UserPromptSubmit",
                "prompt": prompt})
            .to_string(),
        )
        .unwrap();
        // `output` runs with stdin closed, and a hook reads its input there.
        let stdout = root.path().join(format!("hook-{client}.out"));
        let stderr = root.path().join(format!("hook-{client}.err"));
        let status = runtime
            .kin_command()
            .args(["ask", "--hook", client])
            .env("KIN_DAEMON_URL", &url)
            .current_dir(root.path())
            .stdin(fs::File::open(&input).unwrap())
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn_owned()
            .expect("run kin ask --hook")
            .wait()
            .expect("wait for kin ask --hook");
        let stderr = fs::read_to_string(&stderr).unwrap_or_default();
        assert!(status.success(), "{status}: {stderr}");
        eprintln!("hook {client} stderr: {stderr}");
        fs::read_to_string(&stdout).unwrap().trim().to_string()
    };
    let printed: serde_json::Value =
        serde_json::from_str(&hooked("claude", "What calls `dispatch`?")).expect("hook JSON");
    assert_eq!(
        printed["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    let context = printed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        context.starts_with("Kin ran this question against the repository graph"),
        "{context}"
    );
    assert!(
        context.contains("Kin ran: direct callers of shop.app :: dispatch"),
        "{context}"
    );
    assert!(context.chars().count() < 10_000, "{}", context.len());
    let printed: serde_json::Value =
        serde_json::from_str(&hooked("gemini", "What calls `dispatch`?")).expect("hook JSON");
    assert_eq!(
        printed["hookSpecificOutput"]["hookEventName"],
        "BeforeAgent"
    );
    // A prompt that asks nothing Kin routes adds nothing.
    assert_eq!(hooked("claude", "thanks, that looks right"), "");
}
