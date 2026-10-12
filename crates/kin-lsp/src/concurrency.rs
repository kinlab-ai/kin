// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! How many questions a pass keeps in flight to one language server, and how
//! it keeps what it proves independent of the order the answers arrive in.
//!
//! A pass asks its questions in a fixed order and folds each answer into what
//! it has proven in that same order. [`Ordered`] lets the questions after the
//! current one go out early, a bounded number at a time, and hands their
//! answers back in the order they were asked, whatever order the server
//! answered them in. The fold is the same code a sequential pass runs, so the
//! proofs, the failures it counts and the point at which it stops are the ones
//! the sequential pass reaches for the same answers.
//!
//! What a server can keep up with is its own. [`InFlightWindow`] starts at the
//! configured ceiling, halves when an answer that went out beside others timed
//! out or was cancelled by the server, and grows back by one after a run of
//! answers. A question that failed that way beside others is asked again with
//! nothing else in flight before its failure counts, because the sequential
//! pass would have asked it alone: load this pass put on the server is never
//! recorded as a question the server could not answer.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use futures_util::stream::{FuturesOrdered, StreamExt};

/// The ceiling a server gets when nobody configures one: one question at a
/// time, as every pass asked before concurrency existed.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 1;

/// The largest ceiling a configuration may set. Past this a server gains
/// nothing, and a pass would hold that many answers in memory at once.
pub const MAX_IN_FLIGHT_CEILING: usize = 64;

/// How many answers a pass may hold that the fold has not reached yet, as a
/// multiple of the ceiling. It bounds memory while one slow answer at the head
/// holds the fold back.
const READY_PER_SLOT: usize = 4;

/// How many questions one server is asked at once.
///
/// Additive increase and multiplicative decrease: the window starts at the
/// ceiling, [`Self::congested`] halves it, and each run of as many answers as
/// the window holds widens it by one again, never past the ceiling.
#[derive(Debug)]
pub struct InFlightWindow {
    ceiling: AtomicUsize,
    window: AtomicUsize,
    answers: AtomicUsize,
}

impl Default for InFlightWindow {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_IN_FLIGHT)
    }
}

impl InFlightWindow {
    /// A window of `ceiling` questions, clamped to `1..=MAX_IN_FLIGHT_CEILING`.
    pub fn new(ceiling: usize) -> Self {
        let ceiling = ceiling.clamp(1, MAX_IN_FLIGHT_CEILING);
        Self {
            ceiling: AtomicUsize::new(ceiling),
            window: AtomicUsize::new(ceiling),
            answers: AtomicUsize::new(0),
        }
    }

    /// The configured ceiling.
    pub fn ceiling(&self) -> usize {
        self.ceiling.load(Ordering::Relaxed)
    }

    /// Set the ceiling, clamped to `1..=MAX_IN_FLIGHT_CEILING`, and open the
    /// window to it.
    pub fn set_ceiling(&self, ceiling: usize) {
        let ceiling = ceiling.clamp(1, MAX_IN_FLIGHT_CEILING);
        self.ceiling.store(ceiling, Ordering::Relaxed);
        self.window.store(ceiling, Ordering::Relaxed);
        self.answers.store(0, Ordering::Relaxed);
    }

    /// How many questions may be in flight now.
    pub fn current(&self) -> usize {
        self.window.load(Ordering::Relaxed).max(1)
    }

