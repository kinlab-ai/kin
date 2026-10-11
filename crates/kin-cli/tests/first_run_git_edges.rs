// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Three things ordinary Git setups did to a first run of Kin.
//!
//! - `git commit` starts `git gc --auto` in the background once a repository
//!   holds enough loose objects, and `kin init` run straight after refused on
//!   the `gc.pid` that collection held, as it did on one a killed collection
//!   left behind.
//! - With no Git identity configured, `kin status` and `kin diff` refused to
//!   measure the working copy and exited 9, over an author the admission they
//!   ask for never records.
//! - A store made before `kin init` kept it out of `git status` stayed listed
//!   as untracked through every later `kin init` and `kin upgrade`.
//!
//! Each case drives the binary, because a helper being right and the command
//! never calling it look the same from a unit test.

// The held daemon and the process fixtures are Unix-only, as in the sibling
// `unmeasured_working_copy.rs` this borrows its daemon handling from.
#![cfg(unix)]

use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

mod common;

use common::Command;

/// The exit status of a command refused because it would record an author and
/// has none. A literal, so changing the published value has to break here.
const EXIT_AUTHOR_IDENTITY_REQUIRED: i32 = 10;

/// The exit status of a read that measured no working copy.
const EXIT_WORKING_COPY_UNMEASURED: i32 = 9;

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

fn git_status(repo: &Path) -> String {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", kin_git::empty_global_git_config())
        .current_dir(repo)
        .output()
        .expect("run git status");
    assert!(output.status.success(), "git status failed");
    String::from_utf8(output.stdout).expect("utf8 git status")
}

/// A Git repository with one commit. `identity` decides whether the
/// repository's own configuration names an author; the commit itself always
/// names one on the command line.
fn seed_repo(root: &Path, identity: bool) -> PathBuf {
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("create repo");
    run_git(&repo, &["init", "--initial-branch=main"]);
    if identity {
        run_git(&repo, &["config", "user.email", "kin@example.invalid"]);
        run_git(&repo, &["config", "user.name", "Kin"]);
    }
    fs::write(
        repo.join("storage.py"),
        b"def append(entry, path):\n    open(path, 'a').write(repr(entry))\n",
    )
    .expect("write module");
    run_git(&repo, &["add", "--all"]);
    run_git(
        &repo,
        &[
            "-c",
            "user.name=Seed",
            "-c",
            "user.email=seed@example.invalid",
            "commit",
            "-m",
            "seed",
        ],
    );
    repo
}

fn run_kin(
    repo: &Path,
    runtime: &common::IsolatedDaemonRuntime,
    args: &[&str],
    env: &[(&str, &str)],
    daemon_port: Option<u16>,
) -> Output {
    let mut command = runtime.kin_command();
    command
        .args(args)
        // No host identity may reach the binary: the runtime already isolates
        // HOME and XDG_CONFIG_HOME, and this refuses the system scope.
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("KIN_DAEMON_AUTO_EMBED", "0")
        .env("KIN_EMBED_BACKEND", "cpu")
        .env_remove("KIN_VFS_WORKSPACE")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .current_dir(repo);
    for (key, value) in env {
        command.env(key, value);
    }
    match daemon_port {
        Some(port) => {
            command.fixture_daemon_url(format!("http://127.0.0.1:{port}"));
        }
        None => {
            command.env_remove("KIN_DAEMON_URL");
        }
    }
    command.output().expect("run kin")
}

fn text_of(output: &Output) -> String {
    format!(
        "exit={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_init_succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "kin init failed:\n{}",
        text_of(output)
    );
}

/// A process standing in for a `git gc` that is running: alive, on this host,
/// and removing its record when it exits, as Git's collection does.
struct RunningCollection {
    child: Option<std::process::Child>,
}

impl RunningCollection {
    fn start(repo: &Path, runs_for: Duration) -> Self {
        let record = repo.join(".git/gc.pid");
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "sleep {}; rm -f '{}'",
                runs_for.as_secs(),
                record.display()
            ))
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn the stand-in collection");
        fs::write(
            &record,
            format!("{} {}", child.id(), kin_git::gc_lock::this_host()),
        )
        .expect("write gc.pid");
        Self { child: Some(child) }
    }
}

impl Drop for RunningCollection {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn a_gc_record_whose_collection_is_gone_does_not_block_init() {
    let root = tempdir().expect("temp root");
    let repo = seed_repo(root.path(), true);
    let mut killed = std::process::Command::new("true")
        .spawn()
        .expect("spawn true");
    let gone = killed.id();
    killed.wait().expect("reap true");
    fs::write(
        repo.join(".git/gc.pid"),
        format!("{gone} {}", kin_git::gc_lock::this_host()),
    )
    .expect("write gc.pid");

