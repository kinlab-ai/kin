// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The local human workspace. One server answers for every Kin repository on
//! this machine: the page at `/` lists them, and each repository's own pages
//! live under a URL that names it. Changing the selection never changes the
//! authority of an in-flight request.
//!
//! Repositories are found per request, not at startup. One that is added
//! while the server runs, from its own page or by `kin init` in a terminal,
//! answers at its URL without a restart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::handler::Handler;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, MethodRouter};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::review_ui::{self, UiState};
use crate::daemon_client::DaemonClient;

const INDEX: &str = include_str!("../../assets/workspace_ui/index.html");
const STYLE: &str = include_str!("../../assets/workspace_ui/style.css");
const APP: &str = include_str!("../../assets/workspace_ui/workspace.js");
const GRAPH: &str = include_str!("../../assets/workspace_ui/graph.js");
const AGENTS_APP: &str = include_str!("../../assets/workspace_ui/agents.js");
const AGENTS_STYLE: &str = include_str!("../../assets/workspace_ui/agents.css");
const MARK: &str = include_str!("../../assets/review_ui/mark.svg");

/// How long a repository's name is kept before its records are read again. A
/// list that a page polls would otherwise reopen every repository's
/// configuration on every read.
const IDENTITY_KEPT: Duration = Duration::from_secs(30);

/// The longest message a commit made from the page carries, in characters.
const COMMIT_MESSAGE_MAX_CHARS: usize = 4000;

/// The longest name a branch made from the page carries, in characters.
const BRANCH_NAME_MAX_CHARS: usize = 200;

#[derive(Clone, Serialize)]
pub(super) struct RepositoryEntry {
    pub(super) key: String,
    pub(super) name: String,
    /// Who holds the repository on its host, from its recorded Git remote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) host: Option<String>,
    pub(super) url: String,
}

/// One repository this machine can open, with what the registry records
/// about it.
pub(super) struct Listed {
    pub(super) entry: RepositoryEntry,
    pub(super) layout: kin_core::KinLayout,
    /// The registry's record. Absent for a repository this server opened
    /// that the registry does not name.
    pub(super) record: Option<Recorded>,
}

/// What the registry last recorded about a repository.
pub(super) struct Recorded {
    pub(super) entities: usize,
    pub(super) at: String,
}

/// One reading of the repositories this machine can open.
pub(super) struct Listing {
    pub(super) repositories: Vec<Listed>,
    /// Registered locations that no longer hold a Kin repository.
    pub(super) unavailable: usize,
    pub(super) warning: Option<String>,
}

/// The repositories this server can show. It holds no list of its own: every
/// reading comes from the registry as it is when asked.
pub(super) struct Catalog {
    /// False for a test that serves one repository and reads no registry.
    reads_registry: bool,
    /// Repositories this server opened itself, which the registry may not
    /// name: one given on the command line, or one opened from the page.
    opened: RwLock<Vec<kin_core::KinLayout>>,
    names: Mutex<HashMap<String, (Instant, RepositoryEntry)>>,
}

impl Catalog {
    pub(super) fn live() -> Arc<Self> {
        Arc::new(Self {
            reads_registry: true,
            opened: RwLock::default(),
            names: Mutex::default(),
        })
    }

    fn remember(&self, layout: &kin_core::KinLayout) {
        let mut opened = self
            .opened
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !opened
            .iter()
            .any(|known| known.working_dir() == layout.working_dir())
        {
            opened.push(layout.clone());
        }
    }

    /// The entry a repository is listed by, reading its name at most once
    /// per [`IDENTITY_KEPT`].
    fn named(&self, layout: &kin_core::KinLayout) -> RepositoryEntry {
        let key = repository_key(layout.working_dir());
        let names = || {
            self.names
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        if let Some((read, entry)) = names().get(&key) {
            if read.elapsed() < IDENTITY_KEPT {
                return entry.clone();
            }
        }
        // Read outside the lock, so one slow repository holds up no other.
        let entry = entry(layout);
        names().insert(key, (Instant::now(), entry.clone()));
        entry
    }

    /// Whether the registry names exactly this location. Blocking.
    pub(super) fn registered_at(&self, location: &Path) -> bool {
        let wanted = repository_key(location);
        self.reads_registry
            && kin_core::registry::KinRegistry::load().is_ok_and(|registry| {
                registry
                    .repos
                    .iter()
                    .any(|row| repository_key(&row.path) == wanted)
            })
    }

    /// The key of every location the registry names, for a caller that asks
    /// about many locations at once. Blocking.
    pub(super) fn registered_keys(&self) -> std::collections::HashSet<String> {
        if !self.reads_registry {
            return Default::default();
        }
        kin_core::registry::KinRegistry::load()
            .map(|registry| {
                registry
                    .repos
                    .iter()
                    .map(|row| repository_key(&row.path))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Read the registry and name what it holds. Blocking: it reads the
    /// registry and each repository's own records.
    pub(super) fn read(&self) -> Listing {
        let opened = self
            .opened
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let (found, unavailable, warning) = catalog(self.reads_registry, opened);
        Listing {
            repositories: found
                .into_iter()
                .map(|(layout, record)| Listed {
                    entry: self.named(&layout),
                    layout,
                    record,
                })
                .collect(),
            unavailable,
            warning,
        }
    }
}

/// A routing/storage key, not repository authority. The daemon still verifies
/// repository and workspace identity against this exact layout on every read.
pub(super) fn repository_key(root: &Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    hex::encode(Sha256::digest(canonical.as_os_str().as_encoded_bytes()))
}

/// The name a repository reads by: its remote's owner and name when Kin has a
/// remote for it, otherwise its working tree's own name.
pub(super) fn display_identity(
    layout: &kin_core::KinLayout,
) -> (
    String,
    Option<kin_core::repository_origin::RepositoryOrigin>,
) {
    let origin = kin_core::repository_origin::repository_origin(layout);
    let name = origin
        .as_ref()
        .map(|origin| origin.name.clone())
        .unwrap_or_else(|| {
            layout
                .working_dir()
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "repository".into())
        });
    (name, origin)
}

pub(super) fn entry(layout: &kin_core::KinLayout) -> RepositoryEntry {
    let key = repository_key(layout.working_dir());
    let (name, origin) = display_identity(layout);
    RepositoryEntry {
        name,
        owner: origin.as_ref().and_then(|origin| origin.owner.clone()),
        host: origin.map(|origin| origin.host),
        url: format!("/repositories/{key}/workspace/"),
        key,
    }
}

/// Each repository found, with the registry's record of it when it has one.
type Found = Vec<(kin_core::KinLayout, Option<Recorded>)>;

/// The repositories this machine can open now: every registered location
/// that still holds one, then any of `opened` the registry does not name,
/// with how many registered locations hold none and a warning to show. No
/// repository is initialized and no daemon is started to answer.
fn catalog(
    reads_registry: bool,
    opened: Vec<kin_core::KinLayout>,
) -> (Found, usize, Option<String>) {
    // The repository at exactly `path`, or nothing. A stale child
    // registration must not silently become its parent.
    let held_at = |path: &Path| {
        let layout = kin_core::KinLayout::discover(path)?;
        (layout.working_dir().canonicalize().ok() == path.canonicalize().ok() && path.is_dir())
            .then_some(layout)
    };
    let mut found: Vec<(String, kin_core::KinLayout, Option<Recorded>)> = Vec::new();
    let mut unavailable = 0;
    let mut warning = None;
    if reads_registry {
        match kin_core::registry::KinRegistry::load() {
            Ok(registry) => {
                for repository in registry.repos {
                    let Some(layout) = held_at(&repository.path) else {
                        unavailable += 1;
                        continue;
                    };
                    let key = repository_key(layout.working_dir());
                    if found.iter().all(|(known, _, _)| *known != key) {
                        let record = Recorded {
                            entities: repository.entities,
                            at: repository.last_commit,
                        };
                        found.push((key, layout, Some(record)));
                    }
                }
                warning = match unavailable {
                    0 => None,
                    1 => Some("1 registered repository is unavailable on this machine.".into()),
                    count => Some(format!(
                        "{count} registered repositories are unavailable on this machine."
                    )),
                };
            }
            Err(error) => {
                warning = Some(format!("Could not read the repository registry: {error}"))
            }
        }
    }
    for layout in opened {
        let key = repository_key(layout.working_dir());
        if found.iter().all(|(known, _, _)| *known != key)
            && (!reads_registry || held_at(layout.working_dir()).is_some())
        {
            found.push((key, layout, None));
        }
    }
    (
        found
            .into_iter()
            .map(|(_, layout, record)| (layout, record))
            .collect(),
        unavailable,
        warning,
    )
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    ([(axum::http::header::CONTENT_TYPE, content_type)], body).into_response()
}

/// The one page, told where its own scope begins: a repository's base path,
/// or nothing for the page that lists them.
fn page(base: &str) -> Response {
    Html(INDEX.replace("__KIN_BASE__", base)).into_response()
}

async fn index(State(state): State<Arc<UiState>>) -> Response {
    page(&state.base_path)
}

/// What the page loads, the same bytes in every scope that serves it.
fn page_assets<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route(
            "/workspace/style.css",
            get(|| async { asset(STYLE, "text/css; charset=utf-8") }),
        )
        .route(
            "/workspace/workspace.js",
            get(|| async { asset(APP, "application/javascript; charset=utf-8") }),
        )
        .route(
            "/workspace/graph.js",
            get(|| async { asset(GRAPH, "application/javascript; charset=utf-8") }),
        )
        .route(
            "/workspace/agents.js",
            get(|| async { asset(AGENTS_APP, "application/javascript; charset=utf-8") }),
        )
        .route(
            "/workspace/agents.css",
            get(|| async { asset(AGENTS_STYLE, "text/css; charset=utf-8") }),
        )
        .route(
            "/workspace/mark.svg",
            get(|| async { asset(MARK, "image/svg+xml") }),
        )
}

pub(super) fn routes() -> Router<Arc<UiState>> {
    Router::new()
        .route("/workspace/", get(index))
        .merge(page_assets())
        .route("/api/workspace/session", get(session))
        .route("/api/workspace/session/pull", get(session_pull))
        .route("/api/workspace/branches", get(branches))
        .route("/api/workspace/branch/preview", post(branch_preview))
        .route("/api/workspace/branch/confirm", post(branch_confirm))
        .route(
            "/api/workspace/conflicts/details",
            get(super::workspace_conflicts::serve_conflicts),
        )
        .route("/api/workspace/merge/preview", post(merge_preview))
        .route(
            "/api/workspace/conflicts/settle",
            post(super::workspace_conflicts::settle),
        )
        .route("/api/workspace/branch/create", post(branch_create))
        .route("/api/workspace/merge/confirm", post(merge_confirm))
        .route("/api/workspace/commit", post(commit))
        .route("/api/workspace/entity/restore", post(entity_restore))
        .merge(super::workspace_explore::routes())
        .merge(super::workspace_agents::routes())
        .merge(super::workspace_remotes::routes())
        .merge(super::workspace_verify::routes())
        .merge(super::workspace_flags::routes())
        .layer(DefaultBodyLimit::max(64 * 1024))
}

/// A catalog with no other repository in it, for tests that serve one.
#[cfg(test)]
pub(super) fn empty_catalog() -> Arc<Catalog> {
    Arc::new(Catalog {
        reads_registry: false,
        opened: RwLock::default(),
        names: Mutex::default(),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BranchConfirmBody {
    operation_id: kin_model::OperationId,
    expected: super::branch::BranchSwitchExpectation,
}

fn branch_target(name: &kin_model::RefName) -> Result<()> {
    anyhow::ensure!(name.as_bytes().len() <= 512, "branch name is too long");
    anyhow::ensure!(name.is_branch(), "choose a target below refs/heads/");
    Ok(())
}

async fn branches(State(state): State<Arc<UiState>>) -> Response {
    let answer = async {
        review_ui::daemon(&state)
            .await?
            .branch(&super::branch::BranchRequest::List)
            .await?
            .report
            .context("daemon branch list omitted its authority report")
    }
    .await;
    match answer {
        Ok(report) => Json(report).into_response(),
        Err(error) => review_ui::refusal(StatusCode::BAD_GATEWAY, &error),
    }
}

async fn branch_preview(
    State(state): State<Arc<UiState>>,
    headers: HeaderMap,
    Json(request): Json<super::branch::BranchSwitchPreviewRequest>,
) -> Response {
    if !review_ui::may_write(&headers, &state) {
        return review_ui::write_refused();
    }
    if let Err(error) = branch_target(&request.name) {
        return review_ui::refusal(StatusCode::BAD_REQUEST, &error);
    }
    let answer = async {
        let (_, port) = crate::daemon_client::repo_daemon_recorded_endpoint(state.layout.root());
        let port = port.filter(|port| *port != 0)
            .context("No daemon endpoint is recorded. Open repository content before requesting a branch preview.")?;
        DaemonClient::from_running_workspace(format!("http://127.0.0.1:{port}"), &state.layout)
            .await?.branch_switch_preview(&request).await
    }.await;
    match answer {
        Ok(preview) => Json(preview).into_response(),
        Err(error) => review_ui::refusal(StatusCode::BAD_GATEWAY, &error),
    }
}

fn branch_not_dispatched(operation_id: kin_model::OperationId, error: &anyhow::Error) -> Response {
    (StatusCode::CONFLICT, Json(serde_json::json!({
        "error":format!("{error:#}"),"outcome":"refused","operation_id":operation_id,
        "remedy":"This confirmation was not dispatched. Correct the precondition and read a new branch preview.",
    }))).into_response()
}

async fn branch_confirm(
    State(state): State<Arc<UiState>>,
    headers: HeaderMap,
    Json(body): Json<BranchConfirmBody>,
) -> Response {
    if !review_ui::may_write(&headers, &state) {
        return review_ui::write_refused();
    }
    let operation_id = body.operation_id;
    let prepared = (|| {
        anyhow::ensure!(
            !operation_id.0.is_nil(),
            "a nonempty operation ID is required"
        );
        branch_target(&body.expected.target_ref)?;
        let actor = super::require_commit_author_for(&state.layout)?;
        Ok::<_, anyhow::Error>(super::branch::BranchSwitchConfirmRequest {
            expected: body.expected,
            operation_id,
            actor,
        })
    })();
    let request = match prepared {
        Ok(request) => request,
        Err(error) => return branch_not_dispatched(operation_id, &error),
    };
    let client = match review_ui::daemon(&state).await {
        Ok(client) => client,
        Err(error) => return branch_not_dispatched(operation_id, &error),
    };
    match client.branch_switch_confirm(&request).await {
        Ok(response) => Json(response).into_response(),
        Err(error) if error.downcast_ref::<crate::daemon_client::RefusedBeforeWriteError>().is_some() => {
            (StatusCode::CONFLICT, Json(serde_json::json!({
                "error":format!("{error:#}"),"outcome":"refused","operation_id":operation_id,
                "remedy":"The daemon refused before publication. Read current branch and conflict state, then a new preview.",
            }))).into_response()
        }
        Err(error) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({
            "error":format!("{error:#}"),"outcome":"unknown","operation_id":operation_id,
            "remedy":"The switch may have committed. Refresh repository and branch state before another attempt. This request was not retried.",
        }))).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BranchCreateBody {
    name: String,
    operation_id: kin_model::OperationId,
}

/// The branch a reader named, as the ref it will be.
fn new_branch(name: &str) -> Result<kin_model::RefName> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "a branch needs a name");
    anyhow::ensure!(
        name.chars().count() <= BRANCH_NAME_MAX_CHARS,
        "a branch name is at most {BRANCH_NAME_MAX_CHARS} characters"
    );
    let name = super::repository_authority::parse_ref_name(name)?;
    anyhow::ensure!(name.is_branch(), "choose a name below refs/heads/");
    Ok(name)
}

/// Nothing was created: the request stopped before the daemon was asked, or
/// the daemon refused and the branch list shows no such branch.
fn branch_not_created(operation_id: kin_model::OperationId, error: &anyhow::Error) -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error":format!("{error:#}"),"outcome":"refused","operation_id":operation_id,
            "remedy":"No branch was created. Correct what is reported, then create it again.",
        })),
    )
        .into_response()
}

