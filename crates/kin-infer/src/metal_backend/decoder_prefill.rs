// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! GPU-resident decoder prefill for next-token scoring.
//!
//! The generic decoder path runs every operation as its own submission and
//! copies each attention head to and from the host. Scoring a prompt only needs
//! the residual stream at its last position, so this path keeps the whole stack
//! on the device instead: the prompt's embeddings go up once, every layer is
//! encoded into one command buffer, and one hidden row comes back.
//!
//! The matrix products use `matmul2d` from Metal Performance Primitives, which
//! runs on the GPU's matrix units where the hardware has them. Weights and GEMM
//! inputs are fp16, every product accumulates in fp32, and the residual stream,
//! the norms, the softmax and the rotary embedding stay fp32. The library needs
//! Metal Shading Language 4.0, so it compiles on first use and the path declines
//! (the caller falls back to the generic decoder) wherever it does not build.
//!
//! Per layer the stack runs:
//!
//! ```text
//! x    = rms_norm(h)                          fp16
//! qkv  = x @ [Wq; Wk; Wv]^T                   fp32
//! Q, K, V = split, per-head RMSNorm, RoPE     fp16, head-major
//! S    = Q K^T  (tiles on or below the diagonal only)
//! P    = causal softmax(S / sqrt(d))          fp16
//! h   += (P V) @ Wo^T                         residual add inside the GEMM
//! x    = rms_norm(h)
//! h   += (silu(x @ Wg^T) * (x @ Wu^T)) @ Wd^T
//! ```
//!
//! The last layer computes keys and values for every row but runs attention,
//! the output projection and the feed-forward block for the final row only,
//! since nothing downstream reads the others.

use super::{try_new_buffer, MetalCompute};
use crate::gpu::{DecoderLayerTensors, DecoderPrefill, DecoderPrefillConfig};
use crate::InferError;
use half::f16;
use metal::{Buffer, CommandBufferRef, ComputeCommandEncoderRef, ComputePipelineState, MTLSize};
use objc2::rc::autoreleasepool;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Output tile edge of every GEMM, matching `DP_TILE` in the shader source.
const TILE: usize = 64;

/// Threads per GEMM threadgroup: `matmul2d` runs on four 32-wide simdgroups.
const GEMM_THREADS: u64 = 128;

/// Upper bound on one attention chunk's fp32 score buffer. Long prompts run
/// attention over query-row chunks so the scores never need `seq^2` memory.
const SCORE_BUDGET_BYTES: usize = 256 << 20;

/// `MTLLanguageVersion4_0`. metal-rs does not name it yet, so it is set by value.
const MSL_VERSION_4_0: u64 = 0x40000;

const KERNEL_NAMES: &[&str] = &[
    "dp_gemm_nt",
    "dp_gemm_nt_acc",
    "dp_gemm_swiglu",
    "dp_rms_norm_half",
    "dp_qk_norm_rope",
    "dp_attn_scores",
    "dp_attn_softmax",
    "dp_attn_pv",
];

const SHADER_SOURCE: &str = r#"
#include <metal_stdlib>
#include <metal_tensor>
#include <MetalPerformancePrimitives/MetalPerformancePrimitives.h>
using namespace metal;
using namespace mpp::tensor_ops;

// Output tile of every GEMM. 64x64 on four simdgroups keeps each weight tile
// resident across the whole reduction.
constant constexpr int DP_TILE = 64;

typedef tensor<device half, dextents<int32_t, 2>, tensor_inline> dp_half_t;
typedef tensor<device float, dextents<int32_t, 2>, tensor_inline> dp_float_t;

// Tensor extents list the innermost dimension first, so a row-major [rows, cols]
// matrix has extents (cols, rows).
struct DpGemmParams {
    int m;
    int n;
    int k;
};

// C[m, n] = A[m, k] * B[n, k]^T. A and B are fp16; C is fp32 and overwritten.
kernel void dp_gemm_nt(
    device const half* A        [[buffer(0)]],
    device const half* B        [[buffer(1)]],
    device float* C             [[buffer(2)]],
    constant DpGemmParams& p    [[buffer(3)]],
    uint2 tg                    [[threadgroup_position_in_grid]])
{
    dp_half_t tA((device half*)A, dextents<int32_t, 2>(p.k, p.m));
    dp_half_t tB((device half*)B, dextents<int32_t, 2>(p.k, p.n));
    dp_float_t tC(C, dextents<int32_t, 2>(p.n, p.m));
    constexpr auto desc = matmul2d_descriptor(DP_TILE, DP_TILE, static_cast<int>(dynamic_extent),
                                              false, true, false,
                                              matmul2d_descriptor::mode::multiply);
    matmul2d<desc, execution_simdgroups<4>> op;
    auto mA = tA.slice(0, int(tg.y) * DP_TILE);
    auto mB = tB.slice(0, int(tg.x) * DP_TILE);
    auto mC = tC.slice(int(tg.x) * DP_TILE, int(tg.y) * DP_TILE);
    op.run(mA, mB, mC);
}

// C[m, n] += A[m, k] * B[n, k]^T. The attention output and FFN down projections
// accumulate straight into the fp32 residual stream this way.
kernel void dp_gemm_nt_acc(
    device const half* A        [[buffer(0)]],
    device const half* B        [[buffer(1)]],
    device float* C             [[buffer(2)]],
    constant DpGemmParams& p    [[buffer(3)]],
    uint2 tg                    [[threadgroup_position_in_grid]])
{
    dp_half_t tA((device half*)A, dextents<int32_t, 2>(p.k, p.m));
    dp_half_t tB((device half*)B, dextents<int32_t, 2>(p.k, p.n));
    dp_float_t tC(C, dextents<int32_t, 2>(p.n, p.m));
    constexpr auto desc = matmul2d_descriptor(DP_TILE, DP_TILE, static_cast<int>(dynamic_extent),
                                              false, true, false,
                                              matmul2d_descriptor::mode::multiply_accumulate);
    matmul2d<desc, execution_simdgroups<4>> op;
    auto mA = tA.slice(0, int(tg.y) * DP_TILE);
    auto mB = tB.slice(0, int(tg.x) * DP_TILE);
    auto mC = tC.slice(int(tg.x) * DP_TILE, int(tg.y) * DP_TILE);
    op.run(mA, mB, mC);
}

