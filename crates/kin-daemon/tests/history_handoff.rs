// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! End-to-end proof that a daemon which is being read attaches its store's
//! complete history once that history is staged.
//!
//! A store converted with only its checked-out commit derives the rest in a
//! worker and stages it beside the store. A daemon that never idles used to
//! leave it there for good. This test runs the real daemon over such a store
//! while a reader calls it every few milliseconds, and requires:
//!
//! - while the store holds open work, the daemon keeps serving the partial
//!   store, moves nothing, and records what it is waiting for;
//! - once that work is gone, the daemon hands the store over without being
//!   asked: its process ends cleanly, a successor it started itself attaches
//!   the staged history, and that successor serves the complete store.
//!
//! Unix only: the successor stays inside this test's containment because a
//! contained daemon does not start a new session, and that rule is the Unix
//! one.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kin_core::history_attach;
use kin_core::history_state::{
    self, DerivationState, HistoryDerivationProgress, HistoryMode, HistoryStanding,
};
use kin_core::KinLayout;
use tokio::process::Command;

mod common;

use common::{
    daemon_test_output, isolate_daemon_test_command, spawn_daemon_test_command, terminate_daemon,
    DaemonChild,
};

const TIMEOUT: Duration = Duration::from_secs(180);
const FIXTURE_ROOT: &str = "KIN_TEST_HISTORY_HANDOFF_FIXTURE_ROOT";
const FIXTURE_REPORT: &str = "KIN_TEST_HISTORY_HANDOFF_FIXTURE_REPORT";

