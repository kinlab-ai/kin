// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Pointer decision models: typed questions answered by a head over option tokens.
//!
//! A pointer decision model (the Kev family) reads one document, the *state*, and answers
//! typed questions about it with one probability per option. It generates nothing and reads no
//! vocabulary logits. A decoder backbone runs the prompt, and a small head compares the final
//! hidden state of each option's closing token with that of a *decide* token after the options.
//!
//! The checkpoint is a PEFT LoRA adapter over a base decoder plus the head, saved as `head.pt`.
//! [`PointerCheckpoint::read`] reads the adapter directory, and [`PointerDecisionModel::load`]
//! merges the adapter into the base checkpoint the head names and pairs it with the head.
//!
//! # Prompt format
//!
//! The model reuses five rarely used special tokens of its tokenizer as delimiters:
//!
//! | token             | role                                |
//! |-------------------|-------------------------------------|
//! | `<\|fim_prefix\|>`  | opens the state                     |
//! | `<\|fim_middle\|>`  | opens a question's instruction      |
//! | `<\|box_start\|>`   | opens an option                     |
//! | `<\|box_end\|>`     | closes an option (read by the head) |
//! | `<\|fim_suffix\|>`  | the decide token (read by the head) |
//!
//! A question is asked as one causal row, the state followed by that question:
//!
//! ```text
//! <|fim_prefix|> state <|fim_middle|> instruction
//!   <|box_start|> option 1 <|box_end|> ... <|box_start|> option K <|box_end|> <|fim_suffix|>
//! ```
//!
//! Every text a caller supplies is tokenized without special tokens, and any `<|name|>` in it is
//! first rewritten to `<¦name¦>`, so no caller text can forge a delimiter. The state is cut to
//! [`TRAINING_MAX_STATE_TOKENS`] tokens including its opening token, the length the model was
//! trained on, and a row longer than [`TRAINING_MAX_ROW_TOKENS`] is refused.
//!
//! The model family packs every question of a request into one sequence under a block-causal
//! mask, so each question sees the state and itself. Running each question as its own causal
//! row gives every token the same context and the same position, so the answers are the same.
//!
//! # Reading an answer
//!
//! With `h_d` the final hidden state of the decide token and `h_j` that of option `j`'s closing
//! token, the head's logit for option `j` is `(K h_j + k) · (Q h_d + q) / sqrt(p) / T` for its
//! learned projections to `p` dimensions and the temperature `T` the checkpoint records (1 when
//! it records none), and a softmax over the options gives the probabilities.
//!
//! A yes-or-no question follows the TypeSafe convention the family was trained on: two options,
//! `no` then `yes`, each optionally followed by `: <criterion>`, and the answer is the
//! probability of `yes` ([`PointerQuestion::yes_no`], [`PointerDecisionModel::yes_probabilities`]).

use std::path::{Path, PathBuf};

use ndarray::{Array1, Array2, ArrayView1};
use tokenizers::Tokenizer;

use super::DecisionError;
use crate::torch_checkpoint::{PickleValue, TorchCheckpoint};
use crate::{gpu, BatchOptions, BertConfig, BertModel, LoraAdapter};

/// The delimiter tokens, in the order state, question, option open, option close, decide.
pub const POINTER_DELIMITERS: [&str; 5] = [
    "<|fim_prefix|>",
    "<|fim_middle|>",
    "<|box_start|>",
    "<|box_end|>",
    "<|fim_suffix|>",
];

/// State tokens the family was trained on, counting the opening delimiter.
pub const TRAINING_MAX_STATE_TOKENS: usize = 384;

/// Tokens of one row (state and one question) the family was trained on.
pub const TRAINING_MAX_ROW_TOKENS: usize = 1024;

/// One question: an instruction and its options, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointerQuestion {
    pub instruction: String,
    pub options: Vec<String>,
}

impl PointerQuestion {
    pub fn new(instruction: impl Into<String>, options: Vec<String>) -> Self {
        Self {
            instruction: instruction.into(),
            options,
        }
    }

    /// A yes-or-no question: options `no` then `yes`, each followed by `: <criterion>` when one
    /// is given. The probability of `yes` is the second option's.
    pub fn yes_no(instruction: impl Into<String>, criteria: Option<(&str, &str)>) -> Self {
        let options = match criteria {
            Some((when_yes, when_no)) => vec![format!("no: {when_no}"), format!("yes: {when_yes}")],
            None => vec!["no".to_string(), "yes".to_string()],
        };
        Self::new(instruction, options)
    }
}

