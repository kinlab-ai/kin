// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Edits an agent made with its own tools, attributed to the entities they
//! changed.
//!
//! Most coding agents change code with their own edit tools and never call a
//! Kin write. Kin sees such an edit only as the working copy changing, with no
//! one's name on it. A client that runs a hook after each edit can say whose
//! it was, and this module is where that statement is checked and kept.
//!
//! Two records are joined here.
//!
//! - A *step* is one unit moving from one content to the next in an admission:
//!   the content before, the content after, and the entities whose content
//!   changed in between. It is read from the admission itself. An admission
//!   that carries a unit's complete parse applies the entity changes in the
//!   same transaction as the content, and those changes are the step's. A
//!   unit whose parse comes later is remembered as it stood, and becomes a
//!   step once the graph holds a complete parse of the new content. An entity
//!   changed when it was added, removed, or modified in more than where it
//!   sits. A container that changed only because a member inside it did is
//!   left to that member.
//! - A *claim* is a client saying "this session just made this edit", with
//!   the edit itself: the text it replaced and what it put there, or the whole
//!   content it wrote.
//!
//! A pre hook first records the current admitted content identity and base
//! for the specific tool operation. Its matching post hook can claim only a
//! later step with that predecessor. A step is attributed only when applying the claimed edit to the
//! step's content before yields exactly its content after. So a step that also
//! holds someone else's change, or an edit a formatter rewrote before Kin read
//! it, is attributed to nobody: nothing here guesses. A claim that arrives
//! before its edit was admitted waits for the admission, and one that arrives
//! after finds the step already noted.
//!
//! What is kept is entity ids, the client's name and session, a time and the
//! head the working copy stood on. The place a claim names and the text it
//! carries are used to confirm it and then dropped.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use kin_cli::commands::review_guide::ClientEdit;
use kin_model::change::EntityDelta;
use kin_model::{
    Entity, EntityFilter, EntityId, EntityStore as _, FilePathId, Hash256, RepoPath,
    SemanticChange, SemanticChangeId, Timestamp, TreeDelta, TreeEntry,
};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::DaemonState;

/// Units one admission may move and still have its steps noted. A larger
/// admission is a checkout or a bulk rewrite, which no single edit made.
pub const MAX_UNITS_PER_PASS: usize = 32;
/// Units whose step is waiting for the graph to hold their new content.
pub const MAX_PENDING_UNITS: usize = 64;
/// Steps kept for a claim to find. Past it the oldest go.
pub const MAX_STEPS: usize = 256;
/// The largest unit a step is confirmed against.
pub const MAX_UNIT_BYTES: usize = 4 * 1024 * 1024;
/// Replacements one claim may carry.
pub const MAX_CLAIM_EDITS: usize = 256;
/// How long a unit may wait for its new content to be parsed.
const PENDING_UNIT_TTL: Duration = Duration::from_secs(30);
/// How long a claim waits for its edit to be admitted and parsed.
const RESOLVE_BUDGET: Duration = Duration::from_secs(8);
/// How long a claim waits on the watcher before it asks for an admission.
const WATCHER_GRACE: Duration = Duration::from_millis(1500);
const RESOLVE_TICK: Duration = Duration::from_millis(100);
const MAX_PREPARED_EDITS: usize = 256;
const PREPARED_EDIT_TTL: Duration = Duration::from_secs(300);

/// A unit whose tree entry moved, with what it held before.
#[derive(Debug)]
struct PendingUnit {
    /// The base its exact admission predecessor was captured on.
    base: Option<SemanticChangeId>,
    attribution_base: Option<SemanticChangeId>,
    old: Option<Hash256>,
    entities: Vec<Entity>,
    since: Instant,
}

/// One unit moving from one content to the next, and what that changed.
#[derive(Debug)]
struct Step {
    path: RepoPath,
    old: Option<Hash256>,
    new: Hash256,
    at: Timestamp,
    noted: Instant,
    /// Kept unchanged so a delayed post hook matches its prepared operation.
    base: Option<SemanticChangeId>,
    attribution: Vec<StepAttribution>,
    claimed: bool,
}

/// Where the entities of one proven edit now belong. A selected commit can
/// record only some of them while the same edit's other entities stay pending.
#[derive(Debug, Clone)]
struct StepAttribution {
    base: Option<SemanticChangeId>,
    change_id: Option<SemanticChangeId>,
    entities: Vec<EntityId>,
}

#[derive(Debug)]
struct PreparedEdit {
    claim: ClientEditClaim,
    path: RepoPath,
    old: Option<Hash256>,
    base: Option<SemanticChangeId>,
    since: Instant,
}

#[derive(Debug, Default)]
struct Inner {
    pending: HashMap<RepoPath, PendingUnit>,
    steps: VecDeque<Step>,
    prepared: HashMap<(String, String, String), PreparedEdit>,
}

