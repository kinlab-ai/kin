// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Which retrieval arm a description query runs.
//!
//! A description query (the `semantic_locate` tool, `kin locate`) generates
//! candidates and then, optionally, scores them. This module is the one switch
//! that selects both halves, so a measurement can compare arms on one binary:
//!
//! | value | candidates | scorer |
//! | -- | -- | -- |
//! | `fused` | today's fused pipeline, embeddings included | off |
//! | `cards` | entity cards and graph expansion, no embeddings | off |
//! | `fused+scorer` | fused | the scorer [`KIN_LOCATE_SCORER`](SCORER_ENV) names |
//! | `cards+scorer` | cards | the scorer [`KIN_LOCATE_SCORER`](SCORER_ENV) names |
//!
//! `+scorer` may also be spelled with the scorer itself: `cards+cross-encoder`,
//! `fused+decision-engine`, `cards+none`. The `none` scorer runs the stage and
//! changes nothing, which is the control a scorer measurement needs.
//!
//! The switch is read from `KIN_LOCATE_ARM`, and a repository can record it in
//! `.kin/config.toml` under `[retrieval]`. An explicit environment variable
//! always wins; the config fills in for it at process start, the same way the
//! `[resources]` table does, so the CLI and the daemon it spawns agree.
//!
//! When nothing names an arm, the query picks one at run time from the vectors
//! it can actually use ([`RetrievalArm::for_query`]): `fused` over a store with
//! search vectors, `cards` over one with none, which covers a store never
//! embedded, a build without embeddings and vectors the model identity check
//! refused. An explicit setting is never second-guessed.
//!
//! The same table carries the embedding model a repository embeds with
//! (`embed_model`), which is exported as `KIN_EMBED_MODEL_ID`. A model identity
//! change re-embeds the store through the vector sidecar's existing model
//! check; nothing here downloads anything.

use std::path::Path;

/// The environment variable that selects the arm.
pub const ARM_ENV: &str = "KIN_LOCATE_ARM";
/// The environment variable that selects the scorer a `+scorer` arm runs.
pub const SCORER_ENV: &str = "KIN_LOCATE_SCORER";
/// The embedding model id the embedder reads.
pub const EMBED_MODEL_ENV: &str = "KIN_EMBED_MODEL_ID";
/// The decision model the `decision-engine` scorer runs.
pub const SCORER_MODEL_ENV: &str = "KIN_LOCATE_SCORER_MODEL";

/// The `.kin/config.toml` table the arm is recorded under.
pub const CONFIG_TABLE: &str = "retrieval";

/// Every value `KIN_LOCATE_ARM` accepts. One list, kept in kin-core so the
/// config validation and the env registry read the same values this parses.
pub const ARM_VALUES: &[&str] = kin_core::RETRIEVAL_ARM_NAMES;

/// Every value `KIN_LOCATE_SCORER` accepts.
pub const SCORER_VALUES: &[&str] = kin_core::RETRIEVAL_SCORER_NAMES;

/// The Hugging Face id of CodeRankEmbed, the code retriever the embedding
/// model option accepts in place of the default text model.
pub const CODERANKEMBED_MODEL_ID: &str = "nomic-ai/CodeRankEmbed";

/// Where a description query's candidates come from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CandidateSource {
    /// Today's fused pipeline: lexical, graph and embedding signals.
    #[default]
    Fused,
    /// BM25 over one entity card per entity plus graph expansion from the top
    /// hits, fused by reciprocal rank. No embedding is read or computed.
    Cards,
}

impl CandidateSource {
    /// The wire word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fused => "fused",
            Self::Cards => "cards",
        }
    }
}

/// Which scorer the scoring stage runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScorerKind {
    /// The stage runs and changes nothing: no reorder, never abstains.
    None,
    /// The locally cached cross-encoder reranker, calibrated to a probability.
    CrossEncoder,
    /// The optional decision engine's candidate scoring.
    DecisionEngine,
    /// A configured HTTP reranking provider over graph candidates.
    External,
}

impl ScorerKind {
    /// The wire word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::CrossEncoder => "cross-encoder",
            Self::DecisionEngine => "decision-engine",
            Self::External => "external",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "cross-encoder" => Some(Self::CrossEncoder),
            "decision-engine" => Some(Self::DecisionEngine),
            "external" => Some(Self::External),
            _ => None,
        }
    }
}