/// Create a branch at the current branch's head, as `kin branch create` does.
async fn branch_create(
    State(state): State<Arc<UiState>>,
    headers: HeaderMap,
    Json(body): Json<BranchCreateBody>,
) -> Response {
    if !review_ui::may_write(&headers, &state) {
        return review_ui::write_refused();
    }
    let operation_id = body.operation_id;
    let prepared = (|| {
        anyhow::ensure!(
            !operation_id.0.is_nil(),
            "a nonempty operation ID is required"
        );
        let name = new_branch(&body.name)?;
        // Attribution resolves against this URL's repository, never the
        // process working directory or a caller-supplied actor.
        let actor = super::require_commit_author_for(&state.layout)?;
        Ok::<_, anyhow::Error>((name, actor))
    })();
    let (name, actor) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return branch_not_created(operation_id, &error),
    };
    let client = match review_ui::daemon(&state).await {
        Ok(client) => client,
        Err(error) => return branch_not_created(operation_id, &error),
    };
    // Whether a branch of this name exists, read from the daemon's own list.
    // The create request is answered through a transport that reports a
    // refusal and a lost answer alike, so this list is what tells them apart.
    let exists = || async {
        let report = client
            .branch(&super::branch::BranchRequest::List)
            .await?
            .report
            .context("daemon branch list omitted its authority report")?;
        Ok::<_, anyhow::Error>(report.branches.iter().any(|branch| branch.name == name))
    };
    match exists().await {
        Ok(false) => {}
        Ok(true) => {
            return branch_not_created(
                operation_id,
                &anyhow::anyhow!("a branch named {name} already exists"),
            )
        }
        Err(error) => return branch_not_created(operation_id, &error),
    }
    let request = super::branch::BranchRequest::Create {
        name: name.clone(),
        operation_id,
        actor,
    };
    let Err(error) = client.branch(&request).await else {
        return Json(serde_json::json!({
            "operation_id":operation_id, "outcome":"created", "name":name.to_string(),
        }))
        .into_response();
    };
    match exists().await {
        Ok(false) => (StatusCode::CONFLICT, Json(serde_json::json!({
            "error":format!("{error:#}"),"outcome":"refused","operation_id":operation_id,
            "remedy":"The daemon refused and no branch of this name exists. Correct what is reported, then create it again.",
        }))).into_response(),
        // The branch is there, or the list could not be read. Either way the
        // answer that would have said how this request ended was lost.
        _ => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({
            "error":format!("{error:#}"),"outcome":"unknown","operation_id":operation_id,
            "remedy":"The branch may have been created. Read the branches again before creating it again. This request was not retried.",
        }))).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewBody {
    source: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmBody {
    source: String,
    operation_id: kin_model::OperationId,
    expected: super::merge::MergeExpectation,
}

fn source_ref(source: &str) -> Result<kin_model::RefName> {
    anyhow::ensure!(source.len() <= 512, "branch name is too long");
    let source = super::repository_authority::parse_ref_name(source)?;
    anyhow::ensure!(
        source.is_branch(),
        "choose a source branch below refs/heads/"
    );
    Ok(source)
}

async fn merge_preview(
    State(state): State<Arc<UiState>>,
    headers: HeaderMap,
    Json(body): Json<PreviewBody>,
) -> Response {
    if !review_ui::may_write(&headers, &state) {
        return review_ui::write_refused();
    }
    let source = match source_ref(&body.source) {
        Ok(source) => source,
        Err(error) => return review_ui::refusal(StatusCode::BAD_REQUEST, &error),
    };
    let answer = async {
        // Preview observes a running, verified runtime. It must not start a
        // daemon or open repository recovery as a side effect of previewing.
        let (_, port) = crate::daemon_client::repo_daemon_recorded_endpoint(state.layout.root());
        let port = port.filter(|port| *port != 0).context("No daemon endpoint is recorded. Open repository content before requesting a merge preview.")?;
        DaemonClient::from_running_workspace(format!("http://127.0.0.1:{port}"), &state.layout)
            .await?
            .merge_preview(&super::merge::MergePreviewRequest { source })
            .await
    }
    .await;
    match answer {
        Ok(preview) => Json(preview).into_response(),
        Err(error) => review_ui::refusal(StatusCode::BAD_GATEWAY, &error),
    }
}

/// Confirmation never reached the non-idempotent daemon transport, so this
/// attempt is conclusively refused even if acquiring a runtime failed.
fn merge_not_dispatched(operation_id: kin_model::OperationId, error: &anyhow::Error) -> Response {
    (StatusCode::CONFLICT, Json(serde_json::json!({
        "error":format!("{error:#}"), "outcome":"refused",
        "operation_id":operation_id,
        "remedy":"The merge request was not dispatched. Resolve the reported precondition, then read a new preview before confirming again.",
    }))).into_response()
}

async fn merge_confirm(
    State(state): State<Arc<UiState>>,
    headers: HeaderMap,
    Json(body): Json<ConfirmBody>,
) -> Response {
    if !review_ui::may_write(&headers, &state) {
        return review_ui::write_refused();
    }
    let operation_id = body.operation_id;
    let prepared = (|| {
        anyhow::ensure!(
            !body.operation_id.0.is_nil(),
            "a nonempty operation ID is required"
        );
        let source = source_ref(&body.source)?;
        anyhow::ensure!(
            source == body.expected.source_ref,
            "source differs from the displayed preview"
        );
        // Attribution resolves against this URL's repository, never the process
        // working directory or a caller-supplied actor.
        let actor = super::require_commit_author_for(&state.layout)?;
        Ok::<_, anyhow::Error>(super::merge::MergeConfirmRequest {
            source,
            operation_id: body.operation_id,
            actor,
            expected: body.expected,
        })
    })();
    let request = match prepared {
        Ok(request) => request,
        Err(error) => return merge_not_dispatched(operation_id, &error),
    };
    let client = match review_ui::daemon(&state).await {
        Ok(client) => client,
        Err(error) => return merge_not_dispatched(operation_id, &error),
    };
    match client.merge_confirm(&request).await {
        Ok(response) => Json(response).into_response(),
        Err(error) if error.downcast_ref::<crate::daemon_client::RefusedBeforeWriteError>().is_some() => {
            (StatusCode::CONFLICT, Json(serde_json::json!({
                "error":format!("{error:#}"), "outcome":"refused",
                "operation_id":request.operation_id,
                "remedy":"The daemon refused before publication. Read a new preview before confirming again.",
            }))).into_response()
        }
        // A transport failure can follow a committed operation. Return its
        // identity for reconciliation and never replay it behind the page.
        Err(error) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({
            "error":format!("{error:#}"), "outcome":"unknown",
            "operation_id":request.operation_id,
            "remedy":"Refresh repository and conflict state before another merge. This request was not retried.",
        }))).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitBody {
    message: String,
    operation_id: kin_model::OperationId,
    /// Commit only these entities' changes, as the uncommitted-work guide
    /// names them, and leave every other change uncommitted.
    #[serde(default)]
    entities: Option<Vec<String>>,
}

/// The most entities one commit from the page may choose.
const COMMIT_ENTITIES_MAX: usize = 10_000;

/// This attempt recorded nothing: it stopped before the daemon was asked, or
/// the daemon refused before publication.
fn commit_refused(operation_id: kin_model::OperationId, error: &anyhow::Error) -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error":format!("{error:#}"), "outcome":"refused",
            "operation_id":operation_id,
            "remedy":"Nothing was recorded. Resolve what is reported, then commit again.",
        })),
    )
        .into_response()
}

