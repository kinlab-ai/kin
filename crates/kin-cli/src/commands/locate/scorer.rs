// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The scoring stage: one pass of a calibrated scorer over the leading
//! candidates of a description query, with a "none of these" threshold.
//!
//! The stage reads the first `KIN_LOCATE_SCORER_TOP_K` rows of the entity
//! ranking (40 by default), asks a [`CandidateScorer`] for the probability that
//! each one answers the query, and reorders those rows by it. Exact-name hits
//! keep their tier, the same rule every other ordering in this pipeline obeys.
//!
//! When no row reaches `KIN_LOCATE_SCORER_THRESHOLD` and no row is a name the
//! query spelled, the stage abstains. The rows stay, because they are what a
//! reader or a measurement inspects, and the answer says what they are: its
//! top-level `scorer` block carries `abstained: true`, and `kin-mcp` lifts that
//! block into `_kin.verdict.scorer` and lists the gap as the `none_of_these`
//! limiting factor, so no agent reads rank one as a guess the scorer rejected.
//!
//! Scorers are pluggable. `none` is the control, `cross-encoder` is the local
//! reranker, and `decision-engine` runs a small decision model in process
//! through kin-infer: Qwen3-Reranker-0.6B by default, or a Kev pointer decision
//! model, as `KIN_LOCATE_SCORER_MODEL` selects. Both answer one yes-or-no
//! question per candidate, and the answer's probability of `yes` is the score.
//!
//! A scorer's word is a relevance judgment, not evidence. It reorders the rows
//! it read and it may abstain, and that is all: a name the query spelled keeps
//! its tier above every score, and no verdict certifies a row because a model
//! rated it. The answer names the model that scored (`scorer.model`) and each
//! row's probability (`scorer_probability`), so a reader can weigh both.

use kin_core::models::{ModelProvider, ModelSlot, ModelUse};
use serde::{Deserialize, Serialize};

use super::{
    locate_env_f32, locate_env_usize, locate_exact_name_tier, record_degradation, LocateEntity,
    LocateResult, RetrievalDegradation,
};
use crate::retrieval_arm::ScorerKind;

/// The ledger component a scorer that could not run is recorded under.
pub const SCORER_COMPONENT: &str = "candidate_scorer";

/// The tracing target that, at debug level, records every query's scorer
/// inputs as JSON.
pub const CANDIDATES_TRACE_TARGET: &str = "kin::locate::scorer_candidates";

/// One candidate a scorer reads.
#[derive(Clone, Debug, PartialEq)]
pub struct ScoringCandidate {
    /// Graph entity id, empty for an artifact row.
    pub entity_id: String,
    pub name: String,
    pub kind: String,
    pub file: Option<String>,
    /// The candidate as the scorer reads it: one line naming it in the graph's
    /// own words, then its code with the documentation left out.
    pub text: String,
}

/// Why a scorer produced nothing for this query.
#[derive(Clone, Debug, PartialEq)]
pub struct ScorerUnavailable {
    /// Stable machine word: `model_not_cached`, `model_unsupported`,
    /// `init_failed`, `inference_failed`, `feature_disabled`, `not_in_build`.
    pub reason: &'static str,
    pub detail: String,
    pub remediation: String,
}

/// A scorer: the calibrated probability that each candidate answers the query.
///
/// `None` in a slot is "no opinion", which is what the `none` control returns
/// for every row; a scorer that has opinions returns a probability in `[0, 1]`
/// for every row. Local implementations never fetch model weights. An
/// explicitly configured external provider can call its reranking endpoint.
pub trait CandidateScorer: Send + Sync {
    /// The scorer's wire name, recorded on every answer it shaped.
    fn name(&self) -> &'static str;

    fn provider(&self) -> ModelProvider {
        ModelProvider::BuiltIn
    }

    /// The model this scorer runs, recorded on every answer it shaped. `None`
    /// for a scorer that runs no model.
    fn model(&self) -> Option<ScorerModel> {
        None
    }

    /// The most rows this scorer reads per query, when the device it runs on
    /// holds it under the configured `top_k`. May load the scorer's model.
    fn candidate_limit(&self) -> Option<usize> {
        None
    }

    fn score(
        &self,
        query: &str,
        candidates: &[ScoringCandidate],
    ) -> Result<Vec<Option<f32>>, ScorerUnavailable>;
}

/// The model behind a scorer, as an answer records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScorerModel {
    /// The Hugging Face model id, or the local directory it was loaded from.
    pub id: String,
    /// The revision it resolved to: a snapshot commit, a branch name, or
    /// `local`. `None` before the model was resolved.
    pub revision: Option<String>,
    /// The device it ran on (`Metal`, `CPU`, ...), once loaded.
    pub device: Option<String>,
}

/// The control: runs the stage and changes nothing.
pub struct NoneScorer;

impl CandidateScorer for NoneScorer {
    fn name(&self) -> &'static str {
        "none"
    }

    fn provider(&self) -> ModelProvider {
        ModelProvider::None
    }

    fn score(
        &self,
        _query: &str,
        candidates: &[ScoringCandidate],
    ) -> Result<Vec<Option<f32>>, ScorerUnavailable> {
        Ok(vec![None; candidates.len()])
    }
}

/// Platt scaling: `p = sigmoid(slope * logit + intercept)`.
///
/// The defaults, slope 1 and intercept 0, are the reranker's own sigmoid and
/// are NOT a calibration. The measurement that decides this arm fits both on
/// held-out locate data and sets `KIN_LOCATE_SCORER_CALIBRATION`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlattCalibration {
    pub slope: f32,
    pub intercept: f32,
}

impl Default for PlattCalibration {
    fn default() -> Self {
        Self {
            slope: 1.0,
            intercept: 0.0,
        }
    }
}

impl PlattCalibration {
    /// Parse `slope,intercept`. Anything else keeps the default and says so.
    pub fn parse(value: &str) -> Option<Self> {
        let (slope, intercept) = value.split_once(',')?;
        let slope = slope.trim().parse::<f32>().ok().filter(|v| v.is_finite())?;
        let intercept = intercept
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite())?;
        Some(Self { slope, intercept })
    }

    pub fn from_env() -> Self {
        match std::env::var("KIN_LOCATE_SCORER_CALIBRATION") {
            Ok(value) if !value.trim().is_empty() => Self::parse(&value).unwrap_or_else(|| {
                tracing::warn!(
                    value = %value,
                    "KIN_LOCATE_SCORER_CALIBRATION is not `slope,intercept`; using 1,0"
                );
                Self::default()
            }),
            _ => Self::default(),
        }
    }

    pub fn probability(self, logit: f32) -> f32 {
        1.0 / (1.0 + (-(self.slope * logit + self.intercept)).exp())
    }
}

/// The locally cached cross-encoder, calibrated to a probability.
///
/// It loads only from a snapshot already in the Hugging Face cache. A model
/// that is not there is reported unavailable, never fetched, so selecting this
/// scorer on a machine without the model costs one ledger entry and nothing
/// else.
pub struct CrossEncoderScorer {
    pub model_id: String,
    pub revision: String,
    pub calibration: PlattCalibration,
}

impl CrossEncoderScorer {
    /// The reranker `KIN_LOCATE_CROSS_ENCODER_MODEL` names, the same model the
    /// fused pipeline's optional rerank reads.
    pub fn from_env() -> Self {
        Self {
            model_id: std::env::var("KIN_MODEL_RERANKING_MODEL")
                .or_else(|_| std::env::var("KIN_LOCATE_CROSS_ENCODER_MODEL"))
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "BAAI/bge-reranker-base".to_string()),
            revision: std::env::var("KIN_LOCATE_CROSS_ENCODER_REVISION")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "main".to_string()),
            calibration: PlattCalibration::from_env(),
        }
    }
}

