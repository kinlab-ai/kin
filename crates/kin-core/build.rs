// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Takes the key of each product kin-core records, over the sources that
//! decide it.
//!
//! The `graph` key is the derivation identity a store's hydration record
//! names (`hydration_semantics::binary_derivation`). kin-derivation-inputs
//! starts at this crate's `derivation-inputs.toml`, follows every workspace
//! dependency classified as deciding the graph through its own classification,
//! and fails the build on any file or dependency nobody classified.

fn main() {
    kin_derivation_inputs::build_script(&["graph"]);
}