/// A state and the questions asked about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointerRecord {
    pub state: String,
    pub questions: Vec<PointerQuestion>,
}

/// One question as the backbone runs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointerRow {
    /// State then question, as token ids.
    pub ids: Vec<u32>,
    /// Offset of the decide token, the last in the row.
    pub decide: usize,
    /// Offset of each option's closing token.
    pub option_ends: Vec<usize>,
    /// Whether the state was cut to fit.
    pub state_truncated: bool,
}

impl PointerRow {
    /// Rows at the end of the row that the head reads: from the first option's closing token
    /// through the decide token.
    fn tail(&self) -> usize {
        self.ids.len() - self.option_ends.first().copied().unwrap_or(self.decide)
    }
}

/// The trained head: query and key projections of the backbone's hidden states.
#[derive(Clone)]
struct PointerHead {
    /// `[p, hidden]`.
    q_weight: Array2<f32>,
    q_bias: Array1<f32>,
    k_weight: Array2<f32>,
    k_bias: Array1<f32>,
    temperature: f32,
}

impl PointerHead {
    fn logits(&self, decide: ArrayView1<f32>, options: &[ArrayView1<f32>]) -> Vec<f32> {
        let query = self.q_weight.dot(&decide) + &self.q_bias;
        let scale = 1.0 / (self.q_weight.nrows() as f32).sqrt() / self.temperature;
        options
            .iter()
            .map(|h| (self.k_weight.dot(h) + &self.k_bias).dot(&query) * scale)
            .collect()
    }
}

/// An adapter directory read and checked, before the base model is loaded.
pub struct PointerCheckpoint {
    dir: PathBuf,
    head: PointerHead,
    base_model: String,
    base_revision: Option<String>,
}

impl PointerCheckpoint {
    /// Reads `head.pt` from a pointer model's adapter directory.
    ///
    /// # Errors
    ///
    /// Fails when `head.pt` is missing or unreadable, when it lacks the head's four tensors or
    /// names no base model, or when it records a form this runtime does not implement: option
    /// isolation, which needs the packed mask, or trained delimiter embeddings.
    pub fn read(adapter_dir: &Path) -> Result<Self, DecisionError> {
        let path = adapter_dir.join("head.pt");
        let checkpoint = TorchCheckpoint::read(&path)?;
        let root = checkpoint.root();
        let load = |m: String| DecisionError::Load(format!("{}: {m}", path.display()));
        if root.get("option_isolation").and_then(PickleValue::as_bool) == Some(true) {
            return Err(load(
                "the head was trained with option isolation, which this runtime does not run"
                    .into(),
            ));
        }
        if root
            .get("special_embeddings")
            .and_then(PickleValue::as_bool)
            == Some(true)
        {
            return Err(load(
                "the head was trained with its own delimiter embeddings, which this runtime \
                 does not load"
                    .into(),
            ));
        }
        let head = root
            .get("head")
            .ok_or_else(|| load("no `head` state dict".into()))?;
        let tensor = |name: &str, rank: usize| -> Result<(Vec<usize>, Vec<f32>), DecisionError> {
            let t = head
                .get(name)
                .and_then(PickleValue::as_tensor)
                .ok_or_else(|| load(format!("no `{name}` tensor")))?;
            if t.shape.len() != rank {
                return Err(load(format!("`{name}` has shape {:?}", t.shape)));
            }
            Ok((t.shape.clone(), checkpoint.tensor_f32(t)?))
        };
        let (q_shape, q_weight) = tensor("q.weight", 2)?;
        let (k_shape, k_weight) = tensor("k.weight", 2)?;
        let (qb_shape, q_bias) = tensor("q.bias", 1)?;
        let (kb_shape, k_bias) = tensor("k.bias", 1)?;
        if q_shape != k_shape || qb_shape[0] != q_shape[0] || kb_shape[0] != k_shape[0] {
            return Err(load(format!(
                "head shapes disagree: q {q_shape:?}, k {k_shape:?}, biases {qb_shape:?} {kb_shape:?}"
            )));
        }
        let matrix = |v: Vec<f32>| {
            Array2::from_shape_vec((q_shape[0], q_shape[1]), v)
                .map_err(|e| load(format!("head weight: {e}")))
        };
        let temperature = root
            .get("temperature")
            .and_then(PickleValue::as_f64)
            .unwrap_or(1.0) as f32;
        if !(temperature.is_finite() && temperature > 0.0) {
            return Err(load(format!("temperature {temperature} is not positive")));
        }
        let base_model = root
            .get("base")
            .and_then(PickleValue::as_str)
            .map(str::to_string)
            .ok_or_else(|| load("no `base` model named".into()))?;
        let base_revision = root
            .get("base_revision")
            .and_then(PickleValue::as_str)
            .map(str::to_string);
        Ok(Self {
            dir: adapter_dir.to_path_buf(),
            head: PointerHead {
                q_weight: matrix(q_weight)?,
                q_bias: Array1::from(q_bias),
                k_weight: matrix(k_weight)?,
                k_bias: Array1::from(k_bias),
                temperature,
            },
            base_model,
            base_revision,
        })
    }

