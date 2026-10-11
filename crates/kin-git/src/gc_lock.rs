// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Whether Git's garbage collection is running against a repository.
//!
//! `git gc` writes `gc.pid` into the repository's common Git directory while it
//! runs, holding its process id and the name of its host, and removes it when
//! it exits. A collection that was killed, or a repository copied or synced
//! from another machine, leaves the record behind with nothing running. Git
//! reads such a record as stale and simply takes the lock over, so refusing on
//! the record's existence alone turned every leftover into a `kin init`
//! refusal that no Git command would ever clear.
//!
//! The reading follows Git's own in `lock_repo_for_gc`: an unparsable record
//! or one older than twelve hours is stale, and so is one whose process is not
//! alive on this host. It differs from Git in one place. Git defers to a record
//! naming another host, because a collection on another machine sharing the
//! repository might be running; this process can neither prove that nor wait
//! for it, and the usual way such a record arrives is with a repository that
//! was copied from somewhere else. So a record naming another host is stale
//! here too.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use kin_io::WorktreeRoot;

use crate::error::{GitError, Result};
use crate::lossless::open_repo;

/// The record `git gc` holds while it runs.
const GC_PID: &str = "gc.pid";

/// Git's own bound on how long a `gc.pid` can claim a running collection.
const GC_PID_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);

/// How often [`wait_for_running_git_gc`] looks again.
const GC_POLL: Duration = Duration::from_millis(250);

/// How long a conversion waits for Git's garbage collection before naming it.
///
/// An automatic collection, which `git commit` starts in the background once
/// a repository holds enough loose objects, finishes in seconds on a newly made
/// repository. One still running after this is a large repository's full
/// repack, which is better reported than waited out silently.
pub const GIT_GC_WAIT: Duration = Duration::from_secs(30);

/// A garbage collection this host can see running against a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningGitGc {
    /// The collection's process id, as its record names it.
    pub pid: u32,
    /// The record itself, so an operator can check it by hand.
    pub record: PathBuf,
}

impl RunningGitGc {
    /// What is running, in one clause.
    pub fn sentence(&self) -> String {
        format!(
            "Git garbage collection (pid {}) is running against this repository's objects",
            self.pid
        )
    }

    /// What to do about a collection that is still running.
    pub fn remedy(&self) -> String {
        format!(
            "wait for it to finish, then run kin init again; if pid {} is not a git process, the \
             collection was interrupted and {} can be removed",
            self.pid,
            self.record.display()
        )
    }
}

/// The running collection a `gc.pid` in `git_dir` names, or `None` when there
/// is no record or the record is stale.
pub fn running_git_gc_in(git_dir: &Path) -> Result<Option<RunningGitGc>> {
    let record = git_dir.join(GC_PID);
    let metadata = match fs::symlink_metadata(&record) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(GitError::io(&record, error)),
    };
    // Git writes the record through a lock file and a rename, so anything
    // other than a regular file is not one of its records.
    if !metadata.is_file() {
        return Ok(None);
    }
    let age = metadata
        .modified()
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .unwrap_or_default();
    if age > GC_PID_LIFETIME {
        return Ok(None);
    }
    let bytes = match fs::read(&record) {
        Ok(bytes) => bytes,
        // Removed between the two reads: the collection just finished.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(GitError::io(&record, error)),
    };
    let Some((pid, host)) = parse_gc_pid(&bytes) else {
        return Ok(None);
    };
    if host != this_host() || !process_is_running(pid) {
        return Ok(None);
    }
    Ok(Some(RunningGitGc { pid, record }))
}

/// Wait up to `budget` for a running collection against `repo_path` to finish.
///
/// `announce` is called once, when there is something to wait for, so the
/// caller can say why it paused. Returns the collection that is still running
/// when the budget runs out, or `None` once nothing is.
pub fn wait_for_running_git_gc(
    repo_path: &WorktreeRoot,
    budget: Duration,
    announce: &mut dyn FnMut(&RunningGitGc),
) -> Result<Option<RunningGitGc>> {
    let common_dir = open_repo(repo_path.path())?.common_dir().to_path_buf();
    wait_for_running_git_gc_in(&common_dir, budget, announce)
}

/// [`wait_for_running_git_gc`] over a known common Git directory.
pub(crate) fn wait_for_running_git_gc_in(
    common_dir: &Path,
    budget: Duration,
    announce: &mut dyn FnMut(&RunningGitGc),
) -> Result<Option<RunningGitGc>> {
    let started = Instant::now();
    let mut announced = false;
    loop {
        let Some(running) = running_git_gc_in(common_dir)? else {
            return Ok(None);
        };
        let waited = started.elapsed();
        if waited >= budget {
            return Ok(Some(running));
        }
        if !announced {
            announce(&running);
            announced = true;
        }
        std::thread::sleep(GC_POLL.min(budget - waited));
    }
}

