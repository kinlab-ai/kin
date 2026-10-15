// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Semantic Work Graph types: work items, annotations, and their relationships.
//!
//! Phase 8 introduces first-class graph objects for planned work and living
//! annotations. Work items (features, tasks, issues, debt, TODOs) are anchored
//! to semantic scopes — not line numbers — so they are designed to stay
//! attached across renames, moves, and formatting churn.

use crate::ids::*;
use crate::timestamp::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// IDs
// ---------------------------------------------------------------------------

/// Unique identifier for a WorkItem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct WorkId(pub uuid::Uuid);

impl WorkId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for WorkId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for WorkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for an Annotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct AnnotationId(pub uuid::Uuid);

impl AnnotationId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for AnnotationId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for AnnotationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Work Item
// ---------------------------------------------------------------------------

/// The kind of work this item represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum WorkKind {
    Feature,
    Task,
    Issue,
    Debt,
    Todo,
    Investigation,
}

impl std::fmt::Display for WorkKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkKind::Feature => write!(f, "feature"),
            WorkKind::Task => write!(f, "task"),
            WorkKind::Issue => write!(f, "issue"),
            WorkKind::Debt => write!(f, "debt"),
            WorkKind::Todo => write!(f, "todo"),
            WorkKind::Investigation => write!(f, "investigation"),
        }
    }
}

impl std::str::FromStr for WorkKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "feature" => Ok(WorkKind::Feature),
            "task" => Ok(WorkKind::Task),
            "issue" => Ok(WorkKind::Issue),
            "debt" => Ok(WorkKind::Debt),
            "todo" => Ok(WorkKind::Todo),
            "investigation" => Ok(WorkKind::Investigation),
            _ => Err(format!("unknown work kind: {}", s)),
        }
    }
}

/// Lifecycle status of a work item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum WorkStatus {
    Proposed,
    Planned,
    InProgress,
    Blocked,
    Done,
    Verified,
    Archived,
}

impl std::fmt::Display for WorkStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkStatus::Proposed => write!(f, "proposed"),
            WorkStatus::Planned => write!(f, "planned"),
            WorkStatus::InProgress => write!(f, "in_progress"),
            WorkStatus::Blocked => write!(f, "blocked"),
            WorkStatus::Done => write!(f, "done"),
            WorkStatus::Verified => write!(f, "verified"),
            WorkStatus::Archived => write!(f, "archived"),
        }
    }
}

impl std::str::FromStr for WorkStatus {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "proposed" => Ok(WorkStatus::Proposed),
            "planned" => Ok(WorkStatus::Planned),
            "in_progress" | "in-progress" | "inprogress" => Ok(WorkStatus::InProgress),
            "blocked" => Ok(WorkStatus::Blocked),
            "done" => Ok(WorkStatus::Done),
            "verified" => Ok(WorkStatus::Verified),
            "archived" => Ok(WorkStatus::Archived),
            _ => Err(format!("unknown work status: {}", s)),
        }
    }
}

/// Priority level for work items.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum Priority {
    Critical,
    High,
    Medium,
    Low,
    #[default]
    None,
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Priority::Critical => write!(f, "critical"),
            Priority::High => write!(f, "high"),
            Priority::Medium => write!(f, "medium"),
            Priority::Low => write!(f, "low"),
            Priority::None => write!(f, "none"),
        }
    }
}

impl std::str::FromStr for Priority {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "critical" => Ok(Priority::Critical),
            "high" => Ok(Priority::High),
            "medium" => Ok(Priority::Medium),
            "low" => Ok(Priority::Low),
            "none" => Ok(Priority::None),
            _ => Err(format!("unknown priority: {}", s)),
        }
    }
}

/// A semantic scope that a work item or annotation is anchored to.
///
/// Unlike line numbers, these reference semantic identities in the graph,
/// so anchors are designed to stay attached across renames, moves, and
/// formatting changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum WorkScope {
    Entity(EntityId),
    Contract(ContractId),
    Artifact(FilePathId),
    Change(SemanticChangeId),
}

