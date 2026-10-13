// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Human inspection of a parked merge, and settling it one step at a time.
//!
//! The page is shown the steps this merge offers now, each under a stable
//! id, and sends back one id with the identity of the record it was looking
//! at. The server derives the steps again from the record as it is, refuses
//! an id it does not derive or a record that has moved, and performs that one
//! step through the daemon's resolve path, the path `kin resolve` takes. A
//! caller never supplies what is done, only which offered step to take. The
//! durable record stays intact; only source display text may be shortened,
//! with explicit byte counts.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use kin_model::{MergeConflictEntry, MergeConflictSubject, MergeDivergence, MergeSide};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::conflicts::{ConflictsReport, ConflictsRequest, CONFLICTS_REPORT_SCHEMA};
use super::resolve::{ResolveAction, ResolveChoice, ResolveDirective, ResolveRequest};
use super::review_ui::{self, UiState};

const BODY_LIMIT: usize = 16 * 1024;
const TOTAL_BODY_LIMIT: usize = 128 * 1024;
const BODY_ROW_LIMIT: usize = 200;
const RECORD_LIMIT: usize = 256 * 1024;
const RESPONSE_LIMIT: usize = 1024 * 1024;

/// GET within the existing repository-scoped, origin-protected UI router.
pub(super) async fn serve_conflicts(State(state): State<Arc<UiState>>) -> Response {
    let answer = async {
        let client = review_ui::daemon(&state).await?;
        let value = details(&client).await?;
        bounded_json(&value, RESPONSE_LIMIT)
            .context("The conflict details are larger than this page can show.")
    }
    .await;
    match answer {
        Ok(bytes) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "application/json; charset=utf-8",
            )],
            bytes,
        )
            .into_response(),
        Err(error) => {
            let mut message = format!("{error:#}");
            if message.len() > 4096 {
                shorten(&mut message, 4096);
                message.push_str(" (error detail truncated)");
            }
            review_ui::refusal(StatusCode::BAD_GATEWAY, &anyhow::anyhow!("{message}"))
        }
    }
}

/// The daemon's repository and workspace identity, which every report it
/// answers with has to carry.
async fn identity(client: &crate::daemon_client::DaemonClient) -> Result<(String, String)> {
    let health = client.health().await?;
    Ok((
        health
            .repo_id
            .context("The daemon omitted its repository identity.")?,
        health
            .workspace_id
            .context("The daemon omitted its workspace identity.")?,
    ))
}

/// The conflict details as the page reads them, from the merge as it is now.
async fn details(client: &crate::daemon_client::DaemonClient) -> Result<Value> {
    let (repository, workspace) = identity(client).await?;
    let report = client
        .conflicts(&ConflictsRequest { bodies: true })
        .await?
        .report
        .context("The daemon omitted its conflict report.")?;
    present(report, &repository, &workspace)
}

