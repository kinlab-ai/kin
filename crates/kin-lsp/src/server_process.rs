// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A language server's process and every process it starts, owned as one.
//!
//! A language server is rarely one process. typescript-language-server runs
//! two tsserver processes as its children and one of those runs a typings
//! installer; rust-analyzer runs cargo and a proc-macro server; gopls runs the
//! go command. Ending only the process this crate started leaves the rest to
//! notice on their own, and they do not reliably: after kin-bench proof daemons
//! stopped, 19 tsserver processes of 50 to 115 MB each were still running with
//! init as their parent 20 to 40 minutes later.
//!
//! So a server starts as the leader of a new process group, which everything
//! it starts inherits, and every way this crate lets go of it ends the group:
//!
//! - [`ServerProcess::terminate`] is the deliberate stop. It sends SIGTERM to
//!   the group, then SIGKILL to whatever is still in it after
//!   [`TERMINATION_GRACE`].
//! - Dropping a [`ServerProcess`] that was not terminated sends SIGKILL to the
//!   group at once, which is what `kill_on_drop` did for the leader alone.
//! - An owner that dies without doing either, because it was killed, crashed or
//!   force-exited, runs no code on the way out. For that, a watcher process
//!   blocks reading a pipe whose only write end the owner holds. The kernel
//!   closes that write end when the owner ends, however it ends, so the read
//!   returns and the watcher sends the group the same SIGTERM, grace and
//!   SIGKILL. macOS has no `PR_SET_PDEATHSIG`, and a pipe that reaches
//!   end-of-file when its writer dies is the death notice both platforms give.
//!
//! Windows keeps `kill_on_drop` on the leader alone.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

/// How long a language server's process group has to exit after SIGTERM
/// before whatever is left of it is sent SIGKILL.
///
/// Short, because Kin keeps nothing in a language server worth waiting for,
/// and a daemon stops every server it holds out of one bounded shutdown budget.
/// Whole seconds, because the watcher's shell counts it with `sleep 1`.
pub const TERMINATION_GRACE: Duration = Duration::from_secs(2);

/// A shorter grace a test set for every stop in this process, in
/// milliseconds, or zero for [`TERMINATION_GRACE`].
#[cfg(any(test, feature = "test-support"))]
static TEST_TERMINATION_GRACE_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Give every server this process stops from now on `grace`, rather than
/// [`TERMINATION_GRACE`], to exit after SIGTERM before SIGKILL.
///
/// For tests that prove the escalation, with a member that ignores SIGTERM,
/// without waiting out the production grace each time. Only the length
/// changes: a stop still sends SIGTERM, waits, and then sends SIGKILL to
/// whatever is left. A watcher counts its grace in whole seconds, so one
/// started under a grace below a second sends SIGKILL right after SIGTERM.
///
/// It is process-wide, which is what a test that runs in a process of its own
/// wants; `Duration::ZERO` restores the production grace. Built only for this
/// crate's tests and under the `test-support` feature.
#[cfg(any(test, feature = "test-support"))]
pub fn set_termination_grace_for_tests(grace: Duration) {
    let millis = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX);
    TEST_TERMINATION_GRACE_MS.store(millis, std::sync::atomic::Ordering::SeqCst);
}

/// The grace a stop gives a group after SIGTERM: [`TERMINATION_GRACE`] unless
/// a test shortened it.
fn termination_grace() -> Duration {
    #[cfg(any(test, feature = "test-support"))]
    {
        let millis = TEST_TERMINATION_GRACE_MS.load(std::sync::atomic::Ordering::SeqCst);
        if millis > 0 {
            return Duration::from_millis(millis);
        }
    }
    TERMINATION_GRACE
}

/// How often a stop checks whether the group has emptied.
#[cfg(unix)]
const GROUP_POLL: Duration = Duration::from_millis(20);

/// How long a stop waits for SIGKILL to finish the group before it gives up
/// and says what is left.
#[cfg(unix)]
const KILL_SETTLE: Duration = Duration::from_secs(1);

/// The watcher, run by `/bin/sh` with the server's process group as `$1` and
/// the grace in whole seconds as `$2`.
///
/// `read` returns only when every write end of stdin is closed, and the owner
/// holds the only one. The group check between sleeps lets a group that took
/// SIGTERM release the watcher at once rather than after the whole grace.
#[cfg(unix)]
const WATCHER_SCRIPT: &str = r#"read -r ignored
kill -s TERM -- "-$1" 2>/dev/null || exit 0
i=0
while [ "$i" -lt "$2" ]; do
  sleep 1
  kill -s 0 -- "-$1" 2>/dev/null || exit 0
  i=$((i + 1))
done
kill -s KILL -- "-$1" 2>/dev/null
exit 0
"#;

