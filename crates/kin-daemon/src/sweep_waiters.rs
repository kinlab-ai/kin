// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Commands waiting to read a language-server sweep's final status.
//!
//! `kin daemon sweep` asks for a sweep and then polls its status until it
//! reads one that says the sweep it waited on has ended. A daemon asked to
//! retire used to exit on the first idle check after the sweep ended, which
//! comes well inside the command's poll interval, so the command's next read
//! found no daemon and failed over a sweep that had succeeded. Measured on
//! React: the daemon logged the completed sweep, retired 0.3 s later, and the
//! command reported "Error: read language-server sweep progress".
//!
//! A waiter registers with the count of completed sweeps it must see exceeded,
//! and is released by the read that shows it exceeded with nothing running.
//! Until then it keeps the daemon up, but only while its process lives and it
//! keeps polling: a waiter that crashed or stopped asking keeps nothing up.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a waiter may go without reading the status before it no longer
/// keeps the daemon up. Many times the command's poll interval, so a command
/// slowed by its host is still waited for, and short enough that one that
/// hung costs a retiring daemon a minute at most.
pub(crate) const WAITER_LAPSE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
struct Waiter {
    /// The `sweeps_completed` count this waiter must see exceeded.
    after: u64,
    /// When it last asked.
    seen: Instant,
}

/// Every command waiting on a sweep's final status, by process id.
#[derive(Debug, Default)]
pub struct SweepWaiters {
    waiters: Mutex<HashMap<u32, Waiter>>,
}

impl SweepWaiters {
    /// Register `pid` as waiting for a sweep that ends after `after` sweeps
    /// have completed. A pid already waiting keeps the lower count, which is
    /// the earlier sweep it is still owed.
    pub(crate) fn register(&self, pid: u32, after: u64) {
        let mut waiters = self
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        waiters
            .entry(pid)
            .and_modify(|waiter| {
                waiter.after = waiter.after.min(after);
                waiter.seen = now;
            })
            .or_insert(Waiter { after, seen: now });
    }

    /// `pid` read a status showing `completed` sweeps and whether one is
    /// `running`. The read that shows its sweep ended releases it; any other
    /// keeps it waiting. A pid that never registered is not tracked.
    pub(crate) fn read(&self, pid: u32, completed: u64, running: bool) {
        let mut waiters = self
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(waiter) = waiters.get_mut(&pid) else {
            return;
        };
        if completed > waiter.after && !running {
            waiters.remove(&pid);
        } else {
            waiter.seen = Instant::now();
        }
    }

    /// Whether a live command is still waiting to read a sweep's final status.
    /// Forgets every waiter whose process is gone or that stopped asking.
    pub(crate) fn any_waiting(&self, alive: impl Fn(u32) -> bool) -> bool {
        let mut waiters = self
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        waiters.retain(|pid, waiter| waiter.seen.elapsed() < WAITER_LAPSE && alive(*pid));
        !waiters.is_empty()
    }
}

/// The optional `waiter` a sweep request or a status read names: the process
/// id of a command that waits on the sweep's final status. A request without
/// one is served exactly as before.
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct SweepWaiterQuery {
    pub(crate) waiter: Option<u32>,
}

/// Whether the process `pid` is running on this host.
pub(crate) fn process_alive(pid: u32) -> bool {
    kin_cli::daemon_client::is_process_alive(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_waiter_holds_until_it_reads_the_sweep_it_waited_on_end() {
        let waiters = SweepWaiters::default();
        assert!(!waiters.any_waiting(|_| true));
        waiters.register(7, 3);
        assert!(waiters.any_waiting(|_| true));
        // Still running, or the count not yet past: still waiting.
        waiters.read(7, 3, false);
        waiters.read(7, 4, true);
        assert!(waiters.any_waiting(|_| true));
        // Another process's read releases nothing.
        waiters.read(8, 4, false);
        assert!(waiters.any_waiting(|_| true));
        // The final read releases it.
        waiters.read(7, 4, false);
        assert!(!waiters.any_waiting(|_| true));
    }

    #[test]
    fn a_waiter_whose_process_is_gone_holds_nothing() {
        let waiters = SweepWaiters::default();
        waiters.register(7, 0);
        assert!(!waiters.any_waiting(|pid| pid != 7));
        // Forgotten for good, even if the pid comes back.
        assert!(!waiters.any_waiting(|_| true));
    }
}