// out[m, n] = silu(A * Bg^T) * (A * Bu^T) in fp16. Both products come from one
// operation, so their cooperative tensors share a layout and element i of each
// holds the same output coordinate.
kernel void dp_gemm_swiglu(
    device const half* A        [[buffer(0)]],
    device const half* Bg       [[buffer(1)]],
    device const half* Bu       [[buffer(2)]],
    device half* out            [[buffer(3)]],
    constant DpGemmParams& p    [[buffer(4)]],
    uint2 tg                    [[threadgroup_position_in_grid]])
{
    dp_half_t tA((device half*)A, dextents<int32_t, 2>(p.k, p.m));
    dp_half_t tG((device half*)Bg, dextents<int32_t, 2>(p.k, p.n));
    dp_half_t tU((device half*)Bu, dextents<int32_t, 2>(p.k, p.n));
    constexpr auto desc = matmul2d_descriptor(DP_TILE, DP_TILE, static_cast<int>(dynamic_extent),
                                              false, true, false,
                                              matmul2d_descriptor::mode::multiply_accumulate);
    matmul2d<desc, execution_simdgroups<4>> op;
    const int row0 = int(tg.y) * DP_TILE;
    const int col0 = int(tg.x) * DP_TILE;
    auto mA = tA.slice(0, row0);
    auto mG = tG.slice(0, col0);
    auto mU = tU.slice(0, col0);
    auto cG = op.get_destination_cooperative_tensor<decltype(mA), decltype(mG), float>();
    auto cU = op.get_destination_cooperative_tensor<decltype(mA), decltype(mU), float>();
    #pragma unroll
    for (uint16_t i = 0; i < cG.get_capacity(); ++i) {
        if (cG.is_valid_element(i)) { cG[i] = 0.0f; cU[i] = 0.0f; }
    }
    op.run(mA, mG, cG);
    op.run(mA, mU, cU);
    #pragma unroll
    for (uint16_t i = 0; i < cG.get_capacity(); ++i) {
        if (!cG.is_valid_element(i)) continue;
        auto idx = cG.get_multidimensional_index(i);
        const int r = row0 + int(idx[1]);
        const int c = col0 + int(idx[0]);
        if (r < p.m && c < p.n) {
            const float g = cG[i];
            out[size_t(r) * p.n + c] = half((g / (1.0f + exp(-g))) * cU[i]);
        }
    }
}

// out = x * rsqrt(mean(x^2) + eps) * w in fp16, one threadgroup per row.
struct DpNormParams {
    int cols;
    float eps;
};

kernel void dp_rms_norm_half(
    device const float* x       [[buffer(0)]],
    device const float* w       [[buffer(1)]],
    device half* out            [[buffer(2)]],
    constant DpNormParams& p    [[buffer(3)]],
    uint row                    [[threadgroup_position_in_grid]],
    uint tid                    [[thread_index_in_threadgroup]],
    uint ntg                    [[threads_per_threadgroup]],
    uint sg                     [[simdgroup_index_in_threadgroup]],
    uint lane                   [[thread_index_in_simdgroup]])
{
    threadgroup float partial[32];
    device const float* xr = x + size_t(row) * p.cols;
    float ss = 0.0f;
    for (int j = int(tid); j < p.cols; j += int(ntg)) {
        const float v = xr[j];
        ss += v * v;
    }
    ss = simd_sum(ss);
    if (lane == 0) partial[sg] = ss;
    threadgroup_barrier(mem_flags::mem_threadgroup);
    const uint nsg = (ntg + 31) / 32;
    float total = 0.0f;
    for (uint s = 0; s < nsg; ++s) total += partial[s];
    const float inv = rsqrt(total / float(p.cols) + p.eps);
    device half* o = out + size_t(row) * p.cols;
    for (int j = int(tid); j < p.cols; j += int(ntg)) {
        o[j] = half(xr[j] * inv * w[j]);
    }
}

// Split the fused q|k|v projection into head-major fp16 Q, K and V, applying the
// optional per-head RMSNorm (Qwen3's q_norm and k_norm) and then the rotary
// embedding to Q and K. One simdgroup per (row, head); lane l owns elements
// l + 32*j, so the rotate-half partner of slot j is slot j + head_dim/64.
struct DpRopeParams {
    int rows;       // rows in qkv and in each head of Q, K and V
    int heads;
    int kv_heads;
    int head_dim;
    int pos0;       // absolute position of row 0
    int rope_rows;  // rows in the cos/sin tables; later positions stay unrotated
    float eps;
    int has_q_norm;
    int has_k_norm;
};

constant constexpr int DP_MAX_HEAD_DIM = 256;

kernel void dp_qk_norm_rope(
    device const float* qkv     [[buffer(0)]],
    device const float* q_norm  [[buffer(1)]],
    device const float* k_norm  [[buffer(2)]],
    device const float* cos_t   [[buffer(3)]],
    device const float* sin_t   [[buffer(4)]],
    device half* Q              [[buffer(5)]],
    device half* K              [[buffer(6)]],
    device half* V              [[buffer(7)]],
    constant DpRopeParams& p    [[buffer(8)]],
    uint2 tg                    [[threadgroup_position_in_grid]],
    uint lane                   [[thread_index_in_simdgroup]])
{
    const int row = int(tg.x);
    const int hh = int(tg.y);
    const int D = p.head_dim;
    const int q_dim = p.heads * D;
    const int kv_dim = p.kv_heads * D;
    const int width = q_dim + 2 * kv_dim;
    const int per_lane = D / 32;

    int col0;
    device half* dst;
    device const float* nw = nullptr;
    bool rotate;
    if (hh < p.heads) {
        col0 = hh * D;
        dst = Q + (size_t(hh) * p.rows + row) * D;
        if (p.has_q_norm) nw = q_norm;
        rotate = true;
    } else if (hh < p.heads + p.kv_heads) {
        const int k = hh - p.heads;
        col0 = q_dim + k * D;
        dst = K + (size_t(k) * p.rows + row) * D;
        if (p.has_k_norm) nw = k_norm;
        rotate = true;
    } else {
        const int v = hh - p.heads - p.kv_heads;
        col0 = q_dim + kv_dim + v * D;
        dst = V + (size_t(v) * p.rows + row) * D;
        rotate = false;
    }
    device const float* src = qkv + size_t(row) * width + col0;

    float x[DP_MAX_HEAD_DIM / 32];
    for (int j = 0; j < per_lane; ++j) x[j] = src[int(lane) + 32 * j];

    if (nw != nullptr) {
        float ss = 0.0f;
        for (int j = 0; j < per_lane; ++j) ss += x[j] * x[j];
        ss = simd_sum(ss);
        const float inv = 1.0f / sqrt(ss / float(D) + p.eps);
        for (int j = 0; j < per_lane; ++j) x[j] = x[j] * inv * nw[int(lane) + 32 * j];
    }

    const int pos = p.pos0 + row;
    if (rotate && pos < p.rope_rows) {
        const int halfj = per_lane / 2;
        device const float* c = cos_t + size_t(pos) * D;
        device const float* s = sin_t + size_t(pos) * D;
        for (int j = 0; j < halfj; ++j) {
            const int d = int(lane) + 32 * j;
            const float x0 = x[j];
            const float x1 = x[j + halfj];
            x[j] = x0 * c[d] - x1 * s[d];
            x[j + halfj] = x0 * s[d] + x1 * c[d];
        }
    }
    for (int j = 0; j < per_lane; ++j) dst[int(lane) + 32 * j] = half(x[j]);
}

