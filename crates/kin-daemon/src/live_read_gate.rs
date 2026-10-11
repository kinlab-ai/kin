// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Reads of the live graph and language-server enrichment writes take turns.
//!
//! A read that claims an absence, and every analysis read, is served only from
//! an attempt no graph-authority writer spanned. The enrichment sweep is such a
//! writer, and it writes in short bursts for minutes on end: one unit's proven
//! edges, call-site ledgers and settled guesses, then the next unit once the
//! language server answers. A read that takes longer than the gap between two
//! bursts was spanned by one on every attempt, waited out its whole writer
//! limit and refused with 503, although nothing it read was half written and
//! the sweep it waited for would not finish for minutes.
//!
//! The turns close that window without making either side wait on the other's
//! slow work.
//!
//! A read attempt of the live graph holds a [`LiveReadPass`]. Passes are held
//! per task: a read that runs inside another read in the same task, as an MCP
//! tool's own read runs inside the dispatcher's analysis attempt, or a paged
//! answer runs the tool it pages, shares the outer read's hold and never waits
//! to begin. Before any wait for a writer, and before any nap between
//! attempts, the task lets its whole hold go ([`LiveReadGate::release_current`]),
//! outer passes included, so a read that waits never holds the sweep back,
//! however deeply it is nested. The task's next attempt takes the hold again,
//! and an outer read's own certification sees any write that landed meanwhile
//! and reads again.
//!
//! An enrichment write takes its turn through an [`EnrichmentTurnWait`]: it
//! waits, holding nothing, until no read attempt is in flight, takes the
//! coordination gate, and claims the turn if still none is, or gives the gate
//! straight back and waits again. It never holds the coordination gate while
//! it waits for reads, so a reconcile pass or commit queued behind it is never
//! held up by them.
//!
//! A read waits while a write lands. Once a write has waited
//! [`ENRICHMENT_WRITE_PATIENCE`] for its turn, new read attempts also queue
//! behind it until it lands, so it waits only for the attempts already in
//! flight and overlapping reads cannot hold the sweep off. While the write
//! waits for the coordination gate it stands aside, so a read that whoever
//! holds the gate is waiting on never queues behind a write that is waiting
//! on that same holder. A queued read stops queueing after
//! [`QUEUED_READ_LIMIT`], the bound for a dependency the gate cannot see,
//! such as a read one task waits on from another.
//!
//! So:
//!
//! - no enrichment write lands inside a read attempt, and the attempt's own
//!   certification holds without waiting for the sweep to end;
//! - a read waits at most for the write landing now, or, once a write has
//!   used its patience, for the attempts already in flight and that one write,
//!   never for the sweep's language-server queries or the sweep as a whole;
//! - a write is passed over by new reads for at most its patience, then lands
//!   as soon as the attempts already in flight end;
//! - a write holds the coordination gate only to claim its turn and while it
//!   lands, never while it waits for reads.
//!
//! What the sweep has not reached yet is still owed, and every answer read in
//! between says so through its call-site readings, as it did before.
//!
//! Only the live graph takes turns. A session's temporal scope is a historical
//! graph no enrichment writes, so its reads hold no pass.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

/// How long an enrichment write waiting for its turn lets new read attempts
/// begin ahead of it. Past it, new attempts queue behind the write until it
/// lands, and it waits only for the attempts already in flight.
pub(crate) const ENRICHMENT_WRITE_PATIENCE: Duration = Duration::from_millis(250);

/// The longest a new read attempt queues behind an enrichment write that has
/// used its patience. After it the read begins anyway.
///
/// A queued write lands as soon as the attempts already in flight end, so a
/// read reaches this limit only behind an attempt that itself runs this long,
/// or when the write waits on a read through a task the gate cannot see.
pub(crate) const QUEUED_READ_LIMIT: Duration = Duration::from_secs(10);

/// The turn-taking state between live reads and enrichment writes.
#[derive(Debug, Default)]
pub(crate) struct LiveReadGate {
    state: Mutex<GateState>,
    /// Woken whenever a hold is let go, a write lands, or a write stops
    /// holding new reads back.
    changed: Notify,
}

/// Who holds a read: the Tokio task running it, or, outside any task, the
/// thread blocking on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Holder {
    Task(tokio::task::Id),
    Thread(std::thread::ThreadId),
}

