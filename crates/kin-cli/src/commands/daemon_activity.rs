// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! What a repository's daemon is doing right now, as one stable phase.
//!
//! `kin daemon status --json` reports it per daemon so a program can say
//! "Kin is indexing" without reading the daemon's internals. Every fact comes
//! from what Kin already reports: the daemon's `/health` answer (whether it has
//! opened its graph, and which background pass is working and for how long),
//! and the store's own record of a background history derivation, which runs
//! in its own worker beside the daemon. Nothing here is recomputed or guessed,
//! and no field names a path.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::commands::resources::BackgroundPassReport;
use kin_core::history_state::{DerivationState, HistoryStanding};

/// How long one daemon may take to answer `/health` before its activity is
/// reported as unknown. The daemon answers its liveness probe inside its own
/// budget, so this is only ever reached by a daemon that is not answering.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);

/// The daemon is opening its graph and cannot answer yet.
pub const PHASE_STARTING: &str = "starting";
/// The daemon is taking in changes made to the working copy.
pub const PHASE_RECONCILING: &str = "reconciling";
/// The daemon is building an index it derives from the graph.
pub const PHASE_INDEXING: &str = "indexing";
/// Language servers are resolving references between entities.
pub const PHASE_ENRICHING: &str = "enriching";
/// The repository's older history is being read in the background.
pub const PHASE_READING_HISTORY: &str = "reading_history";
/// Nothing is running; the daemon is waiting for changes.
pub const PHASE_IDLE: &str = "idle";
/// The daemon is not running.
pub const PHASE_STOPPED: &str = "stopped";
/// The daemon is alive and did not say what it is doing.
pub const PHASE_UNKNOWN: &str = "unknown";

/// One daemon's current activity: a stable `phase`, when it began if Kin
/// knows, and one plain sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DaemonActivity {
    pub phase: &'static str,
    /// RFC 3339, to the second. Null when Kin does not know, and always null
    /// for `idle`, `stopped` and `unknown`.
    pub since: Option<String>,
    pub detail: String,
}

impl DaemonActivity {
    fn new(phase: &'static str, since: Option<DateTime<Utc>>, detail: impl Into<String>) -> Self {
        Self {
            phase,
            since: since.map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, true)),
            detail: detail.into(),
        }
    }
}

/// The part of a daemon's `/health` answer activity is read from. Every field
/// defaults, so a daemon that predates one still parses.
#[derive(Debug, Default, Deserialize)]
pub struct HealthFacts {
    #[serde(default)]
    pub initialized: Option<bool>,
    #[serde(default)]
    pub uptime_seconds: Option<u64>,
    #[serde(default)]
    pub background_passes: Vec<BackgroundPassReport>,
}

/// A background history derivation that is running now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryFacts {
    pub started_at: Option<DateTime<Utc>>,
    pub step: Option<u32>,
    pub steps: Option<u32>,
    pub commits_total: Option<u64>,
}

/// The running derivation a store records, if one is running. A queued,
/// waiting, failed or staged derivation is not work happening now, and a
/// report whose worker has gone is not either.
pub fn running_history(standing: &HistoryStanding) -> Option<HistoryFacts> {
    let HistoryStanding::Partial {
        progress: Some(progress),
        ..
    } = standing
    else {
        return None;
    };
    (progress.state == DerivationState::Running && progress.worker_is_alive()).then_some(
        HistoryFacts {
            started_at: progress.started_at,
            step: progress.step,
            steps: progress.steps,
            commits_total: progress.commits_total,
        },
    )
}

/// Which background passes map to which phase, in the order a phase wins
/// when several run at once: the daemon's own catch-up first, then the
/// indexes it derives, then enrichment. A pass this build does not know is
/// left out rather than given a phase it may not mean.
const PASS_PHASES: [(&str, &str, &str); 4] = [
    (
        "reconcile",
        PHASE_RECONCILING,
        "Taking in changes made to the working copy.",
    ),
    (
        "projection_rebuild",
        PHASE_INDEXING,
        "Rebuilding the views Kin derives from the graph.",
    ),
    (
        "embed",
        PHASE_INDEXING,
        "Building the search index for semantic search.",
    ),
    (
        "lsp_enrichment",
        PHASE_ENRICHING,
        "Asking language servers to resolve references between entities.",
    ),
];