// Causal attention over head-major Q, K and V in three steps:
//   S = Q K^T in fp32, computing only tiles on or below the diagonal,
//   P = softmax(scale * S) over the keys each query may see, in fp16,
//   O = P V, written position-major in fp16 as the output projection's input.
// Query rows [q0, q0 + q_rows) attend to keys [0, q0 + i]. S, P and O hold the
// q_rows rows of this call only, so long prompts can run in chunks.
struct DpAttnParams {
    int q_rows;     // query rows in this call
    int q0;         // absolute position of the first query row
    int kv_len;     // keys this call may read
    int rows;       // rows per head in the head-major Q, K and V buffers
    int ld_s;       // row stride of S and P, a multiple of DP_TILE
    int heads;
    int group;      // query heads per KV head
    int head_dim;
    float scale;
};

kernel void dp_attn_scores(
    device const half* Q        [[buffer(0)]],
    device const half* K        [[buffer(1)]],
    device float* S             [[buffer(2)]],
    constant DpAttnParams& p    [[buffer(3)]],
    uint3 tg                    [[threadgroup_position_in_grid]])
{
    const int h = int(tg.z);
    const int i0 = int(tg.y) * DP_TILE;
    const int j0 = int(tg.x) * DP_TILE;
    const int last_q = p.q0 + min(i0 + DP_TILE, p.q_rows) - 1;
    if (j0 > last_q) return;  // every key in this tile is in the future
    const int D = p.head_dim;
    dp_half_t tQ((device half*)(Q + (size_t(h) * p.rows + p.q0) * D),
                 dextents<int32_t, 2>(D, p.q_rows));
    dp_half_t tK((device half*)(K + size_t(h / p.group) * p.rows * D),
                 dextents<int32_t, 2>(D, p.kv_len));
    dp_float_t tS(S + size_t(h) * p.q_rows * p.ld_s, dextents<int32_t, 2>(p.kv_len, p.q_rows),
                  array<int32_t, 2>({1, p.ld_s}));
    constexpr auto desc = matmul2d_descriptor(DP_TILE, DP_TILE, static_cast<int>(dynamic_extent),
                                              false, true, false,
                                              matmul2d_descriptor::mode::multiply);
    matmul2d<desc, execution_simdgroups<4>> op;
    auto mQ = tQ.slice(0, i0);
    auto mK = tK.slice(0, j0);
    auto mS = tS.slice(j0, i0);
    op.run(mQ, mK, mS);
}

// One threadgroup per (query row, head). Besides the visible keys it writes
// zeros up to the last key of the row's P*V tile, so the P*V reduction can run
// to that tile boundary without reading scores from the future.
kernel void dp_attn_softmax(
    device const float* S       [[buffer(0)]],
    device half* P              [[buffer(1)]],
    constant DpAttnParams& p    [[buffer(2)]],
    uint2 tg                    [[threadgroup_position_in_grid]],
    uint tid                    [[thread_index_in_threadgroup]],
    uint2 ntg2                  [[threads_per_threadgroup]],
    uint sg                     [[simdgroup_index_in_threadgroup]],
    uint lane                   [[thread_index_in_simdgroup]])
{
    threadgroup float red[32];
    const uint ntg = ntg2.x;
    const int i = int(tg.x);
    const int h = int(tg.y);
    const int visible = min(p.q0 + i + 1, p.kv_len);
    const int tile_end = min(p.kv_len, p.q0 + (i / DP_TILE + 1) * DP_TILE);
    device const float* s = S + (size_t(h) * p.q_rows + i) * p.ld_s;
    device half* out = P + (size_t(h) * p.q_rows + i) * p.ld_s;
    const uint nsg = (ntg + 31) / 32;

    float mx = -INFINITY;
    for (int j = int(tid); j < visible; j += int(ntg)) mx = max(mx, s[j] * p.scale);
    mx = simd_max(mx);
    if (lane == 0) red[sg] = mx;
    threadgroup_barrier(mem_flags::mem_threadgroup);
    mx = -INFINITY;
    for (uint k = 0; k < nsg; ++k) mx = max(mx, red[k]);
    threadgroup_barrier(mem_flags::mem_threadgroup);

    float sum = 0.0f;
    for (int j = int(tid); j < visible; j += int(ntg)) sum += exp(s[j] * p.scale - mx);
    sum = simd_sum(sum);
    if (lane == 0) red[sg] = sum;
    threadgroup_barrier(mem_flags::mem_threadgroup);
    sum = 0.0f;
    for (uint k = 0; k < nsg; ++k) sum += red[k];
    const float inv = 1.0f / sum;

    for (int j = int(tid); j < visible; j += int(ntg)) out[j] = half(exp(s[j] * p.scale - mx) * inv);
    for (int j = visible + int(tid); j < tile_end; j += int(ntg)) out[j] = half(0.0f);
}

// O[i, h*D + d] = sum_j P[h, i, j] * V[h / group, j, d], with i relative to q0.
kernel void dp_attn_pv(
    device const half* P        [[buffer(0)]],
    device const half* V        [[buffer(1)]],
    device half* O              [[buffer(2)]],
    constant DpAttnParams& p    [[buffer(3)]],
    uint3 tg                    [[threadgroup_position_in_grid]])
{
    const int h = int(tg.z);
    const int i0 = int(tg.y) * DP_TILE;
    const int n0 = int(tg.x) * DP_TILE;
    const int D = p.head_dim;
    const int k_end = min(p.kv_len, p.q0 + i0 + DP_TILE);
    dp_half_t tP((device half*)(P + size_t(h) * p.q_rows * p.ld_s), dextents<int32_t, 2>(k_end, p.q_rows),
                 array<int32_t, 2>({1, p.ld_s}));
    dp_half_t tV((device half*)(V + size_t(h / p.group) * p.rows * D), dextents<int32_t, 2>(D, k_end));
    constexpr auto desc = matmul2d_descriptor(DP_TILE, DP_TILE, static_cast<int>(dynamic_extent),
                                              false, false, false,
                                              matmul2d_descriptor::mode::multiply_accumulate);
    matmul2d<desc, execution_simdgroups<4>> op;
    auto mP = tP.slice(0, i0);
    auto mV = tV.slice(n0, 0);
    auto cO = op.get_destination_cooperative_tensor<decltype(mP), decltype(mV), float>();
    #pragma unroll
    for (uint16_t e = 0; e < cO.get_capacity(); ++e) {
        if (cO.is_valid_element(e)) cO[e] = 0.0f;
    }
    op.run(mP, mV, cO);
    const int ld_o = p.heads * D;
    #pragma unroll
    for (uint16_t e = 0; e < cO.get_capacity(); ++e) {
        if (!cO.is_valid_element(e)) continue;
        auto idx = cO.get_multidimensional_index(e);
        const int r = i0 + int(idx[1]);
        const int c = n0 + int(idx[0]);
        if (r < p.q_rows && c < D) {
            O[size_t(r) * ld_o + h * D + c] = half(cO[e]);
        }
    }
}
"#;

#[repr(C)]
struct GemmParams {
    m: i32,
    n: i32,
    k: i32,
}

#[repr(C)]
struct NormParams {
    cols: i32,
    eps: f32,
}

