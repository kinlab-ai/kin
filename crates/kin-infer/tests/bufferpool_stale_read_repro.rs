// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC
//
// Metal standalone-norm stale-read detector.
//
// Hammers the standalone `layer_norm` path with rapid submissions and fails if
// any row reads as unpopulated. LayerNorm of an all-zero row is
// (0 - 0) * inv_std * gamma + beta = beta, so a row of non-constant input that
// comes back equal to `beta` (or non-finite) means the kernel read zeros or stale
// bytes instead of the uploaded input.
//
// Status: passes, reliably, before and after the weight-cache fix described
// below. It stays `#[ignore]`d only because it needs a Metal device.
//
// An earlier version of this header blamed a `StorageModeShared` rapid-reuse
// window in the buffer pool. That was never reproduced, and the pool's ordering
// holds on every path: a standalone op waits on its command buffer before its
// pooled buffers drop, an asynchronously committed command buffer hands its
// pooled buffers to its completion handler, and the pipelined segmented forwards
// wait on their last command buffer before the shared residual can recycle.
//
// The stale reads that were real came from the weight cache, not the pool.
// `GpuCompute::matmul` keeps its `B` operand on the device keyed by the slice's
// address, length and first and last values, which is sound only for a model
// weight. The generic decoder passed each attention head's keys and transposed
// values as that operand, and the allocator reuses a freed slice's address, so a
// later prompt with the same length and the same first and last tokens was
// scored against an earlier prompt's keys and values, while every call leaked
// device buffers. Per-call operands now go through
// `GpuCompute::matmul_activations`, which uploads through the pool, and the fused
// add-norm ops upload `gamma` and `beta` per call. The unit tests
// `matmul_activations_reads_the_operand_a_reused_address_gets_stale_from_the_weight_cache`,
// `fused_add_norm_reads_per_call_norm_vectors_fresh` and
// `decoder_passes_only_model_weights_to_weight_caching_matmul` cover that. The
// standalone norm path uploads its input, `gamma` and `beta` through the pool
// and has no cached operand, which is why this detector passes.
//
//   cargo test -p kin-infer --release --features metal \
//     --test bufferpool_stale_read_repro -- --ignored --nocapture

#![cfg(all(feature = "metal", target_os = "macos"))]

use kin_infer::gpu::{create_compute, GpuBackend};

/// Hammer the standalone `layer_norm` path with rapid submissions across the
/// widths the embedder uses (plus a non-multiple-of-32) and assert no row is ever
/// read stale: a correctly-normalized row of NON-constant input never equals
/// `beta`, so a row that equals `beta` (or is non-finite) means the input buffer
/// was not populated before the kernel ran.
#[test]
#[ignore = "needs a Metal device"]
fn standalone_norm_rapid_submission_no_stale_read() {
    let gpu = create_compute();
    if gpu.backend() != GpuBackend::Metal {
        eprintln!("SKIP: backend is {:?}, not Metal", gpu.backend());
        return;
    }

    let shapes = [
        (384usize, 40usize),
        (768, 40),
        (100, 40),
        (768, 3),
        (384, 1),
    ];
    let iters = 400usize;
    let mut stale = 0usize;
    let mut checked = 0usize;

    for it in 0..iters {
        for &(cols, rows) in &shapes {
            let n = rows * cols;
            // Distinct nonzero, per-row NON-constant data so a stale (zero) read
            // cannot accidentally match a real result.
            let data: Vec<f32> = (0..n)
                .map(|i| (((i + it) % 17) as f32 - 8.0) * 0.13 + 0.5)
                .collect();
            let gamma: Vec<f32> = (0..cols).map(|j| 1.0 + (j % 5) as f32 * 0.01).collect();
            let beta: Vec<f32> = (0..cols).map(|j| (j % 3) as f32 * 0.02).collect();

            let mut out = data.clone();
            gpu.layer_norm(&mut out, &gamma, &beta, rows, cols, 1e-5)
                .expect("layer_norm");

            for r in 0..rows {
                let row = &out[r * cols..(r + 1) * cols];
                let non_finite = row.iter().any(|x| !x.is_finite());
                // Input row was non-constant, so a correct normalized row differs
                // from beta; equality means the input was read as all-zero.
                let collapsed_to_beta = row
                    .iter()
                    .zip(beta.iter())
                    .all(|(x, b)| (x - b).abs() < 1e-6);
                checked += 1;
                if non_finite || collapsed_to_beta {
                    stale += 1;
                    eprintln!(
                        "[bufferpool] stale/unpopulated read: iter={it} cols={cols} rows={rows} row={r}"
                    );
                }
            }
        }
    }

    eprintln!("[bufferpool] checked {checked} rows, {stale} stale");
    assert_eq!(
        stale, 0,
        "BufferPool stale read reproduced: {stale}/{checked} norm rows read a stale/unpopulated pooled input"
    );
}