impl CandidateScorer for CrossEncoderScorer {
    fn name(&self) -> &'static str {
        "cross-encoder"
    }

    fn model(&self) -> Option<ScorerModel> {
        Some(ScorerModel {
            id: self.model_id.clone(),
            revision: Some(self.revision.clone()),
            device: None,
        })
    }

    #[cfg(feature = "embeddings")]
    fn score(
        &self,
        query: &str,
        candidates: &[ScoringCandidate],
    ) -> Result<Vec<Option<f32>>, ScorerUnavailable> {
        let Some(snapshot) =
            crate::retrieval_profile::cross_encoder_model_snapshot(&self.model_id, &self.revision)
        else {
            return Err(ScorerUnavailable {
                reason: "model_not_cached",
                detail: format!(
                    "the reranker {} is not in the local model cache, so no candidate was scored",
                    self.model_id
                ),
                remediation: format!(
                    "fetch {} into the Hugging Face cache once, or select KIN_LOCATE_SCORER=none",
                    self.model_id
                ),
            });
        };
        let load_target = super::cross_encoder_load_target(&self.model_id, Some(&snapshot));
        let encoder = crate::model_residency::cross_encoder(&load_target, &self.revision).map_err(
            |error| ScorerUnavailable {
                reason: "init_failed",
                detail: format!("the reranker {} did not load: {error}", self.model_id),
                remediation: "verify the cached model files, or select KIN_LOCATE_SCORER=none"
                    .to_string(),
            },
        )?;
        let documents: Vec<&str> = candidates.iter().map(|c| c.text.as_str()).collect();
        let logits = encoder
            .rerank(query, &documents)
            .map_err(|error| ScorerUnavailable {
                reason: "inference_failed",
                detail: format!(
                    "the reranker {} failed on this query: {error}",
                    self.model_id
                ),
                remediation: "retry, or select KIN_LOCATE_SCORER=none".to_string(),
            })?;
        Ok(logits
            .into_iter()
            .map(|logit| Some(self.calibration.probability(logit)))
            .collect())
    }

    #[cfg(not(feature = "embeddings"))]
    fn score(
        &self,
        _query: &str,
        _candidates: &[ScoringCandidate],
    ) -> Result<Vec<Option<f32>>, ScorerUnavailable> {
        Err(ScorerUnavailable {
            reason: "feature_disabled",
            detail: "this Kin build has no model inference, so no candidate was scored".to_string(),
            remediation: "install a Kin build with embedding support, or select \
                          KIN_LOCATE_SCORER=none"
                .to_string(),
        })
    }
}

/// The model `KIN_LOCATE_SCORER_MODEL` selects when it is unset.
pub const DEFAULT_DECISION_SCORER_MODEL: &str = "Qwen/Qwen3-Reranker-0.6B";

/// Rows a decision scorer reads per query on the CPU, when
/// `KIN_LOCATE_SCORER_TOP_K` does not say. A 0.6B model reads a candidate in
/// roughly a tenth of a second there, so this keeps the stage near its budget;
/// an accelerator reads the full default of 40.
pub const DEFAULT_DECISION_CPU_TOP_K: usize = 8;

/// The instruction a yes-or-no reranker reads beside every locate query.
///
/// Chosen on the locate seed queries over the model card's web-search default
/// and a code-search wording: it ranks as well as the code-search wording and
/// rates fewer wrong rows as likely.
pub const RERANK_INSTRUCTION: &str =
    "Given a description of what code does, retrieve the function, method or type that does it";

/// The longest reranker prompt, in tokens. A candidate's text is one line
/// naming it and then its code ([`SCORER_CODE_CHARS`] is sized for this
/// window); a prompt that still runs past it loses the end of its code, so one
/// row cannot hold up the query.
pub const RERANK_MAX_PROMPT_TOKENS: usize = 512;

/// Characters of an entity's code a scorer reads, after the line that names it.
///
/// Sized for [`RERANK_MAX_PROMPT_TOKENS`] with Qwen3-Reranker's tokenizer. The
/// prompt's frame and instruction take 78 tokens and a task statement 80 at
/// the ninetieth percentile, which leaves a candidate 354. The naming line
/// takes 41 of them at the median, and whitespace-collapsed Python runs 4.2
/// characters to a token (3.8 at the tenth percentile). At this size 6 of
/// Flask's 419 library functions run past the window and lose the end of
/// their code.
pub const SCORER_CODE_CHARS: usize = 1200;

/// Characters of an entity's documentation its naming line carries: the first
/// line, cut here. The rest of the documentation is what the scorer is spared.
pub const SCORER_DOC_CHARS: usize = 160;

/// The question a pointer decision model answers about each candidate, in the
/// yes-or-no form the family was trained on.
pub fn pointer_question_text(query: &str) -> String {
    format!("Is this code what the search \"{query}\" is looking for?")
}

/// The decision engine: a small decision model asked, for each candidate,
/// whether it is what the query is looking for.
///
/// `KIN_LOCATE_SCORER_MODEL` names the model: a Hugging Face id, optionally
/// `id@revision`, read only from the local model cache, or a local model
/// directory. Two kinds load:
///
/// - a yes-or-no reranker (Qwen3-Reranker), asked with its model card's
///   prompt and [`RERANK_INSTRUCTION`];
/// - a pointer decision model (Kev), a LoRA adapter with `head.pt` over the
///   base it names, asked [`pointer_question_text`].
///
/// Either model's probability of `yes` is the score, through
/// `KIN_LOCATE_SCORER_CALIBRATION` applied to its log-odds (the identity by
/// default). A model that is not cached, or whose backbone this build cannot
/// run, is reported unavailable and never fetched.
pub struct DecisionEngineScorer {
    /// The model as configured.
    pub model: String,
    pub calibration: PlattCalibration,
    #[cfg(feature = "decision")]
    loaded: std::sync::OnceLock<Result<std::sync::Arc<DecisionScorerModel>, ScorerUnavailable>>,
}

impl DecisionEngineScorer {
    pub fn new(model: impl Into<String>, calibration: PlattCalibration) -> Self {
        Self {
            model: model.into(),
            calibration,
            #[cfg(feature = "decision")]
            loaded: std::sync::OnceLock::new(),
        }
    }

    /// The model `KIN_LOCATE_SCORER_MODEL` names, or the default.
    pub fn from_env() -> Self {
        Self::new(
            std::env::var("KIN_MODEL_RERANKING_MODEL")
                .or_else(|_| std::env::var("KIN_LOCATE_SCORER_MODEL"))
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| DEFAULT_DECISION_SCORER_MODEL.to_string()),
            PlattCalibration::from_env(),
        )
    }
}

/// A decision model loaded for the locate scorer, kept resident per process.
#[cfg(feature = "decision")]
pub struct DecisionScorerModel {
    /// The model id, or the local directory it came from.
    pub id: String,
    /// The snapshot commit it resolved to, or `local`.
    pub revision: String,
    pub kind: DecisionModelKind,
}

#[cfg(feature = "decision")]
pub enum DecisionModelKind {
    Reranker(kin_infer::YesNoReranker),
    Pointer(kin_infer::PointerDecisionModel),
}

#[cfg(feature = "decision")]
impl DecisionScorerModel {
    /// The device the model runs on.
    pub fn backend(&self) -> kin_infer::gpu::GpuBackend {
        match &self.kind {
            DecisionModelKind::Reranker(model) => model.backend(),
            DecisionModelKind::Pointer(model) => model.backend(),
        }
    }

    /// The log-odds of `yes` for each candidate text.
    fn log_odds(&self, query: &str, texts: &[&str]) -> Result<Vec<f32>, kin_infer::DecisionError> {
        match &self.kind {
            DecisionModelKind::Reranker(model) => model.log_odds(RERANK_INSTRUCTION, query, texts),
            DecisionModelKind::Pointer(model) => model.yes_log_odds(
                texts,
                &kin_infer::PointerQuestion::yes_no(pointer_question_text(query), None),
            ),
        }
    }
}