impl Holder {
    fn current() -> Self {
        match tokio::task::try_id() {
            Some(id) => Self::Task(id),
            None => Self::Thread(std::thread::current().id()),
        }
    }
}

/// One holder's passes.
#[derive(Debug, Default)]
struct Hold {
    /// Passes alive, outer and nested.
    passes: usize,
    /// Whether the holder's attempt is in flight now. False after it let its
    /// hold go to wait, until its next attempt takes it again.
    held: bool,
}

#[derive(Debug, Default)]
struct GateState {
    holds: HashMap<Holder, Hold>,
    /// Holders whose attempt is in flight.
    readers: usize,
    /// An enrichment write is landing. New attempts wait for it.
    writing: bool,
    /// Enrichment writes that have waited past their patience. While any
    /// does, new attempts queue behind them.
    overdue: usize,
}

impl GateState {
    /// Let `holder`'s hold go, keeping its passes alive. Whether it was held.
    fn release(&mut self, holder: Holder) -> bool {
        match self.holds.get_mut(&holder) {
            Some(hold) if hold.held => {
                hold.held = false;
                self.readers = self
                    .readers
                    .checked_sub(1)
                    .expect("live read hold count underflow");
                true
            }
            _ => false,
        }
    }
}

/// One read attempt's hold on the live graph. While its holder's hold is held,
/// no enrichment write lands.
#[derive(Debug)]
#[must_use = "a pass protects the read attempt only while it is held"]
pub(crate) struct LiveReadPass {
    gate: Arc<LiveReadGate>,
    holder: Holder,
}

impl Drop for LiveReadPass {
    fn drop(&mut self) {
        let released = {
            let mut guard = self.gate.lock();
            let state = &mut *guard;
            let hold = state
                .holds
                .get_mut(&self.holder)
                .expect("a live read pass outlived its hold");
            hold.passes = hold
                .passes
                .checked_sub(1)
                .expect("live read pass count underflow");
            if hold.passes > 0 {
                false
            } else {
                let held = hold.held;
                state.holds.remove(&self.holder);
                if held {
                    state.readers = state
                        .readers
                        .checked_sub(1)
                        .expect("live read hold count underflow");
                }
                held
            }
        };
        if released {
            self.gate.changed.notify_waiters();
        }
    }
}

/// One enrichment write's turn: no read attempt is in flight while it is held.
#[derive(Debug)]
#[must_use = "the turn excludes reads only while it is held"]
pub(crate) struct EnrichmentExclusion {
    gate: Arc<LiveReadGate>,
}

impl Drop for EnrichmentExclusion {
    fn drop(&mut self) {
        self.gate.lock().writing = false;
        self.gate.changed.notify_waiters();
    }
}

/// An enrichment write waiting for its turn. Dropping it, by landing or by
/// being cancelled, stops it holding new reads back.
#[derive(Debug)]
pub(crate) struct EnrichmentTurnWait {
    gate: Arc<LiveReadGate>,
    patience_ends: Instant,
    /// Whether this write counts in [`GateState::overdue`] now.
    overdue: bool,
}

