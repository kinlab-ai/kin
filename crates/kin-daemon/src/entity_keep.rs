// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Keep chosen entities of the uncommitted working copy in a change, and put
//! one entity back as it was.
//!
//! Both are entity work on one working copy, and both start from the same
//! question: which bytes carry exactly the chosen entities' changes and none
//! of the others. [`crate::entity_split`] builds them. This module derives
//! their entities the way every admitted source is derived, proves the result
//! entity by entity, and publishes it through the repository paths the rest of
//! the daemon publishes through.
//!
//! A commit of chosen entities publishes a change whose tree is the head's
//! with only the chosen entities' changes in it, and moves the workspace's base
//! onto that change without moving its tree. Nothing in the working copy is
//! written, so every change that was not chosen is still uncommitted work
//! against the new base, byte for byte as it was.
//!
//! An agent's transaction commits through the same shape when the working
//! copy holds uncommitted work in source units its operations did not write:
//! [`with_units_from`] puts those units back as the head holds them in the
//! transaction's plan, so the change records the head and what the transaction
//! wrote, and gives the working copy the daemon's own entities for them, so
//! that work stays uncommitted with the identities, and so the authorship, it
//! had.
//!
//! A restore splices one entity's version at a change into the working copy as
//! an uncommitted edit, through the same exact workspace transition a branch
//! switch or a checkout writes with, and refuses when the entity changed since
//! the caller read it.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use axum::http::StatusCode;
use kin_cli::commands::entity_work::{
    EntityCommitRequest, EntityCommitResponse, EntityRestoreRequest, EntityRestoreResponse,
    ENTITY_REFUSED,
};
use kin_db::{LocalFileBackend, RepositoryAuthorityManager};
use kin_model::graph::ResolvedGraphState;
use kin_model::{
    ArtifactId, ChangeStore, Entity, EntityDelta, EntityId, EntityKind, EntityStore,
    ExternalReference, ExternalReferenceId, FilePathId, GraphNodeId, LocatedEntry, OperationId,
    Relation, RelationDelta, RelationId, RepoPath, RepositoryCommitReceipt, RepositoryTransaction,
    ResolutionRecord, ResolutionRecordId, ResolvedArtifact, ResolvedTree, SemanticChange,
    SemanticChangeId, TransactionDelta, TreeDelta, TreeEntry, WorkspaceExpectation,
    WorkspaceMutation, WorkspaceState, REPOSITORY_TRANSACTION_SCHEMA_VERSION,
};

use crate::entity_split::{Outside, Spanned, Version};
use crate::local_repository_authority::{
    ActiveLocalRepositoryAuthority, LocalRepositoryAuthorityContext, TargetGraph,
};
use crate::state::{DaemonEvent, DaemonState};

type Authority = RepositoryAuthorityManager<LocalFileBackend>;

/// The error kind of an answer that does not say whether the work landed.
const UNSETTLED: &str = "entity_unsettled";

/// Why entity work wrote nothing, or why its outcome is not known.
#[derive(Debug)]
pub(crate) struct Refusal {
    status: StatusCode,
    kind: &'static str,
    message: String,
}

impl Refusal {
    /// Nothing was written, for a reason one sentence about entities states.
    fn worded(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            kind: ENTITY_REFUSED,
            message: message.into(),
        }
    }

    /// Nothing was written, for a reason that is about storage rather than
    /// about entities. The reason is logged and the caller is told what did
    /// not happen.
    fn failed(what: &str, work: Work, error: impl std::fmt::Display) -> Self {
        tracing::warn!(%error, what, "entity work refused before it published anything");
        Self::worded(format!("Kin could not {what}, so {}.", work.nothing()))
    }

    /// The work may have been published, and nothing here can say.
    fn unsettled(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            kind: UNSETTLED,
            message: format!("the entity work did not finish: {error}"),
        }
    }

    pub(crate) fn into_http(self) -> (StatusCode, String) {
        (
            self.status,
            serde_json::json!({"error": self.kind, "message": self.message}).to_string(),
        )
    }
}

/// Which entity work is being done, for the words its refusals use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Work {
    Commit,
    Restore,
}

impl Work {
    fn verb(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Restore => "restore",
        }
    }

    fn nothing(self) -> &'static str {
        match self {
            Self::Commit => "nothing was committed",
            Self::Restore => "nothing was restored",
        }
    }
}

/// One version of the repository as entity work reads it: its tree and its
/// entities.
struct Held<'a> {
    tree: &'a ResolvedTree,
    entities: Entities<'a>,
}

enum Entities<'a> {
    Recorded(&'a HashMap<EntityId, Entity>),
    Live(&'a kin_db::InMemoryGraph),
}

impl Entities<'_> {
    fn get(&self, id: &EntityId) -> Option<Entity> {
        match self {
            Self::Recorded(entities) => entities.get(id).cloned(),
            Self::Live(graph) => graph.get_entity(id).ok().flatten(),
        }
    }

    /// The entities with a span in the content at `path`.
    fn in_path(&self, path: &str) -> Vec<Entity> {
        let mut found = match self {
            Self::Recorded(entities) => entities
                .values()
                .filter(|entity| origin_is(entity, path))
                .cloned()
                .collect::<Vec<_>>(),
            Self::Live(graph) => graph
                .query_entities(&kin_model::EntityFilter {
                    file_path: Some(FilePathId::new(path)),
                    ..Default::default()
                })
                .unwrap_or_default(),
        };
        found.retain(|entity| entity.span.is_some() && origin_is(entity, path));
        found
    }
}

fn origin_is(entity: &Entity, path: &str) -> bool {
    entity
        .file_origin
        .as_ref()
        .is_some_and(|origin| origin.0 == path)
}

fn is_whole_unit(kind: EntityKind) -> bool {
    matches!(
        kind,
        EntityKind::Module | EntityKind::File | EntityKind::Package
    )
}

/// One content as one version holds it.
#[derive(Debug, Clone, Default)]
struct Content {
    artifact: Option<ResolvedArtifact>,
    bytes: Vec<u8>,
    entities: Vec<Entity>,
}

impl Content {
    fn spanned(&self) -> Vec<Spanned> {
        self.entities
            .iter()
            .filter_map(|entity| {
                let span = entity.span.as_ref()?;
                Some(Spanned {
                    id: entity.id,
                    name: entity.name.clone(),
                    kind: entity.kind,
                    span: span.start_byte..span.end_byte,
                })
            })
            .collect()
    }
}

/// What the chosen ids name.
#[derive(Debug, Default)]
struct Chosen {
    /// Entities Kin parses, by id.
    code: HashSet<EntityId>,
    /// The contents those entities live in, in either version.
    paths: BTreeSet<RepoPath>,
    /// Unparsed entities, which are kept or restored whole.
    unparsed: Vec<ArtifactId>,
    /// A name for each chosen id, for the sentences refusals say.
    names: HashMap<EntityId, String>,
}

impl Chosen {
    fn name(&self, id: &EntityId) -> String {
        self.names
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }
}

/// What one content becomes.
#[derive(Debug)]
struct Transition {
    path: RepoPath,
    from: Content,
    to: Content,
    /// The artifact, entry and bytes the content takes, or `None` when it
    /// leaves the tree.
    after: Option<(ArtifactId, TreeEntry, Vec<u8>)>,
    /// Parsed declarations that take an identity the version they are read
    /// from does not hold yet, by kind and name.
    retain: HashMap<(EntityKind, String), EntityId>,
}

impl Transition {
    fn changes_anything(&self) -> bool {
        match (&self.from.artifact, &self.after) {
            (None, None) => false,
            (Some(before), Some((artifact, entry, _))) => {
                before.artifact_id != *artifact || before.entry != *entry
            }
            _ => true,
        }
    }

