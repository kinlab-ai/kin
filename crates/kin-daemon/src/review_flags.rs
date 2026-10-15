// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Review flags in the daemon: where they are written, and how they reach the
//! agent that made the change.
//!
//! Three parts:
//!
//! - `POST /review/flags` plans a [`ReviewFlagRequest`] against the live graph
//!   and commits what it writes to repository authority as one transaction
//!   before the graph shows it, through the writer review state uses
//!   ([`crate::review_write`]). This is how the local app sets, closes and
//!   lists flags, and why a daemon that reopens the store still holds them.
//! - [`reached_by_call`] reads one finished agent call the way the call log
//!   does, by the entity ids its request and its answer named, and says which
//!   open flags that answer reaches. `status` reaches every open flag. What it
//!   returns travels beside the answer to the server the agent talks to.
//! - A session handed a flag is recorded on the flag. The record is held in
//!   memory at once, so the app's next read shows it, and written into the
//!   repository when the sessions it names have gone quiet or ended
//!   ([`settle_sightings`]). A write is a repository transaction that moves
//!   the authority generation, and an agent's own read must not start one in
//!   the middle of its work: a page it was handed continues only while the
//!   graph it was read from is still the one served.
//!
//! A flag is advisory. Nothing here refuses a call or a commit.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use uuid::Uuid;

use kin_cli::commands::review_flags::{
    self as flags, FlagWrite, ReviewFlagRequest, ReviewFlagResponse, ReviewFlagSeen, ReviewFlagView,
};
use kin_mcp::review_flags::{Delivered, FlagNotice, ANSWER_FLAGS_MAX, LISTING_FLAGS_MAX};
use kin_model::ids::SessionId;
use kin_model::timestamp::Timestamp;
use kin_model::{Annotation, AnnotationId, ReviewFlagSighting, WorkStore as _};

use crate::agent_activity::FlagSighting;
use crate::DaemonState;

/// The tool that answers with every open flag in the repository.
const LISTING_TOOL: &str = "kin_graph_status";

/// What a transaction that only records which agents saw a flag is.
const SIGHTING_ACTION: &str = "review_flag.seen";

/// Id-shaped strings read out of one answer before the scan stops.
const ANSWER_ID_CANDIDATES: usize = 8192;

/// How long a session must have made no call before what it saw is written
/// into the repository. A session that has ended is written at once.
#[cfg(not(test))]
const QUIET_BEFORE_WRITE: Duration = Duration::from_secs(20);
#[cfg(test)]
const QUIET_BEFORE_WRITE: Duration = Duration::from_millis(150);

/// How long a sighting that could not be written is held to try again.
const UNWRITTEN_SIGHTING_KEPT: Duration = Duration::from_secs(600);

/// How often the task that writes sightings looks again.
#[cfg(not(test))]
const SETTLE_TICK: Duration = Duration::from_secs(5);
#[cfg(test)]
const SETTLE_TICK: Duration = Duration::from_millis(40);

/// Who a transaction that only records which agents saw a flag is by.
const SIGHTING_ACTOR: &str = "kin-daemon:review-flag-sightings";