#[cfg(feature = "decision")]
impl DecisionEngineScorer {
    /// The resident model, resolving and loading it on first use in this
    /// process. A failure is not cached, so a later query retries.
    fn loaded(&self) -> Result<&std::sync::Arc<DecisionScorerModel>, ScorerUnavailable> {
        self.loaded
            .get_or_init(|| {
                let force_cpu = kin_infer::gpu::force_cpu_from_env();
                let device_key = if force_cpu { "cpu" } else { "auto" };
                crate::model_residency::decision_scorer(&self.model, device_key, || {
                    load_decision_model(&self.model, force_cpu)
                })
                .map(|(model, _)| model)
            })
            .as_ref()
            .map_err(Clone::clone)
    }
}

/// Whether `revision` is a full commit hash rather than a ref name.
#[cfg(feature = "decision")]
fn is_commit(revision: &str) -> bool {
    revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The cached snapshot of `model_id` at `revision` (a ref name or a commit),
/// and the commit it is.
#[cfg(feature = "decision")]
fn cached_snapshot(model_id: &str, revision: &str) -> Option<(std::path::PathBuf, String)> {
    let base = crate::retrieval_profile::resolved_hf_cache_base()?;
    let dir = crate::retrieval_profile::model_cache_dir(&base, model_id);
    let commit = if is_commit(revision) {
        revision.to_string()
    } else {
        std::fs::read_to_string(dir.join("refs").join(revision))
            .ok()?
            .trim()
            .to_string()
    };
    if commit.is_empty() || std::path::Path::new(&commit).components().count() != 1 {
        return None;
    }
    let snapshot = dir.join("snapshots").join(&commit);
    snapshot.is_dir().then_some((snapshot, commit))
}

#[cfg(feature = "decision")]
fn not_cached(model: &str, what: &str) -> ScorerUnavailable {
    ScorerUnavailable {
        reason: "model_not_cached",
        detail: format!(
            "the decision model {model} is not usable from the local model cache ({what}), so no \
             candidate was scored"
        ),
        remediation: format!(
            "fetch {model} into the Hugging Face cache once, or select KIN_LOCATE_SCORER=none"
        ),
    }
}

#[cfg(feature = "decision")]
fn has_files(dir: &std::path::Path, files: &[&str]) -> bool {
    files.iter().all(|file| dir.join(file).is_file())
}

/// Resolve the configured model to local files and load it. Nothing is
/// downloaded.
#[cfg(feature = "decision")]
fn load_decision_model(
    configured: &str,
    force_cpu: bool,
) -> Result<DecisionScorerModel, ScorerUnavailable> {
    let local = std::path::Path::new(configured);
    let (id, revision, dir) = if local.is_dir() {
        (
            configured.to_string(),
            "local".to_string(),
            local.to_path_buf(),
        )
    } else {
        let (id, revision) = configured.split_once('@').unwrap_or((configured, "main"));
        let (dir, commit) = cached_snapshot(id, revision)
            .ok_or_else(|| not_cached(id, &format!("no snapshot for revision {revision}")))?;
        (id.to_string(), commit, dir)
    };
    let init_failed = |error: kin_infer::DecisionError| ScorerUnavailable {
        reason: "init_failed",
        detail: format!("the decision model {id} did not load: {error}"),
        remediation: "verify the cached model files, or select KIN_LOCATE_SCORER=none".to_string(),
    };
    let unsupported = |why: String| ScorerUnavailable {
        reason: "model_unsupported",
        detail: format!("the decision model {id} cannot run in this build: {why}"),
        remediation: format!(
            "select KIN_LOCATE_SCORER_MODEL={DEFAULT_DECISION_SCORER_MODEL}, or \
             KIN_LOCATE_SCORER=none"
        ),
    };
    let check_backbone = |config_dir: &std::path::Path| -> Result<(), ScorerUnavailable> {
        let text = std::fs::read_to_string(config_dir.join("config.json")).unwrap_or_default();
        match kin_infer::decision::unsupported_backbone(&text) {
            Some(why) => Err(unsupported(why)),
            None => Ok(()),
        }
    };

    let kind = if has_files(
        &dir,
        &[
            "adapter_config.json",
            "adapter_model.safetensors",
            "head.pt",
        ],
    ) {
        let checkpoint = kin_infer::PointerCheckpoint::read(&dir).map_err(init_failed)?;
        let base = checkpoint.base_model().to_string();
        let base_revision = checkpoint.base_revision().unwrap_or("main").to_string();
        let (base_dir, _) = cached_snapshot(&base, &base_revision)
            .filter(|(base_dir, _)| has_files(base_dir, &["config.json", "model.safetensors"]))
            .ok_or_else(|| {
                not_cached(
                    &base,
                    &format!("the base model {id} is trained on, at revision {base_revision}"),
                )
            })?;
        check_backbone(&base_dir)?;
        DecisionModelKind::Pointer(
            kin_infer::PointerDecisionModel::load(checkpoint, &base_dir, force_cpu)
                .map_err(init_failed)?,
        )
    } else if has_files(
        &dir,
        &["config.json", "model.safetensors", "tokenizer.json"],
    ) {
        check_backbone(&dir)?;
        DecisionModelKind::Reranker(
            kin_infer::YesNoReranker::load(&dir, force_cpu)
                .map_err(init_failed)?
                .with_max_tokens(RERANK_MAX_PROMPT_TOKENS),
        )
    } else {
        return Err(not_cached(
            &id,
            "the snapshot holds neither a reranker checkpoint nor an adapter with head.pt",
        ));
    };
    Ok(DecisionScorerModel { id, revision, kind })
}

impl CandidateScorer for DecisionEngineScorer {
    fn name(&self) -> &'static str {
        "decision-engine"
    }

    fn model(&self) -> Option<ScorerModel> {
        #[cfg(feature = "decision")]
        if let Some(Ok(model)) = self.loaded.get() {
            return Some(ScorerModel {
                id: model.id.clone(),
                revision: Some(model.revision.clone()),
                device: Some(model.backend().to_string()),
            });
        }
        Some(ScorerModel {
            id: self.model.clone(),
            revision: None,
            device: None,
        })
    }

    #[cfg(feature = "decision")]
    fn candidate_limit(&self) -> Option<usize> {
        let explicit =
            std::env::var("KIN_LOCATE_SCORER_TOP_K").is_ok_and(|value| !value.trim().is_empty());
        if explicit {
            return None;
        }
        let model = self.loaded().ok()?;
        (model.backend() == kin_infer::gpu::GpuBackend::Cpu)
            .then(|| locate_env_usize("KIN_LOCATE_SCORER_CPU_TOP_K", DEFAULT_DECISION_CPU_TOP_K))
    }

    #[cfg(feature = "decision")]
    fn score(
        &self,
        query: &str,
        candidates: &[ScoringCandidate],
    ) -> Result<Vec<Option<f32>>, ScorerUnavailable> {
        let model = self.loaded()?;
        let texts: Vec<&str> = candidates.iter().map(|c| c.text.as_str()).collect();
        let started = std::time::Instant::now();
        let log_odds = model
            .log_odds(query, &texts)
            .map_err(|error| ScorerUnavailable {
                reason: "inference_failed",
                detail: format!(
                    "the decision model {} failed on this query: {error}",
                    model.id
                ),
                remediation: "retry, or select KIN_LOCATE_SCORER=none".to_string(),
            })?;
        tracing::info!(
            model = %model.id,
            backend = %model.backend(),
            candidates = candidates.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "decision scorer ranked candidates"
        );
        Ok(log_odds
            .into_iter()
            .map(|odds| Some(self.calibration.probability(odds)))
            .collect())
    }

    #[cfg(not(feature = "decision"))]
    fn score(
        &self,
        _query: &str,
        _candidates: &[ScoringCandidate],
    ) -> Result<Vec<Option<f32>>, ScorerUnavailable> {
        Err(ScorerUnavailable {
            reason: "not_in_build",
            detail: "this Kin build has no decision engine, so no candidate was scored".to_string(),
            remediation: "select KIN_LOCATE_SCORER=cross-encoder or none, or use a build with \
                          the decision feature"
                .to_string(),
        })
    }
}