/// Fail on oversized authority rather than silently deleting entries or
/// rehashing a record. This also bounds serialization without allocating an
/// unbounded second copy of the daemon response.
fn bounded_json(value: &impl Serialize, limit: usize) -> Result<Vec<u8>> {
    struct Limited {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("JSON byte limit exceeded"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Limited {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.bytes)
}

fn shorten(text: &mut String, limit: usize) {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

fn selector(subject: &MergeConflictSubject) -> String {
    match subject {
        MergeConflictSubject::Entity { entity } => entity.to_string(),
        MergeConflictSubject::Relation { relation } => relation.to_string(),
        MergeConflictSubject::Artifact { artifact } => artifact.0.to_string(),
        MergeConflictSubject::Path { path } => path.to_string(),
    }
}

fn body_subject(subject: &MergeConflictSubject) -> String {
    let kind = match subject {
        MergeConflictSubject::Entity { .. } => "entity",
        MergeConflictSubject::Relation { .. } => "relation",
        MergeConflictSubject::Artifact { .. } => "artifact",
        MergeConflictSubject::Path { .. } => "path",
    };
    format!("{kind} {}", selector(subject))
}

/// Match the resolver's actual selector contract, including labels. A UUID is
/// not automatically preferred over another entry whose label is that UUID.
fn selector_matches(entry: &MergeConflictEntry, needle: &str) -> bool {
    selector(&entry.subject) == needle
        || entry.label.as_deref().is_some_and(|label| {
            label == needle
                || label
                    .split_once(" in ")
                    .is_some_and(|(name, _)| name == needle)
        })
}

fn safe_selector(entry: &MergeConflictEntry, entries: &[MergeConflictEntry]) -> bool {
    if matches!(&entry.subject, MergeConflictSubject::Path { path } if path.as_utf8().is_none()) {
        return false;
    }
    let value = selector(&entry.subject);
    !value.is_empty()
        && value.trim() == value
        && !value.chars().any(char::is_control)
        && entries
            .iter()
            .filter(|entry| selector_matches(entry, &value))
            .count()
            == 1
}

/// Check that a report is this repository's, and that its record is the one
/// its hash and counts describe. Everything a step is derived from passes
/// here first, whether or not the report carries source bodies.
fn validate_record(report: &ConflictsReport, repository: &str, workspace: &str) -> Result<()> {
    anyhow::ensure!(
        report.schema == CONFLICTS_REPORT_SCHEMA && report.authority == "repository-v6",
        "The daemon returned an unsupported conflict report."
    );
    anyhow::ensure!(
        report.repository_id.as_str() == repository && report.workspace_id.to_string() == workspace,
        "The conflict report belongs to another repository or workspace."
    );
    bounded_json(&report.record, RECORD_LIMIT).context(
        "The merge record is larger than this page can show. No authority entries were omitted.",
    )?;
    let Some(record) = &report.record else {
        anyhow::ensure!(
            report.record_hash.is_none()
                && report.unresolved_count == 0
                && report.resolved_count == 0
                && report.bodies.is_empty(),
            "The daemon returned conflict details without a merge record."
        );
        return Ok(());
    };
    record
        .validate()
        .context("The merge record failed identity validation.")?;
    anyhow::ensure!(
        record.repository_id == report.repository_id && record.workspace_id == report.workspace_id,
        "The merge record and conflict report identify different workspaces."
    );
    if let Some(hash) = &report.record_hash {
        anyhow::ensure!(
            *hash == record.hash.to_string(),
            "The reported merge hash differs from its durable record."
        );
    }
    let unresolved = record.unresolved().count();
    anyhow::ensure!(
        report.unresolved_count == unresolved
            && report.resolved_count == record.entries.len() - unresolved,
        "The reported conflict counts differ from the durable record."
    );
    Ok(())
}

fn validate_report(report: &ConflictsReport, repository: &str, workspace: &str) -> Result<()> {
    validate_record(report, repository, workspace)?;
    let Some(record) = &report.record else {
        return Ok(());
    };
    let expected: BTreeMap<_, _> = record
        .entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.subject,
                MergeConflictSubject::Entity { .. } | MergeConflictSubject::Artifact { .. }
            )
        })
        .map(|entry| (body_subject(&entry.subject), entry))
        .collect();
    let mut seen = BTreeSet::new();
    for body in &report.bodies {
        let entry = expected
            .get(&body.subject)
            .context("A conflict body names an unknown source subject.")?;
        anyhow::ensure!(
            seen.insert(body.subject.clone()),
            "The daemon repeated a conflict body."
        );
        anyhow::ensure!(
            body.unverified
                .iter()
                .all(|side| matches!(side.as_str(), "base" | "ours" | "theirs")),
            "A conflict body names an unknown unverified side."
        );
        for (name, text, recorded) in [
            ("base", &body.base, &entry.base),
            ("ours", &body.ours, &entry.ours),
            ("theirs", &body.theirs, &entry.theirs),
        ] {
            let unverified = body.unverified.iter().any(|side| side == name);
            anyhow::ensure!(
                match text {
                    Some(_) => recorded.is_present() && !unverified,
                    None => !recorded.is_present() || unverified,
                },
                "A conflict body disagrees with its recorded side or verification status."
            );
        }
    }
    anyhow::ensure!(
        seen.len() == expected.len(),
        "The daemon omitted source subjects from its requested conflict bodies."
    );
    Ok(())
}

fn display_bodies(report: &mut ConflictsReport) -> Value {
    let omitted = report.bodies.len().saturating_sub(BODY_ROW_LIMIT);
    report.bodies.truncate(BODY_ROW_LIMIT);
    let mut remaining = TOTAL_BODY_LIMIT;
    let mut truncations = Vec::new();
    for body in &mut report.bodies {
        for (side, text) in [
            ("base", &mut body.base),
            ("ours", &mut body.ours),
            ("theirs", &mut body.theirs),
        ] {
            if let Some(text) = text {
                let original_bytes = text.len();
                shorten(text, remaining.min(BODY_LIMIT));
                remaining -= text.len();
                if text.len() != original_bytes {
                    truncations.push(json!({
                        "subject": body.subject, "side": side,
                        "original_bytes": original_bytes, "displayed_bytes": text.len(),
                    }));
                }
            }
        }
    }
    json!({
        "body_limit_bytes": BODY_LIMIT, "total_body_limit_bytes": TOTAL_BODY_LIMIT,
        "truncated": omitted > 0 || !truncations.is_empty(),
        "truncations": truncations, "omitted_body_rows": omitted,
    })
}

