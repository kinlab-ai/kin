// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A trained decision model loaded from a configured local directory.
//!
//! [`DecisionEngine`] is the in-process entry point other crates call. It loads a decoder
//! checkpoint (`config.json`, `model.safetensors`, `tokenizer.json`) from the directory a
//! [`DecisionEngineConfig`] names, reads the calibration the model was trained with from an
//! optional [`MANIFEST_FILE`] beside the weights, and answers:
//!
//! - [`DecisionEngine::score_candidates`]: a probability for every candidate text of a
//!   [`CandidateQuery`] and for "none of these";
//! - [`DecisionEngine::choose`]: a [`TypedQuestion`], answered in the caller's values;
//! - [`DecisionEngine::decide`]: a plain [`ChoiceQuestion`].
//!
//! The engine reads only local files. A directory that does not exist is an error, never a
//! reason to fetch a model, and kin-infer compiles no network client.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{
    CandidateQuery, CandidateScores, ChoiceQuestion, Decision, DecisionError, DecisionModel,
    DecisionSettings, PromptItem, TypedAnswer, TypedQuestion, DEFAULT_OPTIONS_PER_QUESTION,
};
use crate::gpu;

/// The environment variable [`DecisionEngineConfig::from_env`] reads: the local directory of
/// the decision model to load.
pub const DECISION_MODEL_DIR_ENV: &str = "KIN_DECISION_MODEL_DIR";

/// The optional manifest a trained model directory carries beside its weights (see
/// [`StudentManifest`]).
pub const MANIFEST_FILE: &str = "kin_student.json";

/// Where the engine runs the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecisionDevice {
    /// The best available backend: Metal, then CUDA, then the CPU, as compiled in. Setting
    /// `KIN_INFER_FORCE_CPU` pins the CPU, as it does for every kin-infer model.
    #[default]
    Auto,
    /// The CPU, whatever accelerator is available.
    Cpu,
}

/// Which model the engine loads and how it asks questions.
///
/// Every `Option` field overrides the value the model's manifest gives, which in turn overrides
/// the default.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionEngineConfig {
    /// The local model directory. It is read, never downloaded.
    pub model_dir: PathBuf,
    /// Where the model runs.
    pub device: DecisionDevice,
    /// Softmax temperature over the letter logits. Default 1.0.
    pub temperature: Option<f32>,
    /// Also score each question with its options reversed and average the two. Default off.
    pub average_orders: Option<bool>,
    /// The most candidates one question lists. Default [`DEFAULT_OPTIONS_PER_QUESTION`], and
    /// never more than the tokenizer's single-token letters allow.
    pub options_per_question: Option<usize>,
    /// The longest prompt scored, in tokens. Default the model's context window.
    pub max_prompt_tokens: Option<usize>,
    /// The confidence floor below which an answer is read as unsure. Default 0, which never
    /// abstains.
    pub min_confidence: Option<f32>,
}

impl DecisionEngineConfig {
    /// A configuration that loads the model in `model_dir` with its manifest's settings.
    pub fn new(model_dir: impl Into<PathBuf>) -> Self {
        Self {
            model_dir: model_dir.into(),
            device: DecisionDevice::default(),
            temperature: None,
            average_orders: None,
            options_per_question: None,
            max_prompt_tokens: None,
            min_confidence: None,
        }
    }

    /// The configuration [`DECISION_MODEL_DIR_ENV`] names, or `None` when it is unset or
    /// empty, which means no decision model is configured.
    pub fn from_env() -> Option<Self> {
        Self::from_env_value(std::env::var_os(DECISION_MODEL_DIR_ENV))
    }

    fn from_env_value(value: Option<OsString>) -> Option<Self> {
        value.filter(|dir| !dir.is_empty()).map(Self::new)
    }

    /// Runs the model on the CPU.
    pub fn on_cpu(mut self) -> Self {
        self.device = DecisionDevice::Cpu;
        self
    }

    /// Overrides the softmax temperature.
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Overrides order averaging.
    pub fn with_order_averaging(mut self, average_orders: bool) -> Self {
        self.average_orders = Some(average_orders);
        self
    }

