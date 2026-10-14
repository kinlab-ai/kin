// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Review flags: what a reviewer flagged, held in the repository.
//!
//! A reviewer reading uncommitted work entity by entity flags the ones that
//! are not right, with a note. The flag is an annotation on that entity which
//! carries a [`kin_model::ReviewFlag`]: the note is the annotation's body, the
//! reviewer its author, and the flag records the exact version the reviewer
//! was looking at. Held in the graph, it reaches the agent that made the
//! change the next time an answer from Kin returns the entity, and it records
//! each agent session it reached.
//!
//! A flag stays open until the reviewer closes it. Committing the entity does
//! not close it, and an entity that changes afterwards is told apart by its
//! fingerprint, so the reviewer can look again. A flag is advisory: nothing
//! here refuses a commit.
//!
//! This module is the one place flags are planned and read. A write is
//! planned against the graph ([`plan`], [`plan_sightings`]) as the records it
//! admits ([`FlagWrite`]), so the daemon commits them to repository authority
//! as one transaction before the live graph shows them, and a daemon that
//! reopens the store holds every flag it answered with. The daemon reads
//! [`reached_by`] for every agent call it answers.

use anyhow::Result;
use kin_model::provenance::{Actor, AuditEvent};
use kin_model::{
    Annotation, AnnotationFilter, AnnotationId, AnnotationKind, AnnotationTarget,
    CollaborationDelta, EntityId, Hash256, Keyed, OperationId, ReviewFlag, ReviewFlagResolution,
    ReviewFlagSighting, SemanticAnchor, StalenessState, Timestamp, WorkLink, WorkScope,
};
use kin_model::{EntityStore as _, ProvenanceStore as _, WorkStore as _};
use serde::{Deserialize, Serialize};

/// The longest note a flag carries, in characters.
pub const NOTE_MAX_CHARS: usize = 400;

/// The longest name a flag's author or resolver is recorded under, in
/// characters.
pub const AUTHOR_MAX_CHARS: usize = 200;

/// What setting a flag is recorded as.
pub const SET_ACTION: &str = "review_flag.set";

/// What closing a flag is recorded as.
pub const RESOLVE_ACTION: &str = "review_flag.resolve";

/// One request about review flags, as the daemon takes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ReviewFlagRequest {
    /// Flag `entity` at `source:<digest>` of its reviewed span, or a legacy
    /// `behavior:<digest>` or legacy syntax fingerprint without a prefix.
    /// An entity already flagged has its flag replaced.
    Set {
        entity: EntityId,
        #[serde(default)]
        note: Option<String>,
        fingerprint: String,
        operation_id: OperationId,
        author: String,
    },
    /// Close the open flag on `entity`.
    Resolve {
        entity: EntityId,
        operation_id: OperationId,
        author: String,
    },
    /// Every open flag.
    List,
}

impl ReviewFlagRequest {
    /// Whether running this request can write.
    pub fn mutates(&self) -> bool {
        !matches!(self, Self::List)
    }
}

/// How a request ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFlagOutcome {
    /// The entity carries an open flag as asked.
    Flagged,
    /// The entity's flag is closed.
    Resolved,
    /// There was no open flag on the entity to close.
    NothingOpen,
    /// The open flags are listed.
    Listed,
    /// Nothing was written, and `error` says why.
    Refused,
}

/// One agent session a flag reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFlagSeen {
    /// The name the session's client gave itself.
    pub agent: String,
    pub session: String,
    /// When Kin first handed that session an answer carrying the flag.
    pub at: Timestamp,
}

/// One flag, as a reader is shown it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFlagView {
    /// The flag's own id, stable while it stays open.
    pub flag_id: String,
    pub entity_id: String,
    /// The entity's name now, or the name it was flagged under once the graph
    /// no longer holds it.
    pub name: String,
    /// The entity's kind. Absent once the graph no longer holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The reviewer's note. Empty when the reviewer left none.
    pub note: String,
    pub author: String,
    pub set_at: Timestamp,
    /// Exact entity source identity as `source:<digest>`. Legacy flags retain
    /// their original semantic fingerprint without the prefix.
    pub fingerprint: String,
    /// The entity is no longer the version that was flagged: it changed, or
    /// the graph no longer holds it.
    pub changed_since: bool,
    /// The graph no longer holds the entity.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
    /// An exact-source flag could not be compared with current recorded source.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub source_unavailable: bool,
    /// The agent sessions an answer from Kin handed this flag to, oldest
    /// first.
    pub seen_by: Vec<ReviewFlagSeen>,
}

