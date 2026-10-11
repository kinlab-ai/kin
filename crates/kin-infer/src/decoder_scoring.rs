// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Throughput paths for decoder next-token scoring.
//!
//! A decision prompt is usually a long shared state, such as the caller's code and imports,
//! followed by a short question and its options. The same state is asked several questions, and
//! each question is asked in more than one option order to cancel position bias, so the state is
//! worth encoding once.
//!
//! - [`BertModel::prefill`] runs a prefix through the decoder once and keeps every layer's keys
//!   and values in a [`PrefixState`].
//! - [`BertModel::score_continuation`] runs only a suffix against a copy of that state. It
//!   returns the logits the generic decoder pass returns for the whole prompt.
//! - [`BertModel::score_next_tokens_batch`] scores many requests, each a whole prompt or a
//!   continuation of a prefix. On the CPU it runs several at once under a memory budget.
//!
//! A prefix and its continuations run the decoder's generic forward pass, the one
//! `score_next_tokens_reference` exposes, so a continuation reproduces that pass over the whole
//! prompt to within float rounding on every backend. [`BertModel::score_next_tokens`] takes a
//! faster prefill when the backend has one, and a continuation agrees with it only as closely
//! as that prefill agrees with the generic pass: to the last few bits on the CPU, and to the
//! rounding of fp16 weights on Metal. A whole-prompt request in a batch scores through
//! `score_next_tokens`.

use std::cmp::Reverse;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ndarray::{Array1, Array2};
use rayon::prelude::*;

use crate::{gpu, BertModel, InferError, KvCache};

/// The decoder's state after a prompt prefix: the keys and values every layer computed for the
/// prefix, and the final hidden state of its last token.
///
/// A state belongs to the model that built it. It is read-only once built, so one state can
/// serve any number of continuations, including concurrent ones from several threads.
#[derive(Clone)]
pub struct PrefixState {
    cache: KvCache,
    /// Final-normed hidden state of the last prefix token, so an empty suffix still scores.
    /// `None` only for an empty prefix.
    last_hidden: Option<Array1<f32>>,
    len: usize,
}

impl PrefixState {
    /// Number of prefix tokens. The first suffix token takes this position.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the prefix holds no tokens.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes held by the cached keys and values. Each continuation copies them once.
    #[must_use]
    pub fn cache_bytes(&self) -> usize {
        self.cache
            .key
            .iter()
            .chain(&self.cache.value)
            .map(|a| a.len() * std::mem::size_of::<f32>())
            .sum()
    }
}

/// One request in a scoring batch: a whole prompt, or a suffix that continues a prefilled
/// prefix. The result is the next-token logits for `candidate_ids`, in that order.
#[derive(Clone, Copy)]
pub struct ScoreRequest<'a> {
    prefix: Option<&'a PrefixState>,
    token_ids: &'a [u32],
    candidate_ids: &'a [u32],
}

impl<'a> ScoreRequest<'a> {
    /// Score the whole prompt `token_ids`, as [`BertModel::score_next_tokens`] does.
    #[must_use]
    pub fn new(token_ids: &'a [u32], candidate_ids: &'a [u32]) -> Self {
        Self {
            prefix: None,
            token_ids,
            candidate_ids,
        }
    }

    /// Score `suffix_ids` after `prefix`, as [`BertModel::score_continuation`] does.
    #[must_use]
    pub fn continuation(
        prefix: &'a PrefixState,
        suffix_ids: &'a [u32],
        candidate_ids: &'a [u32],
    ) -> Self {
        Self {
            prefix: Some(prefix),
            token_ids: suffix_ids,
            candidate_ids,
        }
    }

    /// The prefix this request continues, if any.
    #[must_use]
    pub fn prefix(&self) -> Option<&'a PrefixState> {
        self.prefix
    }

    /// The tokens this request runs through the decoder: the whole prompt, or the suffix.
    #[must_use]
    pub fn token_ids(&self) -> &'a [u32] {
        self.token_ids
    }

    /// The tokens whose logits the request returns.
    #[must_use]
    pub fn candidate_ids(&self) -> &'a [u32] {
        self.candidate_ids
    }

    /// Positions the request attends over, prefix included.
    #[must_use]
    pub fn total_len(&self) -> usize {
        self.prefix.map_or(0, PrefixState::len) + self.token_ids.len()
    }
}

