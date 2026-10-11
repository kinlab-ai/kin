// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use anyhow::{Context, Result};
use kin_core::registry::{KinRegistry, RegisteredRepo};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

pub const DEPS_SCHEMA: &str = "kin.deps.v1";

/// One direction of one repository's recorded cross-repo edges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DependencyEdge {
    pub repo_id: String,
    pub source: String,
}

/// The cross-repo neighbourhood of one registered repository.
///
/// `depends_on` is what this repository's own records name as providers.
/// `consumers` is the inverse: every registered repository whose records name
/// this one as a provider. Both come from records ingestion wrote, so the
/// inverse direction is recorded truth rather than a re-derivation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoDependencyView {
    pub schema: String,
    pub repo_id: String,
    pub depends_on: Vec<DependencyEdge>,
    pub consumers: Vec<DependencyEdge>,
    /// This repository's registration was written before any build recorded
    /// dependencies, so an empty `depends_on` cannot be read as none.
    pub depends_on_predates_recording: bool,
    /// At least one OTHER registered repository's registration was, so an empty
    /// `consumers` cannot be read as none either. A separate fact because the
    /// two directions are answered by different entries' records.
    pub consumers_predate_recording: bool,
    pub finalized_at: String,
    pub manifests_recorded_at: Option<String>,
    pub refresh_gap: Option<String>,
    pub refresh_command: String,
}

/// A manifest refresh asks the daemon that already owns this repository's tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepsRequest {
    pub repository_id: String,
    pub registry_repo_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepsResponse {
    pub repository_id: String,
    pub dependencies: Vec<kin_core::dependencies::RepoDependency>,
    pub observed_at: String,
}