/// The daemon's answer to a [`ReviewFlagRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFlagResponse {
    pub outcome: ReviewFlagOutcome,
    /// The flag a `set` left open, or the one a `resolve` closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag: Option<ReviewFlagView>,
    /// Every open flag, oldest first. Only a `list` fills it.
    #[serde(default)]
    pub flags: Vec<ReviewFlagView>,
    /// Why nothing was written, in words for the reader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ReviewFlagResponse {
    fn refused(reason: impl Into<String>) -> Self {
        Self {
            outcome: ReviewFlagOutcome::Refused,
            flag: None,
            flags: Vec::new(),
            error: Some(reason.into()),
        }
    }

    fn of(outcome: ReviewFlagOutcome, flag: Option<ReviewFlagView>) -> Self {
        Self {
            outcome,
            flag,
            flags: Vec::new(),
            error: None,
        }
    }
}

/// Every record one flag write admits, as authority holds it afterwards.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FlagWrite {
    /// Each flag the write sets or changes, as its whole new annotation.
    pub annotations: Vec<Annotation>,
    /// The link from a newly set flag to the entity it is on.
    pub work_links: Vec<WorkLink>,
    /// The actor the audit event below names, when the store does not hold
    /// them yet.
    pub actors: Vec<Actor>,
    pub audit_events: Vec<AuditEvent>,
}

impl FlagWrite {
    /// Whether this write records nothing, which is what a request that
    /// changes nothing plans.
    pub fn is_empty(&self) -> bool {
        self.annotations.is_empty()
            && self.work_links.is_empty()
            && self.actors.is_empty()
            && self.audit_events.is_empty()
    }

    /// The collaboration delta that admits these records into repository
    /// authority, in the one order kin-model accepts.
    pub fn to_delta(&self) -> Result<CollaborationDelta> {
        let mut annotations = self.annotations.clone();
        // A key is ordered by its canonical encoding, which for an id is its
        // text. The delta's own validation below is what holds this to it.
        annotations.sort_by_key(|annotation| annotation.annotation_id.0.to_string());
        let delta = CollaborationDelta {
            annotations: annotations
                .into_iter()
                .map(|annotation| Keyed::new(annotation.annotation_id, annotation))
                .collect(),
            work_links: self.work_links.clone(),
            actors: self
                .actors
                .iter()
                .map(|actor| Keyed::new(actor.actor_id, actor.clone()))
                .collect(),
            audit_events: self.audit_events.clone(),
            ..CollaborationDelta::default()
        };
        delta.validate()?;
        Ok(delta)
    }

    /// Make a graph hold these records. Run on the live graph only once
    /// authority holds them.
    pub fn apply_to(&self, graph: &kin_db::InMemoryGraph) -> Result<()> {
        for actor in &self.actors {
            if graph.get_actor(&actor.actor_id)?.is_none() {
                graph.create_actor(actor)?;
            }
        }
        for annotation in &self.annotations {
            graph.create_annotation(annotation)?;
        }
        for link in &self.work_links {
            graph.create_work_link(link)?;
        }
        for event in &self.audit_events {
            graph.record_audit_event(event)?;
        }
        Ok(())
    }
}

/// One request, planned: the records it admits, and what its caller answers
/// once they are held.
#[derive(Debug, Clone)]
pub struct PlannedFlagRequest {
    pub write: FlagWrite,
    pub response: ReviewFlagResponse,
    /// What the write is, for the transaction that carries it.
    pub action: &'static str,
    /// Who the write is by.
    pub actor: String,
}

impl PlannedFlagRequest {
    fn read(response: ReviewFlagResponse) -> Self {
        Self {
            write: FlagWrite::default(),
            response,
            action: "review_flag.read",
            actor: String::new(),
        }
    }
}

/// A request's answer, and whether it wrote.
#[derive(Debug, Clone)]
pub struct ReviewFlagExecution {
    pub response: ReviewFlagResponse,
    pub mutated: bool,
}

/// A note as it is stored: trimmed, or the reason it cannot be.
fn stored_note(note: Option<String>) -> std::result::Result<String, String> {
    let note = note.unwrap_or_default().trim().to_string();
    if note.chars().count() > NOTE_MAX_CHARS {
        return Err(format!("A note is at most {NOTE_MAX_CHARS} characters."));
    }
    Ok(note)
}

/// Who a write is recorded under, or the reason nobody can be named.
fn stored_author(author: &str) -> std::result::Result<String, String> {
    let author = author.trim();
    if author.is_empty() {
        return Err("A flag needs to know who set it, and no name is configured.".to_string());
    }
    Ok(author.chars().take(AUTHOR_MAX_CHARS).collect())
}

/// The name a reader knows an agent by, from the vendor and client name its
/// session registered with: the name the local app shows the same agent under.
pub fn agent_name(vendor: &str, client: &str) -> String {
    super::workspace_agents::agent_display_name(vendor, client)
}

fn kind_label(kind: kin_model::EntityKind) -> Option<String> {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
}

/// One flag as a reader is shown it, read against the graph as it is now.
pub fn view(graph: &kin_db::InMemoryGraph, annotation: &Annotation) -> Option<ReviewFlagView> {
    view_with_source(graph, annotation, |_| None)
}

