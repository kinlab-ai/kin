// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Fast CPU prefill for decoder-only next-token scoring.
//!
//! `BertModel::score_next_tokens` needs one vector from a prompt: the final hidden state of
//! its last position. The generic `decoder_forward` pass reaches it through the
//! backend-neutral `GpuCompute` interface, one attention head at a time, over full-size
//! intermediate arrays. On the CPU this module runs the same mathematics with a layout built
//! for the CPU:
//!
//! - One GEMM per projection over every prompt row. Q, K and V come from the fused Q|K|V
//!   weight in one call, and the output and down projections accumulate straight onto the
//!   residual stream. Large GEMMs are split into row blocks that run concurrently.
//! - Attention per key/value group. The query heads that share a key/value head (GQA) share
//!   one Q·Kᵀ GEMM, and query rows are tiled so each tile reaches only the keys it can see,
//!   which skips most of the causal upper triangle.
//! - RMSNorm, the per-head query and key norms, RoPE, softmax and SwiGLU run as row passes
//!   with an exp the compiler vectorizes.
//! - Every buffer is allocated once per call and reused by every layer.
//! - The last layer computes keys and values for every row, but the query, attention, output
//!   projection and feed-forward block for the last row only, since nothing reads the rest.
//!
//! The GEMMs use Apple Accelerate when the `accelerate` feature selected it for the CPU backend
//! and the pure-Rust `matrixmultiply` kernel otherwise, so `KIN_INFER_CPU_BACKEND` governs this
//! path the same way it governs the generic one. The generic pass stays the numerical
//! reference, reachable as `BertModel::score_next_tokens_reference`.
//!
//! Two environment variables help profile and tune the prefill. `KIN_INFER_CPU_PREFILL_TIMINGS`
//! prints a per-stage time breakdown of every call to stderr. `KIN_INFER_CPU_GEMM_SPLIT` sets
//! how many concurrent Accelerate calls one projection is split into (default 6; 1 issues a
//! single call).

use std::cell::RefCell;
use std::marker::PhantomData;
use std::ops::Range;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ndarray::{Array1, Array2};
use rayon::prelude::*;

use crate::gpu::{cpu_backend, CpuBackend};
use crate::{BertModel, InferError};

/// Query rows per attention tile. A tile's Q·Kᵀ product covers the keys up to its last row,
/// so smaller tiles skip more of the causal upper triangle and larger tiles make larger GEMMs.
const ATTN_ROW_TILE: usize = 64;

/// Rows per concurrent block of a pure-Rust projection, as in the generic CPU matmul.
const PURE_RUST_ROW_BLOCK: usize = 64;

/// Concurrent Accelerate calls per large projection unless `KIN_INFER_CPU_GEMM_SPLIT` says
/// otherwise. One call keeps a single matrix unit busy; concurrent calls reach the others.
#[cfg(all(feature = "accelerate", target_os = "macos"))]
const DEFAULT_BLAS_SPLIT: usize = 6;

/// Fewest rows worth giving one concurrent Accelerate call.
#[cfg(all(feature = "accelerate", target_os = "macos"))]
const BLAS_MIN_BLOCK_ROWS: usize = 32;

/// Multiply-accumulates below which a projection runs as one call on the calling thread.
const SPLIT_MIN_MACS: usize = 1 << 22;

/// Elements below which a row pass runs on the calling thread.
const PARALLEL_ELEMENTS: usize = 1 << 15;

/// Largest attention scratch, in floats, that a worker thread keeps between calls (64 MiB).
const MAX_RETAINED_SCRATCH: usize = 1 << 24;

/// Accumulator lanes for the vectorized reductions.
const LANES: usize = 8;

// ---------------------------------------------------------------------------
// Layout: the model shapes and weight slices the prefill runs on
// ---------------------------------------------------------------------------

/// Weight slices of one decoder layer, checked against the model dimensions.
struct LayerView<'a> {
    /// The fused `[q_dim + 2 * kv_dim, hidden]` Q|K|V weight when the loader built one.
    qkv: Option<&'a [f32]>,
    q: &'a [f32],
    k: &'a [f32],
    v: &'a [f32],
    q_bias: Option<&'a [f32]>,
    k_bias: Option<&'a [f32]>,
    v_bias: Option<&'a [f32]>,
    /// Per-head RMSNorm weights applied to every query and key head before RoPE (Qwen3).
    q_norm: Option<&'a [f32]>,
    k_norm: Option<&'a [f32]>,
    out: &'a [f32],
    out_bias: Option<&'a [f32]>,
    norm1: &'a [f32],
    norm2: &'a [f32],
    gate: &'a [f32],
    up: &'a [f32],
    down: &'a [f32],
    down_bias: Option<&'a [f32]>,
}

/// Model dimensions and weights, validated once per call.
struct Layout<'a> {
    hidden: usize,
    heads: usize,
    kv_heads: usize,
    head_dim: usize,
    inter: usize,
    eps: f32,
    vocab: usize,
    embeddings: &'a [f32],
    /// RoPE tables, `[max_positions, head_dim]`; the first `head_dim / 2` columns are read.
    rope_cos: &'a [f32],
    rope_sin: &'a [f32],
    max_positions: usize,
    final_norm: Option<&'a [f32]>,
    layers: Vec<LayerView<'a>>,
}

impl Layout<'_> {
    fn q_dim(&self) -> usize {
        self.heads * self.head_dim
    }

    fn kv_dim(&self) -> usize {
        self.kv_heads * self.head_dim
    }

    /// Width of one row of the Q|K|V buffer.
    fn qkv_cols(&self) -> usize {
        self.q_dim() + 2 * self.kv_dim()
    }

    /// Query heads per key/value head.
    fn group(&self) -> usize {
        self.heads / self.kv_heads
    }
}

/// The contiguous data of a `[rows, cols]` matrix, if it has that shape and standard layout.
fn matrix(w: &Array2<f32>, rows: usize, cols: usize) -> Option<&[f32]> {
    if w.dim() == (rows, cols) {
        w.as_slice()
    } else {
        None
    }
}

/// The contiguous data of a vector, if it has length `len`.
fn vector(v: &Array1<f32>, len: usize) -> Option<&[f32]> {
    if v.len() == len {
        v.as_slice()
    } else {
        None
    }
}

/// `Some(None)` for an absent vector, `Some(Some(data))` for a present one of length `len`,
/// and `None` for a present one of any other shape.
fn optional_vector(v: Option<&Array1<f32>>, len: usize) -> Option<Option<&[f32]>> {
    match v {
        None => Some(None),
        Some(v) => vector(v, len).map(Some),
    }
}

