# kin-infer

> Inference and embedding primitive: transformer inference in pure Rust.

`kin-infer` runs encoder and decoder transformer models entirely in Rust, with
GPU acceleration and no external ML framework dependency (no PyTorch, no ONNX,
no TensorFlow). Custom compute shaders and kernels drive the GPU backends.

It sits in the supporting layer of the open Kin local substrate and depends on
no other Kin crate. `kin-db` consumes it to embed source entities on-device.
Kin runs it with a deterministic pure-Rust CPU path for bit-reproducible
embedding over local code corpora.

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Part of Kin](https://img.shields.io/badge/part%20of-Kin-6E56CF.svg)](https://github.com/firelock-ai/kin)

## What is Kin?

Kin is a graph-native code repository for AI agents and the people who build with them:
your code as a graph of entities, relations, and intents, not a pile of files and diffs.
AI agents and humans navigate it semantically, with provenance, review, and governance
built in. It coexists with Git and projects graph truth back to a normal filesystem, so
any tool works unchanged.

Start at **[firelock-ai/kin](https://github.com/firelock-ai/kin)** · **[kinlab.ai](https://kinlab.ai)**

## Build

```bash
cargo build
```

The default build is pure Rust on the CPU. No feature flags, no GPU, and no
build-time ML dependency. `dot_product` compiles to NEON intrinsics on aarch64
and to AVX2 plus FMA on x86_64 when those target features are on, and drops to a
scalar kernel everywhere else.

Every GPU and BLAS backend is opt-in:

| Flag | Default | Purpose |
|------|---------|---------|
| `metal` | off | Apple Metal GPU backend (macOS, Apple silicon) |
| `cuda` | off | NVIDIA CUDA GPU backend through the driver API (Linux/Windows) |
| `accelerate` | off | Apple Accelerate BLAS on the CPU matmul path (macOS) |
| `decision` | off | `DecisionModel` and `DecisionEngine`, which load a decoder checkpoint with its `tokenizer.json` for typed decisions |

`gpu::create_compute()` walks Metal, then CUDA, then CPU, and the CPU rung is
always present, so a build with no feature flag still runs. `KIN_INFER_FORCE_CPU`
pins a whole process to the CPU. With `accelerate` compiled in, the CPU matmul
selects Accelerate at runtime; set `KIN_INFER_CPU_BACKEND=pure-rust` to force the
deterministic kernel, which is what bit-reproducible runs need because BLAS
differs from the pure-Rust reduction in the last ULPs.

### Backend maturity

The two GPU backends are not at the same depth.

**Metal is the developed path.** It overrides every operation in the
`GpuCompute` trait, including fused attention, the fused SwiGLU FFN, the fused
linear-add-norm folds, and the resident multi-layer forward paths that keep a
whole batch on device across layers. Most of the crate's dedicated backend
tests point at it: 20 of the 21 files under `tests/` are Metal-gated.

Decoder next-token scoring (`BertModel::score_next_tokens`) runs on Metal as one
device-resident prefill: the whole layer stack goes into a single command buffer
and only the last position's hidden state comes back. Its matrix products use
the `matmul2d` primitive of Metal Performance Primitives with fp16 weights and
fp32 accumulation, so it needs macOS 26 (Metal Shading Language 4.0). It covers
LLaMA-style decoders without projection biases, including Qwen3. Other models,
older systems, and runs with `KIN_INFER_NO_DECODER_PREFILL` set score on the
generic fp32 decoder.

**CUDA is real but narrower.** It JIT-compiles embedded PTX through the CUDA
driver API, so it needs the NVIDIA driver and no toolkit at build time. It
implements matmul, batched matmul, attention values, softmax, LayerNorm,
RMSNorm, GELU, SiLU, elementwise multiply, and RoPE. The fused and multi-layer
paths are not overridden, so they fall back to the trait's default
implementations, which compose those primitives and do part of the work back on
the CPU. Treat it as correct rather than tuned.

CI compiles every configuration on each push and pull request: pure Rust,
Accelerate, and Metal on macOS, pure Rust and CUDA on Linux. CI executes no GPU
kernel on either backend, because compiling a backend needs no device and
running one does. The Metal kernels are covered by the `metal`-gated suite in
`tests/`, which runs on a real device outside CI. The CUDA kernels have no
automated device coverage here.

## Test

```bash
cargo test                # CPU suite, no GPU needed
./scripts/run-tests.sh    # Metal suite, needs a real Apple GPU
```

`cargo test` is what CI runs on both macOS and Linux. The GPU probes are gated
behind the `metal` feature, so a default run compiles and exercises the CPU path
only.

`scripts/run-tests.sh` is the way in for Metal. It sweeps leftover GPU
processes, clears stale test binaries and fingerprints that have produced false
results before, then runs `cargo test --release --features metal`. Take the
umbrella GPU lane first, since these tests hold the device.

One test loads a real 273 MB model that lives outside the repo. It skips cleanly
when the model is absent, so it never breaks an ordinary run.

## Supported architectures

Architecture detection reads `model_type` out of the checkpoint's `config.json`,
so a model loads through whichever family it declares.

**Encoder:** BERT, RoBERTa and XLM-RoBERTa, ALBERT, DeBERTa and DeBERTa-v2, T5
and mT5 encoders, nomic-embed

**Decoder:** GPT-2, LLaMA, Mistral, Phi and Phi-3, Gemma and Gemma 2, Qwen2,
Qwen3

**Weight formats:** safetensors (single or sharded), F32/F16/BF16/Q8_0/Q4_0

**Positional:** learned, ALiBi, RoPE, relative bias (T5), disentangled (DeBERTa)

**Attention:** MHA, GQA, MQA

**Norm:** LayerNorm, RMSNorm

**FFN:** GELU, SwiGLU, GeGLU, ReGLU

## Key types

- `BertConfig` and `ModelArchitecture`: model configuration and family
  detection.
- `BertModel`: a loaded model and its weights.
- `KvCache` and `SamplingParams`: decoder-side generation state and sampling
  controls.
- `InferError`: typed error enum for model load, inference, and I/O failures.
- `decision::ChoiceQuestion` and `Decision`, and `DecisionModel` with the
  `decision` feature: ask a decoder model which of a few options, or none of them,
  fits a state described in text. The options are lettered in the prompt, and the next-token logits of
  `" a"`, `" b"`, ... become one probability per option through a softmax with a
  temperature, optionally averaged over the shown and the reversed option order.
- `DecisionEngine` and `DecisionEngineConfig`, with the `decision` feature: the
  in-process entry point for a trained decision model. It loads the model from a
  local directory, named in the config or by `KIN_DECISION_MODEL_DIR`, and never
  downloads one. It answers three kinds of question:
  - `score_candidates` takes a `CandidateQuery` (a query or a call site, and any
    number of candidate texts) and returns `CandidateScores`, a probability for every
    candidate and for "none of these". Candidates beyond one question's options are
    asked in several questions, and the answers combine into one distribution.
  - `choose` answers a `TypedQuestion<T>`, whose options carry caller values, in
    those values. `TypedQuestion::yes_no` asks a yes-or-no question.
  - `decide` answers a plain `ChoiceQuestion`.

  An optional `kin_student.json` beside the weights records how the model was
  trained: its fitted `temperature`, `max_options`, `max_prompt_tokens` and the
  `min_confidence` floor for a target error rate. The config overrides any of them.
  Every answer reads against a floor as a `Verdict`: an option, "none of these", or
  unsure.

  ```rust
  use kin_infer::{CandidateQuery, DecisionEngine, DecisionEngineConfig, Verdict};

  let engine = DecisionEngine::load(&DecisionEngineConfig::new("/models/decision"))?;
  let query = CandidateQuery::new("parse the config file", ["fn load_config", "fn render"]);
  let scores = engine.score_candidates(&query)?;
  match scores.verdict(engine.settings().min_confidence) {
      Verdict::Chosen(i) => println!("candidate {i} at {:.2}", scores.confidence),
      Verdict::NoneOfThese => println!("none of them"),
      Verdict::Unsure => println!("not confident enough to say"),
  }
  ```
- `gpu::GpuCompute`: the backend trait every compute path implements, plus
  `gpu::discover_devices()` and `gpu::create_compute()` for device selection.
- `metal_backend` (macOS, `metal`): Apple Metal compute shaders.
- `cuda_backend` (Linux/Windows, `cuda`): PTX kernels over the CUDA driver API.
- `resource::ResourcePlan`: host, memory, and accelerator inspection with the
  resolved embedding and kernel plans.
- `watchdog::EmbedWatchdog`: self-clean guard for long embed runs. It exits when
  the process is orphaned, when a wall-clock cap is exceeded, or when persisted
  throughput stays under the floor. Liveness keys on persisted batches, not on
  GPU utilization, because a busy-spin keeps utilization high while persisting
  nothing.

## License

[Apache-2.0](LICENSE).
