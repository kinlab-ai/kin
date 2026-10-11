// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Check a pointer decision model against reference probabilities, and time it.
//!
//! Each input line is a JSON record in the family's internal form, `{"state": ..., "questions":
//! [{"instr": ..., "options": [...]}, ...]}`, optionally with `reference`: one probability list
//! per question from another runtime for the same checkpoint, and `token_ids`: the reference
//! runtime's ids for each question's row. The check loads the adapter directory and the base
//! directory, answers every record in one batch, and reports how many rows tokenized
//! differently, the largest probability difference from the reference, how often the top
//! option agrees, and the time to load and to answer: cold (the first batch after load) and
//! warm (the median of `--repeat` more).
//!
//!   cargo run --release --features decision --example pointer_decision_check -- \
//!       <adapter_dir> <base_dir> <records.jsonl> [--cpu] [--repeat <n>] [--limit <n>]

use std::path::PathBuf;
use std::time::Instant;

use kin_infer::{PointerCheckpoint, PointerDecisionModel, PointerQuestion, PointerRecord};

#[derive(serde::Deserialize)]
struct Question {
    instr: String,
    options: Vec<String>,
}

#[derive(serde::Deserialize)]
struct Record {
    state: String,
    questions: Vec<Question>,
    #[serde(default)]
    reference: Option<Vec<Vec<f32>>>,
    #[serde(default)]
    token_ids: Option<Vec<Vec<u32>>>,
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
    if positional.len() < 3 {
        eprintln!(
            "usage: pointer_decision_check <adapter_dir> <base_dir> <records.jsonl> [--cpu] \
             [--repeat <n>] [--limit <n>]"
        );
        std::process::exit(2);
    }
    let adapter = PathBuf::from(positional[0]);
    let base = PathBuf::from(positional[1]);
    let records_path = PathBuf::from(positional[2]);
    let cpu = args.iter().any(|a| a == "--cpu");
    let repeat = flag_value("--repeat").unwrap_or(3);
    let limit = flag_value("--limit").unwrap_or(usize::MAX);

    let text = std::fs::read_to_string(&records_path).expect("read records");
    let records: Vec<Record> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(limit)
        .map(|l| serde_json::from_str(l).expect("parse record"))
        .collect();
    let inputs: Vec<PointerRecord> = records
        .iter()
        .map(|r| PointerRecord {
            state: r.state.clone(),
            questions: r
                .questions
                .iter()
                .map(|q| PointerQuestion::new(q.instr.clone(), q.options.clone()))
                .collect(),
        })
        .collect();

    let started = Instant::now();
    let checkpoint = PointerCheckpoint::read(&adapter).expect("read head.pt");
    println!(
        "checkpoint: base {} @ {}, head {} wide, temperature {}",
        checkpoint.base_model(),
        checkpoint.base_revision().unwrap_or("?"),
        checkpoint.head_dim(),
        checkpoint.temperature()
    );
    let model = PointerDecisionModel::load(checkpoint, &base, cpu).expect("load model");
    let load = started.elapsed();
    println!(
        "loaded on {} in {:.2}s",
        model.backend(),
        load.as_secs_f64()
    );

    let mut rows = 0usize;
    let mut tokens = 0usize;
    let mut id_mismatches = 0usize;
    for (record, input) in records.iter().zip(&inputs) {
        let encoded = model.encode(input).expect("encode");
        for (i, row) in encoded.iter().enumerate() {
            rows += 1;
            tokens += row.ids.len();
            if let Some(ids) = record.token_ids.as_ref().and_then(|t| t.get(i)) {
                if *ids != row.ids {
                    id_mismatches += 1;
                    if id_mismatches <= 3 {
                        let at = ids.iter().zip(&row.ids).position(|(a, b)| a != b);
                        eprintln!(
                            "row {rows}: ids differ at {at:?} (reference {} tokens, ours {})",
                            ids.len(),
                            row.ids.len()
                        );
                    }
                }
            }
        }
    }
    println!(
        "{} records, {rows} rows, {tokens} tokens ({:.0} per row), {id_mismatches} rows tokenized \
         differently from the reference",
        records.len(),
        tokens as f64 / rows.max(1) as f64
    );

    let cold = Instant::now();
    let answers = model.answer_batch(&inputs).expect("answer");
    let cold = cold.elapsed();
    let mut warm: Vec<f64> = (0..repeat)
        .map(|_| {
            let t = Instant::now();
            model.answer_batch(&inputs).expect("answer");
            t.elapsed().as_secs_f64()
        })
        .collect();
    warm.sort_by(f64::total_cmp);
    let median = warm.get(warm.len() / 2).copied().unwrap_or(f64::NAN);
    println!(
        "answer_batch: cold {:.3}s, warm median {:.3}s over {repeat} ({:.1} ms per row warm)",
        cold.as_secs_f64(),
        median,
        median * 1000.0 / rows.max(1) as f64
    );

    let mut compared = 0usize;
    let mut agree = 0usize;
    let mut max_diff = 0f32;
    for (record, answer) in records.iter().zip(&answers) {
        let Some(reference) = &record.reference else {
            continue;
        };
        for (ours, theirs) in answer.iter().zip(reference) {
            compared += 1;
            let argmax = |p: &[f32]| {
                p.iter()
                    .enumerate()
                    .max_by(|a, b| a.1.total_cmp(b.1))
                    .map(|(i, _)| i)
            };
            if argmax(ours) == argmax(theirs) {
                agree += 1;
            }
            for (a, b) in ours.iter().zip(theirs) {
                max_diff = max_diff.max((a - b).abs());
            }
        }
    }
    if compared > 0 {
        println!(
            "reference: {compared} questions, top option agrees on {agree}, max |dp| {max_diff:.4}"
        );
    }
    for (record, answer) in answers.iter().enumerate().take(5) {
        println!("record {record}: {answer:?}");
    }
}