impl BertModel {
    /// The prefill's view of this model, or `None` when its layout is outside what the
    /// prefill implements. The prefill covers exactly what `decoder_forward` computes for a
    /// pre-norm RMSNorm decoder with RoPE, (grouped-query) attention, optional Q/K/V, output
    /// and down-projection biases, optional per-head query and key norms, and a SwiGLU
    /// feed-forward block.
    fn cpu_prefill_layout(&self) -> Option<Layout<'_>> {
        let config = &self.config;
        if !config.architecture().is_decoder_only() || !config.uses_rmsnorm() {
            return None;
        }
        let weights = &self.weights;
        if weights.position_embeddings.is_some() {
            return None;
        }
        let hidden = config.hidden_size;
        let heads = config.num_attention_heads;
        let kv_heads = config.effective_num_kv_heads();
        let head_dim = self.head_dim;
        if hidden == 0
            || heads == 0
            || kv_heads == 0
            || heads % kv_heads != 0
            || head_dim == 0
            || head_dim % 2 != 0
        {
            return None;
        }
        let q_dim = heads * head_dim;
        let kv_dim = kv_heads * head_dim;
        let vocab = weights.word_embeddings.nrows();
        let embeddings = matrix(&weights.word_embeddings, vocab, hidden)?;
        let cos = self.rope_cos.as_ref()?;
        let sin = self.rope_sin.as_ref()?;
        let rope_cos = matrix(cos, cos.nrows(), head_dim)?;
        let rope_sin = matrix(sin, sin.nrows(), head_dim)?;
        let max_positions = cos.nrows().min(sin.nrows());
        let final_norm = optional_vector(weights.final_norm_weight.as_ref(), hidden)?;
        let inter = match weights.layers.first() {
            Some(layer) => layer.ffn_gate_weight.as_ref()?.nrows(),
            None => 0,
        };

        let layers = weights
            .layers
            .iter()
            .map(|layer| {
                // Whole-row LayerNorm on Q or K belongs to encoder families; decline it.
                if layer.q_ln_weight.is_some()
                    || layer.q_ln_bias.is_some()
                    || layer.k_ln_weight.is_some()
                    || layer.k_ln_bias.is_some()
                {
                    return None;
                }
                let qkv = match layer.qkv_weight.as_ref() {
                    Some(fused) => Some(matrix(fused, q_dim + 2 * kv_dim, hidden)?),
                    None => None,
                };
                Some(LayerView {
                    qkv,
                    q: matrix(&layer.q_weight, q_dim, hidden)?,
                    k: matrix(&layer.k_weight, kv_dim, hidden)?,
                    v: matrix(&layer.v_weight, kv_dim, hidden)?,
                    q_bias: optional_vector(layer.q_bias.as_ref(), q_dim)?,
                    k_bias: optional_vector(layer.k_bias.as_ref(), kv_dim)?,
                    v_bias: optional_vector(layer.v_bias.as_ref(), kv_dim)?,
                    q_norm: optional_vector(layer.q_head_norm.as_ref(), head_dim)?,
                    k_norm: optional_vector(layer.k_head_norm.as_ref(), head_dim)?,
                    out: matrix(&layer.attn_out_weight, hidden, q_dim)?,
                    out_bias: optional_vector(layer.attn_out_bias.as_ref(), hidden)?,
                    norm1: vector(&layer.norm1_weight, hidden)?,
                    norm2: vector(&layer.norm2_weight, hidden)?,
                    gate: matrix(layer.ffn_gate_weight.as_ref()?, inter, hidden)?,
                    up: matrix(layer.ffn_up_weight.as_ref()?, inter, hidden)?,
                    down: matrix(&layer.ffn_down_weight, hidden, inter)?,
                    down_bias: optional_vector(layer.ffn_down_bias.as_ref(), hidden)?,
                })
            })
            .collect::<Option<Vec<_>>>()?;

        Some(Layout {
            hidden,
            heads,
            kv_heads,
            head_dim,
            inter,
            eps: config.effective_rms_eps(),
            vocab,
            embeddings,
            rope_cos,
            rope_sin,
            max_positions,
            final_norm,
            layers,
        })
    }

    /// The final-norm hidden state of the last prompt position, computed by the CPU prefill.
    ///
    /// Returns `Ok(None)` when the model's layout or the prompt length is outside what the
    /// prefill implements (a prompt longer than the RoPE table, for one); the caller then runs
    /// the generic decoder pass, which is also the numerical reference for this one.
    pub(crate) fn cpu_prefill_last_hidden(
        &self,
        token_ids: &[u32],
    ) -> Result<Option<Vec<f32>>, InferError> {
        self.cpu_prefill_tail_hidden(token_ids, 1)
    }

    /// The final-norm hidden states of the last `tail` prompt positions, row after row, as
    /// `tail * hidden` floats. `tail` must be between 1 and the prompt length.
    ///
    /// Declines like [`cpu_prefill_last_hidden`](Self::cpu_prefill_last_hidden). The last
    /// layer runs its query side for the `tail` rows only.
    pub(crate) fn cpu_prefill_tail_hidden(
        &self,
        token_ids: &[u32],
        tail: usize,
    ) -> Result<Option<Vec<f32>>, InferError> {
        let Some(layout) = self.cpu_prefill_layout() else {
            return Ok(None);
        };
        if token_ids.is_empty() || token_ids.len() > layout.max_positions {
            return Ok(None);
        }
        if tail == 0 || tail > token_ids.len() {
            return Err(InferError::Internal(format!(
                "cpu prefill: {tail} tail rows asked of a {}-token prompt",
                token_ids.len()
            )));
        }
        let _span = tracing::info_span!(
            "kin_infer.model.cpu_prefill",
            seq_len = token_ids.len(),
            tail = tail,
            layers = layout.layers.len()
        )
        .entered();
        prefill(&layout, token_ids, tail).map(Some)
    }
}

// ---------------------------------------------------------------------------
// The prefill
// ---------------------------------------------------------------------------

fn prefill(l: &Layout<'_>, token_ids: &[u32], tail: usize) -> Result<Vec<f32>, InferError> {
    let n = token_ids.len();
    let h = l.hidden;
    let q_dim = l.q_dim();
    let inter = l.inter;
    let mut clock = StageClock::start();

    let mut hidden = vec![0.0f32; n * h];
    for (row, &id) in hidden.chunks_exact_mut(h).zip(token_ids) {
        let id = id as usize;
        if id >= l.vocab {
            return Err(InferError::Internal(format!(
                "token {id} outside the {}-token vocabulary",
                l.vocab
            )));
        }
        row.copy_from_slice(&l.embeddings[id * h..(id + 1) * h]);
    }
    let mut normed = vec![0.0f32; n * h];
    let mut qkv = vec![0.0f32; n * l.qkv_cols()];
    let mut attn = vec![0.0f32; n * q_dim];
    let mut gate = vec![0.0f32; n * inter];
    let mut up = vec![0.0f32; n * inter];
    clock.lap(Stage::Setup);

    for (li, layer) in l.layers.iter().enumerate() {
        // Nothing reads the last layer's output for any row but the last `tail`. Its keys and
        // values still come from every row, so only the query side narrows.
        let first = if li + 1 == l.layers.len() {
            n - tail
        } else {
            0
        };
        let rows = n - first;

        rms_norm_rows(&hidden, &mut normed, h, 0..n, layer.norm1, l.eps);
        clock.lap(Stage::Norm);

        project_qkv(l, layer, &normed, n, first, &mut qkv);
        clock.lap(Stage::Qkv);

        prepare_qk(l, layer, &mut qkv, n, first);
        clock.lap(Stage::QkPrep);

        attention(l, &qkv, first..n, &mut attn);
        clock.lap(Stage::Attention);

        // The output projection accumulates onto the residual stream.
        let residual = &mut hidden[first * h..];
        project(
            &attn[first * q_dim..],
            q_dim,
            rows,
            layer.out,
            h,
            1.0,
            residual,
            h,
        );
        add_bias_rows(residual, h, layer.out_bias);
        clock.lap(Stage::OutProj);

        rms_norm_rows(&hidden, &mut normed, h, first..n, layer.norm2, l.eps);
        clock.lap(Stage::Norm);

        let x = &normed[first * h..];
        project(
            x,
            h,
            rows,
            layer.gate,
            inter,
            0.0,
            &mut gate[first * inter..],
            inter,
        );
        project(
            x,
            h,
            rows,
            layer.up,
            inter,
            0.0,
            &mut up[first * inter..],
            inter,
        );
        clock.lap(Stage::FfnIn);

        swiglu(&mut gate[first * inter..], &up[first * inter..]);
        clock.lap(Stage::Swiglu);

        let residual = &mut hidden[first * h..];
        project(
            &gate[first * inter..],
            inter,
            rows,
            layer.down,
            h,
            1.0,
            residual,
            h,
        );
        add_bias_rows(residual, h, layer.down_bias);
        clock.lap(Stage::FfnDown);
    }

    let last = &hidden[(n - tail) * h..];
    let out = match l.final_norm {
        Some(weight) => {
            let mut out = vec![0.0f32; tail * h];
            for (row, dst) in last.chunks_exact(h).zip(out.chunks_exact_mut(h)) {
                rms_norm_row(row, weight, l.eps, dst);
            }
            out
        }
        None => last.to_vec(),
    };
    clock.lap(Stage::Final);
    clock.report(n, l.layers.len());
    Ok(out)
}

