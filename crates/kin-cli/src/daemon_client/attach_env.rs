// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The behavior environment this command would hand a repository daemon.
//!
//! A repository daemon inherits the environment of the command that spawned it
//! and then adopts its repository's recorded `[resources]` profile, `[retrieval]`
//! table and model slots before it starts any threads. Every command except
//! `kin ui` adopts the same settings for its own repository before it runs, so
//! its raw environment already equals what a daemon spawned from it would hold.
//!
//! `kin ui` does not adopt them, on purpose: it serves several repositories, and
//! one repository's settings must not leak into the daemons of the others. When
//! it attached to a daemon that was already running, a raw comparison reported
//! every value the repository supplied as a divergence, such as
//! `KIN_MODEL_EMBEDDINGS_PROVIDER: cli=(unset) daemon="none"`, and the remedy it
//! printed could not clear it, because a restarted daemon adopts the same
//! values again.
//!
//! [`daemon_behavior_env`] answers the question the attach check asks: what a
//! daemon for this store would run under had this command started it. A
//! divergence from that is one a restart clears, so the warning and its remedy
//! agree. A lever the command states itself still wins over the repository, the
//! same precedence the daemon applies, so a real difference still warns.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

use kin_core::behavior_env::{self, BehaviorEnv};
use kin_core::{KinConfig, KinLayout};

use crate::{resource_profile, retrieval_arm};

/// The behavior environment a daemon for the store at `kin_root` would run
/// under had this process spawned it. A config that cannot be read leaves the
/// process environment as it is, which is the comparison this used to make.
pub(super) fn daemon_behavior_env_for_store(kin_root: &Path) -> BehaviorEnv {
    let layout = KinLayout::new(kin_root.to_path_buf());
    match KinConfig::load_or_default(&layout.config_path()) {
        Ok(config) => daemon_behavior_env(&config, |name| std::env::var(name).ok()),
        Err(_) => behavior_env::snapshot_from_process(),
    }
}

/// The behavior environment `lookup` describes once `config`'s repository
/// settings are adopted over it, in kin-daemon's startup order: the resource
/// profile, the retrieval table, then the model slots, which read the retrieval
/// variables. Each step is the function the daemon's own adoption calls.
///
/// A model configuration that does not resolve leaves the slots as `lookup`
/// states them. A daemon refuses to start on one, so the daemon that is running
/// did not start from it, and the raw values are the most honest comparison.
pub(super) fn daemon_behavior_env<F>(config: &KinConfig, lookup: F) -> BehaviorEnv
where
    F: Fn(&str) -> Option<String>,
{
    let mut adopted: BTreeMap<String, String> = BTreeMap::new();
    if let Some(profile) = resource_profile::repository_profile_to_adopt(
        config.resources.normalized_profile().as_deref(),
        &lookup,
    ) {
        adopted.insert(resource_profile::RESOURCE_PROFILE_ENV.to_string(), profile);
    }
    let retrieval = retrieval_arm::repository_retrieval_env(&config.retrieval, |name| {
        read(&adopted, &lookup, name).map(OsString::from)
    });
    adopted.extend(
        retrieval
            .into_iter()
            .map(|(name, value)| (name.to_string(), value)),
    );
    if let Ok(models) =
        kin_core::models::repository_model_env(config, |name| read(&adopted, &lookup, name))
    {
        adopted.extend(models);
    }
    behavior_env::snapshot_with(|name| read(&adopted, &lookup, name))
}

