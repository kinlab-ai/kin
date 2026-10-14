// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Entity-at-a-time work on the uncommitted working copy: commit only chosen
//! entities, or put one entity back as it was.
//!
//! These are the requests the daemon answers on `/commands/commit/entities`
//! and `/commands/entity/restore`, shared here so the CLI, the local app and
//! the daemon read one contract. Both are writes. Each carries the caller's
//! operation id, which the daemon records as the repository operation, so an
//! answer that never arrives is settled by asking repository authority whether
//! that operation landed, and never by sending it again.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The error kind every refusal of entity work carries. Its `message` is one
/// sentence about entities, written for the person who asked.
pub const ENTITY_REFUSED: &str = "entity_refused";

/// Commit only the named entities' uncommitted changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityCommitRequest {
    pub operation_id: kin_model::OperationId,
    pub timestamp: kin_model::Timestamp,
    pub message: String,
    pub author: kin_model::AuthorId,
    /// Entity ids as a review guide gives them, and the ids of unparsed
    /// entities.
    pub entities: Vec<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// The change an entity commit recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityCommitResponse {
    pub change_id: String,
    /// The branch the change was published onto, or `null` on a detached head.
    #[serde(default)]
    pub branch: Option<String>,
    pub entity_count: usize,
    pub relation_count: usize,
    pub file_count: usize,
    /// The entities whose changes the change carries, as they were named.
    pub kept: Vec<String>,
    /// Whether this operation had already landed, so this answer restates it.
    #[serde(default)]
    pub replayed: bool,
}

/// Put one entity's source back as it was at the head, or at an earlier
/// change, as an uncommitted edit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityRestoreRequest {
    pub operation_id: kin_model::OperationId,
    pub entity: String,
    /// The change to restore from. The workspace's head when absent.
    #[serde(default)]
    pub to: Option<String>,
    /// The entity as the caller last read it in the working copy:
    /// `source:<body_hash>` for its exact span as the guide reports it,
    /// `absent` when the guide reports it removed, or an unparsed entity's
    /// content address. A restore discards the entity's whole source, so only
    /// an exact reading guards it. A semantic fingerprint, such as `behavior`
    /// or `ast`, stays the same when a comment or spacing changes, so a
    /// restore that names one is refused even while it matches.
    /// Absent means the caller asks for no check.
    #[serde(default)]
    pub expected: Option<String>,
    pub actor: kin_model::AuthorId,
}

/// What a restore did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityRestoreResponse {
    pub entity: String,
    /// The entity's name, when it has one in either version.
    #[serde(default)]
    pub name: Option<String>,
    /// The change whose version of the entity the working copy now holds.
    /// Absent when authority's receipt proves the restore landed without
    /// retaining the requested target, including an explicit-target replay.
    #[serde(default)]
    pub restored_to: Option<String>,
    /// Whether this operation had already landed, so this answer restates it.
    #[serde(default)]
    pub replayed: bool,
}

/// The sentence a refusal of entity work carries, when the body is one.
pub(crate) fn refusal_message(body: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    if parsed.get("error")?.as_str()? != ENTITY_REFUSED {
        return None;
    }
    Some(parsed.get("message")?.as_str()?.to_string())
}

/// How a restore the local app asked for ended.
pub(super) enum AppRestore {
    Restored(EntityRestoreResponse),
    /// Nothing was written. The error is Kin's own sentence.
    Refused(anyhow::Error),
    /// The request was sent and its outcome is not known.
    Unknown(anyhow::Error),
}

/// Restore one entity for the local app.
///
/// The actor is resolved against `layout`, never taken from the caller, and
/// the caller's operation id is the one the daemon records. The app is no
/// agent session, so the process's ambient session is not sent.
pub(super) async fn restore_for_app(
    layout: &kin_core::KinLayout,
    entity: String,
    to: Option<String>,
    expected: Option<String>,
    operation_id: kin_model::OperationId,
) -> AppRestore {
    let actor = match super::require_commit_author_for(layout) {
        Ok(actor) => actor,
        Err(error) => return AppRestore::Refused(error),
    };
    let request = EntityRestoreRequest {
        operation_id,
        entity,
        to,
        expected,
        actor,
    };
    send_restore(layout, &request, None).await
}

/// `kin checkout --entity`: restore one entity and say what was done.
pub async fn run_restore(
    entity: String,
    to: Option<String>,
    expected: Option<String>,
) -> Result<()> {
    let layout = crate::commands::require_repository_layout()?;
    let actor = crate::commands::require_commit_author_for(&layout)?;
    let request = EntityRestoreRequest {
        operation_id: kin_model::OperationId::new(),
        entity,
        to,
        expected,
        actor,
    };
    // The session this command runs in is named, so the daemon can refuse a
    // restore asked for from inside one, as it refuses a checkout there.
    match send_restore(&layout, &request, super::commit::commit_session_header()).await {
        AppRestore::Restored(restored) => {
            println!("{}", render_restored(&restored));
            Ok(())
        }
        AppRestore::Refused(error) => Err(error),
        AppRestore::Unknown(error) => Err(error.context(format!(
            "whether operation {} restored the entity is not known; it was not sent again",
            request.operation_id
        ))),
    }
}

fn render_restored(restored: &EntityRestoreResponse) -> String {
    let named = restored.name.as_deref().unwrap_or(&restored.entity);
    let lead = if restored.replayed {
        "Already restored"
    } else {
        "Restored"
    };
    match &restored.restored_to {
        Some(change) => {
            format!("{lead} {named} to its version at change {change} as an uncommitted edit")
        }
        None => format!("{lead} {named} as an uncommitted edit"),
    }
}