/// Commit one planned write to repository authority, then apply it to the
/// live graph. The caller holds the coordination gate and a graph-authority
/// mutation guard.
fn commit(
    state: &DaemonState,
    write: &FlagWrite,
    action: &str,
    actor: &str,
) -> Result<(), (StatusCode, String)> {
    use crate::review_write::ReviewWriteRefusal;
    let failed = |detail: String| (StatusCode::INTERNAL_SERVER_ERROR, detail);
    let delta = write
        .to_delta()
        .map_err(|error| failed(format!("{error:#}")))?;
    crate::review_write::commit_collaboration_records(state, delta, action, actor, |graph| {
        write.apply_to(graph).map_err(|error| format!("{error:#}"))
    })
    .map(drop)
    .map_err(|refusal| {
        let status = match &refusal {
            ReviewWriteRefusal::Hosted | ReviewWriteRefusal::Commit { conflict: true, .. } => {
                StatusCode::CONFLICT
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, refusal.to_string())
    })
}

/// Add the sightings still held in memory to the flags a reader is shown, so
/// an agent that was handed a flag a moment ago is already named.
fn show_pending(shown: &mut [ReviewFlagView], pending: &[FlagSighting]) {
    for (flag_id, sighting) in pending {
        let flag_id = flag_id.to_string();
        let session = sighting.session.to_string();
        let Some(flag) = shown.iter_mut().find(|flag| flag.flag_id == flag_id) else {
            continue;
        };
        if flag.seen_by.iter().all(|seen| seen.session != session) {
            flag.seen_by.push(ReviewFlagSeen {
                agent: sighting.agent.clone(),
                session,
                at: sighting.at.clone(),
            });
        }
    }
}

/// POST /review/flags: set, close or list review flags in the repo daemon.
pub(crate) async fn route(
    State(state): State<Arc<DaemonState>>,
    Json(request): Json<ReviewFlagRequest>,
) -> Result<Json<ReviewFlagResponse>, (StatusCode, String)> {
    if !state
        .is_initialized
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "daemon not fully initialized".to_string(),
        ));
    }
    let mutates = request.mutates();
    let _coordinated = if mutates {
        Some(state.coordination_gate.lock().await)
    } else {
        None
    };
    state
        .prepared_publication
        .ensure_serving()
        .map_err(crate::prepared_publication::write_refusal)?;
    let _mutation = mutates.then(|| state.begin_graph_authority_mutation());
    let graph = state.graph.as_ref();
    let activity = state.coordinator.activity();
    let mut planned = flags::plan(graph, request)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")))?;
    if !planned.write.is_empty() {
        // A flag being closed, explicitly or by replacement, first takes
        // its pending sightings. The replacement has a new identity and
        // starts unseen while the old record keeps everyone it reached.
        let written: Vec<AnnotationId> = planned
            .write
            .annotations
            .iter()
            .map(|annotation| annotation.annotation_id)
            .collect();
        let held = activity.take_flag_sightings(|(flag, _)| written.contains(flag));
        for annotation in &mut planned.write.annotations {
            let Some(flag) = annotation
                .review
                .as_mut()
                .filter(|flag| flag.resolution.is_some())
            else {
                continue;
            };
            for (_, sighting) in held
                .iter()
                .filter(|(id, _)| *id == annotation.annotation_id)
            {
                flag.record_sighting(sighting.clone());
            }
        }
        if planned.action == flags::RESOLVE_ACTION {
            if let Some(annotation) = planned.write.annotations.first() {
                planned.response.flag = flags::view(graph, annotation);
            }
        }
        if let Err(refusal) = commit(&state, &planned.write, planned.action, &planned.actor) {
            if activity.return_flag_sightings(held) {
                start_settling(Arc::clone(&state));
            }
            return Err(refusal);
        }
    }
    let mut response = planned.response;
    response.flags = response
        .flags
        .into_iter()
        .filter_map(|shown| {
            let id = Uuid::parse_str(&shown.flag_id).ok().map(AnnotationId)?;
            let annotation = graph.get_annotation(&id).ok().flatten()?;
            view(&state, &annotation)
        })
        .collect();
    response.flag = response.flag.and_then(|shown| {
        let id = Uuid::parse_str(&shown.flag_id).ok().map(AnnotationId)?;
        let annotation = graph.get_annotation(&id).ok().flatten()?;
        view(&state, &annotation)
    });
    let pending = activity.pending_flag_sightings();
    show_pending(&mut response.flags, &pending);
    if let Some(flag) = response.flag.as_mut() {
        show_pending(std::slice::from_mut(flag), &pending);
    }
    Ok(Json(response))
}

/// Whether what `sighting`'s session saw may be written now: the session has
/// ended, or it has made no call for [`QUIET_BEFORE_WRITE`].
fn is_quiet(state: &DaemonState, sighting: &ReviewFlagSighting, now: &Timestamp) -> bool {
    let coordinator = &state.coordinator;
    let live = coordinator
        .get_session(&sighting.session)
        .ok()
        .flatten()
        .is_some();
    if !live {
        return true;
    }
    if coordinator.active_call_count(&sighting.session) > 0 {
        return false;
    }
    let last_heard = coordinator
        .activity()
        .last_call_at(&sighting.session)
        .unwrap_or_else(|| sighting.at.clone());
    now.0
        .signed_duration_since(last_heard.0)
        .to_std()
        .is_ok_and(|idle| idle >= QUIET_BEFORE_WRITE)
}