impl std::fmt::Display for WorkScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkScope::Entity(id) => write!(f, "entity:{}", id),
            WorkScope::Contract(id) => write!(f, "contract:{}", id),
            WorkScope::Artifact(id) => write!(f, "artifact:{}", id),
            WorkScope::Change(id) => write!(f, "change:{}", id),
        }
    }
}

/// A reference to an external system (Jira, GitHub Issues, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct ExternalRef {
    pub system: String,
    pub identifier: String,
    pub url: Option<String>,
}

/// A lightweight identity reference for authorship.
///
/// Phase 8 uses this as a placeholder. Phase 10 hardens it into
/// the full `Actor` / `Delegation` / `Approval` model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct IdentityRef {
    pub name: String,
    pub kind: IdentityKind,
}

impl IdentityRef {
    pub fn human(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: IdentityKind::Human,
        }
    }

    pub fn assistant(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: IdentityKind::Assistant,
        }
    }
}

/// Whether an identity is a human or an assistant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum IdentityKind {
    Human,
    Assistant,
}

/// A fingerprint used to detect when an annotation's anchor has drifted.
///
/// Stored at annotation creation time and compared against the current
/// entity fingerprint to determine staleness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SemanticAnchor {
    pub ast_hash: Hash256,
    pub signature_hash: Hash256,
}

/// The canonical work item: a feature, task, issue, debt item, or TODO
/// anchored to semantic scopes in the code graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WorkItem {
    pub work_id: WorkId,
    pub kind: WorkKind,
    pub title: String,
    pub description: String,
    pub status: WorkStatus,
    pub priority: Priority,
    pub scopes: Vec<WorkScope>,
    pub acceptance_criteria: Vec<String>,
    pub external_refs: Vec<ExternalRef>,
    pub created_by: IdentityRef,
    pub created_at: Timestamp,
}

impl WorkItem {
    /// Returns true if this work item is in a terminal state.
    pub fn is_closed(&self) -> bool {
        matches!(
            self.status,
            WorkStatus::Done | WorkStatus::Verified | WorkStatus::Archived
        )
    }
}

// ---------------------------------------------------------------------------
// Annotation
// ---------------------------------------------------------------------------

/// The kind of annotation attached to a semantic scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum AnnotationKind {
    Comment,
    Warning,
    Instruction,
    Reasoning,
}

impl std::fmt::Display for AnnotationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnnotationKind::Comment => write!(f, "comment"),
            AnnotationKind::Warning => write!(f, "warning"),
            AnnotationKind::Instruction => write!(f, "instruction"),
            AnnotationKind::Reasoning => write!(f, "reasoning"),
        }
    }
}

impl std::str::FromStr for AnnotationKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "comment" => Ok(AnnotationKind::Comment),
            "warning" => Ok(AnnotationKind::Warning),
            "instruction" => Ok(AnnotationKind::Instruction),
            "reasoning" => Ok(AnnotationKind::Reasoning),
            _ => Err(format!("unknown annotation kind: {}", s)),
        }
    }
}

/// How fresh an annotation's anchor is relative to current code state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum StalenessState {
    /// The anchored fingerprint matches the current entity state.
    #[default]
    Fresh,
    /// The entity has changed but not structurally — annotation may still apply.
    Suspect,
    /// The entity's signature or behavior has changed materially.
    Stale,
}

impl std::fmt::Display for StalenessState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StalenessState::Fresh => write!(f, "fresh"),
            StalenessState::Suspect => write!(f, "suspect"),
            StalenessState::Stale => write!(f, "stale"),
        }
    }
}

/// A living annotation attached to one or more semantic scopes.
///
/// Annotations are anchored to entity/contract/artifact identities, not
/// line numbers, so they are designed to stay attached across renames and
/// moves. When the anchored entity's fingerprint drifts, staleness is
/// detected.
///
/// Persisted through positional MessagePack, so a field is only ever
/// appended. `review` is left out of the encoding when absent, which keeps
/// every annotation that is not a review flag at the bytes, and so the
/// digest, it always had.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Annotation {
    pub annotation_id: AnnotationId,
    pub kind: AnnotationKind,
    pub body: String,
    pub scopes: Vec<WorkScope>,
    pub anchored_fingerprint: Option<SemanticAnchor>,
    pub authored_by: IdentityRef,
    pub created_at: Timestamp,
    pub staleness: StalenessState,
    /// Present when a reviewer set this annotation as a flag on the entity it
    /// is scoped to. The annotation's `body` is the reviewer's note,
    /// `authored_by` the reviewer and `created_at` when the flag was set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewFlag>,
}