fn seconds_before(now: DateTime<Utc>, seconds: u64) -> Option<DateTime<Utc>> {
    let seconds = i64::try_from(seconds).ok()?;
    now.checked_sub_signed(chrono::Duration::try_seconds(seconds)?)
}

fn history_detail(history: &HistoryFacts) -> String {
    let mut parts = Vec::new();
    if let (Some(step), Some(steps)) = (history.step, history.steps) {
        parts.push(format!("step {step} of {steps}"));
    }
    if let Some(commits) = history.commits_total {
        parts.push(format!(
            "{commits} commit{}",
            if commits == 1 { "" } else { "s" }
        ));
    }
    if parts.is_empty() {
        "Reading the repository's older history in the background.".to_string()
    } else {
        format!(
            "Reading the repository's older history in the background ({}).",
            parts.join(", ")
        )
    }
}

/// The activity a daemon that answered `/health` reports.
pub fn activity_from(
    health: &HealthFacts,
    history: Option<&HistoryFacts>,
    now: DateTime<Utc>,
) -> DaemonActivity {
    if health.initialized == Some(false) {
        return DaemonActivity::new(
            PHASE_STARTING,
            health
                .uptime_seconds
                .and_then(|uptime| seconds_before(now, uptime)),
            "Opening the repository's graph. Answers wait until it is loaded.",
        );
    }
    for (pass_name, phase, detail) in PASS_PHASES {
        let active = health.background_passes.iter().find(|pass| {
            pass.name == pass_name && matches!(pass.state.as_str(), "working" | "publishing")
        });
        if let Some(pass) = active {
            return DaemonActivity::new(
                phase,
                pass.working_seconds
                    .and_then(|working| seconds_before(now, working)),
                detail,
            );
        }
    }
    if let Some(history) = history {
        return DaemonActivity::new(
            PHASE_READING_HISTORY,
            history.started_at,
            history_detail(history),
        );
    }
    DaemonActivity::new(PHASE_IDLE, None, "Waiting for changes.")
}

/// Whether a daemon can be asked, as `kin daemon status` classified it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reachability {
    /// The process is alive and answered on its port.
    Answering,
    /// The process is alive and did not answer.
    NotAnswering,
    /// The process is gone, or none was recorded.
    Gone,
}

/// The activity of a daemon that could not be asked.
pub fn activity_without_health(
    reachability: Reachability,
    history: Option<&HistoryFacts>,
) -> DaemonActivity {
    // A history derivation runs in its own worker, so it carries on, and is
    // still what the repository is doing, whatever the daemon's state.
    if let Some(history) = history {
        return DaemonActivity::new(
            PHASE_READING_HISTORY,
            history.started_at,
            history_detail(history),
        );
    }
    match reachability {
        Reachability::Gone => {
            DaemonActivity::new(PHASE_STOPPED, None, "This daemon is not running.")
        }
        Reachability::Answering | Reachability::NotAnswering => DaemonActivity::new(
            PHASE_UNKNOWN,
            None,
            "This daemon did not say what it is doing.",
        ),
    }
}

/// Whether `endpoint` is a loopback HTTP address. `/health` takes no
/// credential, and this keeps the status command from ever asking anything
/// off this machine.
fn is_loopback_endpoint(endpoint: &str) -> bool {
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    url.scheme() == "http"
        && match url.host() {
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
            None => false,
        }
}