/// Commit the whole working copy, as `kin commit -m` does, or only the
/// entities the body names, as `kin commit -m --entity` does.
async fn commit(
    State(state): State<Arc<UiState>>,
    headers: HeaderMap,
    Json(body): Json<CommitBody>,
) -> Response {
    if !review_ui::may_write(&headers, &state) {
        return review_ui::write_refused();
    }
    let operation_id = body.operation_id;
    let message = body.message.trim().to_string();
    let entities = body.entities.unwrap_or_default();
    let chose = !entities.is_empty();
    let checked = (|| {
        anyhow::ensure!(
            !operation_id.0.is_nil(),
            "a nonempty operation ID is required"
        );
        anyhow::ensure!(!message.is_empty(), "a commit needs a message");
        anyhow::ensure!(
            message.chars().count() <= COMMIT_MESSAGE_MAX_CHARS,
            "a commit message is at most {COMMIT_MESSAGE_MAX_CHARS} characters"
        );
        anyhow::ensure!(
            entities.len() <= COMMIT_ENTITIES_MAX,
            "one commit chooses at most {COMMIT_ENTITIES_MAX} entities"
        );
        anyhow::ensure!(
            entities.iter().all(|entity| !entity.trim().is_empty()),
            "every chosen entity needs its id"
        );
        Ok(())
    })();
    if let Err(error) = checked {
        return commit_refused(operation_id, &error);
    }
    let title = message
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    // Run to its end whatever becomes of this request. A page that closes
    // mid-commit must not leave the daemon's answer unread, and attribution
    // resolves against this URL's repository, never a caller-supplied actor.
    let chosen = entities.clone();
    let committing = tokio::spawn(async move {
        super::commit::commit_for_app(&state.layout, &message, operation_id, &chosen).await
    });
    let outcome = committing.await.unwrap_or_else(|error| {
        super::commit::AppCommit::Unknown(anyhow::anyhow!("the commit did not finish: {error}"))
    });
    match outcome {
        super::commit::AppCommit::Committed { change_id } => {
            let mut answer = serde_json::json!({
                "operation_id":operation_id, "outcome":"committed",
                "change_id":change_id, "title":title,
            });
            if chose {
                answer["entities"] = serde_json::json!(entities);
            }
            Json(answer).into_response()
        }
        super::commit::AppCommit::NothingToCommit => Json(serde_json::json!({
            "operation_id":operation_id, "outcome":"nothing_to_commit",
        }))
        .into_response(),
        super::commit::AppCommit::Refused(error) => commit_refused(operation_id, &error),
        // A lost answer can follow a recorded change. Return its identity for
        // reconciliation and never replay it behind the page.
        super::commit::AppCommit::Unknown(error) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({
            "error":format!("{error:#}"), "outcome":"unknown",
            "operation_id":operation_id,
            "remedy":"The commit may have been recorded. Refresh recent changes before committing again. This request was not retried.",
        }))).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntityRestoreBody {
    entity: String,
    #[serde(default)]
    to: Option<String>,
    operation_id: kin_model::OperationId,
    #[serde(default)]
    expected: Option<String>,
}

/// This restore wrote nothing: it stopped before the daemon was asked, or the
/// daemon refused before writing.
fn restore_refused(operation_id: kin_model::OperationId, error: &anyhow::Error) -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error":format!("{error:#}"), "outcome":"refused",
            "operation_id":operation_id,
            "remedy":"Nothing was changed. Resolve what is reported, then restore it again.",
        })),
    )
        .into_response()
}

/// Put one entity back as it was at the head, or at an earlier change, as an
/// uncommitted edit, as `kin checkout --entity` does.
async fn entity_restore(
    State(state): State<Arc<UiState>>,
    headers: HeaderMap,
    Json(body): Json<EntityRestoreBody>,
) -> Response {
    if !review_ui::may_write(&headers, &state) {
        return review_ui::write_refused();
    }
    let operation_id = body.operation_id;
    let entity = body.entity.trim().to_string();
    let checked = (|| {
        anyhow::ensure!(
            !operation_id.0.is_nil(),
            "a nonempty operation ID is required"
        );
        anyhow::ensure!(!entity.is_empty(), "a restore names the entity it restores");
        Ok(())
    })();
    if let Err(error) = checked {
        return restore_refused(operation_id, &error);
    }
    // Run to its end whatever becomes of this request, like a commit: a page
    // that closes mid-restore must not leave the daemon's answer unread.
    let (to, expected) = (body.to, body.expected);
    let restoring = tokio::spawn(async move {
        super::entity_work::restore_for_app(&state.layout, entity, to, expected, operation_id).await
    });
    let outcome = restoring.await.unwrap_or_else(|error| {
        super::entity_work::AppRestore::Unknown(anyhow::anyhow!(
            "the restore did not finish: {error}"
        ))
    });
    match outcome {
        super::entity_work::AppRestore::Restored(restored) => Json(serde_json::json!({
            "operation_id":operation_id, "outcome":"restored",
            "entity":restored.entity, "name":restored.name,
            "to":restored.restored_to, "replayed":restored.replayed,
        }))
        .into_response(),
        super::entity_work::AppRestore::Refused(error) => restore_refused(operation_id, &error),
        // A lost answer can follow a restore that landed. Return its identity
        // for reconciliation and never replay it behind the page.
        super::entity_work::AppRestore::Unknown(error) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({
            "error":format!("{error:#}"), "outcome":"unknown",
            "operation_id":operation_id,
            "remedy":"The entity may have been restored. Refresh the uncommitted work before restoring it again. This request was not retried.",
        }))).into_response(),
    }
}