/// Read current held manifests without opening authority or scanning source.
pub fn build_deps_response(
    tree: &kin_model::ResolvedTree,
    held: &kin_core::LocalRepositoryAuthorityBinding,
    request: &DepsRequest,
) -> Result<DepsResponse> {
    anyhow::ensure!(
        held.repository_id().as_str() == request.repository_id,
        "dependency refresh repository identity does not match the pinned authority"
    );
    let content = kin_core::dependencies::HeldTree::new(tree, held).manifest_content()?;
    Ok(DepsResponse {
        repository_id: request.repository_id.clone(),
        dependencies: kin_core::dependencies::manifest_dependencies(
            &content,
            &request.registry_repo_ids,
        ),
        observed_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Replace only manifest-derived edges for the exact registered location.
/// Heuristic and inverse edges keep their admission-time provenance.
fn record_manifest_refresh(
    registry: &mut KinRegistry,
    path: &std::path::Path,
    response: &DepsResponse,
) -> Result<()> {
    let id = registered_id_for_path(registry, path)?;
    anyhow::ensure!(
        id == response.repository_id,
        "dependency refresh came from a different repository"
    );
    anyhow::ensure!(
        response
            .dependencies
            .iter()
            .all(|dep| matches!(dep.source.as_str(), "cargo" | "npm" | "go")),
        "manifest refresh contained a non-manifest dependency"
    );
    let row = registry
        .repos
        .iter_mut()
        .find(|row| row.path == path && row.id == response.repository_id)
        .expect("unique registered location was checked above");
    row.dependencies
        .retain(|dep| !matches!(dep.source.as_str(), "cargo" | "npm" | "go"));
    row.dependencies
        .extend(response.dependencies.iter().cloned());
    row.dependencies
        .sort_by(|a, b| (&a.name, &a.source).cmp(&(&b.name, &b.source)));
    row.dependencies
        .dedup_by(|a, b| a.name == b.name && a.source == b.source);
    row.dependencies_manifest_recorded_at = Some(response.observed_at.clone());
    Ok(())
}

/// A stale recorded answer is allowed only when serving cannot start, never
/// when a compatible serving daemon refused the query or the store is invalid.
fn dependency_daemon_resolution(
    resolution: Result<Option<String>>,
) -> Result<(Option<String>, Option<String>)> {
    match resolution {
        Ok(Some(url)) => Ok((Some(url), None)),
        Ok(None) => Ok((
            None,
            Some("the repository daemon is disabled and is not running".to_string()),
        )),
        Err(error) => {
            use crate::daemon_client::AutoStartError;
            if matches!(
                error.downcast_ref::<AutoStartError>(),
                Some(AutoStartError::IncompatibleStore(_) | AutoStartError::BehaviorEnvIgnored(_))
            ) {
                return Err(error);
            }
            Ok((
                None,
                Some(format!("the repository daemon could not start: {error:#}")),
            ))
        }
    }
}

/// Refresh the current repository's held manifests through its daemon. Fleet
/// and inverse views remain recorded data; they never scan sibling checkouts.
pub async fn run(all: bool, json: bool) -> Result<()> {
    let mut registry =
        KinRegistry::load().map_err(|e| anyhow::anyhow!("failed to load registry: {e}"))?;
    if all {
        return report_every_registered_repo(&registry, json);
    }
    let layout = crate::commands::require_repository_layout()?;
    let repo_id = registered_id_for_path(&registry, layout.working_dir())?.to_owned();
    let request = DepsRequest {
        repository_id: repo_id.clone(),
        registry_repo_ids: registry.repos.iter().map(|row| row.id.clone()).collect(),
    };
    let (url, refresh_gap) =
        dependency_daemon_resolution(crate::daemon_client::resolve_daemon_url(&layout).await)?;
    if let Some(url) = url {
        let client = crate::daemon_client::DaemonClient::from_base_url(url)?;
        // A serving daemon's refusal (including missing held content) is an
        // error. It must never be disguised as a successful recorded answer.
        let response = client
            .dependencies(&request)
            .await
            .context("daemon dependency refresh failed")?;
        registry = KinRegistry::update(|registry| -> Result<KinRegistry> {
            record_manifest_refresh(registry, layout.working_dir(), &response)?;
            Ok(KinRegistry {
                repos: registry.repos.clone(),
            })
        })
        .map_err(|error| anyhow::anyhow!("record held dependencies: {error}"))??;
    }
    let selected = registry
        .repos
        .iter()
        .find(|row| row.id == repo_id && row.path == layout.working_dir())
        .context("the refreshed repository location is no longer registered")?;
    let mut view = repo_dependency_view_for_row(&registry, selected);
    view.refresh_gap = refresh_gap;
    if json {
        println!("{}", serde_json::to_string_pretty(&view)?);
    } else {
        for line in render_repo_view_lines(&view) {
            println!("{line}");
        }
    }
    Ok(())
}

/// Resolve the registry entry for the repository the caller is standing in.
///
/// Registration names an entry after the repository's directory, not after the
/// manifest's repository id, and one host routinely carries several checkouts
/// sharing a directory name. The recorded path is the only field that
/// identifies an entry unambiguously, so that is what this matches on. A miss
/// refuses and says which path it looked for rather than falling back to a
/// same-named entry that describes some other checkout.
pub fn registered_id_for_path<'a>(
    registry: &'a KinRegistry,
    working_dir: &std::path::Path,
) -> Result<&'a str> {
    let matches: Vec<&RegisteredRepo> = registry
        .repos
        .iter()
        .filter(|repo| repo.path == working_dir)
        .collect();

    match matches.as_slice() {
        [only] => Ok(only.id.as_str()),
        [] => {
            let same_name = working_dir
                .file_name()
                .map(|name| {
                    registry
                        .repos
                        .iter()
                        .filter(|repo| repo.path.file_name() == Some(name))
                        .count()
                })
                .unwrap_or(0);
            let named = if same_name > 0 {
                format!(
                    " The registry holds {same_name} entry(s) with the same directory name \
                     recorded at other paths, which describe other checkouts and not this one."
                )
            } else {
                String::new()
            };
            anyhow::bail!(
                "no registry entry records the repository at {};{named} `kin init` registers a \
                 repository when it runs, so this one either predates that registration or was \
                 never initialized with `kin init` here; `kin registry` only lists and cleans \
                 rather than adding an entry after the fact. Use `kin deps --all` to list the \
                 repositories that are registered",
                working_dir.display()
            )
        }
        several => anyhow::bail!(
            "{} registry entries record the repository at {} ({}); refusing to pick one, run \
             `kin registry clean` and re-register it",
            several.len(),
            working_dir.display(),
            several
                .iter()
                .map(|repo| repo.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Build the recorded cross-repo neighbourhood of `repo_id`.
///
/// Refuses rather than reporting an empty neighbourhood when the registry has
/// no record for `repo_id`: an unregistered repository has no recorded edges at
/// all, and printing "no dependencies" for it would read as a proved absence.
pub fn repo_dependency_view(registry: &KinRegistry, repo_id: &str) -> Result<RepoDependencyView> {
    let selected = registry
        .repos
        .iter()
        .find(|repo| repo.id == repo_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "repository '{repo_id}' is not in the Kin registry, so no cross-repo dependency \
                 records exist for it. `kin init` registers a repository when it runs; this one \
                 either predates that registration or was never initialized with `kin init` \
                 here, and `kin registry` only lists and cleans rather than adding an entry \
                 after the fact. Use `kin deps --all` to list the repositories that are \
                 registered"
            )
        })?;

    Ok(repo_dependency_view_for_row(registry, selected))
}

fn repo_dependency_view_for_row(
    registry: &KinRegistry,
    selected: &RegisteredRepo,
) -> RepoDependencyView {
    let known_ids: HashSet<String> = registry.repos.iter().map(|r| r.id.clone()).collect();
    let repo_id = selected.id.as_str();
    let depends_on = dependencies_for_repo(selected, &known_ids)
        .into_iter()
        .map(|(repo_id, dep_type)| DependencyEdge {
            repo_id,
            source: dep_type.0,
        })
        .collect();

    let mut consumers: Vec<DependencyEdge> = registry
        .repos
        .iter()
        .filter(|candidate| candidate.id != repo_id)
        .flat_map(|candidate| {
            candidate.dependencies.iter().filter_map(move |dep| {
                (dep.provider_repo.as_deref() == Some(repo_id)).then(|| DependencyEdge {
                    repo_id: candidate.id.clone(),
                    source: dep.source.clone(),
                })
            })
        })
        .collect();
    consumers.sort_by(|a, b| (&a.repo_id, &a.source).cmp(&(&b.repo_id, &b.source)));
    consumers.dedup();

    RepoDependencyView {
        schema: DEPS_SCHEMA.to_string(),
        repo_id: repo_id.to_string(),
        depends_on,
        consumers,
        finalized_at: selected.last_commit.clone(),
        manifests_recorded_at: selected.dependencies_manifest_recorded_at.clone(),
        refresh_gap: None,
        refresh_command: "kin deps".to_string(),
        depends_on_predates_recording: selected.dependencies_recorded_by.is_none(),
        consumers_predate_recording: registry
            .repos
            .iter()
            .any(|other| other.id != repo_id && other.dependencies_recorded_by.is_none()),
    }
}

/// Render one repository's neighbourhood, saying plainly that an empty
/// direction may be unrecorded rather than letting it read as a proved absence.
pub fn render_repo_view_lines(view: &RepoDependencyView) -> Vec<String> {
    let mut lines = vec![
        format!("Cross-repo dependencies for {}:", view.repo_id),
        String::new(),
    ];

    lines.push(format!("  depends on ({}):", view.depends_on.len()));
    if view.depends_on.is_empty() {
        lines.push("    (no cross-repo dependencies recorded)".to_string());
    } else {
        for edge in &view.depends_on {
            lines.push(format!(
                "    -> {} ({})",
                edge.repo_id,
                DepType::from_source(&edge.source)
            ));
        }
    }
    lines.push(String::new());

    lines.push(format!("  consumed by ({}):", view.consumers.len()));
    if view.consumers.is_empty() {
        lines.push("    (no registered repository records this one as a provider)".to_string());
    } else {
        for edge in &view.consumers {
            lines.push(format!(
                "    <- {} ({})",
                edge.repo_id,
                DepType::from_source(&edge.source)
            ));
        }
    }

    // Keyed on which registrations recorded a build, not on the emptiness
    // itself. An empty direction written by a build that records dependencies
    // is a measured zero and saying it "may be unrecorded" makes the reader
    // doubt a correct answer.
    let unrecorded_direction = (view.depends_on.is_empty() && view.depends_on_predates_recording)
        || (view.consumers.is_empty() && view.consumers_predate_recording);
    if unrecorded_direction {
        lines.push(String::new());
        lines.push(
            "note: dependency records are written when `kin init` registers a repository, and a \
             registration here predates that, so an empty direction above may mean unrecorded \
             rather than none."
                .to_string(),
        );
    }
    if let Some(reason) = &view.refresh_gap {
        lines.push(format!(
            "Recorded view: {reason}. Finalization: {}; held manifests: {}. Run `{}` to refresh.",
            view.finalized_at,
            view.manifests_recorded_at
                .as_deref()
                .unwrap_or("not recorded"),
            view.refresh_command
        ));
    } else if let Some(recorded) = &view.manifests_recorded_at {
        lines.push(format!("Held manifests recorded at {recorded}; source heuristics and consumers remain recorded views."));
    }
    lines.push("Use `kin deps --all` for every registered repository.".to_string());
    lines
}

fn report_every_registered_repo(registry: &KinRegistry, json: bool) -> Result<()> {
    if registry.repos.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "schema": DEPS_SCHEMA,
                    "repositories": [],
                }))?
            );
        } else {
            println!("No registered repositories.");
            println!(
                "hint: `kin init` registers a repository when it runs; a repository \
                 initialized before this build predates that and has no entry"
            );
        }
        return Ok(());
    }

    let known_ids: HashSet<String> = registry.repos.iter().map(|r| r.id.clone()).collect();

    let mut repo_deps: BTreeMap<String, Vec<(String, DepType)>> = BTreeMap::new();
    for repo in &registry.repos {
        repo_deps.insert(repo.id.clone(), dependencies_for_repo(repo, &known_ids));
    }

    if json {
        let repositories: Vec<serde_json::Value> = repo_deps
            .iter()
            .map(|(id, deps)| {
                serde_json::json!({
                    "repo_id": id,
                    "depends_on": deps
                        .iter()
                        .map(|(repo_id, dep_type)| serde_json::json!({
                            "repo_id": repo_id,
                            "source": dep_type.0,
                        }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": DEPS_SCHEMA,
                "repositories": repositories,
            }))?
        );
        return Ok(());
    }

    println!("Cross-repo dependencies:\n");
    // Which registrations were written by a build that records dependencies at
    // all. The caveat below keys on this and never on an empty listing: with
    // axum and flask both registered minutes earlier by the same 0.6.0 binary,
    // keying on emptiness named them both as possibly predating that binary.
    let predates_recording: HashSet<&str> = registry
        .repos
        .iter()
        .filter(|repo| repo.dependencies_recorded_by.is_none())
        .map(|repo| repo.id.as_str())
        .collect();
    let mut unrecorded = Vec::new();
    for (id, deps) in &repo_deps {
        println!("  {}", id);
        if deps.is_empty() {
            println!("    (no cross-repo dependencies recorded)");
            if predates_recording.contains(id.as_str()) {
                unrecorded.push(id.clone());
            }
        } else {
            for (name, dep_type) in deps {
                println!("    -> {} ({})", name, dep_type);
            }
        }
        println!();
    }

    if !unrecorded.is_empty() {
        println!(
            "note: dependency records are written when `kin init` registers a repository, and \
             these registrations predate that, so their empty listing may mean unrecorded \
             rather than no dependencies: {}",
            unrecorded.join(", ")
        );
    }

    Ok(())
}

/// Project a registered repo's recorded cross-repo dependencies.
///
/// [`RegisteredRepo::dependencies`] is written by the registry at ingestion, so
/// this is a read of recorded truth. Only dependencies that ingestion resolved
/// to a repo still present in `known_ids` are reported, so the listing never
/// names a provider the registry no longer knows.
pub fn dependencies_for_repo(
    repo: &RegisteredRepo,
    known_ids: &HashSet<String>,
) -> Vec<(String, DepType)> {
    let mut deps: Vec<(String, DepType)> = repo
        .dependencies
        .iter()
        .filter_map(|dep| {
            let provider = dep.provider_repo.as_ref()?;
            known_ids
                .contains(provider)
                .then(|| (provider.clone(), DepType::from_source(&dep.source)))
        })
        .collect();

    deps.sort();
    deps.dedup();
    deps
}

/// Recorded dependencies for the repo registered at `repo_path`.
///
/// Kept for callers that hold a path rather than the registry record; prefer
/// [`dependencies_for_repo`] when the record is already in hand, since this
/// re-reads the registry to find it.
pub fn detect_dependencies_for_repo(
    repo_path: &std::path::Path,
    known_ids: &HashSet<String>,
) -> Vec<(String, DepType)> {
    let registry = match KinRegistry::load() {
        Ok(registry) => registry,
        Err(error) => {
            tracing::warn!(
                path = %repo_path.display(),
                error = %error.to_string(),
                "registry unreadable; reporting no recorded dependencies"
            );
            return Vec::new();
        }
    };
    registry
        .repos
        .iter()
        .find(|repo| repo.path.as_path() == repo_path)
        .map(|repo| dependencies_for_repo(repo, known_ids))
        .unwrap_or_default()
}

/// Format deps as a short suffix string for registry display, e.g. "-> kin-db, kin-vfs-core"
pub fn format_deps_short(deps: &[(String, DepType)]) -> String {
    if deps.is_empty() {
        "(no deps)".to_string()
    } else {
        let names: Vec<&str> = deps.iter().map(|(n, _)| n.as_str()).collect();
        format!("-> {}", names.join(", "))
    }
}

/// Where ingestion observed a dependency.
///
/// Carries the recorded source verbatim so the projection reports what was
/// actually captured rather than collapsing every dependency into one of a
/// couple of guessed manifest kinds.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DepType(String);

impl DepType {
    fn from_source(source: &str) -> Self {
        Self(source.to_string())
    }
}

impl std::fmt::Display for DepType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self.0.as_str() {
            "cargo" => "cargo manifest",
            "npm" => "npm manifest",
            "go" => "go module",
            "protocol" => "protocol import",
            "dockerfile" => "dockerfile",
            "compose" => "docker compose",
            "ci" => "ci workflow",
            "subprocess" => "runtime subprocess",
            "http" => "http api",
            other => other,
        };
        write!(f, "{label}")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        dependencies_for_repo, registered_id_for_path, render_repo_view_lines,
        repo_dependency_view, DepType, DEPS_SCHEMA,
    };
    use kin_core::dependencies::RepoDependency;
    use kin_core::registry::{KinRegistry, RegisteredRepo};
    use std::collections::HashSet;

    fn repo(path: std::path::PathBuf, dependencies: Vec<RepoDependency>) -> RegisteredRepo {
        RegisteredRepo {
            id: "kin".to_string(),
            path,
            entities: 0,
            last_commit: "2026-01-01T00:00:00Z".to_string(),
            dependencies,
            dependencies_recorded_by: None,
            dependencies_manifest_recorded_at: None,
        }
    }

    fn named_repo(id: &str, dependencies: Vec<RepoDependency>) -> RegisteredRepo {
        RegisteredRepo {
            id: id.to_string(),
            path: std::path::PathBuf::from(format!("/registered/{id}")),
            entities: 0,
            last_commit: "2026-01-01T00:00:00Z".to_string(),
            dependencies,
            dependencies_recorded_by: None,
            dependencies_manifest_recorded_at: None,
        }
    }

    /// A registry carrying the selected repository plus noise: the transient
    /// entries a test host accumulates are exactly what makes the fleet-wide
    /// listing unreadable, so the scoped answer must not contain them.
    fn registry_with_noise() -> KinRegistry {
        let mut registry = KinRegistry::default();
        registry.repos = vec![
            named_repo("kin", vec![dep("kin-db", "kin-db", "cargo")]),
            named_repo("kin-db", Vec::new()),
            named_repo("kin-vfs", vec![dep("kin", "kin", "cargo")]),
            named_repo("kin-bench", vec![dep("kin", "kin", "subprocess")]),
            named_repo(".tmp00M8wK", vec![dep("kin", "kin", "cargo")]),
        ];
        registry
    }

    fn dep(name: &str, provider: &str, source: &str) -> RepoDependency {
        RepoDependency {
            name: name.to_string(),
            provider_repo: Some(provider.to_string()),
            source: source.to_string(),
        }
    }

    /// `kin deps` answers from the dependency records ingestion wrote, so a
    /// manifest sitting in the working tree cannot steer the answer. The
    /// manifest here contradicts the records on purpose: anything that re-derives
    /// the answer by parsing it reports kin-vfs and fails.
    #[test]
    fn deps_project_recorded_truth_not_the_manifest_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[dependencies]\nkin-vfs = { git = \"git@example.invalid:kin-vfs.git\" }\n",
        )
        .unwrap();

        let repo = repo(
            dir.path().to_path_buf(),
            vec![dep("kin-db", "kin-db", "cargo")],
        );
        let known: HashSet<String> = ["kin-db", "kin-vfs"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        assert_eq!(
            dependencies_for_repo(&repo, &known),
            vec![("kin-db".to_string(), DepType("cargo".to_string()))],
        );
    }

    /// A repo with no recorded dependencies reports none — the gap is surfaced
    /// rather than filled in from the manifests lying next to it.
    #[test]
    fn deps_report_the_gap_rather_than_scraping_manifests() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[dependencies]\nkin-db = { git = \"git@example.invalid:kin-db.git\" }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            "{\"dependencies\":{\"@kinlab/contracts\":\"workspace:*\"}}",
        )
        .unwrap();

        let repo = repo(dir.path().to_path_buf(), Vec::new());
        let known: HashSet<String> = ["kin-db", "kinlab"].iter().map(|s| s.to_string()).collect();

        assert!(dependencies_for_repo(&repo, &known).is_empty());
    }

    /// A recorded provider the registry no longer knows is dropped rather than
    /// listed as a live cross-repo edge.
    #[test]
    fn deps_drop_providers_the_registry_no_longer_knows() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(
            dir.path().to_path_buf(),
            vec![
                dep("kin-db", "kin-db", "cargo"),
                dep("retired-crate", "retired-repo", "cargo"),
            ],
        );
        let known: HashSet<String> = ["kin-db"].iter().map(|s| s.to_string()).collect();

        assert_eq!(
            dependencies_for_repo(&repo, &known),
            vec![("kin-db".to_string(), DepType("cargo".to_string()))],
        );
    }

    /// Every recorded source family renders with a label, including ones the
    /// retired two-variant enum could not express.
    #[test]
    fn dep_type_labels_every_recorded_source() {
        for (source, label) in [
            ("cargo", "cargo manifest"),
            ("npm", "npm manifest"),
            ("go", "go module"),
            ("protocol", "protocol import"),
            ("dockerfile", "dockerfile"),
            ("compose", "docker compose"),
            ("ci", "ci workflow"),
            ("subprocess", "runtime subprocess"),
            ("http", "http api"),
        ] {
            assert_eq!(DepType::from_source(source).to_string(), label);
        }
        assert_eq!(DepType::from_source("future").to_string(), "future");
    }

    /// The scoped answer names only the caller's own recorded edges. The four
    /// unrelated registry entries, including the transient one, are absent: a
    /// caller standing in `kin` asked about `kin`.
    #[test]
    fn scoped_view_reports_one_repository_in_both_directions() {
        let view = repo_dependency_view(&registry_with_noise(), "kin").unwrap();

        assert_eq!(view.schema, DEPS_SCHEMA);
        assert_eq!(view.repo_id, "kin");
        assert_eq!(
            view.depends_on
                .iter()
                .map(|edge| edge.repo_id.as_str())
                .collect::<Vec<_>>(),
            vec!["kin-db"]
        );
        assert_eq!(
            view.consumers
                .iter()
                .map(|edge| (edge.repo_id.as_str(), edge.source.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (".tmp00M8wK", "cargo"),
                ("kin-bench", "subprocess"),
                ("kin-vfs", "cargo"),
            ],
            "consumers are the registered repositories recording this one as a provider"
        );

        let rendered = render_repo_view_lines(&view).join("\n");
        assert!(rendered.contains("Cross-repo dependencies for kin:"));
        assert!(rendered.contains("kin deps --all"));
        assert!(
            !rendered.contains("kin-db\n  "),
            "the fleet listing's per-repo headings must not appear in a scoped answer"
        );
    }

    /// Falsification: were the scope wired to the registry rather than to the
    /// caller, this would return `kin`'s neighbourhood for `kin-db`.
    #[test]
    fn scoped_view_follows_the_named_repository_not_the_registry_order() {
        let view = repo_dependency_view(&registry_with_noise(), "kin-db").unwrap();

        assert!(
            view.depends_on.is_empty(),
            "kin-db records no providers: {:?}",
            view.depends_on
        );
        assert_eq!(
            view.consumers
                .iter()
                .map(|edge| edge.repo_id.as_str())
                .collect::<Vec<_>>(),
            vec!["kin"]
        );
    }

    /// An unregistered repository has no records at all. Reporting an empty
    /// neighbourhood would read as a proved absence, so it refuses and states
    /// the true state: nothing ships that would register it.
    #[test]
    fn an_unregistered_repository_refuses_rather_than_reporting_no_dependencies() {
        let error = repo_dependency_view(&registry_with_noise(), "not-registered")
            .expect_err("an unregistered repository must not report an empty neighbourhood");
        let message = error.to_string();

        assert!(message.contains("not in the Kin registry"), "{message}");
        assert!(
            message.contains("kin init") && message.contains("registers a repository"),
            "the refusal must explain that registration comes from `kin init`: {message}"
        );
        assert!(
            message.contains("kin registry") && message.contains("only lists and cleans"),
            "the refusal must still say `kin registry` cannot add an entry after the fact: {message}"
        );
        assert!(message.contains("kin deps --all"), "{message}");
    }

    /// Registration names an entry after the repository's directory, so the
    /// path is what identifies it. Falsification: the entry chosen here is not
    /// the one whose directory name matches, which is what a name-keyed lookup
    /// would have returned.
    #[test]
    fn the_caller_resolves_to_the_entry_recording_its_own_path() {
        let mut registry = KinRegistry::default();
        registry.repos = vec![
            named_repo("kin", Vec::new()),
            RegisteredRepo {
                id: "kin-checkout-two".to_string(),
                path: std::path::PathBuf::from("/elsewhere/kin"),
                entities: 0,
                last_commit: "2026-01-01T00:00:00Z".to_string(),
                dependencies: Vec::new(),
                dependencies_recorded_by: None,
                dependencies_manifest_recorded_at: None,
            },
        ];

        assert_eq!(
            registered_id_for_path(&registry, std::path::Path::new("/elsewhere/kin")).unwrap(),
            "kin-checkout-two"
        );
        assert_eq!(
            registered_id_for_path(&registry, std::path::Path::new("/registered/kin")).unwrap(),
            "kin"
        );
    }

    /// An unregistered checkout that shares a name with a registered one must
    /// refuse rather than answer about the other checkout, and must say the
    /// same-named entries exist so the miss is not mistaken for a typo.
    #[test]
    fn a_same_named_checkout_elsewhere_never_answers_for_this_one() {
        let mut registry = KinRegistry::default();
        registry.repos = vec![named_repo("kin", vec![dep("kin-db", "kin-db", "cargo")])];

        let error = registered_id_for_path(&registry, std::path::Path::new("/other/place/kin"))
            .expect_err("an unregistered checkout must not borrow another's records");
        let message = error.to_string();

        assert!(message.contains("/other/place/kin"), "{message}");
        assert!(message.contains("same directory name"), "{message}");
        assert!(
            message.contains("kin init") && message.contains("registers a repository"),
            "the refusal must explain that registration comes from `kin init`: {message}"
        );
        assert!(
            message.contains("kin registry") && message.contains("only lists and cleans"),
            "the refusal must still say `kin registry` cannot add an entry after the fact: {message}"
        );
        assert!(message.contains("kin deps --all"), "{message}");
    }

    /// Two entries recording one path is a corrupted registry, not a choice to
    /// make silently.
    #[test]
    fn duplicate_entries_for_one_path_are_refused() {
        let mut registry = KinRegistry::default();
        registry.repos = vec![named_repo("kin", Vec::new()), named_repo("kin", Vec::new())];
        registry.repos[1].id = "kin-duplicate".to_string();

        let error = registered_id_for_path(&registry, std::path::Path::new("/registered/kin"))
            .expect_err("a duplicated path must be refused");

        assert!(
            error.to_string().contains("refusing to pick one"),
            "{error}"
        );
        assert!(error.to_string().contains("kin registry clean"), "{error}");
    }

    /// Both empty directions still say why they are empty, so "no edges" is
    /// never mistaken for "records exist and prove there are none".
    #[test]
    fn empty_directions_read_as_possibly_unrecorded_not_proved_absent() {
        let mut registry = KinRegistry::default();
        registry.repos = vec![named_repo("solo", Vec::new())];

        let view = repo_dependency_view(&registry, "solo").unwrap();
        let rendered = render_repo_view_lines(&view).join("\n");

        assert!(rendered.contains("no cross-repo dependencies recorded"));
        assert!(rendered.contains("no registered repository records this one as a provider"));
        assert!(rendered.contains("kin init") && rendered.contains("registers a repository"));
    }

    /// The caveat is about stores that predate dependency recording. With axum
    /// and flask both registered minutes earlier by the same 0.6.0 binary,
    /// `kin deps --all` named them both, because the only thing it could key on
    /// was the empty listing, and a correct zero and an unrecorded one look
    /// identical there.
    #[test]
    fn the_unrecorded_caveat_keys_on_the_recorded_build_not_on_an_empty_listing() {
        let recorded = |id: &str| RegisteredRepo {
            id: id.to_string(),
            path: std::path::PathBuf::from(format!("/registered/{id}")),
            entities: 0,
            last_commit: "2026-01-01T00:00:00Z".to_string(),
            dependencies: Vec::new(),
            dependencies_recorded_by: Some("0.6.1".to_string()),
            dependencies_manifest_recorded_at: None,
        };

        // Registered by a build that records dependencies. The zero is measured.
        let mut registry = KinRegistry::default();
        registry.repos = vec![recorded("axum")];
        let view = repo_dependency_view(&registry, "axum").unwrap();
        assert!(
            !view.depends_on_predates_recording,
            "an entry written by a recording build does not predate recording"
        );
        let rendered = render_repo_view_lines(&view).join("\n");
        assert!(
            rendered.contains("no cross-repo dependencies recorded"),
            "the empty direction is still reported: {rendered}"
        );
        assert!(
            !rendered.contains("may mean unrecorded"),
            "but a measured zero is not called possibly-unrecorded: {rendered}"
        );

        // The same shape, written before any build recorded dependencies.
        let mut old = KinRegistry::default();
        old.repos = vec![named_repo("legacy", Vec::new())];
        let old_view = repo_dependency_view(&old, "legacy").unwrap();
        assert!(
            old_view.depends_on_predates_recording,
            "an entry with no recorded build is exactly what the caveat is for"
        );
        assert!(
            render_repo_view_lines(&old_view)
                .join("\n")
                .contains("may mean unrecorded"),
            "and it still gets the caveat"
        );

        // The two directions are answered by different entries' records: this
        // repository recorded its own, a sibling did not, so only the consumers
        // direction is in doubt.
        let mut mixed = KinRegistry::default();
        mixed.repos = vec![recorded("axum"), named_repo("legacy", Vec::new())];
        let mixed_view = repo_dependency_view(&mixed, "axum").unwrap();
        assert!(
            !mixed_view.depends_on_predates_recording,
            "this repository's own records are current"
        );
        assert!(
            mixed_view.consumers_predate_recording,
            "a sibling that predates recording leaves the inverse direction in doubt"
        );
    }

    #[test]
    fn manifest_refresh_removes_old_edges_and_preserves_admission_provenance() {
        let mut registry = registry_with_noise();
        let row = &mut registry.repos[0];
        row.dependencies
            .push(dep("bin:kin-bench", "kin-bench", "subprocess"));
        let original_finalized = row.last_commit.clone();
        let path = row.path.clone();
        let response = super::DepsResponse {
            repository_id: "kin".to_string(),
            dependencies: vec![dep("kin-core", "kin-core", "cargo")],
            observed_at: "2026-10-08T16:00:00Z".to_string(),
        };
        super::record_manifest_refresh(&mut registry, &path, &response).unwrap();
        let row = &registry.repos[0];
        assert!(!row.dependencies.iter().any(|dep| dep.name == "kin-db"));
        assert!(row.dependencies.iter().any(|dep| dep.name == "kin-core"));
        assert!(row
            .dependencies
            .iter()
            .any(|dep| dep.source == "subprocess"));
        assert_eq!(row.last_commit, original_finalized);
        assert_eq!(
            row.dependencies_manifest_recorded_at.as_deref(),
            Some(response.observed_at.as_str())
        );
        assert!(
            row.dependencies_recorded_by.is_none(),
            "a manifest-only read cannot certify the unrecorded heuristics"
        );

        let mut wrong = response.clone();
        wrong.repository_id = "another-repository".to_string();
        assert!(super::record_manifest_refresh(&mut registry, &path, &wrong).is_err());
        wrong.repository_id = "kin".to_string();
        wrong.dependencies = vec![dep("bad", "bad", "subprocess")];
        assert!(super::record_manifest_refresh(&mut registry, &path, &wrong).is_err());
    }

    #[test]
    fn refresh_and_report_select_the_exact_location_of_a_copied_repository() {
        let mut registry = registry_with_noise();
        let mut copy = registry.repos[0].clone();
        copy.path = "/another/kin".into();
        let copy_path = copy.path.clone();
        registry.repos.push(copy);
        let response = super::DepsResponse {
            repository_id: "kin".into(),
            dependencies: vec![],
            observed_at: "2026-10-08T16:00:00Z".into(),
        };
        super::record_manifest_refresh(&mut registry, &copy_path, &response).unwrap();
        assert!(registry.repos[0]
            .dependencies
            .iter()
            .any(|dep| dep.source == "cargo"));
        let row = registry
            .repos
            .iter()
            .find(|row| row.path == copy_path)
            .unwrap();
        let view = super::repo_dependency_view_for_row(&registry, row);
        assert!(!view.depends_on.iter().any(|dep| dep.source == "cargo"));
        assert_eq!(
            view.manifests_recorded_at.as_deref(),
            Some(response.observed_at.as_str())
        );
    }

    #[test]
    fn dependency_daemon_fallback_does_not_hide_store_or_policy_refusals() {
        use crate::daemon_client::AutoStartError;
        let disabled = super::dependency_daemon_resolution(Ok(None)).unwrap();
        assert!(disabled.0.is_none() && disabled.1.unwrap().contains("disabled"));
        let unavailable =
            super::dependency_daemon_resolution(Err(AutoStartError::BinaryNotFound.into()))
                .unwrap();
        assert!(unavailable.0.is_none() && unavailable.1.unwrap().contains("could not start"));
        for refusal in [
            AutoStartError::IncompatibleStore("upgrade needed".into()),
            AutoStartError::BehaviorEnvIgnored("strict policy".into()),
        ] {
            assert!(super::dependency_daemon_resolution(Err(refusal.into())).is_err());
        }
        let mut view = repo_dependency_view(&registry_with_noise(), "kin").unwrap();
        view.refresh_gap = Some("daemon disabled".to_owned());
        let output = render_repo_view_lines(&view).join("\n");
        assert!(
            output.contains("Recorded view")
                && output.contains("2026-01-01")
                && output.contains("kin deps")
        );
    }

    #[test]
    fn dependency_refresh_uses_held_bytes_and_refuses_missing_or_wrong_authority() {
        let temp = tempfile::tempdir().unwrap();
        let _home = kin_core::test_env::EnvVarGuard::set("KIN_HOME", temp.path().join("kin-home"));
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = crate::commands::test_subprocess::fixture_git(&repo)
                .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--initial-branch=main"]);
        git(&["config", "user.email", "kin@example.invalid"]);
        git(&["config", "user.name", "Kin"]);
        std::fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname='sample'\nversion='0.1.0'\n[dependencies]\nkin-db={git='https://github.com/firelock-ai/kin-db.git'}\n",
        )
        .unwrap();
        for (name, bytes) in [
            ("invalid-utf8.bin", &[0xff][..]),
            ("invalid-nul.txt", b"a\0b".as_slice()),
            ("invalid-toml.txt", b"[".as_slice()),
        ] {
            std::fs::write(repo.join(name), bytes).unwrap();
        }
        git(&["add", "--all"]);
        git(&["commit", "--signoff", "-m", "held dependency fixture"]);
        let layout = kin_core::init_from_git(&repo).unwrap().layout;
        let held = kin_core::LocalRepositoryAuthorityBinding::from_layout(&layout).unwrap();
        let authority =
            crate::commands::repository_authority::ActiveRepositoryAuthority::open(&held).unwrap();
        let tree = authority.workspace().unwrap().tree;
        drop(authority);
        // Contradict the admitted bytes. Neither query nor finalization follows disk.
        std::fs::write(
            repo.join("Cargo.toml"),
            "[dependencies]\nkin-cli={git='https://github.com/firelock-ai/kin-cli.git'}\n",
        )
        .unwrap();
        let request = super::DepsRequest {
            repository_id: held.repository_id().to_string(),
            registry_repo_ids: vec![],
        };
        let response = super::build_deps_response(&tree, &held, &request).unwrap();
        assert!(response.dependencies.iter().any(|dep| dep.name == "kin-db"));
        assert!(!response
            .dependencies
            .iter()
            .any(|dep| dep.name == "kin-cli"));
        let admitted = kin_core::dependencies::HeldTree::new(&tree, &held)
            .dependency_content()
            .unwrap();
        let deps = kin_core::dependencies::detect_dependencies(&admitted);
        assert!(deps.iter().any(|dep| dep.name == "kin-db"));
        assert!(!deps.iter().any(|dep| dep.name == "kin-cli"));
        let mut wrong = request.clone();
        wrong.repository_id = "other".into();
        assert!(super::build_deps_response(&tree, &held, &wrong).is_err());
        let mut artifacts: Vec<_> = tree.artifacts_by_path().cloned().collect();
        let manifest = artifacts
            .iter_mut()
            .find(|entry| entry.path.as_utf8() == Some("Cargo.toml"))
            .unwrap();
        manifest.entry = kin_model::TreeEntry::blob(kin_model::Hash256([0x42; 32]), false);
        let missing = kin_model::ResolvedTree::from_artifacts(artifacts.clone()).unwrap();
        assert!(
            super::build_deps_response(&missing, &held, &request).is_err(),
            "missing CAS must never read the working manifest"
        );
        let manifest = artifacts
            .iter_mut()
            .find(|entry| entry.path.as_utf8() == Some("Cargo.toml"))
            .unwrap();
        manifest.entry = kin_model::TreeEntry::symlink(kin_model::Hash256([0x42; 32]));
        let linked = kin_model::ResolvedTree::from_artifacts(artifacts.clone()).unwrap();
        assert!(
            super::build_deps_response(&linked, &held, &request).is_err(),
            "a held link is not manifest text and cannot certify zero dependencies"
        );
        for name in ["invalid-utf8.bin", "invalid-nul.txt", "invalid-toml.txt"] {
            let invalid_entry = artifacts
                .iter()
                .find(|entry| entry.path.as_utf8() == Some(name))
                .unwrap()
                .entry
                .clone();
            artifacts
                .iter_mut()
                .find(|entry| entry.path.as_utf8() == Some("Cargo.toml"))
                .unwrap()
                .entry = invalid_entry;
            let invalid = kin_model::ResolvedTree::from_artifacts(artifacts.clone()).unwrap();
            assert!(
                super::build_deps_response(&invalid, &held, &request).is_err(),
                "invalid held manifest {name} must not erase recorded edges"
            );
        }
    }
}
