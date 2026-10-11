// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Whether language-server enrichment runs for a repository, and who switched
//! it off when it does not.
//!
//! Two switches turn it off. `KIN_DAEMON_DISABLE_LSP` turns it off for the
//! daemons one process starts, and only those. The repository's own
//! `lsp.enabled` store setting turns it off for every daemon that serves the
//! store, whoever starts it: `kin init --no-enrich` records it, and
//! `kin daemon sweep --enable` records it back on. Before the setting was read,
//! `--no-enrich` skipped only the sweep inside `kin init`, and the first query
//! afterwards started a daemon that queued the same sweep at start-up, so
//! "off" meant "later" and the queries behind that sweep waited on it.
//!
//! Every reader asks here, so the daemon that obeys the setting and every
//! sentence that explains it answer from one reading.

use std::path::Path;

use kin_daemon_spawn::EnrichmentState;

use crate::{KinConfig, KinLayout, Result};

/// The command that turns enrichment back on for a repository whose own
/// setting switched it off, in the words every surface names it with.
pub const TURN_ON_COMMAND: &str = "kin daemon sweep --enable";

/// Why enrichment is off when the repository's own setting is the reason, as
/// one clause a sentence can carry.
pub const OFF_FOR_REPOSITORY: &str =
    "language-server enrichment is off for this repository by its own setting";

/// Whether the repository's own setting lets enrichment run.
///
/// A store with no `[lsp]` section, or none of its own settings, runs it: that
/// is the default every store had before the setting was read. A setting this
/// build cannot parse also reads as on, which is what the daemon does with the
/// rest of an unreadable configuration, so the switch a daemon obeys and the
/// sentence that describes it cannot disagree over a store whose settings
/// drifted.
pub fn repository_enabled(layout: &KinLayout) -> bool {
    KinConfig::load_or_default(&layout.config_path()).map_or(true, |config| config.lsp.enabled)
}

/// How enrichment stands for the store at `kin_root`, as the remedies that
/// mention it need to know.
pub fn state_for_store(kin_root: &Path) -> EnrichmentState {
    state_for(&KinLayout::new(kin_root.to_path_buf()))
}

/// How enrichment stands for `layout`: this process's environment switch
/// first, so every sentence keyed on it keeps its wording, then the
/// repository's own setting.
pub fn state_for(layout: &KinLayout) -> EnrichmentState {
    state_given(
        kin_daemon_spawn::enrichment_disabled(),
        repository_enabled(layout),
    )
}

/// [`state_for`] as a pure rule over its two readings.
pub fn state_given(environment_disabled: bool, repository_enabled: bool) -> EnrichmentState {
    if environment_disabled {
        EnrichmentState::AlreadyDisabled
    } else if !repository_enabled {
        EnrichmentState::OffForRepository
    } else {
        EnrichmentState::Available
    }
}

/// The relation-census cause for a repository switched off by its own
/// setting, or `None` when the setting lets enrichment run.
///
/// A census names a cause beside a lost relation kind, and the language-server
/// kinds are what a switched-off repository stops producing. A store reused
/// from an enriched one and then switched off is the case that loses them.
pub fn census_cause(layout: &KinLayout) -> Option<String> {
    (!repository_enabled(layout)).then(|| {
        format!(
            "{OFF_FOR_REPOSITORY} (lsp.enabled = false), so no language-server sweep runs; \
             `{TURN_ON_COMMAND}` turns it on"
        )
    })
}

/// Record whether enrichment runs for a published repository, and say whether
/// the setting changed.
///
/// Leaves the store alone when it already holds the value, so a caller that
/// records the setting it found never takes the projection lock for nothing.
/// Written under the same projection lock the other store-setting writers
/// take, because the settings are part of what an exact eject hands over.
pub fn record(layout: &KinLayout, enabled: bool) -> Result<bool> {
    if repository_enabled(layout) == enabled {
        return Ok(false);
    }
    let projection_freeze = crate::ExactProjectionFreeze::acquire_existing(layout.working_dir())?;
    let changed = record_unfrozen(layout, enabled)?;
    drop(projection_freeze);
    Ok(changed)
}

/// Record the setting in a store copy that is not published yet, such as a
/// prepared store being copied for reuse, where no projection exists to lock.
pub fn record_unpublished(layout: &KinLayout, enabled: bool) -> Result<bool> {
    record_unfrozen(layout, enabled)
}

fn record_unfrozen(layout: &KinLayout, enabled: bool) -> Result<bool> {
    let path = layout.config_path();
    let mut config = KinConfig::load_or_default(&path)?;
    if config.lsp.enabled == enabled {
        return Ok(false);
    }
    config.lsp.enabled = enabled;
    config.save(&path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, KinLayout) {
        let dir = tempfile::tempdir().unwrap();
        let result = crate::init(dir.path()).unwrap();
        (dir, result.layout)
    }

    /// A new store runs enrichment, and recording it off is read back by every
    /// reader here, then recording it on again restores the default.
    #[test]
    fn the_setting_round_trips_through_the_store() {
        let (_dir, layout) = store();
        assert!(repository_enabled(&layout), "a new store enriches");
        assert_eq!(census_cause(&layout), None);

        assert!(record(&layout, false).unwrap(), "off is a change");
        assert!(!repository_enabled(&layout));
        assert!(
            !KinConfig::load(&layout.config_path()).unwrap().lsp.enabled,
            "the typed config the daemon reads holds the same value"
        );
        assert!(
            census_cause(&layout).is_some_and(|cause| cause.contains(TURN_ON_COMMAND)),
            "the census names the setting and the way back on"
        );
        assert!(
            !record(&layout, false).unwrap(),
            "recording it again changes nothing"
        );

        assert!(record(&layout, true).unwrap());
        assert!(repository_enabled(&layout));
        assert_eq!(census_cause(&layout), None);
    }

    /// The environment switch keeps its own wording wherever it is set, and the
    /// repository's setting is named only where it is the reason.
    #[test]
    fn the_environment_switch_is_named_before_the_repository_setting() {
        assert_eq!(state_given(true, true), EnrichmentState::AlreadyDisabled);
        assert_eq!(state_given(true, false), EnrichmentState::AlreadyDisabled);
        assert_eq!(state_given(false, false), EnrichmentState::OffForRepository);
        assert_eq!(state_given(false, true), EnrichmentState::Available);
    }
}