/// One step this merge offers now: what the page shows of it, and exactly
/// what taking it does. The action is derived here from the record and is
/// never read back from a caller.
struct Step {
    id: String,
    label: &'static str,
    /// The conflict the step settles. Publishing and abandoning name none.
    selector: Option<String>,
    action: ResolveAction,
}

impl Step {
    fn settling(id: String, label: &'static str, selector: &str, choice: ResolveChoice) -> Self {
        Self {
            id,
            label,
            selector: Some(selector.to_string()),
            action: ResolveAction::Settle {
                directives: vec![ResolveDirective {
                    selector: selector.to_string(),
                    choice,
                }],
                all: None,
            },
        }
    }
}

/// What the merge in `report` offers.
struct Offered {
    steps: Vec<Step>,
    /// Conflicts no step can name without ambiguity, each with why.
    unavailable_subjects: Vec<Value>,
    /// Why no step is offered at all, when none is.
    unavailable: Option<&'static str>,
}

/// The steps the merge in `report` offers now, each bound to that record.
fn offered(report: &ConflictsReport) -> Offered {
    let mut steps = Vec::new();
    let mut unavailable_subjects = Vec::new();
    let unavailable = match (&report.record, report.record_hash.as_deref()) {
        (None, _) => Some("No merge is parked in this workspace."),
        (Some(record), _) if !record.state.is_in_progress() => {
            Some("This merge has already terminated.")
        }
        (Some(_), None) => {
            Some("The daemon omitted the expected record hash. Refresh before resolving.")
        }
        (Some(record), Some(_)) => {
            for entry in record.unresolved() {
                let selected = selector(&entry.subject);
                if !safe_selector(entry, &record.entries) {
                    unavailable_subjects.push(json!({
                        "selector":selected,
                        "reason":"Kin cannot name this conflict unambiguously by its exact identity, so it cannot be settled here.",
                    }));
                    continue;
                }
                if let MergeDivergence::PathCollision { artifacts } = &entry.divergence {
                    for artifact in artifacts {
                        steps.push(Step::settling(
                            format!("keep-path:{selected}:{}", artifact.0),
                            "Keep this claimant at the contested place",
                            &selected,
                            ResolveChoice::PathOwner {
                                artifact: artifact.0.to_string(),
                            },
                        ));
                    }
                    continue;
                }
                for (name, side, label) in [
                    ("ours", MergeSide::Ours, "Use the recorded target side"),
                    (
                        "theirs",
                        MergeSide::Theirs,
                        "Use the recorded incoming side",
                    ),
                    ("base", MergeSide::Base, "Use the recorded base side"),
                ] {
                    if side == MergeSide::Base && !entry.base.is_present() {
                        continue;
                    }
                    steps.push(Step::settling(
                        format!("{name}:{selected}"),
                        label,
                        &selected,
                        ResolveChoice::Side { side },
                    ));
                }
                steps.push(Step::settling(
                    format!("remove:{selected}"),
                    "Remove this identity from the merge",
                    &selected,
                    ResolveChoice::Remove,
                ));
            }
            if record.is_fully_resolved() {
                steps.push(Step {
                    id: "continue".into(),
                    label: "Publish the recorded resolutions",
                    selector: None,
                    action: ResolveAction::Continue,
                });
            }
            steps.push(Step {
                id: "abort".into(),
                label: "Abandon the merge and keep the working copy as it is",
                selector: None,
                action: ResolveAction::Abort,
            });
            None
        }
    };
    Offered {
        steps,
        unavailable_subjects,
        unavailable,
    }
}