/// Read exact-source freshness through the caller's persisted span authority.
/// No source bytes become part of the flag answer.
pub fn view_with_source(
    graph: &kin_db::InMemoryGraph,
    annotation: &Annotation,
    source_digest: impl FnOnce(&kin_model::Entity) -> Option<Hash256>,
) -> Option<ReviewFlagView> {
    let flag = annotation.review.as_ref()?;
    let entity_id = annotation.flagged_entity()?;
    let entity = graph.get_entity(&entity_id).ok().flatten();
    let current_source = flag
        .flagged_source
        .and_then(|_| entity.as_ref().and_then(source_digest));
    let source_unavailable =
        flag.flagged_source.is_some() && entity.is_some() && current_source.is_none();
    let changed_since = entity.as_ref().is_none_or(|entity| {
        if let Some(flagged) = flag.flagged_source {
            current_source != Some(flagged)
        } else {
            match flag.flagged_behavior {
                Some(flagged) => entity.fingerprint.behavior_hash != flagged,
                None => entity.fingerprint.ast_hash != flag.flagged_ast,
            }
        }
    });
    Some(ReviewFlagView {
        flag_id: annotation.annotation_id.to_string(),
        entity_id: entity_id.to_string(),
        name: entity
            .as_ref()
            .map(|entity| entity.name.clone())
            .unwrap_or_else(|| flag.flagged_name.clone()),
        kind: entity.as_ref().and_then(|entity| kind_label(entity.kind)),
        note: annotation.body.clone(),
        author: annotation.authored_by.name.clone(),
        set_at: annotation.created_at.clone(),
        fingerprint: flag.flagged_source.map_or_else(
            || {
                flag.flagged_behavior.map_or_else(
                    || flag.flagged_ast.to_string(),
                    |digest| format!("behavior:{digest}"),
                )
            },
            |digest| format!("source:{digest}"),
        ),
        changed_since,
        removed: entity.is_none(),
        source_unavailable,
        seen_by: flag
            .seen_by
            .iter()
            .map(|seen| ReviewFlagSeen {
                agent: seen.agent.clone(),
                session: seen.session.to_string(),
                at: seen.at.clone(),
            })
            .collect(),
    })
}

/// Every open flag, oldest first.
pub fn open_flags(graph: &kin_db::InMemoryGraph) -> Vec<Annotation> {
    let mut open = graph.open_review_flags();
    open.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.annotation_id.0.cmp(&right.annotation_id.0))
    });
    open
}

/// The open flags on the entities `named` holds, oldest first: what an answer
/// that returned those entities reaches.
pub fn reached_by(
    graph: &kin_db::InMemoryGraph,
    named: &std::collections::HashSet<uuid::Uuid>,
) -> Vec<Annotation> {
    if named.is_empty() {
        return Vec::new();
    }
    let mut reached: Vec<Annotation> = graph
        .open_review_flags()
        .into_iter()
        .filter(|annotation| {
            annotation
                .flagged_entity()
                .is_some_and(|entity_id| named.contains(&entity_id.0))
        })
        .collect();
    reached.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.annotation_id.0.cmp(&right.annotation_id.0))
    });
    reached
}

/// The write that records one agent session on each open flag in `flags`
/// that does not record it yet. Empty when there is nothing to record: a
/// session a flag already holds, and a flag closed in the meantime.
pub fn plan_sightings(
    graph: &kin_db::InMemoryGraph,
    sightings: &[(AnnotationId, ReviewFlagSighting)],
) -> Result<FlagWrite> {
    let mut changed: Vec<Annotation> = Vec::new();
    for (annotation_id, sighting) in sightings {
        // A flag two sessions saw is written once, holding both.
        let planned = changed
            .iter()
            .position(|annotation| annotation.annotation_id == *annotation_id);
        let mut annotation = match planned {
            Some(at) => changed.remove(at),
            None => match graph.get_annotation(annotation_id)? {
                Some(annotation) => annotation,
                None => continue,
            },
        };
        let recorded = annotation
            .review
            .as_mut()
            .filter(|flag| flag.resolution.is_none())
            .is_some_and(|flag| flag.record_sighting(sighting.clone()));
        if recorded || planned.is_some() {
            changed.push(annotation);
        }
    }
    Ok(FlagWrite {
        annotations: changed,
        ..FlagWrite::default()
    })
}

/// Add to `write` the record of who did what to a flag, the way `kin note
/// add` records a note.
fn plan_audit(
    graph: &kin_db::InMemoryGraph,
    write: &mut FlagWrite,
    author: &str,
    action: &str,
    annotation: &Annotation,
) -> Result<()> {
    let (actor, event) = crate::provenance::plan_audit_event(
        graph,
        author,
        action,
        annotation.scopes.first().cloned(),
        Some(format!("annotation_id={}", annotation.annotation_id)),
    )?;
    write.actors.extend(actor);
    write.audit_events.push(event);
    Ok(())
}