#[repr(C)]
struct RopeParams {
    rows: i32,
    heads: i32,
    kv_heads: i32,
    head_dim: i32,
    pos0: i32,
    rope_rows: i32,
    eps: f32,
    has_q_norm: i32,
    has_k_norm: i32,
}

#[repr(C)]
struct AttnParams {
    q_rows: i32,
    q0: i32,
    kv_len: i32,
    rows: i32,
    ld_s: i32,
    heads: i32,
    group: i32,
    head_dim: i32,
    scale: f32,
}

type WeightKey = Vec<(usize, usize, u32, u32)>;

/// Per-backend state of the decoder prefill path.
pub(super) struct DecoderPrefillState {
    /// Compiled on first use; `None` when this device or OS cannot build it.
    kernels: OnceLock<Option<HashMap<&'static str, ComputePipelineState>>>,
    /// fp16 copies of the layer weights, keyed by the fp32 source slices the way
    /// the fp32 weight cache is. Model weights never move while the model lives.
    half_weights: Mutex<HashMap<WeightKey, Buffer>>,
    /// Activation buffers, grown to the longest prompt seen. The lock is held
    /// for a whole pass, so concurrent callers take turns on the device.
    scratch: Mutex<Scratch>,
    /// Byte ceiling for one attention chunk's scores ([`SCORE_BUDGET_BYTES`]).
    score_budget_bytes: usize,
}

impl Default for DecoderPrefillState {
    fn default() -> Self {
        Self {
            kernels: OnceLock::new(),
            half_weights: Mutex::new(HashMap::new()),
            scratch: Mutex::new(Scratch::default()),
            score_budget_bytes: SCORE_BUDGET_BYTES,
        }
    }
}

/// Query rows per attention chunk: as many whole tiles as fit the score budget,
/// at least one tile, and never more than the prompt.
fn attention_chunk_rows(seq: usize, heads: usize, budget_bytes: usize) -> usize {
    let ld = seq.div_ceil(TILE) * TILE;
    (budget_bytes / (heads * ld * 4) / TILE * TILE)
        .max(TILE)
        .min(ld)
}

#[derive(Default)]
struct Scratch {
    hidden: Option<Buffer>,
    normed: Option<Buffer>,
    qkv: Option<Buffer>,
    q: Option<Buffer>,
    k: Option<Buffer>,
    v: Option<Buffer>,
    scores: Option<Buffer>,
    probs: Option<Buffer>,
    attn: Option<Buffer>,
    act: Option<Buffer>,
}

/// Return `slot`'s buffer, replacing it first when it is smaller than `bytes`.
fn ensure(
    device: &metal::Device,
    slot: &mut Option<Buffer>,
    bytes: usize,
) -> Result<Buffer, InferError> {
    let bytes = bytes.max(16);
    if let Some(buf) = slot.as_ref() {
        if buf.length() as usize >= bytes {
            return Ok(buf.clone());
        }
    }
    *slot = None;
    let buf = try_new_buffer(device, bytes as u64).ok_or_else(|| {
        InferError::OutOfMemory(format!("metal decoder scratch alloc failed: {bytes} bytes"))
    })?;
    *slot = Some(buf.clone());
    Ok(buf)
}

/// Device buffers for one layer's weights.
struct LayerBuffers {
    attn_norm: Buffer,
    qkv: Buffer,
    q_head_norm: Option<Buffer>,
    k_head_norm: Option<Buffer>,
    o: Buffer,
    ffn_norm: Buffer,
    gate: Buffer,
    up: Buffer,
    down: Buffer,
}

fn compile(device: &metal::Device) -> Option<HashMap<&'static str, ComputePipelineState>> {
    use metal::foreign_types::ForeignType;
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let opts = metal::CompileOptions::new();
    let raw = opts.as_ptr() as *const AnyObject;
    if raw.is_null() {
        return None;
    }
    // SAFETY: `raw` is a live MTLCompileOptions owned by `opts`, and
    // `setLanguageVersion:` takes an NSUInteger.
    let () = unsafe { msg_send![&*raw, setLanguageVersion: MSL_VERSION_4_0] };
    let library = match device.new_library_with_source(SHADER_SOURCE, &opts) {
        Ok(library) => library,
        Err(err) => {
            tracing::info!(
                target: "kin_infer::decoder_prefill",
                error = %err,
                "Metal decoder prefill unavailable; decoder scoring uses the generic path"
            );
            return None;
        }
    };
    let mut pipelines = HashMap::new();
    for &name in KERNEL_NAMES {
        let pipeline = library
            .get_function(name, None)
            .and_then(|f| device.new_compute_pipeline_state_with_function(&f));
        match pipeline {
            Ok(p) => {
                pipelines.insert(name, p);
            }
            Err(err) => {
                tracing::info!(
                    target: "kin_infer::decoder_prefill",
                    kernel = name,
                    error = %err,
                    "Metal decoder prefill kernel unavailable; decoder scoring uses the generic path"
                );
                return None;
            }
        }
    }
    // The per-head kernel maps one 32-lane simdgroup onto a head.
    if pipelines["dp_qk_norm_rope"].thread_execution_width() != 32 {
        return None;
    }
    Some(pipelines)
}

/// Whether the kernels support these shapes. Anything else declines to the
/// generic decoder rather than failing.
fn supported(input: &DecoderPrefill<'_>) -> bool {
    let c = &input.config;
    let (h, d) = (c.hidden_size, c.head_dim);
    let q_dim = c.num_heads * d;
    let kv_dim = c.num_kv_heads * d;
    let seq = input.token_ids.len();
    let fits_i32 = |v: usize| v <= i32::MAX as usize;
    seq > 0
        && !input.layers.is_empty()
        && h > 0
        && c.num_kv_heads > 0
        && c.num_heads % c.num_kv_heads == 0
        && d % 64 == 0
        && d <= 256
        && c.inter_size > 0
        && fits_i32(seq * (q_dim + 2 * kv_dim))
        && fits_i32(seq * c.inter_size)
        && input.word_embeddings.len() % h == 0
        && input.rope_cos.len() == input.rope_sin.len()
        && input.rope_cos.len() % d == 0
        && input.layers.iter().all(|l| layer_shapes_match(l, c))
}

fn layer_shapes_match(l: &DecoderLayerTensors<'_>, c: &DecoderPrefillConfig) -> bool {
    let (h, d, inter) = (c.hidden_size, c.head_dim, c.inter_size);
    let q_dim = c.num_heads * d;
    let kv_dim = c.num_kv_heads * d;
    l.attn_norm.len() == h
        && l.q_weight.len() == q_dim * h
        && l.k_weight.len() == kv_dim * h
        && l.v_weight.len() == kv_dim * h
        && l.q_head_norm.is_none_or(|w| w.len() == d)
        && l.k_head_norm.is_none_or(|w| w.len() == d)
        && l.o_weight.len() == h * q_dim
        && l.ffn_norm.len() == h
        && l.gate_weight.len() == inter * h
        && l.up_weight.len() == inter * h
        && l.down_weight.len() == h * inter
}