impl LiveReadGate {
    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Begin a read attempt.
    ///
    /// Inside an attempt this task already has in flight, the pass shares that
    /// hold and is given at once. Otherwise it waits while an enrichment write
    /// lands, and queues behind a write that has used its patience, for at
    /// most [`QUEUED_READ_LIMIT`].
    pub(crate) async fn enter_read(self: &Arc<Self>) -> LiveReadPass {
        let holder = Holder::current();
        let queue_limit = Instant::now() + QUEUED_READ_LIMIT;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // Registered before the check, so a change between the check and
            // the wait still wakes this one.
            changed.as_mut().enable();
            let queued = {
                let mut guard = self.lock();
                let state = &mut *guard;
                let nested = state.holds.get(&holder).is_some_and(|hold| hold.held);
                let queued = !nested && state.overdue > 0 && Instant::now() < queue_limit;
                if nested || (!state.writing && !queued) {
                    let hold = state.holds.entry(holder).or_default();
                    hold.passes = hold
                        .passes
                        .checked_add(1)
                        .expect("live read pass count exhausted");
                    if !hold.held {
                        hold.held = true;
                        state.readers += 1;
                    }
                    return LiveReadPass {
                        gate: Arc::clone(self),
                        holder,
                    };
                }
                queued
            };
            if queued {
                let _ = tokio::time::timeout_at(queue_limit, changed).await;
            } else {
                changed.await;
            }
        }
    }

    /// Let the current task's hold go before it waits, outer passes included,
    /// so no enrichment write waits on a read that is itself waiting. Its next
    /// attempt takes the hold again. Nothing happens when it holds none.
    pub(crate) fn release_current(&self) {
        let released = self.lock().release(Holder::current());
        if released {
            self.changed.notify_waiters();
        }
    }

    /// Begin waiting for an enrichment write's turn.
    pub(crate) fn wait_for_turn(self: &Arc<Self>) -> EnrichmentTurnWait {
        debug_assert!(
            !self
                .lock()
                .holds
                .get(&Holder::current())
                .is_some_and(|hold| hold.held),
            "an enrichment write waits for the read attempts in flight, so it \
             must not wait inside one of its own task's"
        );
        EnrichmentTurnWait {
            gate: Arc::clone(self),
            patience_ends: Instant::now() + ENRICHMENT_WRITE_PATIENCE,
            overdue: false,
        }
    }

    /// Read attempts in flight, for tests.
    #[cfg(test)]
    pub(crate) fn readers_in_flight(&self) -> usize {
        self.lock().readers
    }
}

impl EnrichmentTurnWait {
    /// Wait, holding nothing, until no read attempt is in flight. Once this
    /// write's patience has run out, new attempts queue behind it.
    pub(crate) async fn reads_drained(&mut self) {
        loop {
            let changed = self.gate.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self.gate.lock();
                if !self.overdue && Instant::now() >= self.patience_ends {
                    state.overdue += 1;
                    self.overdue = true;
                }
                if state.readers == 0 {
                    return;
                }
            }
            if self.overdue {
                changed.await;
            } else {
                let _ = tokio::time::timeout_at(self.patience_ends, changed).await;
            }
        }
    }

    /// Stop holding new reads back while this write waits for something other
    /// than reads, such as the coordination gate. The next
    /// [`Self::reads_drained`] holds them back again.
    pub(crate) fn stand_aside(&mut self) {
        if !self.overdue {
            return;
        }
        self.overdue = false;
        {
            let mut state = self.gate.lock();
            state.overdue = state
                .overdue
                .checked_sub(1)
                .expect("overdue enrichment write count underflow");
        }
        self.gate.changed.notify_waiters();
    }

    /// Take the turn if no read attempt is in flight. Call it holding the
    /// coordination gate, which serializes enrichment writes.
    pub(crate) fn claim(&mut self) -> Option<EnrichmentExclusion> {
        let mut state = self.gate.lock();
        debug_assert!(
            !state.writing,
            "enrichment writes are serialized by the coordination gate"
        );
        if state.readers != 0 {
            return None;
        }
        state.writing = true;
        if self.overdue {
            // New reads now wait for the write itself.
            self.overdue = false;
            state.overdue = state
                .overdue
                .checked_sub(1)
                .expect("overdue enrichment write count underflow");
        }
        Some(EnrichmentExclusion {
            gate: Arc::clone(&self.gate),
        })
    }
}

