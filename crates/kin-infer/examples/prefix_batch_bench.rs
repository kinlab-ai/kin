// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Measure prefix reuse and batched scoring on a file of decision prompts.
//!
//! Each input line is a JSON object with `text`, `token_ids` and `candidate_ids`, and optionally
//! `reference_logits` (or `torch_logits`) from another runtime. Every prompt's text holds
//! "\n\nQuestion:" where the question and its lettered options begin. The bench splits the ids
//! there: the prefix is the longest run of ids that the text before the boundary, tokenized with
//! the model's `tokenizer.json`, shares with `token_ids`. It also writes the same question with
//! its options in a second order (reversed, with a trailing "none of these" kept last), as a
//! caller does to cancel position bias, and reports:
//!
//! 1. Per decision, the time to score both orders as whole prompts, and the time to prefill the
//!    shared state once and score both continuations, one after the other and as one batch,
//!    with the largest logit difference between the two routes.
//! 2. Batch throughput over every prompt: one at a time, then `score_next_tokens_batch` at
//!    several in-flight counts, and the continuations of every prefilled state as one batch.
//!
//!   KIN_INFER_FORCE_CPU=1 cargo run --release --example prefix_batch_bench -- \
//!       <model_dir> <prompts.jsonl> [limit]

use std::path::PathBuf;
use std::time::Instant;

use kin_infer::{BatchOptions, BertConfig, BertModel, PrefixState, ScoreRequest};

#[derive(serde::Deserialize)]
struct Prompt {
    text: String,
    token_ids: Vec<u32>,
    candidate_ids: Vec<u32>,
    #[serde(default, alias = "torch_logits")]
    reference_logits: Option<Vec<f32>>,
}

/// One decision asked in two option orders over one shared state.
struct Decision {
    split: usize,
    order_a: Vec<u32>,
    order_b: Vec<u32>,
    candidate_ids: Vec<u32>,
    reference: Option<Vec<f32>>,
}

impl Decision {
    fn prefix(&self) -> &[u32] {
        &self.order_a[..self.split]
    }
    fn suffix_a(&self) -> &[u32] {
        &self.order_a[self.split..]
    }
    fn suffix_b(&self) -> &[u32] {
        &self.order_b[self.split..]
    }
}

const BOUNDARY: &str = "\n\nQuestion:";

fn argmax(v: &[f32]) -> usize {
    (0..v.len())
        .max_by(|&a, &b| v[a].total_cmp(&v[b]))
        .unwrap_or(0)
}

fn max_diff(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0f32, f32::max)
}

/// Option lines look like "a. text". Reverse them, keep a final "none of these" option last,
/// and letter them again in order.
fn reorder_options(question: &str) -> String {
    let lines: Vec<&str> = question.split('\n').collect();
    let is_option = |l: &str| {
        let b = l.as_bytes();
        b.len() >= 3 && b[0].is_ascii_lowercase() && b[1] == b'.' && b[2] == b' '
    };
    let options: Vec<usize> = (0..lines.len()).filter(|&i| is_option(lines[i])).collect();
    let mut bodies: Vec<&str> = options.iter().map(|&i| &lines[i][3..]).collect();
    let keep_last = bodies
        .last()
        .is_some_and(|b| b.starts_with("none of these"));
    let movable = bodies.len() - usize::from(keep_last);
    bodies[..movable].reverse();
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    for (slot, (&line, body)) in options.iter().zip(bodies).enumerate() {
        out[line] = format!("{}. {body}", (b'a' + slot as u8) as char);
    }
    out.join("\n")
}

