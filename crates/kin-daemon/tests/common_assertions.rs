// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Verify shared integration helpers once while keeping worker entrypoints local.

mod common {
    include!("common/support.rs");
    include!("common/assertions.rs");
}
