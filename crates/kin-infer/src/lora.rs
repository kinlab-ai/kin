// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Low-rank adapters (LoRA) merged into a decoder's weights at load time.
//!
//! A LoRA fine-tune ships as a small adapter beside a base checkpoint: for each adapted
//! projection `W` (`[out, in]`), a pair `A` (`[r, in]`) and `B` (`[out, r]`) whose product is
//! the fine-tune's change to `W`. The adapted layer computes `W x + scale * B (A x)`, which is
//! `(W + scale * B A) x`, so merging `scale * B A` into `W` once gives every forward path,
//! including the device-resident prefills, the adapted model with no per-call cost.
//!
//! [`LoraAdapter::load`] reads the adapter directory the PEFT library writes:
//! `adapter_config.json` and `adapter_model.safetensors`. It accepts the plain LoRA form on the
//! decoder's attention and feed-forward projections and refuses anything else it would
//! otherwise have to drop silently: DoRA, trained biases, saved whole modules, per-module
//! ranks, and trained token embeddings.
//!
//! [`BertModel::load_with_lora`] loads a base checkpoint and merges an adapter before the model
//! runs, so no backend has cached a weight from before the merge.

use std::collections::BTreeMap;
use std::path::Path;

use ndarray::Array2;
use safetensors::SafeTensors;
use serde::Deserialize;

use super::{
    build_fused_projection, decode_tensor_to_f32, load_weight_bytes, BertConfig, BertModel,
    InferError,
};

/// The projection one LoRA update adapts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LoraModule {
    Query,
    Key,
    Value,
    Output,
    Gate,
    Up,
    Down,
}

impl LoraModule {
    /// The module from its PEFT path within a decoder layer, such as `self_attn.q_proj`.
    fn from_path(path: &str) -> Option<Self> {
        Some(match path {
            "self_attn.q_proj" => Self::Query,
            "self_attn.k_proj" => Self::Key,
            "self_attn.v_proj" => Self::Value,
            "self_attn.o_proj" => Self::Output,
            "mlp.gate_proj" => Self::Gate,
            "mlp.up_proj" => Self::Up,
            "mlp.down_proj" => Self::Down,
            _ => return None,
        })
    }
}

/// One adapted projection: `W += scale * B A`.
struct LoraUpdate {
    layer: usize,
    module: LoraModule,
    /// `[r, in]`.
    a: Array2<f32>,
    /// `[out, r]`.
    b: Array2<f32>,
}

/// A LoRA adapter ready to merge into a decoder of the shape it was trained on.
pub struct LoraAdapter {
    rank: usize,
    scale: f32,
    base_model: Option<String>,
    updates: Vec<LoraUpdate>,
}

/// The `adapter_config.json` fields that decide how an adapter merges. Anything the merge
/// cannot honour is read so it can be refused.
#[derive(Deserialize)]
struct AdapterConfig {
    #[serde(default)]
    peft_type: Option<String>,
    r: usize,
    lora_alpha: f32,
    #[serde(default)]
    use_rslora: bool,
    #[serde(default)]
    use_dora: bool,
    #[serde(default)]
    fan_in_fan_out: bool,
    #[serde(default)]
    bias: Option<String>,
    #[serde(default)]
    modules_to_save: Option<Vec<String>>,
    #[serde(default)]
    rank_pattern: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    alpha_pattern: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    trainable_token_indices: Option<serde_json::Value>,
    #[serde(default)]
    base_model_name_or_path: Option<String>,
}