/// Write into the repository every held sighting whose session has gone quiet
/// or ended, as one transaction. Returns how many sightings it took.
pub(crate) async fn settle_sightings(state: &Arc<DaemonState>) -> usize {
    if crate::agent_authorship::any_session_at_work(state) {
        return 0;
    }
    let now = Timestamp::now();
    let activity = state.coordinator.activity();
    if !activity
        .pending_flag_sightings()
        .iter()
        .any(|(_, sighting)| is_quiet(state, sighting, &now))
    {
        return 0;
    }
    let _coordinated = state.coordination_gate.lock().await;
    if state.prepared_publication.ensure_serving().is_err() {
        // Not serving writes now. What is held stays held for the next look.
        return 0;
    }
    if crate::agent_authorship::any_session_at_work(state) {
        return 0;
    }
    let ready = activity.take_flag_sightings(|(_, sighting)| is_quiet(state, sighting, &now));
    if ready.is_empty() {
        return 0;
    }
    let _mutation = state.begin_graph_authority_mutation();
    let written = flags::plan_sightings(state.graph.as_ref(), &ready)
        .map_err(|error| format!("{error:#}"))
        .and_then(|write| {
            if write.is_empty() {
                return Ok(());
            }
            commit(state, &write, SIGHTING_ACTION, SIGHTING_ACTOR).map_err(|(_, detail)| detail)
        });
    let taken = ready.len();
    if let Err(error) = written {
        // Held again for the next look: a sighting is a record of something
        // that happened, and one failed write does not unmake it. One that
        // has failed for this long is let go, so a store that cannot take the
        // write is not asked forever.
        tracing::debug!(%error, "review flag sightings could not be written");
        let recent: Vec<FlagSighting> = ready
            .into_iter()
            .filter(|(_, sighting)| {
                now.0
                    .signed_duration_since(sighting.at.0)
                    .to_std()
                    .is_ok_and(|age| age < UNWRITTEN_SIGHTING_KEPT)
            })
            .collect();
        activity.return_flag_sightings(recent);
    }
    taken
}

/// Run until nothing is held, writing sightings as their sessions go quiet.
fn start_settling(state: Arc<DaemonState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SETTLE_TICK).await;
            settle_sightings(&state).await;
            if state.coordinator.activity().flag_sightings_settled() {
                break;
            }
        }
    });
}

/// Compare exact-source flags with the current entity span admitted in the
/// graph. Bytes are read only from its recorded content digest, never from a
/// filesystem projection, and only the resulting span digest leaves here.
fn view(state: &DaemonState, annotation: &Annotation) -> Option<ReviewFlagView> {
    flags::view_with_source(state.graph.as_ref(), annotation, |entity| {
        let span = entity.span.as_ref()?;
        let blob = entity.metadata.extra.get("blob_hash")?.as_str()?;
        let digest = kin_blobs::Hash256::from_hex(blob).ok()?;
        let bytes = state.blobs.read(&digest).ok()?;
        let body = bytes.get(span.start_byte..span.end_byte)?;
        Some(kin_model::Hash256::from_bytes(kin_blobs::digest_bytes(
            body,
        )))
    })
}

fn notice(view: ReviewFlagView) -> FlagNotice {
    FlagNotice {
        entity: view.entity_id,
        name: view.name,
        note: view.note,
        by: view.author,
        at: view
            .set_at
            .0
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        changed_since: view.changed_since,
        source_unavailable: view.source_unavailable,
    }
}

/// The entity ids a finished call named: its request's whole-id arguments and
/// every id that stands alone in its answer.
fn named_by_call(targets: &[Uuid], result: &kin_mcp::ToolCallResult) -> HashSet<Uuid> {
    let mut found = Vec::new();
    for block in &result.content {
        let kin_mcp::ContentBlock::Text { text } = block;
        crate::agent_activity::uuid_candidates(text, ANSWER_ID_CANDIDATES, &mut found);
    }
    targets.iter().copied().chain(found).collect()
}