/// The steps noted and the units still waiting to become one.
#[derive(Debug, Default)]
pub struct ClientEdits {
    inner: Mutex<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn blob_of(entry: &TreeEntry) -> Option<Hash256> {
    match entry {
        TreeEntry::Blob { hash, .. } => Some(*hash),
        _ => None,
    }
}

fn entities_at(state: &DaemonState, path: &RepoPath) -> Option<Vec<Entity>> {
    let file = FilePathId::new(path.as_utf8()?);
    state
        .graph
        .query_entities(&EntityFilter {
            file_path: Some(file),
            ..Default::default()
        })
        .ok()
}

fn span_of(entity: &Entity) -> Option<(usize, usize)> {
    entity
        .span
        .as_ref()
        .map(|span| (span.start_byte, span.end_byte))
}

/// Whether `inner` lies inside `outer` and is not the whole of it.
fn inside(inner: Option<(usize, usize)>, outer: Option<(usize, usize)>) -> bool {
    match (inner, outer) {
        (Some(inner), Some(outer)) => outer.0 <= inner.0 && inner.1 <= outer.1 && inner != outer,
        _ => false,
    }
}

/// The entities whose content changed between `before` and `after`, the two
/// sets one unit held on either side of a step.
///
/// Added and removed entities changed. An entity in both changed when it was
/// modified in more than where it sits, the test a review uses. A modified
/// container is left out when another changed entity lies inside it, and a
/// module container when anything else in its unit changed: its record moved
/// because a member's did, and the member is what was edited. `moved` names
/// an entity the graph still holds somewhere else, which was not removed.
pub(crate) fn changed_entities(
    before: &[Entity],
    after: &[Entity],
    moved: impl Fn(&EntityId) -> bool,
) -> Vec<EntityId> {
    #[derive(Clone, Copy, PartialEq)]
    enum How {
        Added,
        Modified,
        Removed,
    }
    let before_by: HashMap<EntityId, &Entity> = before.iter().map(|e| (e.id, e)).collect();
    let after_by: HashMap<EntityId, &Entity> = after.iter().map(|e| (e.id, e)).collect();
    let mut changed: Vec<(EntityId, How)> = Vec::new();
    for entity in after {
        match before_by.get(&entity.id) {
            None => changed.push((entity.id, How::Added)),
            Some(old) if kin_review::is_semantic_modification(old, entity) => {
                changed.push((entity.id, How::Modified));
            }
            Some(_) => {}
        }
    }
    for entity in before {
        if !after_by.contains_key(&entity.id) && !moved(&entity.id) {
            changed.push((entity.id, How::Removed));
        }
    }
    let explained = |id: &EntityId| {
        let (Some(old), Some(new)) = (before_by.get(id), after_by.get(id)) else {
            return false;
        };
        if kin_model::is_module_container(new) {
            return changed.len() > 1;
        }
        changed.iter().any(|(other, how)| {
            other != id
                && match how {
                    How::Removed => {
                        inside(before_by.get(other).and_then(|e| span_of(e)), span_of(old))
                    }
                    How::Added | How::Modified => {
                        inside(after_by.get(other).and_then(|e| span_of(e)), span_of(new))
                    }
                }
        })
    };
    let mut ids: Vec<EntityId> = changed
        .iter()
        .filter(|(id, how)| *how != How::Modified || !explained(id))
        .map(|(id, _)| *id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

impl ClientEdits {
    /// Note one step for each unit an admission moved.
    ///
    /// Called by every admission of the working copy, once its transaction
    /// has been applied. `parsed` names the units whose complete parse that
    /// transaction carried, and `applied` is the entity delta it applied: for
    /// those units the step is whole, and its entities are read from the
    /// delta. Any other unit moved in the tree only, with its parse still to
    /// come, so what it holds now is what it held before, and it waits for
    /// [`ClientEdits::settle`]. A unit already waiting keeps what it held
    /// first, so two moves the graph has not parsed between become one step
    /// from the first content to the last.
    pub(crate) fn tree_moved(
        &self,
        state: &DaemonState,
        deltas: &[TreeDelta],
        parsed: Option<&BTreeSet<RepoPath>>,
        applied: &[EntityDelta],
    ) {
        // Pending units were captured before their parse. Their next parse
        // can span several tree admissions, so do not manufacture a second
        // step from a successor delta with that earlier semantic predecessor.
        let already_pending: HashSet<RepoPath> =
            lock(&self.inner).pending.keys().cloned().collect();
        self.settle(state);
        if deltas.is_empty() {
            return;
        }
        let mut inner = lock(&self.inner);
        if deltas.len() > MAX_UNITS_PER_PASS {
            for delta in deltas {
                for located in [delta.old_state(), delta.new_state()].into_iter().flatten() {
                    inner.pending.remove(&located.path);
                }
            }
            return;
        }
        for delta in deltas {
            let (path, old, new) = match delta {
                TreeDelta::Added { new, .. } => match blob_of(&new.entry) {
                    Some(new_hash) => (&new.path, None, new_hash),
                    None => continue,
                },
                TreeDelta::Updated { old, new, .. } if old.path == new.path => {
                    match (blob_of(&old.entry), blob_of(&new.entry)) {
                        (Some(old_hash), Some(new_hash)) => (&new.path, Some(old_hash), new_hash),
                        _ => continue,
                    }
                }
                // A unit that moved place, went away, or is not content of
                // its own is no edit anyone will claim.
                _ => {
                    for located in [delta.old_state(), delta.new_state()].into_iter().flatten() {
                        inner.pending.remove(&located.path);
                    }
                    continue;
                }
            };
            if already_pending.contains(path) || inner.pending.contains_key(path) {
                continue;
            }
            let Some(file) = path.as_utf8().map(FilePathId::new) else {
                continue;
            };
            if parsed.is_some_and(|parsed| parsed.contains(path)) {
                let mut before = Vec::new();
                let mut after = Vec::new();
                let here = |entity: &Entity| entity.file_origin.as_ref() == Some(&file);
                for change in applied {
                    match change {
                        EntityDelta::Added { new } if here(new) => after.push(new.clone()),
                        EntityDelta::Modified { old, new } if here(old) && here(new) => {
                            before.push(old.clone());
                            after.push(new.clone());
                        }
                        EntityDelta::Removed { old } if here(old) => before.push(old.clone()),
                        _ => {}
                    }
                }
                let changed = changed_entities(&before, &after, |id| {
                    state.graph.get_entity(id).ok().flatten().is_some()
                });
                debug!(entities = changed.len(), "noted one unit's admission step");
                let base = workspace_base(state);
                inner.steps.push_back(Step {
                    path: path.clone(),
                    old,
                    new,
                    at: Timestamp::now(),
                    noted: Instant::now(),
                    base,
                    attribution: vec![StepAttribution {
                        base,
                        change_id: None,
                        entities: changed,
                    }],
                    claimed: false,
                });
                while inner.steps.len() > MAX_STEPS {
                    inner.steps.pop_front();
                }
                continue;
            }
            // Only source Kin parses can become a step: anything else holds
            // no entities whose content an edit could change.
            if inner.pending.len() >= MAX_PENDING_UNITS
                || !crate::loop_runner::is_entity_source_path(path)
            {
                continue;
            }
            let Some(entities) = entities_at(state, path) else {
                continue;
            };
            let base = workspace_base(state);
            inner.pending.insert(
                path.clone(),
                PendingUnit {
                    base,
                    attribution_base: base,
                    old,
                    entities,
                    since: Instant::now(),
                },
            );
        }
    }

    /// Turn every waiting unit whose new content the graph now holds into a
    /// step. A unit back at the content it started from changed nothing, and
    /// one the graph has not parsed yet keeps waiting.
    pub(crate) fn settle(&self, state: &DaemonState) {
        let mut inner = lock(&self.inner);
        if inner.pending.is_empty() {
            return;
        }
        let tree = state.graph.resolved_tree();
        let paths: Vec<RepoPath> = inner.pending.keys().cloned().collect();
        for path in paths {
            let Some(unit) = inner.pending.get(&path) else {
                continue;
            };
            let Some(new) = tree
                .artifact_at_path(&path)
                .and_then(|artifact| blob_of(&artifact.entry))
            else {
                inner.pending.remove(&path);
                continue;
            };
            if Some(new) == unit.old {
                inner.pending.remove(&path);
                continue;
            }
            if !crate::loop_runner::unit_semantics_are_held(state, &tree, &path) {
                if unit.since.elapsed() > PENDING_UNIT_TTL {
                    inner.pending.remove(&path);
                }
                continue;
            }
            let Some(unit) = inner.pending.remove(&path) else {
                continue;
            };
            let Some(after) = entities_at(state, &path) else {
                continue;
            };
            let changed = changed_entities(&unit.entities, &after, |id| {
                state.graph.get_entity(id).ok().flatten().is_some()
            });
            debug!(entities = changed.len(), "noted one unit's admission step");
            inner.steps.push_back(Step {
                path,
                old: unit.old,
                new,
                at: Timestamp::now(),
                noted: Instant::now(),
                base: unit.base,
                attribution: vec![StepAttribution {
                    base: unit.attribution_base,
                    change_id: None,
                    entities: changed,
                }],
                claimed: false,
            });
            while inner.steps.len() > MAX_STEPS {
                inner.steps.pop_front();
            }
        }
    }

    fn is_pending(&self, path: &RepoPath) -> bool {
        lock(&self.inner).pending.contains_key(path)
    }

    fn forget_prepared(&self, claim: &ClientEditClaim) {
        if let Some(key) = claim_key(claim) {
            lock(&self.inner).prepared.remove(&key);
        }
    }

    /// The newest unclaimed step of `path` that is exactly `claim`'s edit,
    /// claimed: the entities it changed and when.
    fn claim_step(
        &self,
        state: &DaemonState,
        path: &RepoPath,
        claim: &ClientEditClaim,
    ) -> Option<(Vec<StepAttribution>, Timestamp)> {
        let mut inner = lock(&self.inner);
        let read = |hash: &Hash256| {
            state
                .blobs
                .read(hash)
                .ok()
                .filter(|bytes| bytes.len() <= MAX_UNIT_BYTES)
        };
        let key = claim_key(claim)?;
        let prepared = inner.prepared.remove(&key)?;
        if prepared.since.elapsed() > PREPARED_EDIT_TTL
            || prepared.path != *path
            || !same_operation(&prepared.claim, claim)
        {
            return None;
        }
        let step = inner
            .steps
            .iter_mut()
            .rev()
            .filter(|step| {
                !step.claimed
                    && step.path == *path
                    && step.old == prepared.old
                    && step.base == prepared.base
                    && step.noted >= prepared.since
            })
            .find(|step| {
                let before = match &step.old {
                    Some(hash) => read(hash),
                    None => Some(Vec::new()),
                };
                match (before, read(&step.new)) {
                    (Some(before), Some(after)) => confirms(claim, &before, &after),
                    _ => false,
                }
            });
        let Some(step) = step else {
            inner.prepared.insert(key, prepared);
            return None;
        };
        step.claimed = true;
        Some((step.attribution.clone(), step.at.clone()))
    }
}

// ---------------------------------------------------------------------------
// Claims
// ---------------------------------------------------------------------------

/// One replacement a client's edit tool made: `old_text` became `new_text`,
/// once, or everywhere with `replace_all`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimedReplacement {
    pub old_text: String,
    pub new_text: String,
    #[serde(default)]
    pub replace_all: bool,
}

/// A client saying one of its sessions just made an edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientEditClaim {
    /// The client as it names itself, such as `claude-code`.
    pub client: String,
    /// The client's own session.
    pub session: String,
    /// The same operation id carried by the client's pre and post hooks.
    #[serde(default)]
    pub tool_use_id: Option<String>,
    /// `before` prepares evidence before execution; `after` confirms success.
    #[serde(default = "after_phase")]
    pub phase: String,
    /// Where the client says it edited, as the client names it. Used to find
    /// the unit and then dropped.
    pub unit: String,
    /// The replacements it made, in the order it made them.
    #[serde(default)]
    pub edits: Vec<ClaimedReplacement>,
    /// The whole content it wrote, for a tool that writes a unit whole.
    #[serde(default)]
    pub content: Option<String>,
}

/// What became of a claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimOutcome {
    pub schema: String,
    /// `attributed` when a step was exactly this edit; `accepted` when the
    /// claim was taken and is being resolved; `unconfirmed` when Kin holds the
    /// edited content and no step is exactly this edit; `not_settled` when Kin
    /// had not read the edit in time; `not_this_repository` when the claim
    /// names nothing this repository holds; `refused` when it cannot be read.
    pub outcome: String,
    /// How many entities the edit was attributed on.
    pub entities: usize,
}