/// A running language server: the process this crate started and, on Unix,
/// the process group it leads.
pub(crate) struct ServerProcess {
    child: Child,
    #[cfg(unix)]
    runtime: Option<kin_daemon_spawn::test_runtime::TestRuntimeGroup>,
    /// The group's id, which is the leader's pid.
    #[cfg(unix)]
    group: libc::pid_t,
    /// `None` when the watcher could not be started; a stop and a drop still
    /// end the group.
    #[cfg(unix)]
    watcher: Option<GroupWatcher>,
    /// Set once this process has finished signalling the group: the group was
    /// seen empty, or a stop has sent everything it sends. An empty group's id
    /// is free for the system to hand out again, so nothing signals it after.
    #[cfg(unix)]
    done: bool,
}

impl ServerProcess {
    /// Start `command` with piped stdio, as the leader of a new process group.
    pub(crate) fn spawn(mut command: Command) -> std::io::Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The whole mechanism on Windows. On Unix it also hands a leader
            // that outlived its drop to tokio, which reaps it.
            .kill_on_drop(true);
        #[cfg(unix)]
        if let Some(mut runtime) =
            kin_daemon_spawn::test_runtime::TestRuntimeGroup::language_server_from_env()?
        {
            let group = runtime.group;
            let child = runtime.spawn_tokio(command)?;
            return Ok(Self {
                child,
                runtime: Some(runtime),
                group,
                watcher: None,
                done: false,
            });
        }
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn()?;

        #[cfg(unix)]
        {
            // A group id of 0 or 1 would turn every signal below into one sent
            // to this process's own group or to every process it may signal.
            // A child's pid is never either, and this makes that a checked fact.
            let group = child
                .id()
                .and_then(|pid| libc::pid_t::try_from(pid).ok())
                .filter(|pid| *pid > 1)
                .ok_or_else(|| std::io::Error::other("the server started without a usable pid"))?;
            let watcher = match GroupWatcher::spawn(group) {
                Ok(watcher) => Some(watcher),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        group,
                        "could not start the watcher that ends this language server's \
                         process group if this process dies; a stop or a drop still ends it"
                    );
                    None
                }
            };
            Ok(Self {
                child,
                runtime: None,
                group,
                watcher,
                done: false,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self { child })
        }
    }

    /// The executable leader's pid (the guardian has a separate sentinel).
    #[cfg(unix)]
    pub(crate) fn leader_pid(&self) -> libc::pid_t {
        if self.runtime.is_some() {
            return self
                .child
                .id()
                .and_then(|pid| libc::pid_t::try_from(pid).ok())
                .unwrap_or(0);
        }
        self.group
    }

    /// The server's stdin, stdout and stderr, each available once.
    pub(crate) fn take_stdio(
        &mut self,
    ) -> (Option<ChildStdin>, Option<ChildStdout>, Option<ChildStderr>) {
        (
            self.child.stdin.take(),
            self.child.stdout.take(),
            self.child.stderr.take(),
        )
    }

    /// How the server's own process ended, once it has. `None` while it is
    /// still running.
    ///
    /// On Unix this reads the status without reaping the leader. Its pid is
    /// the group's id, and an unreaped leader is still a member that keeps the
    /// id reserved, so the stop [`Self::terminate`] sends afterwards reaches
    /// this group and no other. Reaping here would free the id before that
    /// signal: a leader that died alone leaves an empty group, and a new
    /// process could be handed its id. `terminate` reaps it after signalling.
    pub(crate) fn exit_status(&mut self) -> Option<std::process::ExitStatus> {
        #[cfg(unix)]
        {
            let pid = if self.runtime.is_some() {
                match self.child.id().and_then(|p| libc::pid_t::try_from(p).ok()) {
                    Some(pid) => pid,
                    None => return self.child.try_wait().ok().flatten(),
                }
            } else {
                self.group
            };
            match exit_without_reaping(pid) {
                Ok(status) => status,
                // Not this process's child to wait on any more: already
                // reaped, in which case the handle kept the status and asking
                // it reaps nothing.
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                    self.child.try_wait().ok().flatten()
                }
                Err(_) => None,
            }
        }
        #[cfg(not(unix))]
        {
            self.child.try_wait().ok().flatten()
        }
    }

    /// Stop the server and everything it started: SIGTERM to the group, then
    /// SIGKILL to whatever is left after [`TERMINATION_GRACE`].
    ///
    /// Returns once the group is empty, or once SIGKILL has had a further
    /// second to empty it.
    pub(crate) async fn terminate(mut self) {
        #[cfg(unix)]
        {
            if let Some(mut runtime) = self.runtime.take() {
                // This runtime owns a pinned guardian, not an unverified PID.
                // Reap the child before the guardian finalizes its group proof.
                runtime.request_cleanup();
                let _ = tokio::time::timeout(KILL_SETTLE, self.child.wait()).await;
                if let Err(error) = runtime.finish_cleanup() {
                    tracing::warn!(%error, "runtime language server cleanup failed");
                }
                self.done = true;
                return;
            }
            signal_group(self.group, libc::SIGTERM);
            if !self.wait_for_empty_group(termination_grace()).await {
                signal_group(self.group, libc::SIGKILL);
                if !self.wait_for_empty_group(KILL_SETTLE).await {
                    tracing::warn!(
                        group = self.group,
                        "a language server's process group still has members after SIGKILL"
                    );
                }
            }
            self.done = true;
            if let Some(watcher) = self.watcher.take() {
                watcher.stand_down().await;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self.child.start_kill();
            let _ = tokio::time::timeout(termination_grace(), self.child.wait()).await;
        }
    }

    /// Whether the group empties within `budget`.
    #[cfg(unix)]
    async fn wait_for_empty_group(&mut self, budget: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            // Reap the leader as soon as it exits. Until it is reaped it is
            // still a member of its group and reads as a survivor.
            let _ = self.child.try_wait();
            if group_is_empty(self.group) {
                self.done = true;
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(GROUP_POLL).await;
        }
    }
}

