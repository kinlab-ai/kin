// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Time `score_next_tokens` over a file of decision prompts and check it against a reference.
//!
//! Each input line is a JSON object with `text`, `token_ids`, `candidate_ids` and
//! `reference_logits` (from any other runtime). The bench re-tokenizes `text` with the model's
//! `tokenizer.json` and counts prompts whose ids differ from `token_ids`, then scores
//! `token_ids`, and reports latency percentiles, how often the top candidate agrees with the
//! reference, and the largest logit difference.
//!
//!   cargo run --release --features metal --example decision_bench -- <model_dir> <prompts.jsonl> [limit]

use std::path::PathBuf;
use std::time::Instant;

use kin_infer::{BertConfig, BertModel};

#[derive(serde::Deserialize)]
struct Prompt {
    text: String,
    token_ids: Vec<u32>,
    candidate_ids: Vec<u32>,
    #[serde(alias = "torch_logits")]
    reference_logits: Vec<f32>,
}

fn argmax(v: &[f32]) -> usize {
    (0..v.len())
        .max_by(|&a, &b| v[a].total_cmp(&v[b]))
        .unwrap_or(0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: decision_bench <model_dir> <prompts.jsonl> [limit]");
        std::process::exit(2);
    }
    let dir = PathBuf::from(&args[1]);
    let limit: usize = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    let prompts: Vec<Prompt> = std::fs::read_to_string(&args[2])
        .expect("read prompts")
        .lines()
        .take(limit)
        .map(|l| serde_json::from_str(l).expect("parse prompt"))
        .collect();

    let tokenizer =
        tokenizers::Tokenizer::from_file(dir.join("tokenizer.json")).expect("tokenizer");
    let token_mismatches = prompts
        .iter()
        .filter(|p| {
            tokenizer
                .encode(p.text.as_str(), false)
                .expect("encode")
                .get_ids()
                != p.token_ids.as_slice()
        })
        .count();

    let config: BertConfig =
        serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).expect("config"))
            .expect("parse config");
    let t = Instant::now();
    let model = BertModel::load(&dir.join("model.safetensors"), config).expect("load");
    eprintln!(
        "loaded on {} in {:.1}s",
        model.backend(),
        t.elapsed().as_secs_f32()
    );

    // One warm-up call so first-use pipeline and buffer setup is not timed.
    let _ = model.score_next_tokens(&prompts[0].token_ids, &prompts[0].candidate_ids);

    let mut ms = Vec::with_capacity(prompts.len());
    let (mut agree, mut max_diff, mut tokens) = (0usize, 0f32, 0usize);
    let wall = Instant::now();
    for p in &prompts {
        let t = Instant::now();
        let got = model
            .score_next_tokens(&p.token_ids, &p.candidate_ids)
            .expect("score");
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        tokens += p.token_ids.len();
        agree += usize::from(argmax(&got) == argmax(&p.reference_logits));
        for (a, b) in got.iter().zip(&p.reference_logits) {
            max_diff = max_diff.max((a - b).abs());
        }
    }
    let wall_s = wall.elapsed().as_secs_f64();
    ms.sort_by(f64::total_cmp);
    let pct = |q: f64| ms[((ms.len() as f64 - 1.0) * q).round() as usize];
    println!(
        "prompts {} | tokenizer mismatches {} | p50 {:.1} ms | p95 {:.1} ms | {:.0} prompts/s | {:.0} tokens/s | top choice agrees {}/{} | max |logit diff| {:.4}",
        prompts.len(),
        token_mismatches,
        pct(0.5),
        pct(0.95),
        prompts.len() as f64 / wall_s,
        tokens as f64 / wall_s,
        agree,
        prompts.len(),
        max_diff
    );
}