/// The scorer a kind names.
pub fn scorer_for(kind: ScorerKind) -> Box<dyn CandidateScorer> {
    #[cfg(test)]
    if let Some(scorer) = test_scorer::take() {
        return scorer;
    }
    let slot = kin_core::models::resolve_slot(
        ModelSlot::Reranking,
        &kin_core::KinConfig::default(),
        |key| std::env::var(key).ok(),
    );
    match slot {
        Ok(slot) if slot.provider == ModelProvider::External => {
            return match crate::model_slots::ExternalReranker::new(slot) {
                Ok(scorer) => Box::new(scorer),
                Err(error) => Box::new(UnavailableScorer(error)),
            };
        }
        Ok(slot)
            if slot.provider == ModelProvider::None
                && std::env::var("KIN_MODEL_RERANKING_PROVIDER").is_ok() =>
        {
            return Box::new(NoneScorer);
        }
        Err(error) => {
            return Box::new(UnavailableScorer(ScorerUnavailable {
                reason: "model_configuration_invalid",
                detail: error.to_string(),
                remediation: "correct the reranking model slot, or select its none provider".into(),
            }))
        }
        _ => {}
    }
    match kind {
        ScorerKind::None => Box::new(NoneScorer),
        ScorerKind::CrossEncoder => Box::new(CrossEncoderScorer::from_env()),
        ScorerKind::DecisionEngine => Box::new(DecisionEngineScorer::from_env()),
        ScorerKind::External => Box::new(UnavailableScorer(ScorerUnavailable {
            reason: "model_configuration_invalid",
            detail: "external reranking needs a model and endpoint in its model slot".into(),
            remediation: "configure the external reranking slot, or select its none provider"
                .into(),
        })),
    }
}

struct UnavailableScorer(ScorerUnavailable);

impl CandidateScorer for UnavailableScorer {
    fn name(&self) -> &'static str {
        "unavailable"
    }
    fn score(
        &self,
        _query: &str,
        _candidates: &[ScoringCandidate],
    ) -> Result<Vec<Option<f32>>, ScorerUnavailable> {
        Err(self.0.clone())
    }
}

/// How the stage runs: how many rows it reads and where it abstains.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScorerConfig {
    pub top_k: usize,
    pub threshold: f32,
}

impl ScorerConfig {
    pub const DEFAULT_TOP_K: usize = 40;
    pub const DEFAULT_THRESHOLD: f32 = 0.5;

    pub fn from_env() -> Self {
        Self {
            top_k: locate_env_usize("KIN_LOCATE_SCORER_TOP_K", Self::DEFAULT_TOP_K),
            threshold: locate_env_f32("KIN_LOCATE_SCORER_THRESHOLD", Self::DEFAULT_THRESHOLD)
                .min(1.0),
        }
    }
}

/// What the stage did to one answer, published at the answer's top level as
/// `scorer` and lifted by `kin-mcp` into `_kin.verdict.scorer`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ScorerReport {
    /// Which scorer ran: `none`, `cross-encoder` or `decision-engine`.
    pub name: String,
    /// Rows the scorer read.
    pub scored: usize,
    /// The calibrated probability below which the stage abstains.
    pub threshold: f32,
    /// The best calibrated probability among the scored rows. Absent when the
    /// scorer offered no opinion or could not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_probability: Option<f32>,
    /// True when no scored row reached the threshold and none was a name the
    /// query spelled: none of these rows is the answer.
    pub abstained: bool,
    /// Why the scorer produced nothing, when it could not run. The ledger
    /// carries the detail under the `candidate_scorer` component.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    /// The model that scored, or that was asked to: a Hugging Face id or a
    /// local directory. Absent for the control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The revision the model resolved to: a snapshot commit, a ref name, or
    /// `local`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_revision: Option<String>,
    /// The device the model ran on, such as `Metal` or `CPU`. It decides how
    /// many rows a decision scorer reads when `KIN_LOCATE_SCORER_TOP_K` is
    /// unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Successful execution over the graph-owned candidate set. A configured
    /// model that failed to run produces no observation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<ModelUse>,
}

/// Run the configured scorer over `result`'s entity ranking.
pub(crate) fn run_scorer_stage(
    result: &mut LocateResult,
    graph: &kin_db::InMemoryGraph,
    query: &str,
    kind: ScorerKind,
) {
    let scorer = scorer_for(kind);
    let mut config = ScorerConfig::from_env();
    if let Some(limit) = scorer.candidate_limit() {
        config.top_k = config.top_k.min(limit);
    }
    let candidates = scoring_candidates(result, graph, config.top_k);
    if tracing::enabled!(target: CANDIDATES_TRACE_TARGET, tracing::Level::DEBUG) {
        // The rows exactly as the scorer reads them, so a scorer can be
        // compared offline on the same inputs a query gave it.
        let rows: Vec<serde_json::Value> = candidates
            .iter()
            .zip(&result.entities)
            .map(|(c, entity)| {
                serde_json::json!({
                    "entity_id": c.entity_id, "name": c.name, "kind": c.kind,
                    "file": c.file, "text": c.text,
                    "name_tier": locate_exact_name_tier(entity, query),
                })
            })
            .collect();
        tracing::debug!(
            target: CANDIDATES_TRACE_TARGET,
            query,
            candidates = %serde_json::Value::Array(rows),
            "scorer candidates"
        );
    }
    let report = apply_candidate_scorer(result, query, scorer.as_ref(), &candidates, config);
    result.scorer = Some(report);
}

/// The first `top_k` rows as the scorer reads them.
fn scoring_candidates(
    result: &LocateResult,
    graph: &kin_db::InMemoryGraph,
    top_k: usize,
) -> Vec<ScoringCandidate> {
    result
        .entities
        .iter()
        .take(top_k)
        .map(|entity| ScoringCandidate {
            entity_id: entity.entity_id.clone(),
            name: entity.name.clone(),
            kind: entity.kind.clone(),
            file: entity.provenance.file.clone(),
            text: candidate_text(entity, graph),
        })
        .collect()
}

/// The text a scorer reads for one row: a line naming the entity, then its
/// code past its documentation. No file is read.
///
/// The code is the body the graph holds for the entity, the same text a
/// description is ranked against, so the scorer reads it whether or not the
/// answer carries bodies. A scorer shown a card with its whole docstring and
/// the start of a body, which in Python is the docstring again, reads almost
/// no code: on 256 descriptions that named no function that reading put the
/// described function in the top five 5 points less often than no scorer at
/// all, where the same model shown the code put it there 9 points more often.
fn candidate_text(entity: &LocateEntity, graph: &kin_db::InMemoryGraph) -> String {
    use kin_model::EntityStore as _;

    let held = entity
        .entity_id
        .parse::<uuid::Uuid>()
        .ok()
        .map(kin_model::EntityId)
        .and_then(|id| {
            let held = graph.get_entity(&id).ok().flatten()?;
            Some((held, super::cards::card_source_for(graph, &id)?))
        });
    let Some((held, card)) = held else {
        // An artifact row, or a row the graph no longer holds: what the answer
        // carries is all there is to read.
        let line = format!(
            "{} {} | {} | in {}",
            entity.kind,
            entity.name,
            entity.signature,
            entity.provenance.file.as_deref().unwrap_or("")
        );
        // No language is known for such a row, so its body is left whole.
        let code = code_past_documentation(entity.body.as_deref().unwrap_or(""), "", "", None);
        return scorer_text(line, &code);
    };
    let body = held
        .metadata
        .extra
        .get(kin_parser::extract::EMBEDDING_BODY_PREVIEW_KEY)
        .and_then(|value| value.as_str())
        .or(entity.body.as_deref())
        .unwrap_or("");
    let code = code_past_documentation(
        body,
        &held.signature,
        held.doc_summary.as_deref().unwrap_or(""),
        kin_parser::body_syntax::body_syntax_for(held.language).as_ref(),
    );
    scorer_text(card.naming_line(SCORER_DOC_CHARS), &code)
}

