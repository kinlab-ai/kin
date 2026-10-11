// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Human inspection of a parked merge and fixed CLI handoffs. No resolution is
//! dispatched here. The durable record stays intact; only source display text
//! may be shortened, with explicit byte counts.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use kin_model::{MergeConflictEntry, MergeConflictSubject, MergeDivergence};
use serde::Serialize;
use serde_json::{json, Value};

use super::conflicts::{ConflictsReport, ConflictsRequest, CONFLICTS_REPORT_SCHEMA};
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
        let health = client.health().await?;
        let repository = health.repo_id.context("The daemon omitted its repository identity.")?;
        let workspace = health.workspace_id.context("The daemon omitted its workspace identity.")?;
        let report = client
            .conflicts(&ConflictsRequest { bodies: true })
            .await?
            .report
            .context("The daemon omitted its conflict report.")?;
        let value = present(report, &repository, &workspace)?;
        bounded_json(&value, RESPONSE_LIMIT)
            .context("Conflict details exceed the UI response limit; inspect them with kin conflicts --json --show.")
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

fn validate_report(report: &ConflictsReport, repository: &str, workspace: &str) -> Result<()> {
    anyhow::ensure!(
        report.schema == CONFLICTS_REPORT_SCHEMA && report.authority == "repository-v6",
        "The daemon returned an unsupported conflict report."
    );
    anyhow::ensure!(
        report.repository_id.as_str() == repository && report.workspace_id.to_string() == workspace,
        "The conflict report belongs to another repository or workspace."
    );
    bounded_json(&report.record, RECORD_LIMIT)
        .context("The merge record exceeds the UI limit; use kin conflicts --json. No authority entries were omitted.")?;
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

fn command(
    id: String,
    label: &str,
    hash: &str,
    selector: Option<&str>,
    args: Vec<String>,
) -> Value {
    let mut argv = vec![
        "kin".to_string(),
        "resolve".to_string(),
        "--expect".to_string(),
        hash.to_string(),
    ];
    argv.extend(args);
    json!({"id":id, "label":label, "argv":argv, "modifies":true, "selector":selector})
}

/// What the page offers for settling the merge in a terminal. It names no
/// directory: the commands run in this repository, and the page says so.
fn handoff(report: &ConflictsReport) -> Value {
    let mut commands = Vec::new();
    let mut unavailable_subjects = Vec::new();
    let unavailable = match (&report.record, report.record_hash.as_deref()) {
        (None, _) => Some("No merge is parked in this workspace."),
        (Some(record), _) if !record.state.is_in_progress() => {
            Some("This merge has already terminated.")
        }
        (Some(_), None) => {
            Some("The daemon omitted the expected record hash. Refresh before resolving.")
        }
        (Some(record), Some(hash)) => {
            for entry in record.unresolved() {
                let selected = selector(&entry.subject);
                if !safe_selector(entry, &record.entries) {
                    unavailable_subjects.push(json!({
                        "selector":selected,
                        "reason":"The existing CLI cannot address this subject unambiguously with its exact identity.",
                    }));
                    continue;
                }
                if let MergeDivergence::PathCollision { artifacts } = &entry.divergence {
                    for artifact in artifacts {
                        commands.push(command(
                            format!("keep-path:{selected}:{}", artifact.0),
                            "Keep this artifact at the contested path",
                            hash,
                            Some(&selected),
                            vec![format!("--keep-path={selected}={}", artifact.0)],
                        ));
                    }
                    continue;
                }
                for (side, label) in [
                    ("ours", "Use the recorded target side"),
                    ("theirs", "Use the recorded incoming side"),
                    ("base", "Use the recorded base side"),
                ] {
                    if side == "base" && !entry.base.is_present() {
                        continue;
                    }
                    commands.push(command(
                        format!("{side}:{selected}"),
                        label,
                        hash,
                        Some(&selected),
                        vec![format!("--{side}"), selected.clone()],
                    ));
                }
                commands.push(command(
                    format!("remove:{selected}"),
                    "Remove this identity from the merge",
                    hash,
                    Some(&selected),
                    vec!["--remove".into(), selected.clone()],
                ));
            }
            if record.is_fully_resolved() {
                commands.push(command(
                    "continue".into(),
                    "Publish the recorded resolutions",
                    hash,
                    None,
                    vec!["--continue".into()],
                ));
            }
            commands.push(command(
                "abort".into(),
                "Abandon the merge; keep current workspace files",
                hash,
                None,
                vec!["--abort".into()],
            ));
            None
        }
    };
    json!({
        "commands":commands, "unavailable_subjects":unavailable_subjects,
        "unavailable":unavailable,
        "instructions":[
            "Run at most one chosen command in this repository's working tree. This page does not execute commands.",
            "argv contains separate arguments, not a shell command. Quote each argument correctly before pasting into a shell.",
            "After every settlement, refresh conflict details and use the new record hash. Do not reuse a previous handoff for continue or abort.",
            "A side choice can remove an identity when that recorded side is absent. Base, target and incoming source are historical display text, not current editing authority.",
            "Custom complete-artifact bytes use the existing --file resolution form with a fresh --expect record hash; a source snippet is not a complete artifact.",
            "Abandoning a merge discards its conflict state and leaves current workspace files untouched. Publishing remains a separate guarded action.",
            "If a command's outcome is uncertain, inspect repository and conflict state before another action; do not retry automatically."
        ],
    })
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

    #[test]
    fn modifying_handoffs_are_fixed_arguments_bound_to_the_exact_record() {
        let report = fixture();
        let hash = report.record_hash.clone().unwrap();
        let selected = selector(&report.record.as_ref().unwrap().entries[0].subject);
        let body_name = report.bodies[0].subject.clone();
        let value = view(report).unwrap();
        assert_eq!(value["state"]["active"], true);
        assert_eq!(value["provenance"]["editable"], false);
        for command in value["handoff"]["commands"].as_array().unwrap() {
            let argv = command["argv"].as_array().unwrap();
            assert_eq!(
                &argv[..4],
                &json!(["kin", "resolve", "--expect", hash])
                    .as_array()
                    .unwrap()[..]
            );
            assert!(!argv
                .iter()
                .any(|arg| arg.as_str() == Some(body_name.as_str())));
            if command["selector"].is_string() {
                assert_eq!(command["selector"], selected);
                assert_eq!(argv.last().unwrap(), &json!(selected));
            }
        }
        assert!(!value["handoff"]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command["id"] == "continue"));
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
    fn continue_uses_the_resolved_successor_hash() {
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
        let value = view(report).unwrap();
        assert_eq!(value["state"]["all_resolved"], true);
        let command = value["handoff"]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|command| command["id"] == "continue")
            .unwrap();
        assert_ne!(command["argv"][3], before);
        assert_eq!(command["argv"][3], value["provenance"]["record_hash"]);
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
    fn absent_base_is_not_offered_and_removal_keeps_its_record_guard() {
        let mut entry = fixture().record.unwrap().entries.remove(0);
        entry.base = MergeSideValue::Absent;
        entry.divergence = MergeDivergence::AddedBothSides;
        let value = view(fixture_with_entry(entry)).unwrap();
        let commands = value["handoff"]["commands"].as_array().unwrap();
        assert!(!commands
            .iter()
            .any(|command| command["argv"][4] == "--base"));
        let remove = commands
            .iter()
            .find(|command| command["argv"][4] == "--remove")
            .unwrap();
        assert_eq!(remove["argv"][2], "--expect");
        assert_eq!(remove["argv"][3], value["provenance"]["record_hash"]);
    }

    #[test]
    fn contested_path_claims_are_single_guarded_arguments_without_side_choices() {
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
        let value = view(fixture_with_entry(entry)).unwrap();
        let commands = value["handoff"]["commands"].as_array().unwrap();
        let claims: Vec<_> = commands
            .iter()
            .filter(|command| command["selector"] == path)
            .collect();
        assert_eq!(claims.len(), 2);
        for claim in claims {
            let argv = claim["argv"].as_array().unwrap();
            assert_eq!(argv.len(), 5);
            assert_eq!(argv[2], "--expect");
            assert_eq!(argv[3], value["provenance"]["record_hash"]);
            let argument = argv[4]
                .as_str()
                .unwrap()
                .strip_prefix("--keep-path=")
                .unwrap();
            let (selected, artifact) = argument.rsplit_once('=').unwrap();
            assert_eq!(selected, path);
            assert!(uuid::Uuid::parse_str(artifact).is_ok());
        }
    }
}
