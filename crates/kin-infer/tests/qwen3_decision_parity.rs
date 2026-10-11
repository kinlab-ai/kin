// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

#![cfg(feature = "engine")]

//! Qwen3 decision scoring matches Hugging Face transformers.
//!
//! `score_next_tokens` replays the token ids in `tests/fixtures/qwen3_reference.json` (written by
//! `scripts/qwen3_reference.py` from float32 transformers) and must reproduce the last position's
//! candidate logits. It needs the model on disk, so it runs only when `KIN_INFER_QWEN3_DIR` names
//! a Qwen3 directory holding `config.json` and `model.safetensors`.

use std::path::PathBuf;
use std::time::Instant;

use kin_infer::{decoder_prefill_runtime_stats, BertConfig, BertModel};

#[derive(serde::Deserialize)]
struct Case {
    token_ids: Vec<u32>,
    candidate_ids: Vec<u32>,
    logits: Vec<f32>,
}

#[derive(serde::Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

fn model_dir() -> Option<PathBuf> {
    std::env::var_os("KIN_INFER_QWEN3_DIR").map(PathBuf::from)
}

fn check(force_cpu: bool, tolerance: f32) {
    let Some(dir) = model_dir() else {
        eprintln!("skipping: KIN_INFER_QWEN3_DIR is not set");
        return;
    };
    let config: BertConfig =
        serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap()).unwrap();
    let t0 = Instant::now();
    let model =
        BertModel::load_with_backend(&dir.join("model.safetensors"), config, force_cpu).unwrap();
    eprintln!(
        "loaded on {} in {:.1}s",
        model.backend(),
        t0.elapsed().as_secs_f32()
    );
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/qwen3_reference.json")).unwrap();
    let used_before = decoder_prefill_runtime_stats().used;
    for (i, case) in fixture.cases.iter().enumerate() {
        let t = Instant::now();
        let got = model
            .score_next_tokens(&case.token_ids, &case.candidate_ids)
            .unwrap();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let max_diff = got
            .iter()
            .zip(&case.logits)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        let argmax = |v: &[f32]| (0..v.len()).max_by(|&a, &b| v[a].total_cmp(&v[b])).unwrap();
        eprintln!(
            "case {i}: {} tokens, {ms:.1} ms, max |logit diff| {max_diff:.4}, got {got:?}",
            case.token_ids.len()
        );
        assert_eq!(
            argmax(&got),
            argmax(&case.logits),
            "case {i}: decision differs"
        );
        assert!(
            max_diff < tolerance,
            "case {i}: logits differ by {max_diff}"
        );
    }
    // On Metal the cases must have run on the device-resident prefill, not the
    // generic decoder it falls back to. That path needs Metal Shading Language 4.0
    // (macOS 26); set KIN_INFER_NO_DECODER_PREFILL to check the generic path alone.
    if model.backend() == kin_infer::gpu::GpuBackend::Metal
        && std::env::var_os("KIN_INFER_NO_DECODER_PREFILL").is_none()
    {
        assert_eq!(
            decoder_prefill_runtime_stats().used - used_before,
            fixture.cases.len() as u64,
            "Metal scored some cases on the generic decoder instead of the device prefill"
        );
    }
}

#[test]
fn qwen3_candidate_logits_match_transformers_on_cpu() {
    check(true, 0.05);
}

#[test]
fn qwen3_candidate_logits_match_transformers_on_default_backend() {
    check(false, 0.1);
}

/// On the CPU backend `score_next_tokens` runs the fast CPU prefill. It must agree with the
/// generic decoder pass it replaced within 1e-3 on every candidate logit, and repeat bit for bit.
#[test]
fn qwen3_fast_cpu_prefill_matches_generic_decoder_pass() {
    let Some(dir) = model_dir() else {
        eprintln!("skipping: KIN_INFER_QWEN3_DIR is not set");
        return;
    };
    let config: BertConfig =
        serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap()).unwrap();
    let model = BertModel::load_with_backend(&dir.join("model.safetensors"), config, true).unwrap();
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/qwen3_reference.json")).unwrap();
    for (i, case) in fixture.cases.iter().enumerate() {
        let t = Instant::now();
        let fast = model
            .score_next_tokens(&case.token_ids, &case.candidate_ids)
            .unwrap();
        let fast_ms = t.elapsed().as_secs_f64() * 1000.0;
        let again = model
            .score_next_tokens(&case.token_ids, &case.candidate_ids)
            .unwrap();
        let t = Instant::now();
        let reference = model
            .score_next_tokens_reference(&case.token_ids, &case.candidate_ids)
            .unwrap();
        let reference_ms = t.elapsed().as_secs_f64() * 1000.0;
        let max_diff = fast
            .iter()
            .zip(&reference)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        eprintln!(
            "case {i}: {} tokens, fast {fast_ms:.1} ms, generic {reference_ms:.1} ms, max |logit diff| {max_diff:.2e}",
            case.token_ids.len()
        );
        assert!(
            fast.iter()
                .zip(&again)
                .all(|(a, b)| a.to_bits() == b.to_bits()),
            "case {i}: a repeated score changed"
        );
        assert!(
            max_diff < 1e-3,
            "case {i}: fast and generic differ by {max_diff}"
        );
    }
}