impl Drop for EnrichmentTurnWait {
    fn drop(&mut self) {
        self.stand_aside();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> Arc<LiveReadGate> {
        Arc::new(LiveReadGate::default())
    }

    /// A write's turn as `DaemonState::coordinate_enrichment_write` takes it,
    /// with no coordination gate to contend for.
    async fn turn(gate: &Arc<LiveReadGate>) -> EnrichmentExclusion {
        let mut wait = gate.wait_for_turn();
        loop {
            wait.reads_drained().await;
            if let Some(exclusion) = wait.claim() {
                return exclusion;
            }
        }
    }

    fn spawn_turn(gate: &Arc<LiveReadGate>) -> tokio::task::JoinHandle<EnrichmentExclusion> {
        let gate = Arc::clone(gate);
        tokio::spawn(async move { turn(&gate).await })
    }

    #[tokio::test]
    async fn a_write_with_no_read_in_flight_takes_its_turn_at_once() {
        let gate = gate();
        let landed = tokio::time::timeout(Duration::from_secs(5), turn(&gate))
            .await
            .expect("no read is in flight");
        drop(landed);
        let pass = gate.enter_read().await;
        assert_eq!(gate.readers_in_flight(), 1);
        drop(pass);
        assert_eq!(gate.readers_in_flight(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_write_waits_for_the_attempt_in_flight_and_then_lands() {
        let gate = gate();
        let pass = gate.enter_read().await;
        let writer = spawn_turn(&gate);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !writer.is_finished(),
            "the write must not land inside a read"
        );
        drop(pass);
        let landed = tokio::time::timeout(Duration::from_secs(5), writer)
            .await
            .expect("the write lands once the read ends")
            .unwrap();
        // A read that begins while the write lands waits for it.
        let reader = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { drop(gate.enter_read().await) })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !reader.is_finished(),
            "a read must not begin inside a write"
        );
        drop(landed);
        tokio::time::timeout(Duration::from_secs(5), reader)
            .await
            .expect("the read begins once the write lands")
            .unwrap();
    }

    /// Inside the patience a new read begins ahead of the waiting write. Past
    /// it, new reads queue behind the write, which lands once the attempt in
    /// flight ends, and the queued read begins once it has landed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn new_reads_pass_a_waiting_write_only_for_its_patience() {
        let gate = gate();
        let first = gate.enter_read().await;
        let writer = spawn_turn(&gate);
        tokio::time::sleep(Duration::from_millis(20)).await;
        // From another task: this one's own reads share its hold.
        let early = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { drop(gate.enter_read().await) })
        };
        tokio::time::timeout(Duration::from_millis(100), early)
            .await
            .expect("a read inside the write's patience begins ahead of it")
            .unwrap();
        tokio::time::sleep(ENRICHMENT_WRITE_PATIENCE * 2).await;
        let queued = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { drop(gate.enter_read().await) })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !queued.is_finished(),
            "a read after the write's patience queues behind it"
        );
        assert!(
            !writer.is_finished(),
            "the write waits for the read in flight"
        );
        drop(first);
        let landed = tokio::time::timeout(Duration::from_secs(5), writer)
            .await
            .expect("the write lands once the read in flight ends")
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!queued.is_finished(), "the queued read waits for the write");
        drop(landed);
        tokio::time::timeout(Duration::from_secs(5), queued)
            .await
            .expect("the queued read begins once the write lands")
            .unwrap();
    }

    /// Reads that overlap without a gap held the old turn off for as long as
    /// they kept coming. The write now lands within its patience plus one
    /// read, and the reads go on after it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn overlapping_reads_do_not_starve_a_write() {
        let gate = gate();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut readers = Vec::new();
        for offset in 0..3_u64 {
            let gate = Arc::clone(&gate);
            let stop = Arc::clone(&stop);
            let reads = Arc::clone(&reads);
            readers.push(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(7 * offset)).await;
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    let pass = gate.enter_read().await;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    drop(pass);
                    reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }));
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        for _ in 0..3 {
            let started = Instant::now();
            let landed = tokio::time::timeout(Duration::from_secs(3), turn(&gate))
                .await
                .expect("overlapping reads must not hold a write off");
            assert!(started.elapsed() < ENRICHMENT_WRITE_PATIENCE + Duration::from_secs(1));
            drop(landed);
            let before = reads.load(std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(80)).await;
            assert!(
                reads.load(std::sync::atomic::Ordering::SeqCst) > before,
                "reads go on after the write"
            );
        }
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        for reader in readers {
            reader.await.unwrap();
        }
        assert_eq!(gate.readers_in_flight(), 0);
    }

    /// A read nested inside another read's attempt in the same task, as the
    /// MCP dispatch runs a tool's own read inside its analysis attempt, shares
    /// the outer hold and begins at once, even behind a write that has used
    /// its patience.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_nested_read_shares_its_task_hold_and_never_queues() {
        let gate = gate();
        let outer = gate.enter_read().await;
        let writer = spawn_turn(&gate);
        tokio::time::sleep(ENRICHMENT_WRITE_PATIENCE * 2).await;
        let nested = tokio::time::timeout(Duration::from_millis(500), gate.enter_read())
            .await
            .expect("a nested read must not queue behind a write its own task holds off");
        assert_eq!(gate.readers_in_flight(), 1, "one hold for the task");
        drop(nested);
        assert!(
            !writer.is_finished(),
            "the outer attempt is still in flight"
        );
        drop(outer);
        drop(
            tokio::time::timeout(Duration::from_secs(5), writer)
                .await
                .expect("the write lands once the outer read ends")
                .unwrap(),
        );
    }

    /// A nested read that waits lets its task's whole hold go, the outer
    /// pass's included, so the write lands during the wait. The task's next
    /// attempt takes the hold again once the write has landed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_nested_read_that_waits_lets_the_whole_task_hold_go() {
        let gate = gate();
        let (waiting_tx, waiting_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel::<()>();
        let reader = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                let outer = gate.enter_read().await;
                let inner = gate.enter_read().await;
                // The inner read waits for a writer.
                gate.release_current();
                waiting_tx.send(()).unwrap();
                resume_rx.await.unwrap();
                // Its next attempt.
                drop(inner);
                let again = gate.enter_read().await;
                (outer, again)
            })
        };
        waiting_rx.await.unwrap();
        assert_eq!(
            gate.readers_in_flight(),
            0,
            "the waiting task holds nothing"
        );
        let landed = tokio::time::timeout(Duration::from_secs(2), turn(&gate))
            .await
            .expect("a waiting nested read must not hold the write off");
        resume_tx.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !reader.is_finished(),
            "the next attempt waits for the write landing"
        );
        drop(landed);
        let (outer, again) = tokio::time::timeout(Duration::from_secs(5), reader)
            .await
            .expect("the next attempt begins once the write lands")
            .unwrap();
        assert_eq!(gate.readers_in_flight(), 1);
        drop(again);
        assert_eq!(gate.readers_in_flight(), 1, "the outer pass keeps the hold");
        drop(outer);
        assert_eq!(gate.readers_in_flight(), 0);
    }

    /// A write waiting for the coordination gate stands aside, and the reads
    /// queued behind it begin.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_write_that_stands_aside_lets_queued_reads_in() {
        let gate = gate();
        let mut wait = gate.wait_for_turn();
        tokio::time::sleep(ENRICHMENT_WRITE_PATIENCE * 2).await;
        tokio::time::timeout(Duration::from_secs(1), wait.reads_drained())
            .await
            .expect("no read is in flight");
        let queued = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { drop(gate.enter_read().await) })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!queued.is_finished(), "the write has used its patience");
        wait.stand_aside();
        tokio::time::timeout(Duration::from_secs(5), queued)
            .await
            .expect("a write that stands aside holds no read back")
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_queued_read_begins_after_its_limit() {
        let gate = gate();
        let in_flight = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                let pass = gate.enter_read().await;
                tokio::time::sleep(QUEUED_READ_LIMIT * 3).await;
                drop(pass);
            })
        };
        tokio::task::yield_now().await;
        let writer = spawn_turn(&gate);
        tokio::time::sleep(ENRICHMENT_WRITE_PATIENCE * 2).await;
        let started = Instant::now();
        let queued = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.enter_read().await })
        };
        let pass = queued.await.unwrap();
        let queued_for = started.elapsed();
        assert!(queued_for >= QUEUED_READ_LIMIT, "{queued_for:?}");
        assert!(queued_for < QUEUED_READ_LIMIT * 2, "{queued_for:?}");
        drop(pass);
        in_flight.await.unwrap();
        drop(writer.await.unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_wait_for_a_turn_holds_no_read_back() {
        let gate = gate();
        let in_flight = gate.enter_read().await;
        let waiting = spawn_turn(&gate);
        tokio::time::sleep(ENRICHMENT_WRITE_PATIENCE * 2).await;
        waiting.abort();
        let _ = waiting.await;
        let after = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { drop(gate.enter_read().await) })
        };
        tokio::time::timeout(Duration::from_secs(1), after)
            .await
            .expect("a cancelled turn must not strand the reads it held back")
            .unwrap();
        drop(in_flight);
        assert_eq!(gate.readers_in_flight(), 0);
    }
}