impl ClaimOutcome {
    fn of(outcome: &str, entities: usize) -> Self {
        Self {
            schema: "kin.agents.edit.v1".to_string(),
            outcome: outcome.to_string(),
            entities,
        }
    }
}

/// `before` with `claim`'s edit applied, or `None` when the edit does not
/// apply to it as the client's tool would have applied it: a replacement
/// whose text is absent, or present more than once without `replace_all`.
fn applied(claim: &ClientEditClaim, before: &str) -> Option<String> {
    if let Some(content) = &claim.content {
        return Some(content.clone());
    }
    if claim.edits.is_empty() {
        return None;
    }
    let mut text = before.to_string();
    for edit in &claim.edits {
        if edit.old_text.is_empty() {
            // Nothing to replace: the tool wrote a unit that held nothing.
            if !text.is_empty() {
                return None;
            }
            text.clone_from(&edit.new_text);
            continue;
        }
        let found = text.matches(edit.old_text.as_str()).count();
        if found == 0 || (found > 1 && !edit.replace_all) {
            return None;
        }
        text = if edit.replace_all {
            text.replace(edit.old_text.as_str(), &edit.new_text)
        } else {
            text.replacen(edit.old_text.as_str(), &edit.new_text, 1)
        };
    }
    Some(text)
}