    fn after_bytes(&self) -> &[u8] {
        self.after
            .as_ref()
            .map(|(_, _, bytes)| bytes.as_slice())
            .unwrap_or_default()
    }
}

fn parse_entity(raw: &str) -> Result<EntityId, Refusal> {
    uuid::Uuid::parse_str(raw.trim())
        .map(EntityId)
        .map_err(|_| Refusal::worded(format!("{} does not name an entity.", raw.trim())))
}

/// The name an unparsed entity goes by: its own name, without where it is
/// projected.
fn unparsed_name(artifact: &ResolvedArtifact) -> String {
    artifact
        .path
        .as_utf8()
        .and_then(|path| path.rsplit('/').next())
        .unwrap_or("this unparsed entity")
        .to_string()
}

/// Read what the chosen ids name in the two versions.
fn classify(work: Work, raw: &[String], from: &Held<'_>, to: &Held<'_>) -> Result<Chosen, Refusal> {
    let mut chosen = Chosen::default();
    for raw in raw {
        let id = parse_entity(raw)?;
        if kin_mcp::artifact_entity::parts::is_part_id(&id.0) {
            return Err(Refusal::worded(format!(
                "Kin cannot {} one section of a text on its own; choose the whole text.",
                work.verb()
            )));
        }
        let held = [from.entities.get(&id), to.entities.get(&id)];
        let origins = held
            .iter()
            .flatten()
            .filter(|entity| entity.span.is_some())
            .filter_map(|entity| entity.file_origin.as_ref())
            .filter_map(|origin| RepoPath::from_utf8(origin.0.clone()).ok())
            .collect::<Vec<_>>();
        if let Some(entity) = held.iter().flatten().next() {
            if is_whole_unit(entity.kind) {
                return Err(Refusal::worded(format!(
                    "{} stands for its whole content, so Kin {}s the entities in it one at a time.",
                    entity.name,
                    work.verb()
                )));
            }
            if !origins.is_empty() {
                chosen.names.insert(id, entity.name.clone());
                chosen.code.insert(id);
                chosen.paths.extend(origins);
                continue;
            }
        }
        let artifact = ArtifactId(id.0);
        let (before, after) = (from.tree.get(&artifact), to.tree.get(&artifact));
        let Some(named) = before.or(after) else {
            return Err(Refusal::worded(match work {
                Work::Commit => format!("No uncommitted work names entity {}.", raw.trim()),
                Work::Restore => format!(
                    "Neither the working copy nor that change holds entity {}.",
                    raw.trim()
                ),
            }));
        };
        let name = unparsed_name(named);
        if before == after {
            return Err(Refusal::worded(match work {
                Work::Commit => format!(
                    "{name} has no uncommitted change, so there is nothing of it to commit."
                ),
                Work::Restore => {
                    format!("{name} already matches that change, so there is nothing to restore.")
                }
            }));
        }
        let parsed = [before, after].into_iter().flatten().any(|artifact| {
            artifact.path.as_utf8().is_some_and(|path| {
                !from.entities.in_path(path).is_empty() || !to.entities.in_path(path).is_empty()
            })
        });
        if parsed {
            return Err(Refusal::worded(format!(
                "{name} is held as code, so Kin {}s the entities in it one at a time.",
                work.verb()
            )));
        }
        chosen.names.insert(id, name);
        chosen.unparsed.push(artifact);
    }
    if chosen.code.is_empty() && chosen.unparsed.is_empty() {
        return Err(Refusal::worded(format!(
            "Choose at least one entity to {}.",
            work.verb()
        )));
    }
    Ok(chosen)
}

/// One content as `held` has it at `path`.
fn content(
    state: &DaemonState,
    authority: &Authority,
    work: Work,
    held: &Held<'_>,
    path: &RepoPath,
    names: &str,
) -> Result<Content, Refusal> {
    let Some(artifact) = held.tree.artifact_at_path(path).cloned() else {
        return Ok(Content {
            entities: path
                .as_utf8()
                .map(|path| held.entities.in_path(path))
                .unwrap_or_default(),
            ..Content::default()
        });
    };
    let TreeEntry::Blob { hash, .. } = artifact.entry else {
        return Err(Refusal::worded(format!(
            "{names} is not held as text Kin can separate, so {}.",
            work.nothing()
        )));
    };
    let bytes = crate::source_cas::read_publishable_source(state.blobs.as_ref(), authority, hash)
        .map_err(|error| Refusal::failed("read the recorded content", work, error))?
        .body()
        .to_vec();
    let entities = path
        .as_utf8()
        .map(|path| held.entities.in_path(path))
        .unwrap_or_default();
    Ok(Content {
        artifact: Some(artifact),
        bytes,
        entities,
    })
}

/// Separate the chosen entities' changes in every content they live in.
fn separate(
    state: &DaemonState,
    authority: &Authority,
    work: Work,
    chosen: &Chosen,
    from: &Held<'_>,
    to: &Held<'_>,
    outside: Outside,
) -> Result<(Vec<Transition>, BTreeSet<EntityId>), Refusal> {
    let names = chosen
        .code
        .iter()
        .map(|id| chosen.name(id))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");
    let mut transitions = Vec::new();
    let mut changed = BTreeSet::new();
    for path in &chosen.paths {
        let from_content = content(state, authority, work, from, path, &names)?;
        let to_content = content(state, authority, work, to, path, &names)?;
        // A content whose identity moved, or that was replaced by another,
        // changed as a whole, and its entities have no place to be kept apart
        // from that move.
        let moved = |artifact: &Option<ResolvedArtifact>, other: &Held<'_>| {
            artifact.as_ref().is_some_and(|artifact| {
                other
                    .tree
                    .get(&artifact.artifact_id)
                    .is_some_and(|there| there.path != *path)
            })
        };
        let replaced = matches!(
            (&from_content.artifact, &to_content.artifact),
            (Some(before), Some(after)) if before.artifact_id != after.artifact_id
        );
        if replaced || moved(&from_content.artifact, to) || moved(&to_content.artifact, from) {
            return Err(Refusal::worded(format!(
                "{names} sits in content that was moved or replaced as a whole, so Kin cannot {} it \
                 apart from that content.",
                work.verb()
            )));
        }
        let (from_spans, to_spans) = (from_content.spanned(), to_content.spanned());
        let selected = crate::entity_split::select(
            Version {
                bytes: &from_content.bytes,
                entities: &from_spans,
            },
            Version {
                bytes: &to_content.bytes,
                entities: &to_spans,
            },
            &chosen.code,
            outside,
        )
        .map_err(|entangled| {
            Refusal::worded(format!(
                "{} and {} were changed in the same place, so Kin cannot {} one without the other.",
                entangled.chosen,
                entangled.other,
                work.verb()
            ))
        })?;
        changed.extend(selected.changed.iter().copied());
        // Repository authority admits content no commit holds yet only as the
        // workspace holds it, so a change cannot introduce part of it. Text
        // being restored into a working copy is no change and has no such
        // bound.
        if work == Work::Commit
            && from_content.artifact.is_none()
            && selected.bytes != to_content.bytes
            && selected.bytes != from_content.bytes
        {
            let named = |picked: bool| {
                selected
                    .changed
                    .iter()
                    .find(|id| chosen.code.contains(id) == picked)
                    .and_then(|id| to_content.entities.iter().find(|entity| entity.id == *id))
                    .map(|entity| entity.name.clone())
            };
            return Err(Refusal::worded(match (named(true), named(false)) {
                (Some(kept), Some(left)) => format!(
                    "{kept} and {left} are in content no commit holds yet, which Kin commits \
                     whole, so it cannot commit one without the other."
                ),
                _ => format!(
                    "{names} is in content no commit holds yet, which Kin commits whole, so {}.",
                    work.nothing()
                ),
            }));
        }
        let after = if selected.bytes == to_content.bytes {
            to_content
                .artifact
                .as_ref()
                .map(|artifact| (artifact.artifact_id, artifact.entry, selected.bytes.clone()))
        } else if selected.bytes == from_content.bytes {
            from_content
                .artifact
                .as_ref()
                .map(|artifact| (artifact.artifact_id, artifact.entry, selected.bytes.clone()))
        } else {
            let carrier = from_content
                .artifact
                .as_ref()
                .or(to_content.artifact.as_ref())
                .expect("a content a chosen entity lives in is held by one version");
            let executable = match carrier.entry {
                TreeEntry::Blob { executable, .. } => executable,
                _ => false,
            };
            let hash = kin_blobs::digest(&selected.bytes);
            Some((
                carrier.artifact_id,
                TreeEntry::blob(hash, executable),
                selected.bytes,
            ))
        };
        // A chosen declaration the starting version does not hold yet keeps
        // the identity the other version gave it, so the work left behind
        // reads it as the same entity and not as one removed and one added.
        let mut retain = HashMap::new();
        let mut seen: HashMap<(EntityKind, String), usize> = HashMap::new();
        for entity in &to_content.entities {
            *seen.entry((entity.kind, entity.name.clone())).or_default() += 1;
        }
        for entity in &to_content.entities {
            let key = (entity.kind, entity.name.clone());
            if chosen.code.contains(&entity.id)
                && from.entities.get(&entity.id).is_none()
                && seen.get(&key) == Some(&1)
            {
                retain.insert(key, entity.id);
            }
        }
        transitions.push(Transition {
            path: path.clone(),
            from: from_content,
            to: to_content,
            after,
            retain,
        });
    }
    Ok((transitions, changed))
}

