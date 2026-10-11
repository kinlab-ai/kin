// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! `kin review guide`: one recorded change as a walkthrough.
//!
//! A review answers what changed and what it puts at risk. A guide answers the
//! question a reader has before either: where to start, and what to read next.
//! It orders the change the way the graph connects it rather than the way a
//! tree lists it, and it shows each step with the record that supports it.
//!
//! Everything here is derived from recorded state. The chapters are the
//! connected groups of changed entities, read off the relations committed at
//! the change. The order inside a chapter follows those relations, so what
//! others rely on comes before what relies on it. The before and after text of
//! an entity is its own span at the base change and at the change itself, read
//! through the one source authority every other body-serving surface uses.
//! Nothing is summarized by a model, so the same change always reads the same
//! way, and every sentence names the record it stands on.
//!
//! The unit is the entity. A guide never serves a file, and an entity whose
//! source is a whole file's module surface says so rather than quoting it.
//!
//! A guide is bounded. A change that touches more entities than
//! [`STEPS_WALKED_MAX`] walks the ones a reviewer should see first and counts
//! the rest in `not_walked`, because an import of twenty thousand entities is
//! not something anyone reads step by step, and pretending otherwise would
//! cost a history replay per entity to produce a page nobody finishes.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use anyhow::Result;
use kin_mcp::handlers::common::{
    entity_body_gap_reason, entity_presentation_start_line, read_entity_source_exact_at,
    EntitySourceScope, HeldSourceAuthority,
};
use kin_model::entity::{Entity, EntityKind, EntityRole};
use kin_model::relation::{Relation, RelationKind};
use kin_model::review::RiskLevel;
use kin_model::{ChangeStore, EntityId, SemanticChange, SemanticChangeId};
use kin_review::impact::ImpactGraph;
use kin_review::{EntityChangeKind, GraphAtRef};

use super::repository_authority::RequestRepositoryAuthority;
use serde::{Deserialize, Serialize};

pub const GUIDE_SCHEMA: &str = "kin.review.guide.v1";

/// What a working guide answers as its change id and reads "after" at. It is
/// the selector `kin diff` uses for the same thing: the working tree, which no
/// change holds yet.
pub const WORKING: &str = "WORKSPACE";

/// How many of a range's changes a guide lists by name.
const RANGE_CHANGES_LISTED_MAX: usize = 50;
/// How many change ids a range carries for membership.
const RANGE_MEMBERS_MAX: usize = 2_000;

/// How many changed entities one guide walks with bodies and connections.
pub const STEPS_WALKED_MAX: usize = 60;

/// How many containers changed only through their members a guide names.
const THROUGH_MEMBERS_LISTED_MAX: usize = 12;

/// How many changes one count request may ask about: a page of history.
pub const COUNTS_ASKED_MAX: usize = 500;

/// The largest entity body a guide inlines for one side of a step.
const BODY_MAX_BYTES: usize = 96 * 1024;

/// How many connections of one kind a step lists by name. The totals beside
/// them are exact, so a bounded list never reads as the whole set.
const CONNECTIONS_LISTED_MAX: usize = 24;

/// How many relation changes a step lists by name.
const RELATION_CHANGES_LISTED_MAX: usize = 12;

/// How many stored reviews of one change a guide lists, newest first.
const REVIEWS_LISTED_MAX: usize = 20;

/// How many of one review's decisions and notes a guide lists: the latest.
const REVIEW_EVENTS_LISTED_MAX: usize = 200;

/// The longest comment or note a guide carries whole, in characters. The page
/// records none longer, and a terminal or an agent tool can.
const REVIEW_TEXT_MAX_CHARS: usize = 10_000;

/// The longest name, title, ref or scope a guide carries for a review or one
/// of its entries, in characters. These are short by nature, and nothing stops
/// a caller from storing a long one on every entry of a long history, so the
/// text budget below would bound the bodies and leave the labels free to grow.
const REVIEW_LABEL_MAX_CHARS: usize = 200;

/// How much comment and note text one guide carries in all, in characters.
/// The limits above bound each review, entry and text on its own, and their
/// product is still tens of millions of characters. This is the bound on the
/// whole: the newest entries of the newest reviews keep their text first.
const REVIEW_TEXT_BUDGET_CHARS: usize = 200_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewGuide {
    pub schema: String,
    pub change: GuideChange,
    pub summary: GuideSummary,
    pub chapters: Vec<GuideChapter>,
    pub findings: GuideFindings,
    /// Changed entities this guide counted and did not walk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_walked: Option<GuideNotWalked>,
    /// The stored reviews of this change, newest first, each with what was
    /// recorded against it in the order it was recorded. Empty when nobody has
    /// recorded one.
    #[serde(default)]
    pub reviews: Vec<GuideReview>,
    /// How many reviews the change holds. More than `reviews` lists when the
    /// oldest were left out.
    #[serde(default)]
    pub reviews_total: usize,
    /// What the guide walks: `change` for one recorded change, `working` for
    /// what the working tree holds beyond the last recorded change, `range`
    /// for everything one ref reaches that another does not.
    #[serde(default = "change_kind")]
    pub kind: String,
    /// The two refs a `range` guide was asked for and what lies between them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<GuideRange>,
    /// For a `working` guide, how the working tree stands against the last
    /// recorded change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_tree: Option<GuideWorkingTree>,
}

/// How the working tree stands against the last recorded change.
///
/// A guide speaks in entities. The working tree can still hold work no
/// entity shows yet, and a guide that walked nothing would then read as a
/// clean tree, so whether there is any work at all is carried apart from the
/// entities that changed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuideWorkingTree {
    /// Whether the working tree holds anything the last recorded change does
    /// not, whether or not an entity changed with it.
    pub differs: bool,
    /// Whether what changed was derived into entities. When the live graph
    /// could not be read, only that something differs is known.
    pub derived: bool,
    /// Whether the graph held the working tree's own state when it was read.
    pub caught_up: bool,
    /// Set when nothing measured the working copy before it was read, with the
    /// reason. The guide is then about what was last admitted, which can be a
    /// moment behind the working tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unmeasured: Option<String>,
}

fn change_kind() -> String {
    "change".to_string()
}

/// The range a guide walks: what `head_ref` reaches that `base_ref` does not.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideRange {
    pub base_ref: String,
    pub head_ref: String,
    /// The change both refs share, which "before" is read at.
    pub merge_base: String,
    /// How many changes the range holds.
    pub changes_total: usize,
    /// The newest of them, newest first.
    pub changes: Vec<GuideRangeChange>,
    /// Every change the range holds, by id, so a reader can tell whether a
    /// change it sees elsewhere belongs to the range. Up to
    /// `RANGE_MEMBERS_MAX`; `members_complete` says whether that was all.
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub members_complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideRangeChange {
    pub id: String,
    pub title: String,
    pub author: String,
    pub timestamp: String,
}

/// The recorded changes that changed one entity, for a reader who wants to
/// know how it got here and whether it moved again afterwards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideHistory {
    pub entity_id: String,
    /// The changes that changed the entity itself, newest first.
    pub revisions: Vec<GuideRangeChange>,
    /// How many there are. More than `revisions` lists when the oldest were
    /// left out.
    pub revisions_total: usize,
    /// Revisions of the entity's file that left the entity as it was. They
    /// are counted and not listed: they are what the file did.
    pub unchanged_revisions: usize,
    /// The change that removed the entity, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_by: Option<GuideRangeChange>,
    /// Asked with a range's base: the listed revisions that belong to the
    /// range, by id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub in_range: Vec<String>,
}

/// Where each reviewed change stands, for a list of changes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuideStanding {
    pub standing: BTreeMap<String, GuideStandingEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideStandingEntry {
    /// The state of the newest stored review of the change.
    pub state: String,
    /// How many stored reviews name the change.
    pub reviews: usize,
}

/// One stored review of the change and its history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuideReview {
    pub review_id: String,
    pub title: String,
    /// Where the review stands, in the spelling the JSON review reads use.
    pub state: String,
    pub base_ref: String,
    pub head_ref: String,
    /// Decisions and notes together, oldest first. A reader follows them as
    /// one history: a concern noted before an approval reads before it.
    pub events: Vec<GuideReviewEvent>,
    /// How many the review holds. More than `events` lists when the earliest
    /// were left out.
    pub events_total: usize,
}

/// One decision or note in a review's history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuideReviewEvent {
    /// `decision` or `note`.
    pub kind: String,
    pub at: String,
    pub by: String,
    /// The decision made. Absent on a note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// A decision's comment or a note's body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// What a note is about. Absent when it is about the change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Whether `text` is the start of a longer one, cut at the guide's limit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub text_cut: bool,
}