fn params<T>(enc: &ComputeCommandEncoderRef, index: u64, value: &T) {
    enc.set_bytes(
        index,
        std::mem::size_of::<T>() as u64,
        value as *const T as *const std::ffi::c_void,
    );
}

fn dispatch(
    enc: &ComputeCommandEncoderRef,
    pipeline: &ComputePipelineState,
    buffers: &[(&Buffer, u64)],
    groups: MTLSize,
    threads: u64,
) {
    enc.set_compute_pipeline_state(pipeline);
    for (i, (buf, offset)) in buffers.iter().enumerate() {
        enc.set_buffer(i as u64, Some(buf), *offset);
    }
    enc.dispatch_thread_groups(groups, MTLSize::new(threads, 1, 1));
}

fn grid(x: usize, y: usize, z: usize) -> MTLSize {
    MTLSize::new(x as u64, y as u64, z as u64)
}

impl MetalCompute {
    /// fp16 device copy of `parts` concatenated row-major, converted once.
    fn half_weight(&self, parts: &[&[f32]]) -> Result<Buffer, InferError> {
        use half::slice::HalfFloatSliceExt;
        use rayon::prelude::*;

        let key: WeightKey = parts.iter().map(|p| Self::weight_cache_key(p)).collect();
        let mut cache = self.decoder_prefill.half_weights.lock();
        if let Some(buf) = cache.get(&key) {
            return Ok(buf.clone());
        }
        let total: usize = parts.iter().map(|p| p.len()).sum();
        let buf = try_new_buffer(&self.device, (total * 2) as u64).ok_or_else(|| {
            InferError::OutOfMemory(format!(
                "metal fp16 weight alloc failed: {} bytes",
                total * 2
            ))
        })?;
        // SAFETY: the buffer is shared storage of exactly `total` fp16 values, and
        // no command buffer references it before it is returned.
        let dst = unsafe { std::slice::from_raw_parts_mut(buf.contents() as *mut f16, total) };
        let mut offset = 0;
        for part in parts {
            dst[offset..offset + part.len()]
                .par_chunks_mut(1 << 16)
                .zip(part.par_chunks(1 << 16))
                .for_each(|(d, s)| d.convert_from_f32_slice(s));
            offset += part.len();
        }
        cache.insert(key, buf.clone());
        Ok(buf)
    }

    fn layer_buffers(&self, l: &DecoderLayerTensors<'_>) -> Result<LayerBuffers, InferError> {
        Ok(LayerBuffers {
            attn_norm: self.buf_cached(l.attn_norm)?,
            qkv: self.half_weight(&[l.q_weight, l.k_weight, l.v_weight])?,
            q_head_norm: l.q_head_norm.map(|w| self.buf_cached(w)).transpose()?,
            k_head_norm: l.k_head_norm.map(|w| self.buf_cached(w)).transpose()?,
            o: self.half_weight(&[l.o_weight])?,
            ffn_norm: self.buf_cached(l.ffn_norm)?,
            gate: self.half_weight(&[l.gate_weight])?,
            up: self.half_weight(&[l.up_weight])?,
            down: self.half_weight(&[l.down_weight])?,
        })
    }