/// How much of a repository's history its store holds, from the store's own
/// record of it. `complete` is a store with its whole history, which a
/// repository that began in Kin always is. `partial` is a converted project
/// whose one admitted commit is being followed by the rest in the background.
/// `none` is a store that holds that one commit and derives no more: it was
/// asked not to, or the project it came from has no more to give.
///
/// A partial store also says where the rest of its history stands. `deriving`
/// is true while it is being read, and `step` and `steps` then say how far
/// the reading has come, when the worker has said. `staged` is true once all
/// of it has been read and the repository has only to switch to it, which it
/// does at its next quiet point; `admitted` stays at what the repository shows
/// until then. `waiting_for` is there while that switch is held back, and
/// says what for, as a clause that follows "waiting for".
pub(super) fn history_json(
    standing: &kin_core::history_state::HistoryStanding,
) -> serde_json::Value {
    use kin_core::history_state::{DerivationState, HistoryMode, HistoryStanding};
    match standing {
        HistoryStanding::Complete => serde_json::json!({"state":"complete"}),
        // A record that cannot be read is treated as incomplete, as every
        // other surface treats it, and no count is claimed for it.
        HistoryStanding::Unreadable { .. } => serde_json::json!({"state":"partial"}),
        HistoryStanding::Partial { record, progress } => {
            let staged = progress
                .as_ref()
                .is_some_and(|progress| progress.state == DerivationState::Staged);
            // A history that has been read is on its way in whatever mode the
            // store records, so it is never shown as one that will not come.
            let filling =
                staged || (record.mode == HistoryMode::Background && record.underivable.is_none());
            let running = super::daemon_activity::running_history(standing);
            let mut history = serde_json::json!({
                "state": if filling { "partial" } else { "none" },
                "admitted": record.admitted_commits,
                "deriving": running.is_some(),
                "staged": staged,
            });
            if let Some((step, steps)) = running
                .as_ref()
                .and_then(|running| running.step.zip(running.steps))
            {
                history["step"] = serde_json::json!(step);
                history["steps"] = serde_json::json!(steps);
            }
            if let Some(total) = record.reachable_commits {
                history["total"] = serde_json::json!(total);
            }
            if let Some(waiting_for) = progress
                .as_ref()
                .and_then(|progress| progress.staged_waiting_for())
            {
                history["waiting_for"] = serde_json::json!(waiting_for);
            }
            history
        }
    }
}

/// A repository's identity for the page: its remote's owner, name and host
/// when Kin has a remote for it, otherwise its working tree's name alone.
/// `fields` carries whatever else the answer names it by.
pub(super) fn repository_identity(
    state: &UiState,
    mut fields: serde_json::Value,
) -> serde_json::Value {
    let (name, origin) = display_identity(&state.layout);
    fields["name"] = serde_json::Value::String(name);
    if let Some(origin) = origin {
        if let Some(owner) = origin.owner {
            fields["owner"] = serde_json::Value::String(owner);
        }
        fields["host"] = serde_json::Value::String(origin.host);
    }
    fields
}

async fn session(State(state): State<Arc<UiState>>) -> Response {
    // Read recorded endpoint metadata and verify health without discovery,
    // endpoint cleanup, startup or process control. Other resource
    // reads use the ordinary command resolver when the user opens that resource.
    let observe = tokio::time::timeout(Duration::from_secs(5), async {
        let (pid, port) = crate::daemon_client::repo_daemon_recorded_endpoint(state.layout.root());
        match (pid, port) {
            (Some(_), Some(port)) if port != 0 => {
                let client = DaemonClient::from_running_workspace(
                    format!("http://127.0.0.1:{port}"),
                    &state.layout,
                )
                .await?;
                let health = client.health().await?;
                // Whether the repository declares a test command Kin can run,
                // read from its manifests by the same daemon a run would use,
                // as it last said: a daemon that is writing can take most of
                // a minute to say, and nothing on the page waits for it. One
                // that cannot say leaves it unknown.
                let tests = super::workspace_verify::test_command_known(&state, client).flatten();
                Ok::<_, anyhow::Error>((
                    serde_json::json!({"state":"running","detail":health.status,
                        "graph_loaded":health.graph_loaded,
                        "reconciliation":health.reconciliation_status}),
                    health.repo_id,
                    health.workspace_id,
                    tests,
                ))
            }
            _ => Ok((
                serde_json::json!({"state":"unknown", "detail":"No complete local daemon endpoint is recorded. Opening repository content uses Kin's normal startup path."}),
                None,
                None,
                None,
            )),
        }
    });
    // Whether Git can carry a branch for this project, asked of Git at most
    // once in a while, beside the runtime reading rather than after it. A
    // session never asks a remote anything, and never waits for the daemon
    // to say whether this branch can update from Git: it says what is known,
    // and that it is checking when nothing is yet.
    let (observed, git) = tokio::join!(observe, super::workspace_git_remote::ready(&state));
    let git_pull = super::workspace_git_remote::update_ready_known(&state, &git);
    let (runtime, repository_id, workspace_id, verify_unavailable) = match observed {
        Ok(Ok(reading)) => reading,
        Ok(Err(error)) => (
            serde_json::json!({"state":"unknown", "detail":format!("{error:#}")}),
            None,
            None,
            None,
        ),
        Err(_) => (
            serde_json::json!({"state":"unknown", "detail":"Runtime observation did not finish within five seconds. Retry to check again."}),
            None,
            None,
            None,
        ),
    };
    let key = repository_key(state.layout.working_dir());
    // Read for this answer, so a repository added since the server started
    // is in the list the page switches between.
    let (listing, history, transfer) = {
        let state = state.clone();
        tokio::task::spawn_blocking(move || {
            let listing = state.catalog.as_ref().map(|catalog| catalog.read());
            // The store's own record of its history, so the page can say
            // when the record of changes is still filling in.
            let standing = kin_core::history_state::standing(&state.layout);
            let machine: Vec<_> = listing
                .iter()
                .flat_map(|listing| &listing.repositories)
                .map(|listed| listed.layout.clone())
                .collect();
            let transfer =
                session_transfer(&state.layout, &standing, &machine, &git, git_pull.as_ref());
            (listing, history_json(&standing), transfer)
        })
        .await
        .unwrap_or_else(|_| {
            let unread = || Err("Kin could not read this repository's remotes.".to_string());
            (
                None,
                serde_json::Value::Null,
                SessionTransfer {
                    push: unread(),
                    pull: unread(),
                    fetch: false,
                    pull_checking: false,
                },
            )
        })
    };
    let SessionTransfer {
        push: remote_push,
        pull: remote_pull,
        fetch: remote_fetch,
        pull_checking,
    } = transfer;
    let repositories: Vec<_> = listing.iter().flat_map(|listing| &listing.repositories).map(|listed| {
        let item = &listed.entry;
        serde_json::json!({"key":item.key,"name":item.name,"owner":item.owner,"host":item.host,"url":item.url,"selected":item.key == key})
    }).collect();
    Json(serde_json::json!({
        "repository":repository_identity(&state, serde_json::json!({"key":key,"repository_id":repository_id,"workspace_id":workspace_id})),
        "repositories":repositories,
        "registry_warning":listing.as_ref().and_then(|listing| listing.warning.as_deref()),
        "catalog_basis":"Repositories registered on this machine, read for this answer.",
        "capabilities":{"graph":true,"review":true,"merge":true,"commit":true,"entity_commit":true,"entity_restore":true,"editor":true,"branch_switch":true,"branch_switch_unavailable_reason":null,"branch_create":true,"conflict_details":true,"conflict_settle":true,
            "remotes":true,
            "remote_push":remote_push.is_ok(),"remote_pull":remote_pull.is_ok(),"remote_fetch":remote_fetch,
            "remote_unavailable_reason":remote_push.as_ref().err(),
            "remote_pull_unavailable_reason":remote_pull.as_ref().err(),
            "remote_pull_checking":pull_checking,
            "review_flags":true,"verify_run":verify_unavailable.is_none(),"verify_run_unavailable_reason":verify_unavailable},
        "history":history,
        "runtime":runtime,"version":env!("CARGO_PKG_VERSION"),"actor":state.actor,"token":state.token,
        "review_url":format!("{}/review/",state.base_path),
        "graph_url":format!("{}/workspace/#/entities",state.base_path),
    })).into_response()
}

/// Whether a push, a pull and a fetch can work here, as a session says it.
struct SessionTransfer {
    push: Result<(), String>,
    pull: Result<(), String>,
    fetch: bool,
    /// The pull depends on whether this branch can update from Git, which
    /// is being asked and is not known yet. `pull` then says nothing either
    /// way, and the page asks `/api/workspace/session/pull` for it.
    pull_checking: bool,
}

/// What a push, a pull and a fetch can do here, from configuration,
/// credentials, what Git said, and, when it is known, whether this branch
/// can update from Git. Blocking.
fn session_transfer(
    layout: &kin_core::KinLayout,
    standing: &kin_core::history_state::HistoryStanding,
    machine: &[kin_core::KinLayout],
    git: &Result<(), String>,
    git_pull: Option<&Result<(), String>>,
) -> SessionTransfer {
    let capability = |git_pull: &Result<(), String>| {
        super::workspace_remotes::transfer_capability(layout, standing, machine, git, git_pull)
    };
    if let Some(git_pull) = git_pull {
        let (push, pull, fetch) = capability(git_pull);
        return SessionTransfer {
            push,
            pull,
            fetch,
            pull_checking: false,
        };
    }
    // Not known yet. When the answer would not change what a pull can do,
    // as with no Git remote, or one that cannot pull for another reason,
    // the session says so; otherwise it says the pull is being checked.
    let (push, pull_if_ready, fetch) = capability(&Ok(()));
    let (_, pull_if_not, _) = capability(&Err(String::new()));
    if pull_if_ready == pull_if_not {
        return SessionTransfer {
            push,
            pull: pull_if_ready,
            fetch,
            pull_checking: false,
        };
    }
    SessionTransfer {
        push,
        pull: Err(PULL_CHECKING.to_string()),
        fetch,
        pull_checking: true,
    }
}

/// What a session says of a pull while it checks whether this branch can
/// update from Git.
const PULL_CHECKING: &str = "Kin is checking whether this branch can update from Git.";

