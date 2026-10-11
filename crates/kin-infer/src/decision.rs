// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Typed decisions: ask a small decoder model which of a few options, or none of them, fits.
//!
//! A decision is a multiple-choice question over a state the caller describes in text. The prompt
//! lists every option under a letter, with an optional "none of these" choice last, and ends in
//! `Answer:`. The model's next-token logits for the letter tokens `" a"`, `" b"`, ... become a
//! probability for every choice through a softmax with a temperature. The prompt reads:
//!
//! ```text
//! <state>
//!
//! Question: <question>
//! a. <first option>
//! b. <second option>
//! c. none of these
//! Answer:
//! ```
//!
//! A model tends to favor the first letters it sees. Order averaging scores the options a second
//! time in reverse order, with "none of these" still last, and averages the two distributions
//! option by option.
//!
//! Three kinds of question share that prompt:
//!
//! - a [`ChoiceQuestion`] lists option texts and answers with a [`Decision`];
//! - a [`TypedQuestion`] pairs each option text with a caller value and answers with that value,
//!   and [`TypedQuestion::yes_no`] asks a yes-or-no question the same way;
//! - a [`CandidateQuery`] scores any number of candidate texts against a query and answers with
//!   a probability for every candidate and for "none of these" ([`CandidateScores`]). Candidates
//!   beyond the options one question holds are asked in several questions, and their answers
//!   combine into one distribution.
//!
//! [`Verdict`] reads an answer against a confidence floor, so a caller can act on a confident
//! answer and abstain on the rest.
//!
//! The question, prompt and probability code is plain Rust and always compiled. `DecisionModel`,
//! which loads a model directory with its `tokenizer.json` and scores prompts, and
//! `DecisionEngine`, which loads a trained model from a configured local directory and answers
//! all three kinds of question, need the `decision` feature. Nothing here downloads a model.

use crate::InferError;

mod candidates;
#[cfg(feature = "decision")]
mod engine;
#[cfg(feature = "decision")]
mod pointer;
#[cfg(feature = "decision")]
mod reranker;
mod typed;

pub use candidates::{
    CandidateQuery, CandidateScores, DEFAULT_CANDIDATE_QUESTION, DEFAULT_OPTIONS_PER_QUESTION,
};
#[cfg(feature = "decision")]
pub use engine::{
    DecisionDevice, DecisionEngine, DecisionEngineConfig, EngineSettings, StudentManifest,
    DECISION_MODEL_DIR_ENV, MANIFEST_FILE,
};
#[cfg(feature = "decision")]
pub use pointer::{
    defuse_special_tokens, unsupported_backbone, PointerCheckpoint, PointerDecisionModel,
    PointerQuestion, PointerRecord, PointerRow, POINTER_DELIMITERS, TRAINING_MAX_ROW_TOKENS,
    TRAINING_MAX_STATE_TOKENS,
};
#[cfg(feature = "decision")]
pub use reranker::{
    YesNoReranker, QWEN3_RERANKER_DEFAULT_INSTRUCTION, QWEN3_RERANKER_MAX_TOKENS,
    QWEN3_RERANKER_PREFIX, QWEN3_RERANKER_SUFFIX,
};
pub use typed::{TypedAnswer, TypedQuestion};

/// Option letters in the order they are assigned. A question has at most this many choices,
/// counting "none of these".
pub const LETTERS: &str = "abcdefghijklmnopqrstuvwxyz";

/// The most choices a question can have, counting "none of these".
pub const MAX_CHOICES: usize = LETTERS.len();

/// The plain "none of these" choice text.
pub const NONE_OF_THESE: &str = "none of these";

/// The "none of these" text of a call-target question (see [`ChoiceQuestion::call_target`]).
pub const CALL_TARGET_NONE_OF_THESE: &str =
    "none of these: the call runs a definition not listed here (another type, a library, or a builtin)";