#[cfg(unix)]
impl Drop for ServerProcess {
    fn drop(&mut self) {
        // A group's id is not handed out again while the group has a member,
        // so short of the group emptying in the instant before this runs, this
        // reaches only this server's processes.
        if !self.done {
            if let Some(runtime) = self.runtime.as_mut() {
                // Drop of the owned guardian records its own cleanup proof.
                runtime.request_cleanup();
                return;
            }
            signal_group(self.group, libc::SIGKILL);
        }
    }
}

/// The process that ends a server's group when its owner dies.
#[cfg(unix)]
struct GroupWatcher {
    // Declared first so it drops first: a watcher killed before its pipe closes
    // never mistakes the close for its owner's death.
    process: Child,
    // Never written. Holding it open is the whole message.
    _lifeline: ChildStdin,
}

#[cfg(unix)]
impl GroupWatcher {
    fn spawn(group: libc::pid_t) -> std::io::Result<Self> {
        let mut process = Command::new("/bin/sh")
            .arg("-c")
            .arg(WATCHER_SCRIPT)
            .arg("kin-lsp-group-watcher")
            .arg(group.to_string())
            .arg(termination_grace().as_secs().to_string())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Its own group, so a signal aimed at the owner's group, a
            // terminal's Ctrl-C for one, cannot end it before it has acted.
            .process_group(0)
            .kill_on_drop(true)
            .spawn()?;
        let lifeline = process
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("the watcher's stdin was not captured"))?;
        Ok(Self {
            process,
            _lifeline: lifeline,
        })
    }

    /// End the watcher without letting it act, once the group it guards is
    /// already gone.
    async fn stand_down(mut self) {
        let _ = self.process.start_kill();
        let _ = tokio::time::timeout(KILL_SETTLE, self.process.wait()).await;
    }
}

/// Send `signal` to every process in `group`. ESRCH, the only failure for a
/// group this process created, means the group has already emptied.
#[cfg(unix)]
fn signal_group(group: libc::pid_t, signal: libc::c_int) {
    // SAFETY: `kill` takes no pointers. `group` is a started child's pid,
    // checked above 1, so `-group` names exactly that process group.
    unsafe {
        libc::kill(-group, signal);
    }
}

/// How the child `pid` ended, read with `WNOWAIT` so it stays waitable, or
/// `None` while it is still running.
#[cfg(unix)]
fn exit_without_reaping(pid: libc::pid_t) -> std::io::Result<Option<std::process::ExitStatus>> {
    use std::os::unix::process::ExitStatusExt;

    // SAFETY: `info` is zeroed and the call writes only into it. Zeroing is
    // also how WNOHANG's "nothing to report" is told apart: si_pid stays 0.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let (code, reported, status) = child_status_parts(&info);
    if reported == 0 {
        return Ok(None);
    }
    // Rebuilt in the wait-status encoding `ExitStatus` reads on both Linux and
    // macOS: an exit code in the second byte, a signal in the low seven bits,
    // and 0x80 for a core dump.
    Ok(match code {
        libc::CLD_EXITED => Some(std::process::ExitStatus::from_raw((status & 0xff) << 8)),
        libc::CLD_KILLED => Some(std::process::ExitStatus::from_raw(status & 0x7f)),
        libc::CLD_DUMPED => Some(std::process::ExitStatus::from_raw((status & 0x7f) | 0x80)),
        _ => None,
    })
}

/// The code, pid and status a child-status `siginfo_t` carries.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn child_status_parts(info: &libc::siginfo_t) -> (libc::c_int, libc::pid_t, libc::c_int) {
    // SAFETY: waitid filled a child-status siginfo, which sets these fields.
    unsafe { (info.si_code, info.si_pid(), info.si_status()) }
}