    /// Overrides the most candidates one question lists.
    pub fn with_options_per_question(mut self, options: usize) -> Self {
        self.options_per_question = Some(options);
        self
    }

    /// Overrides the longest prompt scored.
    pub fn with_max_prompt_tokens(mut self, tokens: usize) -> Self {
        self.max_prompt_tokens = Some(tokens);
        self
    }

    /// Overrides the confidence floor.
    pub fn with_min_confidence(mut self, min_confidence: f32) -> Self {
        self.min_confidence = Some(min_confidence);
        self
    }
}

/// What a trained model directory records about how its model should be asked.
///
/// It is read from [`MANIFEST_FILE`] when the directory has one. Every field is optional, and
/// fields the engine does not use, such as training notes, are ignored.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct StudentManifest {
    /// The checkpoint the model was tuned from.
    pub base: Option<String>,
    /// How it was tuned.
    pub recipe: Option<String>,
    /// The softmax temperature fitted on held-out answers.
    pub temperature: Option<f32>,
    /// Whether that temperature was fitted on averaged option orders.
    pub average_orders: Option<bool>,
    /// The most options per question the model was trained on, not counting "none of these".
    pub max_options: Option<usize>,
    /// The longest prompt the model was trained on, in tokens.
    pub max_prompt_tokens: Option<usize>,
    /// The confidence floor fitted on held-out answers for a target error rate.
    pub min_confidence: Option<f32>,
}

impl StudentManifest {
    /// Reads the manifest of a model directory: `None` when it has no [`MANIFEST_FILE`], and an
    /// error when the file is there but does not parse.
    pub fn read(dir: &Path) -> Result<Option<Self>, DecisionError> {
        let path = dir.join(MANIFEST_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(DecisionError::Load(format!("{}: {e}", path.display()))),
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| DecisionError::Load(format!("{}: {e}", path.display())))
    }
}

/// The settings an engine asks every question with, resolved from its configuration, its
/// model's manifest and the defaults.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineSettings {
    /// Softmax temperature over the letter logits.
    pub temperature: f32,
    /// Whether each question is also scored with its options reversed.
    pub average_orders: bool,
    /// The most candidates one question lists.
    pub options_per_question: usize,
    /// The longest prompt scored, in tokens.
    pub max_prompt_tokens: usize,
    /// The confidence floor below which an answer is read as unsure.
    pub min_confidence: f32,
}

impl EngineSettings {
    fn resolve(
        config: &DecisionEngineConfig,
        manifest: Option<&StudentManifest>,
        model: &DecisionModel,
    ) -> Result<Self, DecisionError> {
        let from = |pick: fn(&StudentManifest) -> Option<f32>| manifest.and_then(pick);
        let temperature = config
            .temperature
            .or_else(|| from(|m| m.temperature))
            .unwrap_or(1.0);
        DecisionSettings {
            temperature,
            average_orders: false,
        }
        .validate()?;
        let min_confidence = config
            .min_confidence
            .or_else(|| from(|m| m.min_confidence))
            .unwrap_or(0.0);
        check_min_confidence(min_confidence)?;
        let average_orders = config
            .average_orders
            .or_else(|| manifest.and_then(|m| m.average_orders))
            .unwrap_or(false);
        let requested = config
            .options_per_question
            .or_else(|| manifest.and_then(|m| m.max_options))
            .unwrap_or(DEFAULT_OPTIONS_PER_QUESTION);
        if requested == 0 {
            return Err(DecisionError::InvalidInput(
                "a question must list at least one option".into(),
            ));
        }
        // One letter is kept for "none of these".
        let options_per_question = requested.min(model.letter_ids().len().saturating_sub(1));
        let window = model.model().config.effective_max_seq_len();
        let max_prompt_tokens = config
            .max_prompt_tokens
            .or_else(|| manifest.and_then(|m| m.max_prompt_tokens))
            .map_or(window, |limit| limit.min(window));
        if max_prompt_tokens == 0 {
            return Err(DecisionError::InvalidInput(
                "the prompt limit must be at least one token".into(),
            ));
        }
        Ok(Self {
            temperature,
            average_orders,
            options_per_question,
            max_prompt_tokens,
            min_confidence,
        })
    }