/// The process id and host name a `gc.pid` holds.
///
/// Git writes `<pid> <host>` with no newline; a trailing newline is tolerated
/// because a record written by hand or by another tool may carry one.
fn parse_gc_pid(bytes: &[u8]) -> Option<(u32, String)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let text = text.trim_end_matches(['\n', '\r']);
    let (pid, host) = text.split_once(' ')?;
    let pid = pid.parse::<u32>().ok().filter(|pid| *pid != 0)?;
    Some((pid, host.to_string()))
}

/// This host's name as Git records it in `gc.pid`, which is `gethostname`.
pub fn this_host() -> String {
    #[cfg(unix)]
    {
        rustix::system::uname()
            .nodename()
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}

/// Whether `pid` names a live process on this host.
///
/// Errs toward running: only a definite "no such process" reads as gone,
/// because the cost of a wrong "gone" is reading objects a live collection is
/// repacking, and the cost of a wrong "running" is a bounded wait and a
/// refusal that names the pid.
fn process_is_running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Some(pid) = i32::try_from(pid)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        else {
            return false;
        };
        !matches!(
            rustix::process::test_kill_process(pid),
            Err(rustix::io::Errno::SRCH)
        )
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        child.wait().expect("reap true");
        pid
    }

    fn write_record(dir: &Path, body: &str) {
        fs::write(dir.join(GC_PID), body).expect("write gc.pid");
    }

    #[test]
    fn parses_the_record_git_writes() {
        assert_eq!(
            parse_gc_pid(b"4242 build-host.local"),
            Some((4242, "build-host.local".to_string()))
        );
        assert_eq!(
            parse_gc_pid(b"4242 build-host.local\n"),
            Some((4242, "build-host.local".to_string()))
        );
        assert_eq!(parse_gc_pid(b"not-a-pid host"), None);
        assert_eq!(parse_gc_pid(b"4242"), None);
        assert_eq!(parse_gc_pid(b"0 host"), None);
    }

    #[test]
    fn no_record_is_no_collection() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(running_git_gc_in(dir.path()).unwrap(), None);
    }

    #[test]
    fn a_record_whose_process_is_gone_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), &format!("{} {}", dead_pid(), this_host()));
        assert_eq!(running_git_gc_in(dir.path()).unwrap(), None);
    }

    #[test]
    fn a_record_from_another_host_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        // This test's own process is certainly alive; only the host differs.
        write_record(
            dir.path(),
            &format!("{} another-host.invalid", std::process::id()),
        );
        assert_eq!(running_git_gc_in(dir.path()).unwrap(), None);
    }

    #[test]
    fn an_unreadable_record_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), "garbage");
        assert_eq!(running_git_gc_in(dir.path()).unwrap(), None);
    }

    #[test]
    fn a_record_older_than_git_honours_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        write_record(
            dir.path(),
            &format!("{} {}", std::process::id(), this_host()),
        );
        let old = SystemTime::now() - GC_PID_LIFETIME - Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(dir.path().join(GC_PID))
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert_eq!(running_git_gc_in(dir.path()).unwrap(), None);
    }

    #[test]
    fn waiting_returns_once_the_collection_finishes() {
        let dir = tempfile::tempdir().unwrap();
        write_record(
            dir.path(),
            &format!("{} {}", std::process::id(), this_host()),
        );
        let record = dir.path().join(GC_PID);
        let finisher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            fs::remove_file(record).unwrap();
        });
        let mut announced = Vec::new();
        let still = wait_for_running_git_gc_in(dir.path(), Duration::from_secs(30), &mut |r| {
            announced.push(r.pid)
        })
        .unwrap();
        finisher.join().unwrap();
        assert_eq!(still, None);
        assert_eq!(announced, vec![std::process::id()], "announced once");
    }

    #[test]
    fn waiting_gives_up_after_its_budget_and_names_the_collection() {
        let dir = tempfile::tempdir().unwrap();
        write_record(
            dir.path(),
            &format!("{} {}", std::process::id(), this_host()),
        );
        let started = Instant::now();
        let mut announcements = 0;
        let still = wait_for_running_git_gc_in(dir.path(), Duration::from_millis(600), &mut |_| {
            announcements += 1
        })
        .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(600));
        assert_eq!(still.map(|r| r.pid), Some(std::process::id()));
        assert_eq!(announcements, 1);
    }

    #[test]
    fn waiting_on_a_stale_record_does_not_wait() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), &format!("{} {}", dead_pid(), this_host()));
        let mut announcements = 0;
        let still = wait_for_running_git_gc_in(dir.path(), Duration::from_secs(30), &mut |_| {
            announcements += 1
        })
        .unwrap();
        assert_eq!(still, None);
        assert_eq!(announcements, 0);
    }

    #[test]
    fn a_live_process_on_this_host_is_running() {
        let dir = tempfile::tempdir().unwrap();
        write_record(
            dir.path(),
            &format!("{} {}", std::process::id(), this_host()),
        );
        let running = running_git_gc_in(dir.path()).unwrap().expect("running");
        assert_eq!(running.pid, std::process::id());
        assert!(running.sentence().contains("garbage collection"));
        assert!(running.remedy().contains("wait for it to finish"));
    }
}
