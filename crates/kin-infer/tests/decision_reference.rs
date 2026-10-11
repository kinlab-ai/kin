// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

#![cfg(feature = "engine")]

//! Typed decisions match the reference Python template and float32 transformers.
//!
//! `tests/fixtures/decision_reference.json` is written by `scripts/decision_reference.py`. For
//! each example question it holds the prompt text of every scored option order, the prompt's
//! token ids and letter ids from the Hugging Face tokenizer, the letter logits float32
//! transformers computes, and the option probabilities for one order, two averaged orders, and
//! two averaged orders at temperature 2.
//!
//! The prompt and probability tests always run. The model tests need the `decision` feature and
//! run only when `KIN_INFER_QWEN3_DIR` names the Qwen3 directory the fixture was written from,
//! holding `config.json`, `model.safetensors` and `tokenizer.json`.

use kin_infer::decision::CALL_TARGET_NONE_OF_THESE;
use kin_infer::{Choice, ChoiceQuestion, Decision};

#[derive(serde::Deserialize)]
struct Probabilities {
    single: Vec<f64>,
    averaged: Vec<f64>,
    averaged_temperature_2: Vec<f64>,
}

#[derive(serde::Deserialize)]
#[allow(dead_code)] // token and letter ids are read only by the model tests
struct Case {
    name: String,
    state: String,
    question: String,
    options: Vec<String>,
    none_text: Option<String>,
    prompts: Vec<String>,
    token_ids: Vec<Vec<u32>>,
    letter_logits: Vec<Vec<f32>>,
    probabilities: Probabilities,
}

#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct Fixture {
    letter_ids: Vec<u32>,
    cases: Vec<Case>,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/decision_reference.json")).unwrap()
}

fn question(case: &Case) -> ChoiceQuestion {
    ChoiceQuestion {
        question: case.question.clone(),
        options: case.options.clone(),
        none_of_these: case.none_text.clone(),
    }
}

/// All choice probabilities of a decision in question order, "none of these" last.
fn all_probabilities(d: &Decision) -> Vec<f64> {
    d.probabilities
        .iter()
        .chain(&d.none_probability)
        .map(|&p| f64::from(p))
        .collect()
}

fn max_diff(got: &[f64], want: &[f64]) -> f64 {
    assert_eq!(got.len(), want.len());
    got.iter()
        .zip(want)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f64::max)
}

/// The reference pick: the first most probable choice.
fn reference_choice(case: &Case, probabilities: &[f64]) -> Choice {
    let best = (1..probabilities.len()).fold(0, |best, i| {
        if probabilities[i] > probabilities[best] {
            i
        } else {
            best
        }
    });
    if best < case.options.len() {
        Choice::Option(best)
    } else {
        Choice::NoneOfThese
    }
}

#[test]
fn prompts_match_the_reference_template_byte_for_byte() {
    let fixture = fixture();
    assert!(fixture.cases.len() >= 4);
    for case in &fixture.cases {
        let q = question(case);
        let orders = q.orders(true);
        assert_eq!(orders.len(), case.prompts.len(), "{}", case.name);
        assert_eq!(
            q.prompt(&case.state).unwrap(),
            case.prompts[0],
            "{}",
            case.name
        );
        for (order, want) in orders.iter().zip(&case.prompts) {
            let got = q.prompt_for_order(&case.state, order).unwrap();
            assert_eq!(got, *want, "{}: order {order:?}", case.name);
        }
    }
}

#[test]
fn call_target_questions_match_the_reference_wording() {
    let fixture = fixture();
    let calls: Vec<&Case> = fixture
        .cases
        .iter()
        .filter(|c| c.none_text.as_deref() == Some(CALL_TARGET_NONE_OF_THESE))
        .collect();
    assert!(calls.len() >= 2);
    for case in calls {
        let callee = case.question.split('`').nth(1).unwrap();
        let q = ChoiceQuestion::call_target(callee, case.options.clone());
        assert_eq!(q, question(case), "{}", case.name);
    }
}

#[test]
fn reference_logits_combine_to_reference_probabilities() {
    for case in &fixture().cases {
        let q = question(case);
        let orders = q.orders(true);
        let logits = &case.letter_logits;
        let checks = [
            (
                "single",
                &orders[..1],
                &logits[..1],
                1.0,
                &case.probabilities.single,
            ),
            (
                "averaged",
                &orders[..],
                &logits[..],
                1.0,
                &case.probabilities.averaged,
            ),
            (
                "averaged at temperature 2",
                &orders[..],
                &logits[..],
                2.0,
                &case.probabilities.averaged_temperature_2,
            ),
        ];
        for (label, orders, logits, temperature, want) in checks {
            let d = Decision::from_letter_logits(&q, orders, logits, temperature).unwrap();
            let diff = max_diff(&all_probabilities(&d), want);
            assert!(
                diff < 1e-6,
                "{} {label}: probabilities differ by {diff}",
                case.name
            );
            assert_eq!(
                d.choice,
                reference_choice(case, want),
                "{} {label}",
                case.name
            );
            assert_eq!(Some(d.confidence), d.probability(d.choice));
        }
    }
}