/// The open flags one finished agent call reaches, for the reply the agent
/// reads, and the record that its session was handed them.
///
/// A call reaches the flags on the entities its request or its answer named,
/// and `status` reaches every open flag. The live graph is read whatever
/// graph the session's own scope selects: a flag is about the work in
/// progress, not about the commit a session is looking back at.
pub(crate) fn reached_by_call(
    state: &Arc<DaemonState>,
    session_id: Option<SessionId>,
    tool: &str,
    targets: &[Uuid],
    result: &kin_mcp::ToolCallResult,
    flags_cursor: Option<&serde_json::Value>,
) -> Result<Delivered, String> {
    if tool.is_empty() || crate::agent_activity::is_lifecycle_tool(tool) {
        return Ok(Delivered::default());
    }
    let listing = tool == LISTING_TOOL && result.is_error != Some(true);
    let graph = state.graph.as_ref();
    let mut total = None;
    let reached: Vec<Annotation> = if listing {
        let open = flags::open_flags(graph);
        total = Some(open.len());
        let offset = match flags_cursor {
            None | Some(serde_json::Value::Null) => 0,
            Some(serde_json::Value::String(cursor)) => {
                let id = cursor
                    .strip_prefix("review-flags:")
                    .and_then(|id| Uuid::parse_str(id).ok())
                    .ok_or_else(|| {
                        "Invalid flags_cursor; restart status without flags_cursor.".to_string()
                    })?;
                open.iter()
                    .position(|annotation| annotation.annotation_id.0 == id)
                    .map(|index| index + 1)
                    .ok_or_else(|| {
                        "Stale flags_cursor: its flag was closed or replaced; restart status \
                         without flags_cursor."
                            .to_string()
                    })?
            }
            Some(_) => return Err("flags_cursor must be a string.".to_string()),
        };
        open.into_iter().skip(offset).collect()
    } else {
        // Nothing is read out of the answer unless a flag is open.
        if graph.open_review_flags().is_empty() {
            return Ok(Delivered::default());
        }
        flags::reached_by(graph, &named_by_call(targets, result))
    };
    let limit = if listing {
        LISTING_FLAGS_MAX
    } else {
        ANSWER_FLAGS_MAX
    };
    let more = reached.len().saturating_sub(limit);
    let handed: Vec<&Annotation> = reached.iter().take(limit).collect();
    let delivered = Delivered {
        flags: handed
            .iter()
            .filter_map(|annotation| view(state, annotation))
            .map(notice)
            .collect(),
        more,
        total,
        next_cursor: (listing && more > 0)
            .then(|| format!("review-flags:{}", handed.last().unwrap().annotation_id)),
        listing,
    };
    if let Some(session_id) = session_id {
        note_seen(state, session_id, &handed);
    }
    Ok(delivered)
}

