// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Universal transformer inference engine — pure Rust, GPU-accelerated.
//!
//! Supports encoder (BERT, RoBERTa, ALBERT, DeBERTa, T5, nomic-embed, GTE) and
//! decoder-only (LLaMA, Mistral, Gemma, GPT-2, Phi, Qwen2) architectures.
//!
//! The engine is the `engine` feature, on by default. Without it the crate is
//! [`resource`] alone: host, memory and accelerator detection plus the
//! per-profile budgets, which a build that runs no model still reports.
//!
//! This file is the crate root so that the engine can stay in `lib.rs`, loaded
//! below as a module only when the feature is on. Everything public in it is
//! re-exported here, so its paths are the same as they always were
//! (`kin_infer::BertModel`, `kin_infer::gpu`, and so on). Code inside the
//! engine that names another engine item through `crate::` needs that item to
//! be at least `pub(crate)`, since the glob below re-exports nothing narrower.

pub mod resource;

#[cfg(feature = "engine")]
#[path = "lib.rs"]
mod engine;

#[cfg(feature = "engine")]
pub use engine::*;
