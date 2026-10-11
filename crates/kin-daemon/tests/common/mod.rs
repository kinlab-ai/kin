// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Shared subprocess isolation for daemon integration tests.

// Worker entrypoints stay in every integration binary that launches them.
include!("support.rs");
include!("native_init.rs");
