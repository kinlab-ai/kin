// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! How long a question to one language server may take before it counts as
//! unanswered, as that server's own recent answers say it is keeping up.
//!
//! Every pass states the allowance a question gets on a server that keeps up:
//! two seconds for a definition, ten for most requests, five for an entity's
//! query arm. On a loaded machine a server that is answering correctly answers
//! slower than that, and a fixed allowance turned its slowness into unanswered
//! questions: on a busy host the sweep of a 37-file Python repository gave up
//! on a third of its call sites. The allowance here grows with what the server
//! shows: a question that runs out doubles it for the questions after it, up
//! to [`MAX_SCALE`] times the stated allowance, and a run of answers that come
//! back well inside the stated allowance halves it again. A server that has
//! stopped answering still runs every allowance out, and a hung one is found
//! within the ceiling.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// The most any allowance is stretched: a two-second question may wait 32 s,
/// a ten-second request 160 s.
pub const MAX_SCALE: u32 = 16;

/// How many answers in a row, each inside a quarter of its stated allowance,
/// ease the stretch by half.
const QUICK_RUN: u32 = 32;

/// The stretch one server's questions get now.
#[derive(Debug)]
pub struct AnswerPace {
    scale: AtomicU32,
    quick: AtomicU32,
}

impl Default for AnswerPace {
    fn default() -> Self {
        Self {
            scale: AtomicU32::new(1),
            quick: AtomicU32::new(0),
        }
    }
}

impl AnswerPace {
    /// How many times its stated allowance a question gets now.
    pub fn scale(&self) -> u32 {
        self.scale.load(Ordering::Relaxed).clamp(1, MAX_SCALE)
    }

    /// The allowance a question stated as `base` gets now.
    pub fn allow(&self, base: Duration) -> Duration {
        base.saturating_mul(self.scale())
    }

    /// A question ran its allowance out: the questions after it get twice as
    /// long, up to the ceiling.
    pub fn timed_out(&self) {
        self.quick.store(0, Ordering::Relaxed);
        let before = self.scale();
        let after = before.saturating_mul(2).min(MAX_SCALE);
        if after != before {
            self.scale.store(after, Ordering::Relaxed);
            tracing::info!(
                scale = after,
                "a language server ran out a question's allowance; the questions after it get \
                 longer"
            );
        }
    }

    /// A question stated as `base` was answered after `took`.
    pub fn answered(&self, took: Duration, base: Duration) {
        if took > base / 4 {
            self.quick.store(0, Ordering::Relaxed);
            return;
        }
        if self.quick.fetch_add(1, Ordering::Relaxed) + 1 < QUICK_RUN {
            return;
        }
        self.quick.store(0, Ordering::Relaxed);
        let before = self.scale();
        if before > 1 {
            self.scale.store(before / 2, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO: Duration = Duration::from_secs(2);

    #[test]
    fn a_server_that_keeps_up_gets_the_stated_allowance() {
        let pace = AnswerPace::default();
        for _ in 0..100 {
            pace.answered(Duration::from_millis(3), TWO);
        }
        assert_eq!(pace.allow(TWO), TWO);
    }

    #[test]
    fn timeouts_stretch_the_allowance_up_to_the_ceiling() {
        let pace = AnswerPace::default();
        pace.timed_out();
        assert_eq!(pace.allow(TWO), 2 * TWO);
        for _ in 0..10 {
            pace.timed_out();
        }
        assert_eq!(pace.scale(), MAX_SCALE);
        assert_eq!(pace.allow(TWO), TWO * MAX_SCALE);
    }

    #[test]
    fn quick_answers_ease_the_stretch_and_slow_ones_do_not() {
        let pace = AnswerPace::default();
        pace.timed_out();
        pace.timed_out();
        assert_eq!(pace.scale(), 4);
        // Answers that took most of the stated allowance hold the stretch.
        for _ in 0..100 {
            pace.answered(Duration::from_millis(1900), TWO);
        }
        assert_eq!(pace.scale(), 4);
        for _ in 0..QUICK_RUN {
            pace.answered(Duration::from_millis(10), TWO);
        }
        assert_eq!(pace.scale(), 2);
        for _ in 0..QUICK_RUN - 1 {
            pace.answered(Duration::from_millis(10), TWO);
        }
        // One slow answer restarts the run.
        pace.answered(Duration::from_millis(1500), TWO);
        pace.answered(Duration::from_millis(10), TWO);
        assert_eq!(pace.scale(), 2);
        for _ in 0..2 * QUICK_RUN {
            pace.answered(Duration::from_millis(10), TWO);
        }
        assert_eq!(pace.scale(), 1);
    }
}
