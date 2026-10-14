// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Review flags, as the workspace page sets and reads them.
//!
//! A reader walking uncommitted work flags an entity with a note. These routes
//! record that in the repository, through the daemon that holds it, so the
//! agent that made the change is told the next time an answer from Kin
//! returns the entity, and so the page can show which agents have been told.
//! See [`super::review_flags`].
//!
//! A flag is signed as the page's own reader, never as a name the request
//! carries. It is advisory: no route here stops a commit.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::review_flags::{
    ReviewFlagOutcome, ReviewFlagRequest, ReviewFlagResponse, ReviewFlagView,
};
use super::review_ui::{self, UiState};

/// The schema of the answer that lists a repository's open flags.
const FLAGS_SCHEMA: &str = "kin.workspace.flags.v1";

pub(super) fn routes() -> Router<Arc<UiState>> {
    Router::new()
        .route("/api/workspace/flags", get(serve_flags).post(set_flag))
        .route("/api/workspace/flags/resolve", post(resolve_flag))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FlagBody {
    entity: String,
    #[serde(default)]
    note: Option<String>,
    fingerprint: String,
    operation_id: kin_model::OperationId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveBody {
    entity: String,
    operation_id: kin_model::OperationId,
}

/// One flag as the page reads it: the daemon's view, with each agent that saw
/// it under the key the page knows that agent's lane by.
fn flag_json(flag: &ReviewFlagView) -> Value {
    let mut shown = serde_json::to_value(flag).unwrap_or_default();
    if let Some(seen_by) = shown.get_mut("seen_by").and_then(Value::as_array_mut) {
        for seen in seen_by {
            let key = seen
                .get("session")
                .and_then(Value::as_str)
                .map(super::workspace_agents::agent_key);
            if let Some(key) = key {
                seen["key"] = json!(key);
            }
        }
    }
    shown
}

/// Nothing was recorded: the request stopped before the daemon was asked, or
/// the daemon said why it would not record it.
fn refused(operation_id: kin_model::OperationId, reason: &str) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": reason, "outcome": "refused", "operation_id": operation_id,
            "remedy": "Nothing was recorded. Correct what is reported, then send it again.",
        })),
    )
        .into_response()
}

/// The daemon's answer was lost, so this request cannot say how it ended. The
/// same request sent again records at most one flag.
fn unknown(operation_id: kin_model::OperationId, error: &anyhow::Error) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({
            "error": format!("{error:#}"), "outcome": "unknown", "operation_id": operation_id,
            "remedy": "This may have been recorded. Read the flags again, or send the same \
                       request again: one operation is recorded once.",
        })),
    )
        .into_response()
}

fn entity_id(text: &str) -> Option<kin_model::EntityId> {
    uuid::Uuid::parse_str(text.trim())
        .ok()
        .map(kin_model::EntityId)
}

/// Send one write to the daemon and answer the page with how it ended.
async fn write(
    state: &UiState,
    operation_id: kin_model::OperationId,
    request: ReviewFlagRequest,
) -> Response {
    let answered = async { review_ui::daemon(state).await?.review_flags(&request).await }.await;
    let response: ReviewFlagResponse = match answered {
        Ok(response) => response,
        Err(error) => return unknown(operation_id, &error),
    };
    let outcome = match response.outcome {
        ReviewFlagOutcome::Refused => {
            return refused(
                operation_id,
                response
                    .error
                    .as_deref()
                    .unwrap_or("The repository did not record this."),
            )
        }
        ReviewFlagOutcome::Flagged => "flagged",
        ReviewFlagOutcome::Resolved => "resolved",
        ReviewFlagOutcome::NothingOpen => "nothing_open",
        ReviewFlagOutcome::Listed => "listed",
    };
    let mut answer = json!({ "operation_id": operation_id, "outcome": outcome });
    if let Some(flag) = &response.flag {
        answer["flag"] = flag_json(flag);
    }
    Json(answer).into_response()
}

/// Flag an entity at the version the reader saw, with an optional note.
async fn set_flag(
    State(state): State<Arc<UiState>>,
    headers: HeaderMap,
    Json(body): Json<FlagBody>,
) -> Response {
    if !review_ui::may_write(&headers, &state) {
        return review_ui::write_refused();
    }
    let operation_id = body.operation_id;
    let Some(entity) = entity_id(&body.entity) else {
        return refused(operation_id, "A flag is set on an entity, named by its id.");
    };
    write(
        &state,
        operation_id,
        ReviewFlagRequest::Set {
            entity,
            note: body.note,
            fingerprint: body.fingerprint,
            operation_id,
            // Signed as this page's own reader, resolved for this URL's
            // repository, never as a caller-supplied name.
            author: state.actor.clone(),
        },
    )
    .await
}

/// Close the open flag on an entity.
async fn resolve_flag(
    State(state): State<Arc<UiState>>,
    headers: HeaderMap,
    Json(body): Json<ResolveBody>,
) -> Response {
    if !review_ui::may_write(&headers, &state) {
        return review_ui::write_refused();
    }
    let operation_id = body.operation_id;
    let Some(entity) = entity_id(&body.entity) else {
        return refused(
            operation_id,
            "A flag is closed on an entity, named by its id.",
        );
    };
    write(
        &state,
        operation_id,
        ReviewFlagRequest::Resolve {
            entity,
            operation_id,
            author: state.actor.clone(),
        },
    )
    .await
}

/// Every open flag in this repository, oldest first.
async fn serve_flags(State(state): State<Arc<UiState>>) -> Response {
    let answered = async {
        review_ui::daemon(&state)
            .await?
            .review_flags(&ReviewFlagRequest::List)
            .await
    }
    .await;
    match answered {
        Ok(response) => Json(json!({
            "schema": FLAGS_SCHEMA,
            "flags": response.flags.iter().map(flag_json).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(error) => review_ui::refusal(StatusCode::BAD_GATEWAY, &error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::Timestamp;

    #[test]
    fn a_flag_names_each_agent_that_saw_it_by_the_key_of_its_lane() {
        let session = uuid::Uuid::new_v4().to_string();
        let flag = ReviewFlagView {
            flag_id: uuid::Uuid::new_v4().to_string(),
            entity_id: uuid::Uuid::new_v4().to_string(),
            name: "ServerReply.send".into(),
            kind: Some("method".into()),
            note: "a second send must fail loudly".into(),
            author: "Ada Reviewer".into(),
            set_at: Timestamp::now(),
            fingerprint: "ab".repeat(32),
            changed_since: false,
            removed: false,
            source_unavailable: false,
            seen_by: vec![super::super::review_flags::ReviewFlagSeen {
                agent: "Codex".into(),
                session: session.clone(),
                at: Timestamp::now(),
            }],
        };
        let shown = flag_json(&flag);
        assert_eq!(shown["name"], "ServerReply.send");
        assert_eq!(shown["changed_since"], false);
        assert!(shown.get("removed").is_none());
        let seen = &shown["seen_by"][0];
        assert_eq!(seen["agent"], "Codex");
        assert_eq!(seen["session"], json!(session));
        assert_eq!(
            seen["key"],
            json!(super::super::workspace_agents::agent_key(&session))
        );
        assert!(seen["at"].is_string());
    }

    #[test]
    fn an_entity_is_named_by_its_id_and_nothing_else() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(entity_id(&format!(" {id} ")), Some(kin_model::EntityId(id)));
        assert_eq!(entity_id("ServerReply.send"), None);
        assert_eq!(entity_id("src/reply.ts"), None);
    }
}