/// GET /api/workspace/session/pull: whether a pull can work here, once Kin
/// has asked the repository's daemon whether this branch can update from
/// Git. A session answers without waiting for that ask and says
/// `remote_pull_checking`; the page reads the pull from here.
async fn session_pull(State(state): State<Arc<UiState>>) -> Response {
    let git = super::workspace_git_remote::ready(&state).await;
    let git_pull = super::workspace_git_remote::update_ready(&state).await;
    let transfer = {
        let state = state.clone();
        tokio::task::spawn_blocking(move || {
            let listing = state.catalog.as_ref().map(|catalog| catalog.read());
            let standing = kin_core::history_state::standing(&state.layout);
            let machine: Vec<_> = listing
                .iter()
                .flat_map(|listing| &listing.repositories)
                .map(|listed| listed.layout.clone())
                .collect();
            session_transfer(&state.layout, &standing, &machine, &git, Some(&git_pull))
        })
        .await
    };
    match transfer {
        Ok(transfer) => Json(serde_json::json!({
            "remote_pull": transfer.pull.is_ok(),
            "remote_pull_unavailable_reason": transfer.pull.as_ref().err(),
            "remote_pull_checking": false,
            "remote_fetch": transfer.fetch,
        }))
        .into_response(),
        Err(_) => review_ui::refusal(
            StatusCode::INTERNAL_SERVER_ERROR,
            &anyhow::anyhow!("Kin could not read this repository's remotes."),
        ),
    }
}

/// Canonicalize the explicitly selected repository at the opening boundary.
/// This locates its Kin authority; no semantic answer comes from host bytes.
fn workspace_layout(path: &Path) -> Result<kin_core::KinLayout> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("cannot open {}", path.display()))?;
    super::require_repository_layout_at(&canonical)
}

/// One repository as this server serves it: its state, and its routes.
///
/// The routes are held as a `MethodRouter` because that is the form axum
/// itself calls with a request and a state. A router nested at startup is
/// fixed for the life of the server; one called this way is chosen per
/// request, so a repository can be served the first time it is asked for.
struct Mounted {
    state: Arc<UiState>,
    service: MethodRouter,
}

/// Everything one `kin ui` server answers for: the page that lists this
/// machine's repositories, and each repository it has been asked to show.
pub(super) struct Hub {
    pub(super) port: u16,
    /// Minted once per run for the page that lists repositories. A
    /// repository's own token is separate, so neither scope can write in the
    /// other.
    pub(super) token: String,
    /// Who the listing page shows its reader as.
    pub(super) actor: Option<String>,
    pub(super) catalog: Arc<Catalog>,
    mounted: RwLock<HashMap<String, Mounted>>,
    pub(super) home: super::workspace_home::Home,
}

impl Hub {
    pub(super) fn new(port: u16, catalog: Arc<Catalog>, actor: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            port,
            token: uuid::Uuid::new_v4().simple().to_string(),
            actor,
            catalog,
            mounted: RwLock::default(),
            home: Default::default(),
        })
    }

    fn mounted(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Mounted>> {
        self.mounted
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Serve `state` under `key`. A repository already served keeps the state
    /// it has, so its write token and what it remembers survive.
    fn mount(&self, key: String, state: Arc<UiState>) -> Arc<UiState> {
        let mut mounted = self
            .mounted
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        mounted
            .entry(key)
            .or_insert_with(|| Mounted {
                service: axum::routing::any_service(review_ui::workspace_router(state.clone())),
                state,
            })
            .state
            .clone()
    }

    /// Serve the repository at `layout` under its own key, and list it.
    /// Blocking: naming the reader reads the repository's configuration.
    pub(super) fn open(&self, layout: kin_core::KinLayout) -> Arc<UiState> {
        let key = repository_key(layout.working_dir());
        if let Some(state) = self.state(&key) {
            return state;
        }
        self.catalog.remember(&layout);
        let state = Arc::new(UiState::workspace(
            layout,
            self.port,
            format!("/repositories/{key}"),
            self.catalog.clone(),
        ));
        self.mount(key, state)
    }

    /// The state serving `key`, when this server has been asked for it.
    pub(super) fn state(&self, key: &str) -> Option<Arc<UiState>> {
        self.mounted().get(key).map(|mounted| mounted.state.clone())
    }

    /// The routes for `key`. A repository this server has not served yet is
    /// looked for in the registry as it is now, which is how one added after
    /// startup answers without a restart.
    async fn service(self: &Arc<Self>, key: &str) -> Option<MethodRouter> {
        let served = |hub: &Self| {
            hub.mounted()
                .get(key)
                .map(|mounted| mounted.service.clone())
        };
        if let Some(service) = served(self) {
            return Some(service);
        }
        // A key is the hex of a digest. Anything else names no repository,
        // and is refused before the registry is read for it.
        if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let hub = self.clone();
        let wanted = key.to_string();
        tokio::task::spawn_blocking(move || {
            let listed = hub
                .catalog
                .read()
                .repositories
                .into_iter()
                .find(|listed| listed.entry.key == wanted)?;
            Some(hub.open(listed.layout))
        })
        .await
        .ok()
        .flatten()?;
        served(self)
    }
}

/// Split `/repositories/<key>/<rest>` into the key and the request the
/// repository's own routes see.
fn repository_scope(uri: &Uri) -> Option<(&str, Uri)> {
    let scoped = uri.path().strip_prefix("/repositories/")?;
    let (key, rest) = scoped.split_once('/').unwrap_or((scoped, ""));
    if key.is_empty() {
        return None;
    }
    let mut inner = format!("/{rest}");
    if let Some(query) = uri.query() {
        inner.push('?');
        inner.push_str(query);
    }
    Some((key, inner.parse().ok()?))
}

/// Hand a request under `/repositories/<key>` to that repository's routes.
async fn serve_repository(State(hub): State<Arc<Hub>>, mut request: Request) -> Response {
    let unknown = || {
        (
            StatusCode::NOT_FOUND,
            "no repository on this machine answers at this address\n",
        )
            .into_response()
    };
    let Some((key, inner)) = repository_scope(request.uri()) else {
        return unknown();
    };
    let Some(service) = hub.service(key).await else {
        return unknown();
    };
    *request.uri_mut() = inner;
    service.call(request, ()).await
}

async fn serve_hub_page() -> Response {
    page("")
}

/// Every route of one server: the listing page and its API at the root, and
/// each repository below its own key. The host check and the page headers
/// cover all of them.
pub(super) fn hub_router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/", get(serve_hub_page))
        .route("/review/highlight.js", get(review_ui::serve_highlight_js))
        .merge(page_assets())
        .merge(super::workspace_home::routes())
        .fallback(serve_repository)
        .layer(axum::middleware::from_fn_with_state(
            hub.port,
            review_ui::loopback_only,
        ))
        .with_state(hub)
}

/// The repository this command was run in, when it was run in one.
fn current_repository() -> Option<kin_core::KinLayout> {
    match super::require_repository_layout() {
        Ok(layout) => Some(layout),
        Err(error) => {
            // Outside any repository is the ordinary case and needs no words:
            // the listing page opens. Any other reason is one the reader can
            // act on, so it is said before that page opens.
            if error.to_string() != super::NOT_A_KIN_REPOSITORY {
                eprintln!("kin: {error:#}");
            }
            None
        }
    }
}