/// The scorer a bare `+scorer` arm runs when nothing names one.
pub const DEFAULT_SCORER: ScorerKind = ScorerKind::CrossEncoder;

/// One retrieval arm: where candidates come from, and whether a scorer runs
/// over them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetrievalArm {
    pub candidates: CandidateSource,
    pub scorer: Option<ScorerKind>,
}

impl RetrievalArm {
    /// The fused pipeline with no scorer: today's serving shape, and the arm a
    /// query with search vectors to use runs when nothing names one.
    pub const FUSED: Self = Self {
        candidates: CandidateSource::Fused,
        scorer: None,
    };

    /// Entity cards and the graph with no scorer: the arm a query with no
    /// usable search vectors runs when nothing names one.
    pub const CARDS: Self = Self {
        candidates: CandidateSource::Cards,
        scorer: None,
    };

    /// Parse an arm value. `scorer_default` is what a bare `+scorer` resolves
    /// to, which is `KIN_LOCATE_SCORER` or the built-in default.
    pub fn parse(value: &str, scorer_default: ScorerKind) -> Result<Self, String> {
        let normalized = value.trim().to_ascii_lowercase();
        let (candidates, scorer) = match normalized.split_once('+') {
            Some((candidates, scorer)) => (candidates, Some(scorer)),
            None => (normalized.as_str(), None),
        };
        let candidates = match candidates {
            "fused" => CandidateSource::Fused,
            "cards" => CandidateSource::Cards,
            _ => return Err(unknown_arm(value)),
        };
        let scorer = match scorer {
            None => None,
            Some("scorer") => Some(scorer_default),
            Some(named) => Some(ScorerKind::parse(named).ok_or_else(|| unknown_arm(value))?),
        };
        Ok(Self { candidates, scorer })
    }

    /// The arm a description query runs: the one `KIN_LOCATE_ARM` (or the
    /// `[retrieval] arm` adopted into it) names, and otherwise the automatic
    /// choice for the vectors this query can use.
    ///
    /// Read per query, in the process that runs the locate code, so one binary
    /// can run every arm and a daemon serving a store that gains or loses its
    /// vectors follows it without a restart.
    pub fn for_query(vectors_usable: bool) -> Self {
        Self::from_lookup(vectors_usable, |key| std::env::var(key).ok())
    }

    fn from_lookup(vectors_usable: bool, lookup: impl Fn(&str) -> Option<String>) -> Self {
        let mut arm =
            Self::explicit_from_lookup(&lookup).unwrap_or_else(|| Self::automatic(vectors_usable));
        match kin_core::models::resolve_slot(
            kin_core::models::ModelSlot::Reranking,
            &kin_core::KinConfig::default(),
            &lookup,
        ) {
            Ok(slot) => match slot.provider {
                kin_core::models::ModelProvider::None => {
                    arm.scorer = arm.scorer.map(|_| ScorerKind::None);
                }
                kin_core::models::ModelProvider::External => {
                    arm.scorer = Some(ScorerKind::External)
                }
                kin_core::models::ModelProvider::BuiltIn => {
                    if matches!(
                        arm.scorer,
                        None | Some(ScorerKind::None | ScorerKind::External)
                    ) {
                        arm.scorer = Some(ScorerKind::DecisionEngine);
                    }
                }
            },
            Err(error) => {
                tracing::warn!(%error, "invalid reranking configuration");
                // The scorer reports the configuration failure on the answer.
                arm.scorer = Some(ScorerKind::External);
            }
        }
        arm
    }

    /// The arm nothing named: `fused` when this query has search vectors it can
    /// use, `cards` when it has none. Fusing an embedding signal that cannot
    /// exist would rank on text alone and call that a degraded fused answer; the
    /// cards arm is the ranking built for exactly that setup.
    pub fn automatic(vectors_usable: bool) -> Self {
        if vectors_usable {
            Self::FUSED
        } else {
            Self::CARDS
        }
    }

    /// The arm the environment names, or `None` when it names none.
    ///
    /// A value this build does not know warns and names none, the same rule
    /// `KIN_PROFILE` follows; the startup env audit refuses it before a daemon
    /// serves at all.
    pub fn explicit_from_env() -> Option<Self> {
        Self::explicit_from_lookup(|key| std::env::var(key).ok())
    }