/// Builds the fixture in a process of its own, so the conversion chooses its
/// store under this test's private home and never under the parent's.
#[test]
fn staged_history_fixture_worker() {
    let Some(root) = std::env::var_os(FIXTURE_ROOT) else {
        return;
    };
    let root = PathBuf::from(root);
    let report = PathBuf::from(std::env::var_os(FIXTURE_REPORT).expect("fixture report path"));
    let git = |args: &[&str]| {
        // A plain Git: this worker's own environment is already scrubbed of
        // inherited Git and Kin authority by the test that started it.
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=History Test",
                "-c",
                "user.email=history@example.test",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(&root)
            .output()
            .expect("run git for the fixture");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q", "--initial-branch=main"]);
    let hooks = root.join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    git(&["config", "core.hooksPath", hooks.to_str().unwrap()]);
    let excludes = root.join(".git/fixture-excludes");
    std::fs::write(&excludes, b"").unwrap();
    git(&["config", "core.excludesFile", excludes.to_str().unwrap()]);
    std::fs::write(root.join("a.py"), b"def a():\n    return 1\n").unwrap();
    git(&["add", "a.py"]);
    git(&["commit", "-q", "-s", "-m", "Add a"]);
    std::fs::write(
        root.join("b.py"),
        b"from a import a\n\ndef b():\n    return a()\n",
    )
    .unwrap();
    git(&["add", "b.py"]);
    git(&["commit", "-q", "-s", "-m", "Add b"]);

    let partial = kin_core::init_current_commit_from_git(&root, None, HistoryMode::Background)
        .expect("convert the checked-out commit");
    let stage = history_attach::new_stage_parent(&root, partial.repository_id.as_str())
        .expect("a stage beside the store");
    kin_core::derive_complete_history_into_stage(
        &root,
        partial.layout.root(),
        &partial.repository_id,
        &stage,
    )
    .expect("derive the complete history into the stage");
    // What a background worker records when it has staged the history.
    let mut staged = HistoryDerivationProgress::new(DerivationState::Staged);
    staged.attempts = 1;
    staged.detail = Some(stage.display().to_string());
    history_state::write_progress(&partial.layout, &staged).expect("record the staged history");

    let output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(report)
        .expect("create the fixture report");
    serde_json::to_writer(
        output,
        &serde_json::json!({
            "root": partial.layout.root(),
            "repository_id": partial.repository_id.as_str(),
            "stage": stage,
        }),
    )
    .expect("write the fixture report");
}

struct StagedStore {
    layout: KinLayout,
    repository_id: kin_model::RepositoryId,
    stage: PathBuf,
    home: tempfile::TempDir,
}

impl StagedStore {
    fn progress(&self) -> Option<HistoryDerivationProgress> {
        match history_state::standing(&self.layout) {
            HistoryStanding::Partial { progress, .. } => progress,
            _ => None,
        }
    }

    fn changes(&self) -> usize {
        let (authority, _) =
            kin_core::repository_authority::open_persisted_local_repository_authority(
                self.repository_id.clone(),
                Arc::new(kin_db::LocalFileBackend::new(self.layout.kindb_dir())),
            )
            .expect("open the store's persisted authority");
        let lease = authority.read_authority();
        lease.snapshot().changes.len()
    }
}

async fn staged_store(repo: &Path) -> StagedStore {
    let repo = repo.canonicalize().expect("canonical repository path");
    let home = tempfile::tempdir().expect("private home");
    let report = home.path().join("fixture.json");
    let mut command = Command::new(std::env::current_exe().expect("this test binary"));
    isolate_daemon_test_command(&mut command);
    command
        .args(["--exact", "staged_history_fixture_worker", "--nocapture"])
        .env("KIN_HOME", home.path())
        .env("KIN_REGISTRY_PATH", home.path().join("registry.toml"))
        .env(FIXTURE_ROOT, &repo)
        .env(FIXTURE_REPORT, &report);
    let output = daemon_test_output(command, "staged history fixture", TIMEOUT)
        .await
        .expect("contained fixture worker");
    assert!(
        output.status.success(),
        "fixture worker failed: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let built: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report).expect("fixture report")).unwrap();
    let root = PathBuf::from(built["root"].as_str().expect("reported store root"));
    assert!(
        root.canonicalize()
            .unwrap()
            .starts_with(home.path().canonicalize().unwrap()),
        "the fixture store escaped its private home: {}",
        root.display()
    );
    StagedStore {
        layout: KinLayout::external(root, repo),
        repository_id: kin_model::RepositoryId::new(
            built["repository_id"].as_str().unwrap().to_string(),
        )
        .unwrap(),
        stage: PathBuf::from(built["stage"].as_str().expect("reported stage")),
        home,
    }
}

fn spawn_daemon(store: &StagedStore) -> DaemonChild {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kin-daemon"));
    isolate_daemon_test_command(&mut command);
    command
        .arg("--repo")
        .arg(store.layout.working_dir())
        .arg("--store")
        .arg(store.layout.root())
        .args(["--port", "0"])
        .env("KIN_HOME", store.home.path())
        .env("KIN_REGISTRY_PATH", store.home.path().join("registry.toml"))
        .env("KIN_DAEMON_DISABLE_LSP", "1")
        .env("KIN_DAEMON_AUTO_EMBED", "0")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    spawn_daemon_test_command(command, "history handoff daemon")
        .expect("spawn the contained daemon")
}

/// The daemon a store's endpoint records, when it answers `/health`.
async fn serving(client: &reqwest::Client, root: &Path) -> Option<(u32, u16)> {
    let read = |name: &str| std::fs::read_to_string(root.join(name)).ok();
    let pid = read("daemon.pid")?.trim().parse::<u32>().ok()?;
    let port = read("daemon.port")?.trim().parse::<u16>().ok()?;
    let health = client
        .get(format!("http://127.0.0.1:{port}/health"))
        .send()
        .await
        .ok()?;
    (port != 0 && health.status().is_success()).then_some((pid, port))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_that_is_being_read_attaches_its_staged_history() {
    let repo = tempfile::tempdir().expect("repository directory");
    let store = staged_store(repo.path()).await;
    let root = store.layout.root().to_path_buf();
    assert_eq!(store.changes(), 1, "the fixture store holds its snapshot");

    // Stashed work is open work: the attach waits for it, and so does a
    // daemon that would otherwise leave to attach.
    let stash = store.layout.stashes_dir().join("held");
    std::fs::create_dir_all(store.layout.stashes_dir()).unwrap();
    std::fs::write(&stash, b"stashed work").unwrap();

    let mut daemon = spawn_daemon(&store);
    let first_pid = daemon.id().expect("daemon pid");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while serving(&client, &root).await.is_none() {
        assert!(
            daemon.try_wait().expect("poll the daemon").is_none(),
            "the daemon exited before it served"
        );
        assert!(Instant::now() < deadline, "the daemon never served");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Something reads the repository the whole time, as the local app does.
    let reading = Arc::new(AtomicBool::new(true));
    let answered = Arc::new(AtomicU64::new(0));
    let reader = {
        let (reading, answered, client, root) = (
            Arc::clone(&reading),
            Arc::clone(&answered),
            client.clone(),
            root.clone(),
        );
        tokio::spawn(async move {
            while reading.load(Ordering::SeqCst) {
                if serving(&client, &root).await.is_some() {
                    answered.fetch_add(1, Ordering::SeqCst);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
    };

    // The start found the stash and attached nothing. The daemon serves the
    // partial store and says what the history is waiting for.
    let waiting_for = history_attach::OpenWork::Stash.describe();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let progress = store.progress().expect("the store stays partial");
        if progress.staged_waiting_for() == Some(waiting_for) {
            assert_eq!(progress.staged_at(), Some(store.stage.clone()));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the wait was never recorded: {progress:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Well past several looks, it is the same daemon, still serving, and the
    // stash and the stage are untouched.
    let answered_before = answered.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(
        daemon.try_wait().expect("poll the daemon").is_none(),
        "the daemon left while the store held open work"
    );
    assert_eq!(
        serving(&client, &root).await.map(|(pid, _)| pid),
        Some(first_pid)
    );
    assert!(answered.load(Ordering::SeqCst) > answered_before);
    assert!(!history_state::standing(&store.layout).is_complete());
    assert_eq!(std::fs::read(&stash).unwrap(), b"stashed work");
    assert!(history_attach::staged_store(&store.stage)
        .join("manifest.json")
        .is_file());

    // The work is finished. Nobody stops the daemon or asks it for anything
    // but its health, and the complete history arrives.
    std::fs::remove_file(&stash).unwrap();
    let handed_off = Instant::now();
    let deadline = handed_off + TIMEOUT;
    let successor = loop {
        if history_state::standing(&store.layout).is_complete() {
            if let Some((pid, port)) = serving(&client, &root).await {
                if pid != first_pid {
                    break (pid, port);
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "the staged history was never attached; progress: {:?}",
            store.progress()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    eprintln!(
        "complete history served by daemon {} after {:?}",
        successor.0,
        handed_off.elapsed()
    );
    reading.store(false, Ordering::SeqCst);
    reader.await.unwrap();

    // The first daemon ended by itself, cleanly.
    let status = tokio::time::timeout(Duration::from_secs(60), daemon.wait())
        .await
        .expect("the first daemon ends after handing off")
        .expect("reap the first daemon");
    let stage_gone = !store.stage.exists();
    let journal_gone = !root.join("history-attach.json").exists();
    let progress = store.progress();
    let changes = store.changes();
    // End the successor before asserting, so a failure leaks no process. The
    // successor is inside this test's containment, which is proven empty.
    let cleanup = terminate_daemon(&mut daemon, "history handoff daemon").await;
    let deadline = Instant::now() + Duration::from_secs(30);
    while serving(&client, &root).await.is_some() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        serving(&client, &root).await.is_none(),
        "the successor daemon {} outlived the test's containment; cleanup={cleanup:?}",
        successor.0
    );
    assert!(cleanup.is_ok(), "{cleanup:?}");

    assert!(status.success(), "the first daemon ended with {status}");
    assert_eq!(changes, 2, "the attached store holds both commits");
    assert!(stage_gone, "the stage was left behind");
    assert!(journal_gone, "an attach journal was left behind");
    assert!(progress.is_none(), "a complete store reports no derivation");
}