/// Serve this machine's repositories on a single local origin. Run in a
/// repository, or given one, its workspace opens; anywhere else the page that
/// lists them does. No repository is initialized and no daemon is started to
/// list them.
pub async fn run(path: Option<PathBuf>, port: Option<u16>, open_browser: bool) -> Result<()> {
    let selected = match path {
        Some(path) => Some(workspace_layout(&path)?),
        None => current_repository(),
    };
    let listener = review_ui::bind(port).await?;
    let port = listener.local_addr()?.port();
    let actor = super::workspace_home::reader(selected.as_ref());
    let hub = Hub::new(port, Catalog::live(), actor);
    let opening = match selected {
        Some(layout) => format!("{}/workspace/", hub.open(layout).base_path),
        None => "/".to_string(),
    };
    let url = format!("http://127.0.0.1:{port}{opening}");
    println!("Kin workspace at {url}");
    println!("Served from this machine only. Press Ctrl-C to stop this page server.");
    if open_browser {
        tokio::task::spawn_blocking(move || {
            if let Err(error) = open::that(&url) {
                // The launcher's own error is for a log, not for a person.
                tracing::debug!(%error, "opening a browser failed");
                eprintln!("No browser opened here. Open {url} in your browser.");
            }
        });
    }
    axum::serve(listener, hub_router(hub))
        .await
        .context("Kin workspace server error")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_keys_cannot_be_route_syntax_or_alias_different_roots() {
        let first = repository_key(Path::new("/repository/a?<b>"));
        let second = repository_key(Path::new("/repository/b"));
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    /// The page is told when the whole history has been read and the
    /// repository has only to switch to it, and what that switch waits for,
    /// so it never says a history is still being read when it is not.
    #[test]
    fn a_staged_history_is_reported_as_read_and_waiting_to_switch() {
        use kin_core::history_state::{
            self, DerivationState, HistoryDerivationProgress, HistoryMode, HistoryStateRecord,
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".kin");
        std::fs::create_dir_all(&root).unwrap();
        let layout = kin_core::KinLayout::new(root);
        let stamp = |mode| {
            history_state::stamp_staged(
                &layout,
                &HistoryStateRecord::new(
                    mode,
                    "0123456789abcdef0123456789abcdef01234567".into(),
                    None,
                    "snapshot".into(),
                    Vec::new(),
                    Some(4_673),
                    None,
                ),
            )
            .unwrap();
        };
        stamp(HistoryMode::Background);
        let read = || history_json(&history_state::standing(&layout));

        // Nothing derived yet: the fields the page already reads, and staged
        // answered rather than left out.
        assert_eq!(
            read(),
            serde_json::json!({
                "state": "partial", "admitted": 1, "total": 4_673,
                "deriving": false, "staged": false,
            })
        );

        // Being read: how far the reading has come, when the worker said.
        let mut running = HistoryDerivationProgress::new(DerivationState::Running);
        running.step = Some(11);
        running.steps = Some(16);
        running.pid = None;
        history_state::write_progress(&layout, &running).unwrap();
        assert_eq!(
            read(),
            serde_json::json!({
                "state": "partial", "admitted": 1, "total": 4_673,
                "deriving": true, "staged": false, "step": 11, "steps": 16,
            })
        );

        let mut staged = HistoryDerivationProgress::new(DerivationState::Staged);
        staged.detail = Some("/stages/one".into());
        history_state::write_progress(&layout, &staged).unwrap();
        assert_eq!(
            read(),
            serde_json::json!({
                "state": "partial", "admitted": 1, "total": 4_673,
                "deriving": false, "staged": true,
            })
        );

        history_state::note_staged_wait(&layout, Some("an open merge to be finished")).unwrap();
        let waiting = read();
        assert_eq!(waiting["staged"], true);
        assert_eq!(waiting["waiting_for"], "an open merge to be finished");
        // Where the staged store is kept is never part of the answer.
        assert!(!waiting.to_string().contains("stages"), "{waiting}");

        // A store asked to derive nothing whose history was read anyway is on
        // its way to having it, not a store that will never have it.
        stamp(HistoryMode::None);
        assert_eq!(read()["state"], "partial");
        history_state::clear_progress(&layout).unwrap();
        assert_eq!(read()["state"], "none");
        assert_eq!(read()["staged"], false);
    }

    /// One server with two repositories mounted, the way `run` composes it,
    /// and no registry behind it. Neither layout holds a repository, so every
    /// route under test answers before any daemon is looked for.
    async fn serve_two() -> (String, Vec<String>, Arc<Hub>, tokio::task::JoinHandle<()>) {
        let listener = review_ui::bind(Some(0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let catalog = empty_catalog();
        let hub = Hub::new(port, catalog.clone(), Some("ada".into()));
        let mut tokens = Vec::new();
        for key in ["first", "second"] {
            let state = Arc::new(UiState::workspace(
                kin_core::KinLayout::new(std::env::temp_dir().join(format!("kin-ui-{key}/.kin"))),
                port,
                format!("/repositories/{key}"),
                catalog.clone(),
            ));
            tokens.push(state.token.clone());
            hub.mount(key.to_string(), state);
        }
        let router = hub_router(hub.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (format!("http://127.0.0.1:{port}"), tokens, hub, server)
    }

    #[test]
    fn a_repository_request_is_split_into_its_key_and_its_own_address() {
        let split = |address: &str| {
            let uri: Uri = address.parse().unwrap();
            repository_scope(&uri).map(|(key, inner)| (key.to_string(), inner.to_string()))
        };
        assert_eq!(
            split("/repositories/abc/api/workspace/entity?id=a%2Fb&x=1"),
            Some(("abc".into(), "/api/workspace/entity?id=a%2Fb&x=1".into()))
        );
        assert_eq!(
            split("/repositories/abc/workspace/"),
            Some(("abc".into(), "/workspace/".into()))
        );
        assert_eq!(split("/repositories/abc"), Some(("abc".into(), "/".into())));
        assert_eq!(split("/repositories/"), None);
        assert_eq!(split("/repositories//workspace/"), None);
        assert_eq!(split("/elsewhere/abc/workspace/"), None);
    }

    /// The page at the root belongs to no repository: it is the same page
    /// with an empty base, under the same host check and headers, and its
    /// write authority is its own.
    #[tokio::test]
    async fn the_listing_page_is_served_at_the_root_with_its_own_scope_and_token() {
        let (origin, tokens, hub, server) = serve_two().await;
        let port = hub.port;
        let client = reqwest::Client::new();
        let page = client.get(format!("{origin}/")).send().await.unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        assert_eq!(page.headers()["cache-control"], "no-store");
        assert!(page.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'"));
        let page = page.text().await.unwrap();
        assert_eq!(page, INDEX.replace("__KIN_BASE__", ""));
        assert!(!page.contains("__KIN_BASE__"));
        for asset in [
            "/workspace/style.css",
            "/workspace/workspace.js",
            "/workspace/graph.js",
            "/workspace/agents.js",
            "/workspace/agents.css",
            "/workspace/mark.svg",
            "/review/highlight.js",
        ] {
            let served = client.get(format!("{origin}{asset}")).send().await.unwrap();
            assert_eq!(served.status(), StatusCode::OK, "{asset}");
            assert_eq!(served.headers()["x-content-type-options"], "nosniff");
            let scoped = client
                .get(format!("{origin}/repositories/first{asset}"))
                .send()
                .await
                .unwrap();
            assert_eq!(
                served.text().await.unwrap(),
                scoped.text().await.unwrap(),
                "{asset}"
            );
        }
        // The host check covers the root, its API and an address that names
        // no repository alike, before any of them is looked up.
        for path in [
            "/",
            "/api/home",
            "/api/home/session",
            "/repositories/unknown/workspace/",
        ] {
            let refused = client
                .get(format!("{origin}{path}"))
                .header("host", format!("foreign.example:{port}"))
                .send()
                .await
                .unwrap();
            assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{path}");
        }
        for unknown in [
            "/repositories/unknown/workspace/".to_string(),
            format!("/repositories/{}/workspace/", "0".repeat(64)),
            "/repositories/".to_string(),
            "/elsewhere".to_string(),
        ] {
            let missing = client
                .get(format!("{origin}{unknown}"))
                .send()
                .await
                .unwrap();
            assert_eq!(missing.status(), StatusCode::NOT_FOUND, "{unknown}");
        }

        let session: serde_json::Value = client
            .get(format!("{origin}/api/home/session"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(session["schema"], "kin.workspace.home-session.v1");
        assert_eq!(session["actor"], "ada");
        assert_eq!(session["version"], env!("CARGO_PKG_VERSION"));
        let token = session["token"].as_str().unwrap().to_string();
        assert_eq!(token, hub.token);
        assert!(!tokens.contains(&token));

        let home: serde_json::Value = client
            .get(format!("{origin}/api/home"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(home["schema"], "kin.workspace.home.v1");
        assert_eq!(home["repositories"], serde_json::json!([]));
        assert_eq!(home["unavailable"], 0);
        assert_eq!(home["warning"], serde_json::Value::Null);

        // Adding a repository is a write in the listing page's scope: it
        // needs that page's token and its own origin, and a repository's
        // token carries no authority there.
        let add = format!("{origin}/api/home/repositories");
        let absent = std::env::temp_dir().join(format!("kin-ui-absent-{}", uuid::Uuid::new_v4()));
        let body = serde_json::json!({"action":"open","path":absent});
        for (sent, request_origin) in [
            ("", origin.as_str()),
            (tokens[0].as_str(), origin.as_str()),
            (token.as_str(), "https://foreign.example"),
        ] {
            let refused = client
                .post(&add)
                .header("x-kin-review-token", sent)
                .header("origin", request_origin)
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        }
        // With authority, a request that cannot be met is refused in words
        // before anything is started.
        let nothing_there = client
            .post(&add)
            .header("x-kin-review-token", &token)
            .header("origin", &origin)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(nothing_there.status(), StatusCode::CONFLICT);
        let refusal: serde_json::Value = nothing_there.json().await.unwrap();
        assert_eq!(refusal["error"], "Nothing is at this location.");
        let relative = client
            .post(&add)
            .header("x-kin-review-token", &token)
            .header("origin", &origin)
            .json(&serde_json::json!({"action":"convert","path":"project"}))
            .send()
            .await
            .unwrap();
        assert_eq!(relative.status(), StatusCode::BAD_REQUEST);
        let injected = client
            .post(&add)
            .header("x-kin-review-token", &token)
            .header("origin", &origin)
            .json(&serde_json::json!({"action":"open","path":absent,"actor":"caller"}))
            .send()
            .await
            .unwrap();
        assert_eq!(injected.status(), StatusCode::UNPROCESSABLE_ENTITY);
        // And the listing page's token writes nothing in a repository.
        let crossed = client
            .post(format!("{origin}/repositories/first/api/workspace/commit"))
            .header("x-kin-review-token", &token)
            .header("origin", &origin)
            .json(&serde_json::json!({
                "message":"work", "operation_id":kin_model::OperationId::new(),
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(crossed.status(), StatusCode::FORBIDDEN);

        let inspected: serde_json::Value = client
            .post(format!("{origin}/api/home/inspect"))
            .json(&serde_json::json!({"path":absent}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(inspected["kind"], "missing");
        assert_eq!(inspected["exists"], false);
        assert_eq!(inspected["registered"], false);
        assert!(inspected["repository"].is_null());
        assert!(inspected["note"]
            .as_str()
            .unwrap()
            .contains("Nothing is at"));
        let operations: serde_json::Value = client
            .get(format!("{origin}/api/home/operations"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(operations["operations"], serde_json::json!([]));
        let unknown = client
            .get(format!(
                "{origin}/api/home/operations/{}",
                uuid::Uuid::new_v4()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
        server.abort();
    }

    /// A commit from the page is refused in words, with nothing dispatched,
    /// when its preconditions fail.
    #[tokio::test]
    async fn a_commit_needs_authority_a_message_and_an_operation_before_anything_is_asked() {
        let (origin, tokens, _hub, server) = serve_two().await;
        let client = reqwest::Client::new();
        let url = format!("{origin}/repositories/second/api/workspace/commit");
        let operation_id = kin_model::OperationId::new();
        for (token, request_origin) in [
            (tokens[0].as_str(), origin.as_str()),
            (tokens[1].as_str(), "https://foreign.example"),
            ("", origin.as_str()),
        ] {
            let refused = client
                .post(&url)
                .header("x-kin-review-token", token)
                .header("origin", request_origin)
                .json(&serde_json::json!({"message":"work","operation_id":operation_id}))
                .send()
                .await
                .unwrap();
            assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        }
        let long = "x".repeat(COMMIT_MESSAGE_MAX_CHARS + 1);
        for (message, id, reason) in [
            ("  \n ", serde_json::json!(operation_id), "needs a message"),
            (long.as_str(), serde_json::json!(operation_id), "at most"),
            (
                "work",
                serde_json::json!(uuid::Uuid::nil()),
                "operation ID is required",
            ),
        ] {
            let refused = client
                .post(&url)
                .header("x-kin-review-token", &tokens[1])
                .header("origin", &origin)
                .json(&serde_json::json!({"message":message,"operation_id":id}))
                .send()
                .await
                .unwrap();
            assert_eq!(refused.status(), StatusCode::CONFLICT);
            let body: serde_json::Value = refused.json().await.unwrap();
            assert_eq!(body["outcome"], "refused");
            assert_eq!(body["operation_id"], id);
            assert!(body["error"].as_str().unwrap().contains(reason), "{body}");
            assert!(body["remedy"]
                .as_str()
                .unwrap()
                .contains("Nothing was recorded"));
        }
        // A caller never names who a commit is by.
        let injected = client
            .post(&url)
            .header("x-kin-review-token", &tokens[1])
            .header("origin", &origin)
            .json(&serde_json::json!({
                "message":"work","operation_id":operation_id,"actor":"caller",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(injected.status(), StatusCode::UNPROCESSABLE_ENTITY);
        server.abort();
    }

    /// Push and pull from the page need its authority and an operation, and
    /// a repository with no Kin remote refuses them with nothing sent. The
    /// remotes reading and the session say the same without asking anyone.
    #[tokio::test]
    async fn push_and_pull_refuse_with_nothing_sent_when_they_cannot_run() {
        let (origin, tokens, _hub, server) = serve_two().await;
        let client = reqwest::Client::new();
        for verb in ["push", "pull", "fetch"] {
            let url = format!("{origin}/repositories/second/api/workspace/{verb}");
            let operation_id = kin_model::OperationId::new();
            for (token, request_origin) in [
                (tokens[0].as_str(), origin.as_str()),
                (tokens[1].as_str(), "https://foreign.example"),
                ("", origin.as_str()),
            ] {
                let refused = client
                    .post(&url)
                    .header("x-kin-review-token", token)
                    .header("origin", request_origin)
                    .json(&serde_json::json!({"operation_id":operation_id}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{verb}");
            }
            let send = |body: serde_json::Value| {
                client
                    .post(&url)
                    .header("x-kin-review-token", &tokens[1])
                    .header("origin", &origin)
                    .json(&body)
                    .send()
            };
            let nil = send(serde_json::json!({"operation_id":uuid::Uuid::nil()}))
                .await
                .unwrap();
            assert_eq!(nil.status(), StatusCode::CONFLICT);
            let nil: serde_json::Value = nil.json().await.unwrap();
            assert_eq!(nil["outcome"], "refused");
            assert!(nil["error"].as_str().unwrap().contains("operation ID"));
            // A caller never names the credential or the address.
            let injected = send(serde_json::json!({
                "operation_id":operation_id,"url":"http://127.0.0.1:1",
            }))
            .await
            .unwrap();
            assert_eq!(injected.status(), StatusCode::UNPROCESSABLE_ENTITY);
            // Refused before anything is sent, so the same operation may be
            // sent again once what is reported is corrected.
            for _ in 0..2 {
                let refused = send(serde_json::json!({
                    "operation_id":operation_id,"remote":null,"ref":"main",
                }))
                .await
                .unwrap();
                assert_eq!(refused.status(), StatusCode::CONFLICT, "{verb}");
                let refused: serde_json::Value = refused.json().await.unwrap();
                assert_eq!(refused["outcome"], "refused");
                assert_eq!(refused["operation_id"], serde_json::json!(operation_id));
                assert_eq!(refused["error"], "No remote is set up for this repository.");
                assert!(refused["remedy"]
                    .as_str()
                    .unwrap()
                    .starts_with("Nothing was sent."));
                assert!(refused.get("already_sent").is_none());
            }
            let named = send(serde_json::json!({"operation_id":operation_id,"remote":"origin"}))
                .await
                .unwrap();
            let named: serde_json::Value = named.json().await.unwrap();
            assert_eq!(
                named["error"],
                "No remote named origin is set up for this repository."
            );
        }

        let remotes: serde_json::Value = client
            .get(format!(
                "{origin}/repositories/second/api/workspace/remotes"
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            remotes["schema"],
            super::super::workspace_remotes::REMOTES_SCHEMA
        );
        assert_eq!(remotes["remotes"], serde_json::json!([]));
        assert_eq!(remotes["default_remote"], serde_json::Value::Null);
        assert_eq!(
            remotes["default_remote_reason"],
            "No remote is set up for this repository."
        );

        let session: serde_json::Value = client
            .get(format!(
                "{origin}/repositories/second/api/workspace/session"
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let capabilities = &session["capabilities"];
        assert_eq!(capabilities["remotes"], true);
        assert_eq!(capabilities["remote_push"], false);
        assert_eq!(capabilities["remote_pull"], false);
        assert_eq!(capabilities["remote_fetch"], false);
        assert_eq!(
            capabilities["remote_pull_unavailable_reason"],
            "No remote is set up for this repository."
        );
        assert_eq!(
            capabilities["remote_unavailable_reason"],
            "No remote is set up for this repository."
        );
        server.abort();
    }

    /// A session answers while the repository's daemon is slow to say
    /// whether this branch can update from Git. Measured on a busy axum, the
    /// session waited ten seconds on that question before every page could
    /// draw, and then said the daemon did not answer. It now says the pull is
    /// being checked, the page reads the pull on its own route, which joins
    /// the ask already running, and the next session has the answer.
    ///
    /// Breaking it: have the session wait for the ask again, and its answer
    /// takes the whole held ask.
    #[tokio::test]
    async fn a_session_answers_while_the_git_update_check_is_slow() {
        let temporary = tempfile::TempDir::new().unwrap();
        let layout = kin_core::KinLayout::new(temporary.path().join(".kin"));
        std::fs::create_dir_all(layout.root()).unwrap();
        let mut config = kin_core::KinConfig::default();
        config.git.remotes.push(kin_core::GitRemoteTransportConfig {
            name: "origin".into(),
            fetch_urls: vec!["https://github.invalid/example/demo.git".into()],
            push_urls: Vec::new(),
            fetch_refspecs: Vec::new(),
            push_refspecs: Vec::new(),
        });
        config.save(&layout.config_path()).unwrap();

        let listener = review_ui::bind(Some(0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let catalog = empty_catalog();
        let hub = Hub::new(port, catalog.clone(), None);
        let state = Arc::new(UiState::workspace(
            layout,
            port,
            "/repositories/slow".to_string(),
            catalog,
        ));
        // Git itself can carry the branch; only the daemon's answer is slow.
        state.transfers.remember_git_ready(Ok(()));
        hub.mount("slow".to_string(), state.clone());
        let router = hub_router(hub.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let held = Duration::from_secs(3);
        *super::super::workspace_git_remote::UPDATE_STATUS_PAUSE_FOR_TEST
            .lock()
            .unwrap() = Some((Arc::as_ptr(&state) as usize, held));

        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}/repositories/slow");
        let read = |path: &'static str| {
            let client = client.clone();
            let url = format!("{base}{path}");
            async move {
                let asked = std::time::Instant::now();
                let answer: serde_json::Value =
                    client.get(url).send().await.unwrap().json().await.unwrap();
                (answer, asked.elapsed())
            }
        };
        let (session, took) = read("/api/workspace/session").await;
        assert!(
            took < Duration::from_millis(1500),
            "the session waited {took:?} for the Git update check"
        );
        let capabilities = &session["capabilities"];
        assert_eq!(capabilities["remote_push"], true, "{capabilities}");
        assert_eq!(capabilities["remote_pull"], false);
        assert_eq!(capabilities["remote_pull_checking"], true);

        let (pull, took) = read("/api/workspace/session/pull").await;
        assert!(
            took < held + Duration::from_millis(1500),
            "the pull read joins the ask the session started: {took:?}"
        );
        assert_eq!(pull["remote_pull_checking"], false);
        assert_eq!(pull["remote_pull"], false);
        assert_eq!(
            pull["remote_pull_unavailable_reason"],
            "Open repository content to read whether this branch can update from Git."
        );

        let (session, took) = read("/api/workspace/session").await;
        assert!(took < Duration::from_millis(1500), "{took:?}");
        let capabilities = &session["capabilities"];
        assert_eq!(capabilities["remote_pull_checking"], false);
        assert_eq!(
            capabilities["remote_pull_unavailable_reason"],
            pull["remote_pull_unavailable_reason"]
        );
        *super::super::workspace_git_remote::UPDATE_STATUS_PAUSE_FOR_TEST
            .lock()
            .unwrap() = None;
        server.abort();
    }

    /// Entity work answers the page's checks before any daemon is asked, and
    /// the session tells the page this server can do it.
    #[tokio::test]
    async fn entity_commit_and_restore_check_authority_and_shape_before_anything_is_asked() {
        let (origin, tokens, _hub, server) = serve_two().await;
        let client = reqwest::Client::new();
        let base = format!("{origin}/repositories/second/api/workspace");
        let operation_id = kin_model::OperationId::new();
        let restore = format!("{base}/entity/restore");
        let foreign = client
            .post(&restore)
            .header("x-kin-review-token", &tokens[0])
            .header("origin", &origin)
            .json(&serde_json::json!({"entity":"e","operation_id":operation_id}))
            .send()
            .await
            .unwrap();
        assert_eq!(foreign.status(), StatusCode::FORBIDDEN);
        for (body, reason) in [
            (
                serde_json::json!({"entity":"  ","operation_id":operation_id}),
                "names the entity",
            ),
            (
                serde_json::json!({"entity":"e","operation_id":uuid::Uuid::nil()}),
                "operation ID is required",
            ),
        ] {
            let refused = client
                .post(&restore)
                .header("x-kin-review-token", &tokens[1])
                .header("origin", &origin)
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(refused.status(), StatusCode::CONFLICT);
            let answer: serde_json::Value = refused.json().await.unwrap();
            assert_eq!(answer["outcome"], "refused");
            assert!(
                answer["error"].as_str().unwrap().contains(reason),
                "{answer}"
            );
            assert!(answer["remedy"]
                .as_str()
                .unwrap()
                .contains("Nothing was changed"));
        }
        // A restore names an entity, never where its content is projected.
        let projected = client
            .post(&restore)
            .header("x-kin-review-token", &tokens[1])
            .header("origin", &origin)
            .json(&serde_json::json!({
                "entity":"e","operation_id":operation_id,"path":"src/lib.rs",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(projected.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let refused = client
            .post(format!("{base}/commit"))
            .header("x-kin-review-token", &tokens[1])
            .header("origin", &origin)
            .json(&serde_json::json!({
                "message":"keep one","operation_id":operation_id,"entities":["a",""],
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::CONFLICT);
        let answer: serde_json::Value = refused.json().await.unwrap();
        assert!(
            answer["error"].as_str().unwrap().contains("needs its id"),
            "{answer}"
        );

        let session: serde_json::Value = client
            .get(format!("{base}/session"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(session["capabilities"]["entity_commit"], true);
        assert_eq!(session["capabilities"]["entity_restore"], true);
        server.abort();
    }

    /// A flag from the page is refused in words, with nothing asked of the
    /// repository, when it lacks the page's authority or names no entity, and
    /// a caller never names who set it.
    #[tokio::test]
    async fn a_flag_needs_authority_and_an_entity_before_anything_is_asked() {
        let (origin, tokens, _hub, server) = serve_two().await;
        let client = reqwest::Client::new();
        let session: serde_json::Value = client
            .get(format!(
                "{origin}/repositories/second/api/workspace/session"
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(session["capabilities"]["review_flags"], true);

        let operation_id = kin_model::OperationId::new();
        let entity = uuid::Uuid::new_v4();
        for (path, body) in [
            (
                "flags",
                serde_json::json!({
                    "entity":entity,"note":"look again","fingerprint":"ab".repeat(32),
                    "operation_id":operation_id,
                }),
            ),
            (
                "flags/resolve",
                serde_json::json!({"entity":entity,"operation_id":operation_id}),
            ),
        ] {
            let url = format!("{origin}/repositories/second/api/workspace/{path}");
            for (token, request_origin) in [
                (tokens[0].as_str(), origin.as_str()),
                (tokens[1].as_str(), "https://foreign.example"),
                ("", origin.as_str()),
            ] {
                let refused = client
                    .post(&url)
                    .header("x-kin-review-token", token)
                    .header("origin", request_origin)
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{path}");
            }
            // An entity is named by its id, never by a name or a place.
            for unnamed in ["ServerReply.send", "src/reply.ts", ""] {
                let mut named = body.clone();
                named["entity"] = serde_json::json!(unnamed);
                let refused = client
                    .post(&url)
                    .header("x-kin-review-token", &tokens[1])
                    .header("origin", &origin)
                    .json(&named)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(refused.status(), StatusCode::CONFLICT, "{path}");
                let said: serde_json::Value = refused.json().await.unwrap();
                assert_eq!(said["outcome"], "refused");
                assert_eq!(said["operation_id"], serde_json::json!(operation_id));
                assert!(
                    said["error"]
                        .as_str()
                        .unwrap()
                        .contains("on an entity, named by its id"),
                    "{said}"
                );
                assert!(said["remedy"]
                    .as_str()
                    .unwrap()
                    .contains("Nothing was recorded"));
            }
            let mut signed = body.clone();
            signed["author"] = serde_json::json!("caller");
            let injected = client
                .post(&url)
                .header("x-kin-review-token", &tokens[1])
                .header("origin", &origin)
                .json(&signed)
                .send()
                .await
                .unwrap();
            assert_eq!(
                injected.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{path}"
            );
        }
        server.abort();
    }

    /// The server's boundaries can be tested without starting a repository
    /// daemon or admitting any content. Tokens belong to URL scopes.
    #[tokio::test]
    async fn scoped_pages_keep_assets_api_and_write_authority_in_the_selected_repository() {
        let (origin, tokens, hub, server) = serve_two().await;
        let port = hub.port;
        let client = reqwest::Client::new();
        for key in ["first", "second"] {
            let base = format!("/repositories/{key}");
            let workspace = client
                .get(format!("{origin}{base}/workspace/"))
                .send()
                .await
                .unwrap();
            assert_eq!(workspace.status(), StatusCode::OK);
            assert_eq!(workspace.headers()["cache-control"], "no-store");
            assert!(workspace.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'"));
            let workspace = workspace.text().await.unwrap();
            assert!(workspace.contains(&format!("data-api-base=\"{base}\"")));
            assert!(workspace.contains(&format!("{base}/workspace/workspace.js")));
            assert!(!workspace.contains("__KIN_BASE__"));
            let review = client
                .get(format!("{origin}{base}/review/"))
                .send()
                .await
                .unwrap();
            assert_eq!(review.status(), StatusCode::OK);
            let review = review.text().await.unwrap();
            assert!(review.contains(&format!("src=\"{base}/review/app.js\"")));
            assert!(review.contains(&format!("data-workspace-url=\"{base}/workspace/\"")));
            let session: serde_json::Value = client
                .get(format!("{origin}{base}/api/session"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(session["repository_key"].as_str().unwrap().len(), 64);
            let refused = client
                .get(format!("{origin}{base}/workspace/"))
                .header("host", format!("foreign.example:{port}"))
                .send()
                .await
                .unwrap();
            assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        }
        for (token, request_origin) in [
            (tokens[0].as_str(), origin.as_str()),
            (tokens[1].as_str(), "https://foreign.example"),
            ("", origin.as_str()),
        ] {
            let refused = client
                .post(format!(
                    "{origin}/repositories/second/api/workspace/merge/preview"
                ))
                .header("x-kin-review-token", token)
                .header("origin", request_origin)
                .json(&serde_json::json!({"source":"main"}))
                .send()
                .await
                .unwrap();
            assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        }
        // Valid local authority still refuses a non-branch before daemon lookup.
        let invalid = client
            .post(format!(
                "{origin}/repositories/second/api/workspace/merge/preview"
            ))
            .header("x-kin-review-token", &tokens[1])
            .header("origin", &origin)
            .json(&serde_json::json!({"source":"refs/tags/release"}))
            .send()
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

        // These layouts have no repository authority or daemon. Confirmation
        // preconditions must produce a definite refusal before resolving either.
        let hash = kin_model::Hash256::from_bytes([1; 32]);
        let root = kin_model::AuthorityRoot::new(1, hash);
        let expected = super::super::merge::MergeExpectation {
            repository_id: kin_model::RepositoryId::new("synthetic-preflight").unwrap(),
            workspace_id: kin_model::WorkspaceId::new(),
            roots: kin_model::RootBundle {
                version: 1,
                generation: 0,
                history: root,
                ref_state: root,
                ref_log: root,
                collaboration: root,
                replication: root,
                local_state: root,
            },
            target_ref: kin_model::RefName::branch(b"main").unwrap(),
            source_ref: kin_model::RefName::branch(b"feature").unwrap(),
            ours_change: kin_model::SemanticChangeId::from_hash(hash),
            theirs_change: kin_model::SemanticChangeId::from_hash(hash),
            workspace_tree_hash: hash,
        };
        for (source, reason) in [
            ("refs/tags/release", "choose a source branch"),
            ("different", "source differs from the displayed preview"),
        ] {
            let operation_id = kin_model::OperationId::new();
            let refused = client
                .post(format!(
                    "{origin}/repositories/second/api/workspace/merge/confirm"
                ))
                .header("x-kin-review-token", &tokens[1])
                .header("origin", &origin)
                .json(&serde_json::json!({
                    "source":source, "operation_id":operation_id, "expected":expected,
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(refused.status(), StatusCode::CONFLICT);
            let body: serde_json::Value = refused.json().await.unwrap();
            assert_eq!(body["outcome"], "refused");
            assert_eq!(body["operation_id"], serde_json::json!(operation_id));
            assert!(body["error"].as_str().unwrap().contains(reason), "{body}");
            assert!(body["remedy"].as_str().unwrap().contains("not dispatched"));
        }
        let branch_expected = super::super::branch::BranchSwitchExpectation {
            repository_id: expected.repository_id.clone(),
            workspace_id: expected.workspace_id,
            roots: expected.roots.clone(),
            workspace_generation: 0,
            workspace_head: kin_model::WorkspaceHead::Symbolic {
                target: expected.target_ref.clone(),
            },
            workspace_base: Some(kin_model::RefTarget::Change {
                change_id: expected.ours_change,
            }),
            workspace_tree: hash,
            source_ref_head: Some(kin_model::RefTarget::Change {
                change_id: expected.ours_change,
            }),
            target_ref: kin_model::RefName::from_bytes(b"refs/tags/release".to_vec()).unwrap(),
            target_head: kin_model::RefTarget::Change {
                change_id: expected.theirs_change,
            },
            target_change: expected.theirs_change,
        };
        let operation_id = kin_model::OperationId::new();
        let branch_url = format!("{origin}/repositories/second/api/workspace/branch/confirm");
        let refused = client
            .post(&branch_url)
            .header("x-kin-review-token", &tokens[1])
            .header("origin", &origin)
            .json(&serde_json::json!({"operation_id":operation_id,"expected":branch_expected}))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::CONFLICT);
        let body: serde_json::Value = refused.json().await.unwrap();
        assert_eq!(body["outcome"], "refused");
        assert_eq!(body["operation_id"], serde_json::json!(operation_id));
        assert!(body["error"].as_str().unwrap().contains("choose a target"));
        assert!(body["remedy"].as_str().unwrap().contains("not dispatched"));
        let refused = client
            .post(&branch_url)
            .header("x-kin-review-token", &tokens[0])
            .header("origin", &origin)
            .json(&serde_json::json!({"operation_id":operation_id,"expected":branch_expected}))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        let injected_actor = client.post(&branch_url)
            .header("x-kin-review-token", &tokens[1]).header("origin", &origin)
            .json(&serde_json::json!({"operation_id":operation_id,"expected":branch_expected,"actor":"caller"}))
            .send().await.unwrap();
        assert_eq!(injected_actor.status(), StatusCode::UNPROCESSABLE_ENTITY);
        server.abort();
    }
}