    /// Backend half of [`crate::gpu::GpuCompute::decoder_prefill_last_hidden`] and
    /// [`crate::gpu::GpuCompute::decoder_prefill_tail_hidden`]: the residual stream of the last
    /// `tail` positions after the final layer, row after row.
    pub(super) fn run_decoder_prefill(
        &self,
        input: &DecoderPrefill<'_>,
        tail: usize,
    ) -> Result<Option<Vec<f32>>, InferError> {
        if !supported(input) {
            return Ok(None);
        }
        if tail == 0 || tail > input.token_ids.len() {
            return Err(InferError::Internal(format!(
                "metal decoder prefill: {tail} tail rows asked of a {}-token prompt",
                input.token_ids.len()
            )));
        }
        let Some(kernels) = self
            .decoder_prefill
            .kernels
            .get_or_init(|| compile(&self.device))
            .as_ref()
        else {
            return Ok(None);
        };
        let _span = tracing::info_span!(
            "kin_infer.metal.decoder_prefill",
            seq_len = input.token_ids.len(),
            layers = input.layers.len()
        )
        .entered();

        let c = input.config;
        let seq = input.token_ids.len();
        let h = c.hidden_size;
        let d = c.head_dim;
        let q_dim = c.num_heads * d;
        let kv_dim = c.num_kv_heads * d;
        let qkv_dim = q_dim + 2 * kv_dim;
        let vocab = input.word_embeddings.len() / h;

        let layers = input
            .layers
            .iter()
            .map(|l| self.layer_buffers(l))
            .collect::<Result<Vec<_>, _>>()?;
        let rope_cos = self.buf_cached(input.rope_cos)?;
        let rope_sin = self.buf_cached(input.rope_sin)?;
        let rope_rows = input.rope_cos.len() / d;

        // Attention runs over query-row chunks whose fp32 scores fit the budget.
        let ld_max = seq.div_ceil(TILE) * TILE;
        let chunk_rows =
            attention_chunk_rows(seq, c.num_heads, self.decoder_prefill.score_budget_bytes);

        let mut scratch = self.decoder_prefill.scratch.lock();
        let s = &mut *scratch;
        let dev = &self.device;
        let hidden = ensure(dev, &mut s.hidden, seq * h * 4)?;
        let normed = ensure(dev, &mut s.normed, seq * h * 2)?;
        let qkv = ensure(dev, &mut s.qkv, seq * qkv_dim * 4)?;
        let q = ensure(dev, &mut s.q, seq * q_dim * 2)?;
        let k = ensure(dev, &mut s.k, seq * kv_dim * 2)?;
        let v = ensure(dev, &mut s.v, seq * kv_dim * 2)?;
        let scores = ensure(dev, &mut s.scores, c.num_heads * chunk_rows * ld_max * 4)?;
        let probs = ensure(dev, &mut s.probs, c.num_heads * chunk_rows * ld_max * 2)?;
        let attn = ensure(dev, &mut s.attn, seq * q_dim * 2)?;
        let act = ensure(dev, &mut s.act, seq * c.inter_size * 2)?;

        // Gather the prompt's embeddings straight into the residual buffer.
        {
            // SAFETY: `hidden` holds at least seq * h floats, and the scratch lock
            // keeps every other pass off it until this one has read it back.
            let dst =
                unsafe { std::slice::from_raw_parts_mut(hidden.contents() as *mut f32, seq * h) };
            for (row, &id) in dst.chunks_exact_mut(h).zip(input.token_ids) {
                let id = id as usize;
                if id >= vocab {
                    return Err(InferError::Internal(format!(
                        "token id {id} outside the {vocab}-row embedding table"
                    )));
                }
                row.copy_from_slice(&input.word_embeddings[id * h..(id + 1) * h]);
            }
        }

        let pipe = |name: &str| &kernels[name];
        let norm = NormParams {
            cols: h as i32,
            eps: c.rms_eps,
        };
        let scale = 1.0 / (d as f32).sqrt();

        let status = autoreleasepool(|_| {
            let cmd: &CommandBufferRef = self.queue.new_command_buffer();
            let enc = cmd.new_compute_command_encoder();
            for (li, w) in layers.iter().enumerate() {
                let last = li + 1 == layers.len();
                // Rows whose output anything reads after attention: every row
                // except in the last layer, where only the last `tail` positions matter.
                let q_start = if last { seq - tail } else { 0 };
                let q_rows = seq - q_start;
                let row_off = (q_start * h * 4) as u64;

                params(enc, 3, &norm);
                dispatch(
                    enc,
                    pipe("dp_rms_norm_half"),
                    &[(&hidden, 0), (&w.attn_norm, 0), (&normed, 0)],
                    grid(seq, 1, 1),
                    256,
                );

                params(
                    enc,
                    3,
                    &GemmParams {
                        m: seq as i32,
                        n: qkv_dim as i32,
                        k: h as i32,
                    },
                );
                dispatch(
                    enc,
                    pipe("dp_gemm_nt"),
                    &[(&normed, 0), (&w.qkv, 0), (&qkv, 0)],
                    grid(qkv_dim.div_ceil(TILE), seq.div_ceil(TILE), 1),
                    GEMM_THREADS,
                );

                params(
                    enc,
                    8,
                    &RopeParams {
                        rows: seq as i32,
                        heads: c.num_heads as i32,
                        kv_heads: c.num_kv_heads as i32,
                        head_dim: d as i32,
                        pos0: 0,
                        rope_rows: rope_rows as i32,
                        eps: c.rms_eps,
                        has_q_norm: w.q_head_norm.is_some() as i32,
                        has_k_norm: w.k_head_norm.is_some() as i32,
                    },
                );
                // Absent head norms bind the rope table as a placeholder the
                // kernel never reads.
                dispatch(
                    enc,
                    pipe("dp_qk_norm_rope"),
                    &[
                        (&qkv, 0),
                        (w.q_head_norm.as_ref().unwrap_or(&rope_cos), 0),
                        (w.k_head_norm.as_ref().unwrap_or(&rope_cos), 0),
                        (&rope_cos, 0),
                        (&rope_sin, 0),
                        (&q, 0),
                        (&k, 0),
                        (&v, 0),
                    ],
                    grid(seq, c.num_heads + 2 * c.num_kv_heads, 1),
                    32,
                );

                let mut c0 = q_start;
                while c0 < seq {
                    let rows = chunk_rows.min(seq - c0);
                    let kv_len = c0 + rows;
                    let ap = AttnParams {
                        q_rows: rows as i32,
                        q0: c0 as i32,
                        kv_len: kv_len as i32,
                        rows: seq as i32,
                        ld_s: (kv_len.div_ceil(TILE) * TILE) as i32,
                        heads: c.num_heads as i32,
                        group: (c.num_heads / c.num_kv_heads) as i32,
                        head_dim: d as i32,
                        scale,
                    };
                    params(enc, 3, &ap);
                    dispatch(
                        enc,
                        pipe("dp_attn_scores"),
                        &[(&q, 0), (&k, 0), (&scores, 0)],
                        grid(kv_len.div_ceil(TILE), rows.div_ceil(TILE), c.num_heads),
                        GEMM_THREADS,
                    );
                    params(enc, 2, &ap);
                    dispatch(
                        enc,
                        pipe("dp_attn_softmax"),
                        &[(&scores, 0), (&probs, 0)],
                        grid(rows, c.num_heads, 1),
                        64,
                    );
                    params(enc, 3, &ap);
                    dispatch(
                        enc,
                        pipe("dp_attn_pv"),
                        &[
                            (&probs, 0),
                            (&v, 0),
                            (&attn, ((c0 - q_start) * q_dim * 2) as u64),
                        ],
                        grid(d.div_ceil(TILE), rows.div_ceil(TILE), c.num_heads),
                        GEMM_THREADS,
                    );
                    c0 += rows;
                }

                params(
                    enc,
                    3,
                    &GemmParams {
                        m: q_rows as i32,
                        n: h as i32,
                        k: q_dim as i32,
                    },
                );
                dispatch(
                    enc,
                    pipe("dp_gemm_nt_acc"),
                    &[(&attn, 0), (&w.o, 0), (&hidden, row_off)],
                    grid(h.div_ceil(TILE), q_rows.div_ceil(TILE), 1),
                    GEMM_THREADS,
                );

                params(enc, 3, &norm);
                dispatch(
                    enc,
                    pipe("dp_rms_norm_half"),
                    &[(&hidden, row_off), (&w.ffn_norm, 0), (&normed, 0)],
                    grid(q_rows, 1, 1),
                    256,
                );

                params(
                    enc,
                    4,
                    &GemmParams {
                        m: q_rows as i32,
                        n: c.inter_size as i32,
                        k: h as i32,
                    },
                );
                dispatch(
                    enc,
                    pipe("dp_gemm_swiglu"),
                    &[(&normed, 0), (&w.gate, 0), (&w.up, 0), (&act, 0)],
                    grid(c.inter_size.div_ceil(TILE), q_rows.div_ceil(TILE), 1),
                    GEMM_THREADS,
                );

                params(
                    enc,
                    3,
                    &GemmParams {
                        m: q_rows as i32,
                        n: h as i32,
                        k: c.inter_size as i32,
                    },
                );
                dispatch(
                    enc,
                    pipe("dp_gemm_nt_acc"),
                    &[(&act, 0), (&w.down, 0), (&hidden, row_off)],
                    grid(h.div_ceil(TILE), q_rows.div_ceil(TILE), 1),
                    GEMM_THREADS,
                );
            }
            enc.end_encoding();
            self.commit_wait(cmd);
            cmd.status()
        });
        if status == metal::MTLCommandBufferStatus::Error {
            return Err(InferError::Internal(
                "metal decoder prefill command buffer failed".into(),
            ));
        }

        // SAFETY: the command buffer has completed, and `hidden` holds seq * h floats.
        let last_row = unsafe {
            std::slice::from_raw_parts(
                (hidden.contents() as *const f32).add((seq - tail) * h),
                tail * h,
            )
        }
        .to_vec();
        if last_row.iter().any(|v| !v.is_finite()) {
            // An activation left fp16 range somewhere in the stack. The generic
            // decoder keeps fp32 throughout, so hand the prompt to it.
            tracing::warn!(
                target: "kin_infer::decoder_prefill",
                seq_len = seq,
                "Metal decoder prefill produced a non-finite hidden state; using the generic path"
            );
            return Ok(None);
        }
        Ok(Some(last_row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HIDDEN: usize = 128;
    const HEADS: usize = 4;
    const KV_HEADS: usize = 2;
    const HEAD_DIM: usize = 64;
    const INTER: usize = 256;
    const VOCAB: usize = 50;
    const ROPE_ROWS: usize = 256;
    const EPS: f32 = 1e-6;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
        }

        fn vec(&mut self, n: usize, scale: f32) -> Vec<f32> {
            (0..n).map(|_| self.next() * scale).collect()
        }

        fn norm(&mut self, n: usize) -> Vec<f32> {
            (0..n).map(|_| 1.0 + 0.2 * self.next()).collect()
        }
    }

    struct Layer {
        attn_norm: Vec<f32>,
        q: Vec<f32>,
        k: Vec<f32>,
        v: Vec<f32>,
        q_norm: Option<Vec<f32>>,
        k_norm: Option<Vec<f32>>,
        o: Vec<f32>,
        ffn_norm: Vec<f32>,
        gate: Vec<f32>,
        up: Vec<f32>,
        down: Vec<f32>,
    }

    struct Model {
        embeddings: Vec<f32>,
        layers: Vec<Layer>,
        cos: Vec<f32>,
        sin: Vec<f32>,
    }

    /// Two layers: the first with Qwen3-style per-head q/k norms, the second
    /// without (the Llama shape).
    fn model() -> Model {
        let mut rng = Rng(0x5eed);
        let q_dim = HEADS * HEAD_DIM;
        let kv_dim = KV_HEADS * HEAD_DIM;
        let s_h = 1.0 / (HIDDEN as f32).sqrt();
        let layers = (0..2)
            .map(|i| Layer {
                attn_norm: rng.norm(HIDDEN),
                q: rng.vec(q_dim * HIDDEN, s_h),
                k: rng.vec(kv_dim * HIDDEN, s_h),
                v: rng.vec(kv_dim * HIDDEN, s_h),
                q_norm: (i == 0).then(|| rng.norm(HEAD_DIM)),
                k_norm: (i == 0).then(|| rng.norm(HEAD_DIM)),
                o: rng.vec(HIDDEN * q_dim, 1.0 / (q_dim as f32).sqrt()),
                ffn_norm: rng.norm(HIDDEN),
                gate: rng.vec(INTER * HIDDEN, s_h),
                up: rng.vec(INTER * HIDDEN, s_h),
                down: rng.vec(HIDDEN * INTER, 1.0 / (INTER as f32).sqrt()),
            })
            .collect();
        let mut cos = vec![0.0; ROPE_ROWS * HEAD_DIM];
        let mut sin = vec![0.0; ROPE_ROWS * HEAD_DIM];
        let half = HEAD_DIM / 2;
        for pos in 0..ROPE_ROWS {
            for i in 0..half {
                let f = pos as f64 / 10_000f64.powf(2.0 * i as f64 / HEAD_DIM as f64);
                for d in [i, i + half] {
                    cos[pos * HEAD_DIM + d] = f.cos() as f32;
                    sin[pos * HEAD_DIM + d] = f.sin() as f32;
                }
            }
        }
        Model {
            embeddings: rng.vec(VOCAB * HIDDEN, 1.0),
            layers,
            cos,
            sin,
        }
    }

    fn config() -> DecoderPrefillConfig {
        DecoderPrefillConfig {
            hidden_size: HIDDEN,
            num_heads: HEADS,
            num_kv_heads: KV_HEADS,
            head_dim: HEAD_DIM,
            inter_size: INTER,
            rms_eps: EPS,
        }
    }

    fn matvec(w: &[f32], x: &[f64], rows: usize) -> Vec<f64> {
        let cols = x.len();
        (0..rows)
            .map(|r| {
                w[r * cols..(r + 1) * cols]
                    .iter()
                    .zip(x)
                    .map(|(&a, &b)| a as f64 * b)
                    .sum()
            })
            .collect()
    }

    fn rms_norm(x: &[f64], w: &[f32]) -> Vec<f64> {
        let ms = x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64;
        let inv = 1.0 / (ms + EPS as f64).sqrt();
        x.iter().zip(w).map(|(v, &g)| v * inv * g as f64).collect()
    }

    /// Plain float64 decoder over every row, returning the last row's residual.
    fn reference(m: &Model, tokens: &[u32]) -> Vec<f32> {
        reference_rows(m, tokens).pop().unwrap()
    }

    /// Plain float64 decoder over every row, returning every row's residual.
    fn reference_rows(m: &Model, tokens: &[u32]) -> Vec<Vec<f32>> {
        let q_dim = HEADS * HEAD_DIM;
        let kv_dim = KV_HEADS * HEAD_DIM;
        let half = HEAD_DIM / 2;
        let mut x: Vec<Vec<f64>> = tokens
            .iter()
            .map(|&t| {
                let t = t as usize;
                m.embeddings[t * HIDDEN..(t + 1) * HIDDEN]
                    .iter()
                    .map(|&v| v as f64)
                    .collect()
            })
            .collect();
        for l in &m.layers {
            let mut qs = Vec::new();
            let mut ks = Vec::new();
            let mut vs = Vec::new();
            for (pos, row) in x.iter().enumerate() {
                let xn = rms_norm(row, &l.attn_norm);
                let mut q = matvec(&l.q, &xn, q_dim);
                let mut k = matvec(&l.k, &xn, kv_dim);
                for (t, norm) in [(&mut q, &l.q_norm), (&mut k, &l.k_norm)] {
                    for head in t.chunks_mut(HEAD_DIM) {
                        if let Some(w) = norm {
                            let normed = rms_norm(head, w);
                            head.copy_from_slice(&normed);
                        }
                        for i in 0..half {
                            let (c, s) = (
                                m.cos[pos * HEAD_DIM + i] as f64,
                                m.sin[pos * HEAD_DIM + i] as f64,
                            );
                            let (a, b) = (head[i], head[i + half]);
                            head[i] = a * c - b * s;
                            head[i + half] = a * s + b * c;
                        }
                    }
                }
                qs.push(q);
                ks.push(k);
                vs.push(matvec(&l.v, &xn, kv_dim));
            }
            let scale = 1.0 / (HEAD_DIM as f64).sqrt();
            for i in 0..x.len() {
                let mut attn = vec![0.0; q_dim];
                for h in 0..HEADS {
                    let kvh = h / (HEADS / KV_HEADS);
                    let qh = &qs[i][h * HEAD_DIM..(h + 1) * HEAD_DIM];
                    let scores: Vec<f64> = (0..=i)
                        .map(|j| {
                            let kh = &ks[j][kvh * HEAD_DIM..(kvh + 1) * HEAD_DIM];
                            qh.iter().zip(kh).map(|(a, b)| a * b).sum::<f64>() * scale
                        })
                        .collect();
                    let mx = scores.iter().cloned().fold(f64::MIN, f64::max);
                    let e: Vec<f64> = scores.iter().map(|s| (s - mx).exp()).collect();
                    let sum: f64 = e.iter().sum();
                    for (j, p) in e.iter().enumerate() {
                        for d in 0..HEAD_DIM {
                            attn[h * HEAD_DIM + d] += p / sum * vs[j][kvh * HEAD_DIM + d];
                        }
                    }
                }
                for (xv, o) in x[i].iter_mut().zip(matvec(&l.o, &attn, HIDDEN)) {
                    *xv += o;
                }
                let xn = rms_norm(&x[i], &l.ffn_norm);
                let g = matvec(&l.gate, &xn, INTER);
                let u = matvec(&l.up, &xn, INTER);
                let act: Vec<f64> = g
                    .iter()
                    .zip(&u)
                    .map(|(g, u)| g / (1.0 + (-g).exp()) * u)
                    .collect();
                for (xv, dn) in x[i].iter_mut().zip(matvec(&l.down, &act, HIDDEN)) {
                    *xv += dn;
                }
            }
        }
        x.iter()
            .map(|row| row.iter().map(|&v| v as f32).collect())
            .collect()
    }

    fn layer_views(m: &Model) -> Vec<DecoderLayerTensors<'_>> {
        m.layers
            .iter()
            .map(|l| DecoderLayerTensors {
                attn_norm: &l.attn_norm,
                q_weight: &l.q,
                k_weight: &l.k,
                v_weight: &l.v,
                q_head_norm: l.q_norm.as_deref(),
                k_head_norm: l.k_norm.as_deref(),
                o_weight: &l.o,
                ffn_norm: &l.ffn_norm,
                gate_weight: &l.gate,
                up_weight: &l.up,
                down_weight: &l.down,
            })
            .collect()
    }

