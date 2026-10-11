// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Yes-or-no rerankers: a causal language model asked whether a document meets a query.
//!
//! A reranker of this kind (Qwen3-Reranker) is a decoder fine-tuned to answer one question with
//! the next token `yes` or `no`. Its score for a document is the `yes` logit minus the `no`
//! logit at the last prompt position, and the probability that the document meets the query is
//! the sigmoid of that difference, which is the softmax over the two tokens.
//!
//! The prompt is the model card's, token for token. A system turn states the task, a user turn
//! carries the instruction, the query and the document, and the assistant turn opens with an
//! empty thinking block:
//!
//! ```text
//! <|im_start|>system
//! Judge whether the Document meets the requirements based on the Query and the Instruct provided. Note that the answer can only be "yes" or "no".<|im_end|>
//! <|im_start|>user
//! <Instruct>: {instruction}
//! <Query>: {query}
//! <Document>: {document}<|im_end|>
//! <|im_start|>assistant
//! <think>
//!
//! </think>
//!
//! ```
//!
//! The text before the user turn's content and the text after it are tokenized on their own, and
//! the `<Instruct>` to `<Document>` block is tokenized as one string, as the card's reference code
//! does. The block is cut from its end when the whole prompt would pass the length limit. One
//! deliberate difference: any `<|name|>` inside the instruction, the query or the document is
//! first rewritten to `<¦name¦>`, so text under judgment cannot close the user turn and answer
//! for the model. Text without such a sequence tokenizes exactly as the card's code does.
//!
//! [`YesNoReranker::load`] accepts a checkpoint directory only when it carries the template:
//! the sentence-transformers `1_LogitScore` module naming the two answer tokens, or a chat
//! template with the card's `<Instruct>` block.

use std::path::Path;

use tokenizers::Tokenizer;

use super::{defuse_special_tokens, DecisionError};
use crate::{gpu, BatchOptions, BertConfig, BertModel, ScoreRequest};

/// The system turn and the opening of the user turn.
pub const QWEN3_RERANKER_PREFIX: &str = "<|im_start|>system\nJudge whether the Document meets the requirements based on the Query and the Instruct provided. Note that the answer can only be \"yes\" or \"no\".<|im_end|>\n<|im_start|>user\n";

/// The close of the user turn and the assistant turn's empty thinking block.
pub const QWEN3_RERANKER_SUFFIX: &str =
    "<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n";

/// The instruction the card uses when a caller gives none.
pub const QWEN3_RERANKER_DEFAULT_INSTRUCTION: &str =
    "Given a web search query, retrieve relevant passages that answer the query";

/// The longest prompt the card's reference code builds.
pub const QWEN3_RERANKER_MAX_TOKENS: usize = 8192;

/// A loaded yes-or-no reranker.
pub struct YesNoReranker {
    model: BertModel,
    tokenizer: Tokenizer,
    prefix_ids: Vec<u32>,
    suffix_ids: Vec<u32>,
    yes_id: u32,
    no_id: u32,
    max_tokens: usize,
    batch: BatchOptions,
}

#[derive(serde::Deserialize)]
struct LogitScoreConfig {
    true_token_id: u32,
    false_token_id: u32,
}

impl YesNoReranker {
    /// Loads `config.json`, `model.safetensors` and `tokenizer.json` from a reranker checkpoint
    /// directory. Nothing is downloaded.
    ///
    /// # Errors
    ///
    /// Fails when a file is missing or malformed, when the model is not decoder-only, when the
    /// directory does not carry the reranker template, or when the tokenizer lacks a token the
    /// template needs.
    pub fn load(dir: &Path, force_cpu: bool) -> Result<Self, DecisionError> {
        let load = |m: String| DecisionError::Load(m);
        let config_path = dir.join("config.json");
        let text = std::fs::read_to_string(&config_path)
            .map_err(|e| load(format!("{}: {e}", config_path.display())))?;
        if let Some(reason) = super::pointer::unsupported_backbone(&text) {
            return Err(load(format!("{}: {reason}", config_path.display())));
        }
        let config: BertConfig = serde_json::from_str(&text)
            .map_err(|e| load(format!("{}: {e}", config_path.display())))?;
        if !config.architecture().is_decoder_only() {
            return Err(load(format!(
                "{} is not a decoder-only model",
                config_path.display()
            )));
        }
        let tokenizer_path = dir.join("tokenizer.json");
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| load(format!("{}: {e}", tokenizer_path.display())))?;
        tokenizer
            .with_truncation(None)
            .map_err(|e| DecisionError::Tokenizer(e.to_string()))?;
        tokenizer.with_padding(None);