/// What the page rereads after it records something.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuideReviews {
    pub reviews: Vec<GuideReview>,
    pub reviews_total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideChange {
    pub id: String,
    /// The change this guide reads "before" at. `None` for a change with no
    /// parent, where everything it holds is an addition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    pub parents: Vec<String>,
    /// The first line of the recorded message.
    pub title: String,
    pub message: String,
    /// Who made the change, as a reader names them: the person's name, or
    /// the address when the record holds no name.
    pub author: String,
    /// The author exactly as the change records it, when that says more than
    /// `author`: the address of a person recorded as `Name <address>`, and, in
    /// a store an earlier import wrote, an imported commit's whole signature
    /// with the seconds since 1970 and the offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_recorded: Option<String>,
    pub timestamp: String,
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideSummary {
    pub risk: String,
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
    /// Relations this change added, removed, or changed beyond where their
    /// evidence sits in the source.
    pub relation_changes: usize,
    /// Recorded relation modifications that moved only where their evidence
    /// sits, because a line above it was added or removed. They are counted
    /// apart from `relation_changes` and not listed on any step.
    #[serde(default)]
    pub relation_changes_position_only: usize,
    /// Recorded modifications that moved only a span or its source provenance.
    /// The review excludes them, and the count says how many it excluded.
    pub provenance_only_entity_changes: usize,
    /// Changed module containers. A file's module surface has no text of its
    /// own to show, so it is counted here and read through its members, which
    /// are the steps.
    #[serde(default)]
    pub module_containers: usize,
    /// Changed containers, such as a class, whose own text held still while
    /// members inside them changed. A class's recorded text holds its
    /// methods, so editing one method moves the class too. Such a container
    /// is counted here and read through its members, which are the steps.
    #[serde(default)]
    pub containers_through_members: usize,
    /// Their names, up to `THROUGH_MEMBERS_LISTED_MAX`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub through_members: Vec<String>,
    /// Distinct entities outside this change that reach an entity it changed.
    /// Declarations of one callable, such as `@overload` stubs beside their
    /// implementation, count once.
    pub recorded_consumers: usize,
    /// Distinct recorded tests that reach an entity it changed.
    pub recorded_tests: usize,
    /// Of `recorded_consumers`, the ones a step's `consumer_verdict` says
    /// were `not_updated`: they reach a changed entity whose signature
    /// changed, that stopped being public or was removed, or whose recorded
    /// contract changed, with an error-level finding to say it breaks them,
    /// and they did not change with it.
    #[serde(default)]
    pub consumers_not_updated: usize,
    /// Of `recorded_consumers`, the ones that reach a changed entity whose
    /// verdict is `may_need_review`, such as a body that changed under a
    /// signature that held, and are not counted in `consumers_not_updated`.
    #[serde(default)]
    pub consumers_may_need_review: usize,
    /// Of `recorded_consumers`, the ones that reach only changed entities
    /// whose verdict is `needs_no_update`: their signature held and their
    /// code reads or behaves as it did, as when only a comment, the layout or
    /// a docstring changed. The three counts add up to `recorded_consumers`.
    #[serde(default)]
    pub consumers_need_no_update: usize,
    pub finding_count: usize,
    pub completeness: GuideCompleteness,
    /// One plain sentence a reader can take away without opening a step.
    pub narration: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideCompleteness {
    /// `not_settled`, `no_recorded_debt` or `unavailable`. No state here
    /// claims the review is complete, because the review never proves that.
    pub state: String,
    pub message: String,
    /// While `not_settled`: how many entities Kin is still mapping, so a
    /// reader can be told the counts may grow by how much work remains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_entities: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideChapter {
    pub title: String,
    /// `connected` for entities the graph ties together, `single` for one
    /// entity with its own consumers, `group` for the unconnected remainder.
    pub kind: String,
    pub reason: String,
    pub risk: String,
    pub steps: Vec<GuideStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideStep {
    pub entity_id: String,
    pub name: String,
    pub kind: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// What contains the entity, as its kind and name: the module a function
    /// is in, the class a method belongs to. This is how a guide says where
    /// an entity is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within: Option<String>,
    /// Where the checkout holds the entity, for handing to an editor. A
    /// guide never shows it: it is a projection's coordinate, not the
    /// entity's identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// The line an editor shows for the entity's first line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// The step's parameters, when another step of the same walk carries the
    /// same name and only its parameters tell the two apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<String>,
    /// `added`, `modified` or `removed`.
    pub change: String,
    pub risk: String,
    pub narration: String,
    /// The recorded fields that moved, each with its two values where the
    /// graph holds values. A body names the field and carries no values here,
    /// because its text is `before` and `after`.
    pub fields: Vec<GuideFieldChange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<GuideBody>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<GuideBody>,
    pub connections: GuideConnections,
    /// What this step's change asks of the recorded consumers that reach it
    /// and did not change with it. Absent only in a guide written before it
    /// was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_verdict: Option<GuideConsumerVerdict>,
    pub findings: Vec<GuideFinding>,
    pub evidence: GuideEvidence,
}

/// What one changed entity's change asks of the recorded consumers that reach
/// it from outside the change.
///
/// Read from the entity's two records: its declaration, and the graph's
/// fingerprint of its tokens, which skips comments and whitespace, and of its
/// behaviour where its language records one. Text that differs only by a
/// comment is not a change a consumer has to follow, so a comment written
/// inside a parameter list asks nothing of anyone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideConsumerVerdict {
    /// `needs_no_update` when the signature held and the code reads or
    /// behaves as it did, `may_need_review` when the body changed under a
    /// signature that held or the entity is new, `not_updated` when the
    /// change breaks what consumers outside it rely on and they did not
    /// change with it, and `updated` when it would have and nothing outside
    /// the change reaches it.
    pub verdict: String,
    /// Why: `comments_or_layout_only`, `behavior_equivalent`,
    /// `visibility_widened`, `moved`, `body_changed`, `visibility_reduced`,
    /// `signature_changed`, `contract_changed`, `added` or `removed`.
    pub reason: String,
    /// How many recorded consumers outside the change the verdict is about.
    pub consumers: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideFieldChange {
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideBody {
    /// `recorded` when `text` is the entity's span at `change_id`,
    /// `no_source` when the entity has no body of its own to show, and
    /// `unavailable` when the read was refused, with the refusal in `reason`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    /// Whether the text is the working tree's and not a recorded change's.
    /// `change_id` is then absent: no change holds this text yet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub working: bool,
    /// The content address of the source the span was cut from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_blob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuideConnections {
    /// Entities that reach this one, tests excluded.
    pub consumers: Vec<GuideConnection>,
    pub consumers_total: usize,
    /// How many of those consumers this change also changed.
    pub consumers_changed_here: usize,
    pub tests: Vec<GuideConnection>,
    pub tests_total: usize,
    /// Modules whose only recorded reach is importing this entity. Listed
    /// apart, because the functions inside them that use it are the consumers.
    #[serde(default)]
    pub importing_modules: Vec<GuideEntityRef>,
    #[serde(default)]
    pub importing_modules_total: usize,
    /// Relations this change added, removed or modified at this entity.
    pub relation_changes: Vec<GuideRelationChange>,
    pub relation_changes_total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideConnection {
    pub relation_id: String,
    pub relation: String,
    pub origin: String,
    pub confidence: f32,
    /// The change the relation was recorded in, when the graph holds one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_in: Option<String>,
    pub entity: GuideEntityRef,
    /// Whether this change also changed the connected entity.
    pub changed_here: bool,
    /// Other declarations of the same callable this row stands for: the
    /// `@overload` stubs, prototypes or ambient declarations its language
    /// adapter says only declare what `entity` defines. A reader sees one
    /// callable, so the list shows one row.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub overloads: usize,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideEntityRef {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub role: String,
    /// What contains the entity, as its kind and name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within: Option<String>,
    /// The entity's parameters, when another row of the same list carries
    /// the same name in the same container and only its parameters tell the
    /// two apart, as two overloads that each define a body do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<String>,
    /// Where the checkout holds the entity, for handing to an editor. Never
    /// shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideRelationChange {
    /// `added`, `removed` or `modified`.
    pub change: String,
    pub relation: String,
    /// `inbound` when the other end points at this entity, else `outbound`.
    pub direction: String,
    pub other: String,
    pub relation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideFinding {
    pub kind: String,
    pub severity: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuideEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_change: Option<String>,
    pub head_change: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint_before: Option<GuideFingerprint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint_after: Option<GuideFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideFingerprint {
    pub signature: String,
    pub behavior: String,
    pub ast: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuideFindings {
    pub breaking_changes: Vec<String>,
    pub contract_violations: Vec<String>,
    pub work_risks: Vec<String>,
    pub test_coverage_gaps: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideNotWalked {
    pub total: usize,
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
    pub message: String,
}

/// One entity's own text at one recorded change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideEntitySource {
    /// The entity as recorded at the change. Absent when the request named
    /// nothing recorded there, and `body` then says what was missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<GuideEntityRef>,
    pub body: GuideBody,
}

/// How many entities a guide of each asked change walks, for a list of
/// changes that should say what the guide will.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuideCounts {
    pub counts: BTreeMap<String, GuideChangeCount>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideChangeCount {
    /// The entities its guide walks as steps.
    pub entities: usize,
    /// Changed module containers, read through their members.
    pub module_containers: usize,
    /// Changed containers whose own text held still, read through their members.
    pub containers_through_members: usize,
    /// Set for a merge. Its guide walks what it brought into its first
    /// parent's line, which its own record does not hold.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub merge: bool,
    /// Set for a merge whose count was read the way its guide reads it,
    /// from what it brought into its first parent's line. A merge without it
    /// has no count here, and an older answer never sets it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub merge_counted: bool,
}

/// One changed entity before it is ordered into a chapter.
struct Draft<'a> {
    id: EntityId,
    change: &'static str,
    old: Option<&'a Entity>,
    new: Option<&'a Entity>,
    risk: Option<RiskLevel>,
}

impl<'a> Draft<'a> {
    /// The record a reader recognizes the entity by: its head side when it
    /// still exists, its base side when this change removed it.
    fn subject(&self) -> Option<&'a Entity> {
        self.new.or(self.old)
    }

    fn name(&self) -> String {
        self.subject()
            .map_or_else(|| self.id.to_string(), |entity| entity.name.clone())
    }

    fn is_test(&self) -> bool {
        self.subject().is_some_and(is_test_entity)
    }
}

fn is_test_entity(entity: &Entity) -> bool {
    entity.role == EntityRole::Test || entity.kind == EntityKind::Test
}

/// `UsesType` as `uses_type`, so every enum a guide names reads one way
/// whatever its own serialization spells.
fn label(value: impl std::fmt::Debug) -> String {
    let debug = format!("{value:?}");
    let mut out = String::with_capacity(debug.len() + 4);
    for (index, character) in debug.chars().enumerate() {
        if character.is_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.extend(character.to_lowercase());
        } else {
            out.push(character);
        }
    }
    out
}

fn risk_label(level: RiskLevel) -> &'static str {
    match level {
        RiskLevel::Low => "low",
        RiskLevel::Medium => "medium",
        RiskLevel::High => "high",
        RiskLevel::Critical => "critical",
    }
}

fn risk_rank(level: Option<RiskLevel>) -> u8 {
    match level {
        Some(RiskLevel::Critical) => 4,
        Some(RiskLevel::High) => 3,
        Some(RiskLevel::Medium) => 2,
        Some(RiskLevel::Low) => 1,
        None => 0,
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

fn entity_ref(entity: &Entity) -> GuideEntityRef {
    GuideEntityRef {
        id: entity.id.to_string(),
        name: entity.name.clone(),
        kind: label(entity.kind),
        role: label(entity.role),
        within: None,
        parameters: None,
        file: entity_file(entity),
        line: entity_presentation_start_line(entity),
    }
}

/// What contains `id` in `graph`, as its kind and name: `module runs`,
/// `class Client`. An entity is placed by the entities around it, never by
/// where a projection writes it. When more than one relation claims it, the
/// one with the smallest id answers, so the same graph always says the same.
fn within_of<G: ImpactGraph + ?Sized>(graph: &G, id: &EntityId) -> Result<Option<String>> {
    let Some(container) = container_of(graph, id)? else {
        return Ok(None);
    };
    // A container recorded as a whole source, and not as a module or a type,
    // names nothing a reader would place an entity by.
    Ok(graph
        .get_entity(&container)?
        .filter(|entity| entity.kind != EntityKind::File)
        .map(|entity| format!("{} {}", label(entity.kind).replace('_', " "), entity.name)))
}

/// The entity that contains `id` in `graph`, by the `Contains` relation with
/// the smallest id, so the same graph always answers the same.
fn container_of<G: ImpactGraph + ?Sized>(graph: &G, id: &EntityId) -> Result<Option<EntityId>> {
    Ok(graph
        .get_all_relations_for_entity(id)?
        .into_iter()
        .filter(|relation| {
            relation.kind == RelationKind::Contains && relation.dst.as_entity() == Some(*id)
        })
        .filter_map(|relation| relation.src.as_entity().map(|source| (relation.id, source)))
        .min()
        .map(|(_, container)| container))
}

/// A recorded author as a reader names them.
///
/// The one reading every surface shares, [`kin_model::AuthorId::name`]: the
/// name of a person recorded as `Name <address>`, the address when the record
/// holds no name, and the label of an actor that is neither. Earlier imports
/// recorded an imported commit's whole signature,
/// `Name <address> 1540586649 +0200`, and that reads as the name too.
fn person(author: &str) -> String {
    kin_model::AuthorId::new(author).name().into_owned()
}

/// A recorded time as a terminal prints it: `2018-10-26 20:44 UTC`. A time
/// that does not parse is printed as it was recorded.
fn readable_time(timestamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(timestamp).map_or_else(
        |_| timestamp.to_string(),
        |time| {
            time.with_timezone(&chrono::Utc)
                .format("%Y-%m-%d %H:%M UTC")
                .to_string()
        },
    )
}

/// The parameter list a signature declares, `(self, value: int)`, read from
/// its first opening parenthesis to the one that closes it.
fn parameters_of(signature: &str) -> Option<String> {
    let start = signature.find('(')?;
    let mut depth = 0usize;
    for (offset, character) in signature[start..].char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(signature[start..=start + offset].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether a recorded relation modification moved only where its evidence
/// sits in the source, because a line above it was added or removed.
///
/// Normalized onto the old record and then compared whole, as
/// `kin_review::is_semantic_modification` does for an entity, so a field
/// added to a relation later counts as a change until someone decides
/// otherwise.
fn relation_moved_only_its_place(old: &Relation, new: &Relation) -> bool {
    if old.evidence.len() != new.evidence.len() {
        return false;
    }
    let mut normalized = new.clone();
    normalized.created_in = old.created_in;
    for (evidence, before) in normalized.evidence.iter_mut().zip(&old.evidence) {
        evidence.source_span.clone_from(&before.source_span);
    }
    normalized == *old
}

fn is_position_only(change: &kin_review::diff::RelationChange) -> bool {
    matches!(
        &change.kind,
        kin_review::diff::RelationChangeKind::Modified { old, new }
            if relation_moved_only_its_place(old, new)
    )
}

/// Whether `inner` is recorded inside `outer`: cut from the same source,
/// with its span within outer's and smaller than it.
fn recorded_inside(inner: &Entity, outer: &Entity) -> bool {
    match (&inner.span, &outer.span) {
        (Some(inner_span), Some(outer_span)) => {
            inner.id != outer.id
                && inner_span.file == outer_span.file
                && inner_span.start_byte >= outer_span.start_byte
                && inner_span.end_byte <= outer_span.end_byte
                && inner_span.end_byte - inner_span.start_byte
                    < outer_span.end_byte - outer_span.start_byte
        }
        _ => false,
    }
}

/// A container's own text: its recorded body with the span of every member
/// listed cut out, and each run of whitespace read as one space, the way a
/// fingerprint reads past formatting.
///
/// `None` when the body is not the container's whole span as recorded, or a
/// member's span cannot be cut from it, so a caller cannot read the
/// container as unchanged by mistake.
fn own_text(body: &str, container: &Entity, members: &[&Entity]) -> Option<String> {
    let span = container.span.as_ref()?;
    if body.len() != span.end_byte.checked_sub(span.start_byte)? {
        return None;
    }
    let mut cuts: Vec<(usize, usize)> = members
        .iter()
        .filter(|member| recorded_inside(member, container))
        .filter_map(|member| member.span.as_ref())
        .map(|member| {
            (
                member.start_byte - span.start_byte,
                member.end_byte - span.start_byte,
            )
        })
        .collect();
    cuts.sort_unstable();
    let mut own = String::with_capacity(body.len());
    let mut at = 0;
    for (start, end) in cuts {
        // A member inside one already cut went with it.
        if end <= at {
            continue;
        }
        let start = start.max(at);
        if !body.is_char_boundary(start) || !body.is_char_boundary(end) {
            return None;
        }
        own.push_str(&body[at..start]);
        own.push(' ');
        at = end;
    }
    own.push_str(&body[at..]);
    Some(own.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Whether a modification left everything a container declares as it was:
/// its name, kind, signature, visibility, role, place and documentation. What
/// moved is its body, which for a container holds its members.
fn declaration_held(old: &Entity, new: &Entity) -> bool {
    field_changes(old, new)
        .iter()
        .all(|field| matches!(field.field.as_str(), "body" | "fingerprint"))
}

/// The containers among `changes` that changed only through their members.
///
/// A container's recorded text holds its members' text, so editing one
/// method of a class records the class as modified too. As a step it would
/// show the method's edit a second time and gather everything that uses the
/// class as if the class had changed. A container is read through its
/// members when what it declares held still, at least one changed entity is
/// recorded inside it, and its own text, with every changed member cut out,
/// reads the same on both sides. A class whose attribute or decorator moved
/// beside a method keeps its step.
///
/// `changes` is every entity the subject changed, as its two records. Text
/// that cannot be read on either side keeps the container a step: it is
/// shown, never hidden, when the record cannot say.
fn changed_through_members(
    held: &HeldSourceAuthority<'_, kin_db::InMemoryGraph>,
    changes: &[(Option<&Entity>, Option<&Entity>)],
    before_at: Option<SemanticChangeId>,
    after: EntitySourceScope,
) -> HashSet<EntityId> {
    let mut through = HashSet::new();
    let Some(before_at) = before_at else {
        return through;
    };
    for (old, new) in changes {
        let (Some(old), Some(new)) = (old, new) else {
            continue;
        };
        if kin_model::is_module_container(new) || !declaration_held(old, new) {
            continue;
        }
        let old_members: Vec<&Entity> = changes
            .iter()
            .filter_map(|(member, _)| *member)
            .filter(|member| recorded_inside(member, old))
            .collect();
        let new_members: Vec<&Entity> = changes
            .iter()
            .filter_map(|(_, member)| *member)
            .filter(|member| recorded_inside(member, new))
            .collect();
        if old_members.is_empty() && new_members.is_empty() {
            continue;
        }
        let before = read_body(held, old, EntitySourceScope::At(before_at));
        let after_body = read_body(held, new, after);
        let (Some(before), Some(after_text)) = (before.text, after_body.text) else {
            continue;
        };
        let (Some(own_before), Some(own_after)) = (
            own_text(&before, old, &old_members),
            own_text(&after_text, new, &new_members),
        ) else {
            continue;
        };
        if own_before == own_after {
            through.insert(new.id);
        }
    }
    through
}

/// Whether `entity` only declares a callable whose body is another
/// entity's, as its own language adapter says: a Python `@overload` stub, a C
/// prototype, a TypeScript ambient declaration.
fn only_declares(entity: &Entity) -> bool {
    kin_parser::declaration_only::declares_only(entity.language, entity.kind, &entity.signature)
}

/// The entity a reader knows a callable by.
///
/// An entity that only declares stands for the definition of the same name
/// in the same container, or, when the container holds no definition, for
/// the first of the declarations by id. Every other entity stands for
/// itself. Read from `graph`, so a list that holds only the stubs of a
/// callable and a list that holds its definition name the same one.
fn canonical_callable<G: ImpactGraph + ?Sized>(graph: &G, entity: &Entity) -> Result<EntityId> {
    if !only_declares(entity) {
        return Ok(entity.id);
    }
    let Some(container) = container_of(graph, &entity.id)? else {
        return Ok(entity.id);
    };
    let (mut definitions, mut declarations) = (Vec::new(), vec![entity.id]);
    for relation in graph.get_all_relations_for_entity(&container)? {
        if relation.kind != RelationKind::Contains || relation.src.as_entity() != Some(container) {
            continue;
        }
        let Some(member) = relation
            .dst
            .as_entity()
            .filter(|member| *member != entity.id)
        else {
            continue;
        };
        let Some(sibling) = graph.get_entity(&member)? else {
            continue;
        };
        if sibling.name != entity.name {
            continue;
        }
        if only_declares(&sibling) {
            declarations.push(sibling.id);
        } else {
            definitions.push(sibling.id);
        }
    }
    Ok(definitions
        .into_iter()
        .min()
        .or_else(|| declarations.into_iter().min())
        .unwrap_or(entity.id))
}

/// Fold the rows of one list that stand for one callable.
///
/// Rows whose entities a reader knows as one callable, `@overload` stubs
/// beside their implementation, fold into one row: the definition's when it
/// is in the list, else the first by id, which counts the rest in
/// `overloads`. Rows that still share a name afterwards are different code,
/// as two Java overloads that each define a body are, and are marked to carry
/// their parameters.
fn fold_declarations(
    reachers: Vec<Reacher>,
    canonical: &dyn Fn(&Entity) -> Result<EntityId>,
) -> Result<Vec<Reacher>> {
    let mut groups: BTreeMap<(bool, EntityId), Vec<Reacher>> = BTreeMap::new();
    for mut reacher in reachers {
        reacher.canonical = canonical(&reacher.entity)?;
        groups
            .entry((reacher.is_test, reacher.canonical))
            .or_default()
            .push(reacher);
    }
    let mut kept: Vec<Reacher> = Vec::with_capacity(groups.len());
    for ((_, canonical), mut members) in groups {
        let at = members
            .iter()
            .position(|member| member.entity.id == canonical)
            .unwrap_or_else(|| {
                members
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, member)| member.entity.id)
                    .map_or(0, |(index, _)| index)
            });
        let changed_here = members.iter().any(|member| member.changed_here);
        let overloads = members.len() - 1;
        let mut keep = members.swap_remove(at);
        keep.changed_here = changed_here;
        keep.overloads = overloads;
        kept.push(keep);
    }
    let mut named: HashMap<(bool, String), usize> = HashMap::new();
    for reacher in &kept {
        *named
            .entry((reacher.is_test, reacher.entity.name.clone()))
            .or_default() += 1;
    }
    for reacher in &mut kept {
        reacher.labelled = named[&(reacher.is_test, reacher.entity.name.clone())] > 1;
    }
    Ok(kept)
}

/// One entity that reaches a step, before rows are folded and built.
struct Reacher {
    relation: Relation,
    entity: Entity,
    changed_here: bool,
    is_test: bool,
    /// The entity a reader knows it by; see [`canonical_callable`].
    canonical: EntityId,
    overloads: usize,
    /// Whether another row of the same list carries the same name, so this
    /// one carries its parameters.
    labelled: bool,
}

fn entity_file(entity: &Entity) -> Option<String> {
    entity
        .span
        .as_ref()
        .map(|span| span.file.to_string())
        .or_else(|| entity.file_origin.as_ref().map(ToString::to_string))
}

fn fingerprint(entity: &Entity) -> GuideFingerprint {
    GuideFingerprint {
        signature: entity.fingerprint.signature_hash.to_string(),
        behavior: entity.fingerprint.behavior_hash.to_string(),
        ast: entity.fingerprint.ast_hash.to_string(),
    }
}

/// The recorded fields that moved between a modified entity's two records.
///
/// Never empty: the review keeps a modification only when the entity changed
/// beyond its span and provenance, so when no named field moved, what moved is
/// the fingerprint, and saying so beats an empty list that reads as no change.
fn field_changes(old: &Entity, new: &Entity) -> Vec<GuideFieldChange> {
    let mut changes = Vec::new();
    let mut compare = |field: &str, before: Option<String>, after: Option<String>| {
        if before != after {
            changes.push(GuideFieldChange {
                field: field.to_string(),
                before,
                after,
            });
        }
    };
    compare("name", Some(old.name.clone()), Some(new.name.clone()));
    compare("kind", Some(label(old.kind)), Some(label(new.kind)));
    compare(
        "signature",
        Some(old.signature.clone()),
        Some(new.signature.clone()),
    );
    compare(
        "visibility",
        Some(label(old.visibility)),
        Some(label(new.visibility)),
    );
    compare("role", Some(label(old.role)), Some(label(new.role)));
    compare("file", entity_file(old), entity_file(new));
    compare(
        "doc_summary",
        old.doc_summary.clone(),
        new.doc_summary.clone(),
    );
    if old.fingerprint.behavior_hash != new.fingerprint.behavior_hash {
        changes.push(GuideFieldChange {
            field: "body".to_string(),
            before: None,
            after: None,
        });
    }
    if changes.is_empty() {
        changes.push(GuideFieldChange {
            field: "fingerprint".to_string(),
            before: None,
            after: None,
        });
    }
    changes
}

/// The entity's own span in `scope`, or why there is none to show.
///
/// A refused read stays a step with its refusal on it. One body the store
/// cannot serve must not cost the reader the rest of the walkthrough, and it
/// must not be replaced by text from anywhere else.
fn read_body(
    held: &HeldSourceAuthority<'_, kin_db::InMemoryGraph>,
    entity: &Entity,
    scope: EntitySourceScope,
) -> GuideBody {
    let (change_id, working) = match scope {
        EntitySourceScope::At(change) => (Some(change.to_string()), false),
        EntitySourceScope::WorkspaceHead => (None, true),
    };
    match read_entity_source_exact_at(held, entity, BODY_MAX_BYTES, scope) {
        Ok(Some(source)) => GuideBody {
            state: "recorded".to_string(),
            text: Some(source.body),
            start_line: entity_presentation_start_line(entity),
            change_id,
            working,
            source_blob: match source.entry {
                kin_model::TreeEntry::Blob { hash, .. } => Some(hash.to_string()),
                _ => None,
            },
            reason: None,
        },
        Ok(None) => GuideBody {
            state: "no_source".to_string(),
            text: None,
            start_line: None,
            change_id,
            working,
            source_blob: None,
            reason: Some(entity_body_gap_reason(entity)),
        },
        Err(error) => GuideBody {
            state: "unavailable".to_string(),
            text: None,
            start_line: None,
            change_id,
            working,
            source_blob: None,
            reason: Some(error.to_string()),
        },
    }
}

/// Whether a stored review's head ref names `change`.
///
/// A head in the explicit `kin:` or `change:` form names a change and nothing
/// else, and a change never moves, so reading one again later reads the same
/// change. A whole id is compared as written. A prefix is a search, and the
/// ref grammar runs it against every change the repository holds, refusing one
/// that two changes share. That search reads repository authority, so it is
/// run only for a prefix this change's id starts with: no other prefix can
/// name this change, and a guide of a change nobody reviewed by prefix pays
/// nothing. A prefix the grammar cannot resolve, ambiguous now or unreadable,
/// is not matched rather than guessed at.
///
/// A bare head is matched only when it spells this change's whole id. Any
/// other bare head is a name, and a name moves: a review opened against a
/// branch was about wherever the branch pointed then, which its provenance
/// records and this guide cannot stand behind for any one change. The grammar
/// would read a branch whose own name is some change's whole id as the
/// branch, and this reads it as the change. That is the one spelling the two
/// can differ on, and a repository would have to name a branch that way.
fn head_names_change(
    refs: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    head_ref: &str,
    change: &SemanticChangeId,
) -> bool {
    // The head is matched as it is stored, with no trimming and no folding of
    // case. A review is opened with whatever spelling its caller gave, and the
    // ref grammar reads that spelling exactly: one it refused then, a padded
    // or upper-case id, never named a change, and matching a tidied copy of
    // it here would attribute a review to a change its head never resolved to.
    let head = head_ref;
    let id = change.to_string();
    let Some(value) = head
        .strip_prefix("kin:")
        .or_else(|| head.strip_prefix("change:"))
    else {
        return head == id;
    };
    if value.len() == id.len() {
        return value == id;
    }
    let plain_prefix = value.bytes().all(|byte| byte.is_ascii_hexdigit());
    if plain_prefix && !id.starts_with(value) {
        return false;
    }
    resolve_through(refs, graph, Some(head)).is_ok_and(|resolved| resolved == *change)
}

/// A review's name, title, ref or scope as a guide carries it: on one line,
/// and whole or its start and an ellipsis.
///
/// A label is a name, and a name with a line break in it would put its later
/// lines at column zero in the terminal, where they read as a new section. A
/// caller can store one, so every control character is carried as a space.
fn label_text(text: &str) -> String {
    let one_line = |part: &str| -> String {
        part.chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect()
    };
    match text.char_indices().nth(REVIEW_LABEL_MAX_CHARS) {
        Some((end, _)) => format!("{}…", one_line(&text[..end])),
        None => one_line(text),
    }
}

/// A comment or note as a guide carries it: whole, or its start and a mark.
///
/// `budget` is what the guide has left for review text in all. A text takes
/// what it needs up to its own limit, and one that arrives after the budget
/// is spent is carried as cut with nothing of it shown.
fn bounded_text(text: &str, budget: &mut usize) -> (String, bool) {
    let limit = REVIEW_TEXT_MAX_CHARS.min(*budget);
    let (kept, cut) = match text.char_indices().nth(limit) {
        Some((end, _)) => (&text[..end], true),
        None => (text, false),
    };
    *budget -= kept.chars().count();
    (kept.to_string(), cut)
}

/// One review's latest `max` decisions and notes as a single history, oldest
/// first, and how many the review holds in all.
///
/// The store keeps the two apart and answers notes in its own map order, which
/// is neither the order they were written in nor stable between two reads. A
/// reader follows a review as one sequence of events, so they are merged by
/// when each was recorded. Two recorded in the same instant keep decisions
/// ahead of notes, decisions in the order the store holds them and notes by
/// their ids.
///
/// The entries are ordered by their keys alone and only the ones kept are
/// built, newest first so the guide's text `budget` is spent on the latest. A
/// review can hold far more history than a guide lists, and building an entry
/// copies its text, so a history a thousand times the limit costs a sort of
/// its keys and the text of the entries shown, not a copy of all of it. The
/// store's own read of the record is not bounded here: it answers a review's
/// decisions and notes whole, as it does for every surface that shows one.
fn review_history(
    record: &kin_review::records::ReviewRecord,
    max: usize,
    budget: &mut usize,
) -> (Vec<GuideReviewEvent>, usize) {
    enum Entry<'a> {
        Decision(&'a kin_model::review::ReviewDecision),
        Note(&'a kin_model::review::ReviewNote),
    }
    let mut keyed: Vec<(&kin_model::Timestamp, u8, u128, Entry<'_>)> = record
        .decisions
        .iter()
        .enumerate()
        .map(|(index, decision)| {
            (
                &decision.decided_at,
                0,
                index as u128,
                Entry::Decision(decision),
            )
        })
        .chain(record.notes.iter().map(|note| {
            (
                &note.created_at,
                1,
                note.note_id.0.as_u128(),
                Entry::Note(note),
            )
        }))
        .collect();
    keyed.sort_by(|left, right| {
        left.0
            .cmp(right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    let total = keyed.len();
    let mut events: Vec<GuideReviewEvent> = keyed
        .iter()
        .rev()
        .take(max)
        .map(|(at, _, _, entry)| match entry {
            Entry::Decision(decision) => {
                let (text, text_cut) = match decision.comment.as_deref() {
                    Some(comment) => {
                        let (kept, cut) = bounded_text(comment, budget);
                        (Some(kept), cut)
                    }
                    None => (None, false),
                };
                GuideReviewEvent {
                    kind: "decision".to_string(),
                    at: at.to_string(),
                    by: label_text(&decision.reviewer.name),
                    state: Some(kin_review::records::review_state_label(decision.state)),
                    text,
                    scope: None,
                    text_cut,
                }
            }
            Entry::Note(note) => {
                let (kept, text_cut) = bounded_text(&note.body, budget);
                GuideReviewEvent {
                    kind: "note".to_string(),
                    at: at.to_string(),
                    by: label_text(&note.authored_by.name),
                    state: None,
                    text: Some(kept),
                    scope: note
                        .scope
                        .as_ref()
                        .map(|scope| label_text(&scope.to_string())),
                    text_cut,
                }
            }
        })
        .collect();
    events.reverse();
    (events, total)
}

/// The stored reviews of one change, newest first, each with its history.
///
/// Read through the one set of record reads every review surface shares, so a
/// decision the page records is the decision `kin review show` prints.
///
/// Bounded like the rest of a guide. A change can gather any number of
/// reviews, and a review any amount of history written from a terminal or an
/// agent tool, and a guide that copied all of it would grow without limit on
/// a change that is otherwise one line. It lists the newest reviews and each
/// one's latest entries, and says how many there are.
pub(crate) fn reviews_of_change(
    refs: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    change: &SemanticChangeId,
) -> Result<GuideReviews> {
    reviews_matching(refs, graph, change, None)
}

/// The stored reviews of the range that ends at `head` and reads "before" at
/// `merge_base`.
///
/// A range and its last change share a head, and a review of one is not a
/// review of the other: approving the last change of a branch says nothing
/// about the forty before it. So a range lists only the reviews whose base
/// lands where its own does. The reverse is left as it was. A review of the
/// whole range did read the last change, and that change's own page shows it.
///
/// The same call answers which review a write lands on. A decision made on
/// one change's page joins a review from that change's own base, never the
/// review of a range that happens to end at it.
pub(crate) fn reviews_of_range(
    refs: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    head: &SemanticChangeId,
    merge_base: &SemanticChangeId,
) -> Result<GuideReviews> {
    reviews_matching(refs, graph, head, Some(merge_base))
}

/// The change a ref spells out whole, bare or in an explicit form. A branch
/// name or a prefix is not one: what it names can move.
fn whole_change_id(reference: &str) -> Option<SemanticChangeId> {
    let id = reference
        .strip_prefix("kin:")
        .or_else(|| reference.strip_prefix("change:"))
        .unwrap_or(reference);
    if id.len() != 64
        || !id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    kin_model::Hash256::from_hex(id)
        .ok()
        .map(SemanticChangeId::from_hash)
}

/// Whether a review's base, read the way a range reads one, lands on
/// `merge_base`: it is that change, or a change whose history meets `head`
/// there.
///
/// Only a base that spells a whole change id is read. A branch name is
/// resolved to wherever it points now, and a review opened against `main`
/// would join or leave a range as `main` moved, long after the decision was
/// made. Two change ids meet where they always met, so a review matched this
/// way stays matched.
fn base_reads_at(
    graph: &kin_db::InMemoryGraph,
    base_ref: &str,
    head: &SemanticChangeId,
    merge_base: &SemanticChangeId,
) -> bool {
    let Some(base) = whole_change_id(base_ref) else {
        return false;
    };
    base == *merge_base
        || matches!(
            graph.find_merge_bases(&base, head).as_deref(),
            Ok([one]) if one == merge_base
        )
}

fn reviews_matching(
    refs: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    change: &SemanticChangeId,
    range_base: Option<&SemanticChangeId>,
) -> Result<GuideReviews> {
    // One answer per spelling of a head. Matching a prefix asks the ref
    // grammar, which reads repository authority, and a change reviewed many
    // times under one prefix must cost that read once and not once a review.
    let mut asked: HashMap<String, bool> = HashMap::new();
    let mut bases: HashMap<String, bool> = HashMap::new();
    let mut matching = Vec::new();
    for review in kin_review::records::list_stored_reviews(graph, None)? {
        let names = *asked
            .entry(review.head_ref.clone())
            .or_insert_with(|| head_names_change(refs, graph, &review.head_ref, change));
        if !names {
            continue;
        }
        let in_range = range_base.is_none_or(|merge_base| {
            *bases
                .entry(review.base_ref.clone())
                .or_insert_with(|| base_reads_at(graph, &review.base_ref, change, merge_base))
        });
        if in_range {
            matching.push(review);
        }
    }
    let reviews_total = matching.len();
    let mut budget = REVIEW_TEXT_BUDGET_CHARS;
    let mut reviews = Vec::new();
    for review in matching.into_iter().take(REVIEWS_LISTED_MAX) {
        let Some(record) = kin_review::records::read_review_record(graph, &review.review_id)?
        else {
            continue;
        };
        // Reviews are taken newest first and each one's entries newest first,
        // so when the budget runs short it is the oldest text of the oldest
        // review that goes without.
        let (events, events_total) = review_history(&record, REVIEW_EVENTS_LISTED_MAX, &mut budget);
        reviews.push(GuideReview {
            review_id: review.review_id.to_string(),
            title: label_text(&review.title),
            state: kin_review::records::review_state_label(review.state),
            base_ref: label_text(&review.base_ref),
            head_ref: label_text(&review.head_ref),
            events,
            events_total,
        });
    }
    Ok(GuideReviews {
        reviews,
        reviews_total,
    })
}

/// [`reviews_of_change`] for a change named by any ref, for the page's reread
/// after it records something.
///
/// With `range_base`, the reviews of the range that ends at `change` and
/// reads "before" at that base, as [`reviews_of_range`] has them.
pub(crate) fn reviews_of_ref(
    refs: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    change: &str,
    range_base: Option<&str>,
) -> Result<GuideReviews> {
    let change_id = resolve_change(refs, graph, Some(change))?;
    match range_base {
        Some(base) => {
            let base_id = resolve_change(refs, graph, Some(base))?;
            reviews_of_range(refs, graph, &change_id, &base_id)
        }
        None => reviews_of_change(refs, graph, &change_id),
    }
}

/// The text of `entity` as recorded at `change`, for a reader who opens
/// something that reaches a step.
///
/// The entity is taken from the graph committed at the change, not from the
/// live graph. Its path there is what binds the read to the right source: a
/// live record of an entity that has since moved would aim the same span at
/// another file's bytes, and the answer would be text that was never its own.
///
/// A request that names nothing recorded at the change is answered, not
/// failed: the body carries the reason and no text. Only a change whose state
/// cannot be materialized is an error.
pub(crate) fn entity_source_at(
    graph: &kin_db::InMemoryGraph,
    entity: &str,
    change: &str,
    repository_authority: Option<&kin_mcp::handlers::RequestRepositoryAuthority>,
) -> Result<GuideEntitySource> {
    let missing = |reason: String| GuideEntitySource {
        entity: None,
        body: GuideBody {
            state: "unavailable".to_string(),
            text: None,
            start_line: None,
            change_id: None,
            working: false,
            source_blob: None,
            reason: Some(reason),
        },
    };
    let Ok(entity_uuid) = uuid::Uuid::parse_str(entity.trim()) else {
        return Ok(missing(format!("'{entity}' is not an entity id")));
    };
    let entity_id = EntityId(entity_uuid);
    if change.trim() == WORKING {
        // The working tree's own record of the entity, read from the graph
        // the request selected, which is the one the working guide walked.
        use kin_model::EntityStore as _;
        let Some(record) = graph.get_entity(&entity_id)? else {
            return Ok(missing(format!(
                "entity {entity_id} is not in the working tree"
            )));
        };
        let held = HeldSourceAuthority::new(graph, repository_authority);
        return Ok(GuideEntitySource {
            entity: Some(entity_ref(&record)),
            body: read_body(&held, &record, EntitySourceScope::WorkspaceHead),
        });
    }
    let Ok(change_hash) = kin_model::Hash256::from_hex(change.trim()) else {
        return Ok(missing(format!("'{change}' is not a change id")));
    };
    let change_id = SemanticChangeId::from_hash(change_hash);
    if graph.get_change(&change_id)?.is_none() {
        return Ok(missing(format!("change {change_id} is not recorded")));
    }
    let at_change = GraphAtRef::materialize(graph, &change_id)?;
    let Some(record) = at_change.get_entity(&entity_id)? else {
        return Ok(missing(format!(
            "entity {entity_id} is not recorded at change {change_id}"
        )));
    };
    let held = HeldSourceAuthority::new(graph, repository_authority);
    Ok(GuideEntitySource {
        entity: Some(entity_ref(&record)),
        body: read_body(&held, &record, EntitySourceScope::At(change_id)),
    })
}

/// What the committed graph at the change says reaches one changed entity.
#[derive(Default)]
struct Wiring {
    consumers: Vec<GuideConnection>,
    tests: Vec<GuideConnection>,
    importing_modules: Vec<GuideEntityRef>,
    /// Entities outside the change that reach this one, for the summary.
    outside_consumers: BTreeSet<EntityId>,
    outside_tests: BTreeSet<EntityId>,
}

fn is_import(kind: RelationKind) -> bool {
    matches!(kind, RelationKind::Imports | RelationKind::Includes)
}

/// Whether `relation` ties its two ends into one chapter.
///
/// Wider than an impact relation on purpose. A class and the method it
/// contains are one thing to read when both changed, and `Contains` is the
/// only edge that says so.
fn ties_chapter(kind: RelationKind) -> bool {
    kind == RelationKind::Contains || kin_review::is_impact_relation(kind)
}

fn connection(relation: &Relation, entity: &Entity, changed_here: bool) -> GuideConnection {
    GuideConnection {
        relation_id: relation.id.to_string(),
        relation: label(relation.kind),
        origin: label(relation.origin),
        confidence: relation.confidence,
        recorded_in: relation.created_in.map(|change| change.to_string()),
        entity: entity_ref(entity),
        changed_here,
        overloads: 0,
    }
}

fn sort_connections(connections: &mut Vec<GuideConnection>) {
    connections.sort_by(|a, b| {
        b.changed_here
            .cmp(&a.changed_here)
            .then_with(|| a.entity.name.cmp(&b.entity.name))
            .then_with(|| a.entity.parameters.cmp(&b.entity.parameters))
            .then_with(|| a.entity.file.cmp(&b.entity.file))
            .then_with(|| a.entity.id.cmp(&b.entity.id))
    });
}

/// Order one chapter so what others rely on is read first.
///
/// `depends` holds `(dependent, dependency)` pairs among the chapter's own
/// members. A cycle has no such order, so whatever a cycle leaves is appended
/// in the tie-break order, which is still deterministic.
fn order_chapter(
    members: &[EntityId],
    depends: &BTreeSet<(EntityId, EntityId)>,
    tie_break: &dyn Fn(&EntityId) -> (bool, std::cmp::Reverse<usize>, String, EntityId),
) -> Vec<EntityId> {
    let member_set: HashSet<EntityId> = members.iter().copied().collect();
    let mut waiting_on: HashMap<EntityId, usize> = members.iter().map(|id| (*id, 0)).collect();
    let mut unblocks: HashMap<EntityId, Vec<EntityId>> = HashMap::new();
    for (dependent, dependency) in depends {
        if dependent == dependency
            || !member_set.contains(dependent)
            || !member_set.contains(dependency)
        {
            continue;
        }
        *waiting_on.entry(*dependent).or_default() += 1;
        unblocks.entry(*dependency).or_default().push(*dependent);
    }
    let mut ordered = Vec::with_capacity(members.len());
    let mut placed: HashSet<EntityId> = HashSet::new();
    loop {
        let mut ready: Vec<EntityId> = members
            .iter()
            .filter(|id| !placed.contains(*id) && waiting_on[*id] == 0)
            .copied()
            .collect();
        if ready.is_empty() {
            break;
        }
        ready.sort_by_key(|id| tie_break(id));
        let next = ready[0];
        placed.insert(next);
        ordered.push(next);
        for dependent in unblocks.get(&next).into_iter().flatten() {
            if let Some(count) = waiting_on.get_mut(dependent) {
                *count = count.saturating_sub(1);
            }
        }
    }
    let mut cyclic: Vec<EntityId> = members
        .iter()
        .filter(|id| !placed.contains(*id))
        .copied()
        .collect();
    cyclic.sort_by_key(|id| tie_break(id));
    ordered.extend(cyclic);
    ordered
}

fn find_root(parents: &mut HashMap<EntityId, EntityId>, id: EntityId) -> EntityId {
    let mut root = id;
    while parents[&root] != root {
        root = parents[&root];
    }
    let mut cursor = id;
    while parents[&cursor] != root {
        let next = parents[&cursor];
        parents.insert(cursor, root);
        cursor = next;
    }
    root
}

fn completeness(review: &kin_review::Review) -> GuideCompleteness {
    match review.impact.enrichment.as_ref() {
        Some(observation) if observation.bounds_answer() => GuideCompleteness {
            state: "not_settled".to_string(),
            message: format!(
                "Not settled: {} entities pending enrichment. Impact counts are lower bounds and \
                 risk may change.",
                observation.total_pending_entities
            ),
            pending_entities: Some(observation.total_pending_entities as u64),
        },
        Some(observation) if observation.status == "no_recorded_call_site_debt" => {
            GuideCompleteness {
                state: "no_recorded_debt".to_string(),
                message: "No recorded call-site debt. Overall completeness is unverified."
                    .to_string(),
                pending_entities: None,
            }
        }
        _ => GuideCompleteness {
            state: "unavailable".to_string(),
            message: "Completeness unavailable. This review does not establish a clear result."
                .to_string(),
            pending_entities: None,
        },
    }
}

/// Whether a finding is about one of `names`: the entity a finding names is
/// the first one it quotes in backquotes.
fn gap_names_one_of(gap: &str, names: &HashSet<String>) -> bool {
    gap.split('`')
        .nth(1)
        .is_some_and(|name| names.contains(name))
}

/// `a`, `a and b`, `a, b and c`.
fn join_and<S: AsRef<str>>(items: &[S]) -> String {
    match items {
        [] => String::new(),
        [one] => one.as_ref().to_string(),
        [rest @ .., last] => format!(
            "{} and {}",
            rest.iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join(", "),
            last.as_ref()
        ),
    }
}

fn describe_fields(fields: &[GuideFieldChange]) -> String {
    let names: Vec<&str> = fields
        .iter()
        .map(|field| match field.field.as_str() {
            "doc_summary" => "documentation",
            "fingerprint" => "recorded fingerprint",
            other => other,
        })
        .collect();
    let has = |name: &str| fields.iter().any(|field| field.field == name);
    let mut text = format!("Its {} changed", join_and(&names));
    if has("body") && !has("signature") {
        text.push_str("; its signature did not");
    } else if has("signature") && !has("body") {
        text.push_str("; its body did not");
    }
    text.push('.');
    text
}

/// What a step's verdict says about the consumers that did not change with
/// it; see [`GuideConsumerVerdict`].
const NEEDS_NO_UPDATE: &str = "needs_no_update";
const MAY_NEED_REVIEW: &str = "may_need_review";
const NOT_UPDATED: &str = "not_updated";
const UPDATED: &str = "updated";

/// What a changed entity's change asks of the `outside` recorded consumers
/// that reach it and did not change with it.
///
/// The entity's two records say what moved: [`kin_review::ContractChange`]
/// reads them through the graph's fingerprint, so a comment or a docstring is
/// told apart from code. The review's findings on the entity say whether what
/// moved breaks a consumer outside the change. An error-level finding is its
/// word that it does. A changed signature with none is a change the review
/// found every recorded caller safe from, such as a renamed parameter every
/// call site passes by position, and those callers may still want a look.
fn consumer_verdict(
    draft: &Draft<'_>,
    findings: &[GuideFinding],
    outside: usize,
) -> GuideConsumerVerdict {
    let breaks = findings.iter().any(|finding| finding.severity == "error");
    let (reading, breaking, unaffected) = match (draft.change, draft.old, draft.new) {
        ("modified", Some(old), Some(new)) => {
            let reading = kin_review::ContractChange::of(old, new);
            (
                reading.label(),
                reading.breaks_consumers(),
                reading.consumers_unaffected(),
            )
        }
        ("removed", _, _) => ("removed", true, false),
        _ => ("added", false, false),
    };
    let (verdict, reason) = if unaffected {
        (NEEDS_NO_UPDATE, reading)
    } else if breaking {
        match (outside, breaks || draft.change == "removed") {
            (0, _) => (UPDATED, reading),
            (_, true) => (NOT_UPDATED, reading),
            (_, false) => (MAY_NEED_REVIEW, reading),
        }
    } else if breaks && outside > 0 {
        // A contract entity, such as a schema, whose body is what its
        // consumers read: the review says the change breaks them.
        (NOT_UPDATED, "contract_changed")
    } else {
        (MAY_NEED_REVIEW, reading)
    };
    GuideConsumerVerdict {
        verdict: verdict.to_string(),
        reason: reason.to_string(),
        consumers: outside,
    }
}

/// What moved in a modified entity, in the words its verdict's reason calls
/// for. A comment inside a signature moves the signature's recorded text, and
/// saying "its signature changed" would send a reader after every caller.
fn describe_modification(fields: &[GuideFieldChange], reason: &str) -> String {
    let has = |name: &str| fields.iter().any(|field| field.field == name);
    match reason {
        "comments_or_layout_only" if has("doc_summary") => {
            "Its documentation changed, and its code reads the same token for token.".to_string()
        }
        "comments_or_layout_only" if has("signature") => "Its signature text changed only by \
                                                          a comment or layout, and its code \
                                                          reads the same token for token."
            .to_string(),
        "comments_or_layout_only" => {
            "Only comments or layout changed, and its code reads the same token for token."
                .to_string()
        }
        "behavior_equivalent" if has("doc_summary") => "Its documentation changed; its \
                                                        signature did not, and the graph \
                                                        records its body as behaving the same."
            .to_string(),
        "behavior_equivalent" => "Its body changed only in ways the graph records as behaving \
                                  the same, such as a docstring; its signature did not."
            .to_string(),
        "visibility_widened" => "It became visible to more of the code; its signature did not \
                                 change, and its code behaves as it did."
            .to_string(),
        "moved" => "It moved, and its signature did not change; a consumer that names it by \
                    where it lives may need to follow it."
            .to_string(),
        _ => describe_fields(fields),
    }
}

/// What a verdict says of the `outside` consumers of `total` that did not
/// change, or nothing when every one of them changed here.
fn verdict_sentence(verdict: &str, outside: usize, total: usize) -> Option<String> {
    if outside == 0 {
        return None;
    }
    let (subject, one) = match (outside == total, outside) {
        (true, 1) => ("It".to_string(), true),
        (true, _) => ("They".to_string(), false),
        (false, 1) => ("The 1 that did not change".to_string(), true),
        (false, count) => (format!("The {count} that did not change"), false),
    };
    Some(match verdict {
        NEEDS_NO_UPDATE => format!(
            "{subject} {} no update.",
            if one { "needs" } else { "need" }
        ),
        MAY_NEED_REVIEW => format!("{subject} may need review."),
        NOT_UPDATED => format!(
            "{subject} {} not updated.",
            if one { "was" } else { "were" }
        ),
        _ => return None,
    })
}

fn step_narration(
    draft: &Draft<'_>,
    fields: &[GuideFieldChange],
    wiring: &Wiring,
    verdict: &GuideConsumerVerdict,
) -> String {
    let kind = draft.subject().map_or_else(
        || "entity".to_string(),
        |entity| label(entity.kind).replace('_', " "),
    );
    let mut text = match draft.change {
        "added" => format!("New {kind} `{}`.", draft.name()),
        "removed" => format!("The {kind} `{}` was removed.", draft.name()),
        _ => format!(
            "The {kind} `{}` was modified. {}",
            draft.name(),
            describe_modification(fields, &verdict.reason)
        ),
    };
    let consumers = wiring.consumers.len();
    let changed_here = wiring
        .consumers
        .iter()
        .filter(|connection| connection.changed_here)
        .count();
    // What reached a removed entity reached it until this change.
    let gone = draft.change == "removed";
    if consumers == 0 {
        text.push_str(if gone {
            " Nothing relied on it."
        } else {
            " Nothing relies on it."
        });
    } else {
        let rely = match (gone, consumers) {
            (true, 1) => "place relied",
            (true, _) => "places relied",
            (false, 1) => "place relies",
            (false, _) => "places rely",
        };
        write!(text, " {consumers} {rely} on it").unwrap();
        match (changed_here, consumers) {
            (0, 1) => text.push_str(", and it did not change here."),
            (0, _) => text.push_str(", and none of them changed here."),
            (changed, total) if changed == total && total == 1 => {
                text.push_str(", and it changed here too.")
            }
            (changed, total) if changed == total => {
                text.push_str(", and all of them changed here too.")
            }
            (changed, _) => write!(text, ", and {changed} of them changed here too.").unwrap(),
        }
        if let Some(sentence) =
            verdict_sentence(&verdict.verdict, consumers - changed_here, consumers)
        {
            write!(text, " {sentence}").unwrap();
        }
    }
    if !draft.is_test() {
        match wiring.tests.len() {
            0 => text.push_str(if gone {
                " No test reached it."
            } else {
                " No test reaches it."
            }),
            tests => {
                let reach = match (gone, tests) {
                    (true, _) => "reached",
                    (false, 1) => "reaches",
                    (false, _) => "reach",
                };
                write!(text, " {} {reach} it.", plural(tests, "test", "tests")).unwrap();
            }
        }
    }
    text
}

/// The change a guide walks.
///
/// A full change id the graph holds is taken as written, the way `kin review
/// <change>` takes it, so naming a change exactly never depends on repository
/// authority being open. Anything else is a ref, resolved by the one grammar
/// the history commands share.
pub(crate) fn resolve_change(
    refs: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    change: Option<&str>,
) -> Result<SemanticChangeId> {
    if let Some(hash) = change.and_then(|change| kin_model::Hash256::from_hex(change.trim()).ok()) {
        let id = SemanticChangeId::from_hash(hash);
        if graph.get_change(&id)?.is_some() {
            return Ok(id);
        }
    }
    resolve_through(refs, graph, change)
}

/// Resolve a ref through the authority this request reads at.
///
/// A daemon hands every review read the authority it keeps for the current
/// publication, so resolving `HEAD` or a branch costs a request nothing that
/// grows with the store. Each guide, history and reviews read used to open the
/// whole repository authority for itself, decoding and re-verifying every
/// persisted body, to read one workspace pointer. A terminal command passes an
/// authority pinned to its binding and opens once, as it always did.
fn resolve_through(
    refs: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    reference: Option<&str>,
) -> Result<SemanticChangeId> {
    crate::commands::ref_lookup::resolve_ref_through(graph, refs, reference)
        .map(|head| head.change_id)
}

/// What a guide walks, once its review is computed: the review, where its
/// relations and its two texts are read from, and how it is introduced.
struct Subject<'a, I: ImpactGraph> {
    kind: &'static str,
    review: kin_review::Review,
    /// The graph state every connection is read from.
    connections: &'a I,
    /// Where "before" is read. Absent when there is nothing before.
    before_at: Option<SemanticChangeId>,
    /// Where "after" is read.
    after: EntitySourceScope,
    change: GuideChange,
    /// How a step names where "after" was read, in its evidence.
    head_label: String,
    /// The command that lists every changed entity, for a guide that walks
    /// only some of them.
    detail_command: String,
    reviews: GuideReviews,
    range: Option<GuideRange>,
    working_tree: Option<GuideWorkingTree>,
}

fn change_header(
    change_id: &SemanticChangeId,
    base: Option<SemanticChangeId>,
    semantic_change: &SemanticChange,
) -> GuideChange {
    let (origin, git_commit) = match &semantic_change.origin {
        kin_model::ChangeOrigin::Native => ("native".to_string(), None),
        kin_model::ChangeOrigin::GitCommit { oid } => {
            ("git_commit".to_string(), Some(oid.to_string()))
        }
    };
    GuideChange {
        id: change_id.to_string(),
        base: base.map(|base| base.to_string()),
        parents: semantic_change
            .parents
            .iter()
            .map(ToString::to_string)
            .collect(),
        title: first_line(&semantic_change.message),
        message: semantic_change.message.clone(),
        author: person(&semantic_change.author.to_string()),
        author_recorded: Some(semantic_change.author.to_string())
            .filter(|recorded| *recorded != person(recorded)),
        timestamp: semantic_change.timestamp.to_string(),
        origin,
        git_commit,
    }
}

fn first_line(message: &str) -> String {
    message
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Build the guide for one recorded change.
///
/// `change` is any ref the history commands take, and the workspace's current
/// change when absent. The graph state every connection is read from is the
/// one committed at that change, never the live adjacency, so a guide of an
/// older change does not borrow relations a later one recorded.
pub(crate) fn build_guide(
    refs: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    change: Option<&str>,
    repository_authority: Option<&kin_mcp::handlers::RequestRepositoryAuthority>,
    escape_evidence: Option<&kin_review::enrichment::EscapeEvidence<'_>>,
) -> Result<ReviewGuide> {
    let change_id = resolve_change(refs, graph, change)?;
    let semantic_change = graph
        .get_change(&change_id)?
        .ok_or_else(|| anyhow::anyhow!("change {change_id} not found"))?;
    let at_head = GraphAtRef::materialize(graph, &change_id)?;
    let review = review_at(
        graph,
        &at_head,
        &change_id,
        &semantic_change,
        escape_evidence,
    )?;
    let base = review
        .base
        .or_else(|| semantic_change.parents.first().copied());
    assemble(
        graph,
        repository_authority,
        Subject {
            kind: "change",
            review,
            connections: &at_head,
            before_at: base,
            after: EntitySourceScope::At(change_id),
            change: change_header(&change_id, base, &semantic_change),
            head_label: change_id.to_string(),
            detail_command: format!("kin review {} --details", short(&change_id.to_string())),
            reviews: reviews_of_change(refs, graph, &change_id)?,
            range: None,
            working_tree: None,
        },
    )
}

/// Build the guide for what the working tree holds beyond the last recorded
/// change: the same walkthrough, of work nobody has committed yet.
///
/// The changed entities are the ones `kin diff` derives between the recorded
/// head and the working tree, so this and that command cannot disagree about
/// what is uncommitted. "Before" is read at the recorded head and "after" from
/// the working tree, and connections are read from the graph the request
/// selected, which is the working tree's own.
///
/// Nothing can be recorded against it. A review names a change, and this work
/// is not one until it is committed.
pub(crate) fn build_working_guide(
    refs: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    repository_authority: Option<&kin_mcp::handlers::RequestRepositoryAuthority>,
    escape_evidence: Option<&kin_review::enrichment::EscapeEvidence<'_>>,
    open_authority: Option<&super::repository_authority::ActiveRepositoryAuthority>,
) -> Result<ReviewGuide> {
    use kin_model::change::{EntityDelta, RelationDelta};
    use kin_review::diff::{RelationChange, RelationChangeKind};

    // The diff `kin diff` reads, from an authority the caller already holds
    // open when it has one. An open re-verifies every persisted body, and a
    // page that follows the working tree asks this again every few seconds.
    let report = match open_authority {
        Some(authority) => {
            super::diff::inspect_with_endpoint_entities_at(authority, None, None, Some(graph))?.0
        }
        None => super::diff::inspect(refs.binding(), None, None, Some(graph))?,
    };
    let head = report.base.change_id;
    let working_tree = GuideWorkingTree {
        differs: !report.artifact_deltas.is_empty(),
        derived: !matches!(
            report.semantic_basis,
            Some(super::diff::WorkspaceSemanticBasis::AdmittedOverlayOnly)
        ),
        caught_up: !matches!(
            report.semantic_basis,
            Some(super::diff::WorkspaceSemanticBasis::Derived {
                matches_admitted_tree: false,
                ..
            })
        ),
        unmeasured: None,
    };
    let mut semantic = kin_review::SemanticDiff {
        base: head,
        head: None,
        ..Default::default()
    };
    for delta in report.entity_deltas {
        let (entity_id, kind) = match delta {
            EntityDelta::Added { new } => (new.id, EntityChangeKind::Added(new)),
            EntityDelta::Modified { old, new } => {
                // A span that moved because a line above it did is not an edit
                // of this entity, here as in a recorded change.
                if !kin_review::is_semantic_modification(&old, &new) {
                    semantic.provenance_only_entity_changes += 1;
                    continue;
                }
                (new.id, EntityChangeKind::Modified { old, new })
            }
            EntityDelta::Removed { old } => (old.id, EntityChangeKind::Removed { old: Some(old) }),
        };
        semantic
            .entity_changes
            .push(kin_review::EntityChange { entity_id, kind });
    }
    for delta in report.relation_deltas {
        semantic.relation_changes.push(RelationChange {
            kind: match delta {
                RelationDelta::Added { new } => RelationChangeKind::Added(new),
                RelationDelta::Modified { old, new } => RelationChangeKind::Modified { old, new },
                RelationDelta::Removed { old } => RelationChangeKind::Removed { old },
            },
        });
    }
    let review =
        kin_review::SemanticReview::review_from_diff_with_source(semantic, graph, escape_evidence)?;

    let mut message = String::from(if working_tree.differs {
        "Uncommitted work\n\nChanges you haven't committed yet."
    } else {
        "Uncommitted work\n\nNothing is uncommitted. The code matches your last commit."
    });
    match report.semantic_basis {
        Some(super::diff::WorkspaceSemanticBasis::AdmittedOverlayOnly) => message.push_str(
            " Kin couldn't read its map of this code, so what changed isn't shown as entities \
             here.",
        ),
        Some(super::diff::WorkspaceSemanticBasis::Derived {
            matches_admitted_tree: false,
            ..
        }) => message.push_str(
            " Kin is still catching up with your latest edits, so this may be a moment behind \
             them.",
        ),
        _ => {}
    }
    let live = kin_review::impact::LiveGraph(graph);
    assemble(
        graph,
        repository_authority,
        Subject {
            kind: "working",
            review,
            connections: &live,
            before_at: head,
            after: EntitySourceScope::WorkspaceHead,
            change: GuideChange {
                id: WORKING.to_string(),
                base: head.map(|head| head.to_string()),
                parents: head.iter().map(ToString::to_string).collect(),
                title: "Uncommitted work".to_string(),
                message,
                author: person(&crate::provenance::current_actor_label()),
                author_recorded: None,
                timestamp: kin_model::Timestamp::now().to_string(),
                origin: "workspace".to_string(),
                git_commit: None,
            },
            head_label: WORKING.to_string(),
            detail_command: "kin diff".to_string(),
            reviews: GuideReviews::default(),
            range: None,
            working_tree: Some(working_tree),
        },
    )
}

/// Build the guide for a range: everything `head` reaches that `base` does
/// not, read from the change the two share.
///
/// That is what a review of a branch means. "Before" is read at the merge
/// base, so a base that has moved on since the branch left it does not show
/// its own later work as something the branch undid.
pub(crate) fn build_range_guide(
    refs: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    base_ref: &str,
    head_ref: Option<&str>,
    repository_authority: Option<&kin_mcp::handlers::RequestRepositoryAuthority>,
    escape_evidence: Option<&kin_review::enrichment::EscapeEvidence<'_>>,
) -> Result<ReviewGuide> {
    let head = resolve_change(refs, graph, head_ref)?;
    let base = resolve_change(refs, graph, Some(base_ref))?;
    let head_label = head_ref.unwrap_or("HEAD");
    let merge_base = if base == head {
        base
    } else {
        match graph.find_merge_bases(&base, &head)?.as_slice() {
            [one] => *one,
            [] => anyhow::bail!(
                "{base_ref} and {head_label} share no recorded history, so there is no range \
                 between them to walk"
            ),
            several => anyhow::bail!(
                "{base_ref} and {head_label} have {} merge bases and none of them is the one \
                 to read before at; name the base change itself",
                several.len()
            ),
        }
    };
    let head_change = graph
        .get_change(&head)?
        .ok_or_else(|| anyhow::anyhow!("change {head} not found"))?;
    let at_head = GraphAtRef::materialize(graph, &head)?;
    let base_ancestry = kin_review::ref_graph::collect_ancestry(graph, &merge_base)?;
    let in_range =
        |id: &SemanticChangeId| at_head.ancestry_contains(id) && !base_ancestry.contains(id);
    let changes_total = at_head
        .ancestry()
        .iter()
        .filter(|id| !base_ancestry.contains(id))
        .count();
    let mut members: Vec<String> = at_head
        .ancestry()
        .iter()
        .filter(|id| !base_ancestry.contains(id))
        .map(ToString::to_string)
        .collect();
    members.sort_unstable();
    let members_complete = members.len() <= RANGE_MEMBERS_MAX;
    members.truncate(RANGE_MEMBERS_MAX);

    let empty = || {
        let diff = kin_review::SemanticDiff {
            base: Some(merge_base),
            head: Some(head),
            ..Default::default()
        };
        let impact = kin_review::ImpactReport::default();
        kin_review::Review {
            base: Some(merge_base),
            head: Some(head),
            risk: kin_review::assess_risk(&diff, &impact),
            diff,
            impact,
            inline_comments: Vec::new(),
        }
    };
    let review = if changes_total == 0 {
        empty()
    } else {
        match kin_review::SemanticReview::create_review_scoped_with_source(
            &merge_base,
            &head,
            graph,
            &at_head,
            in_range,
            escape_evidence,
        ) {
            Ok(review) => review,
            // Changes whose deltas cancel out leave nothing to review. That
            // is an answer about the range, not a failure to read it.
            Err(kin_review::ReviewError::NoChanges) => empty(),
            Err(error) => return Err(error.into()),
        }
    };

    // The newest changes of the range, walked back from the head.
    let mut listed: Vec<SemanticChange> = Vec::new();
    let mut seen: HashSet<SemanticChangeId> = HashSet::new();
    let mut pending = std::collections::VecDeque::from([head]);
    while let Some(id) = pending.pop_front() {
        if listed.len() >= RANGE_CHANGES_LISTED_MAX {
            break;
        }
        if base_ancestry.contains(&id) || !seen.insert(id) {
            continue;
        }
        if let Some(change) = graph.get_change(&id)? {
            pending.extend(change.parents.iter().copied());
            listed.push(change);
        }
    }
    listed.sort_by(|left, right| right.timestamp.cmp(&left.timestamp));

    let (base_words, head_words) = (ref_words(base_ref), ref_words(head_label));
    let title = range_title(&listed, changes_total, base_words, head_words);
    let mut change = change_header(&head, Some(merge_base), &head_change);
    change.message = format!(
        "{title}\n\n{} that {head_words} holds and {base_words} does not.",
        match changes_total {
            1 => "1 change".to_string(),
            count => format!("{count} changes"),
        }
    );
    change.title = title;
    assemble(
        graph,
        repository_authority,
        Subject {
            kind: "range",
            review,
            connections: &at_head,
            before_at: Some(merge_base),
            after: EntitySourceScope::At(head),
            change,
            head_label: head.to_string(),
            detail_command: format!("kin review shadow {base_ref}..{head_label}"),
            reviews: reviews_of_range(refs, graph, &head, &merge_base)?,
            range: Some(GuideRange {
                base_ref: base_ref.to_string(),
                head_ref: head_label.to_string(),
                merge_base: merge_base.to_string(),
                changes_total,
                members,
                members_complete,
                changes: listed
                    .iter()
                    .map(|change| GuideRangeChange {
                        id: change.id.to_string(),
                        title: label_text(&first_line(&change.message)),
                        author: label_text(&person(&change.author.to_string())),
                        timestamp: change.timestamp.to_string(),
                    })
                    .collect(),
            }),
            working_tree: None,
        },
    )
}

/// How many entities the guide of each asked change walks, read the way the
/// guide reads them, so a list of changes and the walk it opens say the same
/// number.
///
/// An ordinary change is counted from its own record: its entity changes,
/// less those that moved only a span, less module containers and containers
/// that changed only through their members. A merge is counted the way its
/// guide reads it: from the changes it brought into its first parent's line,
/// folded in the order the guide's own walk folds them. A merge whose history
/// cannot be walked here is marked and left uncounted. An id this graph holds
/// no change under is left out.
pub(crate) fn change_counts(
    graph: &kin_db::InMemoryGraph,
    changes: &[String],
    repository_authority: Option<&kin_mcp::handlers::RequestRepositoryAuthority>,
) -> Result<GuideCounts> {
    if changes.len() > COUNTS_ASKED_MAX {
        anyhow::bail!(
            "{} changes were asked about at once; ask about at most {COUNTS_ASKED_MAX}",
            changes.len()
        );
    }
    let held = HeldSourceAuthority::new(graph, repository_authority);
    let mut counts = BTreeMap::new();
    let mut parents_of: HashMap<SemanticChangeId, Vec<SemanticChangeId>> = HashMap::new();
    for asked in changes {
        let Some(id) = whole_change_id(asked) else {
            continue;
        };
        let Some(change) = graph.get_change(&id)? else {
            continue;
        };
        let merge = change.parents.len() > 1;
        let diff = if merge {
            match merge_brought_in(graph, &change, &mut parents_of) {
                Ok(diff) => diff,
                Err(_) => {
                    counts.insert(
                        id.to_string(),
                        GuideChangeCount {
                            merge: true,
                            ..GuideChangeCount::default()
                        },
                    );
                    continue;
                }
            }
        } else {
            kin_review::diff_from_change(&change)
        };
        let sides: Vec<(Option<&Entity>, Option<&Entity>)> = diff
            .entity_changes
            .iter()
            .map(|change| match &change.kind {
                EntityChangeKind::Added(entity) => (None, Some(entity)),
                EntityChangeKind::Modified { old, new } => (Some(old), Some(new)),
                EntityChangeKind::Removed { old } => (old.as_ref(), None),
            })
            .collect();
        let (modules, others): (Vec<_>, Vec<_>) = sides
            .into_iter()
            .partition(|(old, new)| new.or(*old).is_some_and(kin_model::is_module_container));
        let through = changed_through_members(
            &held,
            &others,
            change.parents.first().copied(),
            EntitySourceScope::At(id),
        );
        counts.insert(
            id.to_string(),
            GuideChangeCount {
                entities: others.len() - through.len(),
                module_containers: modules.len(),
                containers_through_members: through.len(),
                merge,
                merge_counted: merge,
            },
        );
    }
    Ok(GuideCounts { counts })
}

/// Every change `at` reaches, with each change's parents read once per
/// batch however many merges walk through it.
fn ancestry_of(
    graph: &kin_db::InMemoryGraph,
    at: SemanticChangeId,
    parents_of: &mut HashMap<SemanticChangeId, Vec<SemanticChangeId>>,
) -> Result<HashSet<SemanticChangeId>> {
    let mut seen = HashSet::new();
    let mut pending = vec![at];
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        pending.extend(parents(graph, id, parents_of)?);
    }
    Ok(seen)
}

fn parents(
    graph: &kin_db::InMemoryGraph,
    id: SemanticChangeId,
    parents_of: &mut HashMap<SemanticChangeId, Vec<SemanticChangeId>>,
) -> Result<Vec<SemanticChangeId>> {
    if let Some(held) = parents_of.get(&id) {
        return Ok(held.clone());
    }
    let change = graph
        .get_change(&id)?
        .ok_or_else(|| anyhow::anyhow!("change {id} names a parent this graph does not hold"))?;
    parents_of.insert(id, change.parents.clone());
    Ok(change.parents)
}

/// What a merge brought into its first parent's line, as its guide reads it:
/// the changes the merge reaches and its first parent does not, folded in
/// the order the guide's backward walk from the merge meets them, reversed.
/// The walk expands only changes in that range: a change outside it reaches
/// nothing inside it, so the range's own order is the guide's.
fn merge_brought_in(
    graph: &kin_db::InMemoryGraph,
    merge: &SemanticChange,
    parents_of: &mut HashMap<SemanticChangeId, Vec<SemanticChangeId>>,
) -> Result<kin_review::SemanticDiff> {
    let first = *merge
        .parents
        .first()
        .ok_or_else(|| anyhow::anyhow!("a merge names its first parent"))?;
    let base = ancestry_of(graph, first, parents_of)?;
    let mut walked = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![merge.id];
    while let Some(id) = stack.pop() {
        if id == first || base.contains(&id) || !seen.insert(id) {
            continue;
        }
        stack.extend(parents(graph, id, parents_of)?);
        walked.push(id);
    }
    walked.reverse();
    let changes = walked
        .iter()
        .map(|id| {
            graph
                .get_change(id)?
                .ok_or_else(|| anyhow::anyhow!("change {id} is not held"))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(if changes.is_empty() {
        kin_review::diff_from_change(merge)
    } else {
        kin_review::diff_from_changes(&changes)
    })
}

/// How many of an entity's own revisions a history lists.
const HISTORY_LISTED_MAX: usize = 50;

/// The changes that changed `entity` itself on the line of history that
/// reaches `at`, or the current change when none is named.
///
/// A guide asks at its own head. A change on another branch, or one a merge
/// brought in by its second parent, is not on the current line, and reading
/// there would list revisions the reader is not looking at or none at all.
///
/// The same revisions and the same rule `kin history` and `kin blame` read:
/// a revision minted because the entity's file moved, with the entity's own
/// text as it was, is counted apart and not shown as a change to it.
pub(crate) fn entity_history(
    refs: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    entity: &str,
    at: Option<&str>,
    since: Option<&str>,
) -> Result<GuideHistory> {
    let entity_uuid = uuid::Uuid::parse_str(entity.trim())
        .map_err(|_| anyhow::anyhow!("'{entity}' is not an entity id"))?;
    let entity_id = EntityId(entity_uuid);
    // The working tree stands on the current change, so its history is read there.
    let head = resolve_change(refs, graph, at.filter(|at| at.trim() != WORKING))?;
    let revisions =
        crate::commands::ref_lookup::resolve_entity_revisions_at(graph, &entity_id, &head, None)?;
    let (own, unchanged) = crate::commands::ref_lookup::split_own_revisions(&revisions);
    let row = |id: &SemanticChangeId| -> Result<Option<GuideRangeChange>> {
        Ok(graph.get_change(id)?.map(|change| GuideRangeChange {
            id: change.id.to_string(),
            title: label_text(&first_line(&change.message)),
            author: label_text(&person(&change.author.to_string())),
            timestamp: change.timestamp.to_string(),
        }))
    };
    let mut listed = Vec::new();
    for revision in own.iter().rev().take(HISTORY_LISTED_MAX) {
        listed.extend(row(&revision.introduced_by)?);
    }
    let removed_by = match revisions.last().and_then(|last| last.ended_by) {
        Some(ended_by) => row(&ended_by)?,
        None => None,
    };
    // Every listed revision is on the line reaching `head`, so one belongs to
    // the range exactly when the range's base does not reach it.
    let in_range = match since {
        Some(since) => {
            let base = resolve_change(refs, graph, Some(since))?;
            let base_ancestry = kin_review::ref_graph::collect_ancestry(graph, &base)?;
            own.iter()
                .rev()
                .take(HISTORY_LISTED_MAX)
                .map(|revision| revision.introduced_by)
                .filter(|id| !base_ancestry.contains(id))
                .map(|id| id.to_string())
                .collect()
        }
        None => Vec::new(),
    };
    Ok(GuideHistory {
        entity_id: entity_id.to_string(),
        revisions: listed,
        revisions_total: own.len(),
        unchanged_revisions: unchanged.len(),
        removed_by,
        in_range,
    })
}

/// Where each reviewed change stands: the state of its newest stored review.
///
/// One pass over the stored reviews, reading no histories. A head that spells
/// a whole change id is counted directly, and one given as an explicit prefix
/// is placed by the ref grammar once per spelling. A branch name is not
/// counted: where it points moves, and the decision was about where it
/// pointed then.
pub(crate) fn standing_index(
    refs: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
) -> Result<GuideStanding> {
    let mut standing: BTreeMap<String, GuideStandingEntry> = BTreeMap::new();
    // One answer per spelling, as [`reviews_of_change`] asks it, so a change
    // reviewed many times under one prefix costs one resolution.
    let mut resolved: HashMap<String, Option<String>> = HashMap::new();
    for review in kin_review::records::list_stored_reviews(graph, None)? {
        let head = review.head_ref.as_str();
        let id = match whole_change_id(head) {
            Some(id) => id.to_string(),
            None => {
                let Some(id) = resolved
                    .entry(head.to_string())
                    .or_insert_with(|| explicit_prefix_change(refs, graph, head))
                    .clone()
                else {
                    continue;
                };
                id
            }
        };
        // Newest first, so the first review seen for a change is the one that
        // says where it stands.
        standing
            .entry(id)
            .or_insert_with(|| GuideStandingEntry {
                state: kin_review::records::review_state_label(review.state),
                reviews: 0,
            })
            .reviews += 1;
    }
    Ok(GuideStanding { standing })
}

/// The change an explicit prefix such as `change:abc123` names, through the
/// ref grammar, which needs repository authority to search. A bare prefix is
/// a name and never a change, and a prefix the grammar refuses names none.
fn explicit_prefix_change(
    refs: &RequestRepositoryAuthority,
    graph: &kin_db::InMemoryGraph,
    head: &str,
) -> Option<String> {
    let value = head
        .strip_prefix("kin:")
        .or_else(|| head.strip_prefix("change:"))?;
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    resolve_through(refs, graph, Some(head))
        .ok()
        .map(|change| change.to_string())
}

/// Order what a subject changed into chapters and steps, and read each step.
fn assemble<I: ImpactGraph>(
    graph: &kin_db::InMemoryGraph,
    repository_authority: Option<&kin_mcp::handlers::RequestRepositoryAuthority>,
    subject: Subject<'_, I>,
) -> Result<ReviewGuide> {
    let Subject {
        kind,
        mut review,
        connections: at_head,
        before_at: base,
        after,
        change,
        head_label,
        detail_command,
        reviews,
        range,
        working_tree,
    } = subject;
    let subject_word = subject_word(kind);

    // A removed entity is gone at the head, with the relations into it, so
    // what its removal breaks is read where it still existed: the state
    // "before" is read at. Its step's connections are read there too. A
    // removed relation names its endpoints there even when both survive.
    let removes_entity = kin_review::SemanticReview::removes_an_entity(&review);
    let removes_relation = review.diff.relation_changes.iter().any(|change| {
        matches!(
            &change.kind,
            kin_review::diff::RelationChangeKind::Removed { .. }
        )
    });
    let at_base = match base {
        Some(base) if removes_entity || removes_relation => {
            let at_base = GraphAtRef::materialize(graph, &base)?;
            if removes_entity {
                kin_review::SemanticReview::read_removals_at_base(&mut review, &at_base)?;
            }
            Some(at_base)
        }
        _ => None,
    };

    let entity_risk = kin_review::entity_risk_levels(&review.diff, &review.impact);
    let mut drafts: Vec<Draft<'_>> = review
        .diff
        .entity_changes
        .iter()
        .map(|change| {
            let (kind, old, new) = match &change.kind {
                EntityChangeKind::Added(entity) => ("added", None, Some(entity)),
                EntityChangeKind::Modified { old, new } => ("modified", Some(old), Some(new)),
                EntityChangeKind::Removed { old } => ("removed", old.as_ref(), None),
            };
            Draft {
                id: change.entity_id,
                change: kind,
                old,
                new,
                risk: entity_risk.get(&change.entity_id).copied(),
            }
        })
        .collect();

    // A module container is a whole file's surface. It has no text of its own
    // to read, and it moves whenever a member does, so as a step it would be
    // an empty card repeating the member beside it.
    let module_names: HashSet<String> = drafts
        .iter()
        .filter(|draft| draft.subject().is_some_and(kin_model::is_module_container))
        .map(Draft::name)
        .collect();
    let module_containers = drafts
        .iter()
        .filter(|draft| draft.subject().is_some_and(kin_model::is_module_container))
        .count();
    drafts.retain(|draft| !draft.subject().is_some_and(kin_model::is_module_container));

    // A class moves whenever one of its methods does. When its own text held
    // still it is read through those members, which are the steps.
    let held = HeldSourceAuthority::new(graph, repository_authority);
    let sides: Vec<(Option<&Entity>, Option<&Entity>)> =
        drafts.iter().map(|draft| (draft.old, draft.new)).collect();
    let through = changed_through_members(&held, &sides, base, after);
    let mut through_members: Vec<String> = drafts
        .iter()
        .filter(|draft| through.contains(&draft.id))
        .map(Draft::name)
        .collect();
    through_members.sort();
    let containers_through_members = through_members.len();
    through_members.truncate(THROUGH_MEMBERS_LISTED_MAX);
    drafts.retain(|draft| !through.contains(&draft.id));

    let (mut added, mut modified, mut removed) = (0, 0, 0);
    for draft in &drafts {
        match draft.change {
            "added" => added += 1,
            "modified" => modified += 1,
            _ => removed += 1,
        }
    }

    // What a reviewer should see first when the change is larger than a guide
    // walks: the riskiest, then edits before removals before additions, then
    // the most consumed. The order is total, so the cut is deterministic.
    let consumed = |id: &EntityId| {
        review
            .impact
            .entity_impact(id)
            .map_or(0, kin_review::EntityImpact::inbound_total)
    };
    drafts.sort_by(|a, b| {
        let change_rank = |draft: &Draft<'_>| match draft.change {
            "modified" => 0,
            "removed" => 1,
            _ => 2,
        };
        risk_rank(b.risk)
            .cmp(&risk_rank(a.risk))
            .then_with(|| change_rank(a).cmp(&change_rank(b)))
            .then_with(|| consumed(&b.id).cmp(&consumed(&a.id)))
            .then_with(|| a.name().cmp(&b.name()))
            .then_with(|| a.id.cmp(&b.id))
    });
    let not_walked = (drafts.len() > STEPS_WALKED_MAX).then(|| {
        let rest = &drafts[STEPS_WALKED_MAX..];
        let count = |kind: &str| rest.iter().filter(|draft| draft.change == kind).count();
        GuideNotWalked {
            total: rest.len(),
            added: count("added"),
            modified: count("modified"),
            removed: count("removed"),
            message: format!(
                "This {subject_word} touches {} entities. The guide walks the {STEPS_WALKED_MAX} with \
                 the highest recorded risk and reach; `{detail_command}` lists every one.",
                drafts.len()
            ),
        }
    });
    drafts.truncate(STEPS_WALKED_MAX);

    // A container read through its members is not changed here: its own
    // code, and anything it reaches from there, held still.
    let changed: HashSet<EntityId> = review
        .diff
        .entity_changes
        .iter()
        .map(|change| change.entity_id)
        .filter(|id| !through.contains(id))
        .collect();
    let walked: HashSet<EntityId> = drafts.iter().map(|draft| draft.id).collect();
    let by_id: HashMap<EntityId, &Draft<'_>> =
        drafts.iter().map(|draft| (draft.id, draft)).collect();

    // Connections, read from the graph state the subject names.
    let mut wiring: HashMap<EntityId, Wiring> = HashMap::new();
    let mut depends: BTreeSet<(EntityId, EntityId)> = BTreeSet::new();
    let mut ties: Vec<(EntityId, EntityId)> = Vec::new();
    for draft in &drafts {
        let base_side = at_base.as_ref().filter(|_| draft.change == "removed");
        let all = match base_side {
            Some(at_base) => at_base.get_all_relations_for_entity(&draft.id)?,
            None => at_head.get_all_relations_for_entity(&draft.id)?,
        };
        let entry = wiring.entry(draft.id).or_default();
        let mut seen: HashSet<EntityId> = HashSet::new();
        let mut reachers: Vec<Reacher> = Vec::new();
        // One row per source, and a source that both imports and calls is
        // shown by its call, so the use is read before the import.
        let mut inbound = kin_review::inbound_impact_relations(&all, &draft.id);
        inbound.sort_by(|a, b| {
            is_import(a.kind)
                .cmp(&is_import(b.kind))
                .then_with(|| a.id.cmp(&b.id))
        });
        for relation in inbound {
            let Some(source) = relation.src.as_entity() else {
                continue;
            };
            if source == draft.id || !seen.insert(source) {
                continue;
            }
            let changed_here = changed.contains(&source);
            // A changed consumer this change removed is absent at the change,
            // so its base-side record names it.
            let resolved = match base_side {
                Some(at_base) => at_base.get_entity(&source)?,
                None => at_head.get_entity(&source)?,
            };
            let source_entity = resolved
                .as_ref()
                .or_else(|| by_id.get(&source).and_then(|other| other.subject()));
            let Some(source_entity) = source_entity else {
                continue;
            };
            if is_import(relation.kind) && kin_model::is_module_container(source_entity) {
                entry.importing_modules.push(entity_ref(source_entity));
                continue;
            }
            let is_test = is_test_entity(source_entity)
                || matches!(relation.kind, RelationKind::Tests | RelationKind::Covers);
            reachers.push(Reacher {
                relation: relation.clone(),
                entity: source_entity.clone(),
                changed_here,
                is_test,
                canonical: source,
                overloads: 0,
                labelled: false,
            });
        }
        let canonical = |entity: &Entity| match base_side {
            Some(at_base) => canonical_callable(at_base, entity),
            None => canonical_callable(at_head, entity),
        };
        for reacher in fold_declarations(reachers, &canonical)? {
            let mut row = connection(&reacher.relation, &reacher.entity, reacher.changed_here);
            row.overloads = reacher.overloads;
            if reacher.labelled {
                row.entity.parameters = parameters_of(&reacher.entity.signature);
            }
            // Counted by the callable a reader knows, so stubs a later step
            // lists without their definition are not a second place.
            let source = reacher.canonical;
            if reacher.is_test {
                if !reacher.changed_here {
                    entry.outside_tests.insert(source);
                }
                entry.tests.push(row);
            } else {
                if !reacher.changed_here {
                    entry.outside_consumers.insert(source);
                }
                entry.consumers.push(row);
            }
        }
        for relation in &all {
            if !ties_chapter(relation.kind) {
                continue;
            }
            let (Some(source), Some(target)) = (relation.src.as_entity(), relation.dst.as_entity())
            else {
                continue;
            };
            if source == target || !walked.contains(&source) || !walked.contains(&target) {
                continue;
            }
            ties.push((source, target));
            // A container is read before its members; everything else is read
            // dependency first, so the thing relied on comes before its user.
            if relation.kind == RelationKind::Contains {
                depends.insert((target, source));
            } else {
                depends.insert((source, target));
            }
        }
    }
    // A removed entity has no adjacency at the change, so the relations this
    // change removed are what still ties it to the rest.
    for relation_change in &review.diff.relation_changes {
        let relation = match &relation_change.kind {
            kin_review::diff::RelationChangeKind::Added(relation)
            | kin_review::diff::RelationChangeKind::Removed { old: relation }
            | kin_review::diff::RelationChangeKind::Modified { new: relation, .. } => relation,
        };
        let (Some(source), Some(target)) = (relation.src.as_entity(), relation.dst.as_entity())
        else {
            continue;
        };
        if source != target
            && ties_chapter(relation.kind)
            && walked.contains(&source)
            && walked.contains(&target)
        {
            ties.push((source, target));
            if relation.kind == RelationKind::Contains {
                depends.insert((target, source));
            } else {
                depends.insert((source, target));
            }
        }
    }

    // Chapters: the connected groups of changed entities.
    let mut parents: HashMap<EntityId, EntityId> =
        drafts.iter().map(|draft| (draft.id, draft.id)).collect();
    for (a, b) in ties {
        let (root_a, root_b) = (find_root(&mut parents, a), find_root(&mut parents, b));
        if root_a != root_b {
            // The smaller id becomes the root so the grouping does not depend
            // on the order relations were read in.
            let (keep, fold) = if root_a < root_b {
                (root_a, root_b)
            } else {
                (root_b, root_a)
            };
            parents.insert(fold, keep);
        }
    }
    let mut groups: BTreeMap<EntityId, Vec<EntityId>> = BTreeMap::new();
    for draft in &drafts {
        let root = find_root(&mut parents, draft.id);
        groups.entry(root).or_default().push(draft.id);
    }

    let reach = |id: &EntityId| {
        wiring
            .get(id)
            .map_or(0, |entry| entry.consumers.len() + entry.tests.len())
    };
    let tie_break = |id: &EntityId| {
        let draft = by_id[id];
        (
            draft.is_test(),
            std::cmp::Reverse(reach(id)),
            draft.name(),
            *id,
        )
    };

    struct Planned {
        kind: &'static str,
        members: Vec<EntityId>,
        /// Sort key for the remainder chapters, which follow the rest.
        tail: u8,
    }
    let mut planned: Vec<Planned> = Vec::new();
    let mut loose: BTreeMap<&'static str, Vec<EntityId>> = BTreeMap::new();
    let only_one_entity = drafts.len() == 1;
    for members in groups.into_values() {
        if members.len() > 1 {
            planned.push(Planned {
                kind: "connected",
                members: order_chapter(&members, &depends, &tie_break),
                tail: 0,
            });
            continue;
        }
        let id = members[0];
        let draft = by_id[&id];
        // One entity earns a chapter of its own when something reaches it or
        // the review ranked it above low. The rest read better together.
        if only_one_entity || reach(&id) > 0 || risk_rank(draft.risk) >= 2 {
            planned.push(Planned {
                kind: "single",
                members,
                tail: 0,
            });
        } else {
            loose.entry(draft.change).or_default().push(id);
        }
    }
    for (change, tail) in [("modified", 1), ("added", 2), ("removed", 3)] {
        if let Some(mut members) = loose.remove(change) {
            members.sort_by_key(|id| tie_break(id));
            planned.push(Planned {
                kind: "group",
                members,
                tail,
            });
        }
    }
    let chapter_risk = |members: &[EntityId]| {
        members
            .iter()
            .map(|id| risk_rank(by_id[id].risk))
            .max()
            .unwrap_or(0)
    };
    let chapter_reach = |members: &[EntityId]| members.iter().map(&reach).sum::<usize>();
    planned.sort_by(|a, b| {
        a.tail
            .cmp(&b.tail)
            .then_with(|| chapter_risk(&b.members).cmp(&chapter_risk(&a.members)))
            .then_with(|| chapter_reach(&b.members).cmp(&chapter_reach(&a.members)))
            .then_with(|| b.members.len().cmp(&a.members.len()))
            .then_with(|| {
                by_id[&a.members[0]]
                    .name()
                    .cmp(&by_id[&b.members[0]].name())
            })
            .then_with(|| a.members[0].cmp(&b.members[0]))
    });

    // Findings anchored to an entity, keyed the way the review anchors them.
    let mut anchored: HashMap<(String, u32, u32), Vec<GuideFinding>> = HashMap::new();
    for comment in &review.inline_comments {
        anchored
            .entry((comment.file.clone(), comment.start_line, comment.end_line))
            .or_default()
            .push(GuideFinding {
                kind: label(comment.kind),
                severity: super::review::inline_comment_severity(comment.kind).to_string(),
                message: comment.message.clone(),
            });
    }

    // What reaches the change from outside it, counted before the steps take
    // the wiring apart.
    let mut outside_consumers: BTreeSet<EntityId> = BTreeSet::new();
    let mut outside_tests: BTreeSet<EntityId> = BTreeSet::new();
    let mut outside_by_id: HashMap<EntityId, usize> = HashMap::new();
    for (id, entry) in &wiring {
        outside_consumers.extend(&entry.outside_consumers);
        outside_tests.extend(&entry.outside_tests);
        outside_by_id.insert(*id, entry.outside_consumers.len());
    }

    let name_of = |node: &kin_model::GraphNodeId, removed: bool| -> Result<String> {
        let Some(id) = node.as_entity() else {
            return Ok(kin_review::graph_node_name(graph, node).unwrap_or_else(|| node.to_string()));
        };
        if let Some(entity) = by_id
            .get(&id)
            .and_then(|draft| if removed { draft.old } else { draft.new })
        {
            return Ok(entity.name.clone());
        }
        // An entity name is structural state, just like the relation. Read
        // the side this row describes, including for endpoints not walked
        // here, and never fill a historical gap from today's graph.
        let entity = if removed {
            match &at_base {
                Some(at_base) => at_base.get_entity(&id)?,
                None => None,
            }
        } else {
            at_head.get_entity(&id)?
        };
        // An endpoint the guide does not walk is told apart by what contains
        // it, read on the same side as its name.
        let within = match (&entity, removed, &at_base) {
            (Some(_), _, _) if by_id.contains_key(&id) => None,
            (Some(_), true, Some(at_base)) => within_of(at_base, &id)?,
            (Some(_), false, _) => within_of(at_head, &id)?,
            _ => None,
        };
        Ok(match (entity, within) {
            (Some(entity), Some(within)) => format!("{} in {within}", entity.name),
            (Some(entity), None) => entity.name,
            (None, _) => node.to_string(),
        })
    };
    // Names two walked steps share, where only parameters tell them apart.
    let shared_step_names: HashSet<String> = {
        let mut seen: HashSet<String> = HashSet::new();
        drafts
            .iter()
            .map(Draft::name)
            .filter(|name| !seen.insert(name.clone()))
            .collect()
    };
    // The consumers outside the change, by what the changes they reach ask
    // of them. One that reaches two changed entities is counted by the one
    // that asks the most.
    let mut not_updated: BTreeSet<EntityId> = BTreeSet::new();
    let mut may_need_review: BTreeSet<EntityId> = BTreeSet::new();
    let mut need_no_update: BTreeSet<EntityId> = BTreeSet::new();
    let mut build_step = |id: &EntityId| -> Result<GuideStep> {
        let draft = by_id[id];
        let mut entry = wiring.remove(id).unwrap_or_default();
        sort_connections(&mut entry.consumers);
        sort_connections(&mut entry.tests);
        let fields = match (draft.old, draft.new) {
            (Some(old), Some(new)) => field_changes(old, new),
            _ => Vec::new(),
        };

        let mut findings = draft
            .new
            .and_then(|entity| entity.span.as_ref())
            .and_then(|span| {
                anchored.remove(&(span.file.to_string(), span.start_line, span.end_line))
            })
            .unwrap_or_default();
        // The review anchors no finding to a removal, since the place it was
        // is someone else's at the head. Removing what others still reach is
        // the plainest break there is, so the step says so from the graph.
        if draft.change == "removed" && !entry.outside_consumers.is_empty() {
            let reaching = entry.outside_consumers.len();
            findings.push(GuideFinding {
                kind: "breaking".to_string(),
                severity: "error".to_string(),
                message: format!(
                    "Removed while {} still {} on it.",
                    plural(reaching, "place", "places"),
                    if reaching == 1 { "relies" } else { "rely" }
                ),
            });
        }
        let verdict = consumer_verdict(draft, &findings, entry.outside_consumers.len());
        match verdict.verdict.as_str() {
            NOT_UPDATED => not_updated.extend(&entry.outside_consumers),
            MAY_NEED_REVIEW => may_need_review.extend(&entry.outside_consumers),
            NEEDS_NO_UPDATE => need_no_update.extend(&entry.outside_consumers),
            _ => {}
        }
        let narration = step_narration(draft, &fields, &entry, &verdict);

        let node = kin_model::GraphNodeId::Entity(*id);
        let mut relation_changes = Vec::new();
        for relation_change in &review.diff.relation_changes {
            // Evidence that only moved with the lines around it is counted
            // once for the whole subject, never listed as a step's own.
            if is_position_only(relation_change) {
                continue;
            }
            let (change, relation) = match &relation_change.kind {
                kin_review::diff::RelationChangeKind::Added(relation) => ("added", relation),
                kin_review::diff::RelationChangeKind::Removed { old } => ("removed", old),
                kin_review::diff::RelationChangeKind::Modified { new, .. } => ("modified", new),
            };
            let (direction, other) = if relation.dst == node {
                ("inbound", &relation.src)
            } else if relation.src == node {
                ("outbound", &relation.dst)
            } else {
                continue;
            };
            relation_changes.push(GuideRelationChange {
                change: change.to_string(),
                relation: label(relation.kind),
                direction: direction.to_string(),
                other: name_of(other, change == "removed")?,
                relation_id: relation.id.to_string(),
            });
        }
        relation_changes.sort_by(|a, b| {
            a.change
                .cmp(&b.change)
                .then_with(|| a.relation.cmp(&b.relation))
                .then_with(|| a.other.cmp(&b.other))
                .then_with(|| a.relation_id.cmp(&b.relation_id))
        });
        let relation_changes_total = relation_changes.len();
        relation_changes.truncate(RELATION_CHANGES_LISTED_MAX);

        let consumers_total = entry.consumers.len();
        let consumers_changed_here = entry
            .consumers
            .iter()
            .filter(|connection| connection.changed_here)
            .count();
        let tests_total = entry.tests.len();
        entry.consumers.truncate(CONNECTIONS_LISTED_MAX);
        entry.tests.truncate(CONNECTIONS_LISTED_MAX);
        // What contains each entity shown, read on the side its connections
        // were: where a removed entity still existed, else at the head. Only
        // the rows kept are read.
        let base_side = at_base.as_ref().filter(|_| draft.change == "removed");
        let within = |id: &EntityId| -> Result<Option<String>> {
            match base_side {
                Some(at_base) => within_of(at_base, id),
                None => within_of(at_head, id),
            }
        };
        for row in entry.consumers.iter_mut().chain(entry.tests.iter_mut()) {
            if let Ok(uuid) = uuid::Uuid::parse_str(&row.entity.id) {
                row.entity.within = within(&EntityId(uuid))?;
            }
        }
        let step_within = within(id)?;
        entry
            .importing_modules
            .sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        let importing_modules_total = entry.importing_modules.len();
        entry.importing_modules.truncate(CONNECTIONS_LISTED_MAX);

        let subject = draft.subject();
        Ok(GuideStep {
            entity_id: id.to_string(),
            name: draft.name(),
            kind: subject.map_or_else(|| "unknown".to_string(), |entity| label(entity.kind)),
            role: subject.map_or_else(|| "unknown".to_string(), |entity| label(entity.role)),
            language: subject.map(|entity| entity.language.to_string()),
            within: step_within,
            file: subject.and_then(entity_file),
            line: subject.and_then(entity_presentation_start_line),
            parameters: if shared_step_names.contains(&draft.name()) {
                subject.and_then(|entity| parameters_of(&entity.signature))
            } else {
                None
            },
            change: draft.change.to_string(),
            risk: draft.risk.map_or("not_assessed", risk_label).to_string(),
            narration,
            fields,
            before: match (draft.old, base) {
                (Some(old), Some(base)) => Some(read_body(&held, old, EntitySourceScope::At(base))),
                _ => None,
            },
            after: draft.new.map(|new| read_body(&held, new, after)),
            connections: GuideConnections {
                consumers: entry.consumers,
                consumers_total,
                consumers_changed_here,
                tests: entry.tests,
                tests_total,
                importing_modules: entry.importing_modules,
                importing_modules_total,
                relation_changes,
                relation_changes_total,
            },
            consumer_verdict: Some(verdict),
            findings,
            evidence: GuideEvidence {
                base_change: base.map(|base| base.to_string()),
                head_change: head_label.clone(),
                fingerprint_before: draft.old.map(fingerprint),
                fingerprint_after: draft.new.map(fingerprint),
            },
        })
    };

    let chapters: Vec<GuideChapter> = planned
        .iter()
        .map(|plan| {
            let lead = by_id[&plan.members[0]];
            let outside: usize = plan
                .members
                .iter()
                .filter_map(|id| outside_by_id.get(id))
                .sum();
            let steps = plan
                .members
                .iter()
                .map(&mut build_step)
                .collect::<Result<Vec<_>>>()?;
            let lead_verdict = steps
                .first()
                .and_then(|step| step.consumer_verdict.as_ref())
                .map_or("", |verdict| verdict.verdict.as_str());
            let (title, reason) = match plan.kind {
                "connected" => (
                    format!(
                        "{} and {}",
                        lead.name(),
                        plural(
                            plan.members.len() - 1,
                            "connected change",
                            "connected changes"
                        )
                    ),
                    format!(
                        "These {} entities changed together and the graph connects them, so \
                         they read as one unit. What the others rely on comes first.",
                        plan.members.len()
                    ),
                ),
                "single" => (
                    // Named for what the chapter holds beyond the entity, so
                    // its title is not its only step's name over again.
                    match (lead.change, outside) {
                        (_, 0) => lead.name(),
                        ("removed", _) => format!("{} and what still reached it", lead.name()),
                        _ => format!("{} and what reaches it", lead.name()),
                    },
                    match (lead.change, outside) {
                        (_, 0) => "This entity changed on its own.".to_string(),
                        ("removed", count) => format!(
                            "This entity was removed. {} still relied on it.",
                            plural(count, "place", "places")
                        ),
                        (_, count) => format!(
                            "This entity changed on its own. {}",
                            outside_consumers_sentence(lead_verdict, count)
                        ),
                    },
                ),
                _ => (
                    match lead.change {
                        "added" => "New entities",
                        "removed" => "Removed entities",
                        _ => "Other modifications",
                    }
                    .to_string(),
                    format!(
                        "Nothing connects these to the rest of this {subject_word} or reaches \
                         them from outside it."
                    ),
                ),
            };
            let risk = plan
                .members
                .iter()
                .filter_map(|id| by_id[id].risk)
                .max_by_key(|level| risk_rank(Some(*level)))
                .map_or("not_assessed", risk_label)
                .to_string();
            Ok(GuideChapter {
                title,
                kind: plan.kind.to_string(),
                reason,
                risk,
                steps,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let risk = &review.risk;
    // A module moves with every member, and its coverage is its members':
    // a gap named for the module would contradict the tests that reach them.
    let test_coverage_gaps: Vec<String> = risk
        .test_coverage_gaps
        .iter()
        .filter(|gap| !gap_names_one_of(gap, &module_names))
        .cloned()
        .collect();
    let finding_count = review.inline_comments.len()
        + risk.breaking_changes.len()
        + risk.contract_violations.len()
        + risk.work_risks.len()
        + test_coverage_gaps.len()
        + risk.notes.len();
    // Each consumer once, by the most any change it reaches asks of it.
    let consumers_may_need_review = may_need_review.difference(&not_updated).count();
    let consumers_need_no_update = need_no_update
        .iter()
        .filter(|id| !not_updated.contains(id) && !may_need_review.contains(id))
        .count();
    let consumer_verdicts = ConsumerVerdictCounts {
        not_updated: not_updated.len(),
        may_need_review: consumers_may_need_review,
        need_no_update: consumers_need_no_update,
    };
    let narration = summary_narration(
        subject_word,
        added,
        modified,
        removed,
        outside_consumers.len(),
        outside_tests.len(),
        chapters.len(),
        &consumer_verdicts,
    );
    let relation_changes_position_only = review
        .diff
        .relation_changes
        .iter()
        .filter(|change| is_position_only(change))
        .count();
    Ok(ReviewGuide {
        schema: GUIDE_SCHEMA.to_string(),
        change,
        summary: GuideSummary {
            risk: risk_label(risk.overall_risk).to_string(),
            added,
            modified,
            removed,
            relation_changes: review.diff.relation_changes.len() - relation_changes_position_only,
            relation_changes_position_only,
            provenance_only_entity_changes: review.diff.provenance_only_entity_changes,
            module_containers,
            containers_through_members,
            through_members,
            recorded_consumers: outside_consumers.len(),
            recorded_tests: outside_tests.len(),
            consumers_not_updated: consumer_verdicts.not_updated,
            consumers_may_need_review: consumer_verdicts.may_need_review,
            consumers_need_no_update: consumer_verdicts.need_no_update,
            finding_count,
            completeness: completeness(&review),
            narration,
        },
        chapters,
        findings: GuideFindings {
            breaking_changes: risk.breaking_changes.clone(),
            contract_violations: risk.contract_violations.clone(),
            work_risks: risk.work_risks.clone(),
            test_coverage_gaps,
            notes: risk.notes.clone(),
        },
        not_walked,
        reviews: reviews.reviews,
        reviews_total: reviews.reviews_total,
        kind: kind.to_string(),
        range,
        working_tree,
    })
}

/// What a range is called: by what its changes say, not by two ids.
///
/// The newest change's own title names the range, with how many came before
/// it. A range of one change is that change's title, and an empty one says
/// what it was asked between.
fn range_title(
    newest_first: &[SemanticChange],
    changes_total: usize,
    base_words: &str,
    head_words: &str,
) -> String {
    let newest = newest_first
        .first()
        .map(|change| label_text(&first_line(&change.message)))
        .filter(|title| !title.is_empty());
    match (changes_total, newest) {
        (0, _) => format!("Nothing between {base_words} and {head_words}"),
        (1, Some(title)) => title,
        (count, Some(title)) => format!(
            "\u{201c}{title}\u{201d} and {}",
            plural(count - 1, "earlier change", "earlier changes")
        ),
        (count, None) => format!(
            "{} from {base_words} to {head_words}",
            plural(count, "change", "changes")
        ),
    }
}

/// The sentence that names the containers read through their members, or
/// nothing when there are none.
fn through_members_sentence(summary: &GuideSummary) -> Option<String> {
    let count = summary.containers_through_members;
    if count == 0 {
        return None;
    }
    let mut names: Vec<String> = summary
        .through_members
        .iter()
        .map(|name| format!("`{name}`"))
        .collect();
    if count > names.len() {
        names.push(plural(count - names.len(), "other", "others"));
    }
    Some(format!(
        "{} changed only through {} members and {} read through them.",
        join_and(&names),
        if count == 1 { "its" } else { "their" },
        if count == 1 { "is" } else { "are" }
    ))
}

/// What a guide of this kind calls the thing it walks.
fn subject_word(kind: &str) -> &'static str {
    match kind {
        "working" => "work",
        "range" => "range",
        _ => "change",
    }
}

/// A ref as a sentence carries it. A whole change id is sixty-four
/// characters and a whole Git commit id forty, and neither says anything a
/// reader can hold, so each is cut to the twelve every other surface shows. A
/// branch name or anything else is kept as it was asked.
fn ref_words(reference: &str) -> &str {
    let id = reference
        .strip_prefix("change:")
        .or_else(|| reference.strip_prefix("kin:"))
        .or_else(|| reference.strip_prefix("git:"))
        .unwrap_or(reference);
    if matches!(id.len(), 40 | 64) && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        &id[..12]
    } else {
        reference
    }
}

/// How many recorded consumers outside the subject fall under each verdict.
struct ConsumerVerdictCounts {
    not_updated: usize,
    may_need_review: usize,
    need_no_update: usize,
}

/// What a single chapter says of the `count` places outside the change that
/// rely on its one entity, by that entity's verdict.
fn outside_consumers_sentence(verdict: &str, count: usize) -> String {
    let places = if count == 1 {
        "The 1 place that relies on it".to_string()
    } else {
        format!("None of the {count} places that rely on it")
    };
    let one = count == 1;
    match verdict {
        NEEDS_NO_UPDATE if one => format!("{places} did not change, and it needs no update."),
        NEEDS_NO_UPDATE => format!("{places} changed, and they need no update."),
        MAY_NEED_REVIEW if one => format!("{places} did not change, and it may need review."),
        MAY_NEED_REVIEW => format!("{places} changed, and they may need review."),
        NOT_UPDATED if one => format!("{places} was not updated."),
        NOT_UPDATED => format!("{places} was updated."),
        _ if one => format!("{places} did not change."),
        _ => format!("{places} changed."),
    }
}

/// What the consumers outside the subject need, in one sentence, or nothing
/// when none reaches it.
fn consumers_need_sentence(consumers: usize, counts: &ConsumerVerdictCounts) -> Option<String> {
    if consumers == 0 {
        return None;
    }
    let one = consumers == 1;
    if counts.not_updated == consumers {
        return Some(if one {
            "That place was not updated, and the change breaks what it relies on.".to_string()
        } else {
            "None of them was updated, and the change breaks what they rely on.".to_string()
        });
    }
    if counts.may_need_review == consumers {
        return Some(if one {
            "That place did not change and the code it relies on did, so it may need review."
                .to_string()
        } else {
            "None of them changed and the code they rely on did, so they may need review."
                .to_string()
        });
    }
    if counts.need_no_update == consumers {
        return Some(if one {
            "That place needs no update: what it relies on kept its signature and behaves as \
             it did."
                .to_string()
        } else {
            "None of them needs an update: what they rely on kept its signature and behaves as \
             it did."
                .to_string()
        });
    }
    let mut parts = Vec::new();
    if counts.not_updated > 0 {
        parts.push(format!(
            "{} not updated",
            match counts.not_updated {
                1 => "1 was".to_string(),
                count => format!("{count} were"),
            }
        ));
    }
    if counts.may_need_review > 0 {
        parts.push(format!("{} may need review", counts.may_need_review));
    }
    if counts.need_no_update > 0 {
        parts.push(format!(
            "{} no update",
            match counts.need_no_update {
                1 => "1 needs".to_string(),
                count => format!("{count} need"),
            }
        ));
    }
    (!parts.is_empty()).then(|| format!("Of those places, {}.", join_and(&parts)))
}

#[allow(clippy::too_many_arguments)]
fn summary_narration(
    subject: &str,
    added: usize,
    modified: usize,
    removed: usize,
    consumers: usize,
    tests: usize,
    chapters: usize,
    verdicts: &ConsumerVerdictCounts,
) -> String {
    let mut parts = Vec::new();
    if modified > 0 {
        parts.push(format!(
            "modified {}",
            plural(modified, "entity", "entities")
        ));
    }
    if added > 0 {
        parts.push(format!("added {}", plural(added, "entity", "entities")));
    }
    if removed > 0 {
        parts.push(format!("removed {}", plural(removed, "entity", "entities")));
    }
    if parts.is_empty() {
        return format!("This {subject} holds no entity a review can act on.");
    }
    let what = join_and(&parts);
    let them = if added + modified + removed == 1 {
        "it"
    } else {
        "them"
    };
    // What relies on the subject from outside it, then what that asks of
    // them, then the tests, so each sentence names what the one before it did.
    let mut sentences = Vec::new();
    if consumers == 0 && tests == 0 {
        sentences.push(format!("Nothing outside the {subject} reaches {them}."));
    }
    if consumers > 0 {
        sentences.push(format!(
            "{} outside the {subject} {} on {them}.",
            plural(consumers, "place", "places"),
            if consumers == 1 { "relies" } else { "rely" }
        ));
        sentences.extend(consumers_need_sentence(consumers, verdicts));
    }
    if tests > 0 {
        let reach = if tests == 1 { "reaches" } else { "reach" };
        sentences.push(if consumers > 0 {
            format!("{} {reach} {them}.", plural(tests, "test", "tests"))
        } else {
            format!(
                "{} outside the {subject} {reach} {them}.",
                plural(tests, "test", "tests")
            )
        });
    }
    format!(
        "This {subject} {what}, read here in {}. {}",
        plural(chapters, "chapter", "chapters"),
        sentences.join(" ")
    )
}

/// The review of one change against its first parent, with impact read from
/// the state already materialized at the change.
///
/// The range is everything the change reaches that its first parent does not.
/// For an ordinary change that is the change alone. For a merge it is what the
/// merge brought into the first parent's line, and the membership test is what
/// keeps it to that: the store's backward walk stops only at the literal base,
/// so it crosses a merge into the other parent's whole history, and a walk
/// that accepted every change it met would report that history's first
/// additions as this change's own.
///
/// A state that cannot be materialized is an error. Any other failure to walk
/// the range falls back to the change's own deltas, which is what a change
/// with no parent is reviewed from too.
fn review_at(
    graph: &kin_db::InMemoryGraph,
    at_head: &GraphAtRef<'_, kin_db::InMemoryGraph>,
    change_id: &SemanticChangeId,
    semantic_change: &SemanticChange,
    escape_evidence: Option<&kin_review::enrichment::EscapeEvidence<'_>>,
) -> Result<kin_review::Review> {
    let from_deltas = || -> Result<kin_review::Review> {
        Ok(kin_review::SemanticReview::review_from_diff_with_source(
            kin_review::diff_from_change(semantic_change),
            graph,
            escape_evidence,
        )?)
    };
    let Some(parent) = semantic_change.parents.first() else {
        return from_deltas();
    };
    let base_ancestry = kin_review::ref_graph::collect_ancestry(graph, parent)?;
    match kin_review::SemanticReview::create_review_scoped_with_source(
        parent,
        change_id,
        graph,
        at_head,
        |id| at_head.ancestry_contains(id) && !base_ancestry.contains(id),
        escape_evidence,
    ) {
        Ok(review) => Ok(review),
        Err(error @ kin_review::ReviewError::RefStateUnavailable { .. }) => Err(error.into()),
        Err(_) => from_deltas(),
    }
}

/// The twelve characters of a change id a person reads and types.
fn short(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

/// The guide as terminal text: the same chapters and steps the page shows.
pub(crate) fn guide_text(guide: &ReviewGuide) -> String {
    let mut text = String::new();
    let change = &guide.change;
    let summary = &guide.summary;
    writeln!(text, "Guided review: {}", change.title).unwrap();
    match (guide.kind.as_str(), &guide.range) {
        ("working", _) => {
            match &change.base {
                Some(base) => writeln!(
                    text,
                    "The working tree of {}, compared with change {}",
                    change.author,
                    short(base)
                ),
                None => writeln!(text, "The working tree of {}", change.author),
            }
            .unwrap();
            // What the header says about the tree itself: how many files
            // differ, and whether the graph had caught up with them.
            for line in change
                .message
                .lines()
                .skip(1)
                .filter(|line| !line.is_empty())
            {
                writeln!(text, "{line}").unwrap();
            }
            if let Some(tree) = &guide.working_tree {
                write_working_state(&mut text, guide, tree);
            }
        }
        ("range", Some(range)) => {
            writeln!(
                text,
                "{}, before read at {}, after at {}",
                plural(range.changes_total, "change", "changes"),
                short(&range.merge_base),
                short(&change.id)
            )
            .unwrap();
            for listed in range.changes.iter().take(RANGE_CHANGES_PRINTED_MAX) {
                writeln!(text, "  {}  {}", short(&listed.id), listed.title).unwrap();
            }
            if range.changes_total > RANGE_CHANGES_PRINTED_MAX {
                writeln!(
                    text,
                    "  and {} more",
                    range.changes_total - RANGE_CHANGES_PRINTED_MAX
                )
                .unwrap();
            }
        }
        _ => writeln!(
            text,
            "Change {} by {}, {}",
            short(&change.id),
            change.author,
            readable_time(&change.timestamp)
        )
        .unwrap(),
    }
    writeln!(text, "Risk: {} (recorded evidence)", summary.risk).unwrap();
    writeln!(text, "{}", summary.narration).unwrap();
    writeln!(text, "{}", summary.completeness.message).unwrap();
    if summary.provenance_only_entity_changes > 0 {
        writeln!(
            text,
            "{} recorded modifications moved only a span or its source and are not walked.",
            summary.provenance_only_entity_changes
        )
        .unwrap();
    }

    match summary.module_containers {
        0 => {}
        1 => writeln!(
            text,
            "1 module container changed with its members and is read through them."
        )
        .unwrap(),
        count => writeln!(
            text,
            "{count} module containers changed with their members and are read through them."
        )
        .unwrap(),
    }
    if let Some(line) = through_members_sentence(summary) {
        writeln!(text, "{line}").unwrap();
    }
    if summary.relation_changes_position_only > 0 {
        writeln!(
            text,
            "{} moved only with the lines around {} and {} not walked.",
            plural(
                summary.relation_changes_position_only,
                "recorded relation",
                "recorded relations"
            ),
            if summary.relation_changes_position_only == 1 {
                "it"
            } else {
                "them"
            },
            if summary.relation_changes_position_only == 1 {
                "is"
            } else {
                "are"
            }
        )
        .unwrap();
    }

    let chapter_count = guide.chapters.len();
    for (chapter_index, chapter) in guide.chapters.iter().enumerate() {
        writeln!(text).unwrap();
        writeln!(
            text,
            "Chapter {} of {chapter_count}: {}",
            chapter_index + 1,
            chapter.title
        )
        .unwrap();
        writeln!(text, "  {}", chapter.reason).unwrap();
        let step_count = chapter.steps.len();
        for (step_index, step) in chapter.steps.iter().enumerate() {
            writeln!(text).unwrap();
            let kind = step.kind.replace('_', " ");
            let what = match &step.within {
                Some(within) => format!("{kind} in {within}"),
                None => kind,
            };
            writeln!(
                text,
                "  Step {} of {step_count}: {}{} ({what})  {}",
                step_index + 1,
                step.name,
                step.parameters.as_deref().unwrap_or_default(),
                step.change
            )
            .unwrap();
            writeln!(text, "    {}", step.narration).unwrap();
            for field in &step.fields {
                if let (Some(before), Some(after)) = (&field.before, &field.after) {
                    if field.field != "doc_summary" {
                        writeln!(text, "    {}: {before}", field.field).unwrap();
                        writeln!(text, "    {}  {after}", " ".repeat(field.field.len())).unwrap();
                    }
                }
            }
            write_step_body(&mut text, step);
            write_connections(
                &mut text,
                "Consumers",
                &step.connections.consumers,
                step.connections.consumers_total,
            );
            write_connections(
                &mut text,
                "Tests",
                &step.connections.tests,
                step.connections.tests_total,
            );
            if step.connections.importing_modules_total > 0 {
                writeln!(
                    text,
                    "    Imported by {}: {}",
                    plural(
                        step.connections.importing_modules_total,
                        "module",
                        "modules"
                    ),
                    step.connections
                        .importing_modules
                        .iter()
                        .map(|module| module.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .unwrap();
            }
            // The step's own first line already says an entity is new, so the
            // review's note of the same fact is not printed under it again.
            for finding in step
                .findings
                .iter()
                .filter(|finding| finding.kind != "added")
            {
                writeln!(text, "    {}: {}", finding.severity, finding.message).unwrap();
            }
            writeln!(
                text,
                "    Evidence: entity:{}; {}",
                step.entity_id,
                match (&step.evidence.base_change, guide.kind == "working") {
                    (Some(base), true) =>
                        format!("before at {}, after in the working tree", short(base)),
                    (None, true) => "read from the working tree".to_string(),
                    (Some(base), false) => format!(
                        "before at {}, after at {}",
                        short(base),
                        short(&step.evidence.head_change)
                    ),
                    (None, false) => format!("recorded at {}", short(&step.evidence.head_change)),
                }
            )
            .unwrap();
        }
    }

    if let Some(not_walked) = &guide.not_walked {
        writeln!(text).unwrap();
        writeln!(text, "{}", not_walked.message).unwrap();
    }
    write_overall_findings(&mut text, guide);
    write_recorded_reviews(&mut text, guide);
    writeln!(text).unwrap();
    writeln!(
        text,
        "Read it as a page: kin review open {}",
        target_words(guide)
    )
    .unwrap();
    text
}

/// Whether uncommitted work was measured, and work that no entity shows. A
/// tree that differs while no entity changed is still uncommitted, and the
/// walk below it would otherwise read as a clean tree.
fn write_working_state(text: &mut String, guide: &ReviewGuide, tree: &GuideWorkingTree) {
    if let Some(why) = &tree.unmeasured {
        writeln!(
            text,
            "Not measured before this was read ({why}), so it may be a moment behind the \
             working tree."
        )
        .unwrap();
    }
    if tree.differs && tree.derived && guide.chapters.is_empty() && guide.not_walked.is_none() {
        writeln!(
            text,
            "No entity changed with it. It is still uncommitted and is recorded with the next \
             commit."
        )
        .unwrap();
    }
}

/// How many of a range's changes the terminal names before it counts the rest.
const RANGE_CHANGES_PRINTED_MAX: usize = 12;

/// What a guide walks, as the argument that asks for it again.
fn target_words(guide: &ReviewGuide) -> String {
    match (guide.kind.as_str(), &guide.range) {
        ("working", _) => "--working".to_string(),
        ("range", Some(range)) => format!(
            "{}..{}",
            range.base_ref,
            if range.head_ref == "HEAD" {
                ""
            } else {
                range.head_ref.as_str()
            }
        ),
        _ => short(&guide.change.id).to_string(),
    }
}

/// The command that prints every detail of what a guide walks.
fn detail_words(guide: &ReviewGuide) -> String {
    match (guide.kind.as_str(), &guide.range) {
        ("working", _) => "kin diff".to_string(),
        ("range", Some(range)) => {
            format!("kin review shadow {}..{}", range.base_ref, range.head_ref)
        }
        _ => format!("kin review {} --details", short(&guide.change.id)),
    }
}

/// How many whole-change findings of one kind the terminal lists by name.
const FINDINGS_LISTED_MAX: usize = 8;

/// The review's findings for the whole change, so the risk the guide opens
/// with is explained by the time it ends. Notes are counted, not listed: they
/// are the review's detail, and `--details` prints every one.
fn write_overall_findings(text: &mut String, guide: &ReviewGuide) {
    let findings = &guide.findings;
    let groups = [
        ("Breaking changes", &findings.breaking_changes),
        ("Contract violations", &findings.contract_violations),
        ("Work in progress affected", &findings.work_risks),
        ("Coverage gaps", &findings.test_coverage_gaps),
    ];
    if groups.iter().all(|(_, items)| items.is_empty()) && findings.notes.is_empty() {
        return;
    }
    writeln!(text).unwrap();
    writeln!(text, "Recorded findings for the whole change:").unwrap();
    for (title, items) in groups {
        if items.is_empty() {
            continue;
        }
        writeln!(text, "  {title} ({}):", items.len()).unwrap();
        for item in items.iter().take(FINDINGS_LISTED_MAX) {
            writeln!(text, "    {item}").unwrap();
        }
        if items.len() > FINDINGS_LISTED_MAX {
            writeln!(text, "    and {} more", items.len() - FINDINGS_LISTED_MAX).unwrap();
        }
    }
    if !findings.notes.is_empty() {
        writeln!(
            text,
            "  {} in `{}`.",
            plural(findings.notes.len(), "note", "notes"),
            detail_words(guide)
        )
        .unwrap();
    }
}

/// A stored decision state in words. The record reads spell `NeedsWork` as
/// `needswork`, which is a published JSON spelling and not a phrase.
fn standing_words(state: &str) -> &str {
    match state {
        "needswork" => "needs work",
        other => other,
    }
}

/// Text that may hold line breaks, set so every line after the first keeps
/// `indent`. A note is free text, and a line of it at column zero would read
/// as a new section of the guide or as another review.
fn indented(text: &str, indent: &str) -> String {
    text.lines()
        .collect::<Vec<_>>()
        .join(&format!("\n{indent}"))
}

/// What has been recorded against this change: each stored review, where it
/// stands, and its decisions and notes in the order they were recorded. A note
/// scoped to an entity this guide walks is shown under that entity's name.
fn write_recorded_reviews(text: &mut String, guide: &ReviewGuide) {
    if guide.reviews.is_empty() {
        return;
    }
    let step_names: HashMap<String, &str> = guide
        .chapters
        .iter()
        .flat_map(|chapter| &chapter.steps)
        .map(|step| (format!("entity:{}", step.entity_id), step.name.as_str()))
        .collect();
    writeln!(text).unwrap();
    writeln!(text, "Recorded reviews of this change:").unwrap();
    for review in &guide.reviews {
        writeln!(
            text,
            "  {} ({}): {}",
            review.review_id,
            standing_words(&review.state),
            indented(&review.title, "      ")
        )
        .unwrap();
        if review.events_total > review.events.len() {
            writeln!(
                text,
                "    {} earlier entries are in `kin review show {}`.",
                review.events_total - review.events.len(),
                review.review_id
            )
            .unwrap();
        }
        if review.events.iter().any(|event| event.text_cut) {
            writeln!(
                text,
                "    Some text is cut short. `kin review show {}` prints it whole.",
                review.review_id
            )
            .unwrap();
        }
        for event in &review.events {
            let lead = match (event.state.as_deref(), event.scope.as_ref()) {
                (Some(state), _) => format!("{} by {}", standing_words(state), event.by),
                (None, Some(scope)) => format!(
                    "note by {} on {}",
                    event.by,
                    step_names.get(scope).copied().unwrap_or(scope.as_str())
                ),
                (None, None) => format!("note by {}", event.by),
            };
            let cut = if event.text_cut {
                " [cut short here]"
            } else {
                ""
            };
            match event.text.as_deref().filter(|body| !body.trim().is_empty()) {
                Some(body) => {
                    writeln!(text, "    {lead}: {}{cut}", indented(body, "      ")).unwrap()
                }
                None => writeln!(text, "    {lead}{cut}").unwrap(),
            }
        }
    }
    if guide.reviews_total > guide.reviews.len() {
        writeln!(
            text,
            "  {} older reviews of this change are in `kin review list`.",
            guide.reviews_total - guide.reviews.len()
        )
        .unwrap();
    }
}

fn write_step_body(text: &mut String, step: &GuideStep) {
    let side = |body: &Option<GuideBody>| body.as_ref().and_then(|body| body.text.clone());
    let (before, after) = (side(&step.before), side(&step.after));
    for body in [&step.before, &step.after].into_iter().flatten() {
        if body.state != "recorded" {
            if let Some(reason) = &body.reason {
                writeln!(text, "    Source not shown: {reason}").unwrap();
            }
        }
    }
    if before.is_none() && after.is_none() {
        return;
    }
    let hunks = super::diff::render_hunks(before.as_deref(), after.as_deref());
    if hunks.is_empty() {
        writeln!(text, "    The recorded text is identical at both changes.").unwrap();
        return;
    }
    for hunk in hunks {
        for line in hunk.lines() {
            writeln!(text, "    {line}").unwrap();
        }
    }
}

fn write_connections(text: &mut String, heading: &str, rows: &[GuideConnection], total: usize) {
    if total == 0 {
        return;
    }
    writeln!(text, "    {heading} ({total} recorded):").unwrap();
    for row in rows {
        let place = match &row.entity.within {
            Some(within) => format!("in {within}  "),
            None => String::new(),
        };
        let overloads = match row.overloads {
            0 => String::new(),
            count => format!(
                "{}  ",
                plural(count, "more declaration", "more declarations")
            ),
        };
        writeln!(
            text,
            "      {:<12} {}{}  {place}{overloads}[{}{}]",
            row.relation.replace('_', " "),
            row.entity.name,
            row.entity.parameters.as_deref().unwrap_or_default(),
            row.origin,
            if row.changed_here {
                ", changed here"
            } else {
                ""
            }
        )
        .unwrap();
    }
    if rows.len() < total {
        writeln!(text, "      and {} more", total - rows.len()).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::change::{EntityDelta, RelationDelta};
    use kin_model::relation::{GraphNodeId, RelationOrigin};

    fn binding(layout: &kin_core::KinLayout) -> RequestRepositoryAuthority {
        RequestRepositoryAuthority::pinned(kin_core::LocalRepositoryAuthorityBinding::from_parts(
            kin_model::RepositoryId::new("guide-test").unwrap(),
            kin_model::WorkspaceId::new(),
            std::sync::Arc::new(kin_db::LocalFileBackend::new(layout.kindb_dir())),
        ))
    }

    fn function(name: &str, file: &str, body: u8) -> Entity {
        let span_file = kin_model::FilePathId::new(file);
        Entity {
            id: EntityId::new(),
            kind: EntityKind::Function,
            name: name.to_string(),
            language: kin_model::LanguageId::TypeScript,
            fingerprint: kin_model::SemanticFingerprint {
                algorithm: kin_model::FingerprintAlgorithm::V1TreeSitter,
                ast_hash: kin_model::Hash256::from_bytes([body; 32]),
                signature_hash: kin_model::Hash256::from_bytes([7; 32]),
                behavior_hash: kin_model::Hash256::from_bytes([body; 32]),
                equivalence_hash: kin_model::Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(span_file.clone()),
            span: Some(kin_model::SourceSpan {
                file: span_file,
                start_byte: 0,
                end_byte: 1,
                start_line: 3,
                start_col: 0,
                end_line: 9,
                end_col: 0,
            }),
            signature: format!("function {name}(projectId: string)"),
            visibility: kin_model::Visibility::Public,
            role: EntityRole::Source,
            doc_summary: None,
            metadata: kin_model::EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    fn calls(source: &Entity, target: &Entity) -> RelationDelta {
        RelationDelta::Added {
            new: Relation {
                id: kin_model::RelationId::new(),
                kind: RelationKind::Calls,
                src: GraphNodeId::Entity(source.id),
                dst: GraphNodeId::Entity(target.id),
                confidence: 1.0,
                origin: RelationOrigin::Parsed,
                created_in: None,
                import_source: None,
                evidence: vec![],
            },
        }
    }

    fn commit(
        graph: &kin_db::InMemoryGraph,
        parents: Vec<SemanticChangeId>,
        message: &str,
        entity_deltas: Vec<EntityDelta>,
        relation_deltas: Vec<RelationDelta>,
    ) -> SemanticChangeId {
        let mut change = SemanticChange {
            id: SemanticChangeId::from_hash(kin_model::Hash256::from_bytes([0; 32])),
            parents,
            origin: kin_model::ChangeOrigin::Native,
            timestamp: kin_model::Timestamp::now(),
            author: kin_model::AuthorId::new("reviewer@example.com"),
            message: message.to_string(),
            entity_deltas,
            relation_deltas,
            tree_deltas: vec![],
            projected_files: vec![],
            spec_link: None,
            evidence: vec![],
            risk_summary: None,
            admission_policy_delta: None,
            external_reference_deltas: Vec::new(),
            resolution_record_deltas: Vec::new(),
        };
        change.id = kin_model::compute_semantic_change_id(&change).unwrap();
        graph.create_change(&change).unwrap();
        change.id
    }

    fn added(entities: &[&Entity]) -> Vec<EntityDelta> {
        entities
            .iter()
            .map(|entity| EntityDelta::Added {
                new: (*entity).clone(),
            })
            .collect()
    }

    fn guide_of(graph: &kin_db::InMemoryGraph, change: SemanticChangeId) -> ReviewGuide {
        let dir = tempfile::tempdir().unwrap();
        let layout = kin_core::KinLayout::new(dir.path().to_path_buf());
        build_guide(
            &binding(&layout),
            graph,
            Some(&change.to_string()),
            None,
            None,
        )
        .unwrap()
    }

    /// The change the guide exists to show well: one function's body moves,
    /// and the three callers and the test that reach it do not.
    #[test]
    fn one_changed_entity_reads_with_the_callers_that_did_not_change() {
        let graph = kin_db::InMemoryGraph::new();
        let list_run = function("listRun", "src/runs/listRun.ts", 1);
        let route = function("handleRunsRequest", "src/api/runsRoute.ts", 2);
        let panel = function("renderRunsPanel", "src/ui/runsPanel.ts", 3);
        let report = function("buildNightlyReport", "src/jobs/nightlyReport.ts", 4);
        let mut test = function("testListRun", "test/listRun.test.ts", 5);
        test.role = EntityRole::Test;
        let root = commit(
            &graph,
            vec![],
            "Add run listing with a 24 hour cache",
            added(&[&list_run, &route, &panel, &report, &test]),
            vec![
                calls(&route, &list_run),
                calls(&panel, &list_run),
                calls(&report, &list_run),
                calls(&test, &list_run),
            ],
        );
        let mut shortened = list_run.clone();
        shortened.fingerprint.behavior_hash = kin_model::Hash256::from_bytes([9; 32]);
        shortened.fingerprint.ast_hash = kin_model::Hash256::from_bytes([9; 32]);
        let head = commit(
            &graph,
            vec![root],
            "Shorten the run list cache to 12 hours\n\nA day was too stale for the panel.",
            vec![EntityDelta::Modified {
                old: list_run.clone(),
                new: shortened,
            }],
            vec![],
        );

        let guide = guide_of(&graph, head);
        assert_eq!(guide.schema, GUIDE_SCHEMA);
        assert_eq!(guide.change.title, "Shorten the run list cache to 12 hours");
        assert_eq!(
            guide.change.base.as_deref(),
            Some(root.to_string().as_str())
        );
        assert_eq!(
            (
                guide.summary.added,
                guide.summary.modified,
                guide.summary.removed
            ),
            (0, 1, 0)
        );
        assert_eq!(guide.summary.recorded_consumers, 3);
        assert_eq!(guide.summary.recorded_tests, 1);
        assert!(guide.not_walked.is_none());

        assert_eq!(guide.chapters.len(), 1);
        let chapter = &guide.chapters[0];
        assert_eq!(chapter.kind, "single");
        assert_eq!(chapter.title, "listRun and what reaches it");
        assert_eq!(
            chapter.reason,
            "This entity changed on its own. None of the 3 places that rely on it changed, \
             and they may need review."
        );
        let step = &chapter.steps[0];
        assert_eq!(step.change, "modified");
        assert_eq!(step.file.as_deref(), Some("src/runs/listRun.ts"));
        assert_eq!(
            step.fields,
            vec![GuideFieldChange {
                field: "body".to_string(),
                before: None,
                after: None,
            }]
        );
        assert_eq!(step.connections.consumers_total, 3);
        assert_eq!(step.connections.consumers_changed_here, 0);
        assert_eq!(
            step.connections
                .consumers
                .iter()
                .map(|row| row.entity.name.as_str())
                .collect::<Vec<_>>(),
            vec!["buildNightlyReport", "handleRunsRequest", "renderRunsPanel"]
        );
        assert!(step
            .connections
            .consumers
            .iter()
            .all(|row| { row.relation == "calls" && row.origin == "parsed" && !row.changed_here }));
        assert_eq!(step.connections.tests_total, 1);
        assert_eq!(step.connections.tests[0].entity.name, "testListRun");
        assert_eq!(
            step.narration,
            "The function `listRun` was modified. Its body changed; its signature did not. \
             3 places rely on it, and none of them changed here. They may need review. 1 \
             test reaches it."
        );
        assert_eq!(
            step.consumer_verdict,
            Some(GuideConsumerVerdict {
                verdict: "may_need_review".to_string(),
                reason: "body_changed".to_string(),
                consumers: 3,
            })
        );
        assert_eq!(guide.summary.consumers_may_need_review, 3);
        assert_eq!(guide.summary.consumers_not_updated, 0);
        assert_eq!(guide.summary.consumers_need_no_update, 0);
        assert_eq!(
            step.evidence.base_change.as_deref(),
            Some(root.to_string().as_str())
        );
        assert_ne!(
            step.evidence.fingerprint_before.as_ref().unwrap().behavior,
            step.evidence.fingerprint_after.as_ref().unwrap().behavior
        );

        // This fixture holds no repository authority, so neither body can be
        // read. The step says so on each side and shows no text in its place.
        for body in [step.before.as_ref().unwrap(), step.after.as_ref().unwrap()] {
            assert_eq!(body.state, "unavailable");
            assert!(body.text.is_none());
            assert!(body
                .reason
                .as_deref()
                .is_some_and(|reason| !reason.is_empty()));
        }

        let text = guide_text(&guide);
        assert!(text.starts_with("Guided review: Shorten the run list cache to 12 hours\n"));
        assert!(text.contains("Chapter 1 of 1: listRun"));
        assert!(text.contains("Consumers (3 recorded):"));
        assert!(text.contains("Source not shown:"));
    }

    /// What one changed function asks of the callers that did not change,
    /// for each kind of edit, read from its two records as the parser writes
    /// them. A comment inside the parameter list moves the signature's
    /// recorded text and no token. A docstring moves a token that the
    /// behaviour equivalence class reads past. A body moves its tokens, and a
    /// signature its declaration.
    ///
    /// Falsify by counting `consumers_not_updated` off any change to the
    /// signature's text: the comment then leaves all three callers "not
    /// updated", which is what the guide said before.
    #[test]
    fn each_kind_of_edit_says_what_the_callers_that_did_not_change_need() {
        let hash = |byte: u8| kin_model::Hash256::from_bytes([byte; 32]);
        let graph = kin_db::InMemoryGraph::new();
        let mut want_bytes = function("wantBytes", "src/encoding.ts", 1);
        want_bytes.fingerprint.equivalence_hash = hash(11);
        let sign = function("sign", "src/signer.ts", 2);
        let verify = function("verify", "src/signer.ts", 3);
        let load = function("load", "src/serializer.ts", 4);
        let mut test = function("testWantBytes", "test/encoding.test.ts", 5);
        test.role = EntityRole::Test;
        let root = commit(
            &graph,
            vec![],
            "Add wantBytes and its callers",
            added(&[&want_bytes, &sign, &verify, &load, &test]),
            vec![
                calls(&sign, &want_bytes),
                calls(&verify, &want_bytes),
                calls(&load, &want_bytes),
                calls(&test, &want_bytes),
            ],
        );

        let mut commented = want_bytes.clone();
        commented.signature =
            "function wantBytes(// Bytes pass through unchanged. projectId: string)".to_string();
        let mut documented = want_bytes.clone();
        documented.doc_summary = Some("Return bytes, encoding a string first.".to_string());
        documented.fingerprint.behavior_hash = hash(21);
        documented.fingerprint.ast_hash = hash(21);
        let mut body = want_bytes.clone();
        body.fingerprint.behavior_hash = hash(22);
        body.fingerprint.ast_hash = hash(22);
        body.fingerprint.equivalence_hash = hash(23);
        let mut signature = want_bytes.clone();
        signature.signature = "function wantBytes(projectId: string, team: string)".to_string();
        signature.fingerprint.behavior_hash = hash(24);
        signature.fingerprint.ast_hash = hash(24);
        signature.fingerprint.signature_hash = hash(24);
        signature.fingerprint.equivalence_hash = hash(25);

        let need_no_update = "None of them needs an update: what they rely on kept its \
                              signature and behaves as it did.";
        let cases = [
            (
                commented,
                "Say that bytes pass through",
                ("needs_no_update", "comments_or_layout_only"),
                "Its signature text changed only by a comment or layout, and its code reads \
                 the same token for token. 3 places rely on it, and none of them changed here. \
                 They need no update.",
                "This entity changed on its own. None of the 3 places that rely on it changed, \
                 and they need no update.",
                need_no_update,
            ),
            (
                documented,
                "Describe wantBytes by what it returns",
                ("needs_no_update", "behavior_equivalent"),
                "Its documentation changed; its signature did not, and the graph records its \
                 body as behaving the same. 3 places rely on it, and none of them changed here. \
                 They need no update.",
                "This entity changed on its own. None of the 3 places that rely on it changed, \
                 and they need no update.",
                need_no_update,
            ),
            (
                body,
                "Fall back to strict errors",
                ("may_need_review", "body_changed"),
                "Its body changed; its signature did not. 3 places rely on it, and none of them \
                 changed here. They may need review.",
                "This entity changed on its own. None of the 3 places that rely on it changed, \
                 and they may need review.",
                "None of them changed and the code they rely on did, so they may need review.",
            ),
            (
                signature,
                "Scope wantBytes by team",
                ("not_updated", "signature_changed"),
                "Its signature and body changed. 3 places rely on it, and none of them changed \
                 here. They were not updated.",
                "This entity changed on its own. None of the 3 places that rely on it was \
                 updated.",
                "None of them was updated, and the change breaks what they rely on.",
            ),
        ];
        for (new, message, (verdict, reason), step_says, chapter_says, headline) in cases {
            let head = commit(
                &graph,
                vec![root],
                message,
                vec![EntityDelta::Modified {
                    old: want_bytes.clone(),
                    new,
                }],
                vec![],
            );
            let guide = guide_of(&graph, head);
            let text = guide_text(&guide);
            assert_eq!(guide.chapters.len(), 1, "{text}");
            let chapter = &guide.chapters[0];
            let step = &chapter.steps[0];
            assert_eq!(
                step.consumer_verdict,
                Some(GuideConsumerVerdict {
                    verdict: verdict.to_string(),
                    reason: reason.to_string(),
                    consumers: 3,
                }),
                "{message}: {text}"
            );
            assert!(
                step.narration.contains(step_says),
                "{message}: the step should say `{step_says}`: {}",
                step.narration
            );
            assert_eq!(chapter.reason, chapter_says, "{message}");
            assert!(
                guide.summary.narration.contains(headline),
                "{message}: the summary should say `{headline}`: {}",
                guide.summary.narration
            );
            assert!(text.contains(&guide.summary.narration), "{text}");

            let summary = &guide.summary;
            assert_eq!(summary.recorded_consumers, 3, "{message}");
            let counts = (
                summary.consumers_not_updated,
                summary.consumers_may_need_review,
                summary.consumers_need_no_update,
            );
            assert_eq!(
                counts,
                match verdict {
                    "not_updated" => (3, 0, 0),
                    "may_need_review" => (0, 3, 0),
                    _ => (0, 0, 3),
                },
                "{message}: the three counts hold every consumer once"
            );
            // The review the verdict stands on agrees: what asks nothing of a
            // caller raises no finding that it breaks one.
            let breaks = step
                .findings
                .iter()
                .any(|finding| finding.kind == "breaking" || finding.kind == "signature_change");
            assert_eq!(
                breaks,
                verdict == "not_updated",
                "{message}: {:?}",
                step.findings
            );
        }
    }

    /// Entities that changed together and call each other are one chapter,
    /// read callee first. What nothing connects or reaches is grouped apart.
    #[test]
    fn connected_changes_form_a_chapter_read_dependency_first() {
        let graph = kin_db::InMemoryGraph::new();
        let cache_key = function("cacheKey", "src/runs/cache.ts", 1);
        let list_run = function("listRun", "src/runs/listRun.ts", 2);
        let route = function("handleRunsRequest", "src/api/runsRoute.ts", 3);
        let root = commit(
            &graph,
            vec![],
            "Add run listing",
            added(&[&cache_key, &list_run, &route]),
            vec![calls(&list_run, &cache_key), calls(&route, &list_run)],
        );
        let mut scoped_key = cache_key.clone();
        scoped_key.signature = "function cacheKey(projectId: string, team: string)".to_string();
        scoped_key.fingerprint.signature_hash = kin_model::Hash256::from_bytes([8; 32]);
        let mut scoped_list = list_run.clone();
        scoped_list.fingerprint.behavior_hash = kin_model::Hash256::from_bytes([8; 32]);
        // Private, so the review has no coverage expectation to rank it by and
        // it stays with the unconnected remainder.
        let mut invalidate = function("invalidateRunCache", "src/runs/cache.ts", 6);
        invalidate.visibility = kin_model::Visibility::Private;
        let head = commit(
            &graph,
            vec![root],
            "Scope the run cache by team",
            vec![
                EntityDelta::Modified {
                    old: list_run.clone(),
                    new: scoped_list,
                },
                EntityDelta::Modified {
                    old: cache_key.clone(),
                    new: scoped_key,
                },
                EntityDelta::Added {
                    new: invalidate.clone(),
                },
            ],
            vec![],
        );

        let guide = guide_of(&graph, head);
        assert_eq!(guide.chapters.len(), 2);
        let connected = &guide.chapters[0];
        assert_eq!(connected.kind, "connected");
        assert_eq!(connected.title, "cacheKey and 1 connected change");
        assert_eq!(
            connected
                .steps
                .iter()
                .map(|step| step.name.as_str())
                .collect::<Vec<_>>(),
            vec!["cacheKey", "listRun"],
            "the callee is read before the caller that changed with it"
        );
        let key_step = &connected.steps[0];
        assert_eq!(key_step.fields.len(), 1);
        assert_eq!(key_step.fields[0].field, "signature");
        assert_eq!(key_step.connections.consumers_total, 1);
        assert_eq!(key_step.connections.consumers_changed_here, 1);
        assert!(key_step.connections.consumers[0].changed_here);
        // The route calls listRun and did not change, so it is the one
        // consumer outside the change.
        assert_eq!(guide.summary.recorded_consumers, 1);
        assert_eq!(
            connected.steps[1].connections.consumers[0].entity.name,
            "handleRunsRequest"
        );

        let group = &guide.chapters[1];
        assert_eq!(group.kind, "group");
        assert_eq!(group.title, "New entities");
        assert_eq!(group.steps[0].name, "invalidateRunCache");
        assert_eq!(group.steps[0].change, "added");
        assert!(group.steps[0].before.is_none());
    }

    /// An endpoint that did not change is still named from the reviewed
    /// head, even after later changes rename it or remove it altogether.
    #[test]
    fn historical_relation_endpoint_names_survive_later_renames_and_removals() {
        use kin_model::EntityStore as _;

        let graph = kin_db::InMemoryGraph::new();
        let list_run = function("listRun", "src/runs/listRun.ts", 1);
        let root = commit(
            &graph,
            vec![],
            "Add run listing",
            added(&[&list_run]),
            vec![],
        );
        let route = function("handleRunsRequest", "src/api/runsRoute.ts", 2);
        let head = commit(
            &graph,
            vec![root],
            "Expose run listing",
            added(&[&route]),
            vec![calls(&route, &list_run)],
        );
        let assert_historical_name = || {
            for guide in [guide_of(&graph, head), range_of(&graph, root, head)] {
                let step = guide
                    .chapters
                    .iter()
                    .flat_map(|chapter| &chapter.steps)
                    .find(|step| step.entity_id == route.id.to_string())
                    .expect("the new caller is walked");
                assert_eq!(step.connections.relation_changes_total, 1);
                let relation = &step.connections.relation_changes[0];
                assert_eq!(relation.change, "added");
                assert_eq!(relation.direction, "outbound");
                assert_eq!(relation.other, "listRun");
            }
        };

        graph.upsert_entity(&list_run).unwrap();
        assert_historical_name();
        let mut renamed = list_run.clone();
        renamed.name = "listRecentRuns".to_string();
        let later = commit(
            &graph,
            vec![head],
            "Rename run listing",
            vec![EntityDelta::Modified {
                old: list_run,
                new: renamed.clone(),
            }],
            vec![],
        );
        graph.upsert_entity(&renamed).unwrap();
        assert_historical_name();

        commit(
            &graph,
            vec![later],
            "Remove run listing",
            vec![EntityDelta::Removed {
                old: renamed.clone(),
            }],
            vec![],
        );
        graph.remove_entity(&renamed.id).unwrap();
        assert_historical_name();
    }

    /// Removing an edge reads its old endpoint name, even if that entity
    /// survives with a new name at the reviewed head.
    #[test]
    fn removed_relation_endpoint_names_read_the_base_without_entity_removals() {
        use kin_model::EntityStore as _;

        let graph = kin_db::InMemoryGraph::new();
        let route = function("handleRunsRequest", "src/api/runsRoute.ts", 1);
        let list_run = function("listRun", "src/runs/listRun.ts", 2);
        let cache_key = function("cacheKey", "src/runs/cacheKey.ts", 3);
        let call = calls(&route, &list_run);
        let cache_call = calls(&route, &cache_key);
        let root = commit(
            &graph,
            vec![],
            "Expose cached run listing",
            added(&[&route, &list_run, &cache_key]),
            vec![call.clone(), cache_call.clone()],
        );
        let mut updated_route = route.clone();
        updated_route.fingerprint.behavior_hash = kin_model::Hash256::from_bytes([9; 32]);
        let mut renamed = list_run.clone();
        renamed.name = "listRecentRuns".to_string();
        let removed = |delta| match delta {
            RelationDelta::Added { new } => RelationDelta::Removed { old: new },
            _ => unreachable!("calls() adds a relation"),
        };
        let head = commit(
            &graph,
            vec![root],
            "Stop calling run listing and rename it",
            vec![
                EntityDelta::Modified {
                    old: route.clone(),
                    new: updated_route,
                },
                EntityDelta::Modified {
                    old: list_run,
                    new: renamed,
                },
            ],
            vec![removed(call), removed(cache_call)],
        );
        let mut live_cache_key = cache_key.clone();
        live_cache_key.name = "recentRunCacheKey".to_string();
        graph.upsert_entity(&live_cache_key).unwrap();

        for guide in [guide_of(&graph, head), range_of(&graph, root, head)] {
            assert_eq!(guide.summary.removed, 0);
            let step = guide
                .chapters
                .iter()
                .flat_map(|chapter| &chapter.steps)
                .find(|step| step.entity_id == route.id.to_string())
                .expect("the changed caller is walked");
            assert_eq!(step.connections.relation_changes_total, 2);
            assert!(step
                .connections
                .relation_changes
                .iter()
                .all(|relation| relation.change == "removed" && relation.direction == "outbound"));
            assert_eq!(
                step.connections
                    .relation_changes
                    .iter()
                    .map(|relation| relation.other.as_str())
                    .collect::<Vec<_>>(),
                vec!["cacheKey", "listRun"],
                "walked and unchanged endpoints both use their base-side names"
            );
        }
    }

    /// Opening something that reaches a step reads it at that change, and a
    /// request naming nothing recorded there is answered with the reason.
    #[test]
    fn an_entity_is_read_as_recorded_at_the_change_or_refused_by_name() {
        let graph = kin_db::InMemoryGraph::new();
        let list_run = function("listRun", "src/runs/listRun.ts", 1);
        let root = commit(
            &graph,
            vec![],
            "Add run listing",
            added(&[&list_run]),
            vec![],
        );
        let later = function("renderRunsPanel", "src/ui/runsPanel.ts", 2);
        let head = commit(
            &graph,
            vec![root],
            "Add the runs panel",
            added(&[&later]),
            vec![calls(&later, &list_run)],
        );

        let read =
            entity_source_at(&graph, &later.id.to_string(), &head.to_string(), None).unwrap();
        let entity = read.entity.expect("the panel is recorded at head");
        assert_eq!(entity.name, "renderRunsPanel");
        assert_eq!(entity.file.as_deref(), Some("src/ui/runsPanel.ts"));
        assert_eq!(
            read.body.change_id.as_deref(),
            Some(head.to_string().as_str())
        );
        assert!(
            read.body.text.is_none(),
            "this fixture holds no source to read"
        );

        let early =
            entity_source_at(&graph, &later.id.to_string(), &root.to_string(), None).unwrap();
        assert!(early.entity.is_none());
        assert_eq!(
            early.body.reason.as_deref(),
            Some(format!("entity {} is not recorded at change {root}", later.id).as_str())
        );

        let later_id = later.id.to_string();
        let head_id = head.to_string();
        for (entity, change, reason) in [
            (
                "not-an-id",
                head_id.as_str(),
                "'not-an-id' is not an entity id",
            ),
            (later_id.as_str(), "zz", "'zz' is not a change id"),
        ] {
            let refused = entity_source_at(&graph, entity, change, None).unwrap();
            assert!(refused.entity.is_none() && refused.body.text.is_none());
            assert_eq!(refused.body.reason.as_deref(), Some(reason));
        }
    }

    /// A file's module surface moves whenever a member does and has no text
    /// of its own, so it is counted and never walked, and a module that only
    /// imports a changed entity is listed apart from the function that calls it.
    #[test]
    fn module_containers_are_counted_and_importing_modules_listed_apart() {
        let module = |name: &str, file: &str, body: u8| {
            let mut entity = function(name, file, body);
            entity.kind = EntityKind::Module;
            entity
        };
        let imports = |source: &Entity, target: &Entity| {
            let RelationDelta::Added { mut new } = calls(source, target) else {
                unreachable!()
            };
            new.kind = RelationKind::Imports;
            RelationDelta::Added { new }
        };
        let graph = kin_db::InMemoryGraph::new();
        let list_module = module("listRun", "src/runs/listRun.ts", 1);
        let list_run = function("listRun", "src/runs/listRun.ts", 2);
        let panel_module = module("runsPanel", "src/ui/runsPanel.ts", 3);
        let panel = function("renderRunsPanel", "src/ui/runsPanel.ts", 4);
        let root = commit(
            &graph,
            vec![],
            "Add run listing",
            added(&[&list_module, &list_run, &panel_module, &panel]),
            vec![imports(&panel_module, &list_run), calls(&panel, &list_run)],
        );
        let mut new_module = list_module.clone();
        new_module.fingerprint.behavior_hash = kin_model::Hash256::from_bytes([9; 32]);
        let mut shortened = list_run.clone();
        shortened.fingerprint.behavior_hash = kin_model::Hash256::from_bytes([9; 32]);
        let head = commit(
            &graph,
            vec![root],
            "Shorten the run list cache",
            vec![
                EntityDelta::Modified {
                    old: list_module.clone(),
                    new: new_module,
                },
                EntityDelta::Modified {
                    old: list_run.clone(),
                    new: shortened,
                },
            ],
            vec![],
        );

        let guide = guide_of(&graph, head);
        assert_eq!(guide.summary.module_containers, 1);
        assert_eq!(guide.summary.modified, 1);
        assert_eq!(guide.chapters.len(), 1);
        let step = &guide.chapters[0].steps[0];
        assert_eq!(
            (step.name.as_str(), step.kind.as_str()),
            ("listRun", "function")
        );
        assert_eq!(step.connections.consumers_total, 1);
        assert_eq!(step.connections.consumers[0].entity.name, "renderRunsPanel");
        assert_eq!(step.connections.importing_modules_total, 1);
        assert_eq!(step.connections.importing_modules[0].name, "runsPanel");
        assert_eq!(guide.summary.recorded_consumers, 1);
        let text = guide_text(&guide);
        assert!(text.contains("1 module container changed with its members"));
        assert!(text.contains("Imported by 1 module: runsPanel"));
    }

    /// A merge is read as what it brought into its first parent's line. The
    /// walk from a merge crosses into the merged branch's history, and none of
    /// what that history shares with the first parent is this change's own.
    #[test]
    fn a_merge_reads_what_it_brought_in_and_not_the_history_behind_it() {
        let graph = kin_db::InMemoryGraph::new();
        let on_main = function("handleRunsRequest", "src/api/runsRoute.ts", 1);
        let on_branch = function("listRun", "src/runs/listRun.ts", 2);
        let root = commit(
            &graph,
            vec![],
            "Add run listing",
            added(&[&on_main, &on_branch]),
            vec![calls(&on_main, &on_branch)],
        );
        let mut main_side = on_main.clone();
        main_side.fingerprint.behavior_hash = kin_model::Hash256::from_bytes([8; 32]);
        let main = commit(
            &graph,
            vec![root],
            "Change the route on main",
            vec![EntityDelta::Modified {
                old: on_main.clone(),
                new: main_side,
            }],
            vec![],
        );
        let mut branch_side = on_branch.clone();
        branch_side.fingerprint.behavior_hash = kin_model::Hash256::from_bytes([9; 32]);
        let branch = commit(
            &graph,
            vec![root],
            "Shorten the cache on a branch",
            vec![EntityDelta::Modified {
                old: on_branch.clone(),
                new: branch_side,
            }],
            vec![],
        );
        let merge = commit(
            &graph,
            vec![main, branch],
            "Merge the branch",
            vec![],
            vec![],
        );

        let guide = guide_of(&graph, merge);
        assert_eq!(
            guide.change.base.as_deref(),
            Some(main.to_string().as_str())
        );
        let steps: Vec<(&str, &str)> = guide
            .chapters
            .iter()
            .flat_map(|chapter| &chapter.steps)
            .map(|step| (step.name.as_str(), step.change.as_str()))
            .collect();
        assert_eq!(
            steps,
            vec![("listRun", "modified")],
            "only the branch's own change is in the merge's range"
        );
        assert_eq!(
            (
                guide.summary.added,
                guide.summary.modified,
                guide.summary.removed
            ),
            (0, 1, 0)
        );
    }

    /// A review recorded against this change's id is shown with its history
    /// in the order it was recorded, and one opened against a branch name or
    /// another change is not.
    #[test]
    fn a_guide_carries_the_reviews_recorded_against_its_change() {
        use kin_model::graph::ReviewStore as _;
        use kin_model::review::{
            Review, ReviewCompletionState, ReviewDecision, ReviewDecisionState, ReviewId,
            ReviewNote, ReviewNoteId,
        };
        use kin_model::IdentityRef;

        // Instants a second apart, built from their wire form, so the order
        // under test is the recorded one and not the order of insertion.
        let at = |second: u32| -> kin_model::Timestamp {
            serde_json::from_value(serde_json::json!(format!("2026-10-07T00:00:{second:02}Z")))
                .unwrap()
        };
        let graph = kin_db::InMemoryGraph::new();
        let list_run = function("listRun", "src/runs/listRun.ts", 1);
        let root = commit(
            &graph,
            vec![],
            "Add run listing",
            added(&[&list_run]),
            vec![],
        );
        let mut shortened = list_run.clone();
        shortened.fingerprint.behavior_hash = kin_model::Hash256::from_bytes([9; 32]);
        let head = commit(
            &graph,
            vec![root],
            "Shorten the run list cache",
            vec![EntityDelta::Modified {
                old: list_run.clone(),
                new: shortened,
            }],
            vec![],
        );
        let stored = |title: &str, head_ref: String| {
            let review = Review {
                review_id: ReviewId::new(),
                title: title.to_string(),
                base_ref: root.to_string(),
                head_ref,
                state: ReviewDecisionState::Pending,
                completion: ReviewCompletionState::InReview,
                created_by: IdentityRef::human("ada"),
                created_at: at(0),
                updated_at: at(0),
                scopes: vec![],
            };
            graph.create_review(&review).unwrap();
            review
        };
        let of_head = stored("of this change", format!("change:{head}"));
        stored("of a branch", "main".to_string());
        stored("of another change", root.to_string());
        let decide = |state, comment: &str, second| {
            graph
                .add_review_decision(
                    &of_head.review_id,
                    &ReviewDecision {
                        reviewer: IdentityRef::human("ada"),
                        state,
                        comment: Some(comment.to_string()),
                        decided_at: at(second),
                    },
                )
                .unwrap();
        };
        let note = |body: &str, scope, second| {
            graph
                .add_review_note(&ReviewNote {
                    note_id: ReviewNoteId::new(),
                    review_id: of_head.review_id,
                    body: body.to_string(),
                    scope,
                    authored_by: IdentityRef::human("ada"),
                    created_at: at(second),
                })
                .unwrap();
        };
        // Written out of order on purpose: a concern, a note on the entity,
        // then the approval that answers them.
        decide(ReviewDecisionState::Approved, "twelve hours is fine", 30);
        note(
            "the ttl is the only edit",
            Some(kin_model::work::WorkScope::Entity(list_run.id)),
            20,
        );
        decide(ReviewDecisionState::NeedsWork, "why twelve?", 5);
        note("checked the panel\nand the nightly job", None, 10);

        let guide = guide_of(&graph, head);
        assert_eq!(guide.reviews.len(), 1, "{:?}", guide.reviews);
        assert_eq!(guide.reviews_total, 1);
        let review = &guide.reviews[0];
        assert_eq!(review.title, "of this change");
        assert_eq!(review.events_total, 4);
        assert_eq!(
            review
                .events
                .iter()
                .map(|event| (event.kind.as_str(), event.state.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("decision", Some("needswork")),
                ("note", None),
                ("note", None),
                ("decision", Some("approved")),
            ],
            "a history reads in the order it was recorded, whatever kind each entry is"
        );
        assert_eq!(review.events[0].by, "ada");
        assert_eq!(
            review.events[2].scope.as_deref(),
            Some(format!("entity:{}", list_run.id).as_str())
        );
        let text = guide_text(&guide);
        let recorded = &text[text.find("Recorded reviews of this change:").unwrap()..];
        let order = [
            "needs work by ada: why twelve?",
            "note by ada: checked the panel\n      and the nightly job",
            "note by ada on listRun: the ttl is the only edit",
            "approved by ada: twelve hours is fine",
        ]
        .map(|line| {
            recorded
                .find(line)
                .unwrap_or_else(|| panic!("{line:?} in {recorded}"))
        });
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{recorded}");
        assert!(
            recorded.lines().all(|line| line.is_empty()
                || line.starts_with(' ')
                || line.starts_with("Recorded")
                || line.starts_with("Read it")),
            "no line of a note reaches column zero: {recorded}"
        );
        assert_eq!(standing_words("needswork"), "needs work");
        assert_eq!(standing_words("approved"), "approved");

        assert!(guide_of(&graph, root)
            .reviews
            .iter()
            .all(|review| review.title == "of another change"));
        let dir = tempfile::tempdir().unwrap();
        let layout = kin_core::KinLayout::new(dir.path().to_path_buf());
        let binding = binding(&layout);
        let names = |head_ref: &str| head_names_change(&binding, &graph, head_ref, &head);
        assert!(names(&format!("kin:{head}")));
        assert!(names(&head.to_string()));
        // A head is matched as stored. A padded or upper-case spelling is one
        // the ref grammar never read as this change, so it does not name it.
        assert!(!names(&format!(" change:{head} ")));
        assert!(!names(&format!("{head} ")));
        assert!(!names(&head.to_string().to_uppercase()));
        assert!(!names(&format!(
            "change:{}",
            head.to_string().to_uppercase()
        )));
        assert!(!names("main"));
        assert!(!names(&format!("change:{root}")));
        assert!(!names("change:not-a-change"));
        // A bare prefix is a name, never a change. An explicit prefix of
        // another change's id cannot name this one and is refused outright.
        // One this change's id does start with is the grammar's to search,
        // and this fixture holds no repository authority to search, so it is
        // left unmatched rather than guessed at.
        let prefix = &head.to_string()[..12];
        assert!(!names(prefix));
        assert!(!names(&format!("change:{}", &root.to_string()[..12])));
        assert!(!names(&format!("change:{prefix}")));
    }

    /// A branch two changes long beside a base that moved on after the
    /// branch left it: `root`, then `main` on one side and `first` and `head`
    /// on the other.
    struct Branch {
        graph: kin_db::InMemoryGraph,
        root: SemanticChangeId,
        main: SemanticChangeId,
        first: SemanticChangeId,
        head: SemanticChangeId,
    }

    fn branch() -> Branch {
        let graph = kin_db::InMemoryGraph::new();
        let route = function("handleRunsRequest", "src/api/runsRoute.ts", 1);
        let list_run = function("listRun", "src/runs/listRun.ts", 2);
        let root = commit(
            &graph,
            vec![],
            "Add run listing",
            added(&[&route, &list_run]),
            vec![calls(&route, &list_run)],
        );
        let mut main_side = route.clone();
        main_side.fingerprint.behavior_hash = kin_model::Hash256::from_bytes([8; 32]);
        let main = commit(
            &graph,
            vec![root],
            "Change the route on main",
            vec![EntityDelta::Modified {
                old: route.clone(),
                new: main_side,
            }],
            vec![],
        );
        let mut shortened = list_run.clone();
        shortened.fingerprint.behavior_hash = kin_model::Hash256::from_bytes([9; 32]);
        let first = commit(
            &graph,
            vec![root],
            "Shorten the run list cache",
            vec![EntityDelta::Modified {
                old: list_run.clone(),
                new: shortened.clone(),
            }],
            vec![],
        );
        let cache_key = function("runCacheKey", "src/runs/cacheKey.ts", 3);
        let head = commit(
            &graph,
            vec![first],
            "Scope the run cache by team",
            added(&[&cache_key]),
            vec![calls(&shortened, &cache_key)],
        );
        Branch {
            graph,
            root,
            main,
            first,
            head,
        }
    }

    fn range_of(
        graph: &kin_db::InMemoryGraph,
        base: SemanticChangeId,
        head: SemanticChangeId,
    ) -> ReviewGuide {
        let dir = tempfile::tempdir().unwrap();
        let layout = kin_core::KinLayout::new(dir.path().to_path_buf());
        build_range_guide(
            &binding(&layout),
            graph,
            &format!("change:{base}"),
            Some(&format!("change:{head}")),
            None,
            None,
        )
        .unwrap()
    }

    /// A range is what a review of a branch reads: everything the head holds
    /// that the base does not, with "before" read where the two parted. What
    /// the base did on its own since then is not part of it.
    #[test]
    fn a_range_walks_what_the_head_holds_and_the_base_does_not() {
        let Branch {
            graph,
            root,
            main,
            first,
            head,
        } = branch();

        let guide = range_of(&graph, main, head);
        assert_eq!(guide.kind, "range");
        let range = guide.range.as_ref().expect("a range guide names its range");
        assert_eq!(range.base_ref, format!("change:{main}"));
        assert_eq!(range.head_ref, format!("change:{head}"));
        assert_eq!(
            range.merge_base,
            root.to_string(),
            "before is read where the branch left the base, not at the base's tip"
        );
        assert_eq!(range.changes_total, 2);
        let listed: HashSet<&str> = range
            .changes
            .iter()
            .map(|change| change.id.as_str())
            .collect();
        assert_eq!(
            listed,
            HashSet::from([head.to_string().as_str(), first.to_string().as_str()])
        );
        assert_eq!(guide.change.id, head.to_string());
        assert_eq!(guide.change.base, Some(root.to_string()));

        let mut steps: Vec<(&str, &str)> = guide
            .chapters
            .iter()
            .flat_map(|chapter| &chapter.steps)
            .map(|step| (step.name.as_str(), step.change.as_str()))
            .collect();
        steps.sort_unstable();
        assert_eq!(
            steps,
            vec![("listRun", "modified"), ("runCacheKey", "added")],
            "both changes of the branch are walked once, and main's own change is not"
        );

        let text = guide_text(&guide);
        assert!(
            text.contains("2 changes, before read at"),
            "the terminal says it walks a range: {text}"
        );
        assert!(
            !text.lines().any(|line| line.starts_with("Change ")),
            "a range is not introduced as one change: {text}"
        );
        assert!(
            text.contains(&format!(
                "Read it as a page: kin review open change:{main}..change:{head}"
            )),
            "{text}"
        );

        // A range that holds nothing is an answer, not a refusal.
        let nothing = range_of(&graph, head, head);
        assert_eq!(nothing.range.as_ref().unwrap().changes_total, 0);
        assert!(nothing.chapters.is_empty());
        // A base that is ahead of the head leaves nothing either.
        let behind = range_of(&graph, head, first);
        assert_eq!(behind.range.as_ref().unwrap().changes_total, 0);
        assert!(behind.chapters.is_empty());
    }

    /// A range and its last change share a head. A review of the last change
    /// is not a review of the branch, so the range lists only the reviews
    /// whose base lands where its own does, while the change lists them all.
    #[test]
    fn a_range_keeps_a_review_record_of_its_own() {
        use kin_model::graph::ReviewStore as _;
        use kin_model::review::{Review, ReviewCompletionState, ReviewDecisionState, ReviewId};
        use kin_model::IdentityRef;

        let Branch {
            graph,
            root,
            main,
            first,
            head,
        } = branch();
        let at = |second: u32| -> kin_model::Timestamp {
            serde_json::from_value(serde_json::json!(format!("2026-10-07T00:00:{second:02}Z")))
                .unwrap()
        };
        let stored = |title: &str, base: SemanticChangeId, second: u32| {
            graph
                .create_review(&Review {
                    review_id: ReviewId::new(),
                    title: title.to_string(),
                    base_ref: format!("change:{base}"),
                    head_ref: format!("change:{head}"),
                    state: ReviewDecisionState::Pending,
                    completion: ReviewCompletionState::InReview,
                    created_by: IdentityRef::human("ada"),
                    created_at: at(second),
                    updated_at: at(second),
                    scopes: vec![],
                })
                .unwrap();
        };
        stored("of the last change", first, 1);
        stored("of the branch", root, 2);
        // Opened against a base that has moved on since. It still reads
        // "before" where the branch left it, so it is the same range.
        stored("of the branch from main", main, 3);
        // Opened against a branch by its name. Where a name points moves, so
        // it is matched to no range, then or later.
        graph
            .create_review(&Review {
                review_id: ReviewId::new(),
                title: "of the branch by name".to_string(),
                base_ref: "main".to_string(),
                head_ref: format!("change:{head}"),
                state: ReviewDecisionState::Pending,
                completion: ReviewCompletionState::InReview,
                created_by: IdentityRef::human("ada"),
                created_at: at(4),
                updated_at: at(4),
                scopes: vec![],
            })
            .unwrap();

        let titles = |guide: &ReviewGuide| -> Vec<String> {
            guide
                .reviews
                .iter()
                .map(|review| review.title.clone())
                .collect()
        };
        let of_range = range_of(&graph, main, head);
        assert_eq!(
            titles(&of_range),
            vec!["of the branch from main", "of the branch"],
            "newest first, and the review of the last change alone is not the range's"
        );
        assert_eq!(of_range.reviews_total, 2);

        let of_change = guide_of(&graph, head);
        assert_eq!(
            titles(&of_change),
            vec![
                "of the branch by name",
                "of the branch from main",
                "of the branch",
                "of the last change"
            ],
            "every review whose head is the change did read it"
        );

        // The range of the last change alone is that change, and reads its review.
        assert_eq!(
            titles(&range_of(&graph, first, head)),
            vec!["of the last change"]
        );

        let dir = tempfile::tempdir().unwrap();
        let layout = kin_core::KinLayout::new(dir.path().to_path_buf());
        let binding = binding(&layout);
        let read = |range_base: Option<String>| {
            reviews_of_ref(
                &binding,
                &graph,
                &format!("change:{head}"),
                range_base.as_deref(),
            )
            .unwrap()
            .reviews_total
        };
        assert_eq!(read(None), 4);
        assert_eq!(read(Some(format!("change:{root}"))), 2);
        // What a write from the last change's own page looks for: the review
        // from that change's base, and not the range reviews that end there.
        assert_eq!(read(Some(format!("change:{first}"))), 1);
        assert_eq!(whole_change_id(&head.to_string()), Some(head));
        assert_eq!(whole_change_id(&format!("kin:{head}")), Some(head));
        assert_eq!(whole_change_id("main"), None);
        assert_eq!(whole_change_id(&head.to_string()[..12]), None);
        assert_eq!(whole_change_id(&head.to_string().to_uppercase()), None);
    }

    fn contains(container: &Entity, member: &Entity) -> RelationDelta {
        let RelationDelta::Added { mut new } = calls(container, member) else {
            unreachable!("calls() adds a relation")
        };
        new.kind = RelationKind::Contains;
        RelationDelta::Added { new }
    }

    /// A guide says where an entity is by what contains it: the step, the
    /// consumers that reach it and the terminal all name the module, and none
    /// of them names where a projection writes the entity.
    #[test]
    fn entities_are_placed_by_what_contains_them_and_never_by_a_path() {
        let graph = kin_db::InMemoryGraph::new();
        let mut runs = function("runs", "src/runs/listRun.ts", 4);
        runs.kind = EntityKind::Module;
        let mut api = function("runsRoute", "src/api/runsRoute.ts", 5);
        api.kind = EntityKind::Module;
        let list_run = function("listRun", "src/runs/listRun.ts", 1);
        let route = function("handleRunsRequest", "src/api/runsRoute.ts", 2);
        let root = commit(
            &graph,
            vec![],
            "Add run listing",
            added(&[&runs, &api, &list_run, &route]),
            vec![
                contains(&runs, &list_run),
                contains(&api, &route),
                calls(&route, &list_run),
            ],
        );
        let mut shortened = list_run.clone();
        shortened.fingerprint.behavior_hash = kin_model::Hash256::from_bytes([9; 32]);
        let head = commit(
            &graph,
            vec![root],
            "Shorten the run list cache",
            vec![EntityDelta::Modified {
                old: list_run.clone(),
                new: shortened,
            }],
            vec![],
        );

        let guide = guide_of(&graph, head);
        let step = guide
            .chapters
            .iter()
            .flat_map(|chapter| &chapter.steps)
            .find(|step| step.name == "listRun")
            .expect("the change is walked");
        assert_eq!(step.within.as_deref(), Some("module runs"));
        let caller = step
            .connections
            .consumers
            .iter()
            .find(|consumer| consumer.entity.name == "handleRunsRequest")
            .expect("the caller reaches the change");
        assert_eq!(caller.entity.within.as_deref(), Some("module runsRoute"));

        let text = guide_text(&guide);
        assert!(text.contains("listRun (function in module runs)"), "{text}");
        assert!(
            text.contains("handleRunsRequest  in module runsRoute"),
            "{text}"
        );
        assert!(
            !text.lines().any(|line| (line.contains("Step ")
                || line.contains("handleRunsRequest"))
                && line.contains("src/")),
            "a step and a consumer are placed by entities, never by a path: {text}"
        );
    }

    /// A removed entity is gone at the change, with the relations into it.
    /// Its step reads what reached it where it still existed, and the guide
    /// reports the caller its removal leaves broken, for one change and for a
    /// range alike.
    #[test]
    fn a_removed_entity_reads_its_callers_where_it_still_existed() {
        let graph = kin_db::InMemoryGraph::new();
        let route = function("handleRunsRequest", "src/api/runsRoute.ts", 1);
        let list_run = function("listRun", "src/runs/listRun.ts", 2);
        let call = calls(&route, &list_run);
        let RelationDelta::Added { new: relation } = call.clone() else {
            unreachable!("calls() adds a relation")
        };
        let root = commit(
            &graph,
            vec![],
            "Add run listing",
            added(&[&route, &list_run]),
            vec![call],
        );
        let head = commit(
            &graph,
            vec![root],
            "Drop the run list",
            vec![EntityDelta::Removed {
                old: list_run.clone(),
            }],
            vec![RelationDelta::Removed { old: relation }],
        );

        for guide in [guide_of(&graph, head), range_of(&graph, root, head)] {
            let step = guide
                .chapters
                .iter()
                .flat_map(|chapter| &chapter.steps)
                .find(|step| step.name == "listRun")
                .unwrap_or_else(|| panic!("the removal is walked: {}", guide_text(&guide)));
            assert_eq!(step.change, "removed");
            let callers: Vec<&str> = step
                .connections
                .consumers
                .iter()
                .map(|consumer| consumer.entity.name.as_str())
                .collect();
            assert_eq!(
                callers,
                vec!["handleRunsRequest"],
                "the caller is read where listRun still existed: {}",
                guide_text(&guide)
            );
            assert_eq!(guide.summary.recorded_consumers, 1);
            assert!(
                guide
                    .findings
                    .breaking_changes
                    .iter()
                    .any(|finding| finding.contains("listRun")),
                "the removal breaks its caller: {:?}",
                guide.findings
            );
            assert_ne!(guide.summary.risk, "low", "{}", guide_text(&guide));
            assert!(
                step.findings
                    .iter()
                    .any(|finding| finding.severity == "error"),
                "{:?}",
                step.findings
            );
            assert_eq!(
                guide.summary.consumers_not_updated, 1,
                "the caller its removal leaves broken is not updated"
            );
        }
    }

    /// The list of changes reads where each one stands from the newest review
    /// that names it by its whole id, and counts every review that does.
    #[test]
    fn the_standing_index_reads_the_newest_review_of_each_change() {
        use kin_model::graph::ReviewStore as _;
        use kin_model::review::{Review, ReviewCompletionState, ReviewDecisionState, ReviewId};
        use kin_model::IdentityRef;

        let Branch {
            graph,
            root,
            first,
            head,
            ..
        } = branch();
        let at = |second: u32| -> kin_model::Timestamp {
            serde_json::from_value(serde_json::json!(format!("2026-10-07T00:00:{second:02}Z")))
                .unwrap()
        };
        let stored = |head_ref: String, state, second: u32| {
            graph
                .create_review(&Review {
                    review_id: ReviewId::new(),
                    title: "a review".to_string(),
                    base_ref: format!("change:{root}"),
                    head_ref,
                    state,
                    completion: ReviewCompletionState::InReview,
                    created_by: IdentityRef::human("ada"),
                    created_at: at(second),
                    updated_at: at(second),
                    scopes: vec![],
                })
                .unwrap();
        };
        stored(format!("change:{head}"), ReviewDecisionState::NeedsWork, 1);
        stored(head.to_string(), ReviewDecisionState::Approved, 5);
        stored(format!("kin:{first}"), ReviewDecisionState::Blocked, 2);
        // A branch name is not a change: where it points moves. An explicit
        // prefix is placed by the ref grammar, which needs repository
        // authority to search, and this fixture holds none, so it is left
        // unplaced rather than guessed at.
        stored("main".to_string(), ReviewDecisionState::Approved, 3);
        stored(
            format!("change:{}", &root.to_string()[..12]),
            ReviewDecisionState::Approved,
            4,
        );

        let dir = tempfile::tempdir().unwrap();
        let layout = kin_core::KinLayout::new(dir.path().to_path_buf());
        let index = standing_index(&binding(&layout), &graph).unwrap().standing;
        assert_eq!(index.len(), 2, "{index:?}");
        let of_head = &index[&head.to_string()];
        assert_eq!((of_head.state.as_str(), of_head.reviews), ("approved", 2));
        let of_first = &index[&first.to_string()];
        assert_eq!((of_first.state.as_str(), of_first.reviews), ("blocked", 1));
    }

    /// A guide's review history is bounded: the latest entries are kept and
    /// the total says how many there were, and a long text is cut with a mark.
    #[test]
    fn a_long_review_history_keeps_its_latest_entries_and_its_total() {
        use kin_model::review::{
            Review, ReviewCompletionState, ReviewDecisionState, ReviewId, ReviewNote, ReviewNoteId,
        };
        use kin_model::IdentityRef;

        let at = |second: u32| -> kin_model::Timestamp {
            serde_json::from_value(serde_json::json!(format!("2026-10-07T00:00:{second:02}Z")))
                .unwrap()
        };
        let review_id = ReviewId::new();
        let record = kin_review::records::ReviewRecord {
            review: Review {
                review_id,
                title: "history".to_string(),
                base_ref: "main".to_string(),
                head_ref: "feature".to_string(),
                state: ReviewDecisionState::Pending,
                completion: ReviewCompletionState::InReview,
                created_by: IdentityRef::human("ada"),
                created_at: at(0),
                updated_at: at(0),
                scopes: vec![],
            },
            decisions: vec![],
            // Held newest first, which is not the order they were written in.
            notes: (0..7u32)
                .rev()
                .map(|index| ReviewNote {
                    note_id: ReviewNoteId::new(),
                    review_id,
                    body: format!("note {index}"),
                    scope: None,
                    authored_by: IdentityRef::human("ada\nbyron"),
                    created_at: at(index),
                })
                .collect(),
            discussions: vec![],
            assignments: vec![],
        };
        let mut budget = REVIEW_TEXT_BUDGET_CHARS;
        let (kept, total) = review_history(&record, 3, &mut budget);
        assert_eq!(total, 7);
        assert_eq!(
            kept.iter()
                .map(|event| event.text.as_deref().unwrap())
                .collect::<Vec<_>>(),
            vec!["note 4", "note 5", "note 6"],
            "the latest three, oldest first"
        );
        assert_eq!(
            budget,
            REVIEW_TEXT_BUDGET_CHARS - 18,
            "only what is kept is paid for"
        );
        assert_eq!(kept[0].by, "ada byron", "a label is carried on one line");

        // With room for one text, the newest entry keeps it and older ones go without.
        let mut budget = 6;
        let (short, _) = review_history(&record, 3, &mut budget);
        assert_eq!(
            short
                .iter()
                .map(|event| (event.text.as_deref().unwrap(), event.text_cut))
                .collect::<Vec<_>>(),
            vec![("", true), ("", true), ("note 6", false)]
        );

        let long = "é".repeat(REVIEW_TEXT_MAX_CHARS + 5);
        let mut budget = REVIEW_TEXT_BUDGET_CHARS;
        let (cut, was_cut) = bounded_text(&long, &mut budget);
        assert!(was_cut);
        assert_eq!(cut.chars().count(), REVIEW_TEXT_MAX_CHARS);
        assert_eq!(budget, REVIEW_TEXT_BUDGET_CHARS - REVIEW_TEXT_MAX_CHARS);
        assert_eq!(
            bounded_text("short", &mut budget),
            ("short".to_string(), false)
        );

        // The guide's whole allowance, not only each text's own, is a bound.
        let mut budget = 5;
        assert_eq!(
            bounded_text("abcdefgh", &mut budget),
            ("abcde".to_string(), true)
        );
        assert_eq!(budget, 0);
        assert_eq!(bounded_text("more", &mut budget), (String::new(), true));
        assert_eq!(bounded_text("", &mut budget), (String::new(), false));

        // A name, title, ref or scope is a label, and a label has its own limit.
        assert_eq!(label_text("ada"), "ada");
        let long_label = label_text(&"é".repeat(REVIEW_LABEL_MAX_CHARS + 50));
        assert_eq!(long_label.chars().count(), REVIEW_LABEL_MAX_CHARS + 1);
        assert!(long_label.ends_with('…'));
        assert_eq!(indented("one\ntwo\nthree", "  "), "one\n  two\n  three");
    }

    /// A change larger than a guide walks says how much it left out.
    #[test]
    fn a_change_larger_than_the_walk_counts_what_it_left() {
        let graph = kin_db::InMemoryGraph::new();
        let entities: Vec<Entity> = (0..STEPS_WALKED_MAX + 5)
            .map(|index| function(&format!("view_{index:03}"), "src/views.ts", 1))
            .collect();
        let head = commit(
            &graph,
            vec![],
            "Import the views",
            added(&entities.iter().collect::<Vec<_>>()),
            vec![],
        );
        let guide = guide_of(&graph, head);
        let walked: usize = guide
            .chapters
            .iter()
            .map(|chapter| chapter.steps.len())
            .sum();
        assert_eq!(walked, STEPS_WALKED_MAX);
        assert_eq!(guide.summary.added, STEPS_WALKED_MAX + 5);
        let rest = guide.not_walked.expect("the cut is reported");
        assert_eq!((rest.total, rest.added), (5, 5));
        assert!(rest
            .message
            .contains(&format!("touches {} entities", STEPS_WALKED_MAX + 5)));
    }

    fn id(byte: u8) -> EntityId {
        EntityId(uuid::Uuid::from_bytes([byte; 16]))
    }

    fn by_name(id: &EntityId) -> (bool, std::cmp::Reverse<usize>, String, EntityId) {
        (false, std::cmp::Reverse(0), id.to_string(), *id)
    }

    #[test]
    fn a_chapter_reads_what_others_rely_on_first() {
        let (leaf, middle, top) = (id(3), id(2), id(1));
        // top calls middle, middle calls leaf.
        let depends = BTreeSet::from([(top, middle), (middle, leaf)]);
        let ordered = order_chapter(&[top, middle, leaf], &depends, &by_name);
        assert_eq!(ordered, vec![leaf, middle, top]);
    }

    #[test]
    fn a_cycle_still_orders_every_member_once() {
        let (a, b, c) = (id(1), id(2), id(3));
        let depends = BTreeSet::from([(a, b), (b, a), (c, a)]);
        let ordered = order_chapter(&[c, b, a], &depends, &by_name);
        assert_eq!(ordered.len(), 3);
        assert_eq!(
            ordered.iter().copied().collect::<BTreeSet<_>>(),
            BTreeSet::from([a, b, c])
        );
        // Nothing is ready while the cycle holds, so the tie-break decides.
        assert_eq!(ordered, vec![a, b, c]);
    }

    #[test]
    fn an_edge_out_of_the_chapter_does_not_hold_a_member_back() {
        let (inside, outside) = (id(1), id(9));
        let depends = BTreeSet::from([(inside, outside)]);
        assert_eq!(order_chapter(&[inside], &depends, &by_name), vec![inside]);
    }

    #[test]
    fn labels_read_one_way_whatever_the_enum_spells() {
        assert_eq!(label(RelationKind::UsesType), "uses_type");
        assert_eq!(label(EntityKind::TraitDef), "trait_def");
        assert_eq!(label(EntityRole::Test), "test");
        assert_eq!(label(kin_model::relation::RelationOrigin::Lsp), "lsp");
    }

    fn field(name: &str) -> GuideFieldChange {
        GuideFieldChange {
            field: name.to_string(),
            before: None,
            after: None,
        }
    }

    #[test]
    fn a_body_only_change_says_the_signature_held() {
        assert_eq!(
            describe_fields(&[field("body")]),
            "Its body changed; its signature did not."
        );
        assert_eq!(
            describe_fields(&[field("signature")]),
            "Its signature changed; its body did not."
        );
        assert_eq!(
            describe_fields(&[field("signature"), field("body")]),
            "Its signature and body changed."
        );
    }

    #[test]
    fn the_summary_says_what_reaches_the_change_and_never_more() {
        let counts = |not_updated, may_need_review, need_no_update| ConsumerVerdictCounts {
            not_updated,
            may_need_review,
            need_no_update,
        };
        assert_eq!(
            summary_narration("change", 0, 1, 0, 3, 0, 1, &counts(0, 3, 0)),
            "This change modified 1 entity, read here in 1 chapter. 3 places outside the \
             change rely on it. None of them changed and the code they rely on did, so they \
             may need review."
        );
        assert_eq!(
            summary_narration("change", 2, 0, 0, 0, 0, 1, &counts(0, 0, 0)),
            "This change added 2 entities, read here in 1 chapter. Nothing outside the change \
             reaches them."
        );
        assert_eq!(
            summary_narration("change", 0, 0, 0, 0, 0, 0, &counts(0, 0, 0)),
            "This change holds no entity a review can act on."
        );
        assert_eq!(
            summary_narration("work", 0, 1, 0, 0, 2, 1, &counts(0, 0, 0)),
            "This work modified 1 entity, read here in 1 chapter. 2 tests outside the work \
             reach it."
        );
        assert_eq!(
            summary_narration("range", 0, 0, 0, 0, 0, 0, &counts(0, 0, 0)),
            "This range holds no entity a review can act on."
        );
        // Each verdict reads as what it asks of the consumers, and a mix is
        // counted out so the parts add up to the whole.
        assert_eq!(
            summary_narration("change", 0, 1, 0, 16, 2, 1, &counts(0, 0, 16)),
            "This change modified 1 entity, read here in 1 chapter. 16 places outside the \
             change rely on it. None of them needs an update: what they rely on kept its \
             signature and behaves as it did. 2 tests reach it."
        );
        assert_eq!(
            summary_narration("change", 0, 1, 0, 1, 0, 1, &counts(1, 0, 0)),
            "This change modified 1 entity, read here in 1 chapter. 1 place outside the change \
             relies on it. That place was not updated, and the change breaks what it relies on."
        );
        assert_eq!(
            summary_narration("change", 0, 3, 0, 8, 0, 2, &counts(1, 3, 4)),
            "This change modified 3 entities, read here in 2 chapters. 8 places outside the \
             change rely on them. Of those places, 1 was not updated, 3 may need review and 4 \
             need no update."
        );
        // A whole id is cut to what a reader can hold; a name is kept.
        let id = "5f85deb4f0d1412333437407043d957c94a2ccb16185de2ccf63dbe1f44aa959";
        assert_eq!(ref_words(id), "5f85deb4f0d1");
        assert_eq!(ref_words(&format!("change:{id}")), "5f85deb4f0d1");
        assert_eq!(ref_words("main"), "main");
        assert_eq!(ref_words("HEAD~3"), "HEAD~3");
    }

    /// An entity whose span is cut out of `text` where `piece` starts in it.
    fn spanned(kind: EntityKind, name: &str, text: &str, piece: &str, at: usize) -> Entity {
        let mut entity = function(name, "src/pkg/signer.py", 1);
        entity.kind = kind;
        entity.language = kin_model::LanguageId::Python;
        let start = at + text.find(piece).expect("the piece is in the text");
        let span = entity.span.as_mut().unwrap();
        span.start_byte = start;
        span.end_byte = start + piece.len();
        entity
    }

    const SIGNER_BEFORE: &str = "class Signer:\n    default_digest_method = sha1\n\n    \
        def verify_signature(self, sig):\n        return base64_decode(sig)\n";
    const VERIFY_BEFORE: &str =
        "def verify_signature(self, sig):\n        return base64_decode(sig)";
    const SIGNER_AFTER: &str = "class Signer:\n    default_digest_method = sha1\n\n    \
        def verify_signature(self, sig):\n        return base64_decode(sig, limit=128)\n";
    const VERIFY_AFTER: &str =
        "def verify_signature(self, sig):\n        return base64_decode(sig, limit=128)";

    /// The evaluator's case: one method's call changed, and the class records
    /// that as its own modification because its text holds the method. With
    /// the method cut out, the class reads the same on both sides, so it is
    /// read through the method. A class attribute that moved beside the
    /// method keeps the class a step of its own.
    ///
    /// Falsify by comparing the whole bodies in `changed_through_members`:
    /// the class then reads as changed and gets the method's diff twice.
    #[test]
    fn a_class_whose_own_text_held_still_is_read_through_its_members() {
        let offset = 400;
        let class_before = spanned(
            EntityKind::Class,
            "Signer",
            SIGNER_BEFORE,
            SIGNER_BEFORE,
            offset,
        );
        let method_before = spanned(
            EntityKind::Method,
            "Signer.verify_signature",
            SIGNER_BEFORE,
            VERIFY_BEFORE,
            offset,
        );
        let mut class_after = class_before.clone();
        class_after.span.as_mut().unwrap().end_byte = offset + SIGNER_AFTER.len();
        let mut method_after = spanned(
            EntityKind::Method,
            "Signer.verify_signature",
            SIGNER_AFTER,
            VERIFY_AFTER,
            offset,
        );
        method_after.id = method_before.id;
        assert!(recorded_inside(&method_before, &class_before));
        assert!(!recorded_inside(&class_before, &method_before));
        assert!(declaration_held(&class_before, &class_after));

        let before = own_text(SIGNER_BEFORE, &class_before, &[&method_before]).unwrap();
        let after = own_text(SIGNER_AFTER, &class_after, &[&method_after]).unwrap();
        assert_eq!(before, after, "only the method moved");
        assert_eq!(before, "class Signer: default_digest_method = sha1");
        assert_ne!(
            SIGNER_BEFORE, SIGNER_AFTER,
            "the class's whole text did move, which is why it was recorded as modified"
        );

        // The attribute moved too: the class changed itself.
        let attribute = SIGNER_AFTER.replace("sha1", "sha256");
        let mut class_attribute = class_before.clone();
        class_attribute.span.as_mut().unwrap().end_byte = offset + attribute.len();
        let mut method_moved = spanned(
            EntityKind::Method,
            "Signer.verify_signature",
            &attribute,
            VERIFY_AFTER,
            offset,
        );
        method_moved.id = method_before.id;
        assert_ne!(
            own_text(&attribute, &class_attribute, &[&method_moved]).unwrap(),
            before
        );

        // A method added beside an unchanged one leaves the class as it was.
        let added_text =
            format!("{SIGNER_BEFORE}\n    def sign(self, value):\n        return value\n");
        let mut class_grown = class_before.clone();
        class_grown.span.as_mut().unwrap().end_byte = offset + added_text.len();
        let sign = spanned(
            EntityKind::Method,
            "Signer.sign",
            &added_text,
            "def sign(self, value):\n        return value",
            offset,
        );
        assert_eq!(
            own_text(&added_text, &class_grown, &[&method_before, &sign]).unwrap(),
            before
        );

        // A body that is not the container's whole span reads as nothing,
        // never as unchanged.
        assert!(own_text("class Signer: pass", &class_before, &[]).is_none());
        // Documentation that moved is a change to what the class declares.
        let mut documented = class_after.clone();
        documented.doc_summary = Some("Signs values.".to_string());
        assert!(!declaration_held(&class_before, &documented));
    }

    fn relation_with_evidence(span_start: usize, keywords: &[&str]) -> Relation {
        let span_file = kin_model::FilePathId::new("src/pkg/signer.py");
        Relation {
            id: kin_model::RelationId::new(),
            kind: RelationKind::Calls,
            src: GraphNodeId::Entity(EntityId::new()),
            dst: GraphNodeId::Entity(EntityId::new()),
            confidence: 1.0,
            origin: RelationOrigin::Parsed,
            created_in: None,
            import_source: None,
            evidence: vec![kin_model::relation::RelationEvidence {
                source_span: Some(kin_model::SourceSpan {
                    file: span_file,
                    start_byte: span_start,
                    end_byte: span_start + 10,
                    start_line: 3,
                    start_col: 0,
                    end_line: 3,
                    end_col: 10,
                }),
                call_shape: Some(kin_model::relation::CallArgShape {
                    positional: 1,
                    keywords: keywords.iter().map(ToString::to_string).collect(),
                    ..Default::default()
                }),
                ..Default::default()
            }],
        }
    }

    /// A call site that moved down five lines because lines above it were
    /// added is the same relation, and a call that now passes `limit` is not.
    #[test]
    fn a_relation_that_only_moved_with_its_lines_is_counted_apart() {
        let old = relation_with_evidence(100, &[]);
        let mut moved = old.clone();
        moved.evidence[0].source_span.as_mut().unwrap().start_byte = 180;
        moved.created_in = Some(SemanticChangeId::from_hash(kin_model::Hash256::from_bytes(
            [3; 32],
        )));
        assert!(relation_moved_only_its_place(&old, &moved));

        let mut reshaped = moved.clone();
        reshaped.evidence[0].call_shape.as_mut().unwrap().keywords = vec!["limit".to_string()];
        assert!(!relation_moved_only_its_place(&old, &reshaped));

        let mut retargeted = moved.clone();
        retargeted.kind = RelationKind::References;
        assert!(!relation_moved_only_its_place(&old, &retargeted));

        let mut more_evidence = moved;
        more_evidence
            .evidence
            .push(more_evidence.evidence[0].clone());
        assert!(!relation_moved_only_its_place(&old, &more_evidence));
    }

    /// The summary and every step leave out relations that only moved with
    /// their lines, and say how many there were.
    #[test]
    fn a_guide_counts_relations_that_only_moved_apart_from_real_ones() {
        let graph = kin_db::InMemoryGraph::new();
        let decode = function("base64_decode", "src/encoding.ts", 1);
        let verify = function("verify_signature", "src/signer.ts", 2);
        let unsign = function("unsign", "src/timed.ts", 3);
        let mut first = relation_with_evidence(100, &[]);
        first.src = GraphNodeId::Entity(verify.id);
        first.dst = GraphNodeId::Entity(decode.id);
        let mut second = relation_with_evidence(200, &[]);
        second.src = GraphNodeId::Entity(unsign.id);
        second.dst = GraphNodeId::Entity(decode.id);
        let root = commit(
            &graph,
            vec![],
            "Add decoding",
            added(&[&decode, &verify, &unsign]),
            vec![
                RelationDelta::Added { new: first.clone() },
                RelationDelta::Added {
                    new: second.clone(),
                },
            ],
        );
        let mut edited = decode.clone();
        edited.fingerprint.behavior_hash = kin_model::Hash256::from_bytes([9; 32]);
        let mut first_moved = first.clone();
        first_moved.evidence[0]
            .source_span
            .as_mut()
            .unwrap()
            .start_byte = 160;
        let mut second_reshaped = second.clone();
        second_reshaped.evidence[0]
            .call_shape
            .as_mut()
            .unwrap()
            .keywords = vec!["limit".into()];
        let head = commit(
            &graph,
            vec![root],
            "Bound base64 input before decoding",
            vec![EntityDelta::Modified {
                old: decode.clone(),
                new: edited,
            }],
            vec![
                RelationDelta::Modified {
                    old: first,
                    new: first_moved,
                },
                RelationDelta::Modified {
                    old: second,
                    new: second_reshaped,
                },
            ],
        );
        let guide = guide_of(&graph, head);
        assert_eq!(guide.summary.relation_changes, 1);
        assert_eq!(guide.summary.relation_changes_position_only, 1);
        let step = &guide.chapters[0].steps[0];
        assert_eq!(step.connections.relation_changes_total, 1);
        assert_eq!(step.connections.relation_changes[0].other, "unsign");
        assert!(guide_text(&guide).contains(
            "1 recorded relation moved only with the lines around it and is not walked."
        ));
    }

    fn python(kind: EntityKind, name: &str, signature: &str, body: u8) -> Entity {
        let mut entity = function(name, "src/pkg/serializer.py", body);
        entity.kind = kind;
        entity.language = kin_model::LanguageId::Python;
        entity.signature = signature.to_string();
        entity
    }

    fn related(kind: RelationKind, source: &Entity, target: &Entity) -> RelationDelta {
        let RelationDelta::Added { mut new } = calls(source, target) else {
            unreachable!("calls builds an addition")
        };
        new.kind = kind;
        RelationDelta::Added { new }
    }

    /// `Serializer.__init__` has `@overload` stubs above its implementation,
    /// and every one of them names the `Signer` type. A reader sees one
    /// callable, so `Signer`'s list shows one row that counts the stubs, and a
    /// change reached by only the stubs counts the same callable, not a second
    /// place. Two methods that share a name and each define a body are kept
    /// apart and labelled by their parameters.
    ///
    /// Falsify by returning each entity's own id from `canonical_callable`:
    /// the list repeats `Serializer.__init__` three times and the summary
    /// counts three consumers.
    #[test]
    fn overload_stubs_read_as_the_one_callable_they_declare() {
        let graph = kin_db::InMemoryGraph::new();
        let signer = python(EntityKind::Class, "Signer", "class Signer", 1);
        let make = python(EntityKind::Function, "make_signer", "def make_signer()", 2);
        let serializer = python(EntityKind::Class, "Serializer", "class Serializer", 3);
        let implementation = python(
            EntityKind::Method,
            "Serializer.__init__",
            "def __init__(self, signer=None)",
            4,
        );
        let stub_none = python(
            EntityKind::Method,
            "Serializer.__init__",
            "@overload def __init__(self, signer: None = None) -> None",
            5,
        );
        let stub_type = python(
            EntityKind::Method,
            "Serializer.__init__",
            "@overload def __init__(self, signer: type[Signer]) -> None",
            6,
        );
        let getter = python(
            EntityKind::Method,
            "Serializer.key",
            "@property def key(self)",
            7,
        );
        let setter = python(
            EntityKind::Method,
            "Serializer.key",
            "@key.setter def key(self, value)",
            8,
        );
        let root = commit(
            &graph,
            vec![],
            "Add serializers",
            added(&[
                &signer,
                &make,
                &serializer,
                &implementation,
                &stub_none,
                &stub_type,
                &getter,
                &setter,
            ]),
            vec![
                related(RelationKind::Contains, &serializer, &implementation),
                related(RelationKind::Contains, &serializer, &stub_none),
                related(RelationKind::Contains, &serializer, &stub_type),
                related(RelationKind::Contains, &serializer, &getter),
                related(RelationKind::Contains, &serializer, &setter),
                related(RelationKind::UsesType, &implementation, &signer),
                related(RelationKind::UsesType, &stub_none, &signer),
                related(RelationKind::UsesType, &stub_type, &signer),
                calls(&getter, &signer),
                calls(&setter, &signer),
                calls(&stub_none, &make),
                calls(&stub_type, &make),
            ],
        );
        let mut renamed = signer.clone();
        renamed.signature = "class Signer(object)".to_string();
        renamed.fingerprint.behavior_hash = kin_model::Hash256::from_bytes([9; 32]);
        let mut remade = make.clone();
        remade.fingerprint.behavior_hash = kin_model::Hash256::from_bytes([10; 32]);
        let head = commit(
            &graph,
            vec![root],
            "Make the signer an object",
            vec![
                EntityDelta::Modified {
                    old: signer.clone(),
                    new: renamed,
                },
                EntityDelta::Modified {
                    old: make.clone(),
                    new: remade,
                },
            ],
            vec![],
        );
        let guide = guide_of(&graph, head);
        let steps: Vec<&GuideStep> = guide
            .chapters
            .iter()
            .flat_map(|chapter| &chapter.steps)
            .collect();
        let signer_step = steps.iter().find(|step| step.name == "Signer").unwrap();
        let rows: Vec<(&str, usize, Option<&str>)> = signer_step
            .connections
            .consumers
            .iter()
            .map(|row| {
                (
                    row.entity.name.as_str(),
                    row.overloads,
                    row.entity.parameters.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("Serializer.__init__", 2, None),
                ("Serializer.key", 0, Some("(self)")),
                ("Serializer.key", 0, Some("(self, value)")),
            ]
        );
        assert_eq!(
            signer_step.connections.consumers[0].entity.id,
            implementation.id.to_string()
        );
        assert_eq!(signer_step.connections.consumers_total, 3);

        // `make_signer` is reached by the stubs alone, which still stand for
        // the implementation, so the change has three consumers, not four.
        let make_step = steps
            .iter()
            .find(|step| step.name == "make_signer")
            .unwrap();
        assert_eq!(make_step.connections.consumers.len(), 1);
        assert_eq!(make_step.connections.consumers[0].overloads, 1);
        assert_eq!(guide.summary.recorded_consumers, 3);
        let text = guide_text(&guide);
        assert!(text.contains("2 more declarations"), "{text}");
        assert!(text.contains("Serializer.key(self, value)"), "{text}");
    }

    /// A Git signature reads as the person, a native author as its name, and
    /// a time as a date a reader can hold.
    #[test]
    fn an_author_reads_as_a_person_and_a_time_as_a_date() {
        assert_eq!(
            person("Armin Ronacher <armin.ronacher@active-4.com> 1540586649 +0200"),
            "Armin Ronacher"
        );
        assert_eq!(
            person("Review Eval <review-eval@example.invalid>"),
            "Review Eval"
        );
        assert_eq!(
            person("<ci@example.com> 1540586649 -0700"),
            "ci@example.com"
        );
        assert_eq!(person("reviewer@example.com"), "reviewer@example.com");
        assert_eq!(person("Build 2026 +0200 team"), "Build 2026 +0200 team");
        assert_eq!(
            readable_time("2018-10-26T22:44:09+02:00"),
            "2018-10-26 20:44 UTC"
        );
        assert_eq!(readable_time("not a time"), "not a time");
        assert_eq!(
            parameters_of("@overload def f(x: Callable[[int], str], y=(1, 2)) -> None"),
            Some("(x: Callable[[int], str], y=(1, 2))".to_string())
        );
        assert_eq!(parameters_of("class Signer"), None);
    }

    fn titled(message: &str) -> SemanticChange {
        SemanticChange {
            id: SemanticChangeId::from_hash(kin_model::Hash256::from_bytes([1; 32])),
            parents: vec![],
            origin: kin_model::ChangeOrigin::Native,
            timestamp: kin_model::Timestamp::now(),
            author: kin_model::AuthorId::new("Ada <ada@example.com>"),
            message: message.to_string(),
            entity_deltas: vec![],
            relation_deltas: vec![],
            tree_deltas: vec![],
            projected_files: vec![],
            spec_link: None,
            evidence: vec![],
            risk_summary: None,
            admission_policy_delta: None,
            external_reference_deltas: Vec::new(),
            resolution_record_deltas: Vec::new(),
        }
    }

    /// A range is named by what its changes say, never by two ids, and a
    /// whole Git commit id in a sentence is cut like a change id.
    #[test]
    fn a_range_is_titled_by_its_changes() {
        let newest = titled("more name cleanup, parametrize fallback test\n\nBody.");
        let older = titled("Added fallback signers and switch back to sha1");
        assert_eq!(
            range_title(&[newest.clone(), older], 10, "2ef8fe08c159", "ef8fd9894efa"),
            "\u{201c}more name cleanup, parametrize fallback test\u{201d} and 9 earlier changes"
        );
        assert_eq!(
            range_title(std::slice::from_ref(&newest), 1, "a", "b"),
            "more name cleanup, parametrize fallback test"
        );
        assert_eq!(
            range_title(&[], 0, "main", "HEAD"),
            "Nothing between main and HEAD"
        );
        let git = "2ef8fe08c159de9f1232dbab86f8606aecd6392b";
        assert_eq!(ref_words(git), "2ef8fe08c159");
        assert_eq!(ref_words(&format!("git:{git}")), "2ef8fe08c159");
    }

    /// A list of changes counts what each guide will walk: module containers
    /// apart, a merge counted by what it brought in, an unknown id left out.
    #[test]
    fn a_list_counts_what_each_guide_walks() {
        let graph = kin_db::InMemoryGraph::new();
        let mut module = function("encoding", "src/encoding.ts", 1);
        module.kind = EntityKind::Module;
        let decode = function("base64_decode", "src/encoding.ts", 2);
        let root = commit(
            &graph,
            vec![],
            "Add decoding",
            added(&[&module, &decode]),
            vec![],
        );
        let side = commit(&graph, vec![root], "Side", vec![], vec![]);
        let merge = commit(&graph, vec![root, side], "Merge", vec![], vec![]);
        let counts = change_counts(
            &graph,
            &[
                root.to_string(),
                merge.to_string(),
                "not-a-change".to_string(),
            ],
            None,
        )
        .unwrap();
        assert_eq!(
            counts.counts[&root.to_string()],
            GuideChangeCount {
                entities: 1,
                module_containers: 1,
                containers_through_members: 0,
                merge: false,
                merge_counted: false,
            }
        );
        let merged = &counts.counts[&merge.to_string()];
        assert!(merged.merge && merged.merge_counted);
        assert_eq!(merged.entities, 0, "the side branch brought in no entity");
        assert_eq!(counts.counts.len(), 2);
        assert_eq!(
            guide_of(&graph, root).summary.added,
            counts.counts[&root.to_string()].entities,
            "the list and the walk say the same number"
        );
        let too_many: Vec<String> = (0..=COUNTS_ASKED_MAX).map(|_| root.to_string()).collect();
        assert!(change_counts(&graph, &too_many, None).is_err());
    }

    /// A merge in the list says the number its own guide leads with: what
    /// it brought into its first parent's line, and not the history behind.
    #[test]
    fn a_merge_in_the_list_counts_what_its_guide_walks() {
        let graph = kin_db::InMemoryGraph::new();
        let on_main = function("handleRunsRequest", "src/api/runsRoute.ts", 1);
        let on_branch = function("listRun", "src/runs/listRun.ts", 2);
        let root = commit(
            &graph,
            vec![],
            "Add run listing",
            added(&[&on_main, &on_branch]),
            vec![calls(&on_main, &on_branch)],
        );
        let mut main_side = on_main.clone();
        main_side.fingerprint.behavior_hash = kin_model::Hash256::from_bytes([8; 32]);
        let main = commit(
            &graph,
            vec![root],
            "Change the route on main",
            vec![EntityDelta::Modified {
                old: on_main.clone(),
                new: main_side,
            }],
            vec![],
        );
        let mut branch_side = on_branch.clone();
        branch_side.fingerprint.behavior_hash = kin_model::Hash256::from_bytes([9; 32]);
        let added_on_branch = function("pageRuns", "src/runs/pageRuns.ts", 3);
        let first_on_branch = commit(
            &graph,
            vec![root],
            "Shorten the cache on a branch",
            vec![EntityDelta::Modified {
                old: on_branch.clone(),
                new: branch_side,
            }],
            vec![],
        );
        let branch = commit(
            &graph,
            vec![first_on_branch],
            "Page the runs",
            added(&[&added_on_branch]),
            vec![],
        );
        let merge = commit(
            &graph,
            vec![main, branch],
            "Merge the branch",
            vec![],
            vec![],
        );
        let counts = change_counts(&graph, &[merge.to_string()], None).unwrap();
        let counted = &counts.counts[&merge.to_string()];
        assert!(counted.merge && counted.merge_counted);
        let summary = guide_of(&graph, merge).summary;
        assert_eq!(
            counted.entities,
            summary.added + summary.modified + summary.removed,
            "the list and the merge's walk say the same number"
        );
        assert_eq!(counted.entities, 2);
    }
}