    fn decision_settings(&self) -> DecisionSettings {
        DecisionSettings {
            temperature: self.temperature,
            average_orders: self.average_orders,
        }
    }
}

/// Checks that a confidence floor is a probability.
fn check_min_confidence(min_confidence: f32) -> Result<(), DecisionError> {
    if (0.0..=1.0).contains(&min_confidence) {
        Ok(())
    } else {
        Err(DecisionError::InvalidInput(format!(
            "a confidence floor must lie in [0, 1], got {min_confidence}"
        )))
    }
}

/// A decision model loaded from a local directory, ready to score candidates and answer typed
/// questions. It is `Send` and `Sync`, so one engine can serve many threads.
pub struct DecisionEngine {
    model: DecisionModel,
    settings: EngineSettings,
    manifest: Option<StudentManifest>,
    model_dir: PathBuf,
}

impl std::fmt::Debug for DecisionEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionEngine")
            .field("model_dir", &self.model_dir)
            .field("backend", &self.backend())
            .field("settings", &self.settings)
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

impl DecisionEngine {
    /// Loads the model in `config.model_dir` and resolves its settings.
    ///
    /// The directory must exist on local disk and hold `config.json`, `model.safetensors` and
    /// `tokenizer.json`. Its [`MANIFEST_FILE`], when present, supplies any setting the
    /// configuration leaves unset.
    pub fn load(config: &DecisionEngineConfig) -> Result<Self, DecisionError> {
        let dir = &config.model_dir;
        if !dir.is_dir() {
            return Err(DecisionError::Load(format!(
                "{} is not a local model directory; decision models load only from local disk \
                 and are never downloaded",
                dir.display()
            )));
        }
        let manifest = StudentManifest::read(dir)?;
        let force_cpu = match config.device {
            DecisionDevice::Auto => gpu::force_cpu_from_env(),
            DecisionDevice::Cpu => true,
        };
        let model = DecisionModel::load_with_backend(dir, force_cpu)?;
        let settings = EngineSettings::resolve(config, manifest.as_ref(), &model)?;
        let model = model
            .with_max_prompt_tokens(settings.max_prompt_tokens)
            .with_temperature(settings.temperature)
            .with_order_averaging(settings.average_orders);
        Ok(Self {
            model,
            settings,
            manifest,
            model_dir: dir.clone(),
        })
    }

    /// The settings every question is asked with.
    pub fn settings(&self) -> EngineSettings {
        self.settings
    }

    /// The manifest read from the model directory, if it has one.
    pub fn manifest(&self) -> Option<&StudentManifest> {
        self.manifest.as_ref()
    }

    /// The directory the model was loaded from.
    pub fn model_dir(&self) -> &Path {
        &self.model_dir
    }

    /// The backend the model runs on.
    pub fn backend(&self) -> gpu::GpuBackend {
        self.model.model().backend()
    }

    /// The underlying decision model, for prompt-level access.
    pub fn decision_model(&self) -> &DecisionModel {
        &self.model
    }

    /// Scores every candidate of `query`, and "none of these", as probabilities that sum to one.
    ///
    /// The query is asked as [`CandidateQuery::questions`] at the engine's
    /// `options_per_question`, all prompts are scored as one batch, and the answers combine as
    /// [`CandidateScores::from_answers`] describes. The probabilities are calibrated to the
    /// extent the engine's temperature was fitted on questions like these.
    pub fn score_candidates(
        &self,
        query: &CandidateQuery,
    ) -> Result<CandidateScores, DecisionError> {
        let questions = query.questions(self.settings.options_per_question)?;
        let orders: Vec<Vec<Vec<usize>>> = questions
            .iter()
            .map(|(_, question)| question.orders(self.settings.average_orders))
            .collect();
        let items: Vec<PromptItem<'_>> = questions
            .iter()
            .zip(&orders)
            .flat_map(|((_, question), question_orders)| {
                question_orders
                    .iter()
                    .map(move |order| (query.state.as_str(), question, order.as_slice()))
            })
            .collect();
        let mut logits = self.model.letter_logits_batch(&items)?.into_iter();
        let mut answers = Vec::with_capacity(questions.len());
        for ((range, question), question_orders) in questions.iter().zip(&orders) {
            let rows: Vec<Vec<f32>> = logits.by_ref().take(question_orders.len()).collect();
            let decision = Decision::from_letter_logits(
                question,
                question_orders,
                &rows,
                self.settings.temperature,
            )?;
            answers.push((range.clone(), decision));
        }
        CandidateScores::from_answers(query.candidates.len(), &answers)
    }