    /// [`Self::explicit_from_env`] over an arbitrary lookup, so a test states
    /// the whole environment it asserts against.
    pub fn explicit_from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let scorer_default = match lookup(SCORER_ENV)
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
        {
            None => DEFAULT_SCORER,
            Some(value) => ScorerKind::parse(&value).unwrap_or_else(|| {
                tracing::warn!(
                    scorer = %value,
                    "unknown {SCORER_ENV} value; using {}",
                    DEFAULT_SCORER.as_str()
                );
                DEFAULT_SCORER
            }),
        };
        let value = lookup(ARM_ENV).filter(|value| !value.trim().is_empty())?;
        match Self::parse(&value, scorer_default) {
            Ok(arm) => Some(arm),
            Err(error) => {
                tracing::warn!(arm = %value, "{error}; choosing the arm automatically");
                None
            }
        }
    }

    /// Canonical name: `cards`, `fused+cross-encoder`, and so on. Recorded on
    /// every answer the arm shapes so a result is attributable to its arm.
    pub fn name(self) -> String {
        match self.scorer {
            None => self.candidates.as_str().to_string(),
            Some(scorer) => format!("{}+{}", self.candidates.as_str(), scorer.as_str()),
        }
    }

    /// Whether this arm reads or computes embeddings.
    pub fn uses_embeddings(self) -> bool {
        self.candidates == CandidateSource::Fused
    }

    /// Whether this is the fused serving shape, which must answer exactly as it
    /// did before the switch existed and so carries none of the fields the
    /// other arms add. Every other arm, the automatic `cards` included, names
    /// itself on its answers.
    pub fn is_default(self) -> bool {
        self == Self::FUSED
    }
}

fn unknown_arm(value: &str) -> String {
    format!(
        "unknown {ARM_ENV} value {value:?}; expected one of {}",
        ARM_VALUES.join(", ")
    )
}

/// Resolve an `embed_model` config value to the model id the embedder reads.
/// `coderankembed` is accepted as a short name for CodeRankEmbed; anything else
/// is taken as a Hugging Face id or a local model directory, exactly as
/// `KIN_EMBED_MODEL_ID` takes it.
pub fn resolve_embed_model(value: &str) -> String {
    let trimmed = value.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "coderankembed" | "coderank" => CODERANKEMBED_MODEL_ID.to_string(),
        "nomic-embed-text-v1.5" => crate::embed_model::DEFAULT_EMBED_MODEL_ID.to_string(),
        _ => trimmed.to_string(),
    }
}

/// Adopt the `[retrieval]` table the repository containing `from` recorded, for
/// every variable the operator has not set.
///
/// Both binaries call this beside the `[resources]` adoption and under the same
/// rule: while the process is still single-threaded, and never over an
/// explicit environment variable. Every failure is silent here; a config that
/// will not parse is reported by the command that loads it for real.
pub fn apply_repository_retrieval_at(from: &Path) -> Vec<(&'static str, String)> {
    let Some(layout) = kin_core::KinLayout::discover(from) else {
        return Vec::new();
    };
    apply_repository_retrieval_for_layout(&layout)
}

/// Adopt retrieval settings from an already selected store without
/// rediscovering its working tree through the ambient registry. Call while the
/// process is still single-threaded, with the same precedence and silent
/// config-failure behavior as [`apply_repository_retrieval_at`].
pub fn apply_repository_retrieval_for_layout(
    layout: &kin_core::KinLayout,
) -> Vec<(&'static str, String)> {
    let Ok(config) = kin_core::KinConfig::load_or_default(&layout.config_path()) else {
        return Vec::new();
    };
    let adopted = repository_retrieval_env(&config.retrieval, |key| std::env::var_os(key));
    for (key, value) in &adopted {
        std::env::set_var(key, value);
    }
    adopted
}