/// The code, pid and status a child-status `siginfo_t` carries.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn child_status_parts(info: &libc::siginfo_t) -> (libc::c_int, libc::pid_t, libc::c_int) {
    (info.si_code, info.si_pid, info.si_status)
}

/// Whether no process is left in `group`.
#[cfg(unix)]
fn group_is_empty(group: libc::pid_t) -> bool {
    // SAFETY: as in `signal_group`; signal 0 only asks whether any member exists.
    let result = unsafe { libc::kill(-group, 0) };
    result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;

    /// Invoked in its own process by the private lifecycle qualification. This
    /// avoids mutating the environment of parallel ordinary unit tests.
    #[tokio::test]
    #[ignore = "requires a live native runtime guardian and private fixture environment"]
    async fn owned_runtime_fixture() {
        let case = std::env::var("KIN_RUNTIME_LSP_FIXTURE").expect("explicit fixture case");
        let marker = std::path::PathBuf::from(std::env::var_os("KIN_RUNTIME_LSP_MARKER").unwrap());
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "trap '' TERM; echo ready; sleep 60 & wait"]);
        if case == "readiness-failure" {
            // The fixture launcher injects this only into the LSP test
            // process, after its outer runtime guardian is ready.
            assert_eq!(
                std::env::var(kin_daemon_spawn::PROCESS_GROUP_GUARDIAN_EXIT_BEFORE_READY_ENV)
                    .ok()
                    .as_deref(),
                Some("1"),
                "readiness failure must be supplied by the fixture launcher"
            );
            assert!(ServerProcess::spawn(command).is_err());
            std::fs::write(marker, "rejected before payload").unwrap();
            return;
        }
        if case == "missing-command" {
            assert!(ServerProcess::spawn(Command::new("/missing/owned-lsp-fixture")).is_err());
            std::fs::write(marker, "rejected missing command").unwrap();
            return;
        }
        let mut process = ServerProcess::spawn(command).expect("owned server");
        let runtime = process.runtime.as_ref().expect("live runtime guardian");
        let group = runtime.group;
        let directory = runtime.directory.clone();
        assert_ne!(
            process.leader_pid(),
            group,
            "resolver identity must name the executable"
        );
        assert_eq!(Some(process.leader_pid() as u32), process.child.id());
        assert!(process.exit_status().is_none());
        let (_, stdout, _) = process.take_stdio();
        let mut line = String::new();
        tokio::io::BufReader::new(stdout.unwrap())
            .read_line(&mut line)
            .await
            .unwrap();
        assert_eq!(line.trim(), "ready");
        std::fs::write(marker, group.to_string()).unwrap();
        if case == "owner-interrupted" {
            tokio::time::sleep(Duration::from_secs(60)).await;
            panic!("fixture owner should be interrupted");
        }
        if case == "drop" {
            drop(process);
        } else {
            assert_eq!(case, "terminate");
            process.terminate().await;
        }
        let done: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.join("done.json")).unwrap()).unwrap();
        assert_eq!(done["cleanup_verified"], true);
        assert!(kin_daemon_spawn::test_runtime::recorded_group_is_quiescent(
            group
        ));
    }

    /// A stop sends SIGTERM, waits out the grace, and sends SIGKILL to a
    /// member that ignores SIGTERM. A test grace changes only how long the
    /// wait is, and production keeps its two seconds.
    #[tokio::test]
    async fn a_member_that_ignores_sigterm_is_killed_once_the_grace_runs_out() {
        assert_eq!(TERMINATION_GRACE, Duration::from_secs(2));
        let grace = Duration::from_millis(300);
        set_termination_grace_for_tests(grace);
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "trap '' TERM; echo ignoring; exec sleep 600"]);
        let mut process = ServerProcess::spawn(command).expect("spawn a member");
        let group = process.leader_pid();
        let (_stdin, stdout, _stderr) = process.take_stdio();
        // SIGTERM is ignored only once the trap is set, and `sleep` keeps
        // ignoring it across the exec.
        let mut line = String::new();
        tokio::io::BufReader::new(stdout.expect("captured stdout"))
            .read_line(&mut line)
            .await
            .expect("the member says it ignores SIGTERM");
        assert_eq!(line.trim(), "ignoring");

        let started = tokio::time::Instant::now();
        process.terminate().await;
        let took = started.elapsed();
        set_termination_grace_for_tests(Duration::ZERO);

        assert!(
            group_is_empty(group),
            "a member that ignores SIGTERM is ended by the SIGKILL after the grace"
        );
        assert!(
            took >= grace,
            "SIGKILL waited out the grace after SIGTERM: took {took:?}"
        );
        assert!(
            took < TERMINATION_GRACE,
            "the test grace replaced the production one: took {took:?}"
        );
    }
}
