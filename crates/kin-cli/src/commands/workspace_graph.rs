// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Bounded, current-workspace graph reads for the local UI. This module never
//! accepts a daemon address, tool name, source path, or executable from the page.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::review_ui::UiState;
use crate::daemon_client::{urlencoding, DaemonClient, HealthResponse};

const PAGE_SIZE: usize = 50;
const GRAPH_LIMIT: usize = 180;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GraphQuery {
    #[serde(default)]
    query: String,
    #[serde(default)]
    offset: usize,
}

fn failure(status: StatusCode, error: anyhow::Error) -> Response {
    (status, Json(json!({ "error": format!("{error:#}") }))).into_response()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn validate_query(query: &GraphQuery) -> Result<()> {
    anyhow::ensure!(query.query.len() <= 256, "Search is limited to 256 bytes.");
    anyhow::ensure!(
        !query.query.chars().any(char::is_control),
        "Search contains a control character."
    );
    anyhow::ensure!(
        query.offset <= 1_000_000,
        "Entity page offset is too large."
    );
    Ok(())
}

pub(super) async fn serve_graph(
    State(state): State<Arc<UiState>>,
    Query(query): Query<GraphQuery>,
) -> Response {
    if let Err(error) = validate_query(&query) {
        return failure(StatusCode::BAD_REQUEST, error);
    }
    let answer = async {
        let client = DaemonClient::connect_for_workspace("ui graph", &state.layout).await?;
        let health = client.health().await?;
        let repo_id = health.repo_id.as_deref().context("The daemon did not identify its repository.")?;
        let export_query = format!("limit={GRAPH_LIMIT}&include=signature");
        let (graph, mut inventory) = tokio::try_join!(
            client.graph_export(&export_query),
            client.workspace_entities(repo_id, &query.query, PAGE_SIZE, query.offset),
        )?;
        let mut graph = serde_json::to_value(graph)?;
        without_locations(&mut graph["nodes"]);
        without_locations(&mut inventory["entities"]);
        anyhow::Ok(json!({
            "schema": "kin.workspace.graph.v1", "scope": "workspace",
            "repository_id": repo_id, "workspace_id": health.workspace_id,
            "graph": graph, "inventory": inventory,
            "query": query.query, "offset": query.offset, "page_size": PAGE_SIZE,
            "reconciliation_status": health.reconciliation_status,
            "read_at": now(),
            "coverage_notice": "The network is a sample of the repository's entities, and search matches them by name. Kin's connections can be incomplete while it is still reading the code."
        }))
    }.await;
    match answer {
        Ok(value) => Json(value).into_response(),
        Err(error) => failure(StatusCode::BAD_GATEWAY, error),
    }
}

/// The keys an export or inventory row uses for where an entity's text sits
/// in the checkout. The page names entities, never places, so they are
/// removed before an answer leaves this server.
const LOCATION_KEYS: [&str; 5] = ["file", "file_path", "path", "source_path", "location"];

fn without_locations(rows: &mut Value) {
    for row in rows.as_array_mut().into_iter().flatten() {
        if let Some(row) = row.as_object_mut() {
            for key in LOCATION_KEYS {
                row.remove(key);
            }
            // A module's recorded signature is only the place its text is
            // held, so it is dropped the way the explore routes drop it.
            let kind = row.get("kind").and_then(Value::as_str).unwrap_or_default();
            let shown = super::workspace_explore::shown_signature(
                kind,
                row.get("signature").and_then(Value::as_str),
            );
            match shown {
                Some(signature) => {
                    row.insert("signature".into(), Value::String(signature));
                }
                None => {
                    row.remove("signature");
                }
            }
        }
    }
}

/// A source base proves which current workspace answered. It is an expectation,
/// not a bearer credential, and the plugin independently checks its own read.
pub(super) fn editor_uri(
    source: &Value,
    id: &str,
    health: &HealthResponse,
    root: &str,
) -> Result<String> {
    let repository = health
        .repo_id
        .as_deref()
        .context("Missing repository identity.")?;
    let workspace = health
        .workspace_id
        .as_deref()
        .context("Missing workspace identity.")?;
    validate_editor_source(source, id, repository, workspace)?;
    make_editor_uri(repository, workspace, root, id)
}

fn validate_editor_source(
    source: &Value,
    id: &str,
    repository: &str,
    workspace: &str,
) -> Result<()> {
    let base = source.get("source_base").context(
        "VS Code handoff needs a verified current source read. This entity did not provide one.",
    )?;
    if base.get(kin_mcp::source_ticket::TICKET_KEY).is_some() {
        anyhow::ensure!(
            kin_mcp::source_ticket::is_ticket(base),
            "The source ticket is malformed."
        );
        let ticket = kin_mcp::source_ticket::parse(base["ticket"].as_str().unwrap_or_default())
            .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            source["id"].as_str() == Some(id) && ticket.entity_id == id,
            "Source identity differs from the selected entity."
        );
        // Public MCP replies keep the ticket opaque. The repository-bound
        // daemon supplies the body; this link grants no write authority. The
        // editor checks its own connection and rereads before opening, and a
        // future write resolves the original ticket at the daemon.
        let identity = &source["source_identity"];
        anyhow::ensure!(
            identity["entity_id"].as_str() == Some(id)
                && identity["repository_id"].as_str() == Some(repository)
                && identity["workspace_id"].as_str() == Some(workspace),
            "The source response belongs to another repository or workspace."
        );
        anyhow::ensure!(
            source["body"].is_string()
                && source["body_excerpt"].is_null()
                && source["truncated"].as_bool() != Some(true)
                && source["span_coherence"].as_str() == Some("digest_verified")
                && (source["source_state"].is_null()
                    || matches!(
                        source["source_state"].as_str(),
                        Some("workspace" | "committed")
                    )),
            "This entity has no verified current source body to open."
        );
        return Ok(());
    }
    let base: kin_mcp::source_base::EntitySourceBase =
        serde_json::from_value(base.clone()).context("The source base could not be verified.")?;
    base.validate().map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        source["id"].as_str() == Some(id) && base.entity_id.to_string() == id,
        "Source identity differs from the selected entity."
    );
    anyhow::ensure!(
        base.context.repository_id == repository && base.context.workspace_id == workspace,
        "The source response belongs to another workspace."
    );
    let body = source["body"]
        .as_str()
        .context("This entity has no source body to open.")?;
    anyhow::ensure!(
        body.len() == base.end_byte - base.start_byte
            && kin_blobs::digest(body.as_bytes()).to_string() == base.body_hash,
        "The source body differs from its verified source base."
    );
    Ok(())
}