    /// The base checkpoint the adapter was trained on, as a Hugging Face model id.
    pub fn base_model(&self) -> &str {
        &self.base_model
    }

    /// The base checkpoint's revision, when the head records it.
    pub fn base_revision(&self) -> Option<&str> {
        self.base_revision.as_deref()
    }

    /// The softmax temperature the head applies: its fitted value, or 1.
    pub fn temperature(&self) -> f32 {
        self.head.temperature
    }

    /// The width the head projects to.
    pub fn head_dim(&self) -> usize {
        self.head.q_weight.nrows()
    }
}

/// A pointer decision model: backbone with its adapter merged, tokenizer and head.
pub struct PointerDecisionModel {
    model: BertModel,
    tokenizer: Tokenizer,
    delimiters: [u32; 5],
    head: PointerHead,
    base_model: String,
    base_revision: Option<String>,
    max_state_tokens: usize,
    max_row_tokens: usize,
    batch: BatchOptions,
}

impl PointerDecisionModel {
    /// Loads the base checkpoint in `base_dir` (`config.json`, `model.safetensors`), merges the
    /// checkpoint's adapter into it in fp32, and reads the base checkpoint's tokenizer, which is
    /// the one the family tokenizes with, or the adapter's copy when the base has none. Nothing
    /// is downloaded.
    ///
    /// # Errors
    ///
    /// Fails when a file is missing or malformed, when the adapter does not fit the base, when
    /// the head does not fit the backbone's width, or when the tokenizer lacks a delimiter.
    pub fn load(
        checkpoint: PointerCheckpoint,
        base_dir: &Path,
        force_cpu: bool,
    ) -> Result<Self, DecisionError> {
        let config_path = base_dir.join("config.json");
        let text = std::fs::read_to_string(&config_path)
            .map_err(|e| DecisionError::Load(format!("{}: {e}", config_path.display())))?;
        if let Some(reason) = unsupported_backbone(&text) {
            return Err(DecisionError::Load(format!(
                "{}: {reason}",
                config_path.display()
            )));
        }
        let config: BertConfig = serde_json::from_str(&text)
            .map_err(|e| DecisionError::Load(format!("{}: {e}", config_path.display())))?;
        if !config.architecture().is_decoder_only() {
            return Err(DecisionError::Load(format!(
                "{} is not a decoder-only model",
                config_path.display()
            )));
        }
        if checkpoint.head.q_weight.ncols() != config.hidden_size {
            return Err(DecisionError::Load(format!(
                "the head reads {}-wide hidden states and the base model is {} wide",
                checkpoint.head.q_weight.ncols(),
                config.hidden_size
            )));
        }
        let weights = base_dir.join("model.safetensors");
        if !weights.is_file() {
            return Err(DecisionError::Load(format!(
                "{} is missing; sharded base checkpoints are not supported here",
                weights.display()
            )));
        }
        let adapter = LoraAdapter::load(&checkpoint.dir)?;
        let model = BertModel::load_with_lora(&weights, config, &adapter, force_cpu)?;
        let tokenizer_path = [
            base_dir.join("tokenizer.json"),
            checkpoint.dir.join("tokenizer.json"),
        ]
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| {
            DecisionError::Load("no tokenizer.json beside the adapter or base".into())
        })?;
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| DecisionError::Load(format!("{}: {e}", tokenizer_path.display())))?;
        tokenizer
            .with_truncation(None)
            .map_err(|e| DecisionError::Tokenizer(e.to_string()))?;
        tokenizer.with_padding(None);
        let mut delimiters = [0u32; 5];
        for (slot, token) in delimiters.iter_mut().zip(POINTER_DELIMITERS) {
            *slot = tokenizer.token_to_id(token).ok_or_else(|| {
                DecisionError::Load(format!("the tokenizer has no {token} token"))
            })?;
        }
        Ok(Self {
            model,
            tokenizer,
            delimiters,
            head: checkpoint.head,
            base_model: checkpoint.base_model,
            base_revision: checkpoint.base_revision,
            max_state_tokens: TRAINING_MAX_STATE_TOKENS,
            max_row_tokens: TRAINING_MAX_ROW_TOKENS,
            batch: BatchOptions::default(),
        })
    }

    /// Cuts states to `tokens` tokens, counting the opening delimiter, instead of the training
    /// length. A longer state is outside what the model was trained on.
    pub fn with_max_state_tokens(mut self, tokens: usize) -> Self {
        self.max_state_tokens = tokens.max(1);
        self
    }

    /// Refuses rows longer than `tokens` instead of the training length.
    pub fn with_max_row_tokens(mut self, tokens: usize) -> Self {
        self.max_row_tokens = tokens.max(2);
        self
    }

    /// How a batch of rows is scheduled.
    pub fn with_batch_options(mut self, options: BatchOptions) -> Self {
        self.batch = options;
        self
    }

    /// The base checkpoint this model was built on.
    pub fn base_model(&self) -> &str {
        &self.base_model
    }

    pub fn base_revision(&self) -> Option<&str> {
        self.base_revision.as_deref()
    }

    /// The backend the backbone runs on.
    pub fn backend(&self) -> gpu::GpuBackend {
        self.model.backend()
    }

    /// The backbone.
    pub fn model(&self) -> &BertModel {
        &self.model
    }

    /// Caller text as token ids, with any `<|name|>` defused so it cannot become a delimiter.
    pub fn user_tokens(&self, text: &str) -> Result<Vec<u32>, DecisionError> {
        let encoding = self
            .tokenizer
            .encode(defuse_special_tokens(text), false)
            .map_err(|e| DecisionError::Tokenizer(e.to_string()))?;
        Ok(encoding.get_ids().to_vec())
    }

    /// Every question of a record as the row the backbone runs.
    ///
    /// # Errors
    ///
    /// Fails when a question has no options, when tokenization fails, or when a row is longer
    /// than the row limit.
    pub fn encode(&self, record: &PointerRecord) -> Result<Vec<PointerRow>, DecisionError> {
        let [state_open, question_open, option_open, option_close, decide] = self.delimiters;
        let state_tokens = self.user_tokens(&record.state)?;
        let keep = self.max_state_tokens - 1;
        let state_truncated = state_tokens.len() > keep;
        let mut state = Vec::with_capacity(keep.min(state_tokens.len()) + 1);
        state.push(state_open);
        state.extend(state_tokens.into_iter().take(keep));

        record
            .questions
            .iter()
            .map(|question| {
                if question.options.is_empty() {
                    return Err(DecisionError::InvalidInput(
                        "a pointer question needs at least one option".into(),
                    ));
                }
                let mut ids = state.clone();
                ids.push(question_open);
                ids.extend(self.user_tokens(&question.instruction)?);
                let mut option_ends = Vec::with_capacity(question.options.len());
                for option in &question.options {
                    ids.push(option_open);
                    ids.extend(self.user_tokens(option)?);
                    ids.push(option_close);
                    option_ends.push(ids.len() - 1);
                }
                ids.push(decide);
                let limit = self
                    .max_row_tokens
                    .min(self.model.config.effective_max_seq_len());
                if ids.len() > limit {
                    return Err(DecisionError::PromptTooLong {
                        tokens: ids.len(),
                        limit,
                    });
                }
                Ok(PointerRow {
                    decide: ids.len() - 1,
                    ids,
                    option_ends,
                    state_truncated,
                })
            })
            .collect()
    }

    /// The probability of every option of every question of one record.
    pub fn answer(&self, record: &PointerRecord) -> Result<Vec<Vec<f32>>, DecisionError> {
        Ok(self
            .answer_batch(std::slice::from_ref(record))?
            .pop()
            .unwrap_or_default())
    }

    /// [`answer`](Self::answer) for many records: one list per record, one distribution per
    /// question, one probability per option.
    ///
    /// Every row is encoded before any runs, so a malformed record fails the batch without
    /// spending compute.
    pub fn answer_batch(
        &self,
        records: &[PointerRecord],
    ) -> Result<Vec<Vec<Vec<f32>>>, DecisionError> {
        Ok(self
            .logits_batch(records)?
            .into_iter()
            .map(|record| record.iter().map(|logits| softmax(logits)).collect())
            .collect())
    }

    /// The head's logits behind [`answer_batch`](Self::answer_batch), temperature applied and
    /// before the softmax, for a caller that calibrates them itself.
    pub fn logits_batch(
        &self,
        records: &[PointerRecord],
    ) -> Result<Vec<Vec<Vec<f32>>>, DecisionError> {
        let rows: Vec<Vec<PointerRow>> = records
            .iter()
            .map(|record| self.encode(record))
            .collect::<Result<_, _>>()?;
        let prompts: Vec<(&[u32], usize)> = rows
            .iter()
            .flatten()
            .map(|row| (row.ids.as_slice(), row.tail()))
            .collect();
        let hidden = self.model.final_hidden_tail_batch(&prompts, &self.batch)?;
        let mut hidden = hidden.into_iter();
        let mut answers = Vec::with_capacity(records.len());
        for record_rows in &rows {
            let mut record_answers = Vec::with_capacity(record_rows.len());
            for row in record_rows {
                let states = hidden.next().ok_or_else(|| {
                    DecisionError::Infer(crate::InferError::Internal(
                        "pointer batch returned fewer rows than it ran".into(),
                    ))
                })?;
                let first = row.ids.len() - row.tail();
                let options: Vec<ArrayView1<f32>> = row
                    .option_ends
                    .iter()
                    .map(|&end| states.row(end - first))
                    .collect();
                let logits = self.head.logits(states.row(row.decide - first), &options);
                if logits.iter().any(|l| !l.is_finite()) {
                    return Err(DecisionError::Infer(crate::InferError::Internal(
                        "the pointer head produced a non-finite logit".into(),
                    )));
                }
                record_answers.push(logits);
            }
            answers.push(record_answers);
        }
        Ok(answers)
    }

    /// Asks the same yes-or-no question about many states and returns, for each in order, the
    /// log-odds of `yes`: the `yes` logit minus the `no` logit. The probability of `yes` is its
    /// sigmoid.
    pub fn yes_log_odds(
        &self,
        states: &[&str],
        question: &PointerQuestion,
    ) -> Result<Vec<f32>, DecisionError> {
        if question.options.len() != 2 {
            return Err(DecisionError::InvalidInput(
                "a yes-or-no question has two options, no then yes".into(),
            ));
        }
        let records: Vec<PointerRecord> = states
            .iter()
            .map(|state| PointerRecord {
                state: (*state).to_string(),
                questions: vec![question.clone()],
            })
            .collect();
        self.logits_batch(&records)?
            .into_iter()
            .map(|record| match record.first().map(Vec::as_slice) {
                Some([no, yes]) => Ok(yes - no),
                _ => Err(DecisionError::Infer(crate::InferError::Internal(
                    "a yes-or-no row returned other than two logits".into(),
                ))),
            })
            .collect()
    }

    /// The probability of `yes` for each state: the sigmoid of
    /// [`yes_log_odds`](Self::yes_log_odds), which equals the head's softmax.
    pub fn yes_probabilities(
        &self,
        states: &[&str],
        question: &PointerQuestion,
    ) -> Result<Vec<f32>, DecisionError> {
        Ok(self
            .yes_log_odds(states, question)?
            .into_iter()
            .map(|odds| 1.0 / (1.0 + (-odds).exp()))
            .collect())
    }
}