    let runtime = common::IsolatedDaemonRuntime::new(&repo);
    let init = run_kin(&repo, &runtime, &["init", "."], &[], None);
    assert_init_succeeded(&init);
    assert!(
        !String::from_utf8_lossy(&init.stderr).contains("garbage collection"),
        "a collection that is not running was waited on:\n{}",
        text_of(&init)
    );
}

#[test]
fn init_waits_for_a_running_git_gc_and_then_converts() {
    let root = tempdir().expect("temp root");
    let repo = seed_repo(root.path(), true);
    let collection = RunningCollection::start(&repo, Duration::from_secs(6));

    let runtime = common::IsolatedDaemonRuntime::new(&repo);
    let init = run_kin(&repo, &runtime, &["init", "."], &[], None);
    drop(collection);
    assert_init_succeeded(&init);
    let stderr = String::from_utf8_lossy(&init.stderr);
    assert!(
        stderr.contains("Git garbage collection (pid")
            && stderr.contains("waiting up to 30 s for it to finish"),
        "init did not say it was waiting for the collection:\n{}",
        text_of(&init)
    );
    assert!(
        stderr.contains("finished after"),
        "init did not say the collection finished before it went on:\n{}",
        text_of(&init)
    );
}

/// A daemon this test owns, as in `unmeasured_working_copy.rs`: its lifetime is
/// bounded by the test rather than by an idle clock the test would race.
struct HeldDaemon {
    child: Option<common::RuntimeOwnedChild>,
    log: PathBuf,
}

impl HeldDaemon {
    fn spawn(
        repo: &Path,
        layout: &kin_core::KinLayout,
        runtime: &common::IsolatedDaemonRuntime,
    ) -> Self {
        let log = runtime.registry_path().with_file_name("held-daemon.log");
        let sink = fs::File::create(&log).expect("create the held daemon log");
        let child = runtime
            .daemon_command()
            .arg("--store")
            .arg(layout.root())
            .arg("--repo")
            .arg(repo)
            .arg("--port")
            .arg("0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("KIN_DAEMON_DISABLE_LSP", "1")
            .env("KIN_DAEMON_IDLE_TIMEOUT_SECS", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(sink))
            .spawn_owned()
            .expect("spawn the held kin-daemon");
        Self {
            child: Some(child),
            log,
        }
    }

    fn wait_until_serving(&mut self, kin_root: &Path) -> u16 {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let child = self.child.as_mut().expect("daemon child exists");
            if let Some(status) = child.try_wait().expect("inspect daemon child") {
                panic!(
                    "the held daemon exited before readiness: {status}; log:\n{}",
                    fs::read_to_string(&self.log).unwrap_or_default()
                );
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
                "the held daemon did not become ready"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for HeldDaemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

const UNMEASURED_BANNER: &str = "Working copy: NOT MEASURED";

#[test]
fn reads_measure_the_working_copy_with_no_git_identity_and_a_commit_names_what_is_missing() {
    let root = tempdir().expect("temp root");
    let repo = seed_repo(root.path(), false);
    let runtime = common::IsolatedDaemonRuntime::new(&repo);
    let init = run_kin(&repo, &runtime, &["init", "."], &[], None);
    assert_init_succeeded(&init);

    fs::write(
        repo.join("storage.py"),
        b"def append(entry, path):\n    \"\"\"Append one entry.\"\"\"\n    open(path, 'a').write(repr(entry))\n",
    )
    .expect("edit module");
    let layout = runtime.layout();
    let mut daemon = HeldDaemon::spawn(&repo, &layout, &runtime);
    let port = daemon.wait_until_serving(layout.root());

    // The positive control: this fixture really has no identity, so a command
    // that records an author refuses, with its own status, naming what is
    // missing and how to set it. Without it, every read below could pass on a
    // host identity that leaked in.
    let commit = run_kin(&repo, &runtime, &["commit", "-m", "edit"], &[], Some(port));
    assert_eq!(
        commit.status.code(),
        Some(EXIT_AUTHOR_IDENTITY_REQUIRED),
        "a commit with no identity did not exit with the identity status:\n{}",
        text_of(&commit)
    );
    let refusal = String::from_utf8_lossy(&commit.stderr);
    assert!(
        refusal.contains(
            "Error: this change needs an author, and none is configured: Git has no user.name \
             and no user.email"
        ),
        "the refusal does not lead with what is missing:\n{}",
        text_of(&commit)
    );
    assert!(
        refusal.contains("git config --global user.name")
            && refusal.contains("git config --global user.email"),
        "the refusal does not say how to set an identity:\n{}",
        text_of(&commit)
    );

    let status = run_kin(&repo, &runtime, &["status"], &[], Some(port));
    assert_eq!(
        status.status.code(),
        Some(0),
        "status with no identity did not measure the working copy:\n{}",
        text_of(&status)
    );
    assert!(
        !String::from_utf8_lossy(&status.stdout).contains(UNMEASURED_BANNER),
        "status with no identity printed the unmeasured banner:\n{}",
        text_of(&status)
    );

    let diff = run_kin(
        &repo,
        &runtime,
        &["diff", "HEAD", "WORKSPACE"],
        &[],
        Some(port),
    );
    assert_ne!(
        diff.status.code(),
        Some(EXIT_WORKING_COPY_UNMEASURED),
        "a workspace diff with no identity measured nothing:\n{}",
        text_of(&diff)
    );
    assert_eq!(diff.status.code(), Some(0), "{}", text_of(&diff));
    assert!(
        String::from_utf8_lossy(&diff.stdout).contains("Artifacts: +0 ~1 -0"),
        "the measured diff did not report the edit:\n{}",
        text_of(&diff)
    );

    let admit = run_kin(&repo, &runtime, &["admit"], &[], Some(port));
    assert_eq!(
        admit.status.code(),
        Some(0),
        "kin admit with no identity refused:\n{}",
        text_of(&admit)
    );
}

fn exclude_lines(repo: &Path) -> Vec<String> {
    fs::read_to_string(repo.join(".git/info/exclude"))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| *line == "/.kin/")
        .map(str::to_string)
        .collect()
}

/// Take the store's rule back out, as a store made before `kin init` wrote one
/// looks, and prove `git status` then lists the store.
fn forget_the_store_rule(repo: &Path) {
    let exclude = repo.join(".git/info/exclude");
    let kept = fs::read_to_string(&exclude)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.trim() != "/.kin/")
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    fs::write(&exclude, kept).expect("rewrite local exclude");
    assert!(
        git_status(repo).contains(".kin/"),
        "the fixture did not reach the untracked-store state: {}",
        git_status(repo)
    );
}

#[test]
fn reinit_and_upgrade_keep_an_existing_in_tree_store_out_of_git_status() {
    let root = tempdir().expect("temp root");
    let repo = seed_repo(root.path(), true);
    let runtime = common::IsolatedDaemonRuntime::new(&repo);
    let in_tree = [("KIN_STORE_LAYOUT", "in-tree")];
    let init = run_kin(&repo, &runtime, &["init", "."], &in_tree, None);
    assert_init_succeeded(&init);
    assert!(repo.join(".kin").is_dir(), "init made no in-tree store");
    assert_eq!(exclude_lines(&repo), vec!["/.kin/"]);

    forget_the_store_rule(&repo);
    let again = run_kin(&repo, &runtime, &["init", "."], &in_tree, None);
    assert_init_succeeded(&again);
    assert_eq!(
        git_status(&repo),
        "",
        "a second kin init left the store untracked:\n{}",
        text_of(&again)
    );
    assert_eq!(exclude_lines(&repo), vec!["/.kin/"], "one rule, not two");
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("local Git excludes"),
        "the repair was not reported:\n{}",
        text_of(&again)
    );
    let third = run_kin(&repo, &runtime, &["init", "."], &in_tree, None);
    assert_init_succeeded(&third);
    assert_eq!(
        exclude_lines(&repo),
        vec!["/.kin/"],
        "a re-init over a rule that is already there added another"
    );

    forget_the_store_rule(&repo);
    let upgrade = run_kin(&repo, &runtime, &["upgrade"], &[], None);
    assert_eq!(
        upgrade.status.code(),
        Some(0),
        "kin upgrade failed:\n{}",
        text_of(&upgrade)
    );
    assert_eq!(
        git_status(&repo),
        "",
        "kin upgrade left the store untracked:\n{}",
        text_of(&upgrade)
    );
    assert!(
        !repo.join(".gitignore").exists(),
        "the rule must stay local rather than entering the tracked ignore file"
    );
}

#[test]
fn reinit_of_a_central_store_leaves_git_excludes_alone() {
    let root = tempdir().expect("temp root");
    let repo = seed_repo(root.path(), true);
    let exclude_before = fs::read(repo.join(".git/info/exclude")).expect("read local exclude");
    let runtime = common::IsolatedDaemonRuntime::new(&repo);
    for _ in 0..2 {
        let init = run_kin(&repo, &runtime, &["init", "."], &[], None);
        assert_init_succeeded(&init);
    }
    assert!(
        runtime.layout().is_external(),
        "default init was not central"
    );
    assert!(
        !repo.join(".kin").exists(),
        "central init wrote an in-tree store"
    );
    assert_eq!(
        fs::read(repo.join(".git/info/exclude")).expect("read local exclude"),
        exclude_before,
        "a store outside the working tree needs no exclusion"
    );
    assert_eq!(git_status(&repo), "", "init dirtied the working tree");
}