impl AdapterConfig {
    fn refusal(&self) -> Option<String> {
        if let Some(kind) = self.peft_type.as_deref() {
            if !kind.eq_ignore_ascii_case("lora") {
                return Some(format!("adapter type {kind} is not LoRA"));
            }
        }
        if self.r == 0 {
            return Some("adapter rank is 0".into());
        }
        if self.use_dora {
            return Some("DoRA adapters are not supported".into());
        }
        if self.fan_in_fan_out {
            return Some("fan_in_fan_out adapters (Conv1D layouts) are not supported".into());
        }
        if self.bias.as_deref().is_some_and(|b| b != "none") {
            return Some("adapters that train biases are not supported".into());
        }
        if self.modules_to_save.as_ref().is_some_and(|m| !m.is_empty()) {
            return Some("adapters that save whole modules are not supported".into());
        }
        if !self.rank_pattern.is_empty() || !self.alpha_pattern.is_empty() {
            return Some("per-module ranks or alphas are not supported".into());
        }
        if self
            .trainable_token_indices
            .as_ref()
            .is_some_and(|v| !v.is_null())
        {
            return Some("adapters that train token embeddings are not supported".into());
        }
        None
    }
}

/// The decoder layer and module a PEFT tensor name adapts, and whether it is `A` or `B`.
///
/// Names look like `base_model.model.layers.3.self_attn.q_proj.lora_A.weight`, with any
/// prefix before `layers.` and an optional adapter name (`lora_A.default.weight`).
fn parse_tensor_name(name: &str) -> Option<(usize, LoraModule, bool)> {
    let rest = &name[name.find("layers.")? + "layers.".len()..];
    let (layer, rest) = rest.split_once('.')?;
    let layer = layer.parse().ok()?;
    let (path, which) = if let Some(i) = rest.find(".lora_A.") {
        (&rest[..i], true)
    } else if let Some(i) = rest.find(".lora_B.") {
        (&rest[..i], false)
    } else {
        return None;
    };
    if !name.ends_with(".weight") {
        return None;
    }
    Some((layer, LoraModule::from_path(path)?, which))
}

impl LoraAdapter {
    /// Reads `adapter_config.json` and `adapter_model.safetensors` from a PEFT adapter
    /// directory.
    ///
    /// # Errors
    ///
    /// Fails when either file is missing or malformed, when the config asks for a form this
    /// merge does not implement, or when a tensor is not a LoRA `A` or `B` of a decoder
    /// projection, or lacks its partner.
    pub fn load(dir: &Path) -> Result<Self, InferError> {
        let config_path = dir.join("adapter_config.json");
        let text = std::fs::read_to_string(&config_path)
            .map_err(|e| InferError::ModelError(format!("{}: {e}", config_path.display())))?;
        let config: AdapterConfig = serde_json::from_str(&text)
            .map_err(|e| InferError::ModelError(format!("{}: {e}", config_path.display())))?;
        if let Some(reason) = config.refusal() {
            return Err(InferError::ModelIncompatible(format!(
                "{}: {reason}",
                config_path.display()
            )));
        }
        let weights_path = dir.join("adapter_model.safetensors");
        let bytes = load_weight_bytes(&weights_path)?;
        let tensors = SafeTensors::deserialize(&bytes)
            .map_err(|e| InferError::ModelError(format!("{}: {e}", weights_path.display())))?;
        Self::from_tensors(&tensors, &config)
    }