/// The naming line, then as much of the code as [`SCORER_CODE_CHARS`] allows.
fn scorer_text(line: String, code: &str) -> String {
    if code.is_empty() {
        return line;
    }
    let cut: String = code.chars().take(SCORER_CODE_CHARS).collect();
    format!("{line}\n{cut}")
}

/// One space between words, none at either end.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// How far into the text after a declaration its documentation may begin: the
/// rest of the header (a return type, a colon) and the opening quotes.
const DOCUMENTATION_STARTS_WITHIN: usize = 240;

/// An entity's code with its documentation left out, on one line.
///
/// Three things are dropped, each only when it is found:
///
/// - what stands above the declaration in the entity's span, which is its doc
///   comment and attributes, unless it begins with the language's decorator:
///   a decorator is code;
/// - the declaration itself, which the naming line already carries, wherever
///   [`find_declaration`] can place it;
/// - the documentation string that opens the body, in a language that writes
///   one there. It is matched by its own words inside its quotes, and failing
///   that as a string in those quotes with nothing but the header's end before
///   it.
///
/// What the language writes is `syntax`, as its adapter states it
/// ([`kin_parser::body_syntax`]); nothing here knows a language. With no
/// syntax stated the declaration is still dropped and the rest stays whole.
/// Anything this cannot place stays, so a language whose documentation it does
/// not recognize loses nothing.
fn code_past_documentation(
    body: &str,
    signature: &str,
    doc: &str,
    syntax: Option<&kin_parser::body_syntax::BodySyntax>,
) -> String {
    let body = collapse_whitespace(body);
    let doc = collapse_whitespace(doc);
    let (above, declared, mut rest) = match find_declaration(&body, signature) {
        Some((start, end)) => (&body[..start], true, &body[end..]),
        None => ("", false, body.as_str()),
    };
    let mut kept = String::new();
    if let Some(decorator) = syntax.and_then(|syntax| syntax.decorator) {
        if above.trim_start().starts_with(decorator) {
            kept.push_str(above.trim());
        }
    }
    // The characters that close a documentation string, where one is written.
    let quote_chars: Vec<char> = syntax
        .map(|syntax| {
            syntax
                .doc_string_quotes
                .iter()
                .filter_map(|quotes| quotes.chars().last())
                .collect()
        })
        .unwrap_or_default();
    let quoted = |at: &usize| {
        *at <= DOCUMENTATION_STARTS_WITHIN
            && rest[..*at]
                .trim_end()
                .ends_with(|c: char| quote_chars.contains(&c))
    };
    let documented = (!doc.is_empty() && !quote_chars.is_empty())
        .then(|| rest.find(doc.as_str()))
        .flatten()
        .filter(quoted)
        .map(|at| (at, at + doc.len()))
        .or_else(|| syntax.and_then(|syntax| leading_docstring(rest, syntax)));
    let header_end: &[char] = syntax.map_or(&[], |syntax| syntax.header_end);
    if let Some((start, end)) = documented {
        if !declared {
            // The header could not be told from the code, so it stays.
            kept.push(' ');
            kept.push_str(
                rest[..start]
                    .trim_end()
                    .trim_end_matches(quote_chars.as_slice())
                    .trim_end(),
            );
        }
        rest = rest[end..]
            .trim_start()
            .trim_start_matches(quote_chars.as_slice());
    } else if declared {
        // No documentation: the code begins where the header ends.
        rest = rest.trim_start_matches(|c: char| c == ' ' || header_end.contains(&c));
    }
    kept.push(' ');
    kept.push_str(rest.trim());
    kept.trim().to_string()
}

/// Where the declaration `signature` names stands in `body`, as a byte range.
///
/// The two are compared without their whitespace and without a comma that
/// only closes a list, because a declaration written over several lines is one
/// line in the graph's signature. Decorators the signature leads with are not
/// part of the declaration: the source spells them its own way.
fn find_declaration(body: &str, signature: &str) -> Option<(usize, usize)> {
    let declaration = signature
        .split_whitespace()
        .skip_while(|word| word.starts_with('@'))
        .collect::<Vec<_>>()
        .join(" ");
    let needle: Vec<char> = squeezed(&declaration).into_iter().map(|(c, _)| c).collect();
    let hay = squeezed(body);
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    let first = (0..=hay.len() - needle.len()).find(|&at| {
        needle
            .iter()
            .zip(&hay[at..])
            .all(|(want, (have, _))| want == have)
    })?;
    let (last, last_at) = hay[first + needle.len() - 1];
    Some((hay[first].1, last_at + last.len_utf8()))
}

/// The characters of `text` with their byte offsets, leaving out whitespace
/// and a comma that stands directly before a closing bracket.
fn squeezed(text: &str) -> Vec<(char, usize)> {
    let mut kept: Vec<(char, usize)> = Vec::with_capacity(text.len());
    for (at, c) in text.char_indices() {
        if c.is_whitespace() {
            continue;
        }
        if matches!(c, ')' | ']' | '}') && kept.last().is_some_and(|(last, _)| *last == ',') {
            kept.pop();
        }
        kept.push((c, at));
    }
    kept
}

/// The span of a documentation string that opens `rest`, when one does: only
/// the end of a header (a return type, the header's end) and a string prefix
/// stand before it. The quotes, prefixes and header end are `syntax`'s.
fn leading_docstring(
    rest: &str,
    syntax: &kin_parser::body_syntax::BodySyntax,
) -> Option<(usize, usize)> {
    let (open, quotes) = syntax
        .doc_string_quotes
        .iter()
        .filter_map(|quotes| rest.find(quotes).map(|at| (at, *quotes)))
        .min_by_key(|(at, _)| *at)?;
    if open > DOCUMENTATION_STARTS_WITHIN {
        return None;
    }
    // The language's string prefixes, alone or in a pair, may stand before
    // the quotes.
    let head = &rest[..open];
    let unprefixed =
        head.trim_end_matches(|c: char| syntax.string_prefixes.contains(&c.to_ascii_lowercase()));
    let before = if head.len() - unprefixed.len() <= 2 {
        unprefixed.trim_end()
    } else {
        head.trim_end()
    };
    if !(before.is_empty() || before.ends_with(syntax.header_end)) {
        return None;
    }
    let text_start = open + quotes.len();
    let close = rest[text_start..].find(quotes)? + text_start;
    Some((open, close + quotes.len()))
}

