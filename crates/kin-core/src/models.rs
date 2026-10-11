// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Optional model slots. Models select among graph-owned candidates; they never
//! supply repository truth or turn a score into evidence of absence.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{KinConfig, KinError, KinLayout, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSlot {
    Embeddings,
    Reranking,
    AnswerCompleteness,
    CallTarget,
}

impl ModelSlot {
    pub const ALL: [Self; 4] = [
        Self::Embeddings,
        Self::Reranking,
        Self::AnswerCompleteness,
        Self::CallTarget,
    ];

    pub fn env_prefix(self) -> &'static str {
        match self {
            Self::Embeddings => "KIN_MODEL_EMBEDDINGS",
            Self::Reranking => "KIN_MODEL_RERANKING",
            Self::AnswerCompleteness => "KIN_MODEL_ANSWER_COMPLETENESS",
            Self::CallTarget => "KIN_MODEL_CALL_TARGET",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelProvider {
    #[default]
    None,
    BuiltIn,
    External,
}

impl ModelProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::BuiltIn => "built-in",
            Self::External => "external",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "built-in" => Ok(Self::BuiltIn),
            "external" => Ok(Self::External),
            _ => Err(KinError::Config(format!(
                "unknown model provider '{value}': expected none, built-in or external"
            ))),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSlotConfig {
    #[serde(default)]
    pub provider: ModelProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Name of a secret environment variable, never the secret itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

impl ModelSlotConfig {
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("model", &self.model),
            ("base_url", &self.base_url),
            ("api_key_env", &self.api_key_env),
        ] {
            if value.as_ref().is_some_and(|value| value.trim().is_empty()) {
                return Err(KinError::Config(format!(
                    "model slot {name} cannot be empty"
                )));
            }
        }
        if self.provider != ModelProvider::External
            && (self.base_url.is_some() || self.api_key_env.is_some())
        {
            return Err(KinError::Config(
                "only an external model slot accepts base_url or api_key_env".into(),
            ));
        }
        if self.provider == ModelProvider::External
            && (self.model.is_none() || self.base_url.is_none())
        {
            return Err(KinError::Config(
                "an external model slot requires model and base_url".into(),
            ));
        }
        if self.provider == ModelProvider::None && self.model.is_some() {
            return Err(KinError::Config(
                "a none model slot cannot name a model".into(),
            ));
        }
        if let Some(url) = &self.base_url {
            let authority = url
                .split_once("://")
                .map(|(_, rest)| rest.split('/').next().unwrap_or_default());
            if !(url.starts_with("http://") || url.starts_with("https://"))
                || authority.is_none_or(|host| host.is_empty() || host.contains('@'))
                || url.contains(['?', '#'])
            {
                return Err(KinError::Config(
                    "model base_url must be HTTP(S) with no embedded credentials".into(),
                ));
            }
        }
        if self
            .api_key_env
            .as_ref()
            .is_some_and(|name| !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        {
            return Err(KinError::Config(
                "model api_key_env must name an environment variable".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelsConfig(pub BTreeMap<ModelSlot, ModelSlotConfig>);

impl ModelsConfig {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn validate(&self) -> Result<()> {
        for config in self.0.values() {
            config.validate()?;
        }
        Ok(())
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Explicit environment settings win over repository config. Legacy embedding
/// variables are admitted for one compatibility release, with the same rule.
pub fn resolve_slot(
    slot: ModelSlot,
    config: &KinConfig,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> Result<ModelSlotConfig> {
    let prefix = slot.env_prefix();
    let mut resolved = config.models.0.get(&slot).cloned().unwrap_or_default();
    let canonical_provider = nonempty(lookup(&format!("{prefix}_PROVIDER")));
    let canonical_model = nonempty(lookup(&format!("{prefix}_MODEL")));
    let mut explicit_provider = canonical_provider
        .as_deref()
        .map(ModelProvider::parse)
        .transpose()?;
    if slot == ModelSlot::Embeddings {
        let legacy_provider =
            nonempty(lookup("KIN_EMBED_PROVIDER")).map(|value| value.to_ascii_lowercase());
        let legacy_model = nonempty(lookup("KIN_EMBED_OPENAI_MODEL"))
            .or_else(|| nonempty(lookup("KIN_EMBED_MODEL_ID")));
        if explicit_provider.is_none() {
            explicit_provider = match legacy_provider.as_deref() {
                Some("none") => Some(ModelProvider::None),
                Some("local" | "hf" | "huggingface" | "bge") => Some(ModelProvider::BuiltIn),
                Some(
                    "openai" | "lmstudio" | "lm-studio" | "openai-compat" | "openai-compatible"
                    | "compatible",
                ) => Some(ModelProvider::External),
                Some(other) => {
                    return Err(KinError::Config(format!(
                        "unknown legacy embedding provider '{other}'"
                    )))
                }
                None if legacy_model.is_some() => Some(ModelProvider::BuiltIn),
                None => None,
            };
        }
        if canonical_model.is_none() {
            if let Some(model) = legacy_model {
                resolved.model = Some(model);
            } else if !config.models.0.contains_key(&slot) {
                if let Some(model) = &config.retrieval.embed_model {
                    resolved.provider = ModelProvider::BuiltIn;
                    resolved.model = Some(model.clone());
                }
            }
        }
        if explicit_provider == Some(ModelProvider::External) && canonical_provider.is_none() {
            if resolved.provider != ModelProvider::External {
                resolved.base_url = None;
                resolved.api_key_env = None;
            }
            resolved.provider = ModelProvider::External;
            if resolved.base_url.is_none() {
                resolved.base_url = nonempty(lookup("KIN_EMBED_OPENAI_BASE_URL"))
                    .or_else(|| nonempty(lookup("KIN_EMBED_BASE_URL")))
                    .or_else(|| {
                        Some(
                            if legacy_provider.as_deref() == Some("openai") {
                                "https://api.openai.com/v1"
                            } else {
                                "http://localhost:1234/v1"
                            }
                            .into(),
                        )
                    });
            }
            if resolved.model.is_none() && canonical_model.is_none() {
                resolved.model = Some(
                    if legacy_provider.as_deref() == Some("openai") {
                        "text-embedding-3-small"
                    } else {
                        "text-embedding-nomic-embed-text-v1.5"
                    }
                    .into(),
                );
            }
        }
    }
    if slot == ModelSlot::Reranking && canonical_provider.is_none() {
        let scorer_env = nonempty(lookup("KIN_LOCATE_SCORER"));
        let arm = nonempty(lookup("KIN_LOCATE_ARM")).or_else(|| config.retrieval.arm.clone());
        let named = arm
            .as_deref()
            .and_then(|arm| arm.split_once('+'))
            .map(|(_, name)| name);
        let scorer = named
            .filter(|name| *name != "scorer")
            .map(str::to_string)
            .or_else(|| scorer_env.clone())
            .or_else(|| config.retrieval.scorer.clone());
        let legacy_model = if scorer.as_deref() == Some("cross-encoder") {
            nonempty(lookup("KIN_LOCATE_CROSS_ENCODER_MODEL"))
        } else {
            nonempty(lookup("KIN_LOCATE_SCORER_MODEL"))
        };
        let explicit = scorer_env.is_some()
            || arm.as_ref().is_some_and(|arm| arm.contains('+'))
            || legacy_model.is_some();
        if explicit
            && (!config.models.0.contains_key(&slot)
                || scorer_env.is_some()
                || legacy_model.is_some())
        {
            explicit_provider = Some(match scorer.as_deref() {
                Some("none") => ModelProvider::None,
                Some("cross-encoder" | "decision-engine") | None => ModelProvider::BuiltIn,
                Some("external") => ModelProvider::External,
                Some(other) => {
                    return Err(KinError::Config(format!("unknown legacy scorer '{other}'")))
                }
            });
            if canonical_model.is_none() {
                resolved.model = legacy_model;
            }
        }
    }
    if let Some(provider) = explicit_provider {
        if provider != resolved.provider {
            resolved.base_url = None;
            resolved.api_key_env = None;
        }
        resolved.provider = provider;
    }
    if let Some(model) = canonical_model {
        resolved.model = Some(model);
    }
    for (suffix, target) in [
        ("BASE_URL", &mut resolved.base_url),
        ("API_KEY_ENV", &mut resolved.api_key_env),
    ] {
        if let Some(value) = nonempty(lookup(&format!("{prefix}_{suffix}"))) {
            *target = Some(value);
        }
    }
    if resolved.provider == ModelProvider::None {
        resolved.model = None;
        resolved.base_url = None;
        resolved.api_key_env = None;
    }
    resolved.validate()?;
    Ok(resolved)
}

/// Resolve config at the explicit repository boundary before worker threads
/// exist. Does not load weights, call a service, or change retrieval arms.
pub fn apply_repository_models_at(from: &Path) -> Result<()> {
    let Some(layout) = KinLayout::discover(from) else {
        return Ok(());
    };
    apply_repository_models_for_layout(&layout)
}

/// Resolve model config from an already selected store without rediscovering
/// its working tree through the ambient registry. Call before worker threads
/// exist, with the same precedence and validation as [`apply_repository_models_at`].
pub fn apply_repository_models_for_layout(layout: &KinLayout) -> Result<()> {
    let config = KinConfig::load_or_default(&layout.config_path())?;
    for (name, value) in repository_model_env(&config, |name| std::env::var(name).ok())? {
        // SAFETY: callers run this only before their runtime or worker threads.
        unsafe {
            std::env::set_var(name, value);
        }
    }
    Ok(())
}

/// The variables [`apply_repository_models_for_layout`] writes for `config`
/// over the environment `lookup` reads, without writing them.
///
/// Every slot is resolved before any assignment is returned, so no slot sees
/// another's result. A process that wants to know what a repository daemon
/// runs under, without exporting that repository's models into itself, reads
/// it here.
pub fn repository_model_env(
    config: &KinConfig,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> Result<Vec<(String, String)>> {
    let mut assignments = Vec::new();
    for slot in ModelSlot::ALL {
        let resolved = resolve_slot(slot, config, &mut lookup)?;
        let prefix = slot.env_prefix();
        assignments.push((
            format!("{prefix}_PROVIDER"),
            resolved.provider.as_str().to_string(),
        ));
        for (suffix, value) in [
            ("MODEL", resolved.model),
            ("BASE_URL", resolved.base_url),
            ("API_KEY_ENV", resolved.api_key_env),
        ] {
            if let Some(value) = value {
                assignments.push((format!("{prefix}_{suffix}"), value));
            }
        }
    }
    Ok(assignments)
}

/// An observation of a stage that actually ran, never an echo of configured
/// providers. Counts describe the graph population presented to that stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelUse {
    pub slot: ModelSlot,
    pub provider: ModelProvider,
    pub model: Option<String>,
    pub candidates: usize,
    pub returned: usize,
    pub filtered_out: usize,
}

impl ModelUse {
    pub fn validate(&self) -> bool {
        self.returned <= self.candidates
            && self.filtered_out == self.candidates - self.returned
            && match self.provider {
                ModelProvider::None => self.model.is_none(),
                _ => self
                    .model
                    .as_ref()
                    .is_some_and(|value| !value.trim().is_empty()),
            }
    }
}

/// Reject invented ids, duplicate rows and non-finite scores before any model
/// selection is applied. Withheld rows are counted even when all are filtered.
pub fn observe_graph_selection(
    config: &ModelSlotConfig,
    slot: ModelSlot,
    candidates: &[String],
    selected: &[(String, f64)],
) -> Result<ModelUse> {
    let admitted: BTreeSet<_> = candidates.iter().collect();
    if admitted.len() != candidates.len() {
        return Err(KinError::Config(
            "graph candidates contain duplicate ids".into(),
        ));
    }
    let mut seen = BTreeSet::new();
    for (id, score) in selected {
        if !admitted.contains(id) || !seen.insert(id) || !score.is_finite() {
            return Err(KinError::Config(
                "model selection must contain unique graph candidate ids and finite scores".into(),
            ));
        }
    }
    let observation = ModelUse {
        slot,
        provider: config.provider,
        model: if config.provider == ModelProvider::None {
            None
        } else {
            config.model.clone()
        },
        candidates: candidates.len(),
        returned: selected.len(),
        filtered_out: candidates.len() - selected.len(),
    };
    if !observation.validate() {
        return Err(KinError::Config(
            "model observation requires an actual model identity".into(),
        ));
    }
    Ok(observation)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(text: &str) -> KinConfig {
        toml::from_str(text).unwrap()
    }
    fn resolve(text: &str, env: &[(&str, &str)]) -> Result<ModelSlotConfig> {
        resolve_slot(ModelSlot::Embeddings, &config(text), |key| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        })
    }
    #[test]
    fn no_model_is_the_default_and_unknown_slots_fail() {
        assert_eq!(resolve("", &[]).unwrap(), ModelSlotConfig::default());
        assert!(toml::from_str::<KinConfig>("[models.typo]\nprovider='none'").is_err());
        assert!(resolve("", &[("KIN_MODEL_EMBEDDINGS_PROVIDER", "typo")]).is_err());
    }
    #[test]
    fn canonical_environment_then_legacy_environment_then_repository() {
        let text = "[models.embeddings]\nprovider='built-in'\nmodel='repo-model'";
        assert_eq!(
            resolve(text, &[]).unwrap().model.as_deref(),
            Some("repo-model")
        );
        assert_eq!(
            resolve(text, &[("KIN_EMBED_MODEL_ID", "old-env")])
                .unwrap()
                .model
                .as_deref(),
            Some("old-env")
        );
        assert_eq!(
            resolve(
                text,
                &[
                    ("KIN_MODEL_EMBEDDINGS_MODEL", "new-env"),
                    ("KIN_EMBED_MODEL_ID", "old-env")
                ]
            )
            .unwrap()
            .model
            .as_deref(),
            Some("new-env")
        );
        assert_eq!(
            resolve(text, &[("KIN_MODEL_EMBEDDINGS_PROVIDER", "none")]).unwrap(),
            ModelSlotConfig::default()
        );
    }
    #[test]
    fn external_config_requires_endpoint_and_identity_without_inline_secrets() {
        assert!(resolve("[models.embeddings]\nprovider='external'", &[]).is_err());
        assert!(resolve("[models.embeddings]\nprovider='external'\nmodel='x'\nbase_url='https://secret@host/v1'", &[]).is_err());
        assert!(resolve("[models.embeddings]\nprovider='external'\nmodel='x'\nbase_url='http://localhost:1234/v1'\napi_key_env='MODEL_SECRET'", &[]).is_ok());
        assert!(resolve("", &[("KIN_MODEL_EMBEDDINGS_PROVIDER", "external")]).is_err());
        let legacy = resolve("", &[("KIN_EMBED_PROVIDER", "openai")]).unwrap();
        assert_eq!(legacy.provider, ModelProvider::External);
        assert_eq!(
            legacy.base_url.as_deref(),
            Some("https://api.openai.com/v1")
        );
    }
    #[test]
    fn graph_selection_rejects_fabrication_and_counts_withheld_rows() {
        let settings = ModelSlotConfig {
            provider: ModelProvider::External,
            model: Some("observed-model".into()),
            base_url: Some("http://localhost/v1".into()),
            api_key_env: None,
        };
        let candidates = vec!["a".into(), "b".into()];
        let observed = observe_graph_selection(
            &settings,
            ModelSlot::Reranking,
            &candidates,
            &[("b".into(), 0.9)],
        )
        .unwrap();
        assert_eq!(
            (
                observed.candidates,
                observed.returned,
                observed.filtered_out
            ),
            (2, 1, 1)
        );
        assert!(observe_graph_selection(
            &settings,
            ModelSlot::Reranking,
            &candidates,
            &[("c".into(), 0.9)]
        )
        .is_err());
        assert!(observe_graph_selection(
            &settings,
            ModelSlot::Reranking,
            &candidates,
            &[("b".into(), f64::NAN)]
        )
        .is_err());
        assert!(observe_graph_selection(
            &settings,
            ModelSlot::Reranking,
            &candidates,
            &[("b".into(), 1.0), ("b".into(), 0.5)]
        )
        .is_err());
    }

    #[test]
    fn repository_model_env_states_every_provider_and_only_present_settings() {
        let defaults = repository_model_env(&KinConfig::default(), |_| None).unwrap();
        assert_eq!(
            defaults,
            [
                ("KIN_MODEL_EMBEDDINGS_PROVIDER", "none"),
                ("KIN_MODEL_RERANKING_PROVIDER", "none"),
                ("KIN_MODEL_ANSWER_COMPLETENESS_PROVIDER", "none"),
                ("KIN_MODEL_CALL_TARGET_PROVIDER", "none"),
            ]
            .map(|(name, value)| (name.to_string(), value.to_string()))
        );
        let external = config(
            "[models.embeddings]\nprovider='external'\nmodel='m'\nbase_url='http://localhost:9/v1'",
        );
        let assignments = repository_model_env(&external, |_| None).unwrap();
        assert!(assignments.contains(&(
            "KIN_MODEL_EMBEDDINGS_BASE_URL".to_string(),
            "http://localhost:9/v1".to_string()
        )));
        assert!(!assignments
            .iter()
            .any(|(name, _)| name == "KIN_MODEL_EMBEDDINGS_API_KEY_ENV"));
        assert!(repository_model_env(&external, |name| {
            (name == "KIN_MODEL_EMBEDDINGS_PROVIDER").then(|| "sideways".to_string())
        })
        .is_err());
    }

    #[test]
    fn scorer_aliases_opt_in_without_changing_unselected_slots() {
        let base = KinConfig::default();
        let env = [
            ("KIN_LOCATE_ARM", "cards+decision-engine"),
            ("KIN_LOCATE_SCORER_MODEL", "requested-model"),
        ];
        let lookup = |key: &str| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        };
        let slot = resolve_slot(ModelSlot::Reranking, &base, lookup).unwrap();
        assert_eq!(slot.provider, ModelProvider::BuiltIn);
        assert_eq!(slot.model.as_deref(), Some("requested-model"));
        assert_eq!(
            resolve_slot(ModelSlot::Embeddings, &base, lookup)
                .unwrap()
                .provider,
            ModelProvider::None
        );
        assert_eq!(
            resolve_slot(ModelSlot::Reranking, &base, |_| None)
                .unwrap()
                .provider,
            ModelProvider::None
        );
    }
}