/// Hold that `session_id` was handed `handed`, for the flags that do not
/// record it yet, and make sure the task that writes them is running.
fn note_seen(state: &Arc<DaemonState>, session_id: SessionId, handed: &[&Annotation]) {
    let unseen: Vec<AnnotationId> = handed
        .iter()
        .filter(|annotation| {
            annotation
                .review
                .as_ref()
                .is_some_and(|flag| flag.seen_by.iter().all(|seen| seen.session != session_id))
        })
        .map(|annotation| annotation.annotation_id)
        .collect();
    if unseen.is_empty() {
        return;
    }
    // Only a session the daemon holds is an agent it can name.
    let Ok(Some(session)) = state.coordinator.get_session(&session_id) else {
        return;
    };
    let sighting = ReviewFlagSighting {
        agent: flags::agent_name(&session.vendor, &session.client_name),
        session: session_id,
        at: Timestamp::now(),
    };
    if state
        .coordinator
        .activity()
        .note_flag_sightings(&unseen, &sighting)
    {
        start_settling(Arc::clone(state));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use kin_db::EntityStore;
    use kin_model::session::{SessionCapabilities, SessionTransport};
    use tower::ServiceExt;

    /// A daemon over a real store holding two entities.
    fn daemon() -> (
        tempfile::TempDir,
        Arc<DaemonState>,
        kin_model::Entity,
        kin_model::Entity,
    ) {
        kin_core::test_env::install_test_registry();
        let dir = tempfile::tempdir().unwrap();
        let layout = kin_core::init(dir.path()).unwrap().layout;
        let state = Arc::new(DaemonState::open(layout).unwrap());
        state
            .is_initialized
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let send = entity("ServerReply.send", 0x01);
        let serialize = entity("serialize", 0x04);
        state.graph.upsert_entity(&send).unwrap();
        state.graph.upsert_entity(&serialize).unwrap();
        (dir, state, send, serialize)
    }

    fn entity(name: &str, ast: u8) -> kin_model::Entity {
        kin_model::Entity {
            id: kin_model::EntityId::new(),
            kind: kin_model::EntityKind::Method,
            name: name.to_string(),
            language: kin_model::LanguageId::TypeScript,
            fingerprint: kin_model::SemanticFingerprint {
                algorithm: kin_model::FingerprintAlgorithm::V1TreeSitter,
                ast_hash: kin_model::Hash256::from_bytes([ast; 32]),
                signature_hash: kin_model::Hash256::from_bytes([0x02; 32]),
                behavior_hash: kin_model::Hash256::from_bytes([0x03; 32]),
                equivalence_hash: kin_model::Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(kin_model::FilePathId::new("src/reply.ts")),
            span: Some(kin_model::SourceSpan {
                file: kin_model::FilePathId::new("src/reply.ts"),
                start_byte: 0,
                end_byte: 0,
                start_line: 1,
                start_col: 1,
                end_line: 1,
                end_col: 20,
            }),
            signature: format!("{name}(payload: Payload): boolean"),
            visibility: kin_model::Visibility::Public,
            role: kin_model::EntityRole::Source,
            doc_summary: None,
            metadata: Default::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    async fn post(
        state: &Arc<DaemonState>,
        path: &str,
        session: Option<SessionId>,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::post(path).header("content-type", "application/json");
        if let Some(session) = session {
            request = request.header("X-Kin-Session", session.to_string());
        }
        let response = crate::api::router(Arc::clone(state))
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn flag(state: &Arc<DaemonState>, entity: &kin_model::Entity, note: &str) {
        let (status, answer) = post(
            state,
            "/review/flags",
            None,
            serde_json::json!({
                "op": "set", "entity": entity.id, "note": note,
                "fingerprint": format!("behavior:{}", entity.fingerprint.behavior_hash),
                "operation_id": kin_model::OperationId::new(), "author": "Ada Reviewer",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(answer["outcome"], "flagged", "{answer}");
    }

    async fn listed(state: &Arc<DaemonState>) -> Vec<serde_json::Value> {
        let (status, answer) = post(
            state,
            "/review/flags",
            None,
            serde_json::json!({"op": "list"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(answer["outcome"], "listed");
        answer["flags"].as_array().unwrap().clone()
    }

    /// Call a tool the way the server an agent talks to does.
    async fn call(
        state: &Arc<DaemonState>,
        session: Option<SessionId>,
        tool: &str,
        arguments: serde_json::Value,
    ) -> serde_json::Value {
        let (status, answer) = post(
            state,
            "/mcp/tools/call",
            session,
            serde_json::json!({"name": tool, "arguments": arguments}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        answer
    }

    fn agent(state: &Arc<DaemonState>, vendor: &str, client: &str) -> SessionId {
        state
            .coordinator
            .register_session_with_id(
                SessionId::new(),
                vendor,
                client,
                SessionTransport::Mcp,
                None,
                std::env::temp_dir(),
                SessionCapabilities::default(),
            )
            .unwrap()
    }

    #[tokio::test]
    async fn source_flags_compare_only_the_current_recorded_entity_span() {
        let (_dir, state, mut send, _) = daemon();
        let body = "send() { return true; }";
        let stamp = |entity: &mut kin_model::Entity, prefix: &str, body: &str| {
            let bytes = format!("{prefix}{body}");
            let blob = state.blobs.write(bytes.as_bytes()).unwrap();
            entity
                .metadata
                .extra
                .insert("blob_hash".into(), serde_json::json!(blob.to_string()));
            let span = entity.span.as_mut().unwrap();
            span.start_byte = prefix.len();
            span.end_byte = bytes.len();
            state.graph.upsert_entity(entity).unwrap();
        };
        stamp(&mut send, "// sibling\n", body);
        let fingerprint = format!("source:{}", kin_blobs::digest(body.as_bytes()));
        let (status, answer) = post(
            &state,
            "/review/flags",
            None,
            serde_json::json!({
                "op":"set", "entity":send.id, "note":"explain this", "fingerprint":fingerprint,
                "operation_id":kin_model::OperationId::new(), "author":"Ada Reviewer",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(answer["flag"]["fingerprint"], fingerprint);
        assert_eq!(answer["flag"]["changed_since"], false);
        assert!(answer["flag"].get("source_unavailable").is_none());
        // Other source changes alter the blob and span without changing the body.
        stamp(&mut send, "// a different sibling comment\n", body);
        assert_eq!(listed(&state).await[0]["changed_since"], false);
        // Comments inside the entity change its reviewed bytes even though
        // its AST and behavior fingerprints in this fixture stay identical.
        stamp(
            &mut send,
            "// sibling\n",
            "send() { /* explained */ return true; }",
        );
        assert_eq!(listed(&state).await[0]["changed_since"], true);
        send.metadata.extra.remove("blob_hash");
        state.graph.upsert_entity(&send).unwrap();
        assert_eq!(listed(&state).await[0]["source_unavailable"], true);
    }

    #[tokio::test]
    async fn flags_and_agent_sightings_reopen_from_repository_authority() {
        let (_dir, state, send, _) = daemon();
        flag(&state, &send, "a second send must fail loudly").await;
        let session = agent(&state, "codex-mcp-client", "codex-mcp-client");
        call(
            &state,
            Some(session),
            "get_entity",
            serde_json::json!({"entity_id": send.id}),
        )
        .await;
        let before = listed(&state).await;
        state.coordinator.deregister_session(&session).unwrap();
        assert_eq!(settle_sightings(&state).await, 1);
        let reopened = Arc::new(DaemonState::open(state.layout.clone()).unwrap());
        reopened
            .is_initialized
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let after = listed(&reopened).await;
        assert_eq!(after.len(), 1);
        assert_eq!(after[0]["flag_id"], before[0]["flag_id"]);
        assert_eq!(after[0]["note"], before[0]["note"]);
        assert_eq!(after[0]["seen_by"], before[0]["seen_by"]);
        assert_eq!(after[0]["seen_by"][0]["agent"], "Codex");
        assert_eq!(after[0]["seen_by"][0]["session"], session.to_string());
    }

    #[tokio::test]
    async fn the_route_sets_lists_and_closes_a_flag_in_the_live_graph() {
        let (_dir, state, send, _) = daemon();
        assert!(listed(&state).await.is_empty());
        flag(&state, &send, "a second send must fail loudly").await;
        assert!(state.is_dirty(), "a flag is a graph mutation that is saved");

        let open = listed(&state).await;
        assert_eq!(open.len(), 1);
        assert_eq!(open[0]["entity_id"], serde_json::json!(send.id));
        assert_eq!(open[0]["name"], "ServerReply.send");
        assert_eq!(open[0]["note"], "a second send must fail loudly");
        assert_eq!(open[0]["author"], "Ada Reviewer");
        assert_eq!(open[0]["changed_since"], false);
        assert_eq!(open[0]["seen_by"], serde_json::json!([]));

        // A request that cannot be met is answered in words and writes nothing.
        let (status, refused) = post(
            &state,
            "/review/flags",
            None,
            serde_json::json!({
                "op": "set", "entity": kin_model::EntityId::new(), "fingerprint": "ab".repeat(32),
                "operation_id": kin_model::OperationId::new(), "author": "Ada Reviewer",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(refused["outcome"], "refused");
        assert!(refused["error"]
            .as_str()
            .unwrap()
            .contains("not in the repository's graph"));
        assert_eq!(listed(&state).await.len(), 1);

        let (status, closed) = post(
            &state,
            "/review/flags",
            None,
            serde_json::json!({
                "op": "resolve", "entity": send.id,
                "operation_id": kin_model::OperationId::new(), "author": "Ada Reviewer",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(closed["outcome"], "resolved");
        assert!(listed(&state).await.is_empty());
    }

    #[tokio::test]
    async fn an_answer_that_returns_a_flagged_entity_carries_the_flag_beside_it() {
        let (_dir, state, send, serialize) = daemon();
        // With no flag open, an answer is exactly what it was.
        let plain = call(
            &state,
            None,
            "get_entity",
            serde_json::json!({"entity_id": send.id}),
        )
        .await;
        assert!(plain.get("_meta").is_none(), "{plain}");

        flag(&state, &send, "a second send must fail loudly").await;
        let mut answered = call(
            &state,
            None,
            "get_entity",
            serde_json::json!({"entity_id": send.id}),
        )
        .await;
        let reached = kin_mcp::review_flags::take(&mut answered).expect("the flag travels");
        assert!(!reached.listing);
        assert_eq!(reached.more, 0);
        assert_eq!(reached.flags.len(), 1);
        assert_eq!(reached.flags[0].entity, send.id.to_string());
        assert_eq!(reached.flags[0].name, "ServerReply.send");
        assert_eq!(reached.flags[0].note, "a second send must fail loudly");
        assert_eq!(reached.flags[0].by, "Ada Reviewer");
        assert!(
            reached.flags[0].at.ends_with('Z'),
            "{}",
            reached.flags[0].at
        );
        assert!(!reached.flags[0].changed_since);
        // The answer itself is untouched: the flag is beside it.
        assert_eq!(answered["content"], plain["content"]);
        assert_eq!(
            kin_mcp::review_flags::advice(&reached).unwrap(),
            "A reviewer flagged ServerReply.send: a second send must fail loudly. Address it \
             before committing."
        );

        // An answer about another entity reaches no flag.
        let other = call(
            &state,
            None,
            "get_entity",
            serde_json::json!({"entity_id": serialize.id}),
        )
        .await;
        assert!(other.get("_meta").is_none(), "{other}");

        // Once the entity changes, the flag says so.
        let mut edited = send.clone();
        edited.fingerprint.behavior_hash = kin_model::Hash256::from_bytes([0x09; 32]);
        state.graph.upsert_entity(&edited).unwrap();
        let mut answered = call(
            &state,
            None,
            "get_entity",
            serde_json::json!({"entity_id": send.id}),
        )
        .await;
        let reached = kin_mcp::review_flags::take(&mut answered).unwrap();
        assert!(reached.flags[0].changed_since);
    }

    #[tokio::test]
    async fn status_answers_with_every_open_flag_and_nothing_when_there_are_none() {
        let (_dir, state, send, serialize) = daemon();
        let mut answered = call(&state, None, "kin_graph_status", serde_json::json!({})).await;
        assert_eq!(kin_mcp::review_flags::take(&mut answered), None);

        flag(&state, &send, "first").await;
        flag(&state, &serialize, "second").await;
        let mut answered = call(&state, None, "kin_graph_status", serde_json::json!({})).await;
        let all = kin_mcp::review_flags::take(&mut answered).unwrap();
        assert!(all.listing);
        assert_eq!(
            all.flags
                .iter()
                .map(|flag| flag.name.as_str())
                .collect::<Vec<_>>(),
            vec!["ServerReply.send", "serialize"],
            "oldest first"
        );
    }

    #[tokio::test]
    async fn a_session_handed_a_flag_is_named_at_once_and_written_when_it_goes_quiet() {
        let (_dir, state, send, _) = daemon();
        flag(&state, &send, "a second send must fail loudly").await;
        let session = agent(&state, "codex-mcp-client", "codex-mcp-client");

        // A call with no session names nobody.
        call(
            &state,
            None,
            "get_entity",
            serde_json::json!({"entity_id": send.id}),
        )
        .await;
        assert_eq!(listed(&state).await[0]["seen_by"], serde_json::json!([]));

        call(
            &state,
            Some(session),
            "get_entity",
            serde_json::json!({"entity_id": send.id}),
        )
        .await;
        // Named at once, from what the daemon holds.
        let seen = listed(&state).await[0]["seen_by"].clone();
        assert_eq!(seen.as_array().unwrap().len(), 1, "{seen}");
        assert_eq!(seen[0]["agent"], "Codex");
        assert_eq!(seen[0]["session"], serde_json::json!(session.to_string()));
        assert!(seen[0]["at"].is_string());
        // And named once, however often the session reads the entity.
        call(
            &state,
            Some(session),
            "get_entity",
            serde_json::json!({"entity_id": send.id}),
        )
        .await;
        assert_eq!(
            listed(&state).await[0]["seen_by"].as_array().unwrap().len(),
            1
        );

        // Written into the flag itself once the session has gone quiet, so it
        // outlives the session and the daemon.
        let stored = || {
            flags::open_flags(state.graph.as_ref())[0]
                .review
                .clone()
                .unwrap()
                .seen_by
        };
        let mut waited = Duration::ZERO;
        while stored().is_empty() && waited < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(25)).await;
            waited += Duration::from_millis(25);
        }
        let written = stored();
        assert_eq!(written.len(), 1, "the sighting was never written");
        assert_eq!(written[0].session, session);
        assert_eq!(written[0].agent, "Codex");
        assert!(state
            .coordinator
            .activity()
            .pending_flag_sightings()
            .is_empty());
        state.coordinator.deregister_session(&session).unwrap();
        let seen = listed(&state).await[0]["seen_by"].clone();
        assert_eq!(seen.as_array().unwrap().len(), 1, "{seen}");
        assert_eq!(seen[0]["session"], serde_json::json!(session.to_string()));
    }

    #[tokio::test]
    async fn closing_a_flag_keeps_who_saw_it_and_a_new_flag_starts_unseen() {
        let (_dir, state, send, _) = daemon();
        flag(&state, &send, "first").await;
        let session = agent(&state, "claude-code", "claude-code");
        call(
            &state,
            Some(session),
            "kin_graph_status",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(
            listed(&state).await[0]["seen_by"][0]["agent"],
            "Claude Code"
        );

        // Flagged again with something new to say, nobody has seen it.
        flag(&state, &send, "second").await;
        let open = listed(&state).await;
        assert_eq!(open.len(), 1);
        assert_eq!(open[0]["note"], "second");
        assert_eq!(open[0]["seen_by"], serde_json::json!([]));

        call(
            &state,
            Some(session),
            "get_entity",
            serde_json::json!({"entity_id": send.id}),
        )
        .await;
        let (_, closed) = post(
            &state,
            "/review/flags",
            None,
            serde_json::json!({
                "op": "resolve", "entity": send.id,
                "operation_id": kin_model::OperationId::new(), "author": "Ada Reviewer",
            }),
        )
        .await;
        assert_eq!(closed["outcome"], "resolved");
        assert_eq!(closed["flag"]["seen_by"][0]["agent"], "Claude Code");
        assert!(state
            .coordinator
            .activity()
            .pending_flag_sightings()
            .is_empty());
    }

    #[tokio::test]
    async fn status_flag_pages_reach_every_note_and_stale_pages_name_no_new_sightings() {
        let (_dir, state, send, serialize) = daemon();
        let mut entities = vec![send, serialize];
        for index in 2..(LISTING_FLAGS_MAX + 2) {
            let entity = entity(&format!("flagged_{index}"), index as u8);
            state.graph.upsert_entity(&entity).unwrap();
            entities.push(entity);
        }
        for (index, entity) in entities.iter().enumerate() {
            flag(&state, entity, &format!("review note {index}")).await;
        }
        let session = agent(&state, "codex-mcp-client", "codex-mcp-client");
        let mut first = call(
            &state,
            Some(session),
            "kin_graph_status",
            serde_json::json!({}),
        )
        .await;
        let first = kin_mcp::review_flags::take(&mut first).unwrap();
        assert_eq!(first.flags.len(), LISTING_FLAGS_MAX);
        assert_eq!(first.more, 2);
        assert_eq!(first.total, Some(entities.len()));
        let cursor = first.next_cursor.unwrap();
        let first_seen = listed(&state)
            .await
            .into_iter()
            .filter(|flag| !flag["seen_by"].as_array().unwrap().is_empty())
            .count();
        assert_eq!(first_seen, LISTING_FLAGS_MAX);

        let mut next = call(
            &state,
            Some(session),
            "kin_graph_status",
            serde_json::json!({
                "flags_cursor": cursor,
            }),
        )
        .await;
        let next = kin_mcp::review_flags::take(&mut next).unwrap();
        assert_eq!(next.flags.len(), 2);
        assert_eq!(next.more, 0);
        assert_eq!(next.total, Some(entities.len()));
        assert!(next.next_cursor.is_none());
        let reached: std::collections::HashSet<_> = first
            .flags
            .iter()
            .chain(&next.flags)
            .map(|flag| flag.entity.clone())
            .collect();
        assert_eq!(reached.len(), entities.len());

        let last = Uuid::parse_str(&first.flags.last().unwrap().entity).unwrap();
        let last = entities.iter().find(|entity| entity.id.0 == last).unwrap();
        flag(&state, last, "a replacement nobody has seen").await;
        let mut stale = call(
            &state,
            Some(session),
            "kin_graph_status",
            serde_json::json!({
                "flags_cursor": cursor,
            }),
        )
        .await;
        assert_eq!(stale["isError"], true, "{stale}");
        assert!(
            stale["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Stale flags_cursor"),
            "{stale}"
        );
        assert!(kin_mcp::review_flags::take(&mut stale).is_none());
        let listed = listed(&state).await;
        let replacement = listed
            .iter()
            .find(|flag| flag["entity_id"] == last.id.to_string())
            .unwrap();
        assert_eq!(replacement["seen_by"], serde_json::json!([]));

        let invalid = call(
            &state,
            Some(session),
            "kin_graph_status",
            serde_json::json!({
                "flags_cursor": 7,
            }),
        )
        .await;
        assert_eq!(invalid["isError"], true, "{invalid}");
        assert!(invalid["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("flags_cursor must be a string"));
    }
}