/// Score, reorder and judge the leading rows. Split from
/// [`run_scorer_stage`] so a test drives it with any scorer and any rows.
pub(crate) fn apply_candidate_scorer(
    result: &mut LocateResult,
    query: &str,
    scorer: &dyn CandidateScorer,
    candidates: &[ScoringCandidate],
    config: ScorerConfig,
) -> ScorerReport {
    let scored = candidates.len().min(result.entities.len());
    let mut report = ScorerReport {
        name: scorer.name().to_string(),
        scored,
        threshold: config.threshold,
        top_probability: None,
        abstained: false,
        unavailable: None,
        model: None,
        model_revision: None,
        device: None,
        observed: None,
    };
    if scored == 0 {
        record_model(&mut report, scorer);
        return report;
    }
    let outcome = scorer.score(query, &candidates[..scored]);
    record_model(&mut report, scorer);
    let probabilities = match outcome {
        Ok(probabilities) if probabilities.len() == scored => probabilities,
        Ok(probabilities) => {
            record_unavailable(
                result,
                &mut report,
                ScorerUnavailable {
                    reason: "inference_failed",
                    detail: format!(
                        "the {} scorer returned {} scores for {scored} candidates, so none was \
                         used",
                        scorer.name(),
                        probabilities.len()
                    ),
                    remediation: "retry, or select KIN_LOCATE_SCORER=none".to_string(),
                },
            );
            return report;
        }
        Err(unavailable) => {
            record_unavailable(result, &mut report, unavailable);
            return report;
        }
    };
    let probabilities: Vec<Option<f32>> = probabilities
        .into_iter()
        .map(|p| p.filter(|p| p.is_finite()).map(|p| p.clamp(0.0, 1.0)))
        .collect();
    if scorer.provider() == ModelProvider::None || report.model.is_some() {
        report.observed = Some(ModelUse {
            slot: ModelSlot::Reranking,
            provider: scorer.provider(),
            model: report.model.clone(),
            candidates: scored,
            returned: scored,
            filtered_out: 0,
        });
    }
    for (entity, probability) in result.entities.iter_mut().zip(&probabilities) {
        entity.scorer_probability = *probability;
    }
    report.top_probability = probabilities.iter().flatten().copied().reduce(f32::max);
    let Some(top) = report.top_probability else {
        // The control: no opinion, no reorder, no abstention.
        return report;
    };

    let head: Vec<LocateEntity> = result.entities.drain(..scored).collect();
    let mut ordered: Vec<(u8, f32, usize, LocateEntity)> = head
        .into_iter()
        .enumerate()
        .map(|(rank, entity)| {
            let tier = locate_exact_name_tier(&entity, query);
            let probability = entity.scorer_probability.unwrap_or(-1.0);
            (tier, probability, rank, entity)
        })
        .collect();
    ordered.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.total_cmp(&a.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    let named = ordered.iter().any(|(tier, ..)| *tier > 0);
    let reordered: Vec<LocateEntity> = ordered.into_iter().map(|(.., entity)| entity).collect();
    result.entities.splice(0..0, reordered);
    report.abstained = !named && top < config.threshold;
    report
}

/// Name the model behind the scores on the report, as the scorer knows it
/// after scoring.
fn record_model(report: &mut ScorerReport, scorer: &dyn CandidateScorer) {
    if let Some(model) = scorer.model() {
        report.model = Some(model.id);
        report.model_revision = model.revision;
        report.device = model.device;
    }
}

fn record_unavailable(
    result: &mut LocateResult,
    report: &mut ScorerReport,
    unavailable: ScorerUnavailable,
) {
    report.unavailable = Some(unavailable.reason.to_string());
    record_degradation(
        &mut result.degradations,
        RetrievalDegradation {
            component: SCORER_COMPONENT.to_string(),
            reason: unavailable.reason.to_string(),
            detail: unavailable.detail,
            remediation: unavailable.remediation,
        },
    );
}

/// The scorer verdict of a multi-query fusion: it abstains only when every
/// variant that was scored abstained, because a confident candidate from any
/// one variant is a candidate the fused answer carries.
pub(crate) fn fuse_scorer_reports<'a>(
    reports: impl Iterator<Item = Option<&'a ScorerReport>>,
) -> Option<ScorerReport> {
    let reports: Vec<&ScorerReport> = reports.flatten().collect();
    let first = reports.first()?;
    let mut fused = (*first).clone();
    fused.scored = reports
        .iter()
        .map(|report| report.scored)
        .max()
        .unwrap_or(0);
    fused.top_probability = reports
        .iter()
        .filter_map(|report| report.top_probability)
        .reduce(f32::max);
    fused.abstained = reports.iter().all(|report| report.abstained);
    fused.unavailable = reports.iter().find_map(|report| report.unavailable.clone());
    Some(fused)
}

/// A test-only override so a pipeline test can run the scorer stage with a
/// stub scorer instead of a model.
#[cfg(test)]
pub(crate) mod test_scorer {
    use super::CandidateScorer;
    use std::cell::RefCell;

    thread_local! {
        static NEXT: RefCell<Option<Box<dyn CandidateScorer>>> = const { RefCell::new(None) };
    }

    /// The scorer the next [`super::scorer_for`] on this thread returns.
    pub(crate) fn set(scorer: Box<dyn CandidateScorer>) {
        NEXT.with(|next| *next.borrow_mut() = Some(scorer));
    }