/// Why a decision could not be made.
#[derive(Debug, thiserror::Error)]
pub enum DecisionError {
    /// The question, its option order or the settings cannot be asked as given.
    #[error("invalid decision input: {0}")]
    InvalidInput(String),
    /// A model directory is missing a file or holds one that does not parse.
    #[error("cannot load decision model: {0}")]
    Load(String),
    /// The tokenizer failed on a prompt or a letter.
    #[error("tokenizer error: {0}")]
    Tokenizer(String),
    /// The prompt is longer than the model's context window.
    #[error("prompt of {tokens} tokens exceeds the model's {limit}-token window")]
    PromptTooLong { tokens: usize, limit: usize },
    /// The model failed to run, or produced a non-finite logit.
    #[error(transparent)]
    Infer(#[from] InferError),
}

/// A multiple-choice question over a state.
///
/// Every text is used verbatim, so each must be a single line: a line break inside an option
/// could pass for another lettered line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceQuestion {
    /// The question, without the `Question: ` prefix.
    pub question: String,
    /// The option texts, in the order they are shown first.
    pub options: Vec<String>,
    /// The text of a "none of these" choice listed after every option, or `None` when the model
    /// must pick one of the options.
    pub none_of_these: Option<String>,
}

impl ChoiceQuestion {
    /// A question that must be answered with one of `options`.
    pub fn new(question: impl Into<String>, options: Vec<String>) -> Self {
        Self {
            question: question.into(),
            options,
            none_of_these: None,
        }
    }

    /// Appends the plain [`NONE_OF_THESE`] choice after the options.
    pub fn with_none_of_these(self) -> Self {
        self.with_none_text(NONE_OF_THESE)
    }

    /// Appends a "none of these" choice with a caller-chosen text after the options.
    pub fn with_none_text(mut self, text: impl Into<String>) -> Self {
        self.none_of_these = Some(text.into());
        self
    }

    /// Which of `options` does a call to `callee` execute, or none of them.
    ///
    /// The state should describe the call site and each option one candidate definition. The
    /// question and its "none of these" text are fixed, so a model tuned on this wording sees it
    /// exactly.
    pub fn call_target(callee: &str, options: Vec<String>) -> Self {
        Self::new(
            format!("Which definition does the call to `{callee}` at this call site execute?"),
            options,
        )
        .with_none_text(CALL_TARGET_NONE_OF_THESE)
    }

    /// The number of choices, counting "none of these".
    pub fn choice_count(&self) -> usize {
        self.options.len() + usize::from(self.none_of_these.is_some())
    }

    /// Checks that the question can be asked: at least one option, at most [`MAX_CHOICES`]
    /// choices, and no line break in any text.
    pub fn validate(&self) -> Result<(), DecisionError> {
        if self.options.is_empty() {
            return Err(DecisionError::InvalidInput(
                "a question needs at least one option".into(),
            ));
        }
        if self.choice_count() > MAX_CHOICES {
            return Err(DecisionError::InvalidInput(format!(
                "{} choices exceed the {MAX_CHOICES} option letters",
                self.choice_count()
            )));
        }
        let texts = std::iter::once(&self.question)
            .chain(&self.options)
            .chain(&self.none_of_these);
        for text in texts {
            if text.contains(['\n', '\r']) {
                return Err(DecisionError::InvalidInput(format!(
                    "question and option texts must be single lines: {text:?}"
                )));
            }
        }
        Ok(())
    }

    /// The text of choice `index`, where `options.len()` is "none of these".
    fn choice_text(&self, index: usize) -> &str {
        match self.options.get(index) {
            Some(text) => text,
            None => self.none_of_these.as_deref().unwrap_or_default(),
        }
    }