/// Project the normed rows onto Q|K|V, one row of `qkv` per position. In the last layer
/// (`first > 0`) only the query rows from `first` on are projected; keys and values always
/// cover every row.
fn project_qkv(
    l: &Layout<'_>,
    layer: &LayerView<'_>,
    normed: &[f32],
    n: usize,
    first: usize,
    qkv: &mut [f32],
) {
    let h = l.hidden;
    let q_dim = l.q_dim();
    let kv_dim = l.kv_dim();
    let cols = l.qkv_cols();
    if first == 0 {
        if let Some(fused) = layer.qkv {
            project(normed, h, n, fused, cols, 0.0, qkv, cols);
            return;
        }
        project(normed, h, n, layer.q, q_dim, 0.0, qkv, cols);
    } else {
        let queries = &normed[first * h..];
        project(
            queries,
            h,
            n - first,
            layer.q,
            q_dim,
            0.0,
            &mut qkv[first * cols..],
            cols,
        );
    }
    match layer.qkv {
        // K and V rows follow Q in the fused weight, so they project in one call.
        Some(fused) => project(
            normed,
            h,
            n,
            &fused[q_dim * h..],
            2 * kv_dim,
            0.0,
            &mut qkv[q_dim..],
            cols,
        ),
        None => {
            project(normed, h, n, layer.k, kv_dim, 0.0, &mut qkv[q_dim..], cols);
            project(
                normed,
                h,
                n,
                layer.v,
                kv_dim,
                0.0,
                &mut qkv[q_dim + kv_dim..],
                cols,
            );
        }
    }
}

/// Add the Q, K and V biases, then apply the per-head norms and RoPE, in place: to the query
/// heads of rows `first..n` and to the key heads of every row.
fn prepare_qk(l: &Layout<'_>, layer: &LayerView<'_>, qkv: &mut [f32], n: usize, first: usize) {
    let cols = l.qkv_cols();
    let q_dim = l.q_dim();
    let kv_dim = l.kv_dim();
    let head_dim = l.head_dim;
    let half = head_dim / 2;
    let prepare_row = |pos: usize, row: &mut [f32]| {
        let (q, kv) = row.split_at_mut(q_dim);
        let (k, v) = kv.split_at_mut(kv_dim);
        let cos = &l.rope_cos[pos * head_dim..pos * head_dim + half];
        let sin = &l.rope_sin[pos * head_dim..pos * head_dim + half];
        if pos >= first {
            add_bias(q, layer.q_bias);
            for head in q.chunks_exact_mut(head_dim) {
                norm_and_rotate(head, layer.q_norm, l.eps, cos, sin);
            }
        }
        add_bias(k, layer.k_bias);
        for head in k.chunks_exact_mut(head_dim) {
            norm_and_rotate(head, layer.k_norm, l.eps, cos, sin);
        }
        add_bias(v, layer.v_bias);
    };
    let rows = &mut qkv[..n * cols];
    if rows.len() >= PARALLEL_ELEMENTS {
        rows.par_chunks_mut(cols)
            .enumerate()
            .with_min_len(4)
            .for_each(|(pos, row)| prepare_row(pos, row));
    } else {
        for (pos, row) in rows.chunks_mut(cols).enumerate() {
            prepare_row(pos, row);
        }
    }
}

/// RMSNorm one head (when it has a norm weight), then rotate it by RoPE at the position whose
/// `cos` and `sin` rows are given (`head_dim / 2` entries each).
fn norm_and_rotate(head: &mut [f32], norm: Option<&[f32]>, eps: f32, cos: &[f32], sin: &[f32]) {
    if let Some(weight) = norm {
        let inv = 1.0 / (sum_of_squares(head) / head.len() as f32 + eps).sqrt();
        for (x, &w) in head.iter_mut().zip(weight) {
            *x = *x * inv * w;
        }
    }
    let (lo, hi) = head.split_at_mut(head.len() / 2);
    for (((x0, x1), &c), &s) in lo.iter_mut().zip(hi.iter_mut()).zip(cos).zip(sin) {
        let (a, b) = (*x0, *x1);
        *x0 = a * c - b * s;
        *x1 = b * c + a * s;
    }
}

/// Causal self-attention for the query rows in `rows`, reading Q, K and V from `qkv` and
/// writing each position's concatenated head outputs to its row of `attn`.
///
/// The work is split into tiles of up to `ATTN_ROW_TILE` query rows per key/value group. A
/// tile stacks the group's query heads into one matrix, so one GEMM scores all of them against
/// the keys up to the tile's last row, and a second GEMM applies the softmax weights to the
/// values.
fn attention(l: &Layout<'_>, qkv: &[f32], rows: Range<usize>, attn: &mut [f32]) {
    let head_dim = l.head_dim;
    let group = l.group();
    let q_dim = l.q_dim();
    let kv_dim = l.kv_dim();
    let cols = l.qkv_cols();
    let scale = 1.0 / (head_dim as f32).sqrt();
    // One position's outputs for a group: its query heads side by side.
    let width = group * head_dim;

    let mut tiles = Vec::new();
    let mut start = rows.start;
    while start < rows.end {
        let end = (start + ATTN_ROW_TILE).min(rows.end);
        tiles.extend((0..l.kv_heads).map(|g| (g, start, end)));
        start = end;
    }
    // The latest rows see the most keys; start them first so the tail of the pool stays busy.
    tiles.reverse();

    let out = TileWriter::new(attn);
    tiles.par_iter().for_each(|&(g, start, end)| {
        // Row `t` of a tile is query head `g * group + t % group` at position `start + t / group`.
        let m = (end - start) * group;
        let keys = end;
        with_scratch(2 * m * head_dim + m * keys, |scratch| {
            let (queries, rest) = scratch.split_at_mut(m * head_dim);
            let (scores, values) = rest.split_at_mut(m * keys);
            for (dst, pos) in queries.chunks_exact_mut(width).zip(start..end) {
                dst.copy_from_slice(&qkv[pos * cols + g * width..][..width]);
            }
            let k = &qkv[q_dim + g * head_dim..];
            let v = &qkv[q_dim + kv_dim + g * head_dim..];
            sgemm(
                m,
                keys,
                head_dim,
                scale,
                queries,
                head_dim,
                k,
                cols,
                Op::T,
                0.0,
                scores,
                keys,
            );
            for (t, row) in scores.chunks_exact_mut(keys).enumerate() {
                softmax_causal(row, start + t / group + 1);
            }
            sgemm(
                m,
                head_dim,
                keys,
                1.0,
                scores,
                keys,
                v,
                cols,
                Op::N,
                0.0,
                values,
                head_dim,
            );
            for (src, pos) in values.chunks_exact(width).zip(start..end) {
                // SAFETY: tiles are distinct (group, row range) pairs, and this tile is the only
                // one that touches columns `g * width..(g + 1) * width` of rows `start..end`.
                unsafe { out.write(pos * q_dim + g * width, src) };
            }
        });
    });
}