    fn run_layers(
        gpu: &MetalCompute,
        m: &Model,
        layers: &[DecoderLayerTensors<'_>],
        tokens: &[u32],
    ) -> Result<Option<Vec<f32>>, InferError> {
        gpu.run_decoder_prefill(
            &DecoderPrefill {
                token_ids: tokens,
                word_embeddings: &m.embeddings,
                layers,
                config: config(),
                rope_cos: &m.cos,
                rope_sin: &m.sin,
            },
            1,
        )
    }

    fn run(gpu: &MetalCompute, m: &Model, tokens: &[u32]) -> Option<Vec<f32>> {
        run_layers(gpu, m, &layer_views(m), tokens).unwrap()
    }

    fn tokens(seq: usize) -> Vec<u32> {
        (0..seq).map(|i| ((i * 7 + 3) % VOCAB) as u32).collect()
    }

    /// Runs `seq` tokens and checks the last hidden row against the float64
    /// reference, within fp16-operand rounding.
    fn check(seq: usize, score_budget_bytes: Option<usize>) {
        let Some(mut gpu) = MetalCompute::try_new() else {
            eprintln!("skipping: no Metal device");
            return;
        };
        if let Some(bytes) = score_budget_bytes {
            gpu.decoder_prefill.score_budget_bytes = bytes;
        }
        let m = model();
        let t = tokens(seq);
        let Some(got) = run(&gpu, &m, &t) else {
            eprintln!("skipping: the decoder prefill kernels do not build on this device");
            return;
        };
        let want = reference(&m, &t);
        let peak = want.iter().fold(1.0f32, |a, v| a.max(v.abs()));
        let diff = got
            .iter()
            .zip(&want)
            .fold(0.0f32, |a, (g, w)| a.max((g - w).abs()));
        assert!(
            diff / peak < 1e-2,
            "seq {seq}: max |diff| {diff} against peak {peak}"
        );
        let again = run(&gpu, &m, &t).expect("second pass");
        assert!(
            got.iter()
                .zip(&again)
                .all(|(a, b)| a.to_bits() == b.to_bits()),
            "seq {seq}: two passes over the same prompt differ"
        );
    }

    #[test]
    fn decoder_prefill_matches_reference_in_one_attention_chunk() {
        check(100, None);
    }

    #[test]
    fn decoder_prefill_matches_reference_across_attention_chunks() {
        // Room for exactly one 64-row tile of scores per chunk: 150 rows run as
        // chunks of 64, 64 and 22.
        let ld = 150usize.div_ceil(TILE) * TILE;
        assert_eq!(
            attention_chunk_rows(150, HEADS, HEADS * ld * 4 * TILE),
            TILE
        );
        check(150, Some(HEADS * ld * 4 * TILE));
    }

    #[test]
    fn decoder_prefill_matches_reference_for_short_prompts() {
        check(1, None);
        check(65, None);
    }

    /// The last `tail` rows come back row after row, each matching the
    /// reference, and the last of them is the one-row answer.
    #[test]
    fn decoder_prefill_returns_tail_rows_that_match_reference() {
        let Some(gpu) = MetalCompute::try_new() else {
            eprintln!("skipping: no Metal device");
            return;
        };
        let m = model();
        let seq = 100;
        let t = tokens(seq);
        let Some(one) = run(&gpu, &m, &t) else {
            eprintln!("skipping: the decoder prefill kernels do not build on this device");
            return;
        };
        let tail = 5;
        let got = gpu
            .run_decoder_prefill(
                &DecoderPrefill {
                    token_ids: &t,
                    word_embeddings: &m.embeddings,
                    layers: &layer_views(&m),
                    config: config(),
                    rope_cos: &m.cos,
                    rope_sin: &m.sin,
                },
                tail,
            )
            .unwrap()
            .expect("the prefill ran once already");
        assert_eq!(got.len(), tail * HIDDEN);
        let want = reference_rows(&m, &t);
        for (i, row) in got.chunks_exact(HIDDEN).enumerate() {
            let want = &want[seq - tail + i];
            let peak = want.iter().fold(1.0f32, |a, v| a.max(v.abs()));
            let diff = row
                .iter()
                .zip(want)
                .fold(0.0f32, |a, (g, w)| a.max((g - w).abs()));
            assert!(
                diff / peak < 1e-2,
                "tail row {i}: max |diff| {diff} against peak {peak}"
            );
        }
        let last = &got[(tail - 1) * HIDDEN..];
        let peak = one.iter().fold(1.0f32, |a, v| a.max(v.abs()));
        let diff = last
            .iter()
            .zip(&one)
            .fold(0.0f32, |a, (g, w)| a.max((g - w).abs()));
        assert!(
            diff / peak < 1e-3,
            "last tail row against the one-row pass: {diff}"
        );
        let too_many = DecoderPrefill {
            token_ids: &t[..3],
            word_embeddings: &m.embeddings,
            layers: &layer_views(&m),
            config: config(),
            rope_cos: &m.cos,
            rope_sin: &m.sin,
        };
        assert!(gpu.run_decoder_prefill(&too_many, 4).is_err());
    }

    #[test]
    fn decoder_prefill_declines_without_layers_and_rejects_unknown_tokens() {
        let Some(gpu) = MetalCompute::try_new() else {
            return;
        };
        let m = model();
        assert!(run_layers(&gpu, &m, &[], &tokens(3)).unwrap().is_none());
        if run(&gpu, &m, &tokens(3)).is_none() {
            return;
        }
        let bad = [1u32, VOCAB as u32];
        assert!(run_layers(&gpu, &m, &layer_views(&m), &bad).is_err());
    }
}