/// Limits for [`BertModel::score_next_tokens_batch_with`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchOptions {
    /// Most requests in flight at once. `0` picks the default for the backend: on the CPU,
    /// [`DEFAULT_CPU_IN_FLIGHT`] capped by the rayon pool's size. Accelerator backends run one
    /// request at a time until they gain a batched decoder path.
    pub max_in_flight: usize,
    /// Rough ceiling, in bytes, on the working memory the in-flight requests hold together:
    /// each one's key/value cache plus its largest per-layer temporaries. At least one request
    /// always runs, however large.
    pub memory_budget_bytes: usize,
}

/// Default number of CPU requests in flight.
///
/// The CPU matmul splits its work into blocks of 64 rows, one rayon task each, so a prompt of a
/// few hundred tokens keeps only a handful of cores busy, and a short continuation of a prefix
/// keeps one or two. Running several requests at once fills the rest. The requests also share
/// memory bandwidth and the rayon workers their own matmuls use, so the default stays small;
/// set [`BatchOptions::max_in_flight`] to tune it for a host.
pub const DEFAULT_CPU_IN_FLIGHT: usize = 4;

/// Default working-memory ceiling for a batch, 2 GiB.
pub const DEFAULT_BATCH_MEMORY_BUDGET: usize = 2 << 30;

impl Default for BatchOptions {
    fn default() -> Self {
        Self {
            max_in_flight: 0,
            memory_budget_bytes: DEFAULT_BATCH_MEMORY_BUDGET,
        }
    }
}

impl BertModel {
    /// Run `prefix_ids` through the decoder once and keep the result for
    /// [`score_continuation`](Self::score_continuation).
    ///
    /// An empty prefix is allowed and holds no state; its continuations score like whole
    /// prompts.
    ///
    /// # Errors
    ///
    /// Fails when the model is not a decoder, when a token id is outside the vocabulary, when
    /// the prefix is longer than the model's positions, or when the forward pass fails.
    pub fn prefill(&self, prefix_ids: &[u32]) -> Result<PrefixState, InferError> {
        self.require_scoring_decoder("prefill")?;
        self.check_token_ids(prefix_ids)?;
        self.check_positions(prefix_ids.len())?;
        let mut cache = self.empty_kv_cache();
        let last_hidden = if prefix_ids.is_empty() {
            None
        } else {
            let hidden = self.decoder_forward(prefix_ids, &mut cache, 0)?;
            Some(hidden.row(hidden.nrows() - 1).to_owned())
        };
        Ok(PrefixState {
            cache,
            last_hidden,
            len: prefix_ids.len(),
        })
    }

    /// Next-token logits for `candidate_ids` after `prefix` followed by `suffix_ids`.
    ///
    /// Only the suffix runs through the decoder, against a copy of the prefix's cached keys and
    /// values, so the prefix itself is left untouched for the next continuation. The result is
    /// what the generic decoder pass returns for the concatenated prompt, which
    /// [`score_next_tokens`](Self::score_next_tokens) matches to its prefill's rounding (see
    /// the module documentation). An empty suffix scores the prefix's own last position.
    ///
    /// # Errors
    ///
    /// Fails when the model is not a decoder, when `prefix` came from a model of another shape,
    /// when a token or candidate id is outside the vocabulary, when prefix and suffix together
    /// pass the model's positions, when both are empty, or when the forward pass fails.
    pub fn score_continuation(
        &self,
        prefix: &PrefixState,
        suffix_ids: &[u32],
        candidate_ids: &[u32],
    ) -> Result<Vec<f32>, InferError> {
        self.require_scoring_decoder("score_continuation")?;
        self.check_prefix(prefix)?;
        self.check_token_ids(suffix_ids)?;
        self.check_positions(prefix.len + suffix_ids.len())?;
        self.continuation_logits(prefix, suffix_ids, candidate_ids)
    }

