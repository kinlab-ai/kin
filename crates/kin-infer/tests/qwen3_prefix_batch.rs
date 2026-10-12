// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

#![cfg(feature = "engine")]

//! Prefix reuse and batched scoring return what plain Qwen3 scoring returns.
//!
//! `prefill` plus `score_continuation` run the generic decoder pass, so they must reproduce
//! `score_next_tokens_reference` on the concatenated prompt to float rounding.
//! `score_next_tokens` takes the fast CPU prefill instead, so a continuation must stay within
//! that prefill's own tolerance of it. `score_next_tokens_batch` must reproduce scoring each
//! request on its own. The prompts are the ones in `tests/fixtures/qwen3_reference.json`. The
//! tests need the model on disk, so they run only when `KIN_INFER_QWEN3_DIR` names a Qwen3
//! directory holding `config.json` and `model.safetensors`. When the directory also holds
//! `tokenizer.json`, the long prompt is also split where its question begins, the split a
//! decision caller makes.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

use kin_infer::{BatchOptions, BertConfig, BertModel, ScoreRequest};

/// CPU agreement required between a continuation and the generic decoder pass over the whole
/// prompt, in logits.
const TOLERANCE: f32 = 1e-4;

/// CPU agreement required between a continuation and `score_next_tokens` on the whole prompt,
/// in logits. The fast CPU prefill rounds differently from the generic pass, and
/// `qwen3_decision_parity.rs` holds the two paths to this bound.
const FAST_PATH_TOLERANCE: f32 = 1e-3;

#[derive(serde::Deserialize)]
struct Case {
    text: String,
    token_ids: Vec<u32>,
    candidate_ids: Vec<u32>,
}

#[derive(serde::Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

fn model_dir() -> Option<PathBuf> {
    std::env::var_os("KIN_INFER_QWEN3_DIR").map(PathBuf::from)
}

/// The model on the CPU, loaded once for every test in this file.
fn model() -> Option<&'static BertModel> {
    static MODEL: OnceLock<Option<BertModel>> = OnceLock::new();
    MODEL
        .get_or_init(|| {
            let dir = model_dir()?;
            let config: BertConfig =
                serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).unwrap())
                    .unwrap();
            let t = Instant::now();
            let model =
                BertModel::load_with_backend(&dir.join("model.safetensors"), config, true).unwrap();
            eprintln!(
                "loaded on {} in {:.1}s",
                model.backend(),
                t.elapsed().as_secs_f32()
            );
            Some(model)
        })
        .as_ref()
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/qwen3_reference.json")).unwrap()
}

fn max_diff(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0f32, f32::max)
}

/// Where a case's question begins in its token ids, when the case has a question and the model
/// directory has a tokenizer: the length of the longest run of ids the text before
/// "\n\nQuestion:" shares with the whole prompt.
fn question_split(case: &Case) -> Option<usize> {
    let at = case.text.find("\n\nQuestion:")?;
    let tokenizer = tokenizers::Tokenizer::from_file(model_dir()?.join("tokenizer.json")).ok()?;
    let prefix = tokenizer.encode(&case.text[..at], false).ok()?;
    let shared = prefix
        .get_ids()
        .iter()
        .zip(&case.token_ids)
        .take_while(|(a, b)| a == b)
        .count();
    Some(shared)
}

#[test]
fn continuation_of_a_prefix_matches_whole_prompt_scoring_on_cpu() {
    let Some(model) = model() else {
        eprintln!("skipping: KIN_INFER_QWEN3_DIR is not set");
        return;
    };
    for (i, case) in fixture().cases.iter().enumerate() {
        let ids = &case.token_ids;
        let n = ids.len();
        let whole = model
            .score_next_tokens_reference(ids, &case.candidate_ids)
            .unwrap();
        let fast = model.score_next_tokens(ids, &case.candidate_ids).unwrap();

        let mut splits = vec![0, 1, n / 2, n - 1, n];
        splits.extend(question_split(case));
        splits.sort_unstable();
        splits.dedup();
        for split in splits {
            let t = Instant::now();
            let prefix = model.prefill(&ids[..split]).unwrap();
            let prefill_ms = t.elapsed().as_secs_f64() * 1000.0;
            let t = Instant::now();
            let got = model
                .score_continuation(&prefix, &ids[split..], &case.candidate_ids)
                .unwrap();
            let continuation_ms = t.elapsed().as_secs_f64() * 1000.0;
            let diff = max_diff(&got, &whole);
            let fast_diff = max_diff(&got, &fast);
            eprintln!(
                "case {i}: {n} tokens split at {split}: prefill {prefill_ms:.0} ms, \
                 continuation {continuation_ms:.0} ms, max |logit diff| {diff:e} \
                 (fast prefill {fast_diff:e})"
            );
            assert_eq!(prefix.len(), split);
            assert!(
                diff < TOLERANCE,
                "case {i} split at {split}: continuation differs from the whole prompt by {diff}"
            );
            assert!(
                fast_diff < FAST_PATH_TOLERANCE,
                "case {i} split at {split}: continuation differs from score_next_tokens by \
                 {fast_diff}"
            );
        }
    }
}