fn make_editor_uri(repository: &str, workspace: &str, root: &str, entity: &str) -> Result<String> {
    anyhow::ensure!(
        !repository.is_empty()
            && repository.len() <= 256
            && !repository.chars().any(char::is_control),
        "Invalid repository identity."
    );
    let workspace = uuid::Uuid::parse_str(workspace)
        .context("Invalid workspace identity.")?
        .to_string();
    let entity = uuid::Uuid::parse_str(entity)
        .context("Invalid entity identity.")?
        .to_string();
    let root = std::path::Path::new(root)
        .canonicalize()
        .context("The workspace location is unavailable.")?;
    let root = root
        .to_str()
        .context("The workspace location is not Unicode.")?;
    anyhow::ensure!(
        root.len() <= 4096 && !root.chars().any(char::is_control),
        "Invalid workspace location."
    );
    let query = format!(
        "v=1&repository={}&workspace={}&root={}&entity={}&scope=workspace",
        urlencoding::encode(repository),
        workspace,
        urlencoding::encode(root),
        entity,
    );
    // VS Code decodes the complete query before invoking the URI handler. Keep
    // the value escapes intact for the handler's strict, single-value decoder.
    let uri = format!(
        "vscode://firelock.kin-editor/open-entity?{}",
        urlencoding::encode(&query),
    );
    anyhow::ensure!(uri.len() <= 16384, "The editor intent is too long.");
    Ok(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_rows_leave_without_their_locations() {
        let mut nodes = json!([
            {"id": "a", "name": "want_bytes", "kind": "Function", "file": "src/pkg/encoding.py", "degree": 3},
            {"id": "b", "name": "Signer", "kind": "Class", "file_path": "src/pkg/signer.py", "path": "x", "source_path": "y", "location": "z"},
            {"id": "c", "name": "encoding", "kind": "Module", "signature": "module src/pkg/encoding.py"},
            {"id": "d", "name": "sign", "kind": "Method", "signature": "def sign(self)"},
        ]);
        without_locations(&mut nodes);
        let text = nodes.to_string();
        for key in LOCATION_KEYS {
            assert!(
                !text.contains(&format!("\"{key}\"")),
                "{key} left in {text}"
            );
        }
        assert!(
            !text.contains(".py"),
            "a module signature left a location in {text}"
        );
        assert_eq!(nodes[3]["signature"], "def sign(self)");
        assert_eq!(nodes[0]["name"], "want_bytes");
        assert_eq!(nodes[0]["degree"], 3);
        let mut not_rows = json!({"file": "kept, not a row list"});
        without_locations(&mut not_rows);
        assert_eq!(not_rows["file"], "kept, not a row list");
    }

    #[test]
    fn search_bounds_do_not_admit_control_characters_or_unbounded_pages() {
        assert!(validate_query(&GraphQuery {
            query: "handler".into(),
            offset: 50
        })
        .is_ok());
        assert!(validate_query(&GraphQuery {
            query: "x\ncommand".into(),
            offset: 0
        })
        .is_err());
        assert!(validate_query(&GraphQuery {
            query: "x".repeat(257),
            offset: 0
        })
        .is_err());
        assert!(validate_query(&GraphQuery {
            query: String::new(),
            offset: 1_000_001
        })
        .is_err());
    }

    #[test]
    fn editor_intent_encodes_identity_without_accepting_a_command_or_historical_scope() {
        let uuid = "11111111-1111-4111-8111-111111111111";
        let root = std::env::temp_dir();
        let uri = make_editor_uri(
            "team/repo?scope=change&command=run",
            uuid,
            root.to_str().unwrap(),
            uuid,
        )
        .unwrap();
        let uri = url::Url::parse(&uri).unwrap();
        let transport: Vec<_> = uri.query_pairs().collect();
        assert_eq!(transport.len(), 1);
        assert_eq!(transport[0].1, "");
        let query = &transport[0].0;
        assert!(query
            .starts_with("v=1&repository=team%2Frepo%3Fscope%3Dchange%26command%3Drun&workspace="));
        assert_eq!(query.matches("&scope=").count(), 1);
        assert!(query.ends_with("&scope=workspace"));
        assert!(make_editor_uri("repo", "not-a-workspace", root.to_str().unwrap(), uuid).is_err());
        assert!(make_editor_uri("repo\n", uuid, root.to_str().unwrap(), uuid).is_err());
    }

    #[test]
    fn editor_intent_preserves_exact_identity_after_platform_and_value_decoding() {
        let uuid = "11111111-1111-4111-8111-111111111111";
        let directory = tempfile::tempdir().unwrap();
        for name in [
            "R&D",
            "100% coverage",
            "repo%20name",
            "hash#name",
            "equals=name",
            "question?name",
            "plus+name",
            "雪 repo",
            "punctuation!'()*",
        ] {
            let root = directory.path().join(name);
            std::fs::create_dir(&root).unwrap();
            let canonical = root.canonicalize().unwrap();
            let repository = format!("org/{name}");
            let raw = make_editor_uri(&repository, uuid, root.to_str().unwrap(), uuid).unwrap();
            let uri = url::Url::parse(&raw).unwrap();
            assert!(!uri.query().unwrap().contains(['&', '=']));
            // The platform consumes the outer layer before the handler decodes
            // individual values. Form decoding is equivalent for these queries,
            // since the producer escapes every literal plus sign at both layers.
            let transport: Vec<_> = uri.query_pairs().collect();
            assert_eq!(transport.len(), 1);
            assert_eq!(transport[0].1, "");
            let fields: std::collections::HashMap<_, _> =
                url::form_urlencoded::parse(transport[0].0.as_bytes()).collect();
            assert_eq!(fields.len(), 6);
            assert_eq!(fields["repository"], repository);
            assert_eq!(fields["root"], canonical.to_str().unwrap());
            assert_eq!(fields["workspace"], uuid);
            assert_eq!(fields["entity"], uuid);
            assert_eq!(fields["scope"], "workspace");
        }
    }

    #[test]
    fn editor_handoff_requires_the_same_entity_workspace_and_exact_body() {
        let id = "11111111-1111-4111-8111-111111111111";
        let workspace = "22222222-2222-4222-8222-222222222222";
        let body = "fn current() {}";
        let source = json!({"id": id, "body": body, "source_base": {
            "schema": "kin.entity.source_base.v1",
            "context": {"repository_id": "repo", "workspace_id": workspace,
                "workspace_generation": 1, "workspace_head_hash": "a".repeat(64),
                "workspace_tree_hash": "b".repeat(64)},
            "entity_id": id, "artifact_id": workspace, "source_blob_hash": "c".repeat(64),
            "start_byte": 0, "end_byte": body.len(), "body_hash": kin_blobs::digest(body.as_bytes()).to_string()
        }});
        assert!(validate_editor_source(&source, id, "repo", workspace).is_ok());
        assert!(validate_editor_source(&source, id, "other", workspace).is_err());
        assert!(validate_editor_source(&source, id, "repo", id).is_err());
        assert!(validate_editor_source(&source, workspace, "repo", workspace).is_err());
        let mut changed = source.clone();
        changed["body"] = json!("fn changed() {}");
        assert!(validate_editor_source(&changed, id, "repo", workspace).is_err());
        let mut historical = source;
        historical.as_object_mut().unwrap().remove("source_base");
        assert!(validate_editor_source(&historical, id, "repo", workspace).is_err());
    }

    #[test]
    fn compact_editor_ticket_keeps_entity_and_repository_boundaries() {
        let id = "11111111-1111-4111-8111-111111111111";
        let workspace = "22222222-2222-4222-8222-222222222222";

        let mut source = json!({
            "id": id, "body": "fn current() {}", "source_state": "workspace",
            "span_coherence": "digest_verified",
            "source_base": {"ticket": format!("sb2.{id}.1.{}", "a".repeat(32))},
            "source_identity": {"repository_id": "repo", "workspace_id": workspace, "entity_id": id},
        });
        assert!(validate_editor_source(&source, id, "repo", workspace).is_ok());
        assert!(validate_editor_source(&source, id, "other", workspace).is_err());
        assert!(validate_editor_source(&source, id, "repo", id).is_err());
        assert!(validate_editor_source(&source, workspace, "repo", workspace).is_err());
        source.as_object_mut().unwrap().remove("source_state");
        assert!(validate_editor_source(&source, id, "repo", workspace).is_ok());
        source["source_state"] = json!("historical");
        assert!(validate_editor_source(&source, id, "repo", workspace).is_err());
        source.as_object_mut().unwrap().remove("source_state");
        source["body_excerpt"] = json!({"complete": false});
        assert!(validate_editor_source(&source, id, "repo", workspace).is_err());
        source.as_object_mut().unwrap().remove("body_excerpt");
        source["span_coherence"] = json!("unverified");
        assert!(validate_editor_source(&source, id, "repo", workspace).is_err());
        source["span_coherence"] = json!("digest_verified");
        source["source_base"]["extra"] = json!(true);
        assert!(validate_editor_source(&source, id, "repo", workspace).is_err());
        source["source_base"] = json!({"ticket": "unreadable"});
        assert!(validate_editor_source(&source, id, "repo", workspace).is_err());
    }
}