/// Softmax of one causal score row: normalize `row[..visible]` and zero the masked tail, as the
/// generic pass's `-inf` mask followed by a softmax does.
fn softmax_causal(row: &mut [f32], visible: usize) {
    let (live, masked) = row.split_at_mut(visible);
    masked.fill(0.0);
    let max = max_of(live);
    if !max.is_finite() {
        live.fill(0.0);
        return;
    }
    for v in live.iter_mut() {
        *v = exp_fast(*v - max);
    }
    let sum = sum_of(live);
    if sum > 0.0 {
        for v in live.iter_mut() {
            *v /= sum;
        }
    }
}

/// Shared handle on the attention output for tiles that write disjoint regions of it.
struct TileWriter<'a> {
    ptr: *mut f32,
    len: usize,
    _buf: PhantomData<&'a mut [f32]>,
}

// SAFETY: the handle only ever writes, through `write`, regions that its caller guarantees no
// other thread touches, and it borrows the buffer mutably for its whole life.
unsafe impl Send for TileWriter<'_> {}
unsafe impl Sync for TileWriter<'_> {}

impl<'a> TileWriter<'a> {
    fn new(buf: &'a mut [f32]) -> Self {
        Self {
            ptr: buf.as_mut_ptr(),
            len: buf.len(),
            _buf: PhantomData,
        }
    }

    /// Copy `src` into the buffer at `offset`.
    ///
    /// # Safety
    ///
    /// No other thread may read or write `offset..offset + src.len()` while this handle lives.
    unsafe fn write(&self, offset: usize, src: &[f32]) {
        assert!(
            offset <= self.len && src.len() <= self.len - offset,
            "attention tile write out of bounds"
        );
        // SAFETY: the range is in bounds (checked above) and exclusive to the caller.
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), self.ptr.add(offset), src.len()) };
    }
}

thread_local! {
    static SCRATCH: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
}

/// Run `f` on `len` floats of this thread's attention scratch, which persists between calls up
/// to `MAX_RETAINED_SCRATCH` floats so steady-state scoring allocates nothing per tile.
fn with_scratch<R>(len: usize, f: impl FnOnce(&mut [f32]) -> R) -> R {
    SCRATCH.with(|cell| match cell.try_borrow_mut() {
        Ok(mut buf) if len <= MAX_RETAINED_SCRATCH => {
            if buf.len() < len {
                buf.resize(len, 0.0);
            }
            f(&mut buf[..len])
        }
        _ => f(&mut vec![0.0f32; len]),
    })
}

// ---------------------------------------------------------------------------
// Row passes
// ---------------------------------------------------------------------------

/// RMSNorm rows `rows` of `src` into the same rows of `dst`, both `cols` wide.
fn rms_norm_rows(
    src: &[f32],
    dst: &mut [f32],
    cols: usize,
    rows: Range<usize>,
    weight: &[f32],
    eps: f32,
) {
    let src = &src[rows.start * cols..rows.end * cols];
    let dst = &mut dst[rows.start * cols..rows.end * cols];
    if src.len() >= PARALLEL_ELEMENTS {
        src.par_chunks(cols)
            .zip(dst.par_chunks_mut(cols))
            .with_min_len(4)
            .for_each(|(x, out)| rms_norm_row(x, weight, eps, out));
    } else {
        for (x, out) in src.chunks(cols).zip(dst.chunks_mut(cols)) {
            rms_norm_row(x, weight, eps, out);
        }
    }
}

/// `out = x / sqrt(mean(x²) + eps) * weight`, in the generic pass's order of operations.
fn rms_norm_row(x: &[f32], weight: &[f32], eps: f32, out: &mut [f32]) {
    let inv = 1.0 / (sum_of_squares(x) / x.len() as f32 + eps).sqrt();
    for ((o, &v), &w) in out.iter_mut().zip(x).zip(weight) {
        *o = v * inv * w;
    }
}

/// `gate = silu(gate) * up`, elementwise.
fn swiglu(gate: &mut [f32], up: &[f32]) {
    const CHUNK: usize = 1 << 14;
    if gate.len() >= PARALLEL_ELEMENTS {
        gate.par_chunks_mut(CHUNK)
            .zip(up.par_chunks(CHUNK))
            .for_each(|(g, u)| swiglu_chunk(g, u));
    } else {
        swiglu_chunk(gate, up);
    }
}

fn swiglu_chunk(gate: &mut [f32], up: &[f32]) {
    for (g, &u) in gate.iter_mut().zip(up) {
        *g = *g / (1.0 + exp_fast(-*g)) * u;
    }
}

fn add_bias(x: &mut [f32], bias: Option<&[f32]>) {
    if let Some(bias) = bias {
        for (v, &b) in x.iter_mut().zip(bias) {
            *v += b;
        }
    }
}

fn add_bias_rows(rows: &mut [f32], cols: usize, bias: Option<&[f32]>) {
    if bias.is_some() {
        for row in rows.chunks_exact_mut(cols) {
            add_bias(row, bias);
        }
    }
}

// ---------------------------------------------------------------------------
// Vectorizable scalar kernels
// ---------------------------------------------------------------------------

/// `e^x` in straight-line arithmetic the compiler vectorizes: the Cephes `expf` range reduction
/// and polynomial, within a few ulp of `f32::exp`. Inputs are clamped to `[-87.34, 88.0]`, so
/// very negative inputs give the smallest normal float instead of a subnormal or zero, and
/// large ones saturate near 1.7e38. Softmax (inputs at most 0) and SiLU depend on neither tail.
#[inline(always)]
#[allow(clippy::excessive_precision)]
fn exp_fast(x: f32) -> f32 {
    const LOG2E: f32 = std::f32::consts::LOG2_E;
    // ln 2 split into a part exact in a few bits and the remainder (Cody-Waite).
    const LN2_HI: f32 = 0.693_359_375;
    const LN2_LO: f32 = -2.121_944_4e-4;
    let x = x.clamp(-87.336_55, 88.0);
    let n = (x * LOG2E + 0.5).floor();
    let r = x - n * LN2_HI - n * LN2_LO;
    let mut p = 1.987_569_1e-4_f32;
    p = p * r + 1.398_199_9e-3;
    p = p * r + 8.333_452e-3;
    p = p * r + 4.166_579_6e-2;
    p = p * r + 1.666_666_5e-1;
    p = p * r + 5.000_000_1e-1;
    let y = p * (r * r) + r + 1.0;
    // 2^n for n in [-126, 127], built directly in the exponent field.
    let pow2 = f32::from_bits(((n as i32 + 127) as u32) << 23);
    y * pow2
}

fn lanes_sum(acc: [f32; LANES]) -> f32 {
    ((acc[0] + acc[4]) + (acc[1] + acc[5])) + ((acc[2] + acc[6]) + (acc[3] + acc[7]))
}