async fn fetch_health(endpoint: &str) -> anyhow::Result<HealthFacts> {
    anyhow::ensure!(
        is_loopback_endpoint(endpoint),
        "daemon endpoint is not a loopback address"
    );
    let client = reqwest::Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .connect_timeout(Duration::from_secs(1))
        .build()?;
    let url = format!("{}/health", endpoint.trim_end_matches('/'));
    Ok(client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// The running history derivation of the store serving `repo_root`, read from
/// the store's own record.
fn history_for(repo_root: &str) -> Option<HistoryFacts> {
    let layout = kin_core::KinLayout::discover(Path::new(repo_root))?;
    running_history(&kin_core::history_state::standing(&layout))
}

/// What the daemon serving `repo_root` at `endpoint` is doing now.
pub async fn read_activity(
    endpoint: String,
    repo_root: String,
    reachability: Reachability,
) -> DaemonActivity {
    let history = history_for(&repo_root);
    if reachability != Reachability::Answering {
        return activity_without_health(reachability, history.as_ref());
    }
    match fetch_health(&endpoint).await {
        Ok(health) => activity_from(&health, history.as_ref(), Utc::now()),
        Err(_) => activity_without_health(Reachability::NotAnswering, history.as_ref()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_core::history_state::HistoryDerivationProgress;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn health(json: serde_json::Value) -> HealthFacts {
        serde_json::from_value(json).unwrap()
    }

    /// A fixture shaped like a real `/health` answer: the fields activity reads
    /// beside fields it ignores, which must not stop it parsing.
    fn health_fixture(initialized: bool, passes: serde_json::Value) -> HealthFacts {
        health(serde_json::json!({
            "status": "ok",
            "version": "0.8.2",
            "uptime_seconds": 90,
            "graph_entity_count": 1200,
            "graph_loaded": true,
            "reconciliation_status": "idle",
            "repo_id": "repo-1",
            "repo_root": "/not/read/here",
            "pid": 4242,
            "initialized": initialized,
            "background_passes": passes,
            "reconcile": {"skipped_events": 0, "admission_failure_streak": 0, "admission_failures": 0},
            "build": {"sha": "abc", "dirty": false, "built_at": "2026-10-10T00:00:00Z"},
        }))
    }

    #[test]
    fn an_opening_daemon_is_starting_since_it_started() {
        let now = at("2026-10-10T12:00:00Z");
        let activity = activity_from(&health_fixture(false, serde_json::json!([])), None, now);
        assert_eq!(activity.phase, PHASE_STARTING);
        assert_eq!(activity.since.as_deref(), Some("2026-10-10T11:58:30Z"));
    }

    #[test]
    fn each_working_pass_names_its_phase_and_when_it_began() {
        let now = at("2026-10-10T12:00:00Z");
        for (name, phase) in [
            ("reconcile", PHASE_RECONCILING),
            ("projection_rebuild", PHASE_INDEXING),
            ("embed", PHASE_INDEXING),
            ("lsp_enrichment", PHASE_ENRICHING),
        ] {
            let passes = serde_json::json!([
                {"name": name, "state": "working", "progress": 3, "working_seconds": 45},
            ]);
            let activity = activity_from(&health_fixture(true, passes), None, now);
            assert_eq!(activity.phase, phase, "{name}");
            assert_eq!(activity.since.as_deref(), Some("2026-10-10T11:59:15Z"));
            assert!(!activity.detail.contains('/'), "{}", activity.detail);
        }
    }

    /// Catch-up outranks the indexes, which outrank enrichment, and an idle,
    /// deferred or stopped pass is not activity.
    #[test]
    fn the_most_immediate_work_wins_and_quiet_passes_do_not_count() {
        let now = at("2026-10-10T12:00:00Z");
        let passes = serde_json::json!([
            {"name": "lsp_enrichment", "state": "working", "progress": 1, "working_seconds": 600},
            {"name": "embed", "state": "publishing", "progress": 9, "working_seconds": 30},
            {"name": "reconcile", "state": "waiting_deferred", "progress": 0, "deferred_seconds": 90},
        ]);
        assert_eq!(
            activity_from(&health_fixture(true, passes), None, now).phase,
            PHASE_INDEXING
        );

        let quiet = serde_json::json!([
            {"name": "reconcile", "state": "idle", "progress": 4},
            {"name": "embed", "state": "stopped", "progress": 2, "stopped_reason": "no progress"},
            {"name": "some_future_pass", "state": "working", "progress": 0, "working_seconds": 5},
        ]);
        let idle = activity_from(&health_fixture(true, quiet), None, now);
        assert_eq!(idle.phase, PHASE_IDLE);
        assert_eq!(idle.since, None);
    }

    #[test]
    fn a_running_history_derivation_is_reading_history() {
        let now = at("2026-10-10T12:00:00Z");
        let mut progress = HistoryDerivationProgress::new(DerivationState::Running);
        progress.step = Some(3);
        progress.steps = Some(7);
        progress.commits_total = Some(12_000);
        progress.started_at = Some(at("2026-10-10T11:40:00Z"));
        // No pid reads as alive, so this test never depends on a process.
        progress.pid = None;
        let record = kin_core::history_state::HistoryStateRecord::new(
            kin_core::history_state::HistoryMode::Background,
            "0".repeat(40),
            None,
            "change-1".to_string(),
            Vec::new(),
            Some(12_001),
            None,
        );
        let standing = HistoryStanding::Partial {
            record,
            progress: Some(progress.clone()),
        };
        let history = running_history(&standing).expect("a running derivation");
        let activity = activity_from(
            &health_fixture(true, serde_json::json!([])),
            Some(&history),
            now,
        );
        assert_eq!(activity.phase, PHASE_READING_HISTORY);
        assert_eq!(activity.since.as_deref(), Some("2026-10-10T11:40:00Z"));
        assert!(
            activity.detail.contains("step 3 of 7"),
            "{}",
            activity.detail
        );
        assert!(
            activity.detail.contains("12000 commits"),
            "{}",
            activity.detail
        );

        progress.state = DerivationState::Queued;
        assert_eq!(
            running_history(&HistoryStanding::Partial {
                record: match standing {
                    HistoryStanding::Partial { record, .. } => record,
                    _ => unreachable!(),
                },
                progress: Some(progress),
            }),
            None
        );
        assert_eq!(running_history(&HistoryStanding::Complete), None);
    }

    #[test]
    fn a_daemon_that_cannot_be_asked_is_stopped_or_unknown() {
        assert_eq!(
            activity_without_health(Reachability::Gone, None).phase,
            PHASE_STOPPED
        );
        assert_eq!(
            activity_without_health(Reachability::NotAnswering, None).phase,
            PHASE_UNKNOWN
        );
    }

    #[test]
    fn only_loopback_endpoints_are_asked() {
        assert!(is_loopback_endpoint("http://127.0.0.1:4219"));
        assert!(is_loopback_endpoint("http://localhost:4219/"));
        assert!(is_loopback_endpoint("http://[::1]:4219"));
        assert!(!is_loopback_endpoint("http://10.0.0.5:4219"));
        assert!(!is_loopback_endpoint("https://example.com"));
        assert!(!is_loopback_endpoint("not a url"));
    }

    /// The whole read against a served `/health`: the activity comes from what
    /// the daemon answered.
    #[tokio::test]
    async fn read_activity_asks_the_daemon_its_health() {
        let app = axum::Router::new().route(
            "/health",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "status": "ok",
                    "initialized": true,
                    "uptime_seconds": 30,
                    "background_passes": [
                        {"name": "embed", "state": "working", "progress": 0, "working_seconds": 12}
                    ],
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let repo = tempfile::tempdir().unwrap();
        let activity = read_activity(
            format!("http://{address}"),
            repo.path().display().to_string(),
            Reachability::Answering,
        )
        .await;
        assert_eq!(activity.phase, PHASE_INDEXING);
        assert!(activity.since.is_some());

        let value = serde_json::to_value(&activity).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["detail", "phase", "since"]);
    }
}
