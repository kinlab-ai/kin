// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Check a yes-or-no reranker against reference scores, and time it.
//!
//! Each input line is a JSON object `{"instruction": ..., "query": ..., "document": ...}`,
//! optionally with `reference` (the reference runtime's `yes` minus `no` logit for the same
//! checkpoint) and `token_ids` (its prompt ids). The check loads the checkpoint directory,
//! scores every line in one batch, and reports how many prompts tokenized differently, the
//! largest log-odds and probability differences from the reference, and the time to load and
//! to score: cold (the first batch after load) and warm (the median of `--repeat` more).
//!
//!   cargo run --release --features decision --example reranker_check -- \
//!       <model_dir> <pairs.jsonl> [--cpu] [--repeat <n>] [--limit <n>]

use std::path::PathBuf;
use std::time::Instant;

use kin_infer::YesNoReranker;

#[derive(serde::Deserialize)]
struct Pair {
    #[serde(default)]
    instruction: Option<String>,
    query: String,
    document: String,
    #[serde(default)]
    reference: Option<f32>,
    #[serde(default)]
    token_ids: Option<Vec<u32>>,
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let flag_value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<usize>().ok())
    };
    if positional.len() < 2 {
        eprintln!(
            "usage: reranker_check <model_dir> <pairs.jsonl> [--cpu] [--repeat <n>] [--limit <n>]"
        );
        std::process::exit(2);
    }
    let dir = PathBuf::from(positional[0]);
    let cpu = args.iter().any(|a| a == "--cpu");
    let repeat = flag_value("--repeat").unwrap_or(3);
    let limit = flag_value("--limit").unwrap_or(usize::MAX);
    let text = std::fs::read_to_string(positional[1]).expect("read pairs");
    let pairs: Vec<Pair> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(limit)
        .map(|l| serde_json::from_str(l).expect("parse pair"))
        .collect();

    let started = Instant::now();
    let reranker = YesNoReranker::load(&dir, cpu).expect("load reranker");
    println!(
        "loaded on {} in {:.2}s, answer ids {:?}",
        reranker.backend(),
        started.elapsed().as_secs_f64(),
        reranker.answer_ids()
    );
    let instruction = |p: &Pair| {
        p.instruction
            .clone()
            .unwrap_or_else(|| kin_infer::decision::QWEN3_RERANKER_DEFAULT_INSTRUCTION.to_string())
    };
    let mut tokens = 0usize;
    let mut mismatched = 0usize;
    for (i, pair) in pairs.iter().enumerate() {
        let ids = reranker
            .prompt_ids(&instruction(pair), &pair.query, &pair.document)
            .expect("prompt");
        tokens += ids.len();
        if let Some(reference) = &pair.token_ids {
            if *reference != ids {
                mismatched += 1;
                let at = reference.iter().zip(&ids).position(|(a, b)| a != b);
                eprintln!(
                    "pair {i}: ids differ at {at:?} ({} vs {})",
                    reference.len(),
                    ids.len()
                );
            }
        }
    }
    println!(
        "{} pairs, {tokens} tokens ({:.0} per prompt), {mismatched} tokenized differently",
        pairs.len(),
        tokens as f64 / pairs.len().max(1) as f64
    );

    // Pairs with one instruction and query score as one batch, as a locate query does.
    let score_all = || -> Vec<f32> {
        let mut out = Vec::with_capacity(pairs.len());
        let mut start = 0;
        while start < pairs.len() {
            let key = (instruction(&pairs[start]), pairs[start].query.clone());
            let mut end = start + 1;
            while end < pairs.len() && (instruction(&pairs[end]), pairs[end].query.clone()) == key {
                end += 1;
            }
            let documents: Vec<&str> = pairs[start..end]
                .iter()
                .map(|p| p.document.as_str())
                .collect();
            out.extend(
                reranker
                    .log_odds(&key.0, &key.1, &documents)
                    .expect("score"),
            );
            start = end;
        }
        out
    };
    let cold = Instant::now();
    let scores = score_all();
    let cold = cold.elapsed().as_secs_f64();
    let mut warm: Vec<f64> = (0..repeat)
        .map(|_| {
            let t = Instant::now();
            score_all();
            t.elapsed().as_secs_f64()
        })
        .collect();
    warm.sort_by(f64::total_cmp);
    let median = warm.get(warm.len() / 2).copied().unwrap_or(f64::NAN);
    println!(
        "scoring: cold {cold:.3}s, warm median {median:.3}s over {repeat} ({:.1} ms per pair warm)",
        median * 1000.0 / pairs.len().max(1) as f64
    );
    let mut compared = 0;
    let (mut max_odds, mut max_p) = (0f32, 0f32);
    let mut same_side = 0;
    for (pair, ours) in pairs.iter().zip(&scores) {
        if let Some(theirs) = pair.reference {
            compared += 1;
            max_odds = max_odds.max((ours - theirs).abs());
            max_p = max_p.max((sigmoid(*ours) - sigmoid(theirs)).abs());
            if (*ours > 0.0) == (theirs > 0.0) {
                same_side += 1;
            }
        }
    }
    if compared > 0 {
        println!(
            "reference: {compared} pairs, same yes/no side on {same_side}, max |d log-odds| \
             {max_odds:.4}, max |dp| {max_p:.4}"
        );
    }
    for (pair, score) in pairs.iter().zip(&scores).take(8) {
        println!(
            "{:>8.3}  p={:.4}  {}",
            score,
            sigmoid(*score),
            &pair.query.chars().take(60).collect::<String>()
        );
    }
}