    fn from_tensors(tensors: &SafeTensors, config: &AdapterConfig) -> Result<Self, InferError> {
        let mut halves: BTreeMap<(usize, LoraModule), (Option<Array2<f32>>, Option<Array2<f32>>)> =
            BTreeMap::new();
        for name in tensors.names() {
            let Some((layer, module, is_a)) = parse_tensor_name(name) else {
                return Err(InferError::ModelIncompatible(format!(
                    "adapter tensor '{name}' is not a LoRA A or B of a decoder projection"
                )));
            };
            let view = tensors
                .tensor(name)
                .map_err(|e| InferError::ModelError(format!("adapter tensor '{name}': {e}")))?;
            let shape = view.shape();
            if shape.len() != 2 {
                return Err(InferError::ModelIncompatible(format!(
                    "adapter tensor '{name}' has shape {shape:?}, not a matrix"
                )));
            }
            let (rows, cols) = (shape[0], shape[1]);
            let rank_axis = if is_a { rows } else { cols };
            if rank_axis != config.r {
                return Err(InferError::ModelIncompatible(format!(
                    "adapter tensor '{name}' has rank {rank_axis}, the config says {}",
                    config.r
                )));
            }
            let floats = decode_tensor_to_f32(name, &view, rows * cols)?;
            let matrix = Array2::from_shape_vec((rows, cols), floats)
                .map_err(|e| InferError::ModelError(format!("adapter tensor '{name}': {e}")))?;
            let slot = halves.entry((layer, module)).or_default();
            let half = if is_a { &mut slot.0 } else { &mut slot.1 };
            if half.replace(matrix).is_some() {
                return Err(InferError::ModelIncompatible(format!(
                    "adapter holds two {} tensors for layer {layer} {module:?}",
                    if is_a { "A" } else { "B" }
                )));
            }
        }
        let mut updates = Vec::with_capacity(halves.len());
        for ((layer, module), pair) in halves {
            let (Some(a), Some(b)) = pair else {
                return Err(InferError::ModelIncompatible(format!(
                    "adapter has only one half of layer {layer} {module:?}"
                )));
            };
            updates.push(LoraUpdate {
                layer,
                module,
                a,
                b,
            });
        }
        if updates.is_empty() {
            return Err(InferError::ModelIncompatible(
                "adapter holds no LoRA tensors".into(),
            ));
        }
        let rank = config.r;
        let scale = if config.use_rslora {
            config.lora_alpha / (rank as f32).sqrt()
        } else {
            config.lora_alpha / rank as f32
        };
        Ok(Self {
            rank,
            scale,
            base_model: config.base_model_name_or_path.clone(),
            updates,
        })
    }

    /// The adapter's rank.
    pub fn rank(&self) -> usize {
        self.rank
    }

    /// The factor each `B A` product is scaled by before it is added: `alpha / r`, or
    /// `alpha / sqrt(r)` for a rank-stabilized adapter.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// The base checkpoint the adapter names, when its config records one.
    pub fn base_model(&self) -> Option<&str> {
        self.base_model.as_deref()
    }

    /// How many projections the adapter changes.
    pub fn len(&self) -> usize {
        self.updates.len()
    }

    /// Whether the adapter changes nothing. A loaded adapter never is.
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty()
    }
}

#[cfg(test)]
impl LoraAdapter {
    /// An adapter from explicit updates, for tests.
    pub(crate) fn for_test(
        scale: f32,
        updates: Vec<(usize, LoraModule, Array2<f32>, Array2<f32>)>,
    ) -> Self {
        Self {
            rank: updates.first().map_or(0, |(_, _, a, _)| a.nrows()),
            scale,
            base_model: None,
            updates: updates
                .into_iter()
                .map(|(layer, module, a, b)| LoraUpdate {
                    layer,
                    module,
                    a,
                    b,
                })
                .collect(),
        }
    }
}

impl BertModel {
    /// [`merge_lora`](Self::merge_lora), for tests of the paths that read merged weights.
    #[cfg(test)]
    pub(crate) fn merge_lora_for_test(&mut self, adapter: &LoraAdapter) -> Result<(), InferError> {
        self.merge_lora(adapter)
    }

    /// Loads a decoder checkpoint and merges a LoRA adapter into it before it runs.
    ///
    /// # Errors
    ///
    /// Fails when the checkpoint does not load, when the model is not decoder-only, or when an
    /// adapted projection does not exist in the model or has another shape than the adapter's.
    pub fn load_with_lora(
        weights_path: &Path,
        config: BertConfig,
        adapter: &LoraAdapter,
        force_cpu: bool,
    ) -> Result<Self, InferError> {
        let mut model = Self::load_with_backend(weights_path, config, force_cpu)?;
        model.merge_lora(adapter)?;
        Ok(model)
    }