    /// The option orders a decision scores, as lists of choice indices where `options.len()`
    /// stands for "none of these".
    ///
    /// The first order is the shown one. With `average_orders`, a second order lists the options
    /// in reverse with "none of these" still last. A single option has only one order.
    pub fn orders(&self, average_orders: bool) -> Vec<Vec<usize>> {
        let n = self.options.len();
        let none = self.none_of_these.as_ref().map(|_| n);
        let shown: Vec<usize> = (0..n).chain(none).collect();
        let reversed: Vec<usize> = (0..n).rev().chain(none).collect();
        if average_orders && reversed != shown {
            vec![shown, reversed]
        } else {
            vec![shown]
        }
    }

    /// The prompt for the shown order.
    pub fn prompt(&self, state: &str) -> Result<String, DecisionError> {
        self.validate()?;
        let shown: Vec<usize> = (0..self.choice_count()).collect();
        Ok(self.render(state, &shown))
    }

    /// The prompt that lists the choices in `order`, a permutation of the choice indices.
    pub fn prompt_for_order(&self, state: &str, order: &[usize]) -> Result<String, DecisionError> {
        self.validate()?;
        check_order(order, self.choice_count())?;
        Ok(self.render(state, order))
    }

    fn render(&self, state: &str, order: &[usize]) -> String {
        let mut prompt = format!("{state}\n\nQuestion: {}\n", self.question);
        for (letter, &choice) in LETTERS.chars().zip(order) {
            prompt.push(letter);
            prompt.push_str(". ");
            prompt.push_str(self.choice_text(choice));
            prompt.push('\n');
        }
        prompt.push_str("Answer:");
        prompt
    }
}

fn check_order(order: &[usize], choices: usize) -> Result<(), DecisionError> {
    let mut seen = vec![false; choices];
    let is_permutation = order.len() == choices
        && order
            .iter()
            .all(|&i| i < choices && !std::mem::replace(&mut seen[i], true));
    if is_permutation {
        Ok(())
    } else {
        Err(DecisionError::InvalidInput(format!(
            "order {order:?} is not a permutation of {choices} choices"
        )))
    }
}

/// How a decision turns letter logits into probabilities.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecisionSettings {
    /// Divides the letter logits before the softmax. 1.0 keeps the model's own distribution; a
    /// value fitted on held-out answers, usually above 1, calibrates an overconfident model.
    pub temperature: f32,
    /// Also score the options in reverse order and average the two distributions. It costs a
    /// second forward pass.
    pub average_orders: bool,
}

impl Default for DecisionSettings {
    fn default() -> Self {
        Self {
            temperature: 1.0,
            average_orders: false,
        }
    }
}

impl DecisionSettings {
    fn validate(&self) -> Result<(), DecisionError> {
        if self.temperature.is_finite() && self.temperature > 0.0 {
            Ok(())
        } else {
            Err(DecisionError::InvalidInput(format!(
                "temperature must be finite and positive, got {}",
                self.temperature
            )))
        }
    }
}

/// The choice a decision picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// The option at this index of [`ChoiceQuestion::options`].
    Option(usize),
    /// The "none of these" choice.
    NoneOfThese,
}

/// An answer read against a confidence floor.
///
/// A calibrated model is right about as often as its confidence says, so a floor fitted on
/// held-out answers bounds the error rate of the answers a caller acts on. Below the floor the
/// model abstains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict<T> {
    /// The most probable choice is this option, at or above the floor.
    Chosen(T),
    /// "None of these" is the most probable choice, at or above the floor.
    NoneOfThese,
    /// No choice reaches the floor.
    Unsure,
}

impl<T> Verdict<T> {
    /// Reads `choice` at `confidence` against `min_confidence`, naming an option through
    /// `option`.
    pub(crate) fn read(
        choice: Choice,
        confidence: f32,
        min_confidence: f32,
        option: impl FnOnce(usize) -> T,
    ) -> Self {
        // A floor that is not a number abstains rather than accepting everything.
        match confidence.partial_cmp(&min_confidence) {
            Some(std::cmp::Ordering::Less) | None => return Self::Unsure,
            Some(_) => {}
        }
        match choice {
            Choice::Option(i) => Self::Chosen(option(i)),
            Choice::NoneOfThese => Self::NoneOfThese,
        }
    }

