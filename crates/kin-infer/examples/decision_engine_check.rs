// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Check `DecisionEngine` against a file of call-target prompts in the training format.
//!
//! Each input line is a JSON object with the prompt `text` and, optionally, `torch_logits` (the
//! reference letter logits from another runtime, for the same model). The check splits each
//! prompt back into its call site, callee and candidates, rebuilds it as a
//! `CandidateQuery::call_target`, and counts prompts the engine renders differently or
//! tokenizes to different ids than `token_ids`. Unless `--render-only` is given, it then loads
//! the model through `DecisionEngineConfig`, scores every query with `score_candidates`, and
//! reports latency, agreement with the reference's top choice, the largest probability
//! difference at the engine's temperature, and how many answers clear the confidence floor.
//! Last it scores all candidates of the first prompts as several questions of two, to time the
//! combined path.
//!
//! `--render-only` reads only the directory's `tokenizer.json`, never the weights.
//! `--ignore-reference` skips the comparison, for reference logits that came from another model.
//!
//!   cargo run --features decision --example decision_engine_check -- \
//!       <model_dir> <prompts.jsonl> [--cpu] [--render-only] [--ignore-reference] \
//!       [--min-confidence <p>] [--limit <n>]

use std::path::PathBuf;
use std::time::Instant;

use kin_infer::{CandidateQuery, Choice, DecisionEngine, DecisionEngineConfig, Verdict};

#[derive(serde::Deserialize)]
struct Prompt {
    text: String,
    #[serde(default)]
    token_ids: Option<Vec<u32>>,
    #[serde(default, alias = "reference_logits")]
    torch_logits: Option<Vec<f32>>,
}

/// A prompt split back into the pieces a `CandidateQuery::call_target` takes.
struct Parsed {
    state: String,
    callee: String,
    candidates: Vec<String>,
}

fn parse(text: &str) -> Option<Parsed> {
    let at = text.rfind("\n\nQuestion: ")?;
    let state = &text[..at];
    let mut lines = text[at + 2..].lines();
    let question = lines.next()?.strip_prefix("Question: ")?;
    let callee = question
        .strip_prefix("Which definition does the call to `")?
        .strip_suffix("` at this call site execute?")?;
    let mut options: Vec<String> = Vec::new();
    for line in lines {
        if line == "Answer:" {
            break;
        }
        let (_letter, option) = line.split_once(". ")?;
        options.push(option.to_owned());
    }
    // The last option is "none of these".
    options.pop()?;
    Some(Parsed {
        state: state.to_owned(),
        callee: callee.to_owned(),
        candidates: options,
    })
}

fn softmax(logits: &[f32], temperature: f32) -> Vec<f64> {
    let scaled: Vec<f64> = logits
        .iter()
        .map(|&l| f64::from(l) / f64::from(temperature))
        .collect();
    let max = scaled.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let exp: Vec<f64> = scaled.iter().map(|s| (s - max).exp()).collect();
    let sum: f64 = exp.iter().sum();
    exp.into_iter().map(|e| e / sum).collect()
}