        let logit_score = dir.join("1_LogitScore").join("config.json");
        let (yes_id, no_id) = match std::fs::read_to_string(&logit_score) {
            Ok(text) => {
                let ids: LogitScoreConfig = serde_json::from_str(&text)
                    .map_err(|e| load(format!("{}: {e}", logit_score.display())))?;
                (ids.true_token_id, ids.false_token_id)
            }
            Err(_) => {
                let template =
                    std::fs::read_to_string(dir.join("chat_template.jinja")).unwrap_or_default();
                if !template.contains("<Instruct>") || !template.contains("<Document>") {
                    return Err(load(format!(
                        "{} carries no yes-or-no reranker template (no 1_LogitScore module and \
                         no <Instruct> chat template)",
                        dir.display()
                    )));
                }
                let id = |token: &str| {
                    tokenizer
                        .token_to_id(token)
                        .ok_or_else(|| load(format!("the tokenizer has no `{token}` token")))
                };
                (id("yes")?, id("no")?)
            }
        };
        let vocab = config.vocab_size;
        if yes_id as usize >= vocab || no_id as usize >= vocab {
            return Err(load(format!(
                "answer tokens {yes_id} and {no_id} are outside the {vocab}-token vocabulary"
            )));
        }
        let encode = |text: &str| -> Result<Vec<u32>, DecisionError> {
            Ok(tokenizer
                .encode(text, false)
                .map_err(|e| DecisionError::Tokenizer(e.to_string()))?
                .get_ids()
                .to_vec())
        };
        let prefix_ids = encode(QWEN3_RERANKER_PREFIX)?;
        let suffix_ids = encode(QWEN3_RERANKER_SUFFIX)?;
        if tokenizer.token_to_id("<|im_start|>").is_none() {
            return Err(load("the tokenizer has no <|im_start|> token".into()));
        }

        let weights = dir.join("model.safetensors");
        if !weights.is_file() {
            return Err(load(format!(
                "{} is missing; sharded checkpoints are not supported here",
                weights.display()
            )));
        }
        let model = BertModel::load_with_backend(&weights, config, force_cpu)?;
        let max_tokens = QWEN3_RERANKER_MAX_TOKENS.min(model.config.effective_max_seq_len());
        Ok(Self {
            model,
            tokenizer,
            prefix_ids,
            suffix_ids,
            yes_id,
            no_id,
            max_tokens,
            batch: BatchOptions::default(),
        })
    }

    /// Caps a prompt at `tokens`, instead of the card's 8192 or the model's window.
    pub fn with_max_tokens(mut self, tokens: usize) -> Self {
        self.max_tokens = tokens.min(self.model.config.effective_max_seq_len());
        self
    }

    /// How a batch of prompts is scheduled.
    pub fn with_batch_options(mut self, options: BatchOptions) -> Self {
        self.batch = options;
        self
    }

    /// The backend the model runs on.
    pub fn backend(&self) -> gpu::GpuBackend {
        self.model.backend()
    }

    /// The model.
    pub fn model(&self) -> &BertModel {
        &self.model
    }

    /// The ids of the `yes` and `no` answer tokens.
    pub fn answer_ids(&self) -> (u32, u32) {
        (self.yes_id, self.no_id)
    }

    /// The prompt for one document, as token ids.
    ///
    /// # Errors
    ///
    /// Fails when tokenization fails, or when the fixed parts alone reach the length limit.
    pub fn prompt_ids(
        &self,
        instruction: &str,
        query: &str,
        document: &str,
    ) -> Result<Vec<u32>, DecisionError> {
        let block = format!(
            "<Instruct>: {}\n<Query>: {}\n<Document>: {}",
            defuse_special_tokens(instruction),
            defuse_special_tokens(query),
            defuse_special_tokens(document)
        );
        let mut block_ids = self
            .tokenizer
            .encode(block, false)
            .map_err(|e| DecisionError::Tokenizer(e.to_string()))?
            .get_ids()
            .to_vec();
        let fixed = self.prefix_ids.len() + self.suffix_ids.len();
        if fixed >= self.max_tokens {
            return Err(DecisionError::PromptTooLong {
                tokens: fixed,
                limit: self.max_tokens,
            });
        }
        block_ids.truncate(self.max_tokens - fixed);
        let mut ids = Vec::with_capacity(fixed + block_ids.len());
        ids.extend_from_slice(&self.prefix_ids);
        ids.extend(block_ids);
        ids.extend_from_slice(&self.suffix_ids);
        Ok(ids)
    }

    /// For each document in order, the log-odds that it meets the query: the `yes` logit minus
    /// the `no` logit after its prompt. This is the card's raw score; its sigmoid is the
    /// probability.
    ///
    /// Every prompt is built before any runs, so a malformed one fails the batch without
    /// spending compute.
    pub fn log_odds(
        &self,
        instruction: &str,
        query: &str,
        documents: &[&str],
    ) -> Result<Vec<f32>, DecisionError> {
        let prompts: Vec<Vec<u32>> = documents
            .iter()
            .map(|document| self.prompt_ids(instruction, query, document))
            .collect::<Result<_, _>>()?;
        let answers = [self.no_id, self.yes_id];
        let requests: Vec<ScoreRequest<'_>> = prompts
            .iter()
            .map(|ids| ScoreRequest::new(ids, &answers))
            .collect();
        let logits = self
            .model
            .score_next_tokens_batch_with(&requests, &self.batch)?;
        logits
            .into_iter()
            .map(|pair| match pair.as_slice() {
                [no, yes] if no.is_finite() && yes.is_finite() => Ok(yes - no),
                _ => Err(DecisionError::Infer(crate::InferError::Internal(
                    "the reranker produced a non-finite answer logit".into(),
                ))),
            })
            .collect()
    }

    /// The probability that each document meets the query: the sigmoid of
    /// [`log_odds`](Self::log_odds).
    pub fn probabilities(
        &self,
        instruction: &str,
        query: &str,
        documents: &[&str],
    ) -> Result<Vec<f32>, DecisionError> {
        Ok(self
            .log_odds(instruction, query, documents)?
            .into_iter()
            .map(|odds| 1.0 / (1.0 + (-odds).exp()))
            .collect())
    }
}