#[cfg(feature = "decision")]
mod model {
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::time::Instant;

    use kin_infer::{DecisionModel, DecisionSettings};

    use super::*;

    /// The float32 CPU forward pass agrees with transformers to about 1e-4 in the logits, so
    /// probabilities agree well inside this.
    const TOLERANCE: f64 = 1e-3;

    /// The model, loaded once on the CPU, or `None` when `KIN_INFER_QWEN3_DIR` is unset.
    fn model() -> Option<&'static DecisionModel> {
        static MODEL: OnceLock<Option<DecisionModel>> = OnceLock::new();
        MODEL
            .get_or_init(|| {
                let dir = PathBuf::from(std::env::var_os("KIN_INFER_QWEN3_DIR")?);
                let t0 = Instant::now();
                let model = DecisionModel::load_with_backend(&dir, true).unwrap();
                eprintln!("loaded in {:.1}s", t0.elapsed().as_secs_f32());
                Some(model)
            })
            .as_ref()
    }

    #[test]
    fn letter_ids_match_the_reference_tokenizer() {
        let Some(model) = model() else {
            eprintln!("skipping: KIN_INFER_QWEN3_DIR is not set");
            return;
        };
        assert_eq!(model.letter_ids(), fixture().letter_ids.as_slice());
    }

    #[test]
    fn prompt_token_ids_match_the_reference_tokenizer() {
        let Some(model) = model() else {
            eprintln!("skipping: KIN_INFER_QWEN3_DIR is not set");
            return;
        };
        for case in &fixture().cases {
            for (prompt, want) in case.prompts.iter().zip(&case.token_ids) {
                assert_eq!(&model.encode_prompt(prompt).unwrap(), want, "{}", case.name);
            }
        }
    }

    #[test]
    fn decide_matches_transformers_probabilities() {
        let Some(model) = model() else {
            eprintln!("skipping: KIN_INFER_QWEN3_DIR is not set");
            return;
        };
        let fixture = fixture();
        let cases = ["rust-call-with-none", "typescript-call-with-none"];
        for name in cases {
            let case = fixture.cases.iter().find(|c| c.name == name).unwrap();
            let q = question(case);
            let runs = [
                (
                    "single",
                    DecisionSettings::default(),
                    &case.probabilities.single,
                ),
                (
                    "averaged",
                    DecisionSettings {
                        temperature: 1.0,
                        average_orders: true,
                    },
                    &case.probabilities.averaged,
                ),
                (
                    "averaged at temperature 2",
                    DecisionSettings {
                        temperature: 2.0,
                        average_orders: true,
                    },
                    &case.probabilities.averaged_temperature_2,
                ),
            ];
            for (label, settings, want) in runs {
                let t = Instant::now();
                let d = model.decide_with(&case.state, &q, settings).unwrap();
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                let got = all_probabilities(&d);
                let diff = max_diff(&got, want);
                eprintln!(
                    "{name} {label}: {ms:.0} ms, max |p diff| {diff:.2e}, {:?} at {:.3}",
                    d.choice, d.confidence
                );
                assert!(
                    diff < TOLERANCE,
                    "{name} {label}: probabilities differ by {diff}"
                );
                assert_eq!(d.choice, reference_choice(case, want), "{name} {label}");
            }
        }
    }

    #[test]
    fn decide_uses_the_model_settings() {
        let Some(model) = model() else {
            eprintln!("skipping: KIN_INFER_QWEN3_DIR is not set");
            return;
        };
        let fixture = fixture();
        let case = fixture
            .cases
            .iter()
            .find(|c| c.name == "label-with-default-none")
            .unwrap();
        assert_eq!(model.settings(), DecisionSettings::default());
        let d = model.decide(&case.state, &question(case)).unwrap();
        let diff = max_diff(&all_probabilities(&d), &case.probabilities.single);
        assert!(diff < TOLERANCE, "probabilities differ by {diff}");
        let too_many = ChoiceQuestion::new("q", (0..30).map(|i| i.to_string()).collect());
        assert!(model.decide(&case.state, &too_many).is_err());
    }
}