fn argmax(v: &[f64]) -> usize {
    (1..v.len()).fold(0, |best, i| if v[i] > v[best] { i } else { best })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    // Arguments that are neither a flag nor the value of one.
    let takes_value = |a: &str| matches!(a, "--min-confidence" | "--limit");
    let positional: Vec<&String> = args
        .iter()
        .enumerate()
        .filter(|&(i, a)| {
            let is_value = i > 0 && takes_value(&args[i - 1]);
            !a.starts_with("--") && !is_value
        })
        .map(|(_, a)| a)
        .collect();
    if positional.len() != 2 {
        eprintln!(
            "usage: decision_engine_check <model_dir> <prompts.jsonl> [--cpu] [--render-only] \
             [--ignore-reference] [--min-confidence <p>] [--limit <n>]"
        );
        std::process::exit(2);
    }
    let dir = PathBuf::from(positional[0]);
    let limit: usize = value("--limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    let prompts: Vec<Prompt> = std::fs::read_to_string(positional[1])
        .expect("read prompts")
        .lines()
        .take(limit)
        .map(|l| serde_json::from_str(l).expect("parse prompt"))
        .collect();

    // 1. Every prompt must come back byte for byte through the engine's query type.
    let mut tokenizer =
        tokenizers::Tokenizer::from_file(dir.join("tokenizer.json")).expect("tokenizer");
    tokenizer.with_truncation(None).expect("truncation");
    tokenizer.with_padding(None);
    let mut queries = Vec::with_capacity(prompts.len());
    let (mut unparsed, mut render_mismatches, mut token_mismatches) = (0, 0, 0);
    for prompt in &prompts {
        let Some(parsed) = parse(&prompt.text) else {
            unparsed += 1;
            continue;
        };
        let query =
            CandidateQuery::call_target(parsed.state, &parsed.callee, parsed.candidates.clone());
        let (_, question) = query
            .questions(query.candidates.len())
            .expect("questions")
            .remove(0);
        let rendered = question.prompt(&query.state).expect("render");
        if rendered != prompt.text {
            render_mismatches += 1;
        }
        if let Some(ids) = &prompt.token_ids {
            let encoded = tokenizer.encode(rendered.as_str(), true).expect("encode");
            if encoded.get_ids() != ids.as_slice() {
                token_mismatches += 1;
            }
        }
        let reference = prompt
            .torch_logits
            .clone()
            .filter(|_| !flag("--ignore-reference"));
        queries.push((query, reference));
    }
    println!(
        "prompts {} | unparsed {unparsed} | rendered differently {render_mismatches} | \
         token ids differ {token_mismatches}",
        prompts.len()
    );
    if flag("--render-only") || queries.is_empty() {
        return;
    }

    // 2. Score each query through the engine.
    let mut config = DecisionEngineConfig::new(&dir);
    if flag("--cpu") {
        config = config.on_cpu();
    }
    if let Some(floor) = value("--min-confidence").and_then(|s| s.parse().ok()) {
        config = config.with_min_confidence(floor);
    }
    let t = Instant::now();
    let engine = DecisionEngine::load(&config).expect("load");
    let settings = engine.settings();
    eprintln!(
        "loaded on {} in {:.1}s: {settings:?}",
        engine.backend(),
        t.elapsed().as_secs_f32()
    );
    // One warm-up call so first-use pipeline and buffer setup is not timed.
    let _ = engine.score_candidates(&queries[0].0);

    let mut ms = Vec::with_capacity(queries.len());
    let (mut compared, mut agree, mut max_diff) = (0usize, 0usize, 0f64);
    let (mut accepted, mut accepted_agree) = (0usize, 0usize);
    let mut whole_choices = Vec::with_capacity(queries.len());
    for (query, reference) in &queries {
        let t = Instant::now();
        let scores = engine.score_candidates(query).expect("score");
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        whole_choices.push(scores.choice);
        let mut got: Vec<f64> = scores.probabilities.iter().map(|&p| f64::from(p)).collect();
        got.push(f64::from(scores.none_probability));
        let verdict = scores.verdict(settings.min_confidence);
        if verdict != Verdict::Unsure {
            accepted += 1;
        }
        if let Some(reference) = reference {
            let want = softmax(reference, settings.temperature);
            compared += 1;
            let same = argmax(&want) == argmax(&got);
            agree += usize::from(same);
            if verdict != Verdict::Unsure {
                accepted_agree += usize::from(same);
            }
            for (a, b) in got.iter().zip(&want) {
                max_diff = max_diff.max((a - b).abs());
            }
        }
    }
    ms.sort_by(f64::total_cmp);
    let pct = |q: f64| ms[((ms.len() as f64 - 1.0) * q).round() as usize];
    println!(
        "score_candidates {} | p50 {:.1} ms | p95 {:.1} ms | top choice agrees {agree}/{compared} | \
         max |probability diff| {max_diff:.5} | at floor {:.2}: {accepted} answered, \
         {accepted_agree} of them agree",
        queries.len(),
        pct(0.5),
        pct(0.95),
        settings.min_confidence
    );

    // 3. The combined path: every query asked as questions of at most two candidates.
    drop(engine);
    let split = DecisionEngine::load(&config.with_options_per_question(2)).expect("load");
    let (mut questions, mut split_ms, mut same_top, mut both_none) = (0usize, 0f64, 0usize, 0usize);
    let sample = &queries[..queries.len().min(20)];
    for ((query, _), whole) in sample.iter().zip(&whole_choices) {
        let t = Instant::now();
        let parts = split.score_candidates(query).expect("score");
        split_ms += t.elapsed().as_secs_f64() * 1000.0;
        questions += parts.questions;
        same_top += usize::from(*whole == parts.choice);
        both_none += usize::from(*whole == Choice::NoneOfThese && parts.choice == *whole);
    }
    println!(
        "split into questions of 2: {} queries as {questions} questions, {:.1} ms a query | same \
         top choice as one question {same_top}/{} ({both_none} of them \"none of these\")",
        sample.len(),
        split_ms / sample.len() as f64,
        sample.len()
    );
}