/// The graph a version of the repository holds, built from the daemon's live
/// graph so it carries every index and record a derivation reads.
fn graph_at(
    state: &DaemonState,
    target: &ResolvedGraphState,
) -> Result<kin_db::InMemoryGraph, String> {
    graph_toward(state.graph.to_snapshot(), target)
}

/// `current` moved to hold exactly `target`'s entities, relations and tree,
/// with the external symbols and resolution records those relations name, and
/// every other index and record `current` carries.
pub(crate) fn graph_toward(
    current: kin_db::GraphSnapshot,
    target: &ResolvedGraphState,
) -> Result<kin_db::InMemoryGraph, String> {
    let semantic = kin_core::diff_workspace_semantics(
        &current.entities,
        &current.relations,
        &target.entities,
        &target.relations,
    )
    .map_err(|error| error.to_string())?;
    let (references, records) = kin_core::desired_resolution_nodes(
        &target.external_references,
        &target.resolution_records,
        &current.external_references,
        &current.resolution_records,
        target.relations.values(),
    );
    let semantic = kin_core::with_resolution_node_transition(
        semantic,
        &current.external_references,
        &current.resolution_records,
        &references,
        &records,
    )
    .map_err(|error| error.to_string())?;
    let mut delta = semantic.transaction_delta();
    delta.tree_deltas = kin_core::exact_tree_correction(&current.resolved_tree, &target.tree)
        .map_err(|error| error.to_string())?;
    let graph = kin_db::InMemoryGraph::from_snapshot_without_text_index(current)
        .map_err(|error| error.to_string())?;
    graph
        .apply_transaction_delta(&delta)
        .map_err(|error| error.to_string())?;
    Ok(graph)
}

/// One version of the repository's semantics, read as plain maps: a change's
/// resolved state or a graph's snapshot.
#[derive(Clone, Copy)]
pub(crate) struct Semantics<'a> {
    entities: &'a HashMap<EntityId, Entity>,
    relations: &'a HashMap<RelationId, Relation>,
    tree: &'a ResolvedTree,
    external_references: &'a HashMap<ExternalReferenceId, ExternalReference>,
    resolution_records: &'a HashMap<ResolutionRecordId, ResolutionRecord>,
}

impl<'a> Semantics<'a> {
    pub(crate) fn of_state(state: &'a ResolvedGraphState) -> Self {
        Self {
            entities: &state.entities,
            relations: &state.relations,
            tree: &state.tree,
            external_references: &state.external_references,
            resolution_records: &state.resolution_records,
        }
    }

    pub(crate) fn of_snapshot(snapshot: &'a kin_db::GraphSnapshot) -> Self {
        Self {
            entities: &snapshot.entities,
            relations: &snapshot.relations,
            tree: &snapshot.resolved_tree,
            external_references: &snapshot.external_references,
            resolution_records: &snapshot.resolution_records,
        }
    }
}