fn plan_set(
    graph: &kin_db::InMemoryGraph,
    entity_id: EntityId,
    note: Option<String>,
    fingerprint: &str,
    operation_id: OperationId,
    author: &str,
) -> Result<PlannedFlagRequest> {
    let refused = |reason: String| PlannedFlagRequest::read(ReviewFlagResponse::refused(reason));
    if operation_id.0.is_nil() {
        return Ok(refused("A flag needs a nonempty operation ID.".to_string()));
    }
    let note = match stored_note(note) {
        Ok(note) => note,
        Err(reason) => return Ok(refused(reason)),
    };
    let author = match stored_author(author) {
        Ok(author) => author,
        Err(reason) => return Ok(refused(reason)),
    };
    let fingerprint = fingerprint.trim();
    let exact_source = fingerprint.strip_prefix("source:");
    let behavior = fingerprint.strip_prefix("behavior:");
    let Ok(flagged_fingerprint) =
        Hash256::from_hex(exact_source.or(behavior).unwrap_or(fingerprint))
    else {
        return Ok(refused(
            "A flag names the version it was set on by that version's fingerprint, and this is \
             not one."
                .to_string(),
        ));
    };
    let on_entity = graph.list_annotations(&AnnotationFilter {
        kinds: None,
        scopes: Some(vec![WorkScope::Entity(entity_id)]),
        include_stale: true,
    })?;
    // Remember every operation, including one superseded or closed since its
    // answer was lost. Retrying it must not replace a newer reviewer's flag.
    if let Some(recorded) = on_entity.iter().find(|annotation| {
        annotation
            .review
            .as_ref()
            .is_some_and(|flag| flag.operation_id == operation_id)
    }) {
        return Ok(PlannedFlagRequest::read(ReviewFlagResponse::of(
            ReviewFlagOutcome::Flagged,
            view(graph, recorded),
        )));
    }
    let Some(entity) = graph.get_entity(&entity_id)? else {
        return Ok(refused(
            "This entity is not in the repository's graph now, so a flag on it would reach no \
             agent."
                .to_string(),
        ));
    };
    let held = on_entity
        .into_iter()
        .find(|annotation| annotation.open_review_flag().is_some());
    let annotation = Annotation {
        // A replacement is a new record. Keep the old request and its
        // sightings so retries and review history remain truthful.
        annotation_id: AnnotationId::new(),
        kind: AnnotationKind::Warning,
        body: note,
        scopes: vec![WorkScope::Entity(entity_id)],
        // The entity as it is now, which is what recall re-anchors a renamed
        // entity's annotations by. The version the reviewer saw is the
        // flag's own. Captured here, so authority and the live graph hold
        // the same record.
        anchored_fingerprint: Some(SemanticAnchor {
            ast_hash: entity.fingerprint.ast_hash,
            signature_hash: entity.fingerprint.signature_hash,
        }),
        authored_by: crate::provenance::identity_for_label(&author),
        created_at: Timestamp::now(),
        staleness: StalenessState::Fresh,
        review: Some(ReviewFlag {
            flagged_ast: if exact_source.is_none() && behavior.is_none() {
                flagged_fingerprint
            } else {
                entity.fingerprint.ast_hash
            },
            flagged_behavior: behavior.map(|_| flagged_fingerprint),
            flagged_source: exact_source.map(|_| flagged_fingerprint),
            flagged_name: entity.name.clone(),
            operation_id,
            resolution: None,
            // A flag set again says something new, which no agent has seen.
            seen_by: Vec::new(),
        }),
    };
    let mut write = FlagWrite::default();
    write.work_links.push(WorkLink::AttachedTo {
        annotation_id: annotation.annotation_id,
        target: AnnotationTarget::Scope(WorkScope::Entity(entity_id)),
    });
    if let Some(mut held) = held {
        if let Some(flag) = held.review.as_mut() {
            flag.resolution = Some(ReviewFlagResolution {
                by: crate::provenance::identity_for_label(&author),
                at: annotation.created_at.clone(),
                operation_id,
            });
        }
        write.work_links.push(WorkLink::Supersedes {
            new_id: annotation.annotation_id,
            old_id: held.annotation_id,
        });
        write.annotations.push(held);
    }
    plan_audit(graph, &mut write, &author, SET_ACTION, &annotation)?;
    let response = ReviewFlagResponse::of(ReviewFlagOutcome::Flagged, view(graph, &annotation));
    write.annotations.push(annotation);
    Ok(PlannedFlagRequest {
        write,
        response,
        action: SET_ACTION,
        actor: author,
    })
}

