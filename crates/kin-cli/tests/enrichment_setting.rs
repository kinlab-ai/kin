// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A repository's language-server enrichment setting, through the shipped
//! binaries.
//!
//! `kin init --no-enrich` records enrichment off for the repository, on a new
//! one and on one that already exists, and every daemon that serves it then
//! starts without a sweep. `kin init --defer-enrich` skips only its own sweep
//! and leaves enrichment on, so the next daemon runs it. `kin daemon sweep`
//! refuses a switched-off repository and names `kin daemon sweep --enable`,
//! which records it on, restarts the daemon that started switched off, and
//! sweeps. When something the setting does not reach keeps every daemon off,
//! `--enable` names that instead of asking to be run again.
//!
//! A fake `gopls` ahead of the host's on `PATH` answers every question with
//! nothing, so a sweep that runs finishes over a two-unit Go module without
//! depending on what this host has installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

use serde_json::Value;

mod common;

use common::{Command, IsolatedDaemonRuntime};

/// Enrichment on over the harness default, which leaves it off. The servers a
/// daemon meets are the fake this fixture puts first on `PATH`.
const ENRICHMENT_ON: (&str, &str) = ("KIN_DAEMON_DISABLE_LSP", "0");

/// The switch that keeps a daemon's checkout from changing its graph, and with
/// it any language-server enrichment.
const FILESYSTEM_RECONCILE_OFF: (&str, &str) = ("KIN_DAEMON_DISABLE_FILESYSTEM_RECONCILE", "1");

/// What a daemon logs when the repository's own setting switches it off.
const STARTED_SWITCHED_OFF: &str =
    "language-server enrichment is off for this repository by its own setting, so this daemon \
     queues no sweep";

/// What a daemon logs when a sweep starts.
const SWEEP_STARTED: &str = "LSP cold sweep started";

/// How long a daemon is given to start a sweep it queued. The control below
/// observes one inside it, so its absence elsewhere is evidence.
const SWEEP_START_BUDGET: Duration = Duration::from_secs(20);

const FAKE_GOPLS: &str = r#"#!/usr/bin/env python3
import json, sys

if any(arg in ("version", "--version", "-version") for arg in sys.argv[1:]):
    print("golang.org/x/tools/gopls (kin test fixture)")
    sys.exit(0)

def send(message):
    body = json.dumps(message).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()

while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b"\r\n", b"\n"):
            break
        name, value = line.decode().split(":", 1)
        headers[name.strip().lower()] = value.strip()
    message = json.loads(sys.stdin.buffer.read(int(headers["content-length"])))
    method = message.get("method", "")
    if "id" not in message:
        if method == "exit":
            sys.exit(0)
        continue
    if method == "initialize":
        result = {"capabilities": {"definitionProvider": True, "referencesProvider": True}}
    else:
        result = None
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})
"#;

/// A two-unit Go module whose second function calls the first, the fake
/// `gopls`, and an isolated runtime that stops every daemon the test starts.
///
/// The runtime is declared first so it drops first: its daemons stop before
/// the directories they serve are removed.
struct Fixture {
    runtime: IsolatedDaemonRuntime,
    repo: PathBuf,
    servers: tempfile::TempDir,
    _root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temp root");
        let repo = root.path().join("repo");
        fs::create_dir_all(&repo).expect("create repo");
        git(&repo, &["init", "-q", "--initial-branch=main"]);
        git(&repo, &["config", "user.email", "kin@example.invalid"]);
        git(&repo, &["config", "user.name", "Kin"]);
        for (name, body) in [
            ("go.mod", "module example.com/fixture\n\ngo 1.22\n"),
            (
                "greet.go",
                "package fixture\n\nfunc Greet() string {\n\treturn \"hi\"\n}\n",
            ),
            (
                "use.go",
                "package fixture\n\nfunc Use() string {\n\treturn Greet()\n}\n",
            ),
        ] {
            fs::write(repo.join(name), body).expect("write fixture source");
        }
        git(&repo, &["add", "--all"]);
        git(&repo, &["commit", "-q", "-m", "Add the fixture"]);