/// `current` with the source units at `units` exactly as `source` holds them.
///
/// This is how a change leaves uncommitted work out of itself without a second
/// planner: the transaction's own plan is `current`, and the units it did not
/// write are put back as the base change holds them, so the change records
/// only what the transaction wrote. The same move with the daemon's live graph
/// as `source` gives the working copy the transaction leaves behind, whose
/// uncommitted units keep the entities, and so the identities and authorship,
/// the daemon already serves for them.
///
/// A unit moves whole: its tree entry, every entity whose origin is in it, and
/// every relationship that starts or ends at one of those. A relationship the
/// transaction itself changed (`changed` answers that, from the value
/// `current` holds) keeps `current`'s value, since its own operations are what
/// set it; any other takes `source`'s. A relationship left naming an entity or
/// content the result does not hold is dropped, unless it is one the
/// transaction asked for by name in `required`, which refuses instead, because
/// that relationship would name uncommitted work.
///
/// An entity that moved between a unit in `units` and one outside them has a
/// place on both sides of the line, so nothing can put one side back without
/// the other, and the move is refused rather than split.
pub(crate) fn with_units_from(
    current: kin_db::GraphSnapshot,
    source: Semantics<'_>,
    units: &BTreeSet<RepoPath>,
    changed: &dyn Fn(&RelationId, Option<&Relation>) -> bool,
    required: &BTreeSet<RelationId>,
) -> Result<kin_db::InMemoryGraph, String> {
    let unit_paths = units
        .iter()
        .filter_map(|path| path.as_utf8())
        .collect::<HashSet<_>>();
    let in_units = |entity: &Entity| {
        entity
            .file_origin
            .as_ref()
            .is_some_and(|origin| unit_paths.contains(origin.0.as_str()))
    };

    // The tree, with the units' entries as `source` holds them.
    let taken = units
        .iter()
        .filter_map(|path| current.resolved_tree.artifact_id_at_path(path))
        .collect::<HashSet<_>>();
    let given = units
        .iter()
        .filter_map(|path| source.tree.artifact_at_path(path))
        .collect::<Vec<_>>();
    let mut artifacts = current
        .resolved_tree
        .artifacts()
        .filter(|artifact| !taken.contains(&artifact.artifact_id))
        .cloned()
        .collect::<Vec<_>>();
    let standing = artifacts
        .iter()
        .map(|artifact| artifact.artifact_id)
        .collect::<HashSet<_>>();
    for artifact in &given {
        if standing.contains(&artifact.artifact_id) {
            return Err(format!(
                "{} moved between work this change records and uncommitted work it leaves in \
                 place",
                unparsed_name(artifact)
            ));
        }
        artifacts.push((*artifact).clone());
    }
    let tree = ResolvedTree::from_artifacts(artifacts).map_err(|error| error.to_string())?;
    let unit_artifacts = taken
        .iter()
        .copied()
        .chain(given.iter().map(|artifact| artifact.artifact_id))
        .collect::<HashSet<_>>();

    // Every entity in the units, as `source` holds it.
    let scoped = current
        .entities
        .values()
        .chain(source.entities.values())
        .filter(|entity| in_units(entity))
        .map(|entity| entity.id)
        .collect::<BTreeSet<_>>();
    let mut entities = current.entities.clone();
    for id in &scoped {
        let (here, there) = (current.entities.get(id), source.entities.get(id));
        if let Some(outside) = here
            .into_iter()
            .chain(there)
            .find(|entity| entity.file_origin.is_some() && !in_units(entity))
        {
            return Err(format!(
                "{} moved between work this change records and uncommitted work it leaves in \
                 place",
                outside.name
            ));
        }
        match there {
            Some(entity) => entities.insert(*id, entity.clone()),
            None => entities.remove(id),
        };
    }

    // Every relationship that starts or ends in the units.
    let touches = |node: &GraphNodeId| match node {
        GraphNodeId::Entity(id) => scoped.contains(id),
        GraphNodeId::Artifact(id) => unit_artifacts.contains(id),
        _ => false,
    };
    let candidates = current
        .relations
        .values()
        .chain(source.relations.values())
        .filter(|relation| touches(&relation.src) || touches(&relation.dst))
        .map(|relation| relation.id)
        .collect::<BTreeSet<_>>();
    let mut relations = current.relations.clone();
    let mut external_references = current.external_references.clone();
    let mut resolution_records = current.resolution_records.clone();
    for id in candidates {
        let here = current.relations.get(&id);
        let value = if changed(&id, here) {
            here
        } else {
            source.relations.get(&id)
        };
        let Some(relation) = value else {
            relations.remove(&id);
            continue;
        };
        let held = |node: &GraphNodeId| match node {
            GraphNodeId::Entity(entity) => entities.contains_key(entity),
            GraphNodeId::Artifact(artifact) => tree.get(artifact).is_some(),
            GraphNodeId::ExternalReference(reference) => {
                external_references.contains_key(reference)
                    || source.external_references.contains_key(reference)
            }
            _ => true,
        };
        if !(held(&relation.src) && held(&relation.dst)) {
            if required.contains(&id) {
                let name = |node: &GraphNodeId| match node {
                    GraphNodeId::Entity(entity) => current
                        .entities
                        .get(entity)
                        .or_else(|| source.entities.get(entity))
                        .map(|entity| entity.name.clone())
                        .unwrap_or_else(|| entity.to_string()),
                    other => format!("{other:?}"),
                };
                return Err(format!(
                    "the relationship from {} to {} names an entity that exists only in \
                     uncommitted work this change leaves in place",
                    name(&relation.src),
                    name(&relation.dst)
                ));
            }
            relations.remove(&id);
            continue;
        }
        // The external symbols and resolution records it names come with it.
        for node in [relation.src, relation.dst] {
            if let GraphNodeId::ExternalReference(reference) = node {
                if !external_references.contains_key(&reference) {
                    if let Some(found) = source.external_references.get(&reference) {
                        external_references.insert(reference, found.clone());
                    }
                }
            }
        }
        for record in relation
            .evidence
            .iter()
            .filter_map(|evidence| evidence.token.as_deref())
            .filter_map(ResolutionRecordId::from_context_token)
        {
            if !resolution_records.contains_key(&record) {
                if let Some(found) = source.resolution_records.get(&record) {
                    resolution_records.insert(record, found.clone());
                }
            }
        }
        relations.insert(id, relation.clone());
    }

    let target = ResolvedGraphState {
        entities,
        relations,
        tree,
        external_references,
        resolution_records,
        ..ResolvedGraphState::default()
    };
    graph_toward(current, &target)
}

/// Why installing the new contents failed.
enum InstallError {
    /// A content no longer parses as its language.
    Unparseable,
    Other(String),
}