    /// The chosen option, or `None` for "none of these" and for an abstention.
    pub fn chosen(self) -> Option<T> {
        match self {
            Self::Chosen(value) => Some(value),
            Self::NoneOfThese | Self::Unsure => None,
        }
    }
}

/// The answer to a [`ChoiceQuestion`].
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// The probability of each option, in [`ChoiceQuestion::options`] order.
    pub probabilities: Vec<f32>,
    /// The probability of "none of these", when the question offers it.
    pub none_probability: Option<f32>,
    /// The most probable choice. A tie goes to the earlier option, and "none of these" last.
    pub choice: Choice,
    /// The probability of `choice`.
    pub confidence: f32,
}

impl Decision {
    /// The probability of `choice`, or `None` when the question does not offer it.
    pub fn probability(&self, choice: Choice) -> Option<f32> {
        match choice {
            Choice::Option(i) => self.probabilities.get(i).copied(),
            Choice::NoneOfThese => self.none_probability,
        }
    }

    /// The decision read against `min_confidence`: the most probable choice when its
    /// probability reaches the floor, and [`Verdict::Unsure`] otherwise. Options are named by
    /// their index in [`ChoiceQuestion::options`].
    pub fn verdict(&self, min_confidence: f32) -> Verdict<usize> {
        Verdict::read(self.choice, self.confidence, min_confidence, |i| i)
    }

    /// Builds a decision from the letter logits of each scored order.
    ///
    /// `letter_logits[j]` holds the logits of `" a"`, `" b"`, ... after the prompt for
    /// `orders[j]`, so its entry at position `p` belongs to choice `orders[j][p]`. Each order's
    /// logits go through a softmax at `temperature`, and the distributions are averaged per
    /// choice. `DecisionModel::decide` calls this with [`ChoiceQuestion::orders`]; it is public
    /// so logits scored elsewhere combine the same way.
    pub fn from_letter_logits(
        question: &ChoiceQuestion,
        orders: &[Vec<usize>],
        letter_logits: &[Vec<f32>],
        temperature: f32,
    ) -> Result<Self, DecisionError> {
        let total = choice_distribution(question, orders, letter_logits, temperature)?;
        let best = first_max(&total);
        let n = question.options.len();
        Ok(Self {
            probabilities: total[..n].iter().map(|&p| p as f32).collect(),
            none_probability: total.get(n).map(|&p| p as f32),
            choice: if best < n {
                Choice::Option(best)
            } else {
                Choice::NoneOfThese
            },
            confidence: total[best] as f32,
        })
    }
}

/// The probability of every choice of `question`, "none of these" last, in f64: each order's
/// letter logits through a softmax at `temperature`, averaged per choice over the orders.
pub(crate) fn choice_distribution(
    question: &ChoiceQuestion,
    orders: &[Vec<usize>],
    letter_logits: &[Vec<f32>],
    temperature: f32,
) -> Result<Vec<f64>, DecisionError> {
    question.validate()?;
    DecisionSettings {
        temperature,
        average_orders: false,
    }
    .validate()?;
    if orders.is_empty() || orders.len() != letter_logits.len() {
        return Err(DecisionError::InvalidInput(format!(
            "{} orders need as many logit rows, got {}",
            orders.len(),
            letter_logits.len()
        )));
    }
    let choices = question.choice_count();
    let share = 1.0 / orders.len() as f64;
    let mut total = vec![0.0f64; choices];
    for (order, logits) in orders.iter().zip(letter_logits) {
        check_order(order, choices)?;
        if logits.len() != choices {
            return Err(DecisionError::InvalidInput(format!(
                "{choices} choices need as many letter logits, got {}",
                logits.len()
            )));
        }
        if let Some(bad) = logits.iter().find(|l| !l.is_finite()) {
            return Err(InferError::NonFiniteOutput(format!("letter logit {bad}")).into());
        }
        for (&choice, p) in order.iter().zip(softmax(logits, temperature)) {
            total[choice] += p * share;
        }
    }
    Ok(total)
}