        let servers = tempfile::tempdir().expect("fake server directory");
        let gopls = servers.path().join("gopls");
        fs::write(&gopls, FAKE_GOPLS).expect("write the fake gopls");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&gopls, fs::Permissions::from_mode(0o755))
                .expect("make the fake gopls executable");
        }
        let runtime = IsolatedDaemonRuntime::new(&repo);
        Self {
            runtime,
            repo,
            servers,
            _root: root,
        }
    }

    /// `kin <args>` in the repository, with enrichment on and the fake server
    /// first on `PATH`, plus `extra`.
    fn kin(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        let mut command = self.runtime.kin_command();
        command
            .fixture_path_prefix(self.servers.path())
            .args(args)
            .current_dir(&self.repo)
            .env("KIN_STORE_LAYOUT", "in-tree")
            .env("KIN_EMBED_BACKEND", "cpu")
            .env("KIN_DAEMON_AUTO_EMBED", "0")
            .env("KIN_DAEMON_BIN", self.runtime.daemon_bin())
            // Pinned, so this host's free memory cannot refuse a sweep a test
            // asserts runs.
            .env("KIN_MEMORY_PRESSURE", "nominal")
            .env(ENRICHMENT_ON.0, ENRICHMENT_ON.1);
        for (key, value) in extra {
            command.env(key, value);
        }
        command.output().expect("run kin")
    }

    /// `kin init --json <flag>`, which must succeed, and its
    /// `cross_file_enrichment` object.
    fn init_json(&self, flag: &str) -> Value {
        let output = self.kin(&["init", "--json", flag], &[]);
        let payload: Value = serde_json::from_slice(succeeded(&output, "kin init"))
            .expect("kin init --json prints one JSON document");
        payload["cross_file_enrichment"].clone()
    }

    fn layout(&self) -> kin_core::KinLayout {
        kin_core::KinLayout::new(self.repo.join(".kin"))
    }

    fn repository_enabled(&self) -> bool {
        kin_core::enrichment_setting::repository_enabled(&self.layout())
    }

    /// The process the store records as serving it, if any.
    fn daemon_pid(&self) -> Option<u32> {
        fs::read_to_string(self.layout().root().join("daemon.pid"))
            .ok()
            .and_then(|pid| pid.trim().parse().ok())
    }

    /// The daemon's log, without terminal colour.
    fn daemon_log(&self) -> String {
        let raw = fs::read_to_string(self.layout().root().join("daemon.log")).unwrap_or_default();
        let mut plain = String::with_capacity(raw.len());
        let mut chars = raw.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                if chars.next() == Some('[') {
                    for end in chars.by_ref() {
                        if ('@'..='~').contains(&end) {
                            break;
                        }
                    }
                }
            } else {
                plain.push(c);
            }
        }
        plain
    }

    /// Whether the daemon's log says `needle` within `budget`.
    fn log_says_within(&self, needle: &str, budget: Duration) -> bool {
        let started = Instant::now();
        loop {
            if self.daemon_log().contains(needle) {
                return true;
            }
            if started.elapsed() >= budget {
                return false;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Start a daemon for the repository the way the first query does.
    fn start_daemon(&self) -> u32 {
        let output = self.kin(&["refs", "Greet"], &[]);
        self.daemon_pid().unwrap_or_else(|| {
            panic!(
                "the first query starts a daemon that the store records: status={:?} \
                 stdout={} stderr={}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn succeeded<'o>(output: &'o Output, what: &str) -> &'o [u8] {
    assert!(
        output.status.success(),
        "{what} failed: status={:?} stdout={} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    &output.stdout
}

fn refused(output: &Output, what: &str) -> String {
    assert!(
        !output.status.success(),
        "{what} must refuse: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // The refusal itself, after the notes a command prints on the way, such as
    // the start of the daemon it asked.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let refusal = stderr
        .rsplit_once("Error:")
        .map_or(stderr.as_ref(), |(_, refusal)| refusal);
    refusal.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The control for every negative below: `--defer-enrich` skips only this
/// command's sweep. The JSON says the edges are owed to the next daemon, the
/// repository stays on, and the first daemon that starts runs the sweep.
#[test]
fn defer_enrich_skips_only_this_commands_sweep() {
    let fixture = Fixture::new();
    let field = fixture.init_json("--defer-enrich");
    assert_eq!(field["state"], "owed", "{field}");
    assert_eq!(field["reason"], "not_requested", "{field}");
    assert!(
        fixture.repository_enabled(),
        "--defer-enrich leaves enrichment on for the repository"
    );
    assert!(
        fixture.daemon_pid().is_none(),
        "init with its sweep skipped starts no daemon"
    );

    fixture.start_daemon();
    assert!(
        fixture.log_says_within(SWEEP_STARTED, SWEEP_START_BUDGET),
        "the next daemon runs the sweep this command skipped; daemon.log:\n{}",
        fixture.daemon_log()
    );
    assert!(!fixture.daemon_log().contains(STARTED_SWITCHED_OFF));
}

/// `--no-enrich` on a new repository: the JSON says enrichment is off for it,
/// not owed, and names the way on; `kin daemon sweep` refuses before starting
/// any daemon and names `--enable`.
#[test]
fn a_switched_off_repository_reports_off_and_plain_sweep_refuses() {
    let fixture = Fixture::new();
    let field = fixture.init_json("--no-enrich");
    assert_eq!(field["state"], "off", "{field}");
    assert_eq!(field["reason"], "switched_off", "{field}");
    assert!(
        field["detail"].as_str().is_some_and(
            |detail| detail.contains("kin daemon sweep --enable") && !detail.contains("file")
        ),
        "{field}"
    );
    assert!(!fixture.repository_enabled());

    let refusal = refused(&fixture.kin(&["daemon", "sweep"], &[]), "kin daemon sweep");
    assert!(
        refusal.contains("off for this repository")
            && refusal.contains("`kin daemon sweep --enable` turns it on and sweeps"),
        "{refusal}"
    );
    assert!(
        fixture.daemon_pid().is_none(),
        "the refusal starts no daemon"
    );
}

/// `kin init --no-enrich` over a repository that already exists records the
/// setting off, as a first init does, and the daemon it starts for the answer
/// queues no sweep.
#[test]
fn a_second_init_with_no_enrich_switches_an_existing_repository_off() {
    let fixture = Fixture::new();
    fixture.init_json("--defer-enrich");
    assert!(fixture.repository_enabled());

    let output = fixture.kin(&["init", "--no-enrich"], &[]);
    let stdout = String::from_utf8_lossy(succeeded(&output, "a second kin init --no-enrich"));
    assert!(
        stdout.contains("already a Kin repository") && stdout.contains("off for this repository"),
        "the answer says the repository is now off: {stdout}"
    );
    assert!(
        !fixture.repository_enabled(),
        "a second --no-enrich records the setting off"
    );
    assert!(
        fixture.daemon_pid().is_some(),
        "the answer starts the repository's daemon"
    );
    assert!(
        fixture.log_says_within(STARTED_SWITCHED_OFF, SWEEP_START_BUDGET),
        "the daemon it starts reads the setting; daemon.log:\n{}",
        fixture.daemon_log()
    );
    assert!(
        !fixture.log_says_within(SWEEP_STARTED, SWEEP_START_BUDGET),
        "and queues no sweep; daemon.log:\n{}",
        fixture.daemon_log()
    );
}

/// `kin daemon sweep --enable` records the setting on, stops the daemon that
/// started switched off, and sweeps in the next one.
#[test]
fn enable_records_the_setting_on_restarts_the_switched_off_daemon_and_sweeps() {
    let fixture = Fixture::new();
    fixture.init_json("--no-enrich");
    let switched_off = fixture.start_daemon();
    assert!(
        fixture.log_says_within(STARTED_SWITCHED_OFF, SWEEP_START_BUDGET),
        "daemon.log:\n{}",
        fixture.daemon_log()
    );

    let output = fixture.kin(&["daemon", "sweep", "--enable"], &[]);
    succeeded(&output, "kin daemon sweep --enable");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Language-server enrichment is now on for this repository."),
        "{stderr}"
    );
    assert!(fixture.repository_enabled(), "the setting is recorded on");
    let serving = fixture
        .daemon_pid()
        .expect("a daemon serves the repository after the sweep");
    assert_ne!(
        serving, switched_off,
        "the daemon that started switched off was replaced"
    );
    assert!(
        fixture.daemon_log().contains(SWEEP_STARTED),
        "the sweep ran in the new daemon; daemon.log:\n{}",
        fixture.daemon_log()
    );
}

/// The loop the review found: a switch the setting does not reach keeps every
/// daemon this command starts switched off, and `--enable` used to restart one,
/// see it come back off, and tell its reader to run `--enable` again. Now it
/// names the switch and its way out, the same way every time, and once the
/// switch is gone `--enable` restarts the daemon another command left off and
/// sweeps.
#[test]
fn enable_never_loops_while_a_switch_keeps_every_daemon_off() {
    let fixture = Fixture::new();
    fixture.init_json("--no-enrich");

    // The setting is recorded on, and the switch this command carries is named
    // before any daemon is restarted into it.
    let first = refused(
        &fixture.kin(
            &["daemon", "sweep", "--enable"],
            &[FILESYSTEM_RECONCILE_OFF],
        ),
        "kin daemon sweep --enable under the switch",
    );
    assert!(fixture.repository_enabled(), "{first}");
    assert!(
        first.contains("KIN_DAEMON_DISABLE_FILESYSTEM_RECONCILE=1")
            && first.contains("Unset it, run `kin daemon stop`, and run `kin daemon sweep`"),
        "{first}"
    );
    assert!(!first.contains("--enable"), "{first}");

    // Run again, the daemon this command starts carries the switch and says so
    // in its answer. The refusal names it and does not send the reader round.
    let mut refusals = Vec::new();
    for _ in 0..2 {
        let again = refused(
            &fixture.kin(
                &["daemon", "sweep", "--enable"],
                &[FILESYSTEM_RECONCILE_OFF],
            ),
            "kin daemon sweep --enable under the switch, again",
        );
        assert!(
            again.contains("KIN_DAEMON_DISABLE_FILESYSTEM_RECONCILE=1"),
            "{again}"
        );
        assert!(!again.contains("--enable"), "{again}");
        refusals.push(again);
    }
    assert_eq!(refusals[0], refusals[1], "the answer is stable, not a loop");
    let left_off = fixture
        .daemon_pid()
        .expect("the refused command left its daemon serving");

    // Without the switch, `--enable` restarts the daemon that carries it, and
    // the sweep runs.
    let output = fixture.kin(&["daemon", "sweep", "--enable"], &[]);
    succeeded(&output, "kin daemon sweep --enable without the switch");
    assert_ne!(fixture.daemon_pid(), Some(left_off));
    assert!(
        fixture.daemon_log().contains(SWEEP_STARTED),
        "daemon.log:\n{}",
        fixture.daemon_log()
    );
}