/// Whether `claim`'s edit, applied to `before`, yields exactly `after`. A unit
/// that keeps carriage returns is also compared without them, because an edit
/// tool reads and writes such a unit through line-feed text.
pub(crate) fn confirms(claim: &ClientEditClaim, before: &[u8], after: &[u8]) -> bool {
    let (Ok(before), Ok(after)) = (std::str::from_utf8(before), std::str::from_utf8(after)) else {
        return false;
    };
    let exact = |before: &str, after: &str| applied(claim, before).is_some_and(|out| out == after);
    if exact(before, after) {
        return true;
    }
    if !before.contains("\r\n") && !after.contains("\r\n") {
        return false;
    }
    exact(&before.replace("\r\n", "\n"), &after.replace("\r\n", "\n"))
}

/// The change this daemon's working copy stands on.
fn workspace_base(state: &DaemonState) -> Option<SemanticChangeId> {
    crate::unit_lifecycle::workspace_base_change(state)
}

fn client_label(raw: &str) -> String {
    raw.trim().to_ascii_lowercase().replace([' ', '_'], "-")
}

/// The one connected Kin session of `client`, when there is exactly one.
fn lone_session_of(state: &DaemonState, client: &str) -> Option<String> {
    let sessions = state.coordinator.list_sessions().ok()?;
    let mut of_client = sessions.iter().filter(|session| {
        client_label(&session.vendor) == client && !state.coordinator.process_gone(session)
    });
    let only = of_client.next()?;
    of_client
        .next()
        .is_none()
        .then(|| only.session_id.to_string())
}