/// The index of the first largest value, so a tie goes to the earlier entry.
pub(crate) fn first_max(values: &[f64]) -> usize {
    (1..values.len()).fold(0, |best, i| if values[i] > values[best] { i } else { best })
}

/// Softmax of `logits / temperature`, computed in f64.
fn softmax(logits: &[f32], temperature: f32) -> Vec<f64> {
    let t = f64::from(temperature);
    let scaled: Vec<f64> = logits.iter().map(|&l| f64::from(l) / t).collect();
    let max = scaled.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let exp: Vec<f64> = scaled.iter().map(|&s| (s - max).exp()).collect();
    let sum: f64 = exp.iter().sum();
    exp.into_iter().map(|e| e / sum).collect()
}

#[cfg(feature = "decision")]
pub use model::{DecisionModel, PromptItem};

#[cfg(feature = "decision")]
mod model {
    use std::path::Path;

    use tokenizers::Tokenizer;

    use super::{ChoiceQuestion, Decision, DecisionError, DecisionSettings, LETTERS};
    use crate::{gpu, BertConfig, BertModel, InferError, ScoreRequest};

    /// One prompt to score: a state, a question about it, and the order its choices are listed
    /// in (see [`ChoiceQuestion::orders`]).
    pub type PromptItem<'a> = (&'a str, &'a ChoiceQuestion, &'a [usize]);

    /// A decoder model and its tokenizer, ready to answer [`ChoiceQuestion`]s.
    pub struct DecisionModel {
        model: BertModel,
        tokenizer: Tokenizer,
        letter_ids: Vec<u32>,
        settings: DecisionSettings,
        max_prompt_tokens: Option<usize>,
    }

    impl DecisionModel {
        /// Loads `config.json`, `model.safetensors` and `tokenizer.json` from a model directory.
        ///
        /// Honors `KIN_INFER_FORCE_CPU`, like [`BertModel::load`].
        pub fn load(dir: &Path) -> Result<Self, DecisionError> {
            Self::load_with_backend(dir, gpu::force_cpu_from_env())
        }

        /// Loads a model directory on the CPU when `force_cpu` is true, and on the best
        /// available backend otherwise, like [`BertModel::load_with_backend`].
        pub fn load_with_backend(dir: &Path, force_cpu: bool) -> Result<Self, DecisionError> {
            let config_path = dir.join("config.json");
            let config_text = std::fs::read_to_string(&config_path)
                .map_err(|e| DecisionError::Load(format!("{}: {e}", config_path.display())))?;
            let config: BertConfig = serde_json::from_str(&config_text)
                .map_err(|e| DecisionError::Load(format!("{}: {e}", config_path.display())))?;
            if !config.architecture().is_decoder_only() {
                return Err(InferError::ModelIncompatible(
                    "a decision model must be decoder-only".into(),
                )
                .into());
            }
            let weights = dir.join("model.safetensors");
            if !weights.is_file() {
                return Err(DecisionError::Load(format!(
                    "{} is missing; sharded checkpoints are not supported here",
                    weights.display()
                )));
            }
            let tokenizer_path = dir.join("tokenizer.json");
            let tokenizer = Tokenizer::from_file(&tokenizer_path)
                .map_err(|e| DecisionError::Load(format!("{}: {e}", tokenizer_path.display())))?;
            let model = BertModel::load_with_backend(&weights, config, force_cpu)?;
            Self::from_parts(model, tokenizer)
        }

        /// Pairs a loaded decoder model with its tokenizer.
        ///
        /// Truncation and padding are turned off, so a prompt is tokenized whole. Fails when the
        /// model is not decoder-only or when fewer than two letters encode as single tokens.
        pub fn from_parts(
            model: BertModel,
            mut tokenizer: Tokenizer,
        ) -> Result<Self, DecisionError> {
            if !model.config.architecture().is_decoder_only() {
                return Err(InferError::ModelIncompatible(
                    "a decision model must be decoder-only".into(),
                )
                .into());
            }
            tokenizer
                .with_truncation(None)
                .map_err(|e| DecisionError::Tokenizer(e.to_string()))?;
            tokenizer.with_padding(None);
            let letter_ids = letter_token_ids(&tokenizer)?;
            if letter_ids.len() < 2 {
                return Err(DecisionError::Load(
                    "the tokenizer does not encode \" a\" and \" b\" as single tokens".into(),
                ));
            }
            Ok(Self {
                model,
                tokenizer,
                letter_ids,
                settings: DecisionSettings::default(),
                max_prompt_tokens: None,
            })
        }

        /// Refuses prompts longer than `limit` tokens, or than the model's window when that is
        /// smaller. A model tuned on prompts cut at some length has never seen a longer one, so
        /// a caller sets that length here rather than score a prompt the model reads blind.
        pub fn with_max_prompt_tokens(mut self, limit: usize) -> Self {
            self.max_prompt_tokens = Some(limit);
            self
        }

        /// The longest prompt, in tokens, this model scores.
        pub fn max_prompt_tokens(&self) -> usize {
            let window = self.model.config.effective_max_seq_len();
            self.max_prompt_tokens
                .map_or(window, |limit| limit.min(window))
        }

        /// Sets the softmax temperature every later [`decide`](Self::decide) uses.
        pub fn with_temperature(mut self, temperature: f32) -> Self {
            self.settings.temperature = temperature;
            self
        }

        /// Turns order averaging on or off for every later [`decide`](Self::decide).
        pub fn with_order_averaging(mut self, average_orders: bool) -> Self {
            self.settings.average_orders = average_orders;
            self
        }

        /// The settings [`decide`](Self::decide) uses.
        pub fn settings(&self) -> DecisionSettings {
            self.settings
        }

        /// Replaces the settings [`decide`](Self::decide) uses.
        pub fn set_settings(&mut self, settings: DecisionSettings) {
            self.settings = settings;
        }

        /// The underlying model.
        pub fn model(&self) -> &BertModel {
            &self.model
        }

        /// The token ids of `" a"`, `" b"`, ... for every letter the tokenizer encodes as one
        /// token, stopping at the first that it does not. A question can have at most this many
        /// choices on this model.
        pub fn letter_ids(&self) -> &[u32] {
            &self.letter_ids
        }

        /// The token ids of a prompt, with any special tokens the tokenizer adds to a sequence.
        pub fn encode_prompt(&self, prompt: &str) -> Result<Vec<u32>, DecisionError> {
            let encoding = self
                .tokenizer
                .encode(prompt, true)
                .map_err(|e| DecisionError::Tokenizer(e.to_string()))?;
            Ok(encoding.get_ids().to_vec())
        }

        /// The letter logits after the prompt that lists the choices in `order`, one per choice
        /// in the order they are listed.
        pub fn letter_logits(
            &self,
            state: &str,
            question: &ChoiceQuestion,
            order: &[usize],
        ) -> Result<Vec<f32>, DecisionError> {
            let mut logits = self.letter_logits_batch(&[(state, question, order)])?;
            Ok(logits.pop().unwrap_or_default())
        }

        /// The letter logits of several prompts, scored as one batch.
        ///
        /// Each result is what [`letter_logits`](Self::letter_logits) returns for its item. Every
        /// prompt is rendered, tokenized and checked against the prompt limit before any is
        /// scored, so a bad item fails the batch without spending compute. On the CPU several
        /// prompts run at once (see [`BertModel::score_next_tokens_batch`]).
        pub fn letter_logits_batch(
            &self,
            items: &[PromptItem<'_>],
        ) -> Result<Vec<Vec<f32>>, DecisionError> {
            let limit = self.max_prompt_tokens();
            let mut encoded = Vec::with_capacity(items.len());
            for &(state, question, order) in items {
                let choices = question.choice_count();
                if choices > self.letter_ids.len() {
                    return Err(DecisionError::InvalidInput(format!(
                        "{choices} choices exceed the {} single-token letters of this tokenizer",
                        self.letter_ids.len()
                    )));
                }
                let prompt = question.prompt_for_order(state, order)?;
                let ids = self.encode_prompt(&prompt)?;
                if ids.len() > limit {
                    return Err(DecisionError::PromptTooLong {
                        tokens: ids.len(),
                        limit,
                    });
                }
                encoded.push((ids, choices));
            }
            let requests: Vec<ScoreRequest<'_>> = encoded
                .iter()
                .map(|(ids, choices)| ScoreRequest::new(ids, &self.letter_ids[..*choices]))
                .collect();
            Ok(self.model.score_next_tokens_batch(&requests)?)
        }

        /// Answers `question` about `state` with the model's settings.
        pub fn decide(
            &self,
            state: &str,
            question: &ChoiceQuestion,
        ) -> Result<Decision, DecisionError> {
            self.decide_with(state, question, self.settings)
        }

        /// Answers `question` about `state` with the given settings.
        pub fn decide_with(
            &self,
            state: &str,
            question: &ChoiceQuestion,
            settings: DecisionSettings,
        ) -> Result<Decision, DecisionError> {
            settings.validate()?;
            question.validate()?;
            let orders = question.orders(settings.average_orders);
            let items: Vec<PromptItem<'_>> = orders
                .iter()
                .map(|order| (state, question, order.as_slice()))
                .collect();
            let logits = self.letter_logits_batch(&items)?;
            Decision::from_letter_logits(question, &orders, &logits, settings.temperature)
        }
    }

    /// The single-token ids of `" a"`, `" b"`, ..., up to the first letter that is not one
    /// token or repeats an earlier id.
    fn letter_token_ids(tokenizer: &Tokenizer) -> Result<Vec<u32>, DecisionError> {
        let mut ids = Vec::with_capacity(LETTERS.len());
        for letter in LETTERS.chars() {
            let encoding = tokenizer
                .encode(format!(" {letter}"), false)
                .map_err(|e| DecisionError::Tokenizer(e.to_string()))?;
            match encoding.get_ids() {
                [id] if !ids.contains(id) => ids.push(*id),
                _ => break,
            }
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question() -> ChoiceQuestion {
        ChoiceQuestion::new(
            "Which one?",
            vec!["first".into(), "second".into(), "third".into()],
        )
    }

    #[test]
    fn prompt_letters_every_choice_and_lists_none_last() {
        let q = question().with_none_of_these();
        assert_eq!(
            q.prompt("state").unwrap(),
            "state\n\nQuestion: Which one?\na. first\nb. second\nc. third\nd. none of these\nAnswer:"
        );
        let reversed = &q.orders(true)[1];
        assert_eq!(reversed, &[2, 1, 0, 3]);
        assert_eq!(
            q.prompt_for_order("state", reversed).unwrap(),
            "state\n\nQuestion: Which one?\na. third\nb. second\nc. first\nd. none of these\nAnswer:"
        );
    }

    #[test]
    fn orders_reverse_every_option_without_none() {
        assert_eq!(question().orders(false), vec![vec![0, 1, 2]]);
        assert_eq!(question().orders(true), vec![vec![0, 1, 2], vec![2, 1, 0]]);
        let single = ChoiceQuestion::new("q", vec!["only".into()]).with_none_of_these();
        assert_eq!(single.orders(true), vec![vec![0, 1]]);
    }

    #[test]
    fn invalid_questions_and_orders_are_refused() {
        assert!(ChoiceQuestion::new("q", vec![]).validate().is_err());
        let many: Vec<String> = (0..26).map(|i| i.to_string()).collect();
        assert!(ChoiceQuestion::new("q", many.clone()).validate().is_ok());
        assert!(ChoiceQuestion::new("q", many)
            .with_none_of_these()
            .validate()
            .is_err());
        assert!(ChoiceQuestion::new("q", vec!["a\nb. forged".into()])
            .validate()
            .is_err());
        let q = question();
        assert!(q.prompt_for_order("s", &[0, 1]).is_err());
        assert!(q.prompt_for_order("s", &[0, 0, 1]).is_err());
        assert!(q.prompt_for_order("s", &[0, 1, 3]).is_err());
    }

    #[test]
    fn one_order_is_a_tempered_softmax() {
        let q = question();
        let d = Decision::from_letter_logits(&q, &[vec![0, 1, 2]], &[vec![2.0, 1.0, 0.0]], 1.0)
            .unwrap();
        let z = 1.0 + (-1.0f64).exp() + (-2.0f64).exp();
        let want = [1.0 / z, (-1.0f64).exp() / z, (-2.0f64).exp() / z];
        for (got, want) in d.probabilities.iter().zip(want) {
            assert!((f64::from(*got) - want).abs() < 1e-6);
        }
        assert_eq!(d.choice, Choice::Option(0));
        assert_eq!(d.none_probability, None);
        assert!((d.confidence - d.probabilities[0]).abs() < f32::EPSILON);

        let hot = Decision::from_letter_logits(&q, &[vec![0, 1, 2]], &[vec![2.0, 1.0, 0.0]], 2.0)
            .unwrap();
        assert!(hot.confidence < d.confidence);
        assert!(Decision::from_letter_logits(&q, &[vec![0, 1, 2]], &[vec![0.0; 3]], 0.0).is_err());
        assert!(Decision::from_letter_logits(
            &q,
            &[vec![0, 1, 2]],
            &[vec![f32::NAN, 0.0, 0.0]],
            1.0
        )
        .is_err());
    }

    #[test]
    fn averaged_orders_map_positions_back_to_choices() {
        let q = question().with_none_of_these();
        let orders = q.orders(true);
        // The model prefers letter a in both orders: option 0 first, then option 2.
        let logits = vec![vec![3.0, 0.0, 0.0, 0.0], vec![3.0, 0.0, 0.0, 0.0]];
        let d = Decision::from_letter_logits(&q, &orders, &logits, 1.0).unwrap();
        let top = 3f64.exp() / (3f64.exp() + 3.0);
        let rest = 1.0 / (3f64.exp() + 3.0);
        let want = [(top + rest) / 2.0, rest, (rest + top) / 2.0];
        for (got, want) in d.probabilities.iter().zip(want) {
            assert!((f64::from(*got) - want).abs() < 1e-6);
        }
        assert!((f64::from(d.none_probability.unwrap()) - rest).abs() < 1e-6);
        // Options 0 and 2 tie, and the tie goes to the earlier one.
        assert_eq!(d.choice, Choice::Option(0));
        let total: f32 = d.probabilities.iter().sum::<f32>() + d.none_probability.unwrap();
        assert!((total - 1.0).abs() < 1e-6);
    }

    #[test]
    fn none_of_these_can_win() {
        let q = question().with_none_of_these();
        let d =
            Decision::from_letter_logits(&q, &q.orders(false), &[vec![0.0, 0.0, 0.0, 5.0]], 1.0)
                .unwrap();
        assert_eq!(d.choice, Choice::NoneOfThese);
        assert_eq!(Some(d.confidence), d.probability(Choice::NoneOfThese));
    }

    #[test]
    fn call_target_uses_the_fixed_wording() {
        let q = ChoiceQuestion::call_target("parse", vec!["src/a.rs: parse".into()]);
        assert_eq!(
            q.question,
            "Which definition does the call to `parse` at this call site execute?"
        );
        assert_eq!(q.none_of_these.as_deref(), Some(CALL_TARGET_NONE_OF_THESE));
    }
}