async fn send_restore(
    layout: &kin_core::KinLayout,
    request: &EntityRestoreRequest,
    session: Option<String>,
) -> AppRestore {
    let daemon_url = match crate::daemon_client::resolve_daemon_url(layout).await {
        Ok(Some(url)) => url,
        Ok(None) => {
            return AppRestore::Refused(crate::daemon_client::daemon_required_error(
                "restore", layout,
            ))
        }
        Err(error) => return AppRestore::Refused(error),
    };
    // No reply deadline, for the reason `kin commit` has none: the daemon's
    // work is bounded by the store, not by anything this side can know.
    let client = match reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_millis(500))
        .build()
    {
        Ok(client) => client,
        Err(error) => return AppRestore::Refused(error.into()),
    };
    let mut post = client
        .post(format!(
            "{}/commands/entity/restore",
            daemon_url.trim_end_matches('/')
        ))
        .json(request);
    if let Some(token) = crate::daemon_client::resolve_daemon_auth_token_for_layout(layout) {
        post = post.bearer_auth(token);
    }
    if let Some(session) = session {
        post = post.header("X-Kin-Session", session);
    }
    // Not dispatched yet, so a refused admission is a restore that did not run.
    let post = match crate::daemon_client::admit_daemon_request(
        post,
        &daemon_url,
        layout,
        "restore",
    )
    .await
    {
        Ok(post) => post,
        Err(error) => return AppRestore::Refused(error),
    };
    let response = match post.send().await {
        Ok(response) => response,
        Err(error) => {
            return match restored_by_operation(layout, request.operation_id) {
                Ok(true) => AppRestore::Restored(EntityRestoreResponse {
                    entity: request.entity.clone(),
                    name: None,
                    restored_to: request.to.clone(),
                    replayed: true,
                }),
                Ok(false) => AppRestore::Unknown(anyhow::Error::new(error).context(
                    "the restore request was sent and no answer came back; repository authority \
                     holds no record of it yet",
                )),
                Err(read) => AppRestore::Unknown(
                    anyhow::Error::new(error)
                        .context(format!("repository authority could not be read: {read:#}")),
                ),
            };
        }
    };
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status.is_success() {
        return match serde_json::from_str::<EntityRestoreResponse>(&body) {
            Ok(restored) => AppRestore::Restored(restored),
            Err(error) => AppRestore::Unknown(
                anyhow::Error::new(error)
                    .context("the daemon's answer to a restore did not decode"),
            ),
        };
    }
    if let Some(sentence) = refusal_message(&body) {
        return AppRestore::Refused(anyhow::anyhow!(sentence));
    }
    let error = anyhow::anyhow!("the daemon answered a restore with HTTP {status}: {body}");
    // A request turned away is settled. A fault inside one is not: it does
    // not say how far the restore got.
    if status.is_client_error() {
        AppRestore::Refused(error)
    } else {
        match restored_by_operation(layout, request.operation_id) {
            Ok(true) => AppRestore::Restored(EntityRestoreResponse {
                entity: request.entity.clone(),
                name: None,
                restored_to: request.to.clone(),
                replayed: true,
            }),
            _ => AppRestore::Unknown(error),
        }
    }
}

/// Whether repository authority holds a receipt for `operation_id`.
///
/// Read from the durable store rather than asked of the daemon, because the
/// state this answers about is the one where the daemon's answer was lost.
pub(crate) fn restored_by_operation(
    layout: &kin_core::KinLayout,
    operation_id: kin_model::OperationId,
) -> Result<bool> {
    let binding = kin_core::LocalRepositoryAuthorityBinding::from_layout(layout)?;
    let authority = super::repository_authority::ActiveRepositoryAuthority::open(&binding)
        .context("open repository authority to settle a lost answer")?;
    let lease = authority.manager().read_authority();
    Ok(kin_core::rejoined_receipt(lease.metadata(), operation_id).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_entity_refusal_is_read_as_a_sentence() {
        let body = serde_json::json!({
            "error": ENTITY_REFUSED,
            "message": "render and helper were changed in the same place, so Kin cannot commit one without the other.",
        })
        .to_string();
        assert_eq!(
            refusal_message(&body).as_deref(),
            Some(
                "render and helper were changed in the same place, so Kin cannot commit one \
                 without the other."
            )
        );
        assert!(
            refusal_message(&serde_json::json!({"error":"lease_conflict"}).to_string()).is_none()
        );
        assert!(refusal_message("not json").is_none());
    }

    #[test]
    fn requests_refuse_fields_they_do_not_name() {
        let restore = serde_json::json!({
            "operation_id": kin_model::OperationId::new(),
            "entity": "8b0d3c8e-27c2-4b7b-9e0f-0d0c1f4a7a11",
            "actor": "Ada <ada@example.com>",
            "path": "src/lib.rs",
        });
        assert!(serde_json::from_value::<EntityRestoreRequest>(restore).is_err());
        let commit = serde_json::json!({
            "operation_id": kin_model::OperationId::new(),
            "timestamp": kin_model::Timestamp::now(),
            "message": "keep one",
            "author": "Ada <ada@example.com>",
            "entities": ["8b0d3c8e-27c2-4b7b-9e0f-0d0c1f4a7a11"],
        });
        let parsed: EntityCommitRequest = serde_json::from_value(commit).unwrap();
        assert_eq!(parsed.entities.len(), 1);
        assert!(parsed.session_id.is_none());
    }

    #[test]
    fn a_restore_says_what_it_restored_by_name() {
        let line = render_restored(&EntityRestoreResponse {
            entity: "8b0d3c8e-27c2-4b7b-9e0f-0d0c1f4a7a11".into(),
            name: Some("render".into()),
            restored_to: Some("5b8ca7b7".into()),
            replayed: false,
        });
        assert_eq!(
            line,
            "Restored render to its version at change 5b8ca7b7 as an uncommitted edit"
        );
    }
}