    /// Adds `scale * B A` to every projection the adapter adapts, then rebuilds the fused
    /// Q|K|V weight of every layer whose Q, K or V changed. Must run before the first forward
    /// pass: a backend may keep a weight it has already uploaded.
    fn merge_lora(&mut self, adapter: &LoraAdapter) -> Result<(), InferError> {
        if !self.config.architecture().is_decoder_only() {
            return Err(InferError::ModelIncompatible(
                "LoRA merging is implemented for decoder-only models".into(),
            ));
        }
        let layers = self.weights.layers.len();
        let mut qkv_changed = vec![false; layers];
        for update in &adapter.updates {
            let layer = self.weights.layers.get_mut(update.layer).ok_or_else(|| {
                InferError::ModelIncompatible(format!(
                    "adapter changes layer {}, the model has {layers}",
                    update.layer
                ))
            })?;
            let weight = match update.module {
                LoraModule::Query => Some(&mut layer.q_weight),
                LoraModule::Key => Some(&mut layer.k_weight),
                LoraModule::Value => Some(&mut layer.v_weight),
                LoraModule::Output => Some(&mut layer.attn_out_weight),
                LoraModule::Gate => layer.ffn_gate_weight.as_mut(),
                LoraModule::Up => layer.ffn_up_weight.as_mut(),
                LoraModule::Down => Some(&mut layer.ffn_down_weight),
            }
            .ok_or_else(|| {
                InferError::ModelIncompatible(format!(
                    "adapter changes {:?} in layer {}, which this model does not have as a \
                     separate projection",
                    update.module, update.layer
                ))
            })?;
            let delta = update.b.dot(&update.a);
            if delta.dim() != weight.dim() {
                return Err(InferError::ModelIncompatible(format!(
                    "adapter's {:?} update in layer {} is {:?}, the weight is {:?}",
                    update.module,
                    update.layer,
                    delta.dim(),
                    weight.dim()
                )));
            }
            weight.scaled_add(adapter.scale, &delta);
            if matches!(
                update.module,
                LoraModule::Query | LoraModule::Key | LoraModule::Value
            ) {
                qkv_changed[update.layer] = true;
            }
        }
        for (layer, changed) in self.weights.layers.iter_mut().zip(qkv_changed) {
            if changed && layer.qkv_weight.is_some() {
                let (weight, bias) = build_fused_projection(
                    [&layer.q_weight, &layer.k_weight, &layer.v_weight],
                    [
                        layer.q_bias.as_ref(),
                        layer.k_bias.as_ref(),
                        layer.v_bias.as_ref(),
                    ],
                );
                layer.qkv_weight = weight;
                layer.qkv_bias = bias;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tensor_names_parse_with_any_prefix_and_adapter_name() {
        assert_eq!(
            parse_tensor_name("base_model.model.layers.3.self_attn.q_proj.lora_A.weight"),
            Some((3, LoraModule::Query, true))
        );
        assert_eq!(
            parse_tensor_name("base_model.model.model.layers.27.mlp.down_proj.lora_B.weight"),
            Some((27, LoraModule::Down, false))
        );
        assert_eq!(
            parse_tensor_name("layers.0.mlp.up_proj.lora_A.default.weight"),
            Some((0, LoraModule::Up, true))
        );
        assert_eq!(
            parse_tensor_name("base_model.model.layers.0.self_attn.q_proj.lora_magnitude_vector"),
            None
        );
        assert_eq!(
            parse_tensor_name("base_model.model.embed_tokens.lora_embedding_A"),
            None
        );
    }

    fn config(json: &str) -> AdapterConfig {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn forms_the_merge_cannot_honour_are_refused() {
        assert!(config(r#"{"r": 16, "lora_alpha": 32}"#).refusal().is_none());
        for refused in [
            r#"{"r": 16, "lora_alpha": 32, "use_dora": true}"#,
            r#"{"r": 16, "lora_alpha": 32, "bias": "all"}"#,
            r#"{"r": 16, "lora_alpha": 32, "modules_to_save": ["classifier"]}"#,
            r#"{"r": 16, "lora_alpha": 32, "rank_pattern": {"q_proj": 8}}"#,
            r#"{"r": 16, "lora_alpha": 32, "trainable_token_indices": {"embed_tokens": [1]}}"#,
            r#"{"r": 16, "lora_alpha": 32, "fan_in_fan_out": true}"#,
            r#"{"r": 16, "lora_alpha": 32, "peft_type": "IA3"}"#,
        ] {
            assert!(config(refused).refusal().is_some(), "{refused}");
        }
    }
}