fn plan_resolve(
    graph: &kin_db::InMemoryGraph,
    entity_id: EntityId,
    operation_id: OperationId,
    author: &str,
) -> Result<PlannedFlagRequest> {
    let refused = |reason: String| PlannedFlagRequest::read(ReviewFlagResponse::refused(reason));
    if operation_id.0.is_nil() {
        return Ok(refused(
            "Closing a flag needs a nonempty operation ID.".to_string(),
        ));
    }
    let author = match stored_author(author) {
        Ok(author) => author,
        Err(reason) => return Ok(refused(reason)),
    };
    let on_entity: Vec<Annotation> = graph
        .list_annotations(&AnnotationFilter {
            kinds: None,
            scopes: Some(vec![WorkScope::Entity(entity_id)]),
            include_stale: true,
        })?
        .into_iter()
        .filter(|annotation| annotation.flagged_entity() == Some(entity_id))
        .collect();
    // A retry of an earlier close names that historical record, even when
    // another flag is now open on the same entity.
    if let Some(closed) = on_entity.iter().find(|annotation| {
        annotation
            .review
            .as_ref()
            .and_then(|flag| flag.resolution.as_ref())
            .is_some_and(|resolution| resolution.operation_id == operation_id)
    }) {
        return Ok(PlannedFlagRequest::read(ReviewFlagResponse::of(
            ReviewFlagOutcome::Resolved,
            view(graph, closed),
        )));
    }
    let Some(mut open) = on_entity
        .iter()
        .find(|annotation| annotation.open_review_flag().is_some())
        .cloned()
    else {
        return Ok(PlannedFlagRequest::read(ReviewFlagResponse::of(
            ReviewFlagOutcome::NothingOpen,
            None,
        )));
    };
    if let Some(flag) = open.review.as_mut() {
        flag.resolution = Some(ReviewFlagResolution {
            by: crate::provenance::identity_for_label(&author),
            at: Timestamp::now(),
            operation_id,
        });
    }
    let mut write = FlagWrite::default();
    plan_audit(graph, &mut write, &author, RESOLVE_ACTION, &open)?;
    let response = ReviewFlagResponse::of(ReviewFlagOutcome::Resolved, view(graph, &open));
    write.annotations.push(open);
    Ok(PlannedFlagRequest {
        write,
        response,
        action: RESOLVE_ACTION,
        actor: author,
    })
}

/// Plan one request against `graph`, writing nothing.
///
/// A request that cannot be met is answered as refused, in words, and plans
/// no write. An error is a store that could not be read.
pub fn plan(
    graph: &kin_db::InMemoryGraph,
    request: ReviewFlagRequest,
) -> Result<PlannedFlagRequest> {
    match request {
        ReviewFlagRequest::Set {
            entity,
            note,
            fingerprint,
            operation_id,
            author,
        } => plan_set(graph, entity, note, &fingerprint, operation_id, &author),
        ReviewFlagRequest::Resolve {
            entity,
            operation_id,
            author,
        } => plan_resolve(graph, entity, operation_id, &author),
        ReviewFlagRequest::List => Ok(PlannedFlagRequest::read(ReviewFlagResponse {
            outcome: ReviewFlagOutcome::Listed,
            flag: None,
            flags: open_flags(graph)
                .iter()
                .filter_map(|annotation| view(graph, annotation))
                .collect(),
            error: None,
        })),
    }
}