impl Annotation {
    /// The review flag this annotation carries, while it is still open.
    pub fn open_review_flag(&self) -> Option<&ReviewFlag> {
        self.review
            .as_ref()
            .filter(|flag| flag.resolution.is_none())
    }

    /// The entity a review flag is on: the one entity it is scoped to.
    pub fn flagged_entity(&self) -> Option<EntityId> {
        self.review.as_ref()?;
        self.scopes.iter().find_map(|scope| match scope {
            WorkScope::Entity(entity_id) => Some(*entity_id),
            _ => None,
        })
    }
}

// ---------------------------------------------------------------------------
// Review flags
// ---------------------------------------------------------------------------

/// Agent sessions one review flag remembers having reached. Past it, the
/// oldest sighting goes.
pub const REVIEW_FLAG_SIGHTINGS_MAX: usize = 32;

/// A reviewer's flag on one entity, as the annotation that holds it carries
/// it.
///
/// A flag is scoped to an entity and to the exact version the reviewer was
/// looking at. It stays open until the reviewer closes it, whatever becomes
/// of the entity in between: committing the entity does not close it, and an
/// entity that changes afterwards is told apart by its fingerprint no longer
/// being the flagged one. A flag is advisory and stops nothing.
///
/// Positional like the annotation that holds it, so a field is only ever
/// appended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewFlag {
    /// The fingerprint of the entity's syntax as the reviewer saw it.
    pub flagged_ast: Hash256,
    /// The entity's name when it was flagged, which is what a reader is shown
    /// once the graph no longer holds the entity.
    pub flagged_name: String,
    /// The request that set the flag, so the same request sets it once.
    pub operation_id: OperationId,
    /// Who closed the flag and when. Absent while it is open.
    pub resolution: Option<ReviewFlagResolution>,
    /// The agent sessions an answer from Kin handed this flag to, oldest
    /// first, each session once.
    pub seen_by: Vec<ReviewFlagSighting>,
    /// Literal-sensitive fingerprint of the version the reviewer saw. Older
    /// flags without this field continue to use their syntax fingerprint.
    // Kept as an explicit null when absent: a later source digest must not
    // slide into this position in MessagePack.
    #[serde(default)]
    pub flagged_behavior: Option<Hash256>,
    /// Exact reviewed entity span digest, including comments and literals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flagged_source: Option<Hash256>,
}

impl ReviewFlag {
    /// Record that `sighting`'s session was handed this flag. A session
    /// already recorded is left as it was; returns whether anything changed.
    pub fn record_sighting(&mut self, sighting: ReviewFlagSighting) -> bool {
        if self
            .seen_by
            .iter()
            .any(|known| known.session == sighting.session)
        {
            return false;
        }
        self.seen_by.push(sighting);
        if self.seen_by.len() > REVIEW_FLAG_SIGHTINGS_MAX {
            let excess = self.seen_by.len() - REVIEW_FLAG_SIGHTINGS_MAX;
            self.seen_by.drain(..excess);
        }
        true
    }
}

/// Who closed a review flag, and when.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewFlagResolution {
    pub by: IdentityRef,
    pub at: Timestamp,
    /// The request that closed the flag.
    pub operation_id: OperationId,
}

/// One agent session a review flag reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewFlagSighting {
    /// The name the session's client gave itself.
    pub agent: String,
    pub session: SessionId,
    /// When Kin first handed the session an answer carrying the flag.
    pub at: Timestamp,
}

// ---------------------------------------------------------------------------
// Graph Relationship Types
// ---------------------------------------------------------------------------

