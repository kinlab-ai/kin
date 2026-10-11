// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Shared bounded-subprocess support for the CLI integration tests.
//!
//! Every process an integration test drives — `kin`, `kin-daemon`, `cargo`,
//! `rustc` — waits on something the machine may not be able to supply: a daemon
//! that never becomes ready, a cargo build-directory lock another checkout
//! holds, a model that is not downloaded. `Command::output()` waits on all of
//! them without a deadline and with this process's stdin attached, so the
//! default test suite can block forever while printing nothing. Every spawn
//! here closes stdin and carries a wall-clock cap, so the suite always reaches
//! a verdict and names the process that did not finish.

// Each integration test binary includes this module and uses a different
// subset of it.
#![allow(dead_code)]

// Worker entrypoints stay in every integration binary that launches them.
include!("support.rs");
include!("prepared.rs");
include!("native_init.rs");