fn read(
    adopted: &BTreeMap<String, String>,
    lookup: &impl Fn(&str) -> Option<String>,
    name: &str,
) -> Option<String> {
    adopted.get(name).cloned().or_else(|| lookup(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_core::behavior_env::compare;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    fn config(text: &str) -> KinConfig {
        toml::from_str(text).unwrap()
    }

    fn reported(pairs: &[(&str, &str)]) -> BehaviorEnv {
        behavior_env::snapshot_with(env(pairs))
    }

    /// What every kin process states before the `kin ui` branch: kin's own
    /// resource profile default, marked as one nobody typed, plus the two
    /// levers the live report carried on both sides.
    const UI_PROCESS: &[(&str, &str)] = &[
        ("KIN_RESOURCE_PROFILE", "interactive"),
        ("KIN_RESOURCE_PROFILE_PRODUCT_DEFAULT", "interactive"),
        ("KIN_EMBED_BACKEND", "cpu"),
        ("KIN_DAEMON_AUTO_EMBED", "0"),
    ];

    /// What the daemon reported in the live report: the same levers, plus the
    /// four providers its model adoption wrote for a default config.
    const DEFAULT_CONFIG_DAEMON: &[(&str, &str)] = &[
        ("KIN_RESOURCE_PROFILE", "interactive"),
        ("KIN_EMBED_BACKEND", "cpu"),
        ("KIN_DAEMON_AUTO_EMBED", "0"),
        ("KIN_MODEL_EMBEDDINGS_PROVIDER", "none"),
        ("KIN_MODEL_RERANKING_PROVIDER", "none"),
        ("KIN_MODEL_ANSWER_COMPLETENESS_PROVIDER", "none"),
        ("KIN_MODEL_CALL_TARGET_PROVIDER", "none"),
    ];

    #[test]
    fn a_ui_attach_to_a_default_config_daemon_reports_no_divergence() {
        let ui = env(UI_PROCESS);
        let daemon = reported(DEFAULT_CONFIG_DAEMON);
        assert_eq!(
            compare(&behavior_env::snapshot_with(&ui), &daemon)
                .iter()
                .map(|d| d.var.as_str())
                .collect::<Vec<_>>(),
            [
                "KIN_MODEL_EMBEDDINGS_PROVIDER",
                "KIN_MODEL_RERANKING_PROVIDER",
                "KIN_MODEL_ANSWER_COMPLETENESS_PROVIDER",
                "KIN_MODEL_CALL_TARGET_PROVIDER",
            ],
            "the raw comparison is the reported defect"
        );
        let divergences = compare(&daemon_behavior_env(&KinConfig::default(), ui), &daemon);
        assert!(divergences.is_empty(), "{divergences:?}");
    }

    #[test]
    fn a_provider_the_ui_sets_that_the_daemon_lacks_still_diverges() {
        let mut stated = UI_PROCESS.to_vec();
        stated.push(("KIN_MODEL_EMBEDDINGS_PROVIDER", "built-in"));
        let divergences = compare(
            &daemon_behavior_env(&KinConfig::default(), env(&stated)),
            &reported(DEFAULT_CONFIG_DAEMON),
        );
        assert_eq!(divergences.len(), 1, "{divergences:?}");
        assert_eq!(
            divergences[0].describe(),
            "KIN_MODEL_EMBEDDINGS_PROVIDER: cli=\"built-in\" daemon=\"none\""
        );
    }

    #[test]
    fn repository_settings_the_ui_did_not_adopt_match_the_daemon_that_did() {
        let config = config(
            "[resources]\nprofile = 'Throughput'\n\
             [retrieval]\nscorer = 'cross-encoder'\n\
             [models.embeddings]\nprovider = 'external'\nmodel = 'm'\n\
             base_url = 'http://localhost:9/v1'\n",
        );
        let mut daemon = DEFAULT_CONFIG_DAEMON.to_vec();
        daemon.retain(|(name, _)| {
            !matches!(
                *name,
                "KIN_RESOURCE_PROFILE"
                    | "KIN_MODEL_EMBEDDINGS_PROVIDER"
                    | "KIN_MODEL_RERANKING_PROVIDER"
            )
        });
        daemon.extend([
            ("KIN_RESOURCE_PROFILE", "throughput"),
            ("KIN_LOCATE_SCORER", "cross-encoder"),
            ("KIN_MODEL_EMBEDDINGS_PROVIDER", "external"),
            ("KIN_MODEL_EMBEDDINGS_MODEL", "m"),
            ("KIN_MODEL_EMBEDDINGS_BASE_URL", "http://localhost:9/v1"),
            ("KIN_MODEL_RERANKING_PROVIDER", "built-in"),
        ]);
        let divergences = compare(
            &daemon_behavior_env(&config, env(UI_PROCESS)),
            &reported(&daemon),
        );
        assert!(divergences.is_empty(), "{divergences:?}");
        // An operator's own profile still outranks the repository's, as it does
        // in the daemon, so it is reported against a daemon that holds the
        // repository's.
        let mut operator = UI_PROCESS.to_vec();
        operator.retain(|(name, _)| !name.starts_with("KIN_RESOURCE_PROFILE"));
        operator.push(("KIN_RESOURCE_PROFILE", "proof"));
        let divergences = compare(
            &daemon_behavior_env(&config, env(&operator)),
            &reported(&daemon),
        );
        assert_eq!(
            divergences
                .iter()
                .map(|d| d.var.as_str())
                .collect::<Vec<_>>(),
            ["KIN_RESOURCE_PROFILE"]
        );
    }

    /// Every other command adopted its repository's settings before it ran, so
    /// the view of its environment is the environment itself, and its attach
    /// comparison is exactly the raw one it made before.
    #[test]
    fn a_command_that_already_adopted_its_repository_compares_as_before() {
        let config = config(
            "[resources]\nprofile = 'throughput'\n\
             [retrieval]\narm = 'cards'\n\
             [models.reranking]\nprovider = 'built-in'\n",
        );
        let adopted: &[(&str, &str)] = &[
            ("KIN_RESOURCE_PROFILE", "throughput"),
            ("KIN_RESOURCE_PROFILE_REPOSITORY_CONFIG", "throughput"),
            ("KIN_EMBED_BACKEND", "cpu"),
            ("KIN_LOCATE_ARM", "cards"),
            ("KIN_MODEL_EMBEDDINGS_PROVIDER", "none"),
            ("KIN_MODEL_RERANKING_PROVIDER", "built-in"),
            ("KIN_MODEL_ANSWER_COMPLETENESS_PROVIDER", "none"),
            ("KIN_MODEL_CALL_TARGET_PROVIDER", "none"),
        ];
        assert_eq!(
            daemon_behavior_env(&config, env(adopted)),
            behavior_env::snapshot_with(env(adopted))
        );
        assert_eq!(
            daemon_behavior_env(&KinConfig::default(), env(DEFAULT_CONFIG_DAEMON)),
            reported(DEFAULT_CONFIG_DAEMON)
        );
    }

    #[test]
    fn a_model_setting_that_does_not_resolve_is_compared_as_stated() {
        let mut stated = UI_PROCESS.to_vec();
        stated.push(("KIN_MODEL_EMBEDDINGS_PROVIDER", "sideways"));
        let divergences = compare(
            &daemon_behavior_env(&KinConfig::default(), env(&stated)),
            &reported(DEFAULT_CONFIG_DAEMON),
        );
        assert_eq!(
            divergences[0].describe(),
            "KIN_MODEL_EMBEDDINGS_PROVIDER: cli=\"sideways\" daemon=\"none\""
        );
    }
}