    /// Answers a typed question about `state`, in the question's values.
    pub fn choose<'q, T>(
        &self,
        state: &str,
        question: &'q TypedQuestion<T>,
    ) -> Result<TypedAnswer<'q, T>, DecisionError> {
        let decision = self.decide(state, question.choice_question())?;
        question.answer(decision)
    }

    /// Answers a plain choice question about `state`.
    pub fn decide(
        &self,
        state: &str,
        question: &ChoiceQuestion,
    ) -> Result<Decision, DecisionError> {
        self.model
            .decide_with(state, question, self.settings.decision_settings())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::decision::{Choice, Verdict};

    // -- A tiny model directory ---------------------------------------------------------------
    // Qwen3's shape at toy size, with deterministic weights, and a word-level tokenizer that
    // encodes " a" ... " z" as single tokens. It is written to disk and loaded through the same
    // path a trained model takes, on the CPU.

    const WORDS: &[&str] = &[
        "Question",
        "Answer",
        "Query",
        ":",
        ".",
        "?",
        "none",
        "of",
        "these",
        "yes",
        "no",
        "which",
        "call",
        "fn",
        "parse",
        "save",
        "config",
        "the",
        "candidate",
        "matches",
        "query",
    ];
    const VOCAB: usize = 64;
    const HIDDEN: usize = 16;

    fn tokenizer_json() -> serde_json::Value {
        let mut vocab = serde_json::Map::new();
        vocab.insert("[UNK]".into(), 0.into());
        let letters = ('a'..='z').map(String::from);
        for (id, word) in letters
            .chain(WORDS.iter().map(|w| w.to_string()))
            .enumerate()
        {
            vocab.entry(word).or_insert((id + 1).into());
        }
        assert!(vocab.len() <= VOCAB);
        serde_json::json!({
            "version": "1.0",
            "truncation": null,
            "padding": null,
            "added_tokens": [],
            "normalizer": null,
            "pre_tokenizer": { "type": "Whitespace" },
            "post_processor": null,
            "decoder": null,
            "model": { "type": "WordLevel", "vocab": vocab, "unk_token": "[UNK]" }
        })
    }

    fn config_json(max_positions: usize) -> serde_json::Value {
        serde_json::json!({
            "model_type": "qwen3",
            "hidden_size": HIDDEN,
            "num_hidden_layers": 2,
            "num_attention_heads": 4,
            "num_key_value_heads": 2,
            "head_dim": 8,
            "intermediate_size": 24,
            "max_position_embeddings": max_positions,
            "vocab_size": VOCAB,
            "rms_norm_eps": 1e-6,
            "rope_theta": 10000.0,
            "tie_word_embeddings": true
        })
    }

    fn weights() -> Vec<u8> {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut values = |count: usize, center: f32, spread: f32| -> Vec<u8> {
            (0..count)
                .flat_map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    let unit = (state >> 40) as f32 / (1u64 << 24) as f32;
                    (center + spread * (unit - 0.5)).to_le_bytes()
                })
                .collect()
        };
        let mut tensors: Vec<(String, Vec<usize>, Vec<u8>)> = vec![
            (
                "model.embed_tokens.weight".into(),
                vec![VOCAB, HIDDEN],
                values(VOCAB * HIDDEN, 0.0, 2.0),
            ),
            (
                "model.norm.weight".into(),
                vec![HIDDEN],
                values(HIDDEN, 1.0, 0.2),
            ),
        ];
        let matrices = [
            ("self_attn.q_proj", 32, 16),
            ("self_attn.k_proj", 16, 16),
            ("self_attn.v_proj", 16, 16),
            ("self_attn.o_proj", 16, 32),
            ("mlp.gate_proj", 24, 16),
            ("mlp.up_proj", 24, 16),
            ("mlp.down_proj", 16, 24),
        ];
        let norms = [
            ("self_attn.q_norm", 8),
            ("self_attn.k_norm", 8),
            ("input_layernorm", 16),
            ("post_attention_layernorm", 16),
        ];
        for layer in 0..2 {
            for (name, r, c) in matrices {
                tensors.push((
                    format!("model.layers.{layer}.{name}.weight"),
                    vec![r, c],
                    values(r * c, 0.0, 0.6),
                ));
            }
            for (name, n) in norms {
                tensors.push((
                    format!("model.layers.{layer}.{name}.weight"),
                    vec![n],
                    values(n, 1.0, 0.2),
                ));
            }
        }
        let views = tensors.iter().map(|(name, shape, bytes)| {
            let view =
                safetensors::tensor::TensorView::new(safetensors::Dtype::F32, shape.clone(), bytes)
                    .unwrap();
            (name.clone(), view)
        });
        safetensors::serialize(views, &None).unwrap()
    }

    /// A model directory under the system temporary directory, removed on drop.
    struct TinyModelDir(PathBuf);

    impl TinyModelDir {
        fn new(manifest: Option<&str>) -> Self {
            Self::with_positions(manifest, 512)
        }

        fn with_positions(manifest: Option<&str>, max_positions: usize) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "kin-infer-decision-engine-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let json = |value: serde_json::Value| serde_json::to_vec(&value).unwrap();
            std::fs::write(dir.join("config.json"), json(config_json(max_positions))).unwrap();
            std::fs::write(dir.join("tokenizer.json"), json(tokenizer_json())).unwrap();
            std::fs::write(dir.join("model.safetensors"), weights()).unwrap();
            if let Some(manifest) = manifest {
                std::fs::write(dir.join(MANIFEST_FILE), manifest).unwrap();
            }
            Self(dir)
        }

        fn engine(
            &self,
            configure: impl FnOnce(DecisionEngineConfig) -> DecisionEngineConfig,
        ) -> DecisionEngine {
            DecisionEngine::load(&configure(DecisionEngineConfig::new(&self.0).on_cpu())).unwrap()
        }
    }

    impl Drop for TinyModelDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const STATE: &str = "fn save ( config ) calls parse ( config )";

    fn candidates(n: usize) -> Vec<String> {
        let words = [
            "parse",
            "save",
            "config",
            "the",
            "candidate",
            "fn",
            "call",
            "query",
        ];
        (0..n)
            .map(|i| {
                format!(
                    "{} {}",
                    words[i % words.len()],
                    words[(i * 3 + 1) % words.len()]
                )
            })
            .collect()
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn the_engine_is_shareable_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DecisionEngine>();
    }

    #[test]
    fn a_missing_directory_is_refused_without_fetching() {
        let err = DecisionEngine::load(&DecisionEngineConfig::new("Qwen/Qwen3-0.6B-Base"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("never downloaded"), "{err}");
    }

    #[test]
    fn the_environment_names_the_model_directory() {
        assert_eq!(DecisionEngineConfig::from_env_value(None), None);
        assert_eq!(DecisionEngineConfig::from_env_value(Some("".into())), None);
        let config = DecisionEngineConfig::from_env_value(Some("/models/student".into())).unwrap();
        assert_eq!(config, DecisionEngineConfig::new("/models/student"));
    }

    #[test]
    fn settings_come_from_the_config_then_the_manifest_then_defaults() {
        let bare = TinyModelDir::new(None);
        let engine = bare.engine(|c| c);
        let settings = engine.settings();
        assert_eq!(settings.temperature, 1.0);
        assert!(!settings.average_orders);
        assert_eq!(settings.options_per_question, DEFAULT_OPTIONS_PER_QUESTION);
        assert_eq!(settings.max_prompt_tokens, 512);
        assert_eq!(settings.min_confidence, 0.0);
        assert!(engine.manifest().is_none());
        assert_eq!(engine.backend(), gpu::GpuBackend::Cpu);

        // The manifest the trained model ships, plus fields the engine does not read.
        let trained = TinyModelDir::new(Some(
            r#"{"base": "Qwen/Qwen3-0.6B-Base", "temperature": 1.3, "epochs": 2,
                "fold_temperatures": {"0": 1.3}, "max_options": 30, "max_prompt_tokens": 256,
                "min_confidence": 0.9}"#,
        ));
        let engine = trained.engine(|c| c);
        let settings = engine.settings();
        assert_eq!(settings.temperature, 1.3);
        // 26 single-token letters leave 25 options beside "none of these".
        assert_eq!(settings.options_per_question, 25);
        assert_eq!(settings.max_prompt_tokens, 256);
        assert_eq!(settings.min_confidence, 0.9);
        assert_eq!(
            engine.manifest().unwrap().base.as_deref(),
            Some("Qwen/Qwen3-0.6B-Base")
        );

        let overridden = trained.engine(|c| {
            c.with_temperature(2.0)
                .with_order_averaging(true)
                .with_options_per_question(4)
                .with_max_prompt_tokens(10_000)
                .with_min_confidence(0.5)
        });
        let settings = overridden.settings();
        assert_eq!(settings.temperature, 2.0);
        assert!(settings.average_orders);
        assert_eq!(settings.options_per_question, 4);
        // Never past the model's window.
        assert_eq!(settings.max_prompt_tokens, 512);
        assert_eq!(settings.min_confidence, 0.5);
    }

    #[test]
    fn bad_settings_and_manifests_are_refused() {
        let dir = TinyModelDir::new(None);
        let load = |config: DecisionEngineConfig| DecisionEngine::load(&config.on_cpu());
        let config = || DecisionEngineConfig::new(&dir.0);
        assert!(load(config().with_temperature(0.0)).is_err());
        assert!(load(config().with_temperature(f32::NAN)).is_err());
        assert!(load(config().with_min_confidence(1.5)).is_err());
        assert!(load(config().with_options_per_question(0)).is_err());
        assert!(load(config().with_max_prompt_tokens(0)).is_err());

        let broken = TinyModelDir::new(Some(r#"{"temperature": "warm"}"#));
        let err = DecisionEngine::load(&DecisionEngineConfig::new(&broken.0).on_cpu())
            .unwrap_err()
            .to_string();
        assert!(err.contains(MANIFEST_FILE), "{err}");
    }

    #[test]
    fn decide_matches_the_letter_logits_it_is_built_from() {
        let dir = TinyModelDir::new(Some(r#"{"temperature": 1.3}"#));
        for average in [false, true] {
            let engine = dir.engine(|c| c.with_order_averaging(average));
            let question = ChoiceQuestion::call_target("parse", candidates(3));
            let got = engine.decide(STATE, &question).unwrap();

            let model = engine.decision_model();
            let orders = question.orders(average);
            let logits: Vec<Vec<f32>> = orders
                .iter()
                .map(|order| {
                    let prompt = question.prompt_for_order(STATE, order).unwrap();
                    let ids = model.encode_prompt(&prompt).unwrap();
                    model
                        .model()
                        .score_next_tokens(&ids, &model.letter_ids()[..question.choice_count()])
                        .unwrap()
                })
                .collect();
            let want = Decision::from_letter_logits(&question, &orders, &logits, 1.3).unwrap();
            assert_eq!(got, want, "average_orders = {average}");
        }
    }

    #[test]
    fn one_question_of_candidates_scores_like_a_decision() {
        let dir = TinyModelDir::new(None);
        let engine = dir.engine(|c| c.with_temperature(0.7));
        let query = CandidateQuery::call_target(STATE, "parse", candidates(5));
        let scores = engine.score_candidates(&query).unwrap();
        let decision = engine
            .decide(STATE, &ChoiceQuestion::call_target("parse", candidates(5)))
            .unwrap();
        assert_eq!(scores.questions, 1);
        for (got, want) in scores.probabilities.iter().zip(&decision.probabilities) {
            assert!(close(*got, *want), "{got} vs {want}");
        }
        assert!(close(
            scores.none_probability,
            decision.none_probability.unwrap()
        ));
        assert_eq!(scores.choice, decision.choice);
    }

    #[test]
    fn many_candidates_are_asked_in_several_questions_and_combined() {
        let dir = TinyModelDir::new(None);
        for average in [false, true] {
            let engine =
                dir.engine(|c| c.with_options_per_question(3).with_order_averaging(average));
            let query = CandidateQuery::new("parse the config", candidates(8));
            let scores = engine.score_candidates(&query).unwrap();
            assert_eq!(scores.questions, 3);
            assert_eq!(scores.probabilities.len(), 8);
            let total: f32 = scores.probabilities.iter().sum::<f32>() + scores.none_probability;
            assert!((total - 1.0).abs() < 1e-5, "total {total}");
            let first = scores.probabilities[0];
            assert!(
                scores
                    .probabilities
                    .iter()
                    .any(|p| (p - first).abs() > 1e-4),
                "the model should tell candidates apart: {:?}",
                scores.probabilities
            );

            // The batch equals asking each question on its own, then combining.
            let answers: Vec<_> = query
                .questions(3)
                .unwrap()
                .into_iter()
                .map(|(range, question)| {
                    let decision = engine.decide(&query.state, &question).unwrap();
                    (range, decision)
                })
                .collect();
            let want = CandidateScores::from_answers(8, &answers).unwrap();
            assert_eq!(scores, want, "average_orders = {average}");
        }
    }

    #[test]
    fn typed_and_yes_no_questions_answer_in_values() {
        #[derive(Debug, PartialEq)]
        enum Target {
            Parse,
            Save,
        }
        let dir = TinyModelDir::new(None);
        let engine = dir.engine(|c| c);
        let question = TypedQuestion::new(
            "which fn ?",
            [(Target::Parse, "parse"), (Target::Save, "save")],
        )
        .with_none_of_these();
        let answer = engine.choose(STATE, &question).unwrap();
        let decision = engine.decide(STATE, question.choice_question()).unwrap();
        assert_eq!(answer.decision(), &decision);
        match decision.choice {
            Choice::Option(i) => {
                assert_eq!(answer.value(), Some(&question.values()[i]));
                assert_eq!(answer.verdict(0.0), Verdict::Chosen(&question.values()[i]));
            }
            Choice::NoneOfThese => {
                assert_eq!(answer.value(), None);
                assert_eq!(answer.verdict(0.0), Verdict::NoneOfThese);
            }
        }
        assert_eq!(answer.verdict(f32::NAN), Verdict::Unsure);

        let yes_no = TypedQuestion::yes_no("call parse ?");
        let answer = engine.choose(STATE, &yes_no).unwrap();
        let p = answer.yes_probability();
        assert!((0.0..=1.0).contains(&p));
        assert!(close(p, answer.decision().probabilities[0]));
        let want = match answer.decision().choice {
            Choice::Option(0) => Some(&true),
            Choice::Option(1) => Some(&false),
            other => panic!("a yes-or-no question has no {other:?}"),
        };
        assert_eq!(answer.value(), want);
    }

    #[test]
    fn a_prompt_past_the_limit_is_refused() {
        let dir = TinyModelDir::new(None);
        let engine = dir.engine(|c| c.with_max_prompt_tokens(40));
        let long_state = "parse config ".repeat(30);
        let err = engine
            .score_candidates(&CandidateQuery::new(&long_state, candidates(2)))
            .unwrap_err();
        assert!(
            matches!(err, DecisionError::PromptTooLong { limit: 40, .. }),
            "{err}"
        );
        assert!(engine
            .score_candidates(&CandidateQuery::new("parse", candidates(2)))
            .is_ok());
    }
}