impl From<String> for InstallError {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

/// Install every transition into `prospective` and derive its entities again
/// from the exact new bytes, the way every admitted source is derived.
fn install(
    state: &DaemonState,
    authority: &Authority,
    prospective: &kin_db::InMemoryGraph,
    transitions: &[Transition],
    unparsed: &[(Option<ResolvedArtifact>, Option<ResolvedArtifact>)],
) -> Result<(), InstallError> {
    for transition in transitions {
        if let (Some(before), None) = (&transition.from.artifact, &transition.after) {
            retire(prospective, before)?;
        }
    }
    let unparsed_deltas = unparsed
        .iter()
        .filter_map(|(before, after)| match (before, after) {
            (Some(before), Some(after)) if before != after => Some(TreeDelta::Updated {
                artifact_id: before.artifact_id,
                old: before.located_entry(),
                new: after.located_entry(),
            }),
            (Some(before), None) => Some(TreeDelta::Removed {
                artifact_id: before.artifact_id,
                old: before.located_entry(),
            }),
            (None, Some(after)) => Some(TreeDelta::Added {
                artifact_id: after.artifact_id,
                new: after.located_entry(),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !unparsed_deltas.is_empty() {
        prospective
            .apply_transaction_delta(&TransactionDelta {
                tree_deltas: unparsed_deltas,
                ..TransactionDelta::default()
            })
            .map_err(|error| format!("install unparsed content: {error}"))?;
    }

    let mut reconciler = kin_reconcile::Reconciler::new(PathBuf::new());
    reconciler.seed_cross_file_linker_from_graph(prospective);
    for transition in transitions {
        let Some((artifact, entry, bytes)) = &transition.after else {
            continue;
        };
        if !transition.changes_anything() {
            continue;
        }
        let file_id = FilePathId::new(transition.path.as_utf8().unwrap_or_default());
        let digest = state
            .blobs
            .write(bytes)
            .map_err(|error| format!("stage new content: {error}"))?;
        if entry.blob_identity() != Some(digest) {
            return Err(InstallError::Other(
                "staged content does not match the entry it was planned for".into(),
            ));
        }
        let new = LocatedEntry::new(transition.path.clone(), *entry);
        let tree_delta = match &transition.from.artifact {
            Some(before) => TreeDelta::Updated {
                artifact_id: before.artifact_id,
                old: before.located_entry(),
                new,
            },
            None => TreeDelta::Added {
                artifact_id: *artifact,
                new,
            },
        };
        prospective
            .apply_transaction_delta(&TransactionDelta {
                tree_deltas: vec![tree_delta],
                ..TransactionDelta::default()
            })
            .map_err(|error| format!("install new content: {error}"))?;
        let manifests =
            crate::source_cas::graph_manifests(prospective, state.blobs.as_ref(), Some(authority));
        let indexed = kin_index::shared_index::index_any(&file_id, bytes, digest, &manifests)
            .map_err(|error| format!("parse new content: {error}"))?;
        let kin_index::IndexedAny::EntitySource(mut indexed) = indexed else {
            let held = prospective
                .query_entities(&kin_model::EntityFilter {
                    file_path: Some(file_id.clone()),
                    ..Default::default()
                })
                .map_err(|error| format!("read entities: {error}"))?;
            if !held.is_empty() {
                crate::loop_runner::clear_incompatible_facets_in(
                    prospective,
                    &file_id,
                    crate::loop_runner::EnrichmentFacet::None,
                )
                .map_err(|error| format!("retire entities: {error}"))?;
            }
            continue;
        };
        if !matches!(indexed.parse_state, kin_model::ParseState::Valid) {
            return Err(InstallError::Unparseable);
        }
        let parsed = indexed
            .entities
            .iter()
            .map(|entity| (entity.id, entity.kind, entity.name.clone()))
            .collect::<Vec<_>>();
        for (parsed, kind, name) in parsed {
            if let Some(retained) = transition.retain.get(&(kind, name)) {
                if *retained != parsed {
                    indexed
                        .retain_entity_identity(parsed, *retained)
                        .map_err(|error| format!("keep an entity's identity: {error}"))?;
                }
            }
        }
        let reconcile = reconciler
            .reconcile_indexed_content(&indexed, state.blobs.as_ref(), prospective)
            .map_err(|error| match error {
                kin_reconcile::ReconcileError::IncompleteParseWouldDelete { .. }
                | kin_reconcile::ReconcileError::BrokenAstRejected { .. } => {
                    InstallError::Unparseable
                }
                other => InstallError::Other(format!("derive entities: {other}")),
            })?;
        prospective
            .apply_transaction_delta(&reconcile.delta)
            .map_err(|error| format!("apply derived entities: {error}"))?;
        if let Some(layout) = reconciler.projection().get_layout(&file_id).cloned() {
            prospective
                .upsert_file_layout(&layout)
                .map_err(|error| format!("install layout: {error}"))?;
        }
    }
    Ok(())
}

/// Take one content out of `prospective` with its entities and the edges that
/// address it.
fn retire(prospective: &kin_db::InMemoryGraph, before: &ResolvedArtifact) -> Result<(), String> {
    let file_id = FilePathId::new(before.path.as_utf8().unwrap_or_default());
    crate::loop_runner::clear_incompatible_facets_in(
        prospective,
        &file_id,
        crate::loop_runner::EnrichmentFacet::None,
    )
    .map_err(|error| format!("retire entities: {error}"))?;
    let node = GraphNodeId::Artifact(before.artifact_id);
    let relation_deltas = prospective
        .get_all_relations_for_node(&node)
        .map_err(|error| format!("read edges: {error}"))?
        .into_iter()
        .map(|old| RelationDelta::Removed { old })
        .collect();
    prospective
        .apply_transaction_delta(&TransactionDelta {
            relation_deltas,
            tree_deltas: vec![TreeDelta::Removed {
                artifact_id: before.artifact_id,
                old: before.located_entry(),
            }],
            ..TransactionDelta::default()
        })
        .map_err(|error| format!("retire content: {error}"))
}

/// Prove one content's derived entities: every chosen entity reads exactly as
/// the version it was taken from, and every other entity exactly as the
/// version it was left at.
///
/// An entity that holds others whose choice differs from its own, such as a
/// class one of whose methods is chosen, is held to its identity, kind and
/// name rather than its whole text, because its text is by construction part
/// one version's and part the other's. Its own changes are proved by the
/// separation, which never splits an entity's own text.
fn prove(
    prospective: &kin_db::InMemoryGraph,
    transition: &Transition,
    chosen: &HashSet<EntityId>,
) -> Result<(), String> {
    let path = transition.path.as_utf8().unwrap_or_default();
    let after = Entities::Live(prospective)
        .in_path(path)
        .into_iter()
        .map(|entity| (entity.id, entity))
        .collect::<HashMap<_, _>>();
    let from = transition
        .from
        .entities
        .iter()
        .map(|entity| (entity.id, entity))
        .collect::<HashMap<_, _>>();
    let to = transition
        .to
        .entities
        .iter()
        .map(|entity| (entity.id, entity))
        .collect::<HashMap<_, _>>();
    let body = |bytes: &[u8], entity: &Entity| -> Option<Vec<u8>> {
        let span = entity.span.as_ref()?;
        bytes
            .get(span.start_byte..span.end_byte)
            .map(<[u8]>::to_vec)
    };
    let versions: [Vec<&Entity>; 3] = [
        from.values().copied().collect(),
        to.values().copied().collect(),
        after.values().collect(),
    ];
    let holds_differently = |id: &EntityId| {
        let picked = chosen.contains(id);
        versions.iter().any(|version| {
            let Some(outer) = version
                .iter()
                .find(|entity| entity.id == *id)
                .and_then(|entity| entity.span.as_ref())
            else {
                return false;
            };
            version.iter().any(|inner| {
                inner.id != *id
                    && !is_whole_unit(inner.kind)
                    && chosen.contains(&inner.id) != picked
                    && inner.span.as_ref().is_some_and(|span| {
                        outer.start_byte <= span.start_byte && span.end_byte <= outer.end_byte
                    })
            })
        })
    };
    let ids = from
        .keys()
        .chain(to.keys())
        .chain(after.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    for id in ids {
        let kind = from
            .get(&id)
            .map(|entity| entity.kind)
            .or_else(|| to.get(&id).map(|entity| entity.kind))
            .or_else(|| after.get(&id).map(|entity| entity.kind));
        if kind.is_some_and(is_whole_unit) {
            continue;
        }
        let (expected, bytes) = if chosen.contains(&id) {
            (to.get(&id), transition.to.bytes.as_slice())
        } else {
            (from.get(&id), transition.from.bytes.as_slice())
        };
        match (expected, after.get(&id)) {
            (None, None) => {}
            (Some(expected), None) => {
                return Err(format!("{} was lost", expected.name));
            }
            (None, Some(got)) => {
                return Err(format!("{} appeared", got.name));
            }
            (Some(expected), Some(got)) => {
                if expected.kind != got.kind || expected.name != got.name {
                    return Err(format!("{} changed identity", expected.name));
                }
                if holds_differently(&id) {
                    continue;
                }
                if body(bytes, expected) != body(transition.after_bytes(), got) {
                    return Err(format!("{} does not read as it should", expected.name));
                }
            }
        }
    }
    Ok(())
}

/// Refuse a planned change that carries anything outside the chosen work.
fn confined(
    change: &SemanticChange,
    transitions: &[Transition],
    unparsed: &[ArtifactId],
    prospective: &kin_db::InMemoryGraph,
) -> Result<(), String> {
    let paths = transitions
        .iter()
        .filter_map(|transition| transition.path.as_utf8().map(str::to_string))
        .collect::<HashSet<_>>();
    let mut artifacts = unparsed.iter().copied().collect::<HashSet<_>>();
    let mut entities = HashSet::new();
    for transition in transitions {
        for content in [&transition.from, &transition.to] {
            artifacts.extend(
                content
                    .artifact
                    .as_ref()
                    .map(|artifact| artifact.artifact_id),
            );
            entities.extend(content.entities.iter().map(|entity| entity.id));
        }
        if let Some((artifact, _, _)) = &transition.after {
            artifacts.insert(*artifact);
        }
        entities.extend(
            Entities::Live(prospective)
                .in_path(transition.path.as_utf8().unwrap_or_default())
                .into_iter()
                .map(|entity| entity.id),
        );
    }
    let local = |entity: &Entity| {
        entities.contains(&entity.id)
            || entity
                .file_origin
                .as_ref()
                .is_some_and(|origin| paths.contains(&origin.0))
    };
    for delta in &change.entity_deltas {
        let ok = match delta {
            EntityDelta::Added { new } => local(new),
            EntityDelta::Modified { old, new } => local(old) && local(new),
            EntityDelta::Removed { old } => local(old),
        };
        if !ok {
            return Err(format!(
                "the change would also carry entity {}",
                delta.target_id()
            ));
        }
    }
    for delta in &change.tree_deltas {
        if !artifacts.contains(&delta.artifact_id()) {
            return Err("the change would also carry other content".into());
        }
    }
    let node_local = |node: &GraphNodeId| match node {
        GraphNodeId::Entity(id) => entities.contains(id),
        GraphNodeId::Artifact(id) => artifacts.contains(id),
        _ => false,
    };
    for delta in &change.relation_deltas {
        let ok = match delta {
            RelationDelta::Added { new } => node_local(&new.src) || node_local(&new.dst),
            RelationDelta::Modified { old, new } => {
                node_local(&old.src)
                    || node_local(&old.dst)
                    || node_local(&new.src)
                    || node_local(&new.dst)
            }
            RelationDelta::Removed { old } => node_local(&old.src) || node_local(&old.dst),
        };
        if !ok {
            return Err("the change would also carry a relation outside the chosen work".into());
        }
    }
    Ok(())
}

type Lease = kin_db::AuthorityReadLease<kin_db::RepositoryAuthorityState>;

/// This working copy's workspace as one read of repository authority holds it.
fn workspace_in(lease: &Lease, workspace_id: kin_model::WorkspaceId) -> Option<WorkspaceState> {
    lease
        .metadata()
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == workspace_id)
        .cloned()
}

/// The receipt one read of repository authority holds for an operation, when
/// that operation landed.
fn receipt_in(lease: &Lease, operation_id: OperationId) -> Option<RepositoryCommitReceipt> {
    kin_core::rejoined_receipt(lease.metadata(), operation_id)
}

/// The change an operation already published, read from authority.
fn landed(
    authority: &Authority,
    operation_id: OperationId,
) -> Option<(SemanticChange, Option<String>)> {
    let lease = authority.read_authority();
    let receipt = receipt_in(&lease, operation_id)?;
    let published = kin_core::published_change(&receipt.operation)?;
    let change = lease.snapshot().changes.get(&published.change_id)?.clone();
    Some((change, published.branch.map(|branch| branch.to_string())))
}

fn commit_response(
    change: &SemanticChange,
    branch: Option<String>,
    request: &EntityCommitRequest,
    replayed: bool,
) -> EntityCommitResponse {
    EntityCommitResponse {
        change_id: change.id.to_string(),
        branch,
        entity_count: change.entity_deltas.len(),
        relation_count: change.relation_deltas.len(),
        file_count: change.tree_deltas.len(),
        kept: request
            .entities
            .iter()
            .map(|id| id.trim().to_string())
            .collect(),
        replayed,
    }
}

/// Commit the chosen entities' uncommitted changes and leave every other
/// change uncommitted.
///
/// The caller holds the coordination gate and has just admitted the working
/// copy, so the daemon's graph is the working copy this plans against.
pub(crate) fn commit_entities(
    state: &Arc<DaemonState>,
    request: &EntityCommitRequest,
) -> Result<EntityCommitResponse, Refusal> {
    let work = Work::Commit;
    let message = request.message.trim();
    if message.is_empty() {
        return Err(Refusal::worded("A commit needs a message."));
    }
    let context = LocalRepositoryAuthorityContext::from_state(state)
        .map_err(|error| Refusal::failed("open this repository", work, error))?;
    let held = crate::api::held_repository_authority(state)
        .map_err(|(_, error)| Refusal::failed("open this repository", work, error))?;
    if let Some((change, branch)) = landed(&held, request.operation_id) {
        return Ok(commit_response(&change, branch, request, true));
    }

    let lease = held.read_authority();
    let roots = lease.roots().clone();
    let workspace = workspace_in(&lease, context.workspace_id())
        .ok_or_else(|| Refusal::failed("find this working copy", work, "no local workspace"))?;
    let live_tree = state.graph.resolved_tree();
    if live_tree != workspace.tree {
        return Err(Refusal::worded(
            "Kin is still reading the latest edits, so nothing was committed; try again in a \
             moment.",
        ));
    }
    let head = workspace
        .base_target
        .as_ref()
        .map(|target| lease.resolve_target_change_id(target))
        .transpose()
        .map_err(|error| Refusal::failed("read the last commit", work, error))?;
    let head_state = match head {
        Some(head) => crate::commit_deltas::resolve_authority_baseline(lease.snapshot(), &head)
            .map_err(|error| Refusal::failed("read the last commit", work, error))?
            .into_owned(),
        None => ResolvedGraphState::default(),
    };
    drop(lease);

    let from = Held {
        tree: &head_state.tree,
        entities: Entities::Recorded(&head_state.entities),
    };
    let to = Held {
        tree: &live_tree,
        entities: Entities::Live(state.graph.as_ref()),
    };
    let chosen = classify(work, &request.entities, &from, &to)?;
    let (transitions, changed) = separate(
        state,
        &held,
        work,
        &chosen,
        &from,
        &to,
        Outside::WhenEveryEntityIsChosen,
    )?;
    if let Some(unchanged) = chosen.code.iter().find(|id| !changed.contains(id)) {
        return Err(Refusal::worded(format!(
            "{} has no uncommitted change of its own, so there is nothing of it to commit.",
            chosen.name(unchanged)
        )));
    }
    let unparsed = chosen
        .unparsed
        .iter()
        .map(|artifact| {
            (
                head_state.tree.get(artifact).cloned(),
                live_tree.get(artifact).cloned(),
            )
        })
        .collect::<Vec<_>>();
    let names = chosen
        .names
        .values()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");

    let prospective = graph_at(state, &head_state)
        .map_err(|error| Refusal::failed("read the last commit", work, error))?;
    match install(state, &held, &prospective, &transitions, &unparsed) {
        Ok(()) => {}
        Err(InstallError::Unparseable) => {
            return Err(Refusal::worded(format!(
                "Committing {names} without the rest of the uncommitted work would leave code that \
                 does not parse, so nothing was committed."
            )))
        }
        Err(InstallError::Other(error)) => {
            return Err(Refusal::failed(
                &format!("separate {names} from the rest of the uncommitted work"),
                work,
                error,
            ))
        }
    }
    for transition in &transitions {
        prove(&prospective, transition, &chosen.code).map_err(|error| {
            Refusal::failed(
                &format!("separate {names} from the rest of the uncommitted work exactly"),
                work,
                error,
            )
        })?;
    }

    let plan = crate::repository_commit::plan_kept_workspace_commit(
        &prospective,
        state.graph.as_ref(),
        state.blobs.as_ref(),
        &context,
        Arc::clone(&held),
        &roots,
        request.operation_id,
        request.timestamp.clone(),
        request.author.clone(),
        message.to_string(),
    )
    .map_err(|error| Refusal::failed(&format!("record {names}"), work, error))?;
    confined(&plan.change, &transitions, &chosen.unparsed, &prospective).map_err(|error| {
        Refusal::failed(
            &format!("separate {names} from the rest of the uncommitted work exactly"),
            work,
            error,
        )
    })?;
    drop(prospective);
    if let Some(blocked) = crate::api::foreign_hard_lease_refusal(
        state,
        &plan.entity_scope_ids,
        request.session_id.as_deref(),
    )
    .map_err(|(_, error)| Refusal::failed("check who else is working", work, error))?
    {
        tracing::info!(blocked = %blocked.1, "an entity commit met a hard lease");
        return Err(Refusal::worded(
            "Another session holds a lease on one of these entities, so nothing was committed.",
        ));
    }

    let committed = match crate::repository_commit::commit_native_plan_with_projection(
        &state.layout,
        state.blobs.as_ref(),
        &context,
        plan,
    ) {
        Ok(committed) => committed,
        Err(error) => {
            match crate::repository_commit::recover_native_commit(&context, request.operation_id) {
                Ok(Some(recovered)) => recovered,
                Ok(None) => {
                    return Err(Refusal::failed(
                        &format!("record {names} over the working copy as it now is"),
                        work,
                        error,
                    ))
                }
                Err(recovery) => {
                    return Err(Refusal::unsettled(format!(
                        "{error}; reading repository authority afterwards also failed: {recovery}"
                    )))
                }
            }
        }
    };
    after_commit(
        state,
        &committed.change,
        committed.receipt.generation,
        committed.resolved_publication_base,
        &chosen.code,
    )
    .map_err(Refusal::unsettled)?;
    Ok(commit_response(
        &committed.change,
        committed.target.branch().map(|name| name.to_string()),
        request,
        false,
    ))
}

/// What every native commit does once authority holds its change.
fn after_commit(
    state: &Arc<DaemonState>,
    change: &SemanticChange,
    generation: u64,
    base: Option<SemanticChangeId>,
    chosen: &HashSet<EntityId>,
) -> Result<(), String> {
    crate::client_edits::seal_kept_commit(state, change, base, chosen);
    crate::api::relabel_held_authority_after_own_commit(state);
    state
        .record_repository_authority_commit(generation)
        .map_err(|error| error.to_string())?;
    // The working copy did not move, so the live graph keeps every entity it
    // holds and only learns the new change.
    crate::mcp_commit::timed_commit_phase("install_live_graph", || {
        state.graph.create_change(change)
    })
    .map_err(|error| error.to_string())?;
    crate::binding_history::restore_exact_authority(state);
    state.record_durable_entity_count(state.graph.entity_count() as u64);
    state.record_durable_relation_count(state.graph.relation_count() as u64);
    crate::background_work::record_relation_census(
        state,
        state.graph.as_ref(),
        kin_core::relation_census::CensusSource::Commit,
    );
    state.bump_version();
    state.emit_event(DaemonEvent::GraphRootChanged {
        old_root_hash: None,
        new_root_hash: change.id.to_string(),
    });
    Ok(())
}

/// Parse a change id the caller named.
fn parse_change(raw: &str) -> Result<SemanticChangeId, Refusal> {
    let raw = raw.trim();
    kin_model::Hash256::from_hex(raw)
        .ok()
        .filter(|hash| hash.to_string() == raw)
        .map(SemanticChangeId::from_hash)
        .ok_or_else(|| Refusal::worded(format!("{raw} does not name a recorded change.")))
}

/// How an entity the working copy holds now stands against the reading a
/// restore names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// The entity is exactly as it was read.
    Current,
    /// The entity changed after it was read.
    Changed,
    /// The reading names the entity by a semantic fingerprint, which a
    /// comment or spacing edit does not move, so it cannot show that the
    /// source a restore would discard is the source that was read.
    Inexact,
}

impl Reading {
    /// The answer of an exact check: the entity is as read, or it changed.
    fn exact(same: bool) -> Self {
        if same {
            Self::Current
        } else {
            Self::Changed
        }
    }
}

/// How `expected` names the entity as the working copy holds it now.
///
/// Only an exact span check can guard a restore, because a restore discards
/// the entity's whole source, comments and spacing included. The check is on
/// the entity's own span, so an edit to a sibling does not make a read stale,
/// while a later comment or formatting edit to this entity does. A semantic
/// fingerprint that still matches is reported as [`Reading::Inexact`] rather
/// than accepted, since a comment written after the read would match it too.
fn matches_expected(
    state: &DaemonState,
    authority: &Authority,
    held: &Held<'_>,
    id: &EntityId,
    expected: &str,
) -> Result<Reading, Refusal> {
    let entity = held.entities.get(id);
    if expected == "absent" {
        return Ok(Reading::exact(entity.is_none()));
    }
    let Some(entity) = entity else {
        return Ok(Reading::Changed);
    };
    let Some(expected_source) = expected.strip_prefix("source:") else {
        let fingerprint = &entity.fingerprint;
        let semantic = [
            fingerprint.behavior_hash,
            fingerprint.ast_hash,
            fingerprint.signature_hash,
        ];
        return Ok(
            if semantic.iter().any(|hash| hash.to_string() == expected) {
                Reading::Inexact
            } else {
                Reading::Changed
            },
        );
    };
    let Some(span) = entity.span.as_ref() else {
        return Ok(Reading::Changed);
    };
    let Some(path) = entity
        .file_origin
        .as_ref()
        .and_then(|origin| RepoPath::from_utf8(origin.0.clone()).ok())
    else {
        return Ok(Reading::Changed);
    };
    let source = content(state, authority, Work::Restore, held, &path, &entity.name)?;
    Ok(Reading::exact(
        source
            .bytes
            .get(span.start_byte..span.end_byte)
            .is_some_and(|body| kin_blobs::digest(body).to_string() == expected_source),
    ))
}

/// Put one entity back as it was at the head, or at an earlier change, as an
/// uncommitted edit.
///
/// The caller holds the coordination gate and has just admitted the working
/// copy.
pub(crate) fn restore_entity(
    state: &DaemonState,
    request: &EntityRestoreRequest,
) -> Result<EntityRestoreResponse, Refusal> {
    let work = Work::Restore;
    let graph_mutation = state.begin_graph_authority_mutation();
    let persistence = state
        .persist_lock
        .lock()
        .map_err(|_| Refusal::unsettled("the daemon's persistence lock is poisoned"))?;
    let previous_graph_root = hex::encode(state.graph.compute_root_hash());
    let authority = ActiveLocalRepositoryAuthority::open_bound(state)
        .map_err(|refusal| Refusal::failed("open this repository", work, refusal.into_error()))?;
    let lease = authority.manager.read_authority();
    let roots = lease.roots().clone();
    let workspace = workspace_in(&lease, authority.workspace_id)
        .ok_or_else(|| Refusal::failed("find this working copy", work, "no local workspace"))?;
    if let Some(receipt) = receipt_in(&lease, request.operation_id) {
        // An implicit restore used the base the receipt's workspace mutation
        // held then, even when another commit has advanced the base since.
        // Explicit restore targets are not retained in the operation record,
        // so a replay must not claim its new request's target as recorded fact.
        let restored_to = if request.to.is_none() {
            receipt
                .operation
                .workspace_mutation
                .as_ref()
                .filter(|mutation| mutation.workspace_id == authority.workspace_id)
                .and_then(|mutation| mutation.new_base_target.as_ref())
                .and_then(|target| lease.resolve_target_change_id(target).ok())
                .map(|target| target.to_string())
        } else {
            None
        };
        return Ok(EntityRestoreResponse {
            entity: request.entity.trim().to_string(),
            name: None,
            restored_to,
            replayed: true,
        });
    }
    let target = match request.to.as_deref() {
        Some(raw) => parse_change(raw)?,
        None => workspace
            .base_target
            .as_ref()
            .map(|target| lease.resolve_target_change_id(target))
            .transpose()
            .map_err(|error| Refusal::failed("read the last commit", work, error))?
            .ok_or_else(|| {
                Refusal::worded("There is no commit to restore it from, so nothing was restored.")
            })?,
    };
    let workspace_graph = lease
        .workspace_graph_snapshot(&workspace.workspace_id)
        .map_err(|error| Refusal::failed("read the working copy", work, error))?
        .ok_or_else(|| Refusal::failed("read the working copy", work, "no workspace graph"))?;
    crate::local_repository_authority::require_fresh_daemon_workspace(
        state,
        &roots,
        &workspace_graph,
        "restoring an entity",
    )
    .map_err(|error| {
        tracing::info!(%error, "an entity restore met a daemon behind its authority");
        Refusal::worded(
            "Kin is still reading the latest edits, so nothing was restored; try again in a moment.",
        )
    })?;
    if lease.snapshot().changes.get(&target).is_none() {
        return Err(Refusal::worded(format!(
            "{target} does not name a recorded change."
        )));
    }
    let target_state = crate::commit_deltas::resolve_authority_baseline(lease.snapshot(), &target)
        .map_err(|error| Refusal::failed("read that change", work, error))?
        .into_owned();
    drop(lease);

    let live_tree = state.graph.resolved_tree();
    let from = Held {
        tree: &live_tree,
        entities: Entities::Live(state.graph.as_ref()),
    };
    let to = Held {
        tree: &target_state.tree,
        entities: Entities::Recorded(&target_state.entities),
    };
    let chosen = classify(work, std::slice::from_ref(&request.entity), &from, &to)?;
    let id = parse_entity(&request.entity)?;
    let name = chosen.name(&id);
    if let Some(expected) = request.expected.as_deref() {
        // An unparsed entity is read whole, so its content address is exact.
        let reading = match chosen.unparsed.first() {
            Some(artifact) if expected == "absent" => {
                Reading::exact(live_tree.get(artifact).is_none())
            }
            Some(artifact) => Reading::exact(
                live_tree
                    .get(artifact)
                    .and_then(|artifact| artifact.entry.blob_identity())
                    .is_some_and(|hash| {
                        hash.to_string() == expected.strip_prefix("source:").unwrap_or(expected)
                    }),
            ),
            None => matches_expected(state, &authority.manager, &from, &id, expected)?,
        };
        match reading {
            Reading::Current => {}
            Reading::Changed => {
                return Err(Refusal::worded(format!(
                    "{name} changed since it was read, so nothing was restored."
                )))
            }
            Reading::Inexact => {
                return Err(Refusal::worded(format!(
                    "Kin cannot tell whether {name} changed since it was read, because that \
                     fingerprint stays the same when a comment or spacing changes, so nothing \
                     was restored. Guard the restore with the exact source a review reports for \
                     {name}, source:<body_hash>."
                )))
            }
        }
    }
    let (transitions, changed) = separate(
        state,
        &authority.manager,
        work,
        &chosen,
        &from,
        &to,
        Outside::Never,
    )?;
    if chosen.unparsed.is_empty() && !changed.contains(&id) {
        return Err(Refusal::worded(format!(
            "{name} already matches that change, so there is nothing to restore."
        )));
    }
    let unparsed = chosen
        .unparsed
        .iter()
        .map(|artifact| {
            (
                live_tree.get(artifact).cloned(),
                target_state.tree.get(artifact).cloned(),
            )
        })
        .collect::<Vec<_>>();

    let prospective =
        kin_db::InMemoryGraph::from_snapshot_without_text_index(state.graph.to_snapshot())
            .map_err(|error| Refusal::failed("read the working copy", work, error))?;
    match install(
        state,
        &authority.manager,
        &prospective,
        &transitions,
        &unparsed,
    ) {
        Ok(()) => {}
        Err(InstallError::Unparseable) => {
            return Err(Refusal::worded(format!(
                "Restoring {name} would leave code that does not parse, so nothing was restored."
            )))
        }
        Err(InstallError::Other(error)) => {
            return Err(Refusal::failed(&format!("restore {name}"), work, error))
        }
    }
    for transition in &transitions {
        prove(&prospective, transition, &chosen.code)
            .map_err(|error| Refusal::failed(&format!("restore {name} exactly"), work, error))?;
    }

    let next_tree = prospective.resolved_tree();
    let tree_deltas = kin_core::exact_tree_correction(&workspace.tree, &next_tree)
        .map_err(|error| Refusal::failed(&format!("restore {name}"), work, error))?;
    let target_graph = prospective.to_snapshot();
    drop(prospective);
    let semantic_delta = crate::local_repository_authority::plan_workspace_graph_transition(
        TargetGraph::of_snapshot(&workspace_graph),
        TargetGraph::of_snapshot(&target_graph),
    )
    .map_err(|error| Refusal::failed(&format!("restore {name}"), work, error))?;
    let new_tree_hash = kin_model::compute_resolved_tree_hash(&next_tree)
        .map_err(|error| Refusal::failed(&format!("restore {name}"), work, error))?;
    let transaction = RepositoryTransaction {
        schema_version: REPOSITORY_TRANSACTION_SCHEMA_VERSION,
        operation_id: request.operation_id,
        repository_id: authority.repository_id.clone(),
        expected_generation: roots.generation,
        expected_roots: roots,
        actor: request.actor.clone(),
        reason: format!("restore entity {id} to its version at change {target}"),
        external_objects: Vec::new(),
        git_authority_delta: None,
        changes: Vec::new(),
        aliases: Vec::new(),
        ref_mutations: Vec::new(),
        default_ref_mutation: None,
        workspace_mutation: Some(WorkspaceMutation {
            workspace_id: workspace.workspace_id,
            expected: WorkspaceExpectation::MustEqual {
                generation: workspace.generation,
                head: workspace.head.clone(),
                base_target: workspace.base_target.clone(),
                base_tree_hash: workspace.base_tree_hash,
                tree_hash: workspace.tree_hash,
                semantic_overlay_hash: workspace.semantic_overlay_hash,
                admission_policy: workspace.admission_policy,
            },
            new_generation: workspace.generation.checked_add(1).ok_or_else(|| {
                Refusal::failed(
                    &format!("restore {name}"),
                    work,
                    "workspace generation exhausted",
                )
            })?,
            new_head: workspace.head.clone(),
            new_base_target: workspace.base_target.clone(),
            new_base_tree_hash: workspace.base_tree_hash,
            tree_deltas,
            new_tree_hash,
            semantic_delta,
            new_shared_admission_policy: workspace.shared_admission_policy.clone(),
            new_admission_policy: workspace.admission_policy,
        }),
        local_overlay_delta: None,
        merge_transaction_delta: None,
        sealed_observation: None,
        collaboration_delta: None,
        git_snapshot_proofs: Vec::new(),
    };
    transaction
        .validate()
        .map_err(|error| Refusal::failed(&format!("restore {name}"), work, error))?;
    // The transition below projects the target tree from repository CAS, so the
    // body this restore spliced has to be there first. A body is immutable and
    // addressed by its content, so one a refused transition leaves behind is
    // named by nothing, exactly as a commit's staged bodies are.
    for transition in &transitions {
        let Some(hash) = transition
            .after
            .as_ref()
            .and_then(|(_, entry, _)| entry.blob_identity())
        else {
            continue;
        };
        let source = crate::source_cas::read_publishable_source(
            state.blobs.as_ref(),
            &authority.manager,
            hash,
        )
        .map_err(|error| Refusal::failed(&format!("restore {name}"), work, error))?;
        if let Some(body) = source.body_to_publish() {
            authority
                .manager
                .save_source_blob(hash, body)
                .map_err(|error| Refusal::failed(&format!("restore {name}"), work, error))?;
        }
    }
    if request.to.is_some() {
        crate::hydration_requalify::before_restoring(
            state,
            state.graph.as_ref(),
            target,
            "entity restore",
        )
        .map_err(|error| Refusal::failed(&format!("restore {name}"), work, error))?;
    }
    let published =
        kin_core::tree::transition_repository_workspace_tree_and_commit_repository_transaction(
            state.layout.working_dir(),
            &workspace.tree,
            &next_tree,
            &authority.manager,
            transaction,
        );
    let (_, receipt, authority_freeze) = match published {
        Ok(published) => published,
        Err(error) => {
            let landed =
                receipt_in(&authority.manager.read_authority(), request.operation_id).is_some();
            return Err(if landed {
                Refusal::unsettled(error)
            } else {
                Refusal::failed(
                    &format!("write {name} into the working copy as it now is"),
                    work,
                    error,
                )
            });
        }
    };
    let finalization = state
        .finalize_local_repository_commit(
            &receipt,
            &authority_freeze,
            &TransactionDelta::default(),
            &workspace.tree,
            &next_tree,
        )
        .map_err(Refusal::unsettled)?;
    if finalization.graph_changed {
        state.bump_version();
        state.emit_event(DaemonEvent::GraphRootChanged {
            old_root_hash: Some(previous_graph_root),
            new_root_hash: hex::encode(state.graph.compute_root_hash()),
        });
    } else if finalization.generation_advanced {
        state.mark_dirty();
    }
    if finalization.generation_advanced {
        state.emit_event(DaemonEvent::RepositoryAuthorityChanged {
            repository_id: receipt.repository_id.to_string(),
            operation_id: receipt.operation_id,
            previous_generation: receipt.roots_before.generation,
            new_generation: receipt.generation,
        });
    }
    if !finalization.graph_changed {
        state.invalidate_projection();
    }
    crate::api::relabel_held_authority_after_own_commit(state);
    drop(persistence);
    drop(graph_mutation);
    Ok(EntityRestoreResponse {
        entity: request.entity.trim().to_string(),
        name: Some(name),
        restored_to: Some(target.to_string()),
        replayed: false,
    })
}