    /// One question was answered. A run of as many answers as the window
    /// holds widens it by one, up to the ceiling.
    pub fn answered(&self) {
        let window = self.current();
        if window >= self.ceiling() {
            return;
        }
        if self.answers.fetch_add(1, Ordering::Relaxed) + 1 >= window {
            self.answers.store(0, Ordering::Relaxed);
            let _ = self.window.compare_exchange(
                window,
                (window + 1).min(self.ceiling()),
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
    }

    /// A question asked beside others timed out or was turned away by the
    /// server: halve the window.
    pub fn congested(&self) {
        let window = self.current();
        let halved = (window / 2).max(1);
        if halved < window {
            tracing::debug!(
                window,
                halved,
                "a language server fell behind; fewer questions go to it at once"
            );
        }
        self.window.store(halved, Ordering::Relaxed);
        self.answers.store(0, Ordering::Relaxed);
    }
}

/// Answers to questions that went out ahead of the fold, handed back in the
/// order the questions were asked.
///
/// `start` turns the next item into the future that asks about it. At most
/// the window's worth run at once, counting any follow-up question the fold
/// itself is waiting on (see [`Ordered::beside`]), and at most
/// [`READY_PER_SLOT`] times the ceiling are held, running or answered, before
/// the fold takes them. Dropping it drops every future still running, which
/// cancels its request with the server.
pub(crate) struct Ordered<I: Iterator, F, Fut: Future> {
    items: I,
    start: F,
    running: FuturesOrdered<Fut>,
    /// The window each running future was started under, in start order.
    started_under: VecDeque<usize>,
    ready: VecDeque<(Fut::Output, usize)>,
    /// Questions the fold is asking itself while this pumps, which count
    /// against the window.
    beside: usize,
    hold: usize,
}

/// One answer, with the window its question went out under. A question that
/// went out under a window of one was asked alone.
pub(crate) struct Issued<T> {
    pub(crate) answer: T,
    pub(crate) window: usize,
}

impl<T> Issued<T> {
    /// Whether the question went out with nothing else allowed in flight.
    pub(crate) fn alone(&self) -> bool {
        self.window <= 1
    }

    /// The same answer, reshaped, under the same window.
    pub(crate) fn map<U>(self, reshape: impl FnOnce(T) -> U) -> Issued<U> {
        Issued {
            answer: reshape(self.answer),
            window: self.window,
        }
    }
}

/// A follow-up's output, and whether another question was in flight while it
/// ran.
pub(crate) struct Beside<T> {
    pub(crate) output: T,
    pub(crate) overlapped: bool,
}

impl<I, F, Fut> Ordered<I, F, Fut>
where
    I: Iterator,
    F: FnMut(I::Item) -> Fut,
    Fut: Future,
{
    pub(crate) fn new(items: I, start: F, ceiling: usize) -> Self {
        Self {
            items,
            start,
            running: FuturesOrdered::new(),
            started_under: VecDeque::new(),
            ready: VecDeque::new(),
            beside: 0,
            hold: ceiling.clamp(1, MAX_IN_FLIGHT_CEILING) * READY_PER_SLOT,
        }
    }

    /// Start questions until the window, less `reserve` slots kept free,
    /// is full, or enough answers are held. Each is recorded with the window
    /// it started under, which says whether it went out alone.
    fn fill(&mut self, window: &InFlightWindow, reserve: usize) {
        let now = window.current();
        let limit = now.saturating_sub(reserve);
        while self.running.len() + self.beside < limit
            && self.running.len() + self.ready.len() < self.hold
        {
            let Some(item) = self.items.next() else {
                return;
            };
            self.running.push_back((self.start)(item));
            self.started_under.push_back(now);
        }
    }

    /// The answer to the next question in asking order, `None` when every
    /// item was answered.
    ///
    /// Once the answer is handed back, one slot of the window stays free for
    /// a follow-up the fold may ask about it, so a window of one never has a
    /// question in flight while the fold asks one of its own.
    pub(crate) async fn next(&mut self, window: &InFlightWindow) -> Option<Issued<Fut::Output>> {
        let (answer, window_then) = match self.ready.pop_front() {
            Some(held) => held,
            None => {
                self.fill(window, 0);
                let answer = self.running.next().await?;
                (answer, self.started_under.pop_front().unwrap_or(1))
            }
        };
        self.fill(window, 1);
        Some(Issued {
            answer,
            window: window_then,
        })
    }

    /// Wait for `follow_up`, a question the fold asks itself, while the
    /// questions after it keep going out and their answers are held. The
    /// follow-up counts against the window, so a window of one runs it alone.
    /// Returns its output and whether anything else was in flight beside it.
    pub(crate) async fn beside<G: Future>(
        &mut self,
        window: &InFlightWindow,
        follow_up: G,
    ) -> Beside<G::Output> {
        self.beside += 1;
        let mut overlapped = false;
        let output = {
            tokio::pin!(follow_up);
            loop {
                self.fill(window, 0);
                if self.running.is_empty() {
                    break follow_up.await;
                }
                overlapped = true;
                tokio::select! {
                    biased;
                    output = &mut follow_up => break output,
                    Some(answer) = self.running.next() => {
                        let window_then = self.started_under.pop_front().unwrap_or(1);
                        self.ready.push_back((answer, window_then));
                    }
                }
            }
        };
        self.beside -= 1;
        Beside { output, overlapped }
    }

    /// Wait for every question still running and hold its answer, so the
    /// next question the fold asks goes out alone.
    pub(crate) async fn drain(&mut self) {
        while let Some(answer) = self.running.next().await {
            let window_then = self.started_under.pop_front().unwrap_or(1);
            self.ready.push_back((answer, window_then));
        }
    }
}

/// What the fold needs to know about an answer to decide whether to use it.
pub(crate) trait Answer {
    /// Whether it is a failure more load can cause (see
    /// [`crate::error::LspError::under_load`]).
    fn under_load(&self) -> bool;
    /// Whether the server answered, which widens the window.
    fn answered(&self) -> bool;
}

impl<T> Answer for crate::error::Result<T> {
    fn under_load(&self) -> bool {
        self.as_ref().is_err_and(crate::error::LspError::under_load)
    }

    fn answered(&self) -> bool {
        self.is_ok()
    }
}

/// An answer, with how many documents the pass had opened when its question
/// went out (see `ScopedDocuments::opened`).
pub(crate) struct Stamped<A> {
    pub(crate) opened: u64,
    pub(crate) answer: A,
}

/// Ask a question with the pass's current count of opened documents stamped
/// on its answer.
pub(crate) async fn stamped<A>(
    opened: &AtomicU64,
    question: impl Future<Output = A>,
) -> Stamped<A> {
    let opened = opened.load(Ordering::Acquire);
    Stamped {
        opened,
        answer: question.await,
    }
}

/// The answer the fold may use for a question that went out ahead of it.
///
/// Asked again, by `again`, when the pass opened a document after it went
/// out, since the sequential pass would have asked it with that document
/// open; and asked again alone, after every question still running has
/// finished, when it failed under load beside others, since the sequential
/// pass would have asked it alone. Only then does a failure count.
pub(crate) async fn settle<I, F, Fut, A, G, GFut>(
    window: &InFlightWindow,
    opened: &AtomicU64,
    asks: &mut Ordered<I, F, Fut>,
    issued: Issued<Stamped<A>>,
    mut again: G,
) -> A
where
    I: Iterator,
    F: FnMut(I::Item) -> Fut,
    Fut: Future,
    A: Answer,
    G: FnMut() -> GFut,
    GFut: Future<Output = Stamped<A>>,
{
    let mut alone = issued.alone();
    let mut held = issued.answer;
    loop {
        if held.opened != opened.load(Ordering::Acquire) {
            let asked = asks.beside(window, again()).await;
            alone = !asked.overlapped;
            held = asked.output;
            continue;
        }
        if held.answer.under_load() && !alone {
            window.congested();
            asks.drain().await;
            held = again().await;
            alone = true;
            continue;
        }
        if held.answer.answered() {
            window.answered();
        }
        return held.answer;
    }
}

/// Ask a follow-up question beside the questions going out ahead of the
/// fold, and when it failed under load beside them, ask it again alone.
pub(crate) async fn follow_up<I, F, Fut, A, G, GFut>(
    window: &InFlightWindow,
    asks: &mut Ordered<I, F, Fut>,
    mut ask: G,
) -> A
where
    I: Iterator,
    F: FnMut(I::Item) -> Fut,
    Fut: Future,
    A: Answer,
    G: FnMut() -> GFut,
    GFut: Future<Output = A>,
{
    let asked = asks.beside(window, ask()).await;
    if asked.overlapped && asked.output.under_load() {
        window.congested();
        asks.drain().await;
        return ask().await;
    }
    if asked.output.answered() {
        window.answered();
    }
    asked.output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn the_window_halves_under_congestion_and_widens_back_to_its_ceiling() {
        let window = InFlightWindow::new(8);
        assert_eq!(window.current(), 8);
        window.congested();
        assert_eq!(window.current(), 4);
        window.congested();
        window.congested();
        window.congested();
        assert_eq!(window.current(), 1, "never below one");
        // One answer widens a window of one; two widen a window of two.
        window.answered();
        assert_eq!(window.current(), 2);
        window.answered();
        assert_eq!(window.current(), 2);
        window.answered();
        assert_eq!(window.current(), 3);
        for _ in 0..100 {
            window.answered();
        }
        assert_eq!(window.current(), 8, "never past the ceiling");
        window.set_ceiling(0);
        assert_eq!(window.current(), 1, "a ceiling of zero is one");
        window.set_ceiling(10_000);
        assert_eq!(window.ceiling(), MAX_IN_FLIGHT_CEILING);
    }

    /// Answers come back in asking order however the futures finish, and no
    /// more than the window run at once.
    #[tokio::test]
    async fn answers_come_back_in_asking_order_within_the_window() {
        let running = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let window = InFlightWindow::new(4);
        let delays = [40u64, 5, 30, 1, 20, 2, 10, 3, 15, 4];
        let mut ordered = Ordered::new(
            delays.iter().copied().enumerate(),
            |(index, delay)| {
                let (running, most) = (Arc::clone(&running), Arc::clone(&most));
                async move {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    index
                }
            },
            window.ceiling(),
        );
        let mut seen = Vec::new();
        while let Some(issued) = ordered.next(&window).await {
            assert_eq!(issued.window, 4);
            seen.push(issued.answer);
        }
        assert_eq!(seen, (0..delays.len()).collect::<Vec<_>>());
        assert_eq!(most.load(Ordering::SeqCst), 4);
    }

    /// A follow-up the fold waits on takes a slot of the window, the
    /// questions after it keep going, and a window of one runs it alone.
    #[tokio::test]
    async fn a_follow_up_takes_a_slot_and_runs_alone_under_a_window_of_one() {
        for ceiling in [1usize, 3] {
            let running = Arc::new(AtomicUsize::new(0));
            let most = Arc::new(AtomicUsize::new(0));
            let window = InFlightWindow::new(ceiling);
            let track = |running: Arc<AtomicUsize>, most: Arc<AtomicUsize>, delay: u64| async move {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                running.fetch_sub(1, Ordering::SeqCst);
            };
            let mut ordered = Ordered::new(
                0..6usize,
                |index| {
                    let task = track(Arc::clone(&running), Arc::clone(&most), 5);
                    async move {
                        task.await;
                        index
                    }
                },
                window.ceiling(),
            );
            let mut seen = Vec::new();
            let mut overlapped = Vec::new();
            while let Some(issued) = ordered.next(&window).await {
                seen.push(issued.answer);
                let beside = ordered
                    .beside(&window, track(Arc::clone(&running), Arc::clone(&most), 10))
                    .await;
                overlapped.push(beside.overlapped);
            }
            assert_eq!(seen, (0..6).collect::<Vec<_>>());
            assert_eq!(most.load(Ordering::SeqCst), ceiling, "ceiling {ceiling}");
            if ceiling == 1 {
                assert!(overlapped.iter().all(|overlapped| !overlapped));
            } else {
                assert!(
                    overlapped[0],
                    "the questions after the first go out beside it"
                );
            }
        }
    }

    /// While the fold waits on a slow follow-up, the questions after it keep
    /// going out only until the answers held reach the bound, so a fold stuck
    /// on one answer never holds the whole file's.
    #[tokio::test]
    async fn a_slow_fold_holds_a_bounded_number_of_answers() {
        let started = Arc::new(AtomicUsize::new(0));
        let window = InFlightWindow::new(2);
        let mut ordered = Ordered::new(
            0..1_000usize,
            |index| {
                started.fetch_add(1, Ordering::SeqCst);
                async move { index }
            },
            window.ceiling(),
        );
        assert_eq!(
            ordered.next(&window).await.map(|issued| issued.answer),
            Some(0)
        );
        let beside = ordered
            .beside(&window, tokio::time::sleep(Duration::from_millis(50)))
            .await;
        assert!(beside.overlapped);
        let hold = window.ceiling() * READY_PER_SLOT;
        let started_now = started.load(Ordering::SeqCst);
        assert!(started_now > 2, "questions kept going out: {started_now}");
        assert!(
            started_now <= 1 + hold,
            "started {started_now}, hold {hold}"
        );
        let mut next = 1;
        while let Some(issued) = ordered.next(&window).await {
            assert_eq!(issued.answer, next);
            next += 1;
        }
        assert_eq!(next, 1_000);
    }

    /// Dropping the pipeline drops what is still running.
    #[tokio::test]
    async fn dropping_the_pipeline_cancels_what_is_running() {
        let finished = Arc::new(AtomicUsize::new(0));
        let window = InFlightWindow::new(8);
        let mut ordered = Ordered::new(
            0..8usize,
            |index| {
                let finished = Arc::clone(&finished);
                async move {
                    if index > 0 {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                    finished.fetch_add(1, Ordering::SeqCst);
                    index
                }
            },
            window.ceiling(),
        );
        assert_eq!(
            ordered.next(&window).await.map(|issued| issued.answer),
            Some(0)
        );
        drop(ordered);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(finished.load(Ordering::SeqCst), 1);
    }
}
