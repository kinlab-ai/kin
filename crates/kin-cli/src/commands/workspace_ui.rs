// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A local, repository-bound human workspace. Every URL names its repository;
//! changing the selection never changes the authority of an in-flight request.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
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

#[derive(Clone, Serialize)]
pub(super) struct RepositoryEntry {
    key: String,
    name: String,
    /// Who holds the repository on its host, from its recorded Git remote.
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    url: String,
}

pub(super) struct Catalog {
    repositories: Vec<RepositoryEntry>,
    warning: Option<String>,
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

fn entry(layout: &kin_core::KinLayout) -> RepositoryEntry {
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

fn catalog(selected: kin_core::KinLayout) -> (Arc<Catalog>, Vec<kin_core::KinLayout>) {
    let mut layouts = vec![selected];
    let mut warning = None;
    match kin_core::registry::KinRegistry::load() {
        Ok(registry) => {
            let mut unavailable = 0;
            for repository in registry.repos {
                let Some(layout) = kin_core::KinLayout::discover(&repository.path) else {
                    unavailable += 1;
                    continue;
                };
                // A stale child registration must not silently become its parent.
                if layout.working_dir().canonicalize().ok() != repository.path.canonicalize().ok()
                    || !repository.path.is_dir()
                {
                    unavailable += 1;
                    continue;
                }
                let key = repository_key(layout.working_dir());
                if !layouts
                    .iter()
                    .any(|existing| repository_key(existing.working_dir()) == key)
                {
                    layouts.push(layout);
                }
            }
            if unavailable > 0 {
                warning = Some(format!(
                    "{unavailable} registered repositories are unavailable on this machine."
                ));
            }
        }
        Err(error) => warning = Some(format!("Could not read the repository registry: {error}")),
    }
    let entries = layouts.iter().map(entry).collect();
    (
        Arc::new(Catalog {
            repositories: entries,
            warning,
        }),
        layouts,
    )
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    ([(axum::http::header::CONTENT_TYPE, content_type)], body).into_response()
}

async fn index(State(state): State<Arc<UiState>>) -> Response {
    Html(INDEX.replace("__KIN_BASE__", &state.base_path)).into_response()
}

pub(super) fn routes() -> Router<Arc<UiState>> {
    Router::new()
        .route("/workspace/", get(index))
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
        .route("/api/workspace/session", get(session))
        .route("/api/workspace/branches", get(branches))
        .route("/api/workspace/branch/preview", post(branch_preview))
        .route("/api/workspace/branch/confirm", post(branch_confirm))
        .route(
            "/api/workspace/conflicts/details",
            get(super::workspace_conflicts::serve_conflicts),
        )
        .route("/api/workspace/merge/preview", post(merge_preview))
        .route("/api/workspace/merge/confirm", post(merge_confirm))
        .merge(super::workspace_explore::routes())
        .merge(super::workspace_agents::routes())
        .layer(DefaultBodyLimit::max(64 * 1024))
}

/// A catalog with no other repository in it, for tests that serve one.
#[cfg(test)]
pub(super) fn empty_catalog() -> Arc<Catalog> {
    Arc::new(Catalog {
        repositories: Vec::new(),
        warning: None,
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
    let observed = tokio::time::timeout(Duration::from_secs(5), async {
        let (pid, port) = crate::daemon_client::repo_daemon_recorded_endpoint(state.layout.root());
        match (pid, port) {
            (Some(_), Some(port)) if port != 0 => {
                let client = DaemonClient::from_running_workspace(
                    format!("http://127.0.0.1:{port}"), &state.layout,
                ).await?;
                let health = client.health().await?;
                Ok::<_, anyhow::Error>((
                    serde_json::json!({"state":"running","detail":health.status,
                        "graph_loaded":health.graph_loaded,
                        "reconciliation":health.reconciliation_status}),
                    health.repo_id, health.workspace_id,
                ))
            }
            _ => Ok((
                serde_json::json!({"state":"unknown", "detail":"No complete local daemon endpoint is recorded. Opening repository content uses Kin's normal startup path."}),
                None, None,
            )),
        }
    }).await;
    let (runtime, repository_id, workspace_id) = match observed {
        Ok(Ok(reading)) => reading,
        Ok(Err(error)) => (
            serde_json::json!({"state":"unknown", "detail":format!("{error:#}")}),
            None,
            None,
        ),
        Err(_) => (
            serde_json::json!({"state":"unknown", "detail":"Runtime observation did not finish within five seconds. Retry to check again."}),
            None,
            None,
        ),
    };
    let key = repository_key(state.layout.working_dir());
    let repositories: Vec<_> = state.catalog.as_ref().map(|catalog| catalog.repositories.iter().map(|item| {
        serde_json::json!({"key":item.key,"name":item.name,"owner":item.owner,"host":item.host,"url":item.url,"selected":item.key == key})
    }).collect()).unwrap_or_default();
    Json(serde_json::json!({
        "repository":repository_identity(&state, serde_json::json!({"key":key,"repository_id":repository_id,"workspace_id":workspace_id})),
        "repositories":repositories,
        "registry_warning":state.catalog.as_ref().and_then(|catalog| catalog.warning.as_deref()),
        "catalog_basis":"Registered repositories available when this workspace started. Restart kin ui to reload the list.",
        "capabilities":{"graph":true,"review":true,"merge":true,"editor":true,"branch_switch":true,"branch_switch_unavailable_reason":null,"conflict_details":true},
        "runtime":runtime,"version":env!("CARGO_PKG_VERSION"),"actor":state.actor,"token":state.token,
        "review_url":format!("{}/review/",state.base_path),
        "graph_url":format!("{}/workspace/#/entities",state.base_path),
    })).into_response()
}

/// Canonicalize the explicitly selected repository at the opening boundary.
/// This locates its Kin authority; no semantic answer comes from host bytes.
fn workspace_layout(path: &Path) -> Result<kin_core::KinLayout> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("cannot open {}", path.display()))?;
    super::require_repository_layout_at(&canonical)
}

/// Serve the selected and registered repositories on a single local origin.
/// No repository is initialized and no daemon is started during enumeration.
pub async fn run(path: Option<PathBuf>, port: Option<u16>, open_browser: bool) -> Result<()> {
    let selected = if let Some(path) = path {
        workspace_layout(&path)?
    } else {
        super::require_repository_layout()?
    };
    let selected_key = repository_key(selected.working_dir());
    let (catalog, layouts) = catalog(selected);
    let listener = review_ui::bind(port).await?;
    let port = listener.local_addr()?.port();
    let selected_url = format!("/repositories/{selected_key}/workspace/");
    let redirect_url = selected_url.clone();
    let mut router = Router::new().route(
        "/",
        get(move || {
            let location = redirect_url.clone();
            async move { Redirect::temporary(&location) }
        }),
    );
    let mut selected_state = None;
    for layout in layouts {
        let key = repository_key(layout.working_dir());
        let base = format!("/repositories/{key}");
        let state = Arc::new(UiState::workspace(
            layout,
            port,
            base.clone(),
            catalog.clone(),
        ));
        if key == selected_key {
            selected_state = Some(state.clone());
        }
        router = router.nest(&base, review_ui::workspace_router(state));
    }
    let selected_state =
        selected_state.context("selected repository was not registered in this workspace")?;
    router = router.layer(axum::middleware::from_fn_with_state(
        selected_state,
        review_ui::local_only,
    ));
    let url = format!("http://127.0.0.1:{port}{selected_url}");
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
    axum::serve(listener, router)
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

    /// The server's boundaries can be tested without starting a repository
    /// daemon or admitting any content. Tokens belong to URL scopes.
    #[tokio::test]
    async fn scoped_pages_keep_assets_api_and_write_authority_in_the_selected_repository() {
        let listener = review_ui::bind(Some(0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let catalog = Arc::new(Catalog {
            repositories: Vec::new(),
            warning: None,
        });
        let mut router = Router::new();
        let mut tokens = Vec::new();
        for key in ["first", "second"] {
            let base = format!("/repositories/{key}");
            let state = Arc::new(UiState::workspace(
                kin_core::KinLayout::new(std::env::temp_dir().join(format!("kin-ui-{key}/.kin"))),
                port,
                base.clone(),
                catalog.clone(),
            ));
            tokens.push(state.token.clone());
            router = router.nest(&base, review_ui::workspace_router(state));
        }
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let origin = format!("http://127.0.0.1:{port}");
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