fn after_phase() -> String {
    "after".into()
}

fn claim_key(claim: &ClientEditClaim) -> Option<(String, String, String)> {
    let operation = claim.tool_use_id.as_deref()?.trim();
    (!operation.is_empty()).then(|| {
        (
            claim.client.clone(),
            claim.session.clone(),
            operation.to_string(),
        )
    })
}

fn same_operation(before: &ClientEditClaim, after: &ClientEditClaim) -> bool {
    before.client == after.client
        && before.session == after.session
        && before.tool_use_id == after.tool_use_id
        && before.unit == after.unit
        && before.edits == after.edits
        && before.content == after.content
}

/// Seal attribution only when this daemon actually publishes a commit. A
/// branch switch or remote advance does not establish that an edit was part
/// of that change.
pub(crate) fn seal_commit(
    state: &DaemonState,
    change: &SemanticChange,
    base: Option<SemanticChangeId>,
) {
    // Commit may have drained a pending parse without another tree move.
    // Finish its captured admission step before marking this publication.
    state.client_edits.settle(state);
    let held: HashSet<EntityId> = change
        .entity_deltas
        .iter()
        .map(EntityDelta::target_id)
        .collect();
    {
        let mut inner = lock(&state.client_edits.inner);
        for attribution in inner
            .steps
            .iter_mut()
            .flat_map(|step| &mut step.attribution)
            .filter(|attribution| attribution.change_id.is_none() && attribution.base == base)
        {
            attribution.entities.retain(|id| held.contains(id));
            attribution.change_id = Some(change.id);
        }
    }
    state.agent_authorship.reseal_edits(|edits| {
        let mut changed = false;
        for edit in edits.iter_mut().filter(|edit| {
            edit.change_id.is_none() && edit.base == base.map(|base| base.to_string())
        }) {
            edit.entities.retain(|id| held.contains(id));
            edit.change_id = Some(change.id.to_string());
            changed = true;
        }
        changed
    });
}

fn take_committed_entities(
    entities: &mut Vec<EntityId>,
    committed: &HashSet<EntityId>,
) -> Vec<EntityId> {
    let mut recorded = Vec::new();
    entities.retain(|id| {
        if committed.contains(id) {
            recorded.push(*id);
            false
        } else {
            true
        }
    });
    recorded
}

fn seal_kept_step(
    step: &mut Step,
    change_id: SemanticChangeId,
    base: Option<SemanticChangeId>,
    committed: &HashSet<EntityId>,
) {
    let mut recorded = Vec::new();
    for attribution in step
        .attribution
        .iter_mut()
        .filter(|attribution| attribution.change_id.is_none() && attribution.base == base)
    {
        let entities = take_committed_entities(&mut attribution.entities, committed);
        if !entities.is_empty() {
            recorded.push(StepAttribution {
                base: attribution.base,
                change_id: Some(change_id),
                entities,
            });
        }
        attribution.base = Some(change_id);
    }
    step.attribution
        .retain(|attribution| !attribution.entities.is_empty());
    step.attribution.extend(recorded);
}

fn seal_kept_edits(
    edits: &mut Vec<ClientEdit>,
    change_id: SemanticChangeId,
    base: Option<SemanticChangeId>,
    committed: &HashSet<EntityId>,
) -> bool {
    let base = base.map(|base| base.to_string());
    let change_id = change_id.to_string();
    let mut recorded = Vec::new();
    let mut changed = false;
    for edit in edits
        .iter_mut()
        .filter(|edit| edit.change_id.is_none() && edit.base == base)
    {
        let entities = take_committed_entities(&mut edit.entities, committed);
        if !entities.is_empty() {
            let mut sealed = edit.clone();
            sealed.entities = entities;
            sealed.change_id = Some(change_id.clone());
            recorded.push(sealed);
        }
        edit.base = Some(change_id.clone());
        changed = true;
    }
    edits.retain(|edit| !edit.entities.is_empty());
    edits.extend(recorded);
    changed
}

/// Record attribution for the chosen work and keep the rest on the new base.
///
/// The admission step keeps its original base and exact before/after content
/// for a post hook that arrives later. Its attribution can belong to several
/// changes by then, so confirming the edit returns every surviving partition.
pub(crate) fn seal_kept_commit(
    state: &DaemonState,
    change: &SemanticChange,
    base: Option<SemanticChangeId>,
    chosen: &HashSet<EntityId>,
) {
    state.client_edits.settle(state);
    let committed = change
        .entity_deltas
        .iter()
        .filter(|delta| chosen.contains(&delta.target_id()))
        .filter(|delta| match delta {
            EntityDelta::Modified { old, new } => kin_review::is_semantic_modification(old, new),
            EntityDelta::Added { .. } | EntityDelta::Removed { .. } => true,
        })
        .map(EntityDelta::target_id)
        .collect::<HashSet<_>>();
    {
        let mut inner = lock(&state.client_edits.inner);
        for step in &mut inner.steps {
            seal_kept_step(step, change.id, base, &committed);
        }
        for unit in inner
            .pending
            .values_mut()
            .filter(|unit| unit.attribution_base == base)
        {
            unit.attribution_base = Some(change.id);
        }
    }
    state
        .agent_authorship
        .reseal_edits(|edits| seal_kept_edits(edits, change.id, base, &committed));
}