fn sum_of(x: &[f32]) -> f32 {
    let mut acc = [0.0f32; LANES];
    let chunks = x.chunks_exact(LANES);
    let tail = chunks.remainder();
    for c in chunks {
        for (a, &v) in acc.iter_mut().zip(c) {
            *a += v;
        }
    }
    tail.iter().fold(lanes_sum(acc), |s, &v| s + v)
}

fn sum_of_squares(x: &[f32]) -> f32 {
    let mut acc = [0.0f32; LANES];
    let chunks = x.chunks_exact(LANES);
    let tail = chunks.remainder();
    for c in chunks {
        for (a, &v) in acc.iter_mut().zip(c) {
            *a += v * v;
        }
    }
    tail.iter().fold(lanes_sum(acc), |s, &v| s + v * v)
}

fn max_of(x: &[f32]) -> f32 {
    let mut acc = [f32::NEG_INFINITY; LANES];
    let chunks = x.chunks_exact(LANES);
    let tail = chunks.remainder();
    for c in chunks {
        for (a, &v) in acc.iter_mut().zip(c) {
            *a = a.max(v);
        }
    }
    acc.iter()
        .chain(tail)
        .fold(f32::NEG_INFINITY, |m, &v| m.max(v))
}

// ---------------------------------------------------------------------------
// GEMM
// ---------------------------------------------------------------------------

/// How the second GEMM operand is read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    /// `b` is stored `[k, n]` and used as is.
    N,
    /// `b` is stored `[n, k]` and used transposed.
    T,
}

/// Whether a row-major `[rows, cols]` matrix with leading dimension `ld` fits in `len` floats.
fn covers(len: usize, rows: usize, cols: usize, ld: usize) -> bool {
    ld >= cols.max(1) && (rows == 0 || cols == 0 || (rows - 1) * ld + cols <= len)
}

/// `c = alpha * a * op(b) + beta * c` over row-major matrices with explicit leading dimensions:
/// `a` is `[m, k]`, `op(b)` is `[k, n]` and `c` is `[m, n]`. Runs on the calling thread, apart
/// from any threads Accelerate uses inside one call.
#[allow(clippy::too_many_arguments)]
fn sgemm(
    m: usize,
    n: usize,
    k: usize,
    alpha: f32,
    a: &[f32],
    lda: usize,
    b: &[f32],
    ldb: usize,
    op: Op,
    beta: f32,
    c: &mut [f32],
    ldc: usize,
) {
    if m == 0 || n == 0 {
        return;
    }
    let (b_rows, b_cols) = match op {
        Op::T => (n, k),
        Op::N => (k, n),
    };
    assert!(
        covers(a.len(), m, k, lda)
            && covers(b.len(), b_rows, b_cols, ldb)
            && covers(c.len(), m, n, ldc),
        "sgemm operand smaller than its shape"
    );
    match cpu_backend() {
        #[cfg(all(feature = "accelerate", target_os = "macos"))]
        CpuBackend::Accelerate => blas_sgemm(m, n, k, alpha, a, lda, b, ldb, op, beta, c, ldc),
        CpuBackend::PureRust => ndarray_sgemm(m, n, k, alpha, a, lda, b, ldb, op, beta, c, ldc),
    }
}

/// Accelerate `cblas_sgemm`, linked from the system framework through raw FFI.
#[cfg(all(feature = "accelerate", target_os = "macos"))]
#[allow(clippy::too_many_arguments)]
fn blas_sgemm(
    m: usize,
    n: usize,
    k: usize,
    alpha: f32,
    a: &[f32],
    lda: usize,
    b: &[f32],
    ldb: usize,
    op: Op,
    beta: f32,
    c: &mut [f32],
    ldc: usize,
) {
    #[link(name = "Accelerate", kind = "framework")]
    extern "C" {
        fn cblas_sgemm(
            order: i32,
            transa: i32,
            transb: i32,
            m: i32,
            n: i32,
            k: i32,
            alpha: f32,
            a: *const f32,
            lda: i32,
            b: *const f32,
            ldb: i32,
            beta: f32,
            c: *mut f32,
            ldc: i32,
        );
    }
    const ROW_MAJOR: i32 = 101;
    const NO_TRANS: i32 = 111;
    const TRANS: i32 = 112;
    let dim = |x: usize| i32::try_from(x).expect("GEMM dimension exceeds i32");
    let transb = match op {
        Op::N => NO_TRANS,
        Op::T => TRANS,
    };
    // SAFETY: `sgemm` checked that every operand covers its shape at its leading dimension, and
    // `c` is borrowed mutably, so cblas reads and writes only memory this call owns.
    unsafe {
        cblas_sgemm(
            ROW_MAJOR,
            NO_TRANS,
            transb,
            dim(m),
            dim(n),
            dim(k),
            alpha,
            a.as_ptr(),
            dim(lda),
            b.as_ptr(),
            dim(ldb),
            beta,
            c.as_mut_ptr(),
            dim(ldc),
        );
    }
}

/// The pure-Rust GEMM: ndarray's `general_mat_mul` over strided views, which runs the
/// cache-blocked `matrixmultiply` kernel on the calling thread.
#[allow(clippy::too_many_arguments)]
fn ndarray_sgemm(
    m: usize,
    n: usize,
    k: usize,
    alpha: f32,
    a: &[f32],
    lda: usize,
    b: &[f32],
    ldb: usize,
    op: Op,
    beta: f32,
    c: &mut [f32],
    ldc: usize,
) {
    use ndarray::linalg::general_mat_mul;
    use ndarray::{ArrayView2, ArrayViewMut2, ShapeBuilder};
    let a = ArrayView2::from_shape((m, k).strides((lda, 1)), a).expect("sgemm a view");
    let mut c = ArrayViewMut2::from_shape((m, n).strides((ldc, 1)), c).expect("sgemm c view");
    match op {
        Op::T => {
            let b = ArrayView2::from_shape((n, k).strides((ldb, 1)), b).expect("sgemm b view");
            general_mat_mul(alpha, &a, &b.t(), beta, &mut c);
        }
        Op::N => {
            let b = ArrayView2::from_shape((k, n).strides((ldb, 1)), b).expect("sgemm b view");
            general_mat_mul(alpha, &a, &b, beta, &mut c);
        }
    }
}

/// `c = x * wᵀ + beta * c` for `rows` rows of `x` (packed `k` wide) and a row-major weight
/// `w[out, k]`, writing `c` rows at stride `ldc`.
///
/// A large product is split into row blocks that run concurrently. Every output row comes from
/// exactly one block, and the block shapes depend only on the problem size, so the result does
/// not depend on thread scheduling.
#[allow(clippy::too_many_arguments)]
fn project(
    x: &[f32],
    k: usize,
    rows: usize,
    w: &[f32],
    out: usize,
    beta: f32,
    c: &mut [f32],
    ldc: usize,
) {
    if rows == 0 || out == 0 {
        return;
    }
    let c = &mut c[..(rows - 1) * ldc + out];
    let block = projection_block_rows(rows, out, k);
    if block >= rows {
        sgemm(rows, out, k, 1.0, x, k, w, k, Op::T, beta, c, ldc);
        return;
    }
    c.par_chunks_mut(block * ldc)
        .enumerate()
        .for_each(|(i, c_block)| {
            let start = i * block;
            let m = block.min(rows - start);
            sgemm(
                m,
                out,
                k,
                1.0,
                &x[start * k..],
                k,
                w,
                k,
                Op::T,
                beta,
                c_block,
                ldc,
            );
        });
}