fn shared_len(a: &[u32], b: &[u32]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: prefix_batch_bench <model_dir> <prompts.jsonl> [limit]");
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
        .filter(|l| !l.trim().is_empty())
        .take(limit)
        .map(|l| serde_json::from_str(l).expect("parse prompt"))
        .collect();

    let tokenizer =
        tokenizers::Tokenizer::from_file(dir.join("tokenizer.json")).expect("tokenizer");
    let encode = |text: &str| -> Vec<u32> {
        tokenizer
            .encode(text, false)
            .expect("encode")
            .get_ids()
            .to_vec()
    };
    let mut retokenized_differs = 0;
    let decisions: Vec<Decision> = prompts
        .iter()
        .map(|p| {
            let at = p
                .text
                .find(BOUNDARY)
                .expect("prompt text has no question boundary");
            if encode(&p.text) != p.token_ids {
                retokenized_differs += 1;
            }
            let split = shared_len(&encode(&p.text[..at]), &p.token_ids);
            let text_b = format!("{}{}", &p.text[..at], reorder_options(&p.text[at..]));
            let order_b = encode(&text_b);
            assert!(
                order_b[..split] == p.token_ids[..split],
                "the second order changed the shared prefix"
            );
            Decision {
                split,
                order_a: p.token_ids.clone(),
                order_b,
                candidate_ids: p.candidate_ids.clone(),
                reference: p.reference_logits.clone(),
            }
        })
        .collect();
    let n = decisions.len();
    let total_tokens: usize = decisions.iter().map(|d| d.order_a.len()).sum();
    let prefix_tokens: usize = decisions.iter().map(|d| d.split).sum();
    let suffix_tokens: usize = decisions
        .iter()
        .map(|d| d.suffix_a().len() + d.suffix_b().len())
        .sum();
    println!(
        "{n} prompts, {} tokens on average, prefix {} tokens and question {} tokens on average; \
         {retokenized_differs} retokenize differently from their token_ids",
        total_tokens / n.max(1),
        prefix_tokens / n.max(1),
        suffix_tokens / (2 * n).max(1),
    );

    let config: BertConfig =
        serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).expect("config"))
            .expect("parse config");
    let t = Instant::now();
    let model = BertModel::load(&dir.join("model.safetensors"), config).expect("load");
    println!(
        "loaded on {} in {:.1}s, rayon pool {} threads",
        model.backend(),
        t.elapsed().as_secs_f32(),
        rayon::current_num_threads()
    );

    // Warm-up so first-use setup is not timed.
    let d0 = &decisions[0];
    model
        .score_next_tokens(&d0.order_a, &d0.candidate_ids)
        .expect("warm-up");

    // 1. Two option orders per decision.
    let (mut t_whole, mut t_reuse, mut t_reuse_batch, mut t_whole_batch) = (0.0, 0.0, 0.0, 0.0);
    let (mut t_prefill, mut t_cont) = (0.0, 0.0);
    let (mut worst_reuse, mut worst_batch, mut worst_ref) = (0.0f32, 0.0f32, 0.0f32);
    let (mut agree_ref, mut with_ref, mut bit_identical) = (0, 0, 0);
    let mut states: Vec<PrefixState> = Vec::with_capacity(n);
    for (i, d) in decisions.iter().enumerate() {
        let c = &d.candidate_ids;

        let t = Instant::now();
        let whole_a = model.score_next_tokens(&d.order_a, c).unwrap();
        let whole_b = model.score_next_tokens(&d.order_b, c).unwrap();
        let whole_ms = ms(t);

        let t = Instant::now();
        let state = model.prefill(d.prefix()).unwrap();
        let prefill_ms = ms(t);
        let t2 = Instant::now();
        let reuse_a = model.score_continuation(&state, d.suffix_a(), c).unwrap();
        let reuse_b = model.score_continuation(&state, d.suffix_b(), c).unwrap();
        let cont_ms = ms(t2);
        let reuse_ms = ms(t);

        let t = Instant::now();
        let state2 = model.prefill(d.prefix()).unwrap();
        let batched = model
            .score_next_tokens_batch(&[
                ScoreRequest::continuation(&state2, d.suffix_a(), c),
                ScoreRequest::continuation(&state2, d.suffix_b(), c),
            ])
            .unwrap();
        let reuse_batch_ms = ms(t);

        let t = Instant::now();
        let whole_batched = model
            .score_next_tokens_batch(&[
                ScoreRequest::new(&d.order_a, c),
                ScoreRequest::new(&d.order_b, c),
            ])
            .unwrap();
        let whole_batch_ms = ms(t);

        let reuse_diff = max_diff(&reuse_a, &whole_a).max(max_diff(&reuse_b, &whole_b));
        let batch_diff = max_diff(&batched[0], &reuse_a)
            .max(max_diff(&batched[1], &reuse_b))
            .max(max_diff(&whole_batched[0], &whole_a))
            .max(max_diff(&whole_batched[1], &whole_b));
        if reuse_a == whole_a && reuse_b == whole_b {
            bit_identical += 1;
        }
        worst_reuse = worst_reuse.max(reuse_diff);
        worst_batch = worst_batch.max(batch_diff);
        if let Some(r) = &d.reference {
            with_ref += 1;
            worst_ref = worst_ref.max(max_diff(&whole_a, r));
            if argmax(&whole_a) == argmax(r) {
                agree_ref += 1;
            }
        }
        println!(
            "{i:>3}: {:>4} tok (prefix {:>4}, questions {:>3}+{:>3}) whole x2 {whole_ms:>7.0} ms | \
             prefill {prefill_ms:>6.0} + 2 continuations {cont_ms:>6.0} = {reuse_ms:>7.0} ms | \
             reuse+batch {reuse_batch_ms:>6.0} ms | whole batch {whole_batch_ms:>6.0} ms | \
             reuse diff {reuse_diff:.1e}",
            d.order_a.len(),
            d.split,
            d.suffix_a().len(),
            d.suffix_b().len(),
        );
        t_whole += whole_ms;
        t_reuse += reuse_ms;
        t_prefill += prefill_ms;
        t_cont += cont_ms;
        t_reuse_batch += reuse_batch_ms;
        t_whole_batch += whole_batch_ms;
        states.push(state);
    }
    println!("\nTwo option orders per decision, {n} decisions, totals:");
    println!("  whole prompts, one after the other   {t_whole:>9.0} ms  (1.00x)");
    println!(
        "  whole prompts, batched               {t_whole_batch:>9.0} ms  ({:.2}x)",
        t_whole / t_whole_batch
    );
    println!(
        "  prefill + 2 continuations            {t_reuse:>9.0} ms  ({:.2}x)  [prefill {t_prefill:.0}, continuations {t_cont:.0}]",
        t_whole / t_reuse
    );
    println!(
        "  prefill + batched continuations      {t_reuse_batch:>9.0} ms  ({:.2}x)",
        t_whole / t_reuse_batch
    );
    println!(
        "  continuation vs whole prompt: max |logit diff| {worst_reuse:.2e}, bit-identical {bit_identical}/{n}"
    );
    println!("  batch vs one at a time: max |logit diff| {worst_batch:.2e}");
    if with_ref > 0 {
        println!(
            "  whole prompt vs reference: max |logit diff| {worst_ref:.2e}, top answer agrees {agree_ref}/{with_ref}"
        );
    }

    // 2. Batch throughput over every prompt, first option order.
    println!("\nBatch throughput, {n} whole prompts, {total_tokens} tokens:");
    let t = Instant::now();
    let one_by_one: Vec<Vec<f32>> = decisions
        .iter()
        .map(|d| {
            model
                .score_next_tokens(&d.order_a, &d.candidate_ids)
                .unwrap()
        })
        .collect();
    let base_ms = ms(t);
    let report = |label: &str, took: f64, base: f64, count: usize| {
        println!(
            "  {label:<34} {took:>9.0} ms  {:>6.2} prompts/s  {:>7.0} tok/s  ({:.2}x)",
            count as f64 * 1000.0 / took,
            total_tokens as f64 * 1000.0 / took,
            base / took
        );
    };
    report("one at a time", base_ms, base_ms, n);
    let requests: Vec<ScoreRequest<'_>> = decisions
        .iter()
        .map(|d| ScoreRequest::new(&d.order_a, &d.candidate_ids))
        .collect();
    for in_flight in [0usize, 2, 3, 4, 6, 8] {
        let options = BatchOptions {
            max_in_flight: in_flight,
            ..BatchOptions::default()
        };
        let t = Instant::now();
        let got = model
            .score_next_tokens_batch_with(&requests, &options)
            .unwrap();
        let took = ms(t);
        let diff = got
            .iter()
            .zip(&one_by_one)
            .map(|(g, e)| max_diff(g, e))
            .fold(0.0f32, f32::max);
        let label = match in_flight {
            0 => "batch, default in flight".to_string(),
            k => format!("batch, {k} in flight"),
        };
        report(&label, took, base_ms, n);
        println!("    max |logit diff| vs one at a time {diff:.2e}");
    }

    println!(
        "\nEvery decision in both orders ({} requests) against its prefilled state:",
        2 * n
    );
    let continuations: Vec<ScoreRequest<'_>> = decisions
        .iter()
        .zip(&states)
        .flat_map(|(d, s)| {
            [
                ScoreRequest::continuation(s, d.suffix_a(), &d.candidate_ids),
                ScoreRequest::continuation(s, d.suffix_b(), &d.candidate_ids),
            ]
        })
        .collect();
    let t = Instant::now();
    model.score_next_tokens_batch(&continuations).unwrap();
    let cont_batch_ms = ms(t);
    println!(
        "  whole prompts one at a time {t_whole:.0} ms; prefill one at a time {t_prefill:.0} ms \
         + batched continuations {cont_batch_ms:.0} ms = {:.0} ms ({:.2}x)",
        t_prefill + cont_batch_ms,
        t_whole / (t_prefill + cont_batch_ms)
    );
    let state_mb: f64 = states.iter().map(|s| s.cache_bytes() as f64).sum::<f64>() / 1e6;
    println!(
        "  {n} prefix states hold {state_mb:.0} MB, {:.1} MB each on average",
        state_mb / n as f64
    );
}