    pub(super) fn take() -> Option<Box<dyn CandidateScorer>> {
        NEXT.with(|next| next.borrow_mut().take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::locate::{LocateIdSpace, LocateMatchKind, LocateProvenance};

    fn row(name: &str) -> LocateEntity {
        LocateEntity {
            entity_id: format!("id-{name}"),
            id_space: LocateIdSpace::Entity,
            artifact_path: None,
            kind: "function".to_string(),
            name: name.to_string(),
            signature: format!("def {name}()"),
            score: 1.0,
            definition: true,
            span: Some([1, 3]),
            body: None,
            match_kind: Some(LocateMatchKind::TextFallback),
            provenance: LocateProvenance {
                file: Some(format!("pkg/{name}.py")),
                origin: "text".to_string(),
                cosine: None,
            },
            matched_queries: Vec::new(),
            collapsed_rows: 0,
            name_tier: None,
            scorer_probability: None,
            proven_callers: None,
            hops_from_root: None,
        }
    }

    fn candidates(result: &LocateResult) -> Vec<ScoringCandidate> {
        result
            .entities
            .iter()
            .map(|entity| ScoringCandidate {
                entity_id: entity.entity_id.clone(),
                name: entity.name.clone(),
                kind: entity.kind.clone(),
                file: entity.provenance.file.clone(),
                text: entity.name.clone(),
            })
            .collect()
    }

    /// Scores rows by a fixed table keyed on the row text.
    pub(crate) struct TableScorer(pub Vec<(&'static str, f32)>);

    impl CandidateScorer for TableScorer {
        fn name(&self) -> &'static str {
            "table"
        }

        fn score(
            &self,
            _query: &str,
            candidates: &[ScoringCandidate],
        ) -> Result<Vec<Option<f32>>, ScorerUnavailable> {
            Ok(candidates
                .iter()
                .map(|c| {
                    self.0
                        .iter()
                        .find(|(text, _)| *text == c.text)
                        .map(|(_, p)| *p)
                })
                .collect())
        }
    }

    const PY: &kin_parser::body_syntax::BodySyntax = &kin_parser::body_syntax::PYTHON;

    fn names(result: &LocateResult) -> Vec<&str> {
        result.entities.iter().map(|e| e.name.as_str()).collect()
    }

    fn config(top_k: usize, threshold: f32) -> ScorerConfig {
        ScorerConfig { top_k, threshold }
    }

    #[test]
    fn the_scorer_reads_code_past_a_docstring() {
        let body = "def make_response(self, rv): \"\"\"Convert the return value from a view \
                    function to a response. :param rv: the return value \"\"\" status = None \
                    if isinstance(rv, tuple): rv, status = rv return self.response_class(rv, status)";
        let doc =
            "Convert the return value from a view\n        function to a response.\n\n        \
                   :param rv: the return value";
        assert_eq!(
            code_past_documentation(body, "def make_response(self, rv)", doc, Some(PY)),
            "status = None if isinstance(rv, tuple): rv, status = rv return \
             self.response_class(rv, status)"
        );
    }

    #[test]
    fn a_decorator_stays_and_documentation_above_the_declaration_goes() {
        assert_eq!(
            code_past_documentation(
                "@property def name(self) -> str: \"\"\"The name.\"\"\" return self._name",
                "def name(self) -> str",
                "The name.",
                Some(PY),
            ),
            "@property return self._name"
        );
        assert_eq!(
            code_past_documentation(
                "/// Returns the count. /// Never negative. #[inline] pub fn count(&self) -> u32 { self.count }",
                "pub fn count(&self) -> u32",
                "Returns the count.",
                None,
            ),
            "{ self.count }"
        );
    }

    #[test]
    fn a_docstring_is_recognized_when_the_graph_holds_no_documentation_for_it() {
        assert_eq!(
            code_past_documentation(
                "def pattern(x) -> str: r\"\"\"A raw \\d docstring.\"\"\" return x",
                "def pattern(x)",
                "",
                Some(PY),
            ),
            "return x"
        );
        // A string the function returns is its code, not its documentation.
        assert_eq!(
            code_past_documentation(
                "def page(): return \"\"\"<html></html>\"\"\"",
                "def page()",
                "",
                Some(PY),
            ),
            "return \"\"\"<html></html>\"\"\""
        );
        // And so is a word of the documentation that the code happens to use.
        assert_eq!(
            code_past_documentation(
                "def run(self): self.run_forever()",
                "def run(self)",
                "run",
                Some(PY)
            ),
            "self.run_forever()"
        );
    }

    #[test]
    fn a_declaration_is_placed_however_the_source_lays_it_out() {
        // The graph's signature is on one line and the source's is not.
        assert_eq!(
            code_past_documentation(
                "def add( a, b, ): \"\"\"Adds.\"\"\" return a + b",
                "def add(a, b)",
                "Adds.",
                Some(PY),
            ),
            "return a + b"
        );
        // The signature names a decorator its own way; the source's is code.
        assert_eq!(
            code_past_documentation(
                "@app.route(\"/\") def index() -> str: return render_template(\"index.html\")",
                "@route def index() -> str",
                "",
                Some(PY),
            ),
            "@app.route(\"/\") return render_template(\"index.html\")"
        );
        assert_eq!(
            find_declaration("x = [1, 2,] def añadir( a, ): pass", "def añadir(a)"),
            Some((12, 29))
        );
    }

    #[test]
    fn a_declaration_that_cannot_be_placed_stays_with_its_code() {
        assert_eq!(
            code_past_documentation(
                "def add(a, b=1): \"\"\"Adds.\"\"\" return a + b",
                "def add(a, b)",
                "Adds.",
                Some(PY),
            ),
            "def add(a, b=1): return a + b"
        );
        assert_eq!(
            code_past_documentation(
                "function add(a, b) { return a + b; }",
                "",
                "",
                Some(&kin_parser::body_syntax::ECMASCRIPT)
            ),
            "function add(a, b) { return a + b; }"
        );
    }

    /// What a language writes comes from its adapter, and a language whose
    /// adapter states nothing keeps its body whole past the declaration: a
    /// Python docstring rule never reaches another language.
    #[test]
    fn the_documentation_skip_is_the_languages_own() {
        let body = "def f(): \"\"\"Docs.\"\"\" return 1";
        assert_eq!(
            code_past_documentation(body, "def f()", "Docs.", Some(PY)),
            "return 1"
        );
        assert_eq!(
            code_past_documentation(body, "def f()", "Docs.", None),
            ": \"\"\"Docs.\"\"\" return 1"
        );
        assert_eq!(
            code_past_documentation(
                "export function add(a, b) { return a + b; }",
                "export function add(a, b)",
                "",
                Some(&kin_parser::body_syntax::ECMASCRIPT),
            ),
            "return a + b; }"
        );
    }

    #[test]
    fn a_candidate_is_one_naming_line_and_then_its_code() {
        use super::super::cards::tests::{card_entity, edge};
        use kin_model::{EntityStore as _, RelationKind};

        let graph = kin_db::InMemoryGraph::new();
        let owner = card_entity("SessionInterface", "src/sessions.py", None);
        let mut entity = card_entity(
            "get_signing_serializer",
            "src/sessions.py",
            Some("Return the serializer that signs the session cookie.\n\nLong notes follow."),
        );
        entity.signature = "def get_signing_serializer(self, app)".to_string();
        entity.metadata.extra.insert(
            kin_parser::extract::EMBEDDING_BODY_PREVIEW_KEY.to_string(),
            serde_json::Value::String(
                "def get_signing_serializer(self, app): \"\"\"Return the serializer that signs \
                 the session cookie. Long notes follow. \"\"\" keys = [app.secret_key] return \
                 URLSafeTimedSerializer(keys)"
                    .to_string(),
            ),
        );
        for held in [&owner, &entity] {
            graph.upsert_entity(held).unwrap();
        }
        graph
            .upsert_relation(&edge(owner.id, entity.id, RelationKind::Contains))
            .unwrap();
        let mut listed = row("get_signing_serializer");
        listed.entity_id = entity.id.to_string();
        // The answer carries no body: the scorer reads the graph's.
        assert!(listed.body.is_none());

        assert_eq!(
            candidate_text(&listed, &graph),
            "function get_signing_serializer | def get_signing_serializer(self, app) | Return \
             the serializer that signs the session cookie. | in SessionInterface at \
             src/sessions.py\nkeys = [app.secret_key] return URLSafeTimedSerializer(keys)"
        );
    }

    #[test]
    fn a_long_body_is_cut_and_a_row_the_graph_does_not_hold_reads_what_the_answer_carries() {
        let mut listed = row("notes");
        listed.kind = "document".to_string();
        listed.body = Some("word ".repeat(SCORER_CODE_CHARS));
        let text = candidate_text(&listed, &kin_db::InMemoryGraph::new());
        let (line, code) = text.split_once('\n').expect("a naming line, then the text");
        assert_eq!(line, "document notes | def notes() | in pkg/notes.py");
        assert_eq!(code.chars().count(), SCORER_CODE_CHARS);
    }

    #[test]
    fn a_confident_scorer_reorders_the_head_and_leaves_the_tail() {
        let mut result = LocateResult {
            entities: vec![row("alpha"), row("beta"), row("gamma"), row("delta")],
            ..LocateResult::default()
        };
        let scorer = TableScorer(vec![("alpha", 0.2), ("beta", 0.9), ("gamma", 0.4)]);
        let candidates = candidates(&result)[..3].to_vec();
        let report = apply_candidate_scorer(
            &mut result,
            "how is it done",
            &scorer,
            &candidates,
            config(3, 0.5),
        );
        assert_eq!(names(&result), vec!["beta", "gamma", "alpha", "delta"]);
        assert_eq!(report.top_probability, Some(0.9));
        assert!(!report.abstained);
        assert_eq!(report.scored, 3);
        assert_eq!(result.entities[0].scorer_probability, Some(0.9));
        assert_eq!(
            result.entities[3].scorer_probability, None,
            "the tail is unscored"
        );
        assert!(result.degradations.is_empty());
    }

    #[test]
    fn no_row_over_the_threshold_abstains_and_keeps_the_rows() {
        let mut result = LocateResult {
            entities: vec![row("alpha"), row("beta")],
            ..LocateResult::default()
        };
        let scorer = TableScorer(vec![("alpha", 0.1), ("beta", 0.3)]);
        let candidates = candidates(&result);
        let report = apply_candidate_scorer(
            &mut result,
            "how is it done",
            &scorer,
            &candidates,
            config(40, 0.5),
        );
        assert!(report.abstained, "{report:?}");
        assert_eq!(report.top_probability, Some(0.3));
        assert_eq!(
            names(&result),
            vec!["beta", "alpha"],
            "rows stay, reordered"
        );
        assert!(
            result.degradations.is_empty(),
            "abstention is the verdict's to report, not a lost capability"
        );
    }

    #[test]
    fn a_name_the_query_spelled_keeps_its_tier_and_blocks_abstention() {
        let mut named = row("parse_config");
        named.match_kind = Some(LocateMatchKind::Name);
        let mut result = LocateResult {
            entities: vec![row("alpha"), named],
            ..LocateResult::default()
        };
        let scorer = TableScorer(vec![("alpha", 0.3), ("parse_config", 0.05)]);
        let candidates = candidates(&result);
        let report = apply_candidate_scorer(
            &mut result,
            "parse_config",
            &scorer,
            &candidates,
            config(40, 0.5),
        );
        assert_eq!(names(&result), vec!["parse_config", "alpha"]);
        assert!(
            !report.abstained,
            "a row the query named is an answer whatever the scorer thinks"
        );
    }

    #[test]
    fn the_control_scorer_changes_nothing() {
        let mut result = LocateResult {
            entities: vec![row("alpha"), row("beta")],
            ..LocateResult::default()
        };
        let candidates = candidates(&result);
        let report =
            apply_candidate_scorer(&mut result, "q", &NoneScorer, &candidates, config(40, 0.5));
        assert_eq!(names(&result), vec!["alpha", "beta"]);
        assert_eq!(report.name, "none");
        assert_eq!(report.top_probability, None);
        assert!(!report.abstained);
        assert!(result
            .entities
            .iter()
            .all(|e| e.scorer_probability.is_none()));
    }

    #[test]
    fn a_scorer_that_cannot_run_is_a_refusing_ledger_entry_not_a_silent_skip() {
        for scorer in [
            Box::new(DecisionEngineScorer::new(
                "kin-test/no-such-decision-model",
                PlattCalibration::default(),
            )) as Box<dyn CandidateScorer>,
            Box::new(CrossEncoderScorer {
                model_id: "kin-test/no-such-reranker".to_string(),
                revision: "main".to_string(),
                calibration: PlattCalibration::default(),
            }),
        ] {
            let mut result = LocateResult {
                entities: vec![row("alpha"), row("beta")],
                ..LocateResult::default()
            };
            let candidates = candidates(&result);
            let report = apply_candidate_scorer(
                &mut result,
                "q",
                scorer.as_ref(),
                &candidates,
                config(40, 0.5),
            );
            assert!(report.unavailable.is_some(), "{report:?}");
            assert!(
                !report.abstained,
                "a scorer that did not run cannot abstain"
            );
            assert_eq!(names(&result), vec!["alpha", "beta"]);
            let entry = result
                .degradations
                .iter()
                .find(|entry| entry.component == SCORER_COMPONENT)
                .expect("the ledger names the scorer that did not run");
            assert!(entry.severity().is_refusing());
            assert!(!entry.remediation.is_empty());
            assert!(
                report.model.is_some(),
                "the report names the model it tried: {report:?}"
            );
        }
    }

    /// A table scorer that names a model.
    struct NamedTableScorer(TableScorer);

    impl CandidateScorer for NamedTableScorer {
        fn name(&self) -> &'static str {
            "decision-engine"
        }

        fn model(&self) -> Option<ScorerModel> {
            Some(ScorerModel {
                id: "org/small-model".to_string(),
                revision: Some("0123abc".to_string()),
                device: Some("Metal".to_string()),
            })
        }

        fn score(
            &self,
            query: &str,
            candidates: &[ScoringCandidate],
        ) -> Result<Vec<Option<f32>>, ScorerUnavailable> {
            self.0.score(query, candidates)
        }
    }

    #[test]
    fn the_report_names_the_model_that_scored_and_each_row_carries_its_probability() {
        let mut result = LocateResult {
            entities: vec![row("alpha"), row("beta")],
            ..LocateResult::default()
        };
        let scorer = NamedTableScorer(TableScorer(vec![("alpha", 0.3), ("beta", 0.8)]));
        let candidates = candidates(&result);
        let report =
            apply_candidate_scorer(&mut result, "q", &scorer, &candidates, config(40, 0.5));
        assert_eq!(report.name, "decision-engine");
        assert_eq!(report.model.as_deref(), Some("org/small-model"));
        assert_eq!(report.model_revision.as_deref(), Some("0123abc"));
        assert_eq!(report.device.as_deref(), Some("Metal"));
        assert_eq!(names(&result), vec!["beta", "alpha"]);
        assert_eq!(result.entities[0].scorer_probability, Some(0.8));
        let wire = serde_json::to_value(&report).unwrap();
        assert_eq!(wire["model"], "org/small-model");
        let control =
            apply_candidate_scorer(&mut result, "q", &NoneScorer, &candidates, config(40, 0.5));
        let wire = serde_json::to_value(&control).unwrap();
        assert!(
            wire.get("model").is_none(),
            "the control runs no model: {wire}"
        );
    }

    #[cfg(feature = "decision")]
    #[test]
    fn a_decision_model_directory_without_a_model_is_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let scorer = DecisionEngineScorer::new(
            dir.path().to_string_lossy().to_string(),
            PlattCalibration::default(),
        );
        let error = scorer
            .score("q", &[])
            .expect_err("an empty directory holds no model");
        assert_eq!(error.reason, "model_not_cached", "{error:?}");
        assert!(scorer.candidate_limit().is_none());
    }

    #[cfg(feature = "decision")]
    #[test]
    fn a_hybrid_backbone_is_refused_by_name_before_it_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"model_type": "qwen3_5", "text_config": {"layer_types": ["linear_attention", "full_attention"]}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("model.safetensors"), b"").unwrap();
        std::fs::write(dir.path().join("tokenizer.json"), b"{}").unwrap();
        let scorer = DecisionEngineScorer::new(
            dir.path().to_string_lossy().to_string(),
            PlattCalibration::default(),
        );
        let error = scorer.score("q", &[]).expect_err("a DeltaNet backbone");
        assert_eq!(error.reason, "model_unsupported", "{error:?}");
        assert!(error.detail.contains("Gated DeltaNet"), "{error:?}");
    }

    /// Both decision models, loaded from the local model cache, put the
    /// candidate that does what the query describes first. Needs
    /// Qwen/Qwen3-Reranker-0.6B, jaredpalmer/kev-0.6b and Qwen/Qwen3-0.6B-Base
    /// in the Hugging Face cache; run with `--ignored`.
    #[cfg(feature = "decision")]
    #[test]
    #[ignore = "loads two 0.6B models from the local Hugging Face cache"]
    fn cached_decision_models_rank_the_described_candidate_first() {
        let candidate = |name: &str, text: &str| ScoringCandidate {
            entity_id: String::new(),
            name: name.to_string(),
            kind: "function".to_string(),
            file: None,
            text: text.to_string(),
        };
        let candidates = vec![
            candidate(
                "render_page",
                "function render_page | fn render_page(page: &Page, out: &mut String) | in src/html.rs",
            ),
            candidate(
                "parse_config",
                "function parse_config | fn parse_config(path: &Path) -> Result<Config> | in \
                 src/config.rs\nlet text = std::fs::read_to_string(path)?;\nOk(toml::from_str(&text)?)",
            ),
            candidate(
                "content_hash",
                "function content_hash | def content_hash(data: bytes) -> str | in kin/hashing.py",
            ),
        ];
        for model in [DEFAULT_DECISION_SCORER_MODEL, "jaredpalmer/kev-0.6b"] {
            let scorer = DecisionEngineScorer::new(model, PlattCalibration::default());
            let scores = scorer
                .score("read the configuration file from disk", &candidates)
                .unwrap_or_else(|error| panic!("{model}: {error:?}"));
            let scores: Vec<f32> = scores.into_iter().map(Option::unwrap).collect();
            assert!(
                scores[1] > scores[0] && scores[1] > scores[2],
                "{model}: {scores:?}"
            );
            let named = scorer.model().unwrap();
            assert_eq!(named.id, model);
            assert!(named.revision.is_some_and(|r| r.len() == 40));
        }
    }

    #[test]
    fn platt_calibration_parses_and_maps_logits_monotonically() {
        let platt = PlattCalibration::parse(" 2.0 , -1.5 ").unwrap();
        assert_eq!(platt.slope, 2.0);
        assert_eq!(platt.intercept, -1.5);
        assert!(PlattCalibration::parse("2.0").is_none());
        assert!(PlattCalibration::parse("x,1").is_none());
        let default = PlattCalibration::default();
        assert_eq!(default.probability(0.0), 0.5);
        assert!(platt.probability(3.0) > platt.probability(1.0));
    }

    #[test]
    fn a_fused_answer_abstains_only_when_every_variant_did() {
        let abstained = ScorerReport {
            name: "cross-encoder".to_string(),
            scored: 40,
            threshold: 0.5,
            top_probability: Some(0.2),
            abstained: true,
            unavailable: None,
            model: Some("BAAI/bge-reranker-base".to_string()),
            model_revision: Some("main".to_string()),
            device: None,
            observed: None,
        };
        let confident = ScorerReport {
            top_probability: Some(0.8),
            abstained: false,
            ..abstained.clone()
        };
        let fused = fuse_scorer_reports([Some(&abstained), Some(&confident)].into_iter()).unwrap();
        assert!(!fused.abstained);
        assert_eq!(fused.top_probability, Some(0.8));
        let both = fuse_scorer_reports([Some(&abstained), Some(&abstained)].into_iter()).unwrap();
        assert!(both.abstained);
        assert!(fuse_scorer_reports([None, None].into_iter()).is_none());
    }
}