/// Prepare one client operation before it executes. The host and the parsed
/// graph must agree first; a pending or unattributed earlier edit is never
/// swept into this operation's authorship.
async fn prepare(
    state: &Arc<DaemonState>,
    claim: ClientEditClaim,
    path: RepoPath,
    host: PathBuf,
) -> ClaimOutcome {
    let Some(key) = claim_key(&claim) else {
        return ClaimOutcome::of("refused", 0);
    };
    let _coordination = state.coordination_gate.lock().await;
    crate::loop_runner::off_the_runtime_worker(|| {
        let tree = state.graph.resolved_tree();
        let old = tree.artifact_at_path(&path).and_then(|a| blob_of(&a.entry));
        if old.is_some() && !crate::loop_runner::unit_semantics_are_held(state, &tree, &path) {
            return ClaimOutcome::of("not_settled", 0);
        }
        if !crate::loop_runner::host_unit_is_admitted(state, &host, &path) {
            return ClaimOutcome::of("not_settled", 0);
        }
        let mut inner = lock(&state.client_edits.inner);
        inner
            .prepared
            .retain(|_, prepared| prepared.since.elapsed() <= PREPARED_EDIT_TTL);
        if inner.prepared.len() >= MAX_PREPARED_EDITS || inner.prepared.contains_key(&key) {
            return ClaimOutcome::of("refused", 0);
        }
        inner.prepared.insert(
            key,
            PreparedEdit {
                claim,
                path,
                old,
                base: workspace_base(state),
                since: Instant::now(),
            },
        );
        ClaimOutcome::of("prepared", 0)
    })
}