/// What the page is shown of the steps: each one's id, label and the
/// conflict it settles. What a step does stays on this side.
fn handoff(report: &ConflictsReport) -> Value {
    let offered = offered(report);
    let commands: Vec<Value> = offered
        .steps
        .iter()
        .map(|step| {
            json!({"id":step.id, "label":step.label, "modifies":true, "selector":step.selector})
        })
        .collect();
    json!({
        "commands":commands, "unavailable_subjects":offered.unavailable_subjects,
        "unavailable":offered.unavailable,
        "instructions":[
            "Take one step at a time. After each one this page reads the merge again, and the next step is taken against the record as it is then.",
            "A side choice can remove an identity when that recorded side is absent. Base, target and incoming source are historical display text, not current editing authority.",
            "Abandoning a merge discards its conflict state and leaves the working copy untouched. Publishing remains a separate guarded step.",
            "If a step's outcome is uncertain, read the repository and conflict state again before another step. Nothing is retried automatically."
        ],
    })
}

/// The longest step id a request may carry. An id is a selector and at most
/// one claimant's identity, both bounded by the record.
const STEP_ID_LIMIT: usize = 8 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SettleBody {
    /// The id of one step, as the conflict details listed it.
    command: String,
    /// The identity of the record the page was looking at.
    record_hash: String,
    operation_id: kin_model::OperationId,
}

/// A 32-byte record identity from its hex, or nothing when it is not one.
fn record_identity(hex_text: &str) -> Option<kin_model::Hash256> {
    let bytes: [u8; 32] = hex::decode(hex_text.trim()).ok()?.try_into().ok()?;
    Some(kin_model::Hash256::from_bytes(bytes))
}

/// The step was not taken: nothing reached the daemon's resolve path.
fn settle_refused(operation_id: kin_model::OperationId, error: &anyhow::Error) -> Response {
    (StatusCode::CONFLICT, Json(json!({
        "error":format!("{error:#}"), "outcome":"refused",
        "operation_id":operation_id,
        "remedy":"Nothing was recorded. Read the conflicts again, then take a step the merge offers now.",
    }))).into_response()
}

/// Take one step of a parked merge: settle one conflict, publish the merge
/// or abandon it.
pub(super) async fn settle(
    State(state): State<Arc<UiState>>,
    headers: HeaderMap,
    Json(body): Json<SettleBody>,
) -> Response {
    if !review_ui::may_write(&headers, &state) {
        return review_ui::write_refused();
    }
    let operation_id = body.operation_id;
    let prepared = async {
        anyhow::ensure!(
            !operation_id.0.is_nil(),
            "a nonempty operation ID is required"
        );
        anyhow::ensure!(
            body.command.len() <= STEP_ID_LIMIT,
            "this step is not one the merge offers"
        );
        let expected = record_identity(&body.record_hash)
            .context("the record identity is not one a merge carries")?;
        // Attribution resolves against this URL's repository, never the
        // process working directory or a caller-supplied actor.
        let actor = crate::commands::require_commit_author_for(&state.layout)?;
        let client = review_ui::daemon(&state).await?;
        let (repository, workspace) = identity(&client).await?;
        // Read the merge as it is now. The step is derived from this record,
        // and only its id came from the caller.
        let report = client
            .conflicts(&ConflictsRequest { bodies: false })
            .await?
            .report
            .context("The daemon omitted its conflict report.")?;
        validate_record(&report, &repository, &workspace)?;
        anyhow::ensure!(
            report.record_hash.as_deref() == Some(expected.to_string().as_str()),
            "the merge has moved since this page read it"
        );
        let step = offered(&report)
            .steps
            .into_iter()
            .find(|step| step.id == body.command)
            .context("this step is not one the merge offers now")?;
        Ok::<_, anyhow::Error>((
            client,
            ResolveRequest {
                operation_id,
                actor,
                action: step.action,
                expected_record: Some(expected),
            },
        ))
    }
    .await;
    let (client, request) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return settle_refused(operation_id, &error),
    };
    let outcome = match request.action {
        ResolveAction::Settle { .. } => "settled",
        ResolveAction::Continue => "continued",
        ResolveAction::Abort => "aborted",
    };
    match client.resolve(&request).await {
        Ok(answer) => {
            // Read again after the step, so the page shows the record the
            // next step is taken against. The step stands whatever becomes
            // of this read.
            let (after, unread) = match details(&client).await {
                Ok(after) => (after, None),
                Err(error) => (Value::Null, Some(format!("{error:#}"))),
            };
            let mut body = json!({
                "operation_id":operation_id, "outcome":outcome, "details":after,
            });
            if let Some(unread) = unread {
                body["details_error"] = json!(unread);
            }
            if let Some(change) = answer.report.and_then(|report| report.merge_change) {
                body["merge_change"] = json!(change.to_string());
            }
            Json(body).into_response()
        }
        Err(error) if error.downcast_ref::<crate::daemon_client::RefusedBeforeWriteError>().is_some() => {
            (StatusCode::CONFLICT, Json(json!({
                "error":format!("{error:#}"), "outcome":"refused",
                "operation_id":operation_id,
                "remedy":"The daemon refused before recording anything. Read the conflicts again before another step.",
            }))).into_response()
        }
        // A lost answer can follow a recorded step. Return its identity for
        // reconciliation and never replay it behind the page.
        Err(error) => (StatusCode::BAD_GATEWAY, Json(json!({
            "error":format!("{error:#}"), "outcome":"unknown",
            "operation_id":operation_id,
            "remedy":"The step may have been recorded. Read the repository and its conflicts again before another step. This request was not retried.",
        }))).into_response(),
    }
}