/// Rows per concurrent block of a projection; `rows` itself means a single call.
fn projection_block_rows(rows: usize, out: usize, k: usize) -> usize {
    if rows.saturating_mul(out).saturating_mul(k) < SPLIT_MIN_MACS
        || rayon::current_num_threads() < 2
    {
        return rows;
    }
    match cpu_backend() {
        #[cfg(all(feature = "accelerate", target_os = "macos"))]
        CpuBackend::Accelerate => {
            let parts = blas_split().min(rows.div_ceil(BLAS_MIN_BLOCK_ROWS)).max(1);
            rows.div_ceil(parts).next_multiple_of(16)
        }
        CpuBackend::PureRust => PURE_RUST_ROW_BLOCK,
    }
}

/// Concurrent Accelerate calls per large projection: `KIN_INFER_CPU_GEMM_SPLIT`, else
/// `DEFAULT_BLAS_SPLIT`, capped by the Rayon pool.
#[cfg(all(feature = "accelerate", target_os = "macos"))]
fn blas_split() -> usize {
    static SPLIT: OnceLock<usize> = OnceLock::new();
    *SPLIT.get_or_init(|| {
        std::env::var("KIN_INFER_CPU_GEMM_SPLIT")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&s| s >= 1)
            .unwrap_or(DEFAULT_BLAS_SPLIT)
            .min(rayon::current_num_threads().max(1))
    })
}

// ---------------------------------------------------------------------------
// Stage timings
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Stage {
    Setup,
    Norm,
    Qkv,
    QkPrep,
    Attention,
    OutProj,
    FfnIn,
    Swiglu,
    FfnDown,
    Final,
}

const STAGE_NAMES: [&str; 10] = [
    "setup",
    "rmsnorm",
    "qkv",
    "qk-norm+rope",
    "attention",
    "out-proj",
    "gate+up",
    "swiglu",
    "down",
    "final",
];

fn timings_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("KIN_INFER_CPU_PREFILL_TIMINGS").is_some_and(|v| !v.is_empty() && v != "0")
    })
}

/// Per-stage wall time of one prefill, kept only when `KIN_INFER_CPU_PREFILL_TIMINGS` is set.
struct StageClock {
    start: Option<Instant>,
    last: Option<Instant>,
    spent: [Duration; STAGE_NAMES.len()],
}

impl StageClock {
    fn start() -> Self {
        let now = timings_enabled().then(Instant::now);
        Self {
            start: now,
            last: now,
            spent: [Duration::ZERO; STAGE_NAMES.len()],
        }
    }

    fn lap(&mut self, stage: Stage) {
        if let Some(last) = self.last {
            let now = Instant::now();
            self.spent[stage as usize] += now - last;
            self.last = Some(now);
        }
    }

    fn report(&self, tokens: usize, layers: usize) {
        let Some(start) = self.start else {
            return;
        };
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let stages = STAGE_NAMES
            .iter()
            .zip(&self.spent)
            .map(|(name, &d)| format!("{name} {:.1}", ms(d)))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "kin-infer cpu prefill: {tokens} tokens, {layers} layers, {:.1} ms ({stages} ms)",
            ms(start.elapsed())
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BertConfig;
    use safetensors::tensor::{serialize, TensorView};
    use safetensors::{Dtype, SafeTensors};

    /// SplitMix64, for reproducible synthetic weights.
    struct Rng(u64);

    impl Rng {
        fn next_f32(&mut self) -> f32 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            // Uniform in [-1, 1).
            (z >> 40) as f32 / (1u64 << 23) as f32 - 1.0
        }

        fn fill(&mut self, len: usize, scale: f32) -> Vec<f32> {
            (0..len).map(|_| self.next_f32() * scale).collect()
        }

        fn around_one(&mut self, len: usize) -> Vec<f32> {
            (0..len).map(|_| 1.0 + 0.2 * self.next_f32()).collect()
        }
    }

    /// Shape of a synthetic decoder.
    struct Spec {
        layers: usize,
        hidden: usize,
        heads: usize,
        kv_heads: usize,
        head_dim: usize,
        inter: usize,
        vocab: usize,
        max_positions: usize,
        head_norms: bool,
        qkv_bias: bool,
    }

    fn qwen3_like() -> Spec {
        Spec {
            layers: 3,
            hidden: 64,
            heads: 4,
            kv_heads: 2,
            head_dim: 32,
            inter: 160,
            vocab: 97,
            max_positions: 256,
            head_norms: true,
            qkv_bias: false,
        }
    }