#[test]
fn one_prefix_serves_many_continuations_unchanged() {
    let Some(model) = model() else {
        eprintln!("skipping: KIN_INFER_QWEN3_DIR is not set");
        return;
    };
    let fixture = fixture();
    let case = fixture.cases.last().unwrap();
    let ids = &case.token_ids;
    let split = question_split(case).unwrap_or(ids.len() - 24);
    let prefix = model.prefill(&ids[..split]).unwrap();
    let bytes_before = prefix.cache_bytes();

    let first = model
        .score_continuation(&prefix, &ids[split..], &case.candidate_ids)
        .unwrap();
    // A different continuation of the same state, then the first one again.
    let other_suffix: Vec<u32> = ids[split..].iter().rev().copied().collect();
    let other = model
        .score_continuation(&prefix, &other_suffix, &case.candidate_ids)
        .unwrap();
    let again = model
        .score_continuation(&prefix, &ids[split..], &case.candidate_ids)
        .unwrap();

    assert_eq!(prefix.len(), split);
    assert_eq!(prefix.cache_bytes(), bytes_before);
    assert_eq!(first, again, "a continuation changed the prefix state");
    assert_ne!(first, other, "the second continuation was not scored");
    let mut reversed = ids[..split].to_vec();
    reversed.extend(&other_suffix);
    let other_whole = model
        .score_next_tokens_reference(&reversed, &case.candidate_ids)
        .unwrap();
    assert!(max_diff(&other, &other_whole) < TOLERANCE);

    // An empty suffix scores the prefix's own last position.
    let at_prefix_end = model
        .score_continuation(&prefix, &[], &case.candidate_ids)
        .unwrap();
    let prefix_whole = model
        .score_next_tokens_reference(&ids[..split], &case.candidate_ids)
        .unwrap();
    assert!(max_diff(&at_prefix_end, &prefix_whole) < TOLERANCE);
}

#[test]
fn batch_matches_scoring_each_request_on_cpu() {
    let Some(model) = model() else {
        eprintln!("skipping: KIN_INFER_QWEN3_DIR is not set");
        return;
    };
    let fixture = fixture();
    let long = fixture.cases.last().unwrap();
    let split = question_split(long).unwrap_or(long.token_ids.len() - 24);
    let prefix = model.prefill(&long.token_ids[..split]).unwrap();
    let suffix = &long.token_ids[split..];
    let short_suffix = &suffix[..suffix.len() / 2];

    let mut requests: Vec<ScoreRequest<'_>> = fixture
        .cases
        .iter()
        .map(|c| ScoreRequest::new(&c.token_ids, &c.candidate_ids))
        .collect();
    requests.push(ScoreRequest::continuation(
        &prefix,
        suffix,
        &long.candidate_ids,
    ));
    requests.push(ScoreRequest::continuation(
        &prefix,
        short_suffix,
        &long.candidate_ids,
    ));
    requests.push(ScoreRequest::continuation(
        &prefix,
        &[],
        &long.candidate_ids,
    ));

    let expected: Vec<Vec<f32>> = requests
        .iter()
        .map(|r| match r.prefix() {
            Some(p) => model
                .score_continuation(p, r.token_ids(), r.candidate_ids())
                .unwrap(),
            None => model
                .score_next_tokens(r.token_ids(), r.candidate_ids())
                .unwrap(),
        })
        .collect();

    let runs = [
        ("default", BatchOptions::default()),
        (
            "one at a time",
            BatchOptions {
                max_in_flight: 1,
                ..BatchOptions::default()
            },
        ),
        (
            "all at once",
            BatchOptions {
                max_in_flight: requests.len(),
                ..BatchOptions::default()
            },
        ),
        (
            "one-byte budget",
            BatchOptions {
                max_in_flight: requests.len(),
                memory_budget_bytes: 1,
            },
        ),
    ];
    for (name, options) in runs {
        let t = Instant::now();
        let got = model
            .score_next_tokens_batch_with(&requests, &options)
            .unwrap();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(got.len(), requests.len());
        let worst = got
            .iter()
            .zip(&expected)
            .map(|(g, e)| max_diff(g, e))
            .fold(0.0f32, f32::max);
        let identical = got == expected;
        eprintln!(
            "batch {name}: {} requests in {ms:.0} ms, max |logit diff| {worst:e}, \
             bit-identical {identical}",
            requests.len()
        );
        assert!(
            worst < TOLERANCE,
            "batch {name} differs from per-request scoring by {worst}"
        );
    }
    // The continuation of the question-split prefix is the whole long prompt.
    let long_whole = model
        .score_next_tokens_reference(&long.token_ids, &long.candidate_ids)
        .unwrap();
    assert!(max_diff(&expected[3], &long_whole) < TOLERANCE);
    assert!(max_diff(&expected[3], &expected[2]) < FAST_PATH_TOLERANCE);
}

#[test]
fn batch_refuses_bad_requests_before_scoring() {
    let Some(model) = model() else {
        eprintln!("skipping: KIN_INFER_QWEN3_DIR is not set");
        return;
    };
    let vocab = model.config.vocab_size as u32;
    assert!(model.score_next_tokens_batch(&[]).unwrap().is_empty());

    let good = ScoreRequest::new(&[785, 6722, 315], &[12095]);
    let outside = [vocab + 10];
    for bad in [
        ScoreRequest::new(&[], &[12095]),
        ScoreRequest::new(&[785, 6722, 315], &outside),
        ScoreRequest::new(&outside, &[12095]),
    ] {
        assert!(model.score_next_tokens_batch(&[good, bad]).is_err());
    }
    let empty = model.prefill(&[]).unwrap();
    assert!(empty.is_empty());
    assert!(model.score_continuation(&empty, &[], &[12095]).is_err());
    assert!(model.prefill(&outside).is_err());
}