/// The types of relationships in the work graph.
///
/// These correspond to the graph edges defined in PLAN_P3.md Section 4.5:
/// - `(WorkItem)-[:AFFECTS]->(Entity | Contract | Artifact | SemanticChange)`
/// - `(WorkItem)-[:DECOMPOSES_TO]->(WorkItem)`
/// - `(WorkItem)-[:BLOCKED_BY]->(WorkItem)`
/// - `(Entity | Contract | Artifact)-[:IMPLEMENTS]->(WorkItem)`
/// - `(Annotation)-[:ATTACHED_TO]->(Entity | Contract | Artifact | WorkItem)`
/// - `(Annotation)-[:SUPERSEDES]->(Annotation)`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum WorkLink {
    /// Work item affects a semantic scope.
    Affects { work_id: WorkId, scope: WorkScope },
    /// Work item decomposes into a child work item (feature -> task -> subtask).
    DecomposesTo { parent: WorkId, child: WorkId },
    /// Work item is blocked by another work item.
    BlockedBy { blocked: WorkId, blocker: WorkId },
    /// A semantic scope implements a work item.
    Implements { scope: WorkScope, work_id: WorkId },
    /// An annotation is attached to a scope or work item.
    AttachedTo {
        annotation_id: AnnotationId,
        target: AnnotationTarget,
    },
    /// An annotation supersedes another annotation.
    Supersedes {
        new_id: AnnotationId,
        old_id: AnnotationId,
    },
}

/// What an annotation is attached to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum AnnotationTarget {
    Scope(WorkScope),
    Work(WorkId),
}

impl std::fmt::Display for AnnotationTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnnotationTarget::Scope(s) => write!(f, "{}", s),
            AnnotationTarget::Work(id) => write!(f, "work:{}", id),
        }
    }
}

// ---------------------------------------------------------------------------
// Work Filter (for queries)
// ---------------------------------------------------------------------------

/// Filter for querying work items.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct WorkFilter {
    pub kinds: Option<Vec<WorkKind>>,
    pub statuses: Option<Vec<WorkStatus>>,
    pub scope: Option<WorkScope>,
}