fn present(mut report: ConflictsReport, repository: &str, workspace: &str) -> Result<Value> {
    validate_report(&report, repository, workspace)?;
    let (status, active, all_resolved) = match report.record.as_ref().map(|record| &record.state) {
        None => ("none", false, false),
        Some(kin_model::MergeTransactionState::InProgress) => {
            ("in_progress", true, report.unresolved_count == 0)
        }
        Some(kin_model::MergeTransactionState::Committed { .. }) => {
            ("committed", false, report.unresolved_count == 0)
        }
        Some(kin_model::MergeTransactionState::Aborted { .. }) => {
            ("aborted", false, report.unresolved_count == 0)
        }
    };
    let provenance = json!({
        "scope":"recorded_merge_inputs", "editable":false,
        "record_hash":report.record_hash,
        "base_change":report.record.as_ref().map(|record| record.binding.base_change.to_string()),
        "ours_change":report.record.as_ref().map(|record| record.binding.ours_change.to_string()),
        "theirs_change":report.record.as_ref().map(|record| record.binding.theirs_change.to_string()),
    });
    let handoff = handoff(&report);
    let display = display_bodies(&mut report);
    Ok(json!({
        "schema":"kin.workspace.conflicts.v1", "report":report,
        "state":{"status":status,"active":active,"all_resolved":all_resolved},
        "provenance":provenance, "display":display, "handoff":handoff,
        "read_at":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::{
        AdmissionCase, AuthorId, EffectiveAdmissionPolicyStamp, EntityId, FrozenLocalOverlay,
        Hash256, MergeEntryResolution, MergeOpening, MergeParentBinding, MergeResolutionProvenance,
        MergeSide, MergeSideValue, MergeTransactionRecord, MergeTransactionState,
        MergeWorkspaceRestorePoint, OperationId, RefName, RefTarget, RepositoryId,
        SemanticChangeId, SharedAdmissionPolicy, Timestamp, WorkspaceHead, WorkspaceId,
    };

    fn fixture() -> ConflictsReport {
        let workspace = WorkspaceId::new();
        let change = |byte| SemanticChangeId::from_hash(Hash256::from_bytes([byte; 32]));
        let entry = MergeConflictEntry {
            subject: MergeConflictSubject::Entity {
                entity: EntityId::new(),
            },
            divergence: MergeDivergence::ChangedBothSides,
            base: MergeSideValue::Present {
                content: Hash256::from_bytes([10; 32]),
            },
            ours: MergeSideValue::Present {
                content: Hash256::from_bytes([11; 32]),
            },
            theirs: MergeSideValue::Present {
                content: Hash256::from_bytes([12; 32]),
            },
            label: Some("display label; $(never-execute)".into()),
            resolution: MergeEntryResolution::Unresolved,
        };
        let subject = body_subject(&entry.subject);
        let record = MergeTransactionRecord::open(
            RepositoryId::new("synthetic-conflict-ui").unwrap(),
            workspace,
            MergeOpening {
                operation_id: OperationId::new(),
                actor: AuthorId::new("Fixture"),
                opened_at: Timestamp::now(),
            },
            MergeParentBinding {
                target_ref: RefName::branch(b"main").unwrap(),
                source_ref: RefName::branch(b"feature").unwrap(),
                base_change: change(1),
                ours_change: change(2),
                theirs_change: change(3),
                ours_target: RefTarget::change(change(2)),
                theirs_target: RefTarget::change(change(3)),
            },
            MergeWorkspaceRestorePoint {
                generation: 1,
                head: WorkspaceHead::Symbolic {
                    target: RefName::branch(b"main").unwrap(),
                },
                base_target: Some(RefTarget::change(change(2))),
                base_tree_hash: Some(Hash256::from_bytes([20; 32])),
                tree_hash: Hash256::from_bytes([20; 32]),
                semantic_overlay_hash: Hash256::from_bytes([21; 32]),
                admission_policy: EffectiveAdmissionPolicyStamp {
                    shared: SharedAdmissionPolicy::empty(0).stamp(),
                    local: FrozenLocalOverlay::new(workspace, 0, AdmissionCase::Sensitive, vec![])
                        .unwrap()
                        .stamp(),
                },
            },
            vec![entry],
        )
        .unwrap();
        ConflictsReport {
            schema: CONFLICTS_REPORT_SCHEMA.into(),
            authority: "repository-v6".into(),
            repository_id: record.repository_id.clone(),
            workspace_id: workspace,
            authority_generation: 2,
            record_hash: Some(record.hash.to_string()),
            record: Some(record),
            unresolved_count: 1,
            resolved_count: 0,
            bodies: vec![super::super::conflicts::ConflictBody {
                subject,
                label: None,
                base: Some("base".into()),
                ours: Some("ours".into()),
                theirs: Some("theirs".into()),
                unverified: vec![],
            }],
        }
    }

    fn view(report: ConflictsReport) -> Result<Value> {
        let repository = report.repository_id.to_string();
        let workspace = report.workspace_id.to_string();
        present(report, &repository, &workspace)
    }

    fn fixture_with_entry(entry: MergeConflictEntry) -> ConflictsReport {
        let mut report = fixture();
        let prior = report.record.take().unwrap();
        if matches!(
            entry.subject,
            MergeConflictSubject::Entity { .. } | MergeConflictSubject::Artifact { .. }
        ) {
            report.bodies[0].subject = body_subject(&entry.subject);
            if !entry.base.is_present() {
                report.bodies[0].base = None;
            }
            if !entry.ours.is_present() {
                report.bodies[0].ours = None;
            }
            if !entry.theirs.is_present() {
                report.bodies[0].theirs = None;
            }
        } else {
            report.bodies.clear();
        }
        let record = MergeTransactionRecord::open(
            prior.repository_id,
            prior.workspace_id,
            prior.opened,
            prior.binding,
            prior.restore,
            vec![entry],
        )
        .unwrap();
        report.record_hash = Some(record.hash.to_string());
        report.record = Some(record);
        report
    }

    /// The one directive a settling step carries.
    fn directive(step: &Step) -> &ResolveDirective {
        match &step.action {
            ResolveAction::Settle {
                directives,
                all: None,
            } if directives.len() == 1 => &directives[0],
            other => panic!(
                "{} does not settle exactly one conflict: {other:?}",
                step.id
            ),
        }
    }

    #[test]
    fn every_offered_step_is_derived_from_the_record_and_shown_without_what_it_does() {
        let report = fixture();
        let selected = selector(&report.record.as_ref().unwrap().entries[0].subject);
        let offered = offered(&report);
        assert!(offered.unavailable.is_none());
        let ids: Vec<&str> = offered.steps.iter().map(|step| step.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                format!("ours:{selected}"),
                format!("theirs:{selected}"),
                format!("base:{selected}"),
                format!("remove:{selected}"),
                "abort".to_string(),
            ]
        );
        for (step, choice) in offered.steps.iter().zip([
            ResolveChoice::Side {
                side: MergeSide::Ours,
            },
            ResolveChoice::Side {
                side: MergeSide::Theirs,
            },
            ResolveChoice::Side {
                side: MergeSide::Base,
            },
            ResolveChoice::Remove,
        ]) {
            assert_eq!(directive(step).selector, selected);
            assert_eq!(directive(step).choice, choice);
            assert_eq!(step.selector.as_deref(), Some(selected.as_str()));
        }
        assert_eq!(offered.steps[4].action, ResolveAction::Abort);

        // The page is told which steps exist, and nothing of how one is
        // carried out: no arguments, and no words for a terminal.
        let value = view(report).unwrap();
        assert_eq!(value["state"]["active"], true);
        assert_eq!(value["provenance"]["editable"], false);
        let commands = value["handoff"]["commands"].as_array().unwrap();
        assert_eq!(commands.len(), 5);
        for (command, step) in commands.iter().zip(&offered.steps) {
            let shown: Vec<&String> = command.as_object().unwrap().keys().collect();
            assert_eq!(shown, ["id", "label", "modifies", "selector"]);
            assert_eq!(command["id"], step.id);
            assert_eq!(command["modifies"], true);
        }
        let said = value["handoff"].to_string().to_lowercase();
        for word in ["argv", "shell", "terminal", "kin resolve", "--", " file"] {
            assert!(!said.contains(word), "{word:?} in {said}");
        }
        assert!(!commands.iter().any(|command| command["id"] == "continue"));
    }

    #[test]
    fn a_record_identity_is_thirty_two_bytes_of_hex_and_nothing_else() {
        let hash = fixture().record_hash.unwrap();
        assert_eq!(record_identity(&hash).unwrap().to_string(), hash);
        assert_eq!(
            record_identity(&format!(" {hash} ")).unwrap().to_string(),
            hash
        );
        assert!(record_identity(&hash[..62]).is_none());
        assert!(record_identity("continue").is_none());
        assert!(record_identity("").is_none());
        // A request names one offered step and the record it read, and can
        // say nothing of what the step does or who takes it.
        let body = |value: Value| serde_json::from_value::<SettleBody>(value);
        let id = kin_model::OperationId::new();
        assert!(body(json!({"command":"abort","record_hash":hash,"operation_id":id})).is_ok());
        for extra in ["actor", "action", "argv", "selector", "expected_record"] {
            let mut request = json!({"command":"abort","record_hash":hash,"operation_id":id});
            request[extra] = json!("caller");
            assert!(body(request).is_err(), "{extra}");
        }
    }

    #[test]
    fn missing_hash_and_terminated_records_never_offer_modifying_handoffs() {
        let mut absent = fixture();
        absent.record = None;
        absent.record_hash = None;
        absent.unresolved_count = 0;
        absent.bodies.clear();
        let value = view(absent).unwrap();
        assert_eq!(value["state"]["status"], "none");
        assert!(value["handoff"]["commands"].as_array().unwrap().is_empty());
        let mut missing = fixture();
        missing.record_hash = None;
        assert!(view(missing).unwrap()["handoff"]["commands"]
            .as_array()
            .unwrap()
            .is_empty());
        let mut terminal = fixture();
        let record = terminal
            .record
            .as_ref()
            .unwrap()
            .terminate(MergeTransactionState::Aborted {
                operation_id: OperationId::new(),
                actor: AuthorId::new("Fixture"),
                aborted_at: Timestamp::now(),
            })
            .unwrap();
        terminal.record_hash = Some(record.hash.to_string());
        terminal.record = Some(record);
        let value = view(terminal).unwrap();
        assert_eq!(value["state"]["status"], "aborted");
        assert_eq!(value["state"]["active"], false);
        assert!(value["handoff"]["commands"].as_array().unwrap().is_empty());
    }

    #[test]
    fn publishing_is_offered_only_on_the_fully_resolved_successor_record() {
        let mut report = fixture();
        let before = report.record_hash.clone().unwrap();
        let record = report.record.as_ref().unwrap();
        let successor = record
            .resolve_entry(
                &record.entries[0].subject,
                MergeEntryResolution::Side {
                    side: MergeSide::Ours,
                    provenance: MergeResolutionProvenance {
                        actor: AuthorId::new("Fixture"),
                        operation_id: OperationId::new(),
                        resolved_at: Timestamp::now(),
                    },
                },
            )
            .unwrap();
        report.record_hash = Some(successor.hash.to_string());
        report.record = Some(successor);
        report.unresolved_count = 0;
        report.resolved_count = 1;
        let offered = offered(&report);
        let ids: Vec<&str> = offered.steps.iter().map(|step| step.id.as_str()).collect();
        assert_eq!(ids, ["continue", "abort"]);
        assert_eq!(offered.steps[0].action, ResolveAction::Continue);
        // Publishing is taken against the record that carries the
        // resolution, which is not the record the conflict was settled on.
        let value = view(report).unwrap();
        assert_eq!(value["state"]["all_resolved"], true);
        assert_ne!(value["provenance"]["record_hash"], before);
        assert!(value["handoff"]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command["id"] == "continue"));
    }

    #[test]
    fn mismatched_identity_hash_counts_or_side_evidence_are_refused() {
        let mut report = fixture();
        report.record_hash = Some("00".repeat(32));
        assert!(view(report).is_err());
        let mut report = fixture();
        report.unresolved_count = 0;
        assert!(view(report).is_err());
        let report = fixture();
        assert!(present(report, "another-repository", "another-workspace").is_err());
        let mut report = fixture();
        report.bodies[0].ours = None;
        assert!(view(report).is_err());
    }

    #[test]
    fn display_truncation_preserves_record_and_unverified_side_semantics() {
        let mut report = fixture();
        let authority = serde_json::to_value(&report.record).unwrap();
        report.bodies[0].base = Some("é".repeat(BODY_LIMIT));
        report.bodies[0].ours = Some(String::new());
        report.bodies[0].theirs = None;
        report.bodies[0].unverified = vec!["theirs".into()];
        let value = view(report).unwrap();
        assert_eq!(value["report"]["record"], authority);
        assert_eq!(value["display"]["truncated"], true);
        assert_eq!(
            value["report"]["bodies"][0]["base"].as_str().unwrap().len(),
            BODY_LIMIT
        );
        assert_eq!(value["report"]["bodies"][0]["ours"], "");
        assert!(value["report"]["bodies"][0].get("theirs").is_none());
        assert_eq!(
            value["report"]["bodies"][0]["unverified"],
            json!(["theirs"])
        );
        assert!(bounded_json(&value, RESPONSE_LIMIT).is_ok());
        assert!(bounded_json(&json!({"large":"x".repeat(500)}), 100).is_err());
    }

    #[test]
    fn selectors_with_trim_or_label_collisions_are_not_handed_off() {
        let report = fixture();
        let mut record = report.record.unwrap();
        let mut first = record.entries.remove(0);
        let mut second = first.clone();
        second.subject = MergeConflictSubject::Entity {
            entity: EntityId::new(),
        };
        second.label = Some(selector(&first.subject));
        assert!(!safe_selector(&first, &[first.clone(), second]));
        first.subject = MergeConflictSubject::Path {
            path: kin_model::RepoPath::from_utf8(" leading-space").unwrap(),
        };
        assert!(!safe_selector(&first, &[first.clone()]));
        first.subject = MergeConflictSubject::Path {
            path: kin_model::RepoPath::from_bytes(vec![b'a', 0xff]).unwrap(),
        };
        assert!(!safe_selector(&first, &[first.clone()]));
    }

    #[test]
    fn an_absent_base_is_not_offered_and_removal_still_is() {
        let mut entry = fixture().record.unwrap().entries.remove(0);
        entry.base = MergeSideValue::Absent;
        entry.divergence = MergeDivergence::AddedBothSides;
        let offered = offered(&fixture_with_entry(entry));
        assert!(!offered
            .steps
            .iter()
            .any(|step| step.id.starts_with("base:")));
        let remove = offered
            .steps
            .iter()
            .find(|step| step.id.starts_with("remove:"))
            .unwrap();
        assert_eq!(directive(remove).choice, ResolveChoice::Remove);
    }

    #[test]
    fn a_contested_place_is_settled_by_naming_its_keeper_and_offers_no_side() {
        let mut entry = fixture().record.unwrap().entries.remove(0);
        let path = "-contested=path.txt";
        entry.subject = MergeConflictSubject::Path {
            path: kin_model::RepoPath::from_utf8(path).unwrap(),
        };
        entry.divergence = MergeDivergence::PathCollision {
            artifacts: vec![kin_model::ArtifactId::new(), kin_model::ArtifactId::new()],
        };
        entry.base = MergeSideValue::Absent;
        entry.ours = MergeSideValue::Absent;
        entry.theirs = MergeSideValue::Absent;
        let offered = offered(&fixture_with_entry(entry));
        let claims: Vec<&Step> = offered
            .steps
            .iter()
            .filter(|step| step.selector.as_deref() == Some(path))
            .collect();
        assert_eq!(claims.len(), 2);
        for claim in claims {
            let directive = directive(claim);
            assert_eq!(directive.selector, path);
            let ResolveChoice::PathOwner { artifact } = &directive.choice else {
                panic!("a contested place is settled by naming who keeps it");
            };
            assert!(uuid::Uuid::parse_str(artifact).is_ok());
            assert_eq!(claim.id, format!("keep-path:{path}:{artifact}"));
        }
        // A contested place has no side to take.
        assert_eq!(offered.steps.len(), 3);
        assert_eq!(offered.steps[2].id, "abort");
    }
}