/// Check one claim against the steps noted and keep what it confirms.
///
/// Waits for the edit to be admitted and parsed when it has not been: the
/// watcher usually has it within a moment, and an admission is asked for when
/// it does not.
pub(crate) async fn resolve(state: &Arc<DaemonState>, claim: ClientEditClaim) -> ClaimOutcome {
    let client = client_label(&crate::agent_activity::display_name(&claim.client, ""));
    let session = crate::agent_activity::display_name(&claim.session, "");
    let readable = !client.is_empty()
        && !session.is_empty()
        && claim.edits.len() <= MAX_CLAIM_EDITS
        && (claim.content.is_some() || !claim.edits.is_empty());
    let host = PathBuf::from(claim.unit.trim());
    if !readable || !host.is_absolute() {
        return ClaimOutcome::of("refused", 0);
    }
    let Some(path) = crate::loop_runner::off_the_runtime_worker(|| {
        crate::loop_runner::host_repo_path(state, &host)
    }) else {
        return ClaimOutcome::of("not_this_repository", 0);
    };
    if claim.phase == "before" {
        return prepare(state, claim, path, host).await;
    }
    if claim.phase != "after" || claim_key(&claim).is_none() {
        return ClaimOutcome::of("refused", 0);
    }
    let started = Instant::now();
    let mut asked = false;
    loop {
        let found = {
            let _coordination = state.coordination_gate.lock().await;
            crate::loop_runner::off_the_runtime_worker(|| {
                state.client_edits.settle(state);
                let (attribution, at) = state.client_edits.claim_step(state, &path, &claim)?;
                let attributed = attribution.iter().map(|part| part.entities.len()).sum();
                for part in attribution {
                    state.agent_authorship.record_edit(ClientEdit {
                        base: part.base.map(|base| base.to_string()),
                        change_id: part.change_id.map(|change| change.to_string()),
                        client: client.clone(),
                        session: session.clone(),
                        kin_session: lone_session_of(state, &client),
                        at: at.to_string(),
                        entities: part.entities,
                    });
                }
                Some(attributed)
            })
        };
        if let Some(attributed) = found {
            return ClaimOutcome::of("attributed", attributed);
        }
        let admitted = crate::loop_runner::off_the_runtime_worker(|| {
            crate::loop_runner::host_unit_is_admitted(state, &host, &path)
        });
        if admitted && !state.client_edits.is_pending(&path) {
            state.client_edits.forget_prepared(&claim);
            return ClaimOutcome::of("unconfirmed", 0);
        }
        if started.elapsed() >= RESOLVE_BUDGET {
            state.client_edits.forget_prepared(&claim);
            return ClaimOutcome::of("not_settled", 0);
        }
        if !admitted && !asked && started.elapsed() >= WATCHER_GRACE {
            asked = true;
            if let Err(error) = crate::loop_runner::sync_filesystem_with_graph(state).await {
                debug!(%error, "the admission a reported edit asked for did not finish");
            }
            continue;
        }
        tokio::time::sleep(RESOLVE_TICK).await;
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ClaimParams {
    /// Answer once the claim is resolved, with what became of it.
    #[serde(default)]
    wait: bool,
}

/// POST /agents/edits: a client reports an edit one of its sessions made.
///
/// Answers at once, and resolves the claim behind the answer, so a hook that
/// runs after every edit costs its client one local request. `?wait=true`
/// answers with what became of the claim instead.
pub(crate) async fn record_edit_route(
    State(state): State<Arc<DaemonState>>,
    Query(params): Query<ClaimParams>,
    Json(claim): Json<ClientEditClaim>,
) -> (StatusCode, Json<ClaimOutcome>) {
    if params.wait || claim.phase == "before" {
        return (StatusCode::OK, Json(resolve(&state, claim).await));
    }
    tokio::spawn(async move {
        let outcome = resolve(&state, claim).await;
        debug!(outcome = %outcome.outcome, entities = outcome.entities, "resolved a reported edit");
    });
    (StatusCode::ACCEPTED, Json(ClaimOutcome::of("accepted", 0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(name: &str, span: (usize, usize), body: u8) -> Entity {
        let place = FilePathId::new("shapes.py");
        Entity {
            id: EntityId::new(),
            kind: kin_model::EntityKind::Function,
            name: name.to_string(),
            language: kin_model::LanguageId::Python,
            fingerprint: kin_model::SemanticFingerprint {
                algorithm: kin_model::FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([body; 32]),
                signature_hash: Hash256::from_bytes([2; 32]),
                behavior_hash: Hash256::from_bytes([body; 32]),
                equivalence_hash: Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(place.clone()),
            span: Some(kin_model::SourceSpan {
                file: place,
                start_byte: span.0,
                end_byte: span.1,
                start_line: 1,
                start_col: 0,
                end_line: 2,
                end_col: 0,
            }),
            signature: format!("def {name}()"),
            visibility: kin_model::Visibility::Public,
            role: kin_model::EntityRole::Source,
            doc_summary: None,
            metadata: kin_model::EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    fn edited(entity: &Entity, body: u8, span: (usize, usize)) -> Entity {
        let mut new = entity.clone();
        new.fingerprint.ast_hash = Hash256::from_bytes([body; 32]);
        new.fingerprint.behavior_hash = Hash256::from_bytes([body; 32]);
        if let Some(held) = new.span.as_mut() {
            held.start_byte = span.0;
            held.end_byte = span.1;
        }
        new
    }

    fn moved_to(entity: &Entity, span: (usize, usize)) -> Entity {
        let mut new = entity.clone();
        if let Some(held) = new.span.as_mut() {
            held.start_byte = span.0;
            held.end_byte = span.1;
        }
        new
    }

    fn replace(old: &str, new: &str) -> ClientEditClaim {
        ClientEditClaim {
            client: "claude-code".into(),
            session: "s".into(),
            tool_use_id: Some("tool-1".into()),
            phase: "after".into(),
            unit: "/repo/shapes.py".into(),
            edits: vec![ClaimedReplacement {
                old_text: old.into(),
                new_text: new.into(),
                replace_all: false,
            }],
            content: None,
        }
    }

    #[test]
    fn selected_commits_keep_one_late_claims_proof_and_all_its_attribution() {
        let first = EntityId::new();
        let second = EntityId::new();
        let remaining = EntityId::new();
        let base = SemanticChangeId::from_hash(Hash256::from_bytes([1; 32]));
        let first_commit = SemanticChangeId::from_hash(Hash256::from_bytes([2; 32]));
        let second_commit = SemanticChangeId::from_hash(Hash256::from_bytes([3; 32]));
        let old = Hash256::from_bytes([4; 32]);
        let new = Hash256::from_bytes([5; 32]);
        let mut step = Step {
            path: RepoPath::from_utf8("shapes.py").unwrap(),
            old: Some(old),
            new,
            at: Timestamp::now(),
            noted: Instant::now(),
            base: Some(base),
            attribution: vec![StepAttribution {
                base: Some(base),
                change_id: None,
                entities: vec![first, second, remaining],
            }],
            claimed: false,
        };
        seal_kept_step(&mut step, first_commit, Some(base), &HashSet::from([first]));
        seal_kept_step(
            &mut step,
            second_commit,
            Some(first_commit),
            &HashSet::from([second]),
        );

        assert_eq!(step.base, Some(base));
        assert_eq!(step.old, Some(old));
        assert_eq!(step.new, new);
        assert!(!step.claimed);
        assert_eq!(step.attribution.len(), 3);
        let held = |id: EntityId| {
            step.attribution
                .iter()
                .find(|part| part.entities.contains(&id))
                .unwrap()
        };
        assert_eq!(held(first).change_id, Some(first_commit));
        assert_eq!(held(first).base, Some(base));
        assert_eq!(held(second).change_id, Some(second_commit));
        assert_eq!(held(second).base, Some(first_commit));
        assert_eq!(held(remaining).change_id, None);
        assert_eq!(held(remaining).base, Some(second_commit));
    }

    #[test]
    fn selected_commits_preserve_recorded_edits_metadata_and_pending_siblings() {
        let first = EntityId::new();
        let second = EntityId::new();
        let remaining = EntityId::new();
        let base = SemanticChangeId::from_hash(Hash256::from_bytes([1; 32]));
        let first_commit = SemanticChangeId::from_hash(Hash256::from_bytes([2; 32]));
        let second_commit = SemanticChangeId::from_hash(Hash256::from_bytes([3; 32]));
        let original = ClientEdit {
            base: Some(base.to_string()),
            change_id: None,
            client: "claude-code".into(),
            session: "client-session".into(),
            kin_session: Some("kin-session".into()),
            at: Timestamp::now().to_string(),
            entities: vec![first, second, remaining],
        };
        let mut edits = vec![original.clone()];
        assert!(seal_kept_edits(
            &mut edits,
            first_commit,
            Some(base),
            &HashSet::from([first]),
        ));
        assert!(seal_kept_edits(
            &mut edits,
            second_commit,
            Some(first_commit),
            &HashSet::from([second]),
        ));
        assert!(!seal_kept_edits(
            &mut edits,
            first_commit,
            Some(base),
            &HashSet::from([first]),
        ));
        assert_eq!(edits.len(), 3);
        for edit in &edits {
            assert_eq!(edit.client, original.client);
            assert_eq!(edit.session, original.session);
            assert_eq!(edit.kin_session, original.kin_session);
            assert_eq!(edit.at, original.at);
            assert_eq!(edit.entities.len(), 1);
        }
        let held = |id: EntityId| edits.iter().find(|edit| edit.entities == vec![id]).unwrap();
        assert_eq!(held(first).change_id, Some(first_commit.to_string()));
        assert_eq!(held(first).base, original.base);
        assert_eq!(held(second).change_id, Some(second_commit.to_string()));
        assert_eq!(held(second).base, Some(first_commit.to_string()));
        assert_eq!(held(remaining).change_id, None);
        assert_eq!(held(remaining).base, Some(second_commit.to_string()));
    }

    /// One function's body changed. The function after it only moved down,
    /// and the class around it changed only because its member did.
    #[test]
    fn only_entities_whose_content_changed_are_the_edits() {
        let class = entity("Shape", (0, 100), 1);
        let area = entity("area", (10, 40), 2);
        let perimeter = entity("perimeter", (50, 90), 3);
        let before = vec![class.clone(), area.clone(), perimeter.clone()];
        let after = vec![
            edited(&class, 9, (0, 104)),
            edited(&area, 8, (10, 44)),
            moved_to(&perimeter, (54, 94)),
        ];
        assert_eq!(changed_entities(&before, &after, |_| false), vec![area.id]);
    }

    /// An added entity and a removed one both changed, and one the graph
    /// still holds elsewhere was not removed.
    #[test]
    fn entities_that_came_or_went_changed() {
        let kept = entity("area", (0, 10), 1);
        let gone = entity("old", (20, 30), 2);
        let relocated = entity("moved", (40, 50), 3);
        let came = entity("scale", (20, 35), 4);
        let before = vec![kept.clone(), gone.clone(), relocated.clone()];
        let after = vec![kept.clone(), came.clone()];
        let mut expected = vec![gone.id, came.id];
        expected.sort_unstable();
        assert_eq!(
            changed_entities(&before, &after, |id| *id == relocated.id),
            expected
        );
    }

    /// A step is a claim's only when the claimed edit takes its content
    /// before to exactly its content after.
    #[test]
    fn a_step_is_confirmed_only_by_exactly_its_edit() {
        let before = b"def area(w, h):\n    return w * h\n";
        let after = b"def area(w, h):\n    return abs(w * h)\n";
        assert!(confirms(
            &replace("return w * h", "return abs(w * h)"),
            before,
            after
        ));
        // Someone else's change rode along in the same step.
        let mixed = b"def area(w, h):\n    return abs(w * h)\n\nX = 1\n";
        assert!(!confirms(
            &replace("return w * h", "return abs(w * h)"),
            before,
            mixed
        ));
        // The text is not there, or is there twice.
        assert!(!confirms(
            &replace("return w + h", "return 0"),
            before,
            after
        ));
        let twice = b"a = 1\nb = 1\n";
        assert!(!confirms(&replace("1", "2"), twice, b"a = 2\nb = 1\n"));
        let mut everywhere = replace("1", "2");
        everywhere.edits[0].replace_all = true;
        assert!(confirms(&everywhere, twice, b"a = 2\nb = 2\n"));
    }

    /// A whole-content write is confirmed by the content it wrote, an edit
    /// that creates a unit by an empty start, and a unit that keeps carriage
    /// returns by its line-feed text.
    #[test]
    fn whole_writes_new_units_and_carriage_returns_are_confirmed() {
        let mut write = replace("", "");
        write.edits.clear();
        write.content = Some("x = 1\n".into());
        assert!(confirms(&write, b"x = 0\n", b"x = 1\n"));
        assert!(!confirms(&write, b"x = 0\n", b"x = 2\n"));
        assert!(confirms(&replace("", "x = 1\n"), b"", b"x = 1\n"));
        assert!(!confirms(&replace("", "x = 1\n"), b"y = 0\n", b"x = 1\n"));
        assert!(confirms(
            &replace("a = 1\nb = 1", "a = 2\nb = 1"),
            b"a = 1\r\nb = 1\r\n",
            b"a = 2\r\nb = 1\r\n"
        ));
        let mut empty = replace("", "");
        empty.edits.clear();
        assert!(
            !confirms(&empty, b"", b""),
            "a claim of nothing confirms nothing"
        );
    }
}