    /// Score many independent requests with the default [`BatchOptions`].
    ///
    /// Results come back in request order and equal what scoring each request on its own
    /// returns. Every request is checked before any runs, so a malformed one fails the batch
    /// without spending compute.
    ///
    /// # Errors
    ///
    /// Fails for any request [`score_next_tokens`](Self::score_next_tokens) or
    /// [`score_continuation`](Self::score_continuation) would refuse. The checks run in
    /// request order before any scoring, so the error names the first malformed request.
    pub fn score_next_tokens_batch(
        &self,
        requests: &[ScoreRequest<'_>],
    ) -> Result<Vec<Vec<f32>>, InferError> {
        self.score_next_tokens_batch_with(requests, &BatchOptions::default())
    }

    /// Score many independent requests under explicit limits.
    ///
    /// On the CPU, up to `options.max_in_flight` requests run at once, longest first, each on
    /// its own rayon task, and the in-flight count also stays under
    /// `options.memory_budget_bytes` by an estimate of each request's working set. Accelerator
    /// backends run the requests one at a time for now, since concurrent host threads would only
    /// queue on the one device; a batched device path can replace that branch without changing
    /// this signature.
    ///
    /// # Errors
    ///
    /// As for [`score_next_tokens_batch`](Self::score_next_tokens_batch).
    pub fn score_next_tokens_batch_with(
        &self,
        requests: &[ScoreRequest<'_>],
        options: &BatchOptions,
    ) -> Result<Vec<Vec<f32>>, InferError> {
        self.require_scoring_decoder("score_next_tokens_batch")?;
        for request in requests {
            self.check_request(request)?;
        }
        let in_flight = self.batch_in_flight(requests, options);
        let _span = tracing::info_span!(
            "kin_infer.model.score_next_tokens_batch",
            requests = requests.len(),
            in_flight = in_flight,
            backend = %self.backend()
        )
        .entered();
        if in_flight <= 1 {
            return requests.iter().map(|r| self.score_request(r)).collect();
        }
        self.score_requests_concurrently(requests, in_flight)
    }

    /// [`final_hidden_tail`](Self::final_hidden_tail) for many prompts, each with its own
    /// tail length, in prompt order.
    ///
    /// On the CPU up to `options.max_in_flight` prompts run at once, longest first, under
    /// `options.memory_budget_bytes`, as [`score_next_tokens_batch_with`] schedules them.
    /// Accelerator backends run the prompts one at a time, since concurrent host threads would
    /// only queue on the one device.
    ///
    /// # Errors
    ///
    /// Fails for the first prompt, in prompt order, that `final_hidden_tail` would refuse or
    /// that fails in its forward pass.
    ///
    /// [`score_next_tokens_batch_with`]: Self::score_next_tokens_batch_with
    pub fn final_hidden_tail_batch(
        &self,
        prompts: &[(&[u32], usize)],
        options: &BatchOptions,
    ) -> Result<Vec<Array2<f32>>, InferError> {
        let requests: Vec<ScoreRequest<'_>> = prompts
            .iter()
            .map(|(ids, _)| ScoreRequest::new(ids, &[]))
            .collect();
        let in_flight = self.batch_in_flight(&requests, options);
        let _span = tracing::info_span!(
            "kin_infer.model.final_hidden_tail_batch",
            prompts = prompts.len(),
            in_flight = in_flight,
            backend = %self.backend()
        )
        .entered();
        if in_flight <= 1 {
            return prompts
                .iter()
                .map(|(ids, tail)| self.final_hidden_tail(ids, *tail))
                .collect();
        }
        let mut order: Vec<usize> = (0..prompts.len()).collect();
        order.sort_by_key(|&i| Reverse(prompts[i].0.len()));
        let next = AtomicUsize::new(0);
        let parts: Vec<Vec<(usize, Result<Array2<f32>, InferError>)>> = (0..in_flight)
            .into_par_iter()
            .with_max_len(1)
            .map(|_| {
                let mut done = Vec::new();
                while let Some(&i) = order.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let (ids, tail) = prompts[i];
                    done.push((i, self.final_hidden_tail(ids, tail)));
                }
                done
            })
            .collect();
        let mut results: Vec<Option<Result<Array2<f32>, InferError>>> =
            (0..prompts.len()).map(|_| None).collect();
        for (i, result) in parts.into_iter().flatten() {
            results[i] = Some(result);
        }
        results
            .into_iter()
            .map(|r| {
                r.unwrap_or_else(|| {
                    Err(InferError::Internal(
                        "final_hidden_tail_batch: a prompt was never run".into(),
                    ))
                })
            })
            .collect()
    }

    /// Run requests on `in_flight` rayon tasks that each pull the next request, longest first,
    /// until none are left. No more than `in_flight` requests hold memory at once, and a long
    /// request never waits behind a batch boundary.
    fn score_requests_concurrently(
        &self,
        requests: &[ScoreRequest<'_>],
        in_flight: usize,
    ) -> Result<Vec<Vec<f32>>, InferError> {
        let mut order: Vec<usize> = (0..requests.len()).collect();
        order.sort_by_key(|&i| Reverse(self.request_work(&requests[i])));
        let next = AtomicUsize::new(0);
        let failed = AtomicBool::new(false);
        let parts: Vec<Vec<(usize, Result<Vec<f32>, InferError>)>> = (0..in_flight)
            .into_par_iter()
            .with_max_len(1)
            .map(|_| {
                let mut done = Vec::new();
                while !failed.load(Ordering::Relaxed) {
                    let Some(&i) = order.get(next.fetch_add(1, Ordering::Relaxed)) else {
                        break;
                    };
                    let result = self.score_request(&requests[i]);
                    if result.is_err() {
                        failed.store(true, Ordering::Relaxed);
                    }
                    done.push((i, result));
                }
                done
            })
            .collect();

        let mut results: Vec<Option<Vec<f32>>> = vec![None; requests.len()];
        let mut first_error: Option<(usize, InferError)> = None;
        for (i, result) in parts.into_iter().flatten() {
            match result {
                Ok(logits) => results[i] = Some(logits),
                Err(e) => {
                    if first_error.as_ref().is_none_or(|(j, _)| i < *j) {
                        first_error = Some((i, e));
                    }
                }
            }
        }
        if let Some((_, e)) = first_error {
            return Err(e);
        }
        results
            .into_iter()
            .map(|r| r.ok_or_else(|| InferError::Internal("batch request was never scored".into())))
            .collect()
    }

    /// How many requests to run at once: the requested or default count, capped by the pool
    /// size, the request count and the memory budget.
    fn batch_in_flight(&self, requests: &[ScoreRequest<'_>], options: &BatchOptions) -> usize {
        if requests.len() <= 1 || self.backend() != gpu::GpuBackend::Cpu {
            return 1;
        }
        let wanted = match options.max_in_flight {
            0 => DEFAULT_CPU_IN_FLIGHT.min(rayon::current_num_threads()),
            n => n,
        };
        let largest = requests
            .iter()
            .map(|r| self.request_working_bytes(r))
            .max()
            .unwrap_or(0)
            .max(1);
        let by_memory = (options.memory_budget_bytes / largest).max(1);
        wanted.min(by_memory).min(requests.len()).max(1)
    }

    /// Estimated peak bytes one request holds while it runs: the full key/value cache, the
    /// cache copy each layer's append makes, the grouped-query expansion of keys and values,
    /// one head's attention scores, and the hidden, attention and feed-forward activations of
    /// the rows it runs.
    fn request_working_bytes(&self, request: &ScoreRequest<'_>) -> usize {
        let total = request.total_len();
        let rows = request.token_ids.len();
        let hidden = self.config.hidden_size;
        let q_dim = self.config.num_attention_heads * self.head_dim;
        let kv_dim = self.config.effective_num_kv_heads() * self.head_dim;
        let inter = self.config.intermediate_size;
        let layers = self.config.num_hidden_layers;
        let floats = layers * 2 * total * kv_dim
            + 2 * total * kv_dim
            + 2 * total * q_dim
            + 2 * rows * total
            + rows * (4 * hidden + 3 * q_dim + 3 * inter);
        floats * std::mem::size_of::<f32>()
    }

    /// Score one request that has already passed [`check_request`](Self::check_request).
    fn score_request(&self, request: &ScoreRequest<'_>) -> Result<Vec<f32>, InferError> {
        match request.prefix {
            Some(prefix) => {
                self.continuation_logits(prefix, request.token_ids, request.candidate_ids)
            }
            None => self.score_next_tokens(request.token_ids, request.candidate_ids),
        }
    }

    fn continuation_logits(
        &self,
        prefix: &PrefixState,
        suffix_ids: &[u32],
        candidate_ids: &[u32],
    ) -> Result<Vec<f32>, InferError> {
        if suffix_ids.is_empty() {
            let last = prefix.last_hidden.as_ref().ok_or_else(|| {
                InferError::Internal("score_continuation: empty prefix and empty suffix".into())
            })?;
            return self.candidate_logits(last.view(), candidate_ids);
        }
        let mut cache = prefix.cache.clone();
        let hidden = self.decoder_forward(suffix_ids, &mut cache, prefix.len)?;
        self.candidate_logits(hidden.row(hidden.nrows() - 1), candidate_ids)
    }

    fn check_request(&self, request: &ScoreRequest<'_>) -> Result<(), InferError> {
        if let Some(prefix) = request.prefix {
            self.check_prefix(prefix)?;
        }
        if request.total_len() == 0 {
            return Err(InferError::Internal(
                "score_next_tokens_batch: empty prompt".into(),
            ));
        }
        self.check_token_ids(request.token_ids)?;
        self.check_positions(request.total_len())?;
        let vocab = self.output_rows();
        if let Some(&id) = request
            .candidate_ids
            .iter()
            .find(|&&id| id as usize >= vocab)
        {
            return Err(InferError::Internal(format!(
                "candidate token {id} outside vocabulary"
            )));
        }
        Ok(())
    }

    fn require_scoring_decoder(&self, what: &str) -> Result<(), InferError> {
        if self.config.architecture().is_decoder_only() {
            Ok(())
        } else {
            Err(InferError::ModelIncompatible(format!(
                "{what} needs a decoder-only model"
            )))
        }
    }

    fn check_token_ids(&self, ids: &[u32]) -> Result<(), InferError> {
        let vocab = self.weights.word_embeddings.nrows();
        match ids.iter().find(|&&id| id as usize >= vocab) {
            Some(id) => Err(InferError::Internal(format!(
                "token {id} outside vocabulary of {vocab}"
            ))),
            None => Ok(()),
        }
    }

    /// Refuse positions past the rotary table, which the decoder would otherwise leave
    /// unrotated.
    fn check_positions(&self, total_len: usize) -> Result<(), InferError> {
        match self.rope_cos {
            Some(ref cos) if total_len > cos.nrows() => Err(InferError::Internal(format!(
                "prompt of {total_len} tokens exceeds the model's {} positions",
                cos.nrows()
            ))),
            _ => Ok(()),
        }
    }

    fn check_prefix(&self, prefix: &PrefixState) -> Result<(), InferError> {
        let kv_dim = self.kv_cache_width();
        let layers = self.config.num_hidden_layers;
        let fits = prefix.cache.key.len() == layers
            && prefix.cache.value.len() == layers
            && prefix
                .cache
                .key
                .iter()
                .chain(&prefix.cache.value)
                .all(|a| a.ncols() == kv_dim && a.nrows() == prefix.len);
        if fits {
            Ok(())
        } else {
            Err(InferError::ModelIncompatible(
                "prefix state was built by a model of a different shape".into(),
            ))
        }
    }

    fn output_rows(&self) -> usize {
        match self.weights.lm_head_weight {
            Some(ref w) => w.nrows(),
            None => self.weights.word_embeddings.nrows(),
        }
    }

    fn kv_cache_width(&self) -> usize {
        self.config.effective_num_kv_heads() * self.head_dim
    }

    fn empty_kv_cache(&self) -> KvCache {
        KvCache::new(self.config.num_hidden_layers, self.kv_cache_width())
    }

    /// Multiply-adds per layer for one request, for longest-first scheduling: every row it runs
    /// through the projections and the feed-forward block, plus attention over every position
    /// the row sees.
    fn request_work(&self, request: &ScoreRequest<'_>) -> usize {
        let rows = request.token_ids.len();
        let hidden = self.config.hidden_size;
        let q_dim = self.config.num_attention_heads * self.head_dim;
        let kv_dim = self.kv_cache_width();
        let per_row = hidden * (2 * q_dim + 2 * kv_dim + 3 * self.config.intermediate_size);
        rows * (per_row + 2 * q_dim * request.total_len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::Array2;

    fn rows(n: usize, width: usize, start: f32) -> Array2<f32> {
        Array2::from_shape_fn((n, width), |(i, j)| start + (i * width + j) as f32)
    }

    #[test]
    fn cloned_cache_extends_without_touching_the_original() {
        let mut prefix = KvCache::new(2, 3);
        for layer in 0..2 {
            prefix
                .append_kv(layer, &rows(4, 3, 0.0), &rows(4, 3, 100.0))
                .unwrap();
        }
        let before_key = prefix.key.clone();
        let before_value = prefix.value.clone();

        let mut first = prefix.clone();
        let mut second = prefix.clone();
        for layer in 0..2 {
            let (k, v) = first
                .append_kv(layer, &rows(2, 3, 1000.0), &rows(2, 3, 2000.0))
                .unwrap();
            assert_eq!(k.nrows(), 6);
            assert_eq!(v.nrows(), 6);
            second
                .append_kv(layer, &rows(1, 3, -1.0), &rows(1, 3, -2.0))
                .unwrap();
        }

        // The prefix keeps exactly its four rows in every layer.
        assert_eq!(prefix.key, before_key);
        assert_eq!(prefix.value, before_value);
        for layer in 0..2 {
            // Each extension holds the prefix rows first, then only its own new rows.
            let k = &first.key[layer];
            assert_eq!(k.slice(ndarray::s![..4, ..]), before_key[layer]);
            assert_eq!(k.slice(ndarray::s![4.., ..]), rows(2, 3, 1000.0));
            let v = &first.value[layer];
            assert_eq!(v.slice(ndarray::s![..4, ..]), before_value[layer]);
            assert_eq!(v.slice(ndarray::s![4.., ..]), rows(2, 3, 2000.0));
            assert_eq!(second.key[layer].nrows(), 5);
            assert_eq!(
                second.key[layer].slice(ndarray::s![4.., ..]),
                rows(1, 3, -1.0)
            );
        }
    }

    #[test]
    fn request_lengths_count_the_prefix() {
        let mut cache = KvCache::new(1, 2);
        cache
            .append_kv(0, &rows(3, 2, 0.0), &rows(3, 2, 0.0))
            .unwrap();
        let prefix = PrefixState {
            cache,
            last_hidden: Some(Array1::zeros(4)),
            len: 3,
        };
        assert_eq!(prefix.len(), 3);
        assert!(!prefix.is_empty());
        assert_eq!(prefix.cache_bytes(), 2 * 3 * 2 * 4);

        let whole = ScoreRequest::new(&[1, 2, 3, 4, 5], &[7]);
        let tail = ScoreRequest::continuation(&prefix, &[4, 5], &[7]);
        assert_eq!(whole.total_len(), 5);
        assert_eq!(tail.total_len(), 5);
        assert!(tail.prefix().is_some());
        assert_eq!(tail.token_ids(), &[4, 5]);
        assert_eq!(whole.candidate_ids(), &[7]);
    }

    // -- A tiny decoder ------------------------------------------------------
    // Qwen3's shape at toy size: grouped-query attention, per-head q/k RMSNorm, heads wider
    // than the model, SwiGLU and tied embeddings. The weights are deterministic and go through
    // the real safetensors loader, so the continuation and batch paths are checked for
    // exactness without a model on disk.

    const VOCAB: usize = 50;
    const MAX_POSITIONS: usize = 160;

    fn tiny_decoder(layers: usize) -> BertModel {
        let config: crate::BertConfig = serde_json::from_value(serde_json::json!({
            "model_type": "qwen3",
            "hidden_size": 16,
            "num_hidden_layers": layers,
            "num_attention_heads": 4,
            "num_key_value_heads": 2,
            "head_dim": 8,
            "intermediate_size": 24,
            "max_position_embeddings": MAX_POSITIONS,
            "vocab_size": VOCAB,
            "rms_norm_eps": 1e-6,
            "rope_theta": 10000.0,
            "tie_word_embeddings": true
        }))
        .unwrap();

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
                vec![VOCAB, 16],
                values(VOCAB * 16, 0.0, 2.0),
            ),
            ("model.norm.weight".into(), vec![16], values(16, 1.0, 0.2)),
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
        for layer in 0..layers {
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
        let bytes = safetensors::serialize(views, &None).unwrap();
        let tensors = safetensors::SafeTensors::deserialize(&bytes).unwrap();
        BertModel::load_from_tensors(&tensors, config, true).unwrap()
    }

    /// A prompt long enough that the CPU matmul splits it into more than one 64-row block.
    /// `variant` only shifts the token ids, so two variants are two distinct prompts.
    fn prompt(len: usize, variant: u32) -> Vec<u32> {
        (0..len as u32)
            .map(|i| (i * 7 + variant * 13 + i * i % 11) % VOCAB as u32)
            .collect()
    }

    const CANDIDATES: [u32; 4] = [3, 17, 29, 41];

    fn max_diff(a: &[f32], b: &[f32]) -> f32 {
        assert_eq!(a.len(), b.len());
        a.iter()
            .zip(b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f32, f32::max)
    }

    #[test]
    // A continuation runs the generic decoder pass over the suffix, so its exact
    // reference is `score_next_tokens_reference`; `score_next_tokens` may take a
    // faster prefill whose rounding differs in the last bits.
    fn tiny_decoder_continuation_matches_the_whole_prompt_at_every_split() {
        let model = tiny_decoder(2);
        let ids = prompt(90, 1);
        let whole = model
            .score_next_tokens_reference(&ids, &CANDIDATES)
            .unwrap();
        let (mut worst, mut identical) = (0.0f32, 0);
        for split in 0..=ids.len() {
            let prefix = model.prefill(&ids[..split]).unwrap();
            assert_eq!(prefix.len(), split);
            let got = model
                .score_continuation(&prefix, &ids[split..], &CANDIDATES)
                .unwrap();
            let diff = max_diff(&got, &whole);
            assert!(diff < 1e-4, "split at {split}: logits differ by {diff}");
            worst = worst.max(diff);
            identical += usize::from(got == whole);
        }
        eprintln!(
            "{} splits: max |logit diff| {worst:e}, bit-identical {identical}",
            ids.len() + 1
        );
    }

    #[test]
    fn tiny_decoder_prefix_serves_many_continuations_unchanged() {
        let model = tiny_decoder(2);
        let state = prompt(70, 2);
        let prefix = model.prefill(&state).unwrap();
        let question_a = prompt(12, 3);
        let question_b = prompt(15, 4);

        let first_a = model
            .score_continuation(&prefix, &question_a, &CANDIDATES)
            .unwrap();
        let first_b = model
            .score_continuation(&prefix, &question_b, &CANDIDATES)
            .unwrap();
        let again_a = model
            .score_continuation(&prefix, &question_a, &CANDIDATES)
            .unwrap();
        assert_eq!(first_a, again_a, "a continuation changed the prefix state");
        assert_ne!(first_a, first_b);
        for (question, got) in [(&question_a, &first_a), (&question_b, &first_b)] {
            let whole: Vec<u32> = state.iter().chain(question.iter()).copied().collect();
            let expected = model
                .score_next_tokens_reference(&whole, &CANDIDATES)
                .unwrap();
            assert!(max_diff(got, &expected) < 1e-4);
        }
        let own_end = model.score_continuation(&prefix, &[], &CANDIDATES).unwrap();
        assert_eq!(
            own_end,
            model
                .score_next_tokens_reference(&state, &CANDIDATES)
                .unwrap()
        );
    }

    #[test]
    fn tiny_decoder_batch_matches_one_at_a_time() {
        let model = tiny_decoder(2);
        let states: Vec<PrefixState> = [40, 75, 5]
            .iter()
            .enumerate()
            .map(|(i, &n)| model.prefill(&prompt(n, 10 + i as u32)).unwrap())
            .collect();
        let wholes: Vec<Vec<u32>> = [1, 9, 64, 65, 130, 33]
            .iter()
            .enumerate()
            .map(|(i, &n)| prompt(n, 20 + i as u32))
            .collect();
        let questions: Vec<Vec<u32>> = (0..4).map(|i| prompt(6 + 5 * i, 30 + i as u32)).collect();

        let mut requests: Vec<ScoreRequest<'_>> = wholes
            .iter()
            .map(|w| ScoreRequest::new(w, &CANDIDATES))
            .collect();
        for state in &states {
            for question in &questions {
                requests.push(ScoreRequest::continuation(state, question, &CANDIDATES));
            }
            requests.push(ScoreRequest::continuation(state, &[], &CANDIDATES[..2]));
        }
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

        for max_in_flight in [0, 1, 2, 3, 8, requests.len()] {
            for memory_budget_bytes in [DEFAULT_BATCH_MEMORY_BUDGET, 1] {
                let options = BatchOptions {
                    max_in_flight,
                    memory_budget_bytes,
                };
                let got = model
                    .score_next_tokens_batch_with(&requests, &options)
                    .unwrap();
                assert_eq!(got.len(), expected.len());
                for (i, (g, e)) in got.iter().zip(&expected).enumerate() {
                    assert!(
                        max_diff(g, e) < 1e-4,
                        "request {i} with {options:?}: batch differs from one at a time"
                    );
                }
            }
        }
        assert!(model.score_next_tokens_batch(&[]).unwrap().is_empty());
    }

    #[test]
    fn batch_in_flight_honours_the_limits() {
        let model = tiny_decoder(1);
        let ids = prompt(20, 5);
        let requests = vec![ScoreRequest::new(&ids, &CANDIDATES); 10];
        let with = |max_in_flight, memory_budget_bytes| {
            model.batch_in_flight(
                &requests,
                &BatchOptions {
                    max_in_flight,
                    memory_budget_bytes,
                },
            )
        };
        assert_eq!(with(8, usize::MAX), 8);
        assert_eq!(with(64, usize::MAX), 10, "capped by the request count");
        assert_eq!(with(8, 1), 1, "at least one request always runs");
        let one = model.request_working_bytes(&requests[0]);
        assert_eq!(with(8, 3 * one), 3, "capped by the memory budget");
        assert_eq!(
            with(0, usize::MAX),
            DEFAULT_CPU_IN_FLIGHT.min(rayon::current_num_threads())
        );
        assert_eq!(
            model.batch_in_flight(&requests[..1], &BatchOptions::default()),
            1
        );
    }

    #[test]
    fn malformed_requests_are_refused() {
        let model = tiny_decoder(2);
        let ids = prompt(10, 6);
        let outside = [VOCAB as u32];
        let too_long = prompt(MAX_POSITIONS + 1, 7);

        assert!(model.prefill(&outside).is_err());
        assert!(model.prefill(&too_long).is_err());
        let empty = model.prefill(&[]).unwrap();
        assert!(empty.is_empty());
        assert!(model.score_continuation(&empty, &[], &CANDIDATES).is_err());
        assert_eq!(
            model.score_continuation(&empty, &ids, &CANDIDATES).unwrap(),
            model
                .score_next_tokens_reference(&ids, &CANDIDATES)
                .unwrap()
        );

        let prefix = model.prefill(&ids).unwrap();
        assert!(model
            .score_continuation(&prefix, &too_long[..MAX_POSITIONS - 5], &CANDIDATES)
            .is_err());
        assert!(model
            .score_continuation(&prefix, &outside, &CANDIDATES)
            .is_err());

        let good = ScoreRequest::new(&ids, &CANDIDATES);
        for bad in [
            ScoreRequest::new(&[], &CANDIDATES),
            ScoreRequest::new(&ids, &outside),
            ScoreRequest::new(&outside, &CANDIDATES),
            ScoreRequest::new(&too_long, &CANDIDATES),
            ScoreRequest::continuation(&empty, &[], &CANDIDATES),
        ] {
            assert!(model.score_next_tokens_batch(&[good, bad]).is_err());
        }

        // A state from a model of another shape is refused rather than misread.
        let deeper = tiny_decoder(3);
        let foreign = deeper.prefill(&ids).unwrap();
        assert!(model
            .score_continuation(&foreign, &ids, &CANDIDATES)
            .is_err());
        assert!(model
            .score_next_tokens_batch(&[ScoreRequest::continuation(&foreign, &ids, &CANDIDATES)])
            .is_err());
    }
}