    /// Build a decoder with random weights through the real safetensors loader, on the CPU
    /// backend, so the model carries the same fused and per-projection weights a real
    /// checkpoint does.
    fn synthetic_model(spec: &Spec, seed: u64) -> BertModel {
        let mut rng = Rng(seed);
        let (h, hd, inter) = (spec.hidden, spec.head_dim, spec.inter);
        let q_dim = spec.heads * hd;
        let kv_dim = spec.kv_heads * hd;
        let mut tensors: Vec<(String, Vec<usize>, Vec<f32>)> = vec![(
            "model.embed_tokens.weight".into(),
            vec![spec.vocab, h],
            rng.fill(spec.vocab * h, 1.0),
        )];
        let proj = |fan_in: usize| 1.0 / (fan_in as f32).sqrt();
        for i in 0..spec.layers {
            let p = format!("model.layers.{i}");
            let mut add = |name: &str, shape: Vec<usize>, data: Vec<f32>| {
                tensors.push((format!("{p}.{name}"), shape, data));
            };
            add("input_layernorm.weight", vec![h], rng.around_one(h));
            add(
                "self_attn.q_proj.weight",
                vec![q_dim, h],
                rng.fill(q_dim * h, proj(h)),
            );
            add(
                "self_attn.k_proj.weight",
                vec![kv_dim, h],
                rng.fill(kv_dim * h, proj(h)),
            );
            add(
                "self_attn.v_proj.weight",
                vec![kv_dim, h],
                rng.fill(kv_dim * h, proj(h)),
            );
            if spec.qkv_bias {
                add("self_attn.q_proj.bias", vec![q_dim], rng.fill(q_dim, 0.1));
                add("self_attn.k_proj.bias", vec![kv_dim], rng.fill(kv_dim, 0.1));
                add("self_attn.v_proj.bias", vec![kv_dim], rng.fill(kv_dim, 0.1));
            }
            if spec.head_norms {
                add("self_attn.q_norm.weight", vec![hd], rng.around_one(hd));
                add("self_attn.k_norm.weight", vec![hd], rng.around_one(hd));
            }
            add(
                "self_attn.o_proj.weight",
                vec![h, q_dim],
                rng.fill(h * q_dim, proj(q_dim)),
            );
            add(
                "post_attention_layernorm.weight",
                vec![h],
                rng.around_one(h),
            );
            add(
                "mlp.gate_proj.weight",
                vec![inter, h],
                rng.fill(inter * h, proj(h)),
            );
            add(
                "mlp.up_proj.weight",
                vec![inter, h],
                rng.fill(inter * h, proj(h)),
            );
            add(
                "mlp.down_proj.weight",
                vec![h, inter],
                rng.fill(h * inter, proj(inter)),
            );
        }
        tensors.push(("model.norm.weight".into(), vec![h], rng.around_one(h)));

        let bytes: Vec<(String, Vec<usize>, Vec<u8>)> = tensors
            .into_iter()
            .map(|(name, shape, data)| {
                let raw = data.iter().flat_map(|v| v.to_le_bytes()).collect();
                (name, shape, raw)
            })
            .collect();
        let views = bytes.iter().map(|(name, shape, raw)| {
            let view = TensorView::new(Dtype::F32, shape.clone(), raw).expect("tensor view");
            (name.clone(), view)
        });
        let file = serialize(views, &None).expect("serialize safetensors");
        let tensors = SafeTensors::deserialize(&file).expect("parse safetensors");

        let config = format!(
            r#"{{"model_type": "qwen3", "hidden_size": {h}, "num_hidden_layers": {layers},
                "num_attention_heads": {heads}, "num_key_value_heads": {kv}, "head_dim": {hd},
                "intermediate_size": {inter}, "max_position_embeddings": {max_pos},
                "vocab_size": {vocab}, "rms_norm_eps": 1e-6, "rope_theta": 10000.0,
                "tie_word_embeddings": true}}"#,
            layers = spec.layers,
            heads = spec.heads,
            kv = spec.kv_heads,
            max_pos = spec.max_positions,
            vocab = spec.vocab,
        );
        let config: BertConfig = serde_json::from_str(&config).expect("parse config");
        BertModel::load_from_tensors(&tensors, config, true).expect("load synthetic model")
    }

    fn prompt(len: usize, vocab: usize, seed: u64) -> Vec<u32> {
        let mut rng = Rng(seed);
        (0..len)
            .map(|_| ((rng.next_f32() + 1.0) * 0.5 * vocab as f32) as u32 % vocab as u32)
            .collect()
    }

    /// The generic pass's final hidden state of the last position.
    fn reference_last_hidden(model: &BertModel, token_ids: &[u32]) -> Vec<f32> {
        let kv_dim = model.config.effective_num_kv_heads() * model.head_dim;
        let mut cache = crate::KvCache::new(model.config.num_hidden_layers, kv_dim);
        let hidden = model.decoder_forward(token_ids, &mut cache, 0).unwrap();
        hidden.row(hidden.nrows() - 1).to_vec()
    }

    fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
        assert_eq!(a.len(), b.len());
        a.iter()
            .zip(b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f32::max)
    }

    /// Hold the prefill to the generic pass on every prompt length, both on the final hidden
    /// state and on candidate logits through the public scoring entry points.
    fn assert_matches_reference(model: &BertModel, lengths: &[usize]) {
        let candidates: Vec<u32> = (0..8).collect();
        for &len in lengths {
            let ids = prompt(len, model.config.vocab_size, 1000 + len as u64);
            let fast = model
                .cpu_prefill_last_hidden(&ids)
                .unwrap()
                .expect("the prefill supports this model");
            let reference = reference_last_hidden(model, &ids);
            let hidden_diff = max_abs_diff(&fast, &reference);
            assert!(
                hidden_diff < 1e-4,
                "len {len}: hidden differs by {hidden_diff}"
            );

            let logits = model.score_next_tokens(&ids, &candidates).unwrap();
            let reference_logits = model
                .score_next_tokens_reference(&ids, &candidates)
                .unwrap();
            let logit_diff = max_abs_diff(&logits, &reference_logits);
            eprintln!(
                "len {len}: max |hidden diff| {hidden_diff:.2e}, max |logit diff| {logit_diff:.2e}"
            );
            assert!(
                logit_diff < 1e-4,
                "len {len}: logits differ by {logit_diff}"
            );
        }
    }

    const LENGTHS: [usize; 8] = [1, 2, 7, 63, 64, 65, 130, 200];

    /// The prefill's last `tail` rows match the generic pass row for row, and
    /// `final_hidden_tail` serves them on the CPU.
    #[test]
    fn prefill_tail_rows_match_generic_pass() {
        let model = synthetic_model(&qwen3_like(), 7);
        let kv_dim = model.config.effective_num_kv_heads() * model.head_dim;
        for (len, tail) in [(1, 1), (7, 3), (65, 7), (130, 130)] {
            let ids = prompt(len, model.config.vocab_size, 2000 + len as u64);
            let mut cache = crate::KvCache::new(model.config.num_hidden_layers, kv_dim);
            let reference = model.decoder_forward(&ids, &mut cache, 0).unwrap();
            let want: Vec<f32> = reference
                .slice(ndarray::s![len - tail.., ..])
                .iter()
                .copied()
                .collect();
            let fast = model
                .cpu_prefill_tail_hidden(&ids, tail)
                .unwrap()
                .expect("the prefill supports this model");
            let diff = max_abs_diff(&fast, &want);
            assert!(diff < 1e-4, "len {len} tail {tail}: rows differ by {diff}");
            let public = model.final_hidden_tail(&ids, tail).unwrap();
            assert_eq!(public.dim(), (tail, model.config.hidden_size));
            let diff = max_abs_diff(&public.iter().copied().collect::<Vec<_>>(), &want);
            assert!(
                diff < 1e-4,
                "len {len} tail {tail}: public rows differ by {diff}"
            );
        }
        assert!(model.cpu_prefill_tail_hidden(&[1, 2], 3).is_err());
        assert!(model.final_hidden_tail(&[1, 2], 0).is_err());
        assert!(model.final_hidden_tail(&[], 1).is_err());
    }

    /// A LoRA merge adds `scale * B A` to each adapted projection and rebuilds the
    /// fused Q|K|V weight the prefill reads, so the prefill and the generic pass
    /// still agree after it.
    #[test]
    fn a_lora_merge_changes_every_path_the_same_way() {
        let spec = qwen3_like();
        let mut model = synthetic_model(&spec, 7);
        let before_q = model.weights.layers[1].q_weight.clone();
        let before_down = model.weights.layers[0].ffn_down_weight.clone();
        let mut rng = Rng(99);
        let (q_out, q_in) = before_q.dim();
        let (d_out, d_in) = before_down.dim();
        let rank = 2;
        let matrix = |rng: &mut Rng, r: usize, c: usize| {
            Array2::from_shape_vec((r, c), rng.fill(r * c, 0.2)).unwrap()
        };
        let q_a = matrix(&mut rng, rank, q_in);
        let q_b = matrix(&mut rng, q_out, rank);
        let d_a = matrix(&mut rng, rank, d_in);
        let d_b = matrix(&mut rng, d_out, rank);
        let adapter = crate::LoraAdapter::for_test(
            0.5,
            vec![
                (1, crate::LoraModule::Query, q_a.clone(), q_b.clone()),
                (0, crate::LoraModule::Down, d_a.clone(), d_b.clone()),
            ],
        );
        model.merge_lora_for_test(&adapter).unwrap();
        let q = &model.weights.layers[1].q_weight;
        let want_q = &before_q + &(q_b.dot(&q_a) * 0.5);
        assert!(max_abs_diff(q.as_slice().unwrap(), want_q.as_slice().unwrap()) < 1e-6);
        let want_down = &before_down + &(d_b.dot(&d_a) * 0.5);
        assert!(
            max_abs_diff(
                model.weights.layers[0].ffn_down_weight.as_slice().unwrap(),
                want_down.as_slice().unwrap()
            ) < 1e-6
        );
        if let Some(fused) = &model.weights.layers[1].qkv_weight {
            let head = fused.slice(ndarray::s![..q_out, ..]).to_owned();
            assert_eq!(head, *q, "the fused Q|K|V weight carries the merged Q");
        }
        assert_matches_reference(&model, &[7, 65]);
        let bad = crate::LoraAdapter::for_test(1.0, vec![(99, crate::LoraModule::Query, q_a, q_b)]);
        assert!(model.merge_lora_for_test(&bad).is_err());
    }

    #[test]
    fn prefill_matches_generic_pass_on_grouped_query_model() {
        assert_matches_reference(&synthetic_model(&qwen3_like(), 7), &LENGTHS);
    }

    #[test]
    fn prefill_matches_generic_pass_with_biases_and_full_heads() {
        let spec = Spec {
            kv_heads: 4,
            head_norms: false,
            qkv_bias: true,
            ..qwen3_like()
        };
        let mut model = synthetic_model(&spec, 11);
        // The loader reads output and down-projection biases only under encoder names, so set
        // them directly to cover the bias paths.
        let mut rng = Rng(12);
        for layer in &mut model.weights.layers {
            layer.attn_out_bias = Some(Array1::from(rng.fill(spec.hidden, 0.1)));
            layer.ffn_down_bias = Some(Array1::from(rng.fill(spec.hidden, 0.1)));
        }
        assert_matches_reference(&model, &LENGTHS);
    }

    #[test]
    fn prefill_matches_generic_pass_without_fused_qkv_weight() {
        let mut model = synthetic_model(&qwen3_like(), 13);
        for layer in &mut model.weights.layers {
            layer.qkv_weight = None;
            layer.qkv_bias = None;
        }
        assert_matches_reference(&model, &[1, 65, 130]);
    }

    #[test]
    fn prefill_is_bit_for_bit_repeatable() {
        let model = synthetic_model(&qwen3_like(), 17);
        let ids = prompt(150, model.config.vocab_size, 18);
        let first = model.cpu_prefill_last_hidden(&ids).unwrap().unwrap();
        for _ in 0..3 {
            let again = model.cpu_prefill_last_hidden(&ids).unwrap().unwrap();
            let same = first
                .iter()
                .zip(&again)
                .all(|(a, b)| a.to_bits() == b.to_bits());
            assert!(same, "a repeated prefill changed its output");
        }
    }

    #[test]
    fn prefill_declines_prompts_past_the_rope_table() {
        let spec = Spec {
            max_positions: 16,
            ..qwen3_like()
        };
        let model = synthetic_model(&spec, 19);
        let ids = prompt(17, model.config.vocab_size, 20);
        assert!(model.cpu_prefill_last_hidden(&ids).unwrap().is_none());
        // Scoring still answers, through the generic pass.
        let logits = model.score_next_tokens(&ids, &[1, 2]).unwrap();
        let reference = model.score_next_tokens_reference(&ids, &[1, 2]).unwrap();
        assert_eq!(logits, reference);
    }

    #[test]
    fn prefill_rejects_tokens_outside_the_vocabulary() {
        let model = synthetic_model(&qwen3_like(), 21);
        let err = model.cpu_prefill_last_hidden(&[1, 500]).unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");
    }

    #[test]
    fn exp_fast_tracks_libm_across_the_softmax_and_silu_range() {
        let mut worst = 0.0f64;
        let mut x = -87.0f32;
        while x < 88.0 {
            let want = (x as f64).exp();
            let got = exp_fast(x) as f64;
            worst = worst.max(((got - want) / want).abs());
            x += 0.001_7;
        }
        // A few ulp of f32 (one ulp is about 1.2e-7 relative).
        assert!(worst < 4e-7, "worst relative error {worst}");
        assert_eq!(exp_fast(0.0), 1.0);
        assert!(exp_fast(-1000.0) >= 0.0 && exp_fast(-1000.0) < 1e-37);
        assert!(exp_fast(f32::NEG_INFINITY) < 1e-37);
        assert!(exp_fast(1000.0).is_finite());
    }

    fn naive_gemm(m: usize, n: usize, k: usize, a: &[f32], b: &[f32], op: Op) -> Vec<f32> {
        let mut c = vec![0.0f32; m * n];
        for i in 0..m {
            for j in 0..n {
                let mut acc = 0.0f64;
                for p in 0..k {
                    let bv = match op {
                        Op::T => b[j * k + p],
                        Op::N => b[p * n + j],
                    };
                    acc += a[i * k + p] as f64 * bv as f64;
                }
                c[i * n + j] = acc as f32;
            }
        }
        c
    }

    #[test]
    fn sgemm_honours_leading_dimensions_alpha_and_beta() {
        let mut rng = Rng(23);
        let (m, n, k) = (37, 29, 45);
        let a = rng.fill(m * k, 1.0);
        for op in [Op::T, Op::N] {
            let b = rng.fill(n * k, 1.0);
            let want = naive_gemm(m, n, k, &a, &b, op);
            // Embed every operand in a wider buffer so the leading dimensions differ from the
            // logical widths.
            let (lda, ldc) = (k + 3, n + 5);
            let (b_rows, b_cols) = if op == Op::T { (n, k) } else { (k, n) };
            let ldb = b_cols + 2;
            let widen = |src: &[f32], rows: usize, cols: usize, ld: usize| {
                let mut out = vec![0.0f32; rows * ld];
                for r in 0..rows {
                    out[r * ld..r * ld + cols].copy_from_slice(&src[r * cols..(r + 1) * cols]);
                }
                out
            };
            let wa = widen(&a, m, k, lda);
            let wb = widen(&b, b_rows, b_cols, ldb);
            let prior = rng.fill(m * ldc, 1.0);
            let mut c = prior.clone();
            sgemm(m, n, k, 0.5, &wa, lda, &wb, ldb, op, 2.0, &mut c, ldc);
            for i in 0..m {
                for j in 0..ldc {
                    let got = c[i * ldc + j];
                    if j < n {
                        let expect = 0.5 * want[i * n + j] + 2.0 * prior[i * ldc + j];
                        assert!(
                            (got - expect).abs() < 1e-4,
                            "{op:?} [{i},{j}] {got} vs {expect}"
                        );
                    } else {
                        assert_eq!(got, prior[i * ldc + j], "{op:?} wrote past column {n}");
                    }
                }
            }
        }
    }

    #[test]
    fn split_projection_matches_a_single_product() {
        let mut rng = Rng(29);
        // Large enough that `project` splits it into concurrent row blocks.
        let (rows, out, k) = (300, 160, 96);
        assert!(rows * out * k >= SPLIT_MIN_MACS);
        let x = rng.fill(rows * k, 1.0);
        let w = rng.fill(out * k, 1.0);
        let want = naive_gemm(rows, out, k, &x, &w, Op::T);
        let ldc = out + 7;
        let mut c = vec![1.0f32; rows * ldc];
        project(&x, k, rows, &w, out, 1.0, &mut c, ldc);
        for i in 0..rows {
            for j in 0..out {
                let got = c[i * ldc + j];
                let expect = want[i * out + j] + 1.0;
                assert!((got - expect).abs() < 1e-3, "[{i},{j}] {got} vs {expect}");
            }
            assert!(c[i * ldc + out..(i + 1) * ldc].iter().all(|&v| v == 1.0));
        }
    }

    #[test]
    fn causal_softmax_zeroes_the_masked_tail() {
        let mut row = vec![1.0f32, 2.0, 3.0, 50.0, -7.0];
        softmax_causal(&mut row, 3);
        let e: Vec<f32> = [1.0f32, 2.0, 3.0].iter().map(|v| (v - 3.0).exp()).collect();
        let sum: f32 = e.iter().sum();
        for (got, want) in row.iter().zip(e.iter().map(|v| v / sum)) {
            assert!((got - want).abs() < 1e-6);
        }
        assert_eq!(&row[3..], &[0.0, 0.0]);
    }
}