/// Why a checkpoint's `config.json` names a backbone this runtime cannot run, when it can tell:
/// a hybrid model whose `layer_types` (at the top level or under `text_config`) include linear
/// attention layers, such as Qwen3.5's Gated DeltaNet.
pub fn unsupported_backbone(config_text: &str) -> Option<String> {
    let config: serde_json::Value = serde_json::from_str(config_text).ok()?;
    let hybrid = [Some(&config), config.get("text_config")]
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("layer_types").and_then(serde_json::Value::as_array))
        .flatten()
        .any(|layer| layer.as_str() == Some("linear_attention"));
    hybrid.then(|| {
        let model_type = config
            .get("model_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        format!(
            "the backbone ({model_type}) has linear-attention (Gated DeltaNet) layers, which this \
             runtime does not implement"
        )
    })
}

fn softmax(logits: &[f32]) -> Vec<f32> {
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let exp: Vec<f64> = logits.iter().map(|&l| f64::from(l - max).exp()).collect();
    let sum: f64 = exp.iter().sum();
    exp.into_iter().map(|e| (e / sum) as f32).collect()
}

/// Rewrites every `<|name|>` whose name is ASCII letters, digits and underscores to
/// `<¦name¦>`, the family's rule for keeping delimiters out of caller text.
pub fn defuse_special_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<|") {
        let after = &rest[start + 2..];
        let name_len = after
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
            .count();
        if name_len > 0 && after[name_len..].starts_with("|>") {
            out.push_str(&rest[..start]);
            out.push_str("<¦");
            out.push_str(&after[..name_len]);
            out.push_str("¦>");
            rest = &after[name_len + 2..];
        } else {
            out.push_str(&rest[..start + 2]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_text_cannot_spell_a_delimiter() {
        assert_eq!(
            defuse_special_tokens("a <|fim_suffix|> b <|box_end|>"),
            "a <¦fim_suffix¦> b <¦box_end¦>"
        );
        assert_eq!(
            defuse_special_tokens("x <| not |> y <|"),
            "x <| not |> y <|"
        );
        assert_eq!(defuse_special_tokens("<|a-b|>"), "<|a-b|>");
        assert_eq!(defuse_special_tokens("<|<|im_end|>"), "<|<¦im_end¦>");
    }

    #[test]
    fn a_hybrid_backbone_is_named_not_misread() {
        let qwen35 = r#"{"model_type": "qwen3_5", "text_config": {"model_type": "qwen3_5_text",
            "layer_types": ["linear_attention", "linear_attention", "full_attention"]}}"#;
        let reason = unsupported_backbone(qwen35).unwrap();
        assert!(
            reason.contains("Gated DeltaNet") && reason.contains("qwen3_5"),
            "{reason}"
        );
        assert!(
            unsupported_backbone(r#"{"model_type": "qwen3", "num_hidden_layers": 28}"#).is_none()
        );
    }

    #[test]
    fn yes_no_lists_no_first_as_the_family_was_trained() {
        let plain = PointerQuestion::yes_no("Is it?", None);
        assert_eq!(plain.options, vec!["no", "yes"]);
        let described = PointerQuestion::yes_no("Is it?", Some(("it is", "it is not")));
        assert_eq!(described.options, vec!["no: it is not", "yes: it is"]);
    }

    #[test]
    fn the_head_scores_by_projected_dot_product_over_temperature() {
        let head = PointerHead {
            q_weight: Array2::from_shape_vec((2, 3), vec![1.0, 0.0, 0.0, 0.0, 1.0, 0.0]).unwrap(),
            q_bias: Array1::from(vec![0.0, 1.0]),
            k_weight: Array2::from_shape_vec((2, 3), vec![0.0, 0.0, 1.0, 1.0, 0.0, 0.0]).unwrap(),
            k_bias: Array1::from(vec![0.5, 0.0]),
            temperature: 2.0,
        };
        let decide = Array1::from(vec![2.0, 1.0, 0.0]);
        let a = Array1::from(vec![1.0, 0.0, 3.0]);
        let b = Array1::from(vec![0.0, 0.0, 0.0]);
        // query = (2, 2); key a = (3.5, 1), key b = (0.5, 0); scale = 1 / sqrt(2) / 2.
        let logits = head.logits(decide.view(), &[a.view(), b.view()]);
        let scale = 1.0 / 2f32.sqrt() / 2.0;
        assert!((logits[0] - 9.0 * scale).abs() < 1e-6, "{logits:?}");
        assert!((logits[1] - 1.0 * scale).abs() < 1e-6, "{logits:?}");
        let p = softmax(&logits);
        assert!((p[0] + p[1] - 1.0).abs() < 1e-6 && p[0] > p[1]);
    }
}