/// The variables a `[retrieval]` table sets, given which are already set.
pub(crate) fn repository_retrieval_env(
    config: &kin_core::RetrievalConfig,
    current: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<(&'static str, String)> {
    let unset = |key: &str| current(key).is_none_or(|value| value.is_empty());
    let mut adopted = Vec::new();
    if let Some(arm) = config
        .arm
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        if unset(ARM_ENV) {
            adopted.push((ARM_ENV, arm.to_ascii_lowercase()));
        }
    }
    if let Some(scorer) = config
        .scorer
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        if unset(SCORER_ENV) {
            adopted.push((SCORER_ENV, scorer.to_ascii_lowercase()));
        }
    }
    if let Some(model) = config
        .scorer_model
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        if unset(SCORER_MODEL_ENV) {
            adopted.push((SCORER_MODEL_ENV, model.to_string()));
        }
    }
    if let Some(model) = config
        .embed_model
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        if unset(EMBED_MODEL_ENV) {
            adopted.push((EMBED_MODEL_ENV, resolve_embed_model(model)));
        }
    }
    adopted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn unset_names_no_arm_and_the_vectors_choose_one() {
        assert_eq!(RetrievalArm::explicit_from_lookup(env_of(&[])), None);
        let with_vectors = RetrievalArm::automatic(true);
        assert_eq!(with_vectors, RetrievalArm::FUSED);
        assert!(with_vectors.is_default());
        assert!(with_vectors.uses_embeddings());
        let without = RetrievalArm::automatic(false);
        assert_eq!(without, RetrievalArm::CARDS);
        assert!(
            !without.is_default(),
            "the automatic cards arm names itself"
        );
        assert!(!without.uses_embeddings());
        assert_eq!(without.name(), "cards");
    }

    /// An explicit arm is authoritative whatever the vectors say: `fused` on a
    /// store with none still runs fused, and `cards` on an embedded store still
    /// runs cards.
    #[test]
    fn an_explicit_arm_outranks_the_automatic_choice() {
        let fused = RetrievalArm::explicit_from_lookup(env_of(&[(ARM_ENV, "fused")]));
        assert_eq!(fused, Some(RetrievalArm::FUSED));
        let cards = RetrievalArm::explicit_from_lookup(env_of(&[(ARM_ENV, " Cards ")]));
        assert_eq!(cards, Some(RetrievalArm::CARDS));
        for vectors_usable in [true, false] {
            assert_eq!(
                fused.unwrap_or_else(|| RetrievalArm::automatic(vectors_usable)),
                RetrievalArm::FUSED
            );
            assert_eq!(
                cards.unwrap_or_else(|| RetrievalArm::automatic(vectors_usable)),
                RetrievalArm::CARDS
            );
        }
    }

    #[test]
    fn every_listed_value_parses_and_names_itself_back() {
        for value in ARM_VALUES {
            let arm = RetrievalArm::parse(value, DEFAULT_SCORER)
                .unwrap_or_else(|error| panic!("{value}: {error}"));
            let named = arm.name();
            let expected = value.replace("+scorer", &format!("+{}", DEFAULT_SCORER.as_str()));
            assert_eq!(named, expected, "{value}");
            assert_eq!(
                RetrievalArm::parse(&named, DEFAULT_SCORER).unwrap(),
                arm,
                "{value} round-trips through its name"
            );
        }
    }

    #[test]
    fn every_listed_scorer_parses() {
        for value in SCORER_VALUES {
            let scorer = ScorerKind::parse(value).unwrap_or_else(|| panic!("{value}"));
            assert_eq!(scorer.as_str(), *value);
        }
    }

    #[test]
    fn canonical_model_slot_selects_and_disables_the_actual_stage() {
        let external = RetrievalArm::from_lookup(
            false,
            env_of(&[
                (ARM_ENV, "cards"),
                ("KIN_MODEL_RERANKING_PROVIDER", "external"),
                ("KIN_MODEL_RERANKING_MODEL", "graph-reranker"),
                ("KIN_MODEL_RERANKING_BASE_URL", "http://localhost:1234/v1"),
            ]),
        );
        assert_eq!(external.name(), "cards+external");
        let disabled = RetrievalArm::from_lookup(
            false,
            env_of(&[
                (ARM_ENV, "cards+decision-engine"),
                ("KIN_MODEL_RERANKING_PROVIDER", "none"),
            ]),
        );
        assert_eq!(disabled.name(), "cards+none");
        let built_in = RetrievalArm::from_lookup(
            false,
            env_of(&[
                ("KIN_MODEL_RERANKING_PROVIDER", "built-in"),
                ("KIN_MODEL_RERANKING_MODEL", "jaredpalmer/kev-0.6b"),
            ]),
        );
        assert_eq!(built_in.name(), "cards+decision-engine");
        for legacy_arm in ["cards+none", "cards+scorer"] {
            let built_in = RetrievalArm::from_lookup(
                false,
                env_of(&[
                    (ARM_ENV, legacy_arm),
                    (SCORER_ENV, "none"),
                    ("KIN_MODEL_RERANKING_PROVIDER", "built-in"),
                ]),
            );
            assert_eq!(built_in.name(), "cards+decision-engine");
        }
        assert_eq!(
            RetrievalArm::from_lookup(false, env_of(&[])),
            RetrievalArm::CARDS
        );
    }

    #[test]
    fn cards_arms_never_use_embeddings() {
        for value in [
            "cards",
            "cards+scorer",
            "cards+none",
            "cards+decision-engine",
        ] {
            let arm = RetrievalArm::parse(value, DEFAULT_SCORER).unwrap();
            assert_eq!(arm.candidates, CandidateSource::Cards);
            assert!(!arm.uses_embeddings(), "{value}");
        }
    }

    #[test]
    fn a_bare_scorer_suffix_takes_the_scorer_variable() {
        let arm = RetrievalArm::explicit_from_lookup(env_of(&[
            (ARM_ENV, "cards+scorer"),
            (SCORER_ENV, "decision-engine"),
        ]))
        .unwrap();
        assert_eq!(arm.scorer, Some(ScorerKind::DecisionEngine));
        let named = RetrievalArm::explicit_from_lookup(env_of(&[
            (ARM_ENV, "fused+none"),
            (SCORER_ENV, "decision-engine"),
        ]))
        .unwrap();
        assert_eq!(
            named.scorer,
            Some(ScorerKind::None),
            "a scorer named in the arm outranks the scorer variable"
        );
        let defaulted =
            RetrievalArm::explicit_from_lookup(env_of(&[(ARM_ENV, "FUSED+SCORER")])).unwrap();
        assert_eq!(defaulted.scorer, Some(ScorerKind::CrossEncoder));
    }

    #[test]
    fn an_unknown_arm_names_none_rather_than_guessing() {
        assert!(RetrievalArm::parse("cards+magic", DEFAULT_SCORER).is_err());
        assert!(RetrievalArm::parse("vectors", DEFAULT_SCORER).is_err());
        assert_eq!(
            RetrievalArm::explicit_from_lookup(env_of(&[(ARM_ENV, "warp-speed")])),
            None
        );
    }

    /// The id this option exports is the one the embedder recognizes for
    /// CodeRankEmbed's own query instruction.
    #[cfg(feature = "embeddings")]
    #[test]
    fn the_coderankembed_id_is_the_one_the_embedder_names() {
        assert_eq!(
            CODERANKEMBED_MODEL_ID,
            kin_db::embed::CODERANKEMBED_MODEL_ID
        );
    }

    #[test]
    fn the_embed_model_option_accepts_coderankembed_by_name_or_id() {
        assert_eq!(resolve_embed_model("coderankembed"), CODERANKEMBED_MODEL_ID);
        assert_eq!(
            resolve_embed_model(" CodeRankEmbed "),
            CODERANKEMBED_MODEL_ID
        );
        assert_eq!(
            resolve_embed_model("nomic-ai/CodeRankEmbed"),
            CODERANKEMBED_MODEL_ID
        );
        assert_eq!(
            resolve_embed_model("nomic-embed-text-v1.5"),
            crate::embed_model::DEFAULT_EMBED_MODEL_ID
        );
        assert_eq!(resolve_embed_model("/models/local"), "/models/local");
    }

    #[test]
    fn a_recorded_table_fills_only_what_the_environment_left_unset() {
        let config = kin_core::RetrievalConfig {
            arm: Some("Cards+Scorer".to_string()),
            scorer: Some("none".to_string()),
            scorer_model: Some("jaredpalmer/kev-0.6b".to_string()),
            embed_model: Some("coderankembed".to_string()),
        };
        let adopted = repository_retrieval_env(&config, |_| None);
        assert_eq!(
            adopted,
            vec![
                (ARM_ENV, "cards+scorer".to_string()),
                (SCORER_ENV, "none".to_string()),
                (SCORER_MODEL_ENV, "jaredpalmer/kev-0.6b".to_string()),
                (EMBED_MODEL_ENV, CODERANKEMBED_MODEL_ID.to_string()),
            ]
        );

        let operator_set = repository_retrieval_env(&config, |key| {
            (key == ARM_ENV).then(|| std::ffi::OsString::from("fused"))
        });
        assert!(
            operator_set.iter().all(|(key, _)| *key != ARM_ENV),
            "an explicit environment variable wins over the repository: {operator_set:?}"
        );
        assert!(
            repository_retrieval_env(&kin_core::RetrievalConfig::default(), |_| None).is_empty()
        );
    }
}