/// Filter for querying annotations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnnotationFilter {
    pub kinds: Option<Vec<AnnotationKind>>,
    pub scopes: Option<Vec<WorkScope>>,
    pub include_stale: bool,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_kind_roundtrip() {
        for kind in [
            WorkKind::Feature,
            WorkKind::Task,
            WorkKind::Issue,
            WorkKind::Debt,
            WorkKind::Todo,
            WorkKind::Investigation,
        ] {
            let s = kind.to_string();
            let parsed: WorkKind = s.parse().unwrap();
            assert_eq!(kind, parsed);
        }
    }

    #[test]
    fn work_status_roundtrip() {
        for status in [
            WorkStatus::Proposed,
            WorkStatus::Planned,
            WorkStatus::InProgress,
            WorkStatus::Blocked,
            WorkStatus::Done,
            WorkStatus::Verified,
            WorkStatus::Archived,
        ] {
            let s = status.to_string();
            let parsed: WorkStatus = s.parse().unwrap();
            assert_eq!(status, parsed);
        }
    }

    #[test]
    fn priority_roundtrip() {
        for priority in [
            Priority::Critical,
            Priority::High,
            Priority::Medium,
            Priority::Low,
            Priority::None,
        ] {
            let s = priority.to_string();
            let parsed: Priority = s.parse().unwrap();
            assert_eq!(priority, parsed);
        }
    }

    #[test]
    fn annotation_kind_roundtrip() {
        for kind in [
            AnnotationKind::Comment,
            AnnotationKind::Warning,
            AnnotationKind::Instruction,
            AnnotationKind::Reasoning,
        ] {
            let s = kind.to_string();
            let parsed: AnnotationKind = s.parse().unwrap();
            assert_eq!(kind, parsed);
        }
    }

    #[test]
    fn work_scope_display() {
        let entity_scope = WorkScope::Entity(EntityId::new());
        assert!(entity_scope.to_string().starts_with("entity:"));

        let file_scope = WorkScope::Artifact(FilePathId::new("src/main.rs"));
        assert_eq!(file_scope.to_string(), "artifact:src/main.rs");
    }

    #[test]
    fn work_item_is_closed() {
        let make = |status| WorkItem {
            work_id: WorkId::new(),
            kind: WorkKind::Feature,
            title: "test".into(),
            description: String::new(),
            status,
            priority: Priority::None,
            scopes: vec![],
            acceptance_criteria: vec![],
            external_refs: vec![],
            created_by: IdentityRef::human("test"),
            created_at: Timestamp::now(),
        };

        assert!(!make(WorkStatus::Proposed).is_closed());
        assert!(!make(WorkStatus::InProgress).is_closed());
        assert!(make(WorkStatus::Done).is_closed());
        assert!(make(WorkStatus::Verified).is_closed());
        assert!(make(WorkStatus::Archived).is_closed());
    }

    #[test]
    fn identity_ref_constructors() {
        let h = IdentityRef::human("alice");
        assert_eq!(h.kind, IdentityKind::Human);
        assert_eq!(h.name, "alice");

        let a = IdentityRef::assistant("claude");
        assert_eq!(a.kind, IdentityKind::Assistant);
    }

    #[test]
    fn work_item_serialization() {
        let item = WorkItem {
            work_id: WorkId::new(),
            kind: WorkKind::Feature,
            title: "Stripe checkout".into(),
            description: "Add payment processing".into(),
            status: WorkStatus::Proposed,
            priority: Priority::High,
            scopes: vec![WorkScope::Artifact(FilePathId::new("src/checkout.rs"))],
            acceptance_criteria: vec!["Process payments".into()],
            external_refs: vec![ExternalRef {
                system: "jira".into(),
                identifier: "PAY-123".into(),
                url: Some("https://jira.example.com/PAY-123".into()),
            }],
            created_by: IdentityRef::human("alice"),
            created_at: Timestamp::now(),
        };

        let json = serde_json::to_string(&item).unwrap();
        let parsed: WorkItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.work_id, item.work_id);
        assert_eq!(parsed.kind, item.kind);
        assert_eq!(parsed.title, item.title);
    }

    #[test]
    fn annotation_serialization() {
        let ann = Annotation {
            annotation_id: AnnotationId::new(),
            kind: AnnotationKind::Instruction,
            body: "Never call external tax API in checkout path".into(),
            scopes: vec![WorkScope::Entity(EntityId::new())],
            anchored_fingerprint: Some(SemanticAnchor {
                ast_hash: Hash256::from_bytes([0xaa; 32]),
                signature_hash: Hash256::from_bytes([0xbb; 32]),
            }),
            authored_by: IdentityRef::human("alice"),
            created_at: Timestamp::now(),
            staleness: StalenessState::Fresh,
            review: None,
        };

        let json = serde_json::to_string(&ann).unwrap();
        let parsed: Annotation = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.annotation_id, ann.annotation_id);
        assert_eq!(parsed.kind, ann.kind);
        assert_eq!(parsed.staleness, StalenessState::Fresh);
    }

    #[test]
    fn staleness_default_is_fresh() {
        assert_eq!(StalenessState::default(), StalenessState::Fresh);
    }

    /// The annotation as every store written before review flags holds it.
    #[derive(Serialize, Deserialize)]
    struct AnnotationBeforeReviewFlags {
        annotation_id: AnnotationId,
        kind: AnnotationKind,
        body: String,
        scopes: Vec<WorkScope>,
        anchored_fingerprint: Option<SemanticAnchor>,
        authored_by: IdentityRef,
        created_at: Timestamp,
        staleness: StalenessState,
    }

    fn flagged_annotation(entity_id: EntityId) -> Annotation {
        Annotation {
            annotation_id: AnnotationId::new(),
            kind: AnnotationKind::Warning,
            body: "a second send must fail loudly".into(),
            scopes: vec![WorkScope::Entity(entity_id)],
            anchored_fingerprint: Some(SemanticAnchor {
                ast_hash: Hash256::from_bytes([0xaa; 32]),
                signature_hash: Hash256::from_bytes([0xbb; 32]),
            }),
            authored_by: IdentityRef::human("ada"),
            created_at: Timestamp::now(),
            staleness: StalenessState::Fresh,
            review: Some(ReviewFlag {
                flagged_ast: Hash256::from_bytes([0xaa; 32]),
                flagged_behavior: Some(Hash256::from_bytes([0xbb; 32])),
                flagged_source: Some(Hash256::from_bytes([0xcc; 32])),
                flagged_name: "ServerReply.send".into(),
                operation_id: OperationId::new(),
                resolution: None,
                seen_by: Vec::new(),
            }),
        }
    }

    /// An annotation that is not a review flag keeps the bytes it always had,
    /// in the positional encoding the store hashes and in JSON, and a store
    /// written before review flags reads back with none.
    #[test]
    fn an_annotation_without_a_review_flag_keeps_its_stored_bytes() {
        let mut plain = flagged_annotation(EntityId::new());
        plain.review = None;
        let before = AnnotationBeforeReviewFlags {
            annotation_id: plain.annotation_id,
            kind: plain.kind,
            body: plain.body.clone(),
            scopes: plain.scopes.clone(),
            anchored_fingerprint: plain.anchored_fingerprint.clone(),
            authored_by: plain.authored_by.clone(),
            created_at: plain.created_at.clone(),
            staleness: plain.staleness,
        };
        let stored = rmp_serde::to_vec(&before).unwrap();
        assert_eq!(rmp_serde::to_vec(&plain).unwrap(), stored);
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            serde_json::to_string(&before).unwrap()
        );
        let read: Annotation = rmp_serde::from_slice(&stored).unwrap();
        assert_eq!(read, plain);
        let read: Annotation =
            serde_json::from_str(&serde_json::to_string(&before).unwrap()).unwrap();
        assert_eq!(read, plain);
    }

    #[test]
    fn a_review_flag_survives_both_encodings_and_names_its_entity() {
        let entity_id = EntityId::new();
        let mut flagged = flagged_annotation(entity_id);
        let flag = flagged.review.as_mut().unwrap();
        flag.record_sighting(ReviewFlagSighting {
            agent: "codex".into(),
            session: SessionId::new(),
            at: Timestamp::now(),
        });
        let read: Annotation =
            rmp_serde::from_slice(&rmp_serde::to_vec(&flagged).unwrap()).unwrap();
        assert_eq!(read, flagged);
        let read: Annotation =
            serde_json::from_str(&serde_json::to_string(&flagged).unwrap()).unwrap();
        assert_eq!(read, flagged);
        assert_eq!(flagged.flagged_entity(), Some(entity_id));
        assert!(flagged.open_review_flag().is_some());

        flagged.review.as_mut().unwrap().resolution = Some(ReviewFlagResolution {
            by: IdentityRef::human("ada"),
            at: Timestamp::now(),
            operation_id: OperationId::new(),
        });
        assert!(flagged.open_review_flag().is_none());
        assert_eq!(flagged.flagged_entity(), Some(entity_id));

        let mut plain = flagged;
        plain.review = None;
        assert_eq!(plain.flagged_entity(), None);
    }

    #[test]
    fn exact_source_flag_encoding_keeps_the_behavior_slot_when_absent() {
        let mut annotation = flagged_annotation(EntityId::new());
        let flag = annotation.review.as_mut().unwrap();
        flag.flagged_behavior = None;
        let expected_source = flag.flagged_source;
        let read: Annotation =
            rmp_serde::from_slice(&rmp_serde::to_vec(&annotation).unwrap()).unwrap();
        assert_eq!(read.review.as_ref().unwrap().flagged_behavior, None);
        assert_eq!(
            read.review.as_ref().unwrap().flagged_source,
            expected_source
        );
        assert_eq!(read, annotation);
    }

    #[test]
    fn a_review_flag_records_each_session_once_and_keeps_the_newest() {
        let mut flagged = flagged_annotation(EntityId::new());
        let flag = flagged.review.as_mut().unwrap();
        let first = SessionId::new();
        let sighting = |session| ReviewFlagSighting {
            agent: "agent".into(),
            session,
            at: Timestamp::now(),
        };
        assert!(flag.record_sighting(sighting(first)));
        assert!(!flag.record_sighting(sighting(first)));
        assert_eq!(flag.seen_by.len(), 1);
        for _ in 0..REVIEW_FLAG_SIGHTINGS_MAX {
            assert!(flag.record_sighting(sighting(SessionId::new())));
        }
        assert_eq!(flag.seen_by.len(), REVIEW_FLAG_SIGHTINGS_MAX);
        assert!(flag.seen_by.iter().all(|seen| seen.session != first));
    }

    #[test]
    fn work_id_serialization() {
        let id = WorkId::new();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: WorkId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }
}
