// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Check the fast CPU prefill against the generic decoder pass on real prompts.
//!
//! The model loads on the CPU backend, where `score_next_tokens` runs the fast prefill, and
//! every prompt is scored three ways: the fast path twice, then the generic pass through
//! `score_next_tokens_reference`. The check reports the largest logit difference between the
//! two paths, between the fast path and the prompt's own reference logits when it carries them,
//! whether the repeated fast score matched bit for bit, and latency percentiles for both paths.
//! The last line is a fingerprint of every fast logit, so two runs can be compared for
//! determinism across processes.
//!
//! The prompt file is either JSON lines with `token_ids`, `candidate_ids` and optional
//! `reference_logits` (or `torch_logits`), or a fixture object with a `cases` array whose
//! entries carry `logits`, like `tests/fixtures/qwen3_reference.json`.
//!
//!   cargo run --release --features accelerate --example cpu_prefill_check -- <model_dir> <prompts> [limit]
//!
//! Set `KIN_INFER_CPU_PREFILL_TIMINGS=1` for the prefill's per-stage breakdown on stderr.

use std::path::PathBuf;
use std::time::Instant;

use kin_infer::{BertConfig, BertModel};

#[derive(serde::Deserialize)]
struct Prompt {
    token_ids: Vec<u32>,
    candidate_ids: Vec<u32>,
    #[serde(default, alias = "torch_logits", alias = "logits")]
    reference_logits: Option<Vec<f32>>,
}

#[derive(serde::Deserialize)]
struct Fixture {
    cases: Vec<Prompt>,
}

fn read_prompts(text: &str) -> Vec<Prompt> {
    if let Ok(fixture) = serde_json::from_str::<Fixture>(text) {
        return fixture.cases;
    }
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("parse prompt line"))
        .collect()
}

fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f32::max)
}

fn argmax(v: &[f32]) -> usize {
    (0..v.len())
        .max_by(|&a, &b| v[a].total_cmp(&v[b]))
        .unwrap_or(0)
}

/// FNV-1a over the bit patterns of every logit.
fn fingerprint(rows: &[Vec<f32>]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in rows
        .iter()
        .flatten()
        .flat_map(|v| v.to_bits().to_le_bytes())
    {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    sorted[((sorted.len() as f64 - 1.0) * q).round() as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: cpu_prefill_check <model_dir> <prompts> [limit]");
        std::process::exit(2);
    }
    let dir = PathBuf::from(&args[1]);
    let limit: usize = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    let mut prompts = read_prompts(&std::fs::read_to_string(&args[2]).expect("read prompts"));
    prompts.truncate(limit);
    assert!(!prompts.is_empty(), "no prompts");

    let config: BertConfig =
        serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).expect("config"))
            .expect("parse config");
    let model = BertModel::load_with_backend(&dir.join("model.safetensors"), config, true)
        .expect("load model");
    eprintln!("loaded on {}", model.backend());

    // One untimed call so first-use setup (thread pools, scratch) is not measured.
    let _ = model.score_next_tokens(&prompts[0].token_ids, &prompts[0].candidate_ids);

    let (mut fast_ms, mut generic_ms) = (Vec::new(), Vec::new());
    let (mut vs_generic, mut vs_reference) = (0f32, 0f32);
    let (mut repeat_mismatches, mut decision_mismatches, mut with_reference) = (0, 0, 0);
    let mut fast_rows = Vec::with_capacity(prompts.len());
    for (i, p) in prompts.iter().enumerate() {
        let t = Instant::now();
        let fast = model
            .score_next_tokens(&p.token_ids, &p.candidate_ids)
            .expect("fast score");
        fast_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        let again = model
            .score_next_tokens(&p.token_ids, &p.candidate_ids)
            .expect("repeat score");
        let t = Instant::now();
        let generic = model
            .score_next_tokens_reference(&p.token_ids, &p.candidate_ids)
            .expect("generic score");
        generic_ms.push(t.elapsed().as_secs_f64() * 1000.0);

        let diff = max_abs_diff(&fast, &generic);
        vs_generic = vs_generic.max(diff);
        let repeated = fast
            .iter()
            .zip(&again)
            .all(|(a, b)| a.to_bits() == b.to_bits());
        repeat_mismatches += usize::from(!repeated);
        let mut line = format!(
            "prompt {i}: {} tokens | fast {:.1} ms | generic {:.1} ms | fast vs generic {diff:.2e}",
            p.token_ids.len(),
            fast_ms[i],
            generic_ms[i]
        );
        if let Some(reference) = &p.reference_logits {
            let diff = max_abs_diff(&fast, reference);
            vs_reference = vs_reference.max(diff);
            with_reference += 1;
            decision_mismatches += usize::from(argmax(&fast) != argmax(reference));
            line.push_str(&format!(" | fast vs reference {diff:.2e}"));
        }
        if !repeated {
            line.push_str(" | REPEAT DIFFERS");
        }
        println!("{line}");
        fast_rows.push(fast);
    }

    fast_ms.sort_by(f64::total_cmp);
    generic_ms.sort_by(f64::total_cmp);
    println!(
        "prompts {} | fast p50 {:.1} ms p95 {:.1} ms | generic p50 {:.1} ms p95 {:.1} ms | max |fast - generic| {vs_generic:.2e} | repeat mismatches {repeat_mismatches}",
        prompts.len(),
        percentile(&fast_ms, 0.5),
        percentile(&fast_ms, 0.95),
        percentile(&generic_ms, 0.5),
        percentile(&generic_ms, 0.95),
    );
    if with_reference > 0 {
        println!(
            "against reference logits: max |fast - reference| {vs_reference:.2e} | top choice differs on {decision_mismatches} of {with_reference}"
        );
    }
    println!("fast logits fingerprint {:016x}", fingerprint(&fast_rows));
}