/// Plan one request and apply its write to `graph` itself.
///
/// For a graph that is its own authority, as in a test. The daemon commits a
/// plan to repository authority first and applies it after.
pub fn execute(
    graph: &kin_db::InMemoryGraph,
    request: ReviewFlagRequest,
) -> Result<ReviewFlagExecution> {
    let planned = plan(graph, request)?;
    let mutated = !planned.write.is_empty();
    planned.write.apply_to(graph)?;
    Ok(ReviewFlagExecution {
        response: planned.response,
        mutated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Record one session on some flags, as a graph that is its own authority.
    fn record_sightings(
        graph: &kin_db::InMemoryGraph,
        flags: &[AnnotationId],
        sighting: &ReviewFlagSighting,
    ) -> Result<bool> {
        let sightings: Vec<_> = flags.iter().map(|flag| (*flag, sighting.clone())).collect();
        let write = plan_sightings(graph, &sightings)?;
        write.apply_to(graph)?;
        Ok(!write.is_empty())
    }
    use kin_model::{
        Entity, EntityKind, EntityRole, FilePathId, FingerprintAlgorithm, LanguageId,
        SemanticFingerprint, SessionId, Visibility,
    };

    fn entity(name: &str, ast: u8) -> Entity {
        Entity {
            id: EntityId::new(),
            kind: EntityKind::Method,
            name: name.to_string(),
            language: LanguageId::TypeScript,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([ast; 32]),
                signature_hash: Hash256::from_bytes([0x02; 32]),
                behavior_hash: Hash256::from_bytes([0x03; 32]),
                equivalence_hash: Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(FilePathId::new("src/reply.ts")),
            span: None,
            signature: format!("{name}(payload: Payload): boolean"),
            visibility: Visibility::Public,
            role: EntityRole::Source,
            doc_summary: None,
            metadata: Default::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    fn graph_with(entities: &[&Entity]) -> kin_db::InMemoryGraph {
        let graph = kin_db::InMemoryGraph::default();
        for entity in entities {
            graph.upsert_entity(entity).unwrap();
        }
        graph
    }

    fn flag(entity: &Entity, note: &str) -> ReviewFlagRequest {
        ReviewFlagRequest::Set {
            entity: entity.id,
            note: Some(note.to_string()),
            fingerprint: format!("behavior:{}", entity.fingerprint.behavior_hash),
            operation_id: OperationId::new(),
            author: "Ada Reviewer".to_string(),
        }
    }

    fn listed(graph: &kin_db::InMemoryGraph) -> Vec<ReviewFlagView> {
        execute(graph, ReviewFlagRequest::List)
            .unwrap()
            .response
            .flags
    }

    #[test]
    fn a_flag_is_stored_as_an_annotation_on_the_entity_and_listed_open() {
        let send = entity("ServerReply.send", 0x01);
        let graph = graph_with(&[&send]);
        assert!(listed(&graph).is_empty());

        let set = execute(&graph, flag(&send, "  a second send must fail loudly  ")).unwrap();
        assert!(set.mutated);
        assert_eq!(set.response.outcome, ReviewFlagOutcome::Flagged);
        let shown = set.response.flag.unwrap();
        assert_eq!(shown.entity_id, send.id.to_string());
        assert_eq!(shown.name, "ServerReply.send");
        assert_eq!(shown.kind.as_deref(), Some("method"));
        assert_eq!(shown.note, "a second send must fail loudly");
        assert_eq!(shown.author, "Ada Reviewer");
        assert_eq!(
            shown.fingerprint,
            format!("behavior:{}", send.fingerprint.behavior_hash)
        );
        assert!(!shown.changed_since);
        assert!(!shown.removed);
        assert!(shown.seen_by.is_empty());
        assert_eq!(listed(&graph), vec![shown]);

        // The flag is an annotation in the store `kin note list` reads, scoped
        // to the entity, with an audit record of who set it.
        let annotations = graph
            .get_annotations_for_scope(&WorkScope::Entity(send.id))
            .unwrap();
        assert_eq!(annotations.len(), 1);
        assert_eq!(annotations[0].kind, AnnotationKind::Warning);
        assert_eq!(annotations[0].body, "a second send must fail loudly");
        assert!(annotations[0].anchored_fingerprint.is_some());
        assert!(annotations[0].open_review_flag().is_some());
        let audit = graph.query_audit_events(None, 10).unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].action, "review_flag.set");
    }

    #[test]
    fn the_same_request_sets_a_flag_once_and_a_new_one_replaces_it() {
        let send = entity("ServerReply.send", 0x01);
        let graph = graph_with(&[&send]);
        let first = flag(&send, "first note");
        assert!(execute(&graph, first.clone()).unwrap().mutated);
        let again = execute(&graph, first.clone()).unwrap();
        assert!(!again.mutated);
        assert_eq!(again.response.outcome, ReviewFlagOutcome::Flagged);
        assert_eq!(again.response.flag.unwrap().note, "first note");

        // An agent saw the first flag.
        let held = open_flags(&graph);
        let sighting = ReviewFlagSighting {
            agent: "codex".to_string(),
            session: SessionId::new(),
            at: Timestamp::now(),
        };
        assert!(record_sightings(&graph, &[held[0].annotation_id], &sighting).unwrap());

        // Flagged again with a new note, it is one flag, and nobody has seen
        // what it says now.
        let replaced = execute(&graph, flag(&send, "second note")).unwrap();
        assert!(replaced.mutated);
        let open = listed(&graph);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].note, "second note");
        assert!(open[0].seen_by.is_empty());
        assert_ne!(open_flags(&graph)[0].annotation_id, held[0].annotation_id);
        let retried = execute(&graph, first).unwrap();
        assert!(!retried.mutated);
        assert_eq!(retried.response.flag.unwrap().note, "first note");
        assert_eq!(listed(&graph)[0].note, "second note");
        let previous = graph
            .get_annotation(&held[0].annotation_id)
            .unwrap()
            .unwrap();
        assert!(previous.open_review_flag().is_none());
        assert_eq!(previous.review.unwrap().seen_by.len(), 1);
    }

    #[test]
    fn a_flag_that_cannot_be_set_is_refused_in_words_and_writes_nothing() {
        let send = entity("ServerReply.send", 0x01);
        let graph = graph_with(&[&send]);
        let refuse = |request: ReviewFlagRequest| {
            let done = execute(&graph, request).unwrap();
            assert!(!done.mutated);
            assert_eq!(done.response.outcome, ReviewFlagOutcome::Refused);
            done.response.error.unwrap()
        };
        let ast = send.fingerprint.behavior_hash.to_string();
        let request = |entity: EntityId,
                       note: &str,
                       fingerprint: &str,
                       operation_id: OperationId,
                       author: &str| ReviewFlagRequest::Set {
            entity,
            note: Some(note.to_string()),
            fingerprint: fingerprint.to_string(),
            operation_id,
            author: author.to_string(),
        };
        let unheld = refuse(request(
            EntityId::new(),
            "note",
            &ast,
            OperationId::new(),
            "Ada Reviewer",
        ));
        assert!(unheld.contains("not in the repository's graph"), "{unheld}");
        let long = refuse(request(
            send.id,
            &"x".repeat(NOTE_MAX_CHARS + 1),
            &ast,
            OperationId::new(),
            "Ada Reviewer",
        ));
        assert!(long.contains("at most 400 characters"), "{long}");
        let shapeless = refuse(request(
            send.id,
            "note",
            "abc",
            OperationId::new(),
            "Ada Reviewer",
        ));
        assert!(shapeless.contains("fingerprint"), "{shapeless}");
        let unkeyed = refuse(request(
            send.id,
            "note",
            &ast,
            OperationId::from_uuid(uuid::Uuid::nil()),
            "Ada Reviewer",
        ));
        assert!(unkeyed.contains("operation ID"), "{unkeyed}");
        let unsigned = refuse(request(send.id, "note", &ast, OperationId::new(), "  "));
        assert!(unsigned.contains("who set it"), "{unsigned}");
        // No refusal speaks of a file or a command.
        for said in [unheld, long, shapeless, unkeyed, unsigned] {
            assert!(!said.contains("file") && !said.contains("kin "), "{said}");
        }
        assert!(listed(&graph).is_empty());
        assert!(graph
            .list_annotations(&AnnotationFilter {
                include_stale: true,
                ..Default::default()
            })
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_flag_follows_its_entity_until_the_reviewer_closes_it() {
        let send = entity("ServerReply.send", 0x01);
        let graph = graph_with(&[&send]);
        execute(&graph, flag(&send, "note")).unwrap();

        // The entity is written again unchanged, as a commit leaves it: the
        // flag is still open and says nothing changed.
        let mut committed = send.clone();
        committed.created_in = None;
        graph.upsert_entity(&committed).unwrap();
        let open = listed(&graph);
        assert_eq!(open.len(), 1);
        assert!(!open[0].changed_since);

        // The entity changes: the flag is still open, on the version that was
        // flagged, and says the entity moved on.
        let mut edited = send.clone();
        edited.fingerprint.behavior_hash = Hash256::from_bytes([0x09; 32]);
        graph.upsert_entity(&edited).unwrap();
        let open = listed(&graph);
        assert_eq!(open.len(), 1);
        assert!(open[0].changed_since);
        assert!(!open[0].removed);
        assert_eq!(
            open[0].fingerprint,
            format!("behavior:{}", send.fingerprint.behavior_hash)
        );

        // The reviewer closes it. Closing is one write, and the same request
        // sent again answers the same without writing.
        let resolve = ReviewFlagRequest::Resolve {
            entity: send.id,
            operation_id: OperationId::new(),
            author: "Ada Reviewer".to_string(),
        };
        let closed = execute(&graph, resolve.clone()).unwrap();
        assert!(closed.mutated);
        assert_eq!(closed.response.outcome, ReviewFlagOutcome::Resolved);
        assert!(listed(&graph).is_empty());
        let again = execute(&graph, resolve.clone()).unwrap();
        assert!(!again.mutated);
        assert_eq!(again.response.outcome, ReviewFlagOutcome::Resolved);
        // A different request finds nothing open.
        let nothing = execute(
            &graph,
            ReviewFlagRequest::Resolve {
                entity: send.id,
                operation_id: OperationId::new(),
                author: "Ada Reviewer".to_string(),
            },
        )
        .unwrap();
        assert!(!nothing.mutated);
        assert_eq!(nothing.response.outcome, ReviewFlagOutcome::NothingOpen);
        // The closed flag is kept as a record of who closed it.
        let kept = graph
            .get_annotations_for_scope(&WorkScope::Entity(send.id))
            .unwrap();
        assert_eq!(kept.len(), 1);
        let resolution = kept[0].review.as_ref().unwrap().resolution.as_ref();
        assert_eq!(resolution.unwrap().by.name, "Ada Reviewer");

        // Flagged again after that, it is a new flag.
        execute(&graph, flag(&edited, "still wrong")).unwrap();
        assert_eq!(listed(&graph).len(), 1);
        let retry_close = execute(&graph, resolve).unwrap();
        assert!(!retry_close.mutated);
        assert_eq!(retry_close.response.outcome, ReviewFlagOutcome::Resolved);
        assert_eq!(listed(&graph)[0].note, "still wrong");
        assert_eq!(
            graph
                .get_annotations_for_scope(&WorkScope::Entity(send.id))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn exact_source_flags_notice_comment_edits_and_report_missing_source() {
        let send = entity("ServerReply.send", 0x01);
        let graph = graph_with(&[&send]);
        let before = Hash256::from_bytes(kin_blobs::digest_bytes(b"send() { return true; }"));
        let after = Hash256::from_bytes(kin_blobs::digest_bytes(
            b"send() { /* explained */ return true; }",
        ));
        let mut request = flag(&send, "explain this");
        if let ReviewFlagRequest::Set { fingerprint, .. } = &mut request {
            *fingerprint = format!("source:{before}");
        }
        execute(&graph, request).unwrap();
        let held = open_flags(&graph).pop().unwrap();
        let original = view_with_source(&graph, &held, |_| Some(before)).unwrap();
        assert!(!original.changed_since);
        assert!(!original.source_unavailable);
        assert_eq!(original.fingerprint, format!("source:{before}"));
        // The semantic hashes are unchanged; source freshness still changes.
        let changed = view_with_source(&graph, &held, |_| Some(after)).unwrap();
        assert!(changed.changed_since);
        assert!(!changed.source_unavailable);
        let missing = view(&graph, &held).unwrap();
        assert!(missing.changed_since);
        assert!(missing.source_unavailable);
    }

    #[test]
    fn a_flag_on_an_entity_the_graph_no_longer_holds_keeps_its_name() {
        let send = entity("ServerReply.send", 0x01);
        let graph = graph_with(&[&send]);
        execute(&graph, flag(&send, "note")).unwrap();
        graph.remove_entity(&send.id).unwrap();
        let open = listed(&graph);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].name, "ServerReply.send");
        assert_eq!(open[0].kind, None);
        assert!(open[0].changed_since);
        assert!(open[0].removed);
    }

    #[test]
    fn an_answer_reaches_the_flags_on_the_entities_it_returned_and_records_who_saw_them() {
        let send = entity("ServerReply.send", 0x01);
        let not_found = entity("notFound", 0x04);
        let quiet = entity("serialize", 0x05);
        let graph = graph_with(&[&send, &not_found, &quiet]);
        execute(&graph, flag(&send, "first")).unwrap();
        execute(&graph, flag(&not_found, "second")).unwrap();

        let named = |entities: &[&Entity]| -> std::collections::HashSet<uuid::Uuid> {
            entities.iter().map(|entity| entity.id.0).collect()
        };
        assert!(reached_by(&graph, &named(&[])).is_empty());
        assert!(reached_by(&graph, &named(&[&quiet])).is_empty());
        let reached = reached_by(&graph, &named(&[&quiet, &send]));
        assert_eq!(reached.len(), 1);
        assert_eq!(reached[0].flagged_entity(), Some(send.id));
        let both = reached_by(&graph, &named(&[&not_found, &send]));
        assert_eq!(
            both.iter()
                .map(|annotation| annotation.body.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"],
            "oldest first"
        );

        let session = SessionId::new();
        let sighting = ReviewFlagSighting {
            agent: "codex".to_string(),
            session,
            at: Timestamp::now(),
        };
        let ids: Vec<AnnotationId> = reached
            .iter()
            .map(|annotation| annotation.annotation_id)
            .collect();
        assert!(record_sightings(&graph, &ids, &sighting).unwrap());
        assert!(
            !record_sightings(&graph, &ids, &sighting).unwrap(),
            "a session is recorded once"
        );
        let open = listed(&graph);
        let seen = &open
            .iter()
            .find(|flag| flag.name == "ServerReply.send")
            .unwrap()
            .seen_by;
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].agent, "codex");
        assert_eq!(seen[0].session, session.to_string());
        assert!(open
            .iter()
            .find(|flag| flag.name == "notFound")
            .unwrap()
            .seen_by
            .is_empty());

        // A flag closed in the meantime records nothing.
        execute(
            &graph,
            ReviewFlagRequest::Resolve {
                entity: send.id,
                operation_id: OperationId::new(),
                author: "Ada Reviewer".to_string(),
            },
        )
        .unwrap();
        let later = ReviewFlagSighting {
            agent: "claude".to_string(),
            session: SessionId::new(),
            at: Timestamp::now(),
        };
        assert!(!record_sightings(&graph, &ids, &later).unwrap());
        assert!(reached_by(&graph, &named(&[&send])).is_empty());
    }

    /// The wire shapes the daemon route and the local app's routes carry.
    #[test]
    fn requests_and_answers_keep_their_wire_shape() {
        let entity_id = EntityId::new();
        let operation_id = OperationId::new();
        let request: ReviewFlagRequest = serde_json::from_value(serde_json::json!({
            "op": "set", "entity": entity_id, "fingerprint": "ab".repeat(32),
            "operation_id": operation_id, "author": "ada",
        }))
        .unwrap();
        assert!(request.mutates());
        assert!(matches!(request, ReviewFlagRequest::Set { note: None, .. }));
        let list: ReviewFlagRequest =
            serde_json::from_value(serde_json::json!({"op": "list"})).unwrap();
        assert!(!list.mutates());

        let send = entity("ServerReply.send", 0x01);
        let graph = graph_with(&[&send]);
        let set = execute(&graph, flag(&send, "note")).unwrap().response;
        let wire = serde_json::to_value(&set).unwrap();
        assert_eq!(wire["outcome"], "flagged");
        let shown = wire["flag"].as_object().unwrap();
        assert_eq!(
            shown.keys().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "author",
                "changed_since",
                "entity_id",
                "fingerprint",
                "flag_id",
                "kind",
                "name",
                "note",
                "seen_by",
                "set_at",
            ]
        );
        assert!(wire.get("error").is_none());
    }
}
