// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Context packs built from several focal entities at once.
//!
//! A real question names several things. "When I type a character in the
//! editor, how does it end up in the document" names the typing, the editor and
//! the document, and the answer is the material between them. The single-focal
//! builder in [`crate::builder`] answers a different question: it takes one
//! entity and spends the whole budget around it, so a pack built for one end of
//! a chain carries that end twice over and the other end not at all.
//!
//! This module assembles a pack from every focal, under three rules:
//!
//! 1. **Every focal is in the pack.** Focal bodies are admitted first, each at
//!    the deepest projection its share affords, so a chain question never comes
//!    back describing one end.
//! 2. **The route between connected focals comes before either focal's
//!    neighbourhood.** Two focals joined by a dependency chain are joined by
//!    something, and that something is the answer. Spending the budget on the
//!    ends first is what leaves the middle out.
//! 3. **The rest is water-filled.** Each focal's neighbourhood gets an equal
//!    share; a focal that needs less than its share hands the surplus back and
//!    the rest split it again, so a short neighbourhood never holds budget a
//!    long one needed.
//!
//! The pack also comes in **under** the budget it was given. The single-focal
//! builder keeps one row per section whatever the budget says, which is a
//! deliberate choice there and reports itself in the rendering, but it means
//! `--budget 500` can return more than 500 tokens. Here the assembled output is
//! rendered, measured with [`crate::estimate_tokens`], and shrunk row by row
//! until it fits, and the measured number is reported beside the requested one.

use std::collections::{BTreeMap, HashMap, HashSet};

use kin_model::relation::RelationKind;
use kin_model::{
    ContextEntry, ContextPack, Entity, EntityId, GraphNodeId, GraphStore, ProjectionLevel,
    TokenBudget,
};
use serde::{Deserialize, Serialize};

use crate::builder::{
    build_context_pack_with_provenance, group, is_dependency_edge, project_full_body,
    project_name_and_kind, project_signature_only, source_projection, AssistantHint, BodyCandidate,
    ContextOptions, ContextProjectionProvider, DependencyRelation, ProjectionLimits,
    ProjectionReport, FULL_BODY_PROJECTION_NAME, SERVED_BODY_PROJECTION_NAME,
};
use crate::error::{ContextError, Result};
use crate::tokens::estimate_tokens;

/// The reason code a row the token budget refused carries.
///
/// The same string `kin_mcp::budget::ELISION_REASON_TOKEN_BUDGET` and the CLI's
/// `CONTEXT_ELISION_REASON_TOKEN_BUDGET` publish, so a reader moving between the
/// three surfaces is not learning three vocabularies for one fact.
pub const ELISION_REASON_TOKEN_BUDGET: &str = "token_budget";

/// The elision group naming focals the budget could not carry at all.
pub const FOCAL_GROUP: &str = "focal_entities";

/// The pack section route rows join.
///
/// [`ContextPack`] is defined in `kin-model`, which this repository consumes
/// from the registry at a pinned version, so route material cannot have a field
/// of its own yet. `transitive_deps` is its honest home rather than a hiding
/// place: a route entity is reached at more than one hop, which is exactly what
/// that section means. Every route row also opens with [`ROUTE_MARKER`] and is
/// named by id in [`RouteReport::via`], so no reader has to parse a comment to
/// tell a route row from a transitive dependency.
pub const ROUTE_GROUP: &str = "route";

/// The comment a route row's content opens with.
pub const ROUTE_MARKER: &str = "// route:";

/// How far a route search walks from one focal before giving up.
///
/// Four hops is two more than the single-focal pack's own depth, which is what
/// makes a route worth searching for at all: the pair this exists for is the
/// one neither focal's neighbourhood already reaches.
pub const ROUTE_MAX_HOPS: usize = 4;

/// How many entities one route search may expand before it stops.
///
/// A bound that stops a search is reported as [`RouteSearch::bounded`] rather
/// than folded into "no route found". They are different facts: one says the
/// graph joins nothing, the other says nobody looked far enough, and a caller
/// deciding whether "these are unrelated" is safe needs to know which it has.
pub const ROUTE_VISIT_MAX: usize = 4_000;

/// Passes allowed to settle the measured token count against the bytes that
/// carry it.
///
/// The measurement is self-referential: the method line names the number, so
/// writing it back changes the text being counted. It settles because
/// [`estimate_tokens`] counts a number as one token however many digits it has,
/// so the second pass confirms the first rather than chasing it. Borrowed
/// wholesale from `kin_mcp`'s `serialize_with_measured_tokens`, which solved
/// the same problem on the MCP payload.
const MEASURED_TOKEN_PASSES: usize = 4;

/// Options for [`build_multi_focal_pack`].
#[derive(Debug, Clone)]
pub struct MultiFocalOptions {
    pub budget: TokenBudget,
    /// Neighbourhood depth for a single focal. Shrinks with the focal count;
    /// see [`neighborhood_depth_for`].
    pub max_depth: u32,
    pub include_tests: bool,
    pub include_contracts: bool,
    pub assistant_hint: Option<AssistantHint>,
    /// How the caller arrived at each focal, in the same order as the focal
    /// ids. Purely descriptive: the assembler renders it into the method line
    /// and counts the tokens it costs, so the budget it guarantees covers every
    /// byte the caller will print.
    pub resolutions: Vec<FocalResolution>,
    /// One sentence on the store's semantic coverage, from the caller that
    /// asked the graph for it. Rendered into the method line, so a reader
    /// learns what the ranking behind these focals could and could not see.
    pub coverage: Option<String>,
    /// Serve each focal's exact body when its share of the budget holds it.
    /// False serves every focal as its signature, for a caller that already
    /// holds the bodies and wants the neighbourhood around them.
    pub focal_bodies: bool,
    /// Also offer the exact bodies of each focal's dependencies and
    /// dependents. A neighbour whose body does not fit its share keeps its
    /// signature and is named in [`MultiFocalReport::withheld_rows`] as a
    /// withheld body, so the caller can read it by id.
    pub neighbor_bodies: bool,
}

impl Default for MultiFocalOptions {
    fn default() -> Self {
        Self {
            budget: TokenBudget::Small8k,
            max_depth: 2,
            include_tests: true,
            include_contracts: true,
            assistant_hint: None,
            resolutions: Vec::new(),
            coverage: None,
            focal_bodies: true,
            neighbor_bodies: false,
        }
    }
}

/// A row the budget kept out of the pack, or kept only as its signature, by
/// entity id.
///
/// Counts per group say how much was cut. They do not say what, and a caller
/// holding only a count cannot ask for the missing rows: it would have to
/// rebuild the neighbourhood itself. The ids are the recovery path, one
/// entity read each.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WithheldRow {
    pub entity_id: String,
    /// The pack group the row would have joined.
    pub group: String,
    /// [`WITHHELD_BODY`] when the row is in the pack as its signature and only
    /// its body was withheld, [`WITHHELD_ROW`] when the row is not in the pack.
    pub withheld: String,
}

/// The row is in the pack as its signature; its body did not fit.
pub const WITHHELD_BODY: &str = "body";

/// The row is not in the pack at all.
pub const WITHHELD_ROW: &str = "row";

/// The reason a neighbour or focal body the budget could not carry records.
pub const BODY_OVER_ALLOWANCE: &str = "whole source body exceeds its token allowance";

/// How a caller arrived at one focal.
///
/// A pack whose focals were located from a question is a different claim from
/// one whose focals were named, and a reader deciding how much to trust the
/// selection needs the difference. The score is the ranking's own number,
/// carried rather than re-derived.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FocalResolution {
    /// `id`, `name` or `question`.
    pub route: String,
    /// What the caller typed. Empty when the question route chose this focal.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub query: String,
    /// The ranking score, on the question route only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    /// How many entities the store carries under this exact name.
    ///
    /// Two is the ordinary case for a definition beside its header
    /// declaration, and it is the case that used to be invisible: the store
    /// lists twins in no stable order, so two ingests of one tree resolved the
    /// same name to different entities. Reported so a reader knows a choice was
    /// made, and so a caller that wants the other twin knows there is one.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub twins: usize,
    /// What pinned this twin, when the caller said which one they meant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

impl FocalResolution {
    /// The caller gave this focal's entity id.
    pub fn by_id(query: impl Into<String>) -> Self {
        Self {
            route: "id".to_string(),
            query: query.into(),
            score: None,
            twins: 0,
            pin: None,
        }
    }

    /// The caller gave a name that resolved to this focal.
    pub fn by_name(query: impl Into<String>) -> Self {
        Self {
            route: "name".to_string(),
            query: query.into(),
            score: None,
            twins: 0,
            pin: None,
        }
    }

    /// The question's own ranking chose this focal, at this score.
    pub fn from_question(score: f32) -> Self {
        Self {
            route: "question".to_string(),
            query: String::new(),
            score: Some(score),
            twins: 0,
            pin: None,
        }
    }

    /// Note that this name has twins in the store, and which one was taken.
    pub fn with_twins(mut self, twins: usize, pin: Option<String>) -> Self {
        self.twins = twins;
        self.pin = pin;
        self
    }

    /// One clause for the method line.
    fn describe(&self) -> String {
        let base = match self.route.as_str() {
            "question" => match self.score {
                Some(score) => format!("located from the question, score {score:.2}"),
                None => "located from the question".to_string(),
            },
            "id" => "named by id".to_string(),
            _ if self.query.is_empty() => "named".to_string(),
            _ => format!("named as {}", self.query),
        };
        match (self.twins, &self.pin) {
            (0 | 1, _) => base,
            (twins, Some(pin)) => format!("{base}, 1 of {twins} under that name, pinned by {pin}"),
            (twins, None) => format!("{base}, 1 of {twins} under that name"),
        }
    }
}

impl Default for FocalResolution {
    fn default() -> Self {
        Self {
            route: "name".to_string(),
            query: String::new(),
            score: None,
            twins: 0,
            pin: None,
        }
    }
}

/// What one focal put into the pack.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FocalContribution {
    pub entity_id: String,
    pub name: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// `full_body`, `signature_only`, `name_and_kind`, or `elided` when the
    /// budget could not carry even this focal's name.
    pub projection: String,
    /// Tokens the focal's own entry costs.
    pub focal_tokens: usize,
    /// Neighbourhood rows this focal contributed.
    pub neighborhood_rows: usize,
    /// Tokens those rows cost.
    pub neighborhood_tokens: usize,
    /// Rows this focal's neighbourhood offered that an earlier focal or a route
    /// had already contributed. Reported rather than dropped silently: overlap
    /// is evidence the focals are related, which is the opposite of waste.
    pub shared_rows: usize,
    /// Rows the budget refused after this focal's allowance ran out.
    pub withheld_rows: usize,
    pub resolution: FocalResolution,
}

/// A route the graph carries between two focals.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteReport {
    pub from: String,
    pub from_name: String,
    pub to: String,
    pub to_name: String,
    /// Edges on the route, which is one more than the entities between the
    /// ends.
    pub hops: usize,
    /// Entities admitted from this route, in walk order from `from` to `to`.
    pub via: Vec<String>,
    pub via_names: Vec<String>,
    pub tokens: usize,
    /// Route entities the budget could not carry, or that another focal had
    /// already put in the pack.
    pub withheld: usize,
    /// The graph edges the walk crossed, in walk order, one per hop.
    ///
    /// A route without this is a claim a reader cannot check. Measured on
    /// psf/requests at v0.7.2, `get_context_pack` joined
    /// `Session.merge_environment_settings` to
    /// `HTTPAdapter.build_connection_pool_key_attributes` "in 2 hops through
    /// `RequestEncodingMixin._encode_params`", and a reader had no way to learn
    /// that both hops are `References` edges pointing INTO the middle node, so
    /// the graph carries no path from one focal to the other at all, only two
    /// rows that mention the same third thing. Both facts are in this field.
    ///
    /// Defaulted on deserialization so a pack written before this field
    /// existed still reads back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<RouteEdgeReport>,
}

/// One graph edge a route walk crossed.
///
/// `from_name` and `to_name` are in WALK order, which is the order a reader
/// follows the route in. `direction` says whether the edge in the graph agrees
/// with that order, because [`route_between`] deliberately ignores direction
/// and the two readings are different claims: `A -Calls-> B -Calls-> C` is a
/// chain, while `A -References-> X <-References- C` is two rows mentioning the
/// same thing and is not a path between A and C in any useful sense.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteEdgeReport {
    pub from_name: String,
    pub to_name: String,
    /// The relation kind, spelled exactly as the graph holds it.
    pub kind: String,
    /// [`ROUTE_EDGE_FORWARD`] when the walk followed the edge from its source
    /// to its destination, [`ROUTE_EDGE_REVERSE`] when it read it backwards.
    pub direction: String,
}

/// The walk followed this edge from its source to its destination.
pub const ROUTE_EDGE_FORWARD: &str = "forward";

/// The walk read this edge backwards, from its destination to its source.
pub const ROUTE_EDGE_REVERSE: &str = "reverse";

/// A route's edges as one line a human reads left to right.
///
/// `A -Calls-> B` for an edge the walk followed and `A <-Calls- B` for one it
/// read backwards, so the two shapes [`RouteEdgeReport`] exists to separate are
/// separated on sight. Empty for a route with no edges, which cannot happen for
/// a reported route and is rendered as the empty string rather than as a claim.
pub fn render_route_edges(edges: &[RouteEdgeReport]) -> String {
    let Some(first) = edges.first() else {
        return String::new();
    };
    let mut line = first.from_name.clone();
    for edge in edges {
        if edge.direction == ROUTE_EDGE_REVERSE {
            line.push_str(&format!(" <-{}- {}", edge.kind, edge.to_name));
        } else {
            line.push_str(&format!(" -{}-> {}", edge.kind, edge.to_name));
        }
    }
    line
}

/// What the route search did, including whether a bound stopped it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RouteSearch {
    pub pairs_examined: usize,
    pub pairs_connected: usize,
    /// True when [`ROUTE_VISIT_MAX`] stopped a search before it could answer.
    /// While this is true, an absent route is not evidence that none exists.
    pub bounded: bool,
    pub max_hops: usize,
    /// Connected pairs whose route description the reply left out to fit its
    /// token budget, so `routes` lists fewer than `pairs_connected`.
    ///
    /// A route description names both ends, every entity between them and
    /// every edge crossed, and a question naming five focals can connect ten
    /// pairs. Once every row is out and every focal is down to its signature,
    /// those descriptions can cost more than the focals do, and a pack that
    /// kept them had to refuse instead of answering.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub routes_withheld: usize,
}

/// What one pack section lost to the token budget.
///
/// The same four fields, under the same names, that `kin context --json` and
/// the `get_context_pack` MCP tool already publish.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackElision {
    pub elided: usize,
    pub kept: usize,
    pub total: usize,
    pub reason: String,
}

/// One focal's reason for including a context row. These are selection facts,
/// not containment claims or proof of a dependency chain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextRowReason {
    pub focal_entity_id: EntityId,
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency_relation: Option<DependencyRelation>,
}

/// A retained row's display section and every focal that offered it.
///
/// The section follows admission order. A shared row can have opposite roles
/// for different focals, so only the focal-specific reasons express direction.
/// Reasons describe focal selections, not an exhaustive graph-edge inventory;
/// a focal's selection can prefer a leaving edge when the pair is reciprocal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextRowProvenance {
    pub section: String,
    pub reasons: Vec<ContextRowReason>,
}

/// How a multi-focal pack was built, and what it cost.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MultiFocalReport {
    pub focals: Vec<FocalContribution>,
    pub routes: Vec<RouteReport>,
    pub route_search: RouteSearch,
    /// The neighbourhood depth each focal got, after shaping by focal count.
    pub neighborhood_depth: u32,
    /// The budget the caller asked for.
    pub budget_tokens: usize,
    /// What the rendered output actually costs, by [`crate::estimate_tokens`].
    /// Never above `budget_tokens`: the assembler drops rows until it is not.
    pub measured_tokens: usize,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub elisions: BTreeMap<String, PackElision>,
    /// Selection provenance for retained rows, including contributions from
    /// focals whose shared row was admitted through another focal or a route.
    /// Older reports omit this field; absence does not establish a dependency.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub row_provenance: BTreeMap<EntityId, ContextRowProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<String>,
    /// The method sentence, as rendered into the human form.
    pub method: String,
    /// `full` or `compact`.
    ///
    /// The method line costs tokens like everything else, and on a tight budget
    /// with several focals it can be the largest single thing in the pack. When
    /// dropping every row still leaves the output over budget, the sentence
    /// itself shortens rather than the pack silently going over, and this field
    /// is how a reader knows which sentence they are reading.
    #[serde(default = "default_method_style")]
    pub method_style: String,
    /// Every row the budget withheld, whole or down to its signature, by id and
    /// in the order it was withheld. `elisions` counts them per group; this
    /// names them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub withheld_rows: Vec<WithheldRow>,
    /// Withheld rows counted but not named in `withheld_rows`, because the
    /// caller capped the list with [`MultiFocalReport::list_withheld_rows_at_most`].
    #[serde(default, skip_serializing_if = "is_zero")]
    pub withheld_rows_unlisted: usize,
}

impl MultiFocalReport {
    /// Name at most `max` withheld rows and count the rest.
    ///
    /// Every row the budget drops adds one entry to `withheld_rows`, so a
    /// caller that holds the serialized report to the token budget pays more
    /// for the list with every row it drops to fit. On a five-focal question
    /// over psf/requests that list reached forty-eight entries, and the reply
    /// could no longer fit a budget the answer itself fitted easily. Focals are
    /// listed first, because a focal left out is the answer coming back
    /// narrower than the question.
    pub fn list_withheld_rows_at_most(&mut self, max: usize) {
        let total = self.withheld_rows.len() + self.withheld_rows_unlisted;
        self.withheld_rows
            .sort_by_key(|row| row.group != FOCAL_GROUP);
        self.withheld_rows.truncate(max);
        self.withheld_rows_unlisted = total - self.withheld_rows.len();
    }

    /// Record one withheld row, once per entity. A row several focals offered
    /// is one row to read, and a body later withheld whole becomes a row.
    fn withhold(&mut self, entity_id: EntityId, group: &str, withheld: &str) {
        let id = entity_id.to_string();
        if let Some(existing) = self
            .withheld_rows
            .iter_mut()
            .find(|row| row.entity_id == id)
        {
            if withheld == WITHHELD_ROW {
                existing.withheld = WITHHELD_ROW.to_string();
            }
            return;
        }
        self.withheld_rows.push(WithheldRow {
            entity_id: id,
            group: group.to_string(),
            withheld: withheld.to_string(),
        });
    }
}

fn default_method_style() -> String {
    "full".to_string()
}

/// The neighbourhood depth one focal gets, given how many share the pack.
///
/// One focal keeps the depth the single-focal pack always used. Two or more
/// shrink to direct neighbours, because the answer to a chain question lives
/// between the focals: paying two hops around each end spends the budget on the
/// ends and leaves the middle for whatever is left, which is the failure this
/// module exists to fix.
pub fn neighborhood_depth_for(focal_count: usize, base: u32) -> u32 {
    if focal_count <= 1 {
        base
    } else {
        1
    }
}

/// Split `capacity` across `demands` so a small claim never holds budget a
/// large one needed.
///
/// Every claimant gets an equal share. A claimant whose demand is under its
/// share takes exactly what it needs and hands the surplus back, and the
/// remaining claimants split the remainder again, until nobody fits their share
/// and the rest divide it evenly. Returns one allowance per demand, in order.
///
/// The alternative, an equal share for everyone, is what makes a short
/// neighbourhood waste budget: a focal needing 40 tokens holds 300 while the
/// focal that carried the answer is cut at 300.
pub fn water_fill(demands: &[usize], capacity: usize) -> Vec<usize> {
    let mut allowance = vec![0usize; demands.len()];
    let mut pending: Vec<usize> = (0..demands.len()).collect();
    let mut room = capacity;

    while !pending.is_empty() {
        let share = room / pending.len();
        let fits: Vec<usize> = pending
            .iter()
            .copied()
            .filter(|index| demands[*index] <= share)
            .collect();
        if fits.is_empty() {
            for index in pending {
                allowance[index] = share;
            }
            break;
        }
        for index in &fits {
            allowance[*index] = demands[*index];
            room -= demands[*index];
        }
        pending.retain(|index| !fits.contains(index));
    }

    allowance
}

/// How strongly a pack wants to keep one focal whole: a focal the caller named
/// or addressed by id ranks above every focal a ranking picked, and those rank
/// by their score.
pub fn focal_rank(resolution: &FocalResolution) -> f64 {
    match resolution.route.as_str() {
        "question" => resolution.score.map_or(f64::MIN, f64::from),
        _ => f64::INFINITY,
    }
}

/// Allowances for the focals of one pack: every focal its floor first, its
/// signature, then its whole demand in rank order while that fits.
///
/// [`water_fill`] shares a budget evenly, which is right for neighbourhoods and
/// wrong for focal bodies: a body is served whole or not at all, and an even
/// share of a budget too small for every body is a share too small for any.
/// Five focals of 470 tokens under a 1,500-token budget each got 300 and the
/// pack served no body. Ranked, the three a question ranked highest are served
/// whole and the other two as their signatures. When every demand fits, every
/// focal gets its demand, exactly as water filling gave it. `ranks` breaks ties
/// by position, earlier first.
pub fn ranked_fill(
    demands: &[usize],
    floors: &[usize],
    ranks: &[f64],
    capacity: usize,
) -> Vec<usize> {
    let floors: Vec<usize> = demands
        .iter()
        .zip(floors)
        .map(|(demand, floor)| (*floor).min(*demand))
        .collect();
    let floor_total: usize = floors.iter().sum();
    if floor_total >= capacity {
        return water_fill(&floors, capacity);
    }
    let mut allowance = floors;
    let mut room = capacity - floor_total;
    let mut order: Vec<usize> = (0..demands.len()).collect();
    order.sort_by(|left, right| {
        let rank = |index: &usize| ranks.get(*index).copied().unwrap_or(f64::INFINITY);
        rank(right).total_cmp(&rank(left)).then(left.cmp(right))
    });
    for index in order {
        let extra = demands[index] - allowance[index];
        if extra <= room {
            allowance[index] = demands[index];
            room -= extra;
        }
    }
    allowance
}

/// The outcome of one route search between two focals.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RouteOutcome {
    /// The entities strictly between the two focals, in walk order, and the
    /// edges the walk crossed, one per hop.
    Found(Vec<EntityId>, Vec<RouteEdgeReport>),
    /// The walk finished within its bounds and found nothing.
    None,
    /// A bound stopped the walk, so nothing was proved either way.
    Bounded,
}

/// Work allowance for one route search.
struct RouteMeter {
    remaining: usize,
}

impl RouteMeter {
    fn new(budget: usize) -> Self {
        Self { remaining: budget }
    }

    /// Charge one expansion. Returns false once the allowance is spent.
    fn charge(&mut self) -> bool {
        if self.remaining == 0 {
            return false;
        }
        self.remaining -= 1;
        true
    }
}

/// The entities on a dependency route between two focals, if the graph joins
/// them within `max_hops`.
///
/// Direction is deliberately ignored. "How does a character reach the document"
/// is answered by the chain whether the walk reads it as calls out of the
/// keyboard or callers into the document, and a route search that respected
/// direction would report two halves of one answer as no answer at all. The
/// edges are still restricted to real dependency edges by
/// [`is_dependency_edge`], so a co-change or containment coincidence cannot
/// manufacture a route.
fn route_between<G>(
    graph: &G,
    from: &EntityId,
    to: &EntityId,
    max_hops: usize,
    meter: &mut RouteMeter,
) -> Result<RouteOutcome>
where
    G: GraphStore,
{
    if from == to {
        return Ok(RouteOutcome::Found(Vec::new(), Vec::new()));
    }

    // The edge is recorded beside the parent, not derived afterwards. A second
    // pass over the graph to re-find "the" edge between two entities picks one
    // of however many join them, which need not be the one the walk actually
    // crossed, and the whole point of reporting the edges is that they are the
    // ones the walk crossed.
    let mut parent: HashMap<EntityId, (EntityId, RelationKind, bool)> = HashMap::new();
    let mut seen: HashSet<EntityId> = HashSet::new();
    seen.insert(*from);
    let mut frontier = vec![*from];

    for _hop in 0..max_hops {
        let mut next = Vec::new();
        for node in frontier.drain(..) {
            if !meter.charge() {
                return Ok(RouteOutcome::Bounded);
            }
            let relations = graph
                .get_all_relations_for_entity(&node)
                .map_err(|error| ContextError::Graph(error.to_string()))?;
            let node_ref = GraphNodeId::Entity(node);
            for relation in &relations {
                if !is_dependency_edge(&relation.kind) {
                    continue;
                }
                let (neighbor, followed) = if relation.src == node_ref {
                    (relation.dst.as_entity(), true)
                } else if relation.dst == node_ref {
                    (relation.src.as_entity(), false)
                } else {
                    (None, true)
                };
                let Some(neighbor) = neighbor else { continue };
                if neighbor == node {
                    continue;
                }
                if neighbor == *to {
                    parent.insert(neighbor, (node, relation.kind, followed));
                    let (via, edges) = walk_back(graph, &parent, from, to)?;
                    return Ok(RouteOutcome::Found(via, edges));
                }
                if seen.insert(neighbor) {
                    parent.insert(neighbor, (node, relation.kind, followed));
                    next.push(neighbor);
                }
            }
        }
        if next.is_empty() {
            return Ok(RouteOutcome::None);
        }
        frontier = next;
    }

    Ok(RouteOutcome::None)
}

/// The entities strictly between `from` and `to`, read out of a parent map.
fn walk_back<G>(
    graph: &G,
    parent: &HashMap<EntityId, (EntityId, RelationKind, bool)>,
    from: &EntityId,
    to: &EntityId,
) -> Result<(Vec<EntityId>, Vec<RouteEdgeReport>)>
where
    G: GraphStore,
{
    let name_of = |id: &EntityId| -> Result<String> {
        Ok(graph
            .get_entity(id)
            .map_err(|error| ContextError::Graph(error.to_string()))?
            .map(|entity| entity.name)
            .unwrap_or_default())
    };

    let mut chain = Vec::new();
    let mut edges = Vec::new();
    let mut cursor = *to;
    // Walked from `to` back to `from`, so both lists come out reversed and are
    // turned around together at the end. The edge recorded at `cursor` is the
    // one the walk crossed to REACH cursor, so it is rendered from its parent
    // to cursor, which is forward walk order.
    while let Some((previous, kind, followed)) = parent.get(&cursor) {
        edges.push(RouteEdgeReport {
            from_name: name_of(previous)?,
            to_name: name_of(&cursor)?,
            kind: format!("{kind:?}"),
            direction: if *followed {
                ROUTE_EDGE_FORWARD.to_string()
            } else {
                ROUTE_EDGE_REVERSE.to_string()
            },
        });
        if previous == from {
            break;
        }
        chain.push(*previous);
        cursor = *previous;
    }
    chain.reverse();
    edges.reverse();
    Ok((chain, edges))
}

/// One row waiting for a place in the pack.
struct Candidate {
    entity_id: EntityId,
    projection_level: ProjectionLevel,
    content: String,
    tokens: usize,
    /// Which section this row joins, by the names in [`crate::group`] plus
    /// [`ROUTE_GROUP`].
    group: &'static str,
    dependency_relation: Option<DependencyRelation>,
    /// Price of an exact neighbour body. Probing releases the body before the
    /// next candidate; only admitted bodies are read again and retained.
    body_price: Option<BodyPrice>,
}

#[derive(Clone, Copy)]
struct BodyPrice {
    tokens: usize,
    bytes: usize,
}

/// A row that reached the pack, in admission order, so the budget fit can drop
/// the lowest-priority row first.
struct Admitted {
    group: &'static str,
    entity_id: EntityId,
    tokens: usize,
    /// The focal whose neighbourhood offered it, or the route it came from.
    origin: Origin,
    /// The row's signature, kept when it was admitted with its body, so the
    /// fit can walk it down to its signature before dropping it.
    signature: Option<String>,
}

#[derive(Clone, Copy)]
enum Origin {
    Route(usize),
    Neighborhood(usize),
}

/// The semantic candidates offered by the request, before output admission.
/// Only ids and attribution survive here, not source bodies or projections.
struct CandidateCensus {
    focals: Vec<EntityId>,
    rows: Vec<CensusRow>,
}

struct CensusRow {
    entity_id: EntityId,
    group: &'static str,
    origin: Origin,
    dependency_relation: Option<DependencyRelation>,
}

/// Where a pack section's rows live, so an admitted row can be dropped again.
fn section_mut<'pack>(
    pack: &'pack mut ContextPack,
    group_name: &str,
) -> &'pack mut Vec<ContextEntry> {
    match group_name {
        group::DEPENDENCIES | group::DEPENDENTS | group::CONTEXTUAL_NEIGHBORS => {
            &mut pack.dependency_signatures
        }
        group::TESTS => &mut pack.tests,
        group::CONTRACTS => &mut pack.contracts,
        // Route rows and transitive dependencies share a section; see
        // [`ROUTE_GROUP`] for why, and [`RouteReport::via`] for how a reader
        // tells them apart without reading content.
        _ => &mut pack.transitive_deps,
    }
}

/// Build a context pack from several focal entities.
///
/// `focal_ids` is taken in the caller's order, which is the order the question
/// named them, and duplicates collapse to the first mention. The pack that
/// comes back carries every focal it could afford, the route material between
/// connected focals, and each focal's neighbourhood water-filled into what
/// remained, and it measures under `opts.budget`.
pub fn build_multi_focal_pack<G>(
    graph: &G,
    focal_ids: &[EntityId],
    opts: &MultiFocalOptions,
) -> Result<(ContextPack, MultiFocalReport)>
where
    G: GraphStore,
{
    build_multi_focal_pack_inner(
        graph,
        focal_ids,
        opts,
        None,
        ProjectionLimits::default(),
        None,
    )
    .map(|(pack, report, _)| (pack, report))
}

type MultiMeasure<'a> =
    dyn FnMut(&ContextPack, &MultiFocalReport, &ProjectionReport) -> Result<usize> + 'a;

pub fn build_multi_focal_pack_with_provider<G: GraphStore>(
    graph: &G,
    focal_ids: &[EntityId],
    opts: &MultiFocalOptions,
    provider: &mut dyn ContextProjectionProvider,
    limits: ProjectionLimits,
    mut measure: impl FnMut(&ContextPack, &MultiFocalReport, &ProjectionReport) -> Result<usize>,
) -> Result<(ContextPack, MultiFocalReport, ProjectionReport)> {
    build_multi_focal_pack_inner(
        graph,
        focal_ids,
        opts,
        Some(provider),
        limits,
        Some(&mut measure),
    )
}

fn build_multi_focal_pack_inner<G: GraphStore>(
    graph: &G,
    focal_ids: &[EntityId],
    opts: &MultiFocalOptions,
    mut provider: Option<&mut dyn ContextProjectionProvider>,
    limits: ProjectionLimits,
    measure: Option<&mut MultiMeasure<'_>>,
) -> Result<(ContextPack, MultiFocalReport, ProjectionReport)> {
    let mut projections = ProjectionReport::default();
    let source_aware = provider.is_some();
    let mut retained_bytes = 0usize;
    let mut ordered: Vec<EntityId> = Vec::new();
    for id in focal_ids {
        if !ordered.contains(id) {
            ordered.push(*id);
        }
    }
    if ordered.is_empty() {
        return Err(ContextError::Other(
            "a context pack needs at least one focal entity".to_string(),
        ));
    }

    let budget_max = opts.budget.max_tokens();
    let mut focals: Vec<Entity> = Vec::with_capacity(ordered.len());
    for id in &ordered {
        let entity = graph
            .get_entity(id)
            .map_err(|error| ContextError::Graph(error.to_string()))?
            .ok_or_else(|| ContextError::EntityNotFound(id.to_string()))?;
        focals.push(entity);
    }

    let depth = neighborhood_depth_for(focals.len(), opts.max_depth);
    let mut admitted: HashSet<EntityId> = focals.iter().map(|entity| entity.id).collect();
    let mut spent = 0usize;

    // 1. The focals themselves, each at the deepest projection its share
    //    affords. A focal that cannot fit even its name is reported rather than
    //    dropped quietly, because a pack silently missing one end of a chain is
    //    the exact failure this module was written for.
    let mut full_bodies = Vec::with_capacity(focals.len());
    let mut demands = Vec::with_capacity(focals.len());
    let mut exact_demands = Vec::with_capacity(focals.len());
    for entity in &focals {
        // A caller holding the bodies already asked for the neighbourhood
        // alone. The focal is its signature and nothing reads its source.
        if !opts.focal_bodies {
            demands.push(estimate_tokens(&project_signature_only(entity)));
            full_bodies.push(None);
            exact_demands.push(false);
            continue;
        }
        if let Err(reason) = kin_model::require_independent_source(entity) {
            projections.downgrades.insert(entity.id, reason);
            demands.push(estimate_tokens(&project_signature_only(entity)));
            full_bodies.push(None);
            exact_demands.push(false);
            continue;
        }
        if let Some(source) = provider.as_deref_mut() {
            // Probe one candidate at a time. Its allocation is released before
            // the next probe; only costs survive into water filling.
            let probe_limits = ProjectionLimits {
                max_retained_bytes: limits.max_candidate_bytes,
                ..limits
            };
            match source.full_body(entity, probe_limits)? {
                BodyCandidate::Exact { body } => {
                    if body.len() > limits.max_candidate_bytes {
                        return Err(ContextError::Other(
                            "source provider exceeded its probe byte allowance".into(),
                        ));
                    }
                    demands.push(estimate_tokens(&body));
                    exact_demands.push(true);
                }
                BodyCandidate::Unavailable { reason } => {
                    projections.downgrades.insert(entity.id, reason);
                    demands.push(estimate_tokens(&project_signature_only(entity)));
                    exact_demands.push(false);
                }
                BodyCandidate::OverLimit { body_bytes } => {
                    projections.budget_withheld.insert(entity.id);
                    projections.downgrades.insert(entity.id, format!("whole graph-owned span of {body_bytes} bytes exceeds its inline byte allowance"));
                    demands.push(estimate_tokens(&project_signature_only(entity)));
                    exact_demands.push(false);
                }
            }
            full_bodies.push(None);
        } else {
            let body = project_full_body(entity);
            demands.push(estimate_tokens(&body));
            full_bodies.push(Some(body));
            exact_demands.push(false);
        }
    }
    // Whole bodies go to the focals ranked highest first, every focal keeping
    // at least its signature while the budget holds them all.
    let floors: Vec<usize> = focals
        .iter()
        .map(|entity| estimate_tokens(&project_signature_only(entity)))
        .collect();
    let ranks: Vec<f64> = (0..focals.len())
        .map(|index| {
            opts.resolutions
                .get(index)
                .map_or(f64::INFINITY, focal_rank)
        })
        .collect();
    let allowances = ranked_fill(&demands, &floors, &ranks, budget_max);

    let mut focal_entries: Vec<ContextEntry> = Vec::new();
    let mut contributions: Vec<FocalContribution> = Vec::new();
    // The cheaper rungs of each focal's projection ladder, kept so the budget
    // fit can walk a focal down instead of dropping it. Built here because the
    // fit runs on the assembled pack and no longer holds the entities.
    let mut ladders: HashMap<EntityId, (String, String)> = HashMap::new();

    for (index, entity) in focals.iter().enumerate() {
        let room = allowances[index].min(budget_max.saturating_sub(spent));
        let signature = project_signature_only(entity);
        let name_and_kind = project_name_and_kind(entity);
        ladders.insert(entity.id, (signature.clone(), name_and_kind.clone()));
        let body = if let Some(source) = provider.as_deref_mut() {
            if exact_demands[index] && demands[index] <= room {
                let remaining = ProjectionLimits {
                    max_retained_bytes: limits.max_retained_bytes.saturating_sub(retained_bytes),
                    ..limits
                };
                source_projection(source, entity, remaining, room, &mut projections)?
            } else {
                if exact_demands[index] {
                    projections.budget_withheld.insert(entity.id);
                    projections
                        .downgrades
                        .insert(entity.id, BODY_OVER_ALLOWANCE.into());
                }
                None
            }
        } else {
            full_bodies[index].take()
        };
        let mut ladder = Vec::with_capacity(3);
        if let Some(body) = body {
            ladder.push((ProjectionLevel::FullBody, body));
        }
        ladder.push((ProjectionLevel::SignatureOnly, signature));
        ladder.push((ProjectionLevel::NameAndKind, name_and_kind));
        let chosen = ladder
            .into_iter()
            .find(|(_, content)| estimate_tokens(content) <= room);

        let resolution = opts
            .resolutions
            .get(index)
            .cloned()
            .unwrap_or_else(|| FocalResolution::by_name(entity.name.clone()));
        let (projection, focal_tokens) = match chosen {
            Some((level, content)) => {
                let cost = estimate_tokens(&content);
                if source_aware && level == ProjectionLevel::FullBody {
                    retained_bytes += content.len();
                }
                spent += cost;
                focal_entries.push(ContextEntry {
                    entity_id: entity.id,
                    projection_level: level,
                    content,
                });
                (
                    if source_aware && level == ProjectionLevel::FullBody {
                        SERVED_BODY_PROJECTION_NAME
                    } else {
                        projection_name(level)
                    }
                    .to_string(),
                    cost,
                )
            }
            None => {
                // A requested focal stays a focal even when its output is
                // elided. Re-offering it as a neighbour makes the candidate
                // census depend on the requested output budget.
                ("elided".to_string(), 0)
            }
        };

        contributions.push(FocalContribution {
            entity_id: entity.id.to_string(),
            name: entity.name.clone(),
            kind: format!("{:?}", entity.kind),
            file: entity.file_origin.as_ref().map(|origin| origin.to_string()),
            line: entity.span.as_ref().map(|span| span.start_line),
            projection,
            focal_tokens,
            neighborhood_rows: 0,
            neighborhood_tokens: 0,
            shared_rows: 0,
            withheld_rows: 0,
            resolution,
        });
    }

    // A pack with no focal is not an answer, and refusing here would refuse with
    // the wrong number: the cheapest body is not what the output costs. Keep the
    // highest-ranked focal at its cheapest projection even when the budget cannot hold
    // it, and let the fit below refuse with what the rendering actually costs.
    // One refusal, one number, and the number is a budget that would have worked.
    if focal_entries.is_empty() {
        let index = (0..focals.len())
            .max_by(|left, right| ranks[*left].total_cmp(&ranks[*right]).then(right.cmp(left)))
            .expect("at least one focal");
        let entity = &focals[index];
        let content = project_name_and_kind(entity);
        spent += estimate_tokens(&content);
        focal_entries.push(ContextEntry {
            entity_id: entity.id,
            projection_level: ProjectionLevel::NameAndKind,
            content,
        });
        admitted.insert(entity.id);
        contributions[index].projection = projection_name(ProjectionLevel::NameAndKind).to_string();
        contributions[index].focal_tokens = estimate_tokens(&project_name_and_kind(entity));
    }

    // 2. The route between connected focals, before either end's neighbourhood.
    let mut route_search = RouteSearch {
        max_hops: ROUTE_MAX_HOPS,
        ..RouteSearch::default()
    };
    let mut routes: Vec<RouteReport> = Vec::new();
    let mut route_candidates: Vec<(usize, Candidate, String)> = Vec::new();

    for left in 0..focals.len() {
        for right in (left + 1)..focals.len() {
            route_search.pairs_examined += 1;
            let mut meter = RouteMeter::new(ROUTE_VISIT_MAX);
            let outcome = route_between(
                graph,
                &focals[left].id,
                &focals[right].id,
                ROUTE_MAX_HOPS,
                &mut meter,
            )?;
            let (via, edges) = match outcome {
                RouteOutcome::Bounded => {
                    route_search.bounded = true;
                    continue;
                }
                RouteOutcome::None => continue,
                RouteOutcome::Found(via, edges) => (via, edges),
            };
            route_search.pairs_connected += 1;
            let route_index = routes.len();
            let edge_line = render_route_edges(&edges);
            let mut report = RouteReport {
                from: focals[left].id.to_string(),
                from_name: focals[left].name.clone(),
                to: focals[right].id.to_string(),
                to_name: focals[right].name.clone(),
                hops: via.len() + 1,
                via: Vec::new(),
                via_names: Vec::new(),
                tokens: 0,
                withheld: 0,
                edges,
            };
            for (step, id) in via.iter().enumerate() {
                if admitted.contains(id) {
                    report.withheld += 1;
                    continue;
                }
                let Some(entity) = graph
                    .get_entity(id)
                    .map_err(|error| ContextError::Graph(error.to_string()))?
                else {
                    report.withheld += 1;
                    continue;
                };
                // The edge line rides on every step of the route, because the
                // reader who needs it is reading one row, not the report. Left
                // off, the row says two functions are joined and leaves the
                // reader to assume the join is a call.
                let content = format!(
                    "{ROUTE_MARKER} {} to {}, step {} of {}, over {}\n{}",
                    focals[left].name,
                    focals[right].name,
                    step + 1,
                    via.len(),
                    edge_line,
                    project_signature_only(&entity)
                );
                let tokens = estimate_tokens(&content);
                admitted.insert(*id);
                route_candidates.push((
                    route_index,
                    Candidate {
                        entity_id: *id,
                        projection_level: ProjectionLevel::SignatureOnly,
                        content,
                        tokens,
                        group: ROUTE_GROUP,
                        dependency_relation: None,
                        body_price: None,
                    },
                    entity.name.clone(),
                ));
            }
            routes.push(report);
        }
    }

    // 3. Each focal's neighbourhood, offered whole so the water-filling sees
    //    real demands, then admitted under the share those demands earn.
    let mut neighborhood: Vec<Vec<Candidate>> = Vec::with_capacity(focals.len());
    for entity in &focals {
        let sub_opts = ContextOptions {
            // Selection is semantic discovery. The assembled reply below is
            // the one place the caller's output budget admits or withholds
            // these rows. Pre-trimming here loses the refused candidate ids
            // and makes a refusal's advertised minimum change on retry.
            budget: TokenBudget::Custom(usize::MAX),
            max_depth: depth,
            include_tests: opts.include_tests,
            include_contracts: opts.include_contracts,
            include_traffic: false,
            assistant_hint: opts.assistant_hint,
        };
        let (pack, selection) = build_context_pack_with_provenance(graph, &entity.id, &sub_opts)?;
        let mut rows: Vec<Candidate> = Vec::new();
        for entry in &pack.dependency_signatures {
            let relation = selection.relation_for(&entry.entity_id);
            let group_name = match relation {
                DependencyRelation::DependentEdge => group::DEPENDENTS,
                DependencyRelation::DependencyEdge => group::DEPENDENCIES,
                DependencyRelation::SameFileNeighbor => group::CONTEXTUAL_NEIGHBORS,
            };
            let mut candidate = candidate_from(entry, group_name);
            candidate.dependency_relation = Some(relation);
            rows.push(candidate);
        }
        for entry in &pack.transitive_deps {
            rows.push(candidate_from(entry, group::TRANSITIVE_DEPS));
        }
        for entry in &pack.tests {
            rows.push(candidate_from(entry, group::TESTS));
        }
        for entry in &pack.contracts {
            rows.push(candidate_from(entry, group::CONTRACTS));
        }
        neighborhood.push(rows);
    }

    // 3a. Neighbour bodies, when the caller asked for them. Each is priced
    //     before admission so the water-filling below sees what a body really
    //     costs. Probes hold one body at a time, even when many focals offer
    //     the same neighbour. Admission re-reads only bodies it can retain.
    if opts.neighbor_bodies {
        if let Some(source) = provider.as_deref_mut() {
            let mut probed: HashMap<EntityId, Option<BodyPrice>> = HashMap::new();
            for rows in &mut neighborhood {
                for row in rows.iter_mut() {
                    if !matches!(
                        row.group,
                        group::DEPENDENCIES | group::DEPENDENTS | group::CONTEXTUAL_NEIGHBORS
                    ) || admitted.contains(&row.entity_id)
                    {
                        continue;
                    }
                    if let Some(price) = probed.get(&row.entity_id) {
                        row.body_price = *price;
                        continue;
                    }
                    let Some(entity) = graph
                        .get_entity(&row.entity_id)
                        .map_err(|error| ContextError::Graph(error.to_string()))?
                    else {
                        probed.insert(row.entity_id, None);
                        continue;
                    };
                    let price = if let Err(reason) = kin_model::require_independent_source(&entity)
                    {
                        projections.downgrades.insert(entity.id, reason);
                        None
                    } else {
                        let probe_limits = ProjectionLimits {
                            max_retained_bytes: limits.max_candidate_bytes,
                            ..limits
                        };
                        match source.full_body(&entity, probe_limits)? {
                            BodyCandidate::Exact { body }
                                if body.len() <= limits.max_candidate_bytes =>
                            {
                                Some(BodyPrice {
                                    tokens: estimate_tokens(&body),
                                    bytes: body.len(),
                                })
                            }
                            BodyCandidate::Exact { body } => {
                                projections.budget_withheld.insert(entity.id);
                                projections.downgrades.insert(
                                    entity.id,
                                    format!(
                                        "whole graph-owned span of {} bytes exceeds its inline byte allowance",
                                        body.len()
                                    ),
                                );
                                None
                            }
                            BodyCandidate::Unavailable { reason } => {
                                projections.downgrades.insert(entity.id, reason);
                                None
                            }
                            BodyCandidate::OverLimit { body_bytes } => {
                                projections.budget_withheld.insert(entity.id);
                                projections.downgrades.insert(
                                    entity.id,
                                    format!(
                                        "whole graph-owned span of {body_bytes} bytes exceeds its inline byte allowance"
                                    ),
                                );
                                None
                            }
                        }
                    };
                    probed.insert(row.entity_id, price);
                    row.body_price = price;
                }
            }
        }
    }

    let census = CandidateCensus {
        focals: ordered,
        rows: route_candidates
            .iter()
            .map(|(index, row, _)| CensusRow {
                entity_id: row.entity_id,
                group: row.group,
                origin: Origin::Route(*index),
                dependency_relation: row.dependency_relation,
            })
            .chain(neighborhood.iter().enumerate().flat_map(|(index, rows)| {
                rows.iter().map(move |row| CensusRow {
                    entity_id: row.entity_id,
                    group: row.group,
                    origin: Origin::Neighborhood(index),
                    dependency_relation: row.dependency_relation,
                })
            }))
            .collect(),
    };
    let mut assembled = ContextPack {
        focal_entities: focal_entries,
        dependency_signatures: Vec::new(),
        transitive_deps: Vec::new(),
        contracts: Vec::new(),
        tests: Vec::new(),
        work_items: Vec::new(),
        annotations: Vec::new(),
        traffic: Vec::new(),
        supporting_artifacts: Vec::new(),
        token_budget: opts.budget,
        actual_tokens: 0,
    };

    // Route rows are admitted before any neighbourhood row, and dropped last.
    let mut order: Vec<Admitted> = Vec::new();
    for (route_index, candidate, name) in route_candidates {
        if spent + candidate.tokens > budget_max {
            routes[route_index].withheld += 1;
            continue;
        }
        spent += candidate.tokens;
        routes[route_index].tokens += candidate.tokens;
        routes[route_index]
            .via
            .push(candidate.entity_id.to_string());
        routes[route_index].via_names.push(name);
        order.push(Admitted {
            group: candidate.group,
            entity_id: candidate.entity_id,
            tokens: candidate.tokens,
            origin: Origin::Route(route_index),
            signature: None,
        });
        section_mut(&mut assembled, candidate.group).push(ContextEntry {
            entity_id: candidate.entity_id,
            projection_level: candidate.projection_level,
            content: candidate.content,
        });
    }

    // The neighbourhood share is what the focals and routes left. A row
    // offering its body demands the body, so a neighbourhood the caller asked
    // to read claims what reading it costs.
    let neighborhood_demands: Vec<usize> = neighborhood
        .iter()
        .map(|rows| {
            rows.iter()
                .filter(|row| !admitted.contains(&row.entity_id))
                .map(|row| row.body_price.map_or(row.tokens, |price| price.tokens))
                .sum()
        })
        .collect();
    let neighborhood_allowances =
        water_fill(&neighborhood_demands, budget_max.saturating_sub(spent));

    for (index, rows) in neighborhood.into_iter().enumerate() {
        let mut left = neighborhood_allowances[index];
        for row in rows {
            if admitted.contains(&row.entity_id) {
                contributions[index].shared_rows += 1;
                continue;
            }
            // The body first, when one was offered and its share holds it.
            // A body that does not fit leaves the row as its signature, and
            // the row names the body as withheld rather than missing.
            let mut row = row;
            let mut signature = None;
            if let Some(price) = row.body_price {
                let fits_bytes =
                    retained_bytes.saturating_add(price.bytes) <= limits.max_retained_bytes;
                if price.tokens <= left && spent + price.tokens <= budget_max && fits_bytes {
                    if let (Some(source), Some(entity)) = (
                        provider.as_deref_mut(),
                        graph
                            .get_entity(&row.entity_id)
                            .map_err(|error| ContextError::Graph(error.to_string()))?,
                    ) {
                        let remaining = ProjectionLimits {
                            max_retained_bytes: limits
                                .max_retained_bytes
                                .saturating_sub(retained_bytes),
                            ..limits
                        };
                        if let Some(body) = source_projection(
                            source,
                            &entity,
                            remaining,
                            left.min(budget_max.saturating_sub(spent)),
                            &mut projections,
                        )? {
                            retained_bytes += body.len();
                            row.tokens = estimate_tokens(&body);
                            signature = Some(std::mem::replace(&mut row.content, body));
                            row.projection_level = ProjectionLevel::FullBody;
                        }
                    }
                } else {
                    projections.budget_withheld.insert(row.entity_id);
                    projections
                        .downgrades
                        .insert(row.entity_id, BODY_OVER_ALLOWANCE.to_string());
                }
            }
            if row.tokens > left || spent + row.tokens > budget_max {
                contributions[index].withheld_rows += 1;
                continue;
            }
            left -= row.tokens;
            spent += row.tokens;
            admitted.insert(row.entity_id);
            contributions[index].neighborhood_rows += 1;
            contributions[index].neighborhood_tokens += row.tokens;
            order.push(Admitted {
                group: row.group,
                entity_id: row.entity_id,
                tokens: row.tokens,
                origin: Origin::Neighborhood(index),
                signature,
            });
            section_mut(&mut assembled, row.group).push(ContextEntry {
                entity_id: row.entity_id,
                projection_level: row.projection_level,
                content: row.content,
            });
        }
    }

    assembled.actual_tokens = spent;

    let mut report = MultiFocalReport {
        focals: contributions,
        routes,
        route_search,
        neighborhood_depth: depth,
        budget_tokens: budget_max,
        measured_tokens: 0,
        elisions: BTreeMap::new(),
        row_provenance: BTreeMap::new(),
        coverage: opts.coverage.clone(),
        method: String::new(),
        method_style: default_method_style(),
        withheld_rows: Vec::new(),
        withheld_rows_unlisted: 0,
    };

    // 4. Render, measure what the rendering costs, and shrink until the pack is
    //    under the budget it was given. The single-focal builder reports going
    //    over; this one does not go over. A caller that asked for 500 tokens
    //    and received 700 spent budget it did not have, on every call, which is
    //    what the demo measured and had to compensate for.
    fit_to_budget(
        &mut assembled,
        &mut report,
        &mut order,
        &ladders,
        budget_max,
        &census,
        (&mut projections, measure),
    )?;

    name_withheld_bodies(&mut report, &assembled, &projections, &order);

    Ok((assembled, report, projections))
}

/// Name, as withheld bodies, every row the pack carries as its signature
/// because its body did not fit, focals included, so the caller can read each
/// one by id.
///
/// Run on the finished pack, and on a copy of the report every time the fit
/// measures, because a caller that measures the serialized report pays for
/// these entries too. Measured without them, a pack that fitted came back over
/// its budget once they were added.
fn name_withheld_bodies(
    report: &mut MultiFocalReport,
    pack: &ContextPack,
    projections: &ProjectionReport,
    order: &[Admitted],
) {
    let mut body_withheld: Vec<(EntityId, &'static str)> = Vec::new();
    for entry in &pack.focal_entities {
        if entry.projection_level != ProjectionLevel::FullBody
            && projections.budget_withheld.contains(&entry.entity_id)
        {
            body_withheld.push((entry.entity_id, FOCAL_GROUP));
        }
    }
    let admitted_group: HashMap<EntityId, &'static str> = order
        .iter()
        .map(|entry| (entry.entity_id, entry.group))
        .collect();
    for section in [&pack.dependency_signatures, &pack.tests, &pack.contracts] {
        for entry in section {
            if entry.projection_level != ProjectionLevel::FullBody
                && projections.budget_withheld.contains(&entry.entity_id)
            {
                let group_name = admitted_group
                    .get(&entry.entity_id)
                    .copied()
                    .unwrap_or(group::DEPENDENCIES);
                body_withheld.push((entry.entity_id, group_name));
            }
        }
    }
    for (entity_id, group_name) in body_withheld {
        report.withhold(entity_id, group_name, WITHHELD_BODY);
    }
}

/// The report as the caller will receive it, for measuring mid-fit.
fn measured_view(
    report: &MultiFocalReport,
    pack: &ContextPack,
    projections: &ProjectionReport,
    order: &[Admitted],
) -> MultiFocalReport {
    let mut view = report.clone();
    name_withheld_bodies(&mut view, pack, projections, order);
    view
}

/// Derive counts and withheld ids from the offered census and the current
/// output. Admission history cannot answer this: a row refused by one focal
/// may be carried by another, and a shared focal may itself be dropped later.
fn reconcile_candidate_census(
    report: &mut MultiFocalReport,
    pack: &ContextPack,
    order: &[Admitted],
    census: &CandidateCensus,
) {
    let carried: HashSet<EntityId> = pack
        .focal_entities
        .iter()
        .chain(&pack.dependency_signatures)
        .chain(&pack.transitive_deps)
        .chain(&pack.tests)
        .chain(&pack.contracts)
        .map(|entry| entry.entity_id)
        .collect();
    let owners: HashMap<EntityId, &Admitted> =
        order.iter().map(|entry| (entry.entity_id, entry)).collect();
    // Refresh before every measurement. The census retains each focal's
    // selection even when deduplication admitted the row through another focal
    // or a route. A row's display section cannot stand in for these reasons.
    report.row_provenance = carried
        .iter()
        .map(|id| {
            let section = if census.focals.contains(id) {
                FOCAL_GROUP
            } else {
                owners.get(id).map_or("unavailable", |owner| owner.group)
            };
            (
                *id,
                ContextRowProvenance {
                    section: section.to_string(),
                    reasons: Vec::new(),
                },
            )
        })
        .collect();
    for row in &census.rows {
        let Origin::Neighborhood(index) = row.origin else {
            continue;
        };
        let Some(provenance) = report.row_provenance.get_mut(&row.entity_id) else {
            continue;
        };
        let reason = ContextRowReason {
            focal_entity_id: census.focals[index],
            group: row.group.to_string(),
            dependency_relation: row.dependency_relation,
        };
        if !provenance.reasons.contains(&reason) {
            provenance.reasons.push(reason);
        }
    }
    for focal in &mut report.focals {
        focal.neighborhood_rows = 0;
        focal.neighborhood_tokens = 0;
        focal.shared_rows = 0;
        focal.withheld_rows = 0;
    }
    for row in &census.rows {
        let Origin::Neighborhood(index) = row.origin else {
            continue;
        };
        let focal = &mut report.focals[index];
        if !carried.contains(&row.entity_id) {
            focal.withheld_rows += 1;
        } else if let Some(owner) = owners.get(&row.entity_id).filter(|owner| {
            matches!(owner.origin, Origin::Neighborhood(owner_index) if owner_index == index)
        }) {
            focal.neighborhood_rows += 1;
            focal.neighborhood_tokens += owner.tokens;
        } else {
            focal.shared_rows += 1;
        }
    }

    // Count each semantic row once globally, retaining the focal and route
    // ownership that takes priority over neighbourhood overlap.
    let mut unique: BTreeMap<EntityId, &'static str> =
        census.focals.iter().map(|id| (*id, FOCAL_GROUP)).collect();
    for row in &census.rows {
        unique.entry(row.entity_id).or_insert_with(|| {
            owners
                .get(&row.entity_id)
                .map_or(row.group, |owner| owner.group)
        });
    }
    let mut counts: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();
    report.elisions.clear();
    report.withheld_rows.clear();
    report.withheld_rows_unlisted = 0;
    for (id, group_name) in unique {
        let (kept, elided) = counts.entry(group_name).or_default();
        if carried.contains(&id) {
            *kept += 1;
        } else {
            *elided += 1;
            // The census already deduplicated ids. Avoid scanning the growing
            // recovery list once more for every absent neighbour.
            report.withheld_rows.push(WithheldRow {
                entity_id: id.to_string(),
                group: group_name.to_string(),
                withheld: WITHHELD_ROW.to_string(),
            });
        }
    }
    for (group_name, (kept, elided)) in counts {
        record_elision(&mut report.elisions, group_name, elided, kept);
    }
}

fn candidate_from(entry: &ContextEntry, group_name: &'static str) -> Candidate {
    Candidate {
        entity_id: entry.entity_id,
        projection_level: entry.projection_level,
        content: entry.content.clone(),
        tokens: estimate_tokens(&entry.content),
        group: group_name,
        dependency_relation: None,
        body_price: None,
    }
}

/// Render, measure, and drop the lowest-priority row until the whole output
/// fits the budget.
///
/// Dropping runs in reverse admission order, so a neighbourhood row goes before
/// a route row and a route row before a focal. The loop only ever shrinks the
/// output, so it terminates; when nothing is left to drop and the header alone
/// is still over, that is a budget too small to answer in and it is reported as
/// one rather than silently exceeded.
fn fit_to_budget(
    pack: &mut ContextPack,
    report: &mut MultiFocalReport,
    order: &mut Vec<Admitted>,
    ladders: &HashMap<EntityId, (String, String)>,
    budget_max: usize,
    census: &CandidateCensus,
    fitting: (&mut ProjectionReport, Option<&mut MultiMeasure<'_>>),
) -> Result<()> {
    let (projections, mut measure) = fitting;
    // Set once shedding a route description is found to lower nothing the
    // caller measures, so the loop stops offering them.
    let mut routes_unpriced = false;
    // The same, for cutting a focal's name and kind down to the name alone.
    let mut signatures_unpriced = false;
    loop {
        reconcile_candidate_census(report, pack, order, census);
        for entry in &pack.focal_entities {
            if entry.projection_level != ProjectionLevel::FullBody
                && projections.full_bodies.remove(&entry.entity_id)
            {
                projections
                    .downgrades
                    .insert(entry.entity_id, BODY_OVER_ALLOWANCE.into());
                projections.budget_withheld.insert(entry.entity_id);
            }
        }
        let mut measured = settle_measurement(pack, report);
        if let Some(callback) = measure.as_deref_mut() {
            let mut settled = None;
            for _ in 0..MEASURED_TOKEN_PASSES {
                let current = callback(
                    pack,
                    &measured_view(report, pack, projections, order),
                    projections,
                )?;
                if report.measured_tokens == current && pack.actual_tokens == current {
                    settled = Some(current);
                    break;
                }
                report.measured_tokens = current;
                pack.actual_tokens = current;
                report.method = method_line(report);
            }
            measured = settled.ok_or_else(|| {
                ContextError::Other("context response accounting did not converge".into())
            })?;
        }
        if measured <= budget_max {
            pack.actual_tokens = measured;
            return Ok(());
        }
        if measure.is_some()
            && order
                .last()
                .is_some_and(|entry| matches!(entry.origin, Origin::Route(_)))
            && shrink_one_focal(pack, report, ladders)
        {
            continue;
        }
        // A neighbour's body is an enrichment of its row, and the row is the
        // answer: walk the last body down to its signature before any row
        // goes, and name the body as withheld.
        if let Some(at) = order.iter().rposition(|entry| entry.signature.is_some()) {
            let entry = &mut order[at];
            let signature = entry.signature.take().expect("found by its signature");
            let section = section_mut(pack, entry.group);
            if let Some(row) = section
                .iter_mut()
                .find(|row| row.entity_id == entry.entity_id)
            {
                entry.tokens = estimate_tokens(&signature);
                row.content = signature;
                row.projection_level = ProjectionLevel::SignatureOnly;
            }
            projections.full_bodies.remove(&entry.entity_id);
            projections.budget_withheld.insert(entry.entity_id);
            projections
                .downgrades
                .insert(entry.entity_id, BODY_OVER_ALLOWANCE.to_string());
            continue;
        }
        let Some(dropped) = order.pop() else {
            // Nothing left to drop. The sentence describing the pack is now
            // most of the pack, so shorten it once and try again.
            if report.method_style == "full" {
                report.method_style = "compact".to_string();
                continue;
            }
            // Then the focals themselves shrink, deepest projection first and
            // last focal first, because a pack of five names beats a pack of
            // one body when the question named five things.
            if shrink_one_focal_from(pack, report, ladders, ProjectionLevel::FullBody) {
                continue;
            }
            // Every focal is down to its signature. A route description is
            // about the answer rather than part of it, so the descriptions go
            // before any focal loses its signature, lowest-ranked pair first.
            // Only when they cost the caller's measurement something: a
            // caller that bounds the code alone keeps every route.
            if !routes_unpriced && !report.routes.is_empty() {
                let mut without = report.clone();
                without.routes.pop();
                without.route_search.routes_withheld += 1;
                let cost = match measure.as_deref_mut() {
                    Some(callback) => callback(
                        pack,
                        &measured_view(&without, pack, projections, order),
                        projections,
                    )?,
                    None => settle_measurement(pack, &mut without),
                };
                if cost < measured {
                    *report = without;
                    continue;
                }
                routes_unpriced = true;
            }
            if shrink_one_focal_from(pack, report, ladders, ProjectionLevel::SignatureOnly) {
                continue;
            }
            // A name and kind still carries the signature, and a signature
            // can be most of a pack: fastapi's `Form` declares 6,000
            // characters of annotated parameters, and a question ranking it
            // first refused every budget under 5,700 tokens. The last rung
            // before a focal goes is its name and kind alone, taken only when
            // it lowers what the caller measures.
            if !signatures_unpriced {
                let mut pack_without = pack.clone();
                let mut without = report.clone();
                if strip_one_focal_signature(&mut pack_without, &mut without) {
                    reconcile_candidate_census(&mut without, &pack_without, order, census);
                    let cost = match measure.as_deref_mut() {
                        Some(callback) => callback(
                            &pack_without,
                            &measured_view(&without, &pack_without, projections, order),
                            projections,
                        )?,
                        None => settle_measurement(&pack_without, &mut without),
                    };
                    if cost < measured {
                        *pack = pack_without;
                        *report = without;
                        continue;
                    }
                    signatures_unpriced = true;
                }
            }
            // Then focals go, lowest-ranked first and the last among equals,
            // keeping the one the caller asked for first. A pack of two of five
            // focals is a narrower answer than the question, and the method
            // line says which ones it lost, which is worth more than refusing
            // outright. Below one focal there is no pack to have, and that is a
            // budget too small to answer in.
            if pack.focal_entities.len() > 1 {
                let rank_of = |id: &EntityId| {
                    let id = id.to_string();
                    report
                        .focals
                        .iter()
                        .find(|focal| focal.entity_id == id)
                        .map_or(f64::INFINITY, |focal| focal_rank(&focal.resolution))
                };
                let position = pack
                    .focal_entities
                    .iter()
                    .enumerate()
                    .min_by(|(left_index, left), (right_index, right)| {
                        rank_of(&left.entity_id)
                            .total_cmp(&rank_of(&right.entity_id))
                            .then(right_index.cmp(left_index))
                    })
                    .map(|(position, _)| position)
                    .expect("a list of more than one has a lowest-ranked element");
                let dropped = pack.focal_entities.remove(position);
                let id = dropped.entity_id.to_string();
                if let Some(contribution) =
                    report.focals.iter_mut().find(|focal| focal.entity_id == id)
                {
                    contribution.projection = "elided".to_string();
                    contribution.focal_tokens = 0;
                }
                continue;
            }
            return Err(ContextError::BudgetExceeded {
                actual: measured,
                budget: budget_max,
            });
        };
        let section = section_mut(pack, dropped.group);
        if let Some(position) = section
            .iter()
            .position(|entry| entry.entity_id == dropped.entity_id)
        {
            section.remove(position);
        }
        match dropped.origin {
            Origin::Neighborhood(index) => {
                if let Some(contribution) = report.focals.get_mut(index) {
                    contribution.neighborhood_rows =
                        contribution.neighborhood_rows.saturating_sub(1);
                    contribution.neighborhood_tokens = contribution
                        .neighborhood_tokens
                        .saturating_sub(dropped.tokens);
                    contribution.withheld_rows += 1;
                }
            }
            Origin::Route(index) => {
                if let Some(route) = report.routes.get_mut(index) {
                    let id = dropped.entity_id.to_string();
                    if let Some(position) = route.via.iter().position(|via| *via == id) {
                        route.via.remove(position);
                        if position < route.via_names.len() {
                            route.via_names.remove(position);
                        }
                    }
                    route.tokens = route.tokens.saturating_sub(dropped.tokens);
                    route.withheld += 1;
                }
            }
        }
    }
}

/// Walk one focal down its projection ladder, and report whether anything
/// moved.
///
/// Deepest projection first so the most expensive row is the one that shrinks,
/// and last focal first so the entity the caller named first is the last to
/// lose its body.
fn shrink_one_focal(
    pack: &mut ContextPack,
    report: &mut MultiFocalReport,
    ladders: &HashMap<EntityId, (String, String)>,
) -> bool {
    shrink_one_focal_from(pack, report, ladders, ProjectionLevel::FullBody)
        || shrink_one_focal_from(pack, report, ladders, ProjectionLevel::SignatureOnly)
}

/// Walk one focal at `level` down one rung, and report whether anything moved.
fn shrink_one_focal_from(
    pack: &mut ContextPack,
    report: &mut MultiFocalReport,
    ladders: &HashMap<EntityId, (String, String)>,
    level: ProjectionLevel,
) -> bool {
    // The lowest-ranked focal at the deepest projection goes down first, the
    // last one among equals, so the focals a question ranked highest keep
    // their bodies longest whatever order the ranking handed them over in.
    let rank_of = |id: &EntityId| {
        let id = id.to_string();
        report
            .focals
            .iter()
            .find(|focal| focal.entity_id == id)
            .map_or(f64::INFINITY, |focal| focal_rank(&focal.resolution))
    };
    let Some(position) = pack
        .focal_entities
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.projection_level == level)
        .min_by(|(left_index, left), (right_index, right)| {
            rank_of(&left.entity_id)
                .total_cmp(&rank_of(&right.entity_id))
                .then(right_index.cmp(left_index))
        })
        .map(|(position, _)| position)
    else {
        return false;
    };
    let entry = &mut pack.focal_entities[position];
    let Some((signature, name_and_kind)) = ladders.get(&entry.entity_id) else {
        return false;
    };
    let (next_level, next_content) = match level {
        ProjectionLevel::FullBody => (ProjectionLevel::SignatureOnly, signature),
        _ => (ProjectionLevel::NameAndKind, name_and_kind),
    };
    entry.projection_level = next_level;
    entry.content = next_content.clone();
    let id = entry.entity_id.to_string();
    let cost = estimate_tokens(next_content);
    if let Some(contribution) = report.focals.iter_mut().find(|focal| focal.entity_id == id) {
        contribution.projection = projection_name(next_level).to_string();
        contribution.focal_tokens = cost;
    }
    true
}

/// Cut one focal at its name and kind down to the name and kind alone, the
/// lowest-ranked first, and report whether anything moved.
fn strip_one_focal_signature(pack: &mut ContextPack, report: &mut MultiFocalReport) -> bool {
    let bare = |id: &EntityId| -> Option<String> {
        let id = id.to_string();
        report
            .focals
            .iter()
            .find(|focal| focal.entity_id == id)
            .map(|focal| format!("{} ({})\n", focal.name, focal.kind))
    };
    let Some((position, content)) = pack
        .focal_entities
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.projection_level == ProjectionLevel::NameAndKind)
        .filter_map(|(position, entry)| {
            bare(&entry.entity_id)
                .filter(|bare| bare.len() < entry.content.len())
                .map(|bare| (position, bare))
        })
        .min_by(|(left, _), (right, _)| {
            let rank =
                |position: &usize| focal_rank_in(report, &pack.focal_entities[*position].entity_id);
            rank(left).total_cmp(&rank(right)).then(right.cmp(left))
        })
    else {
        return false;
    };
    let entry = &mut pack.focal_entities[position];
    let cost = estimate_tokens(&content);
    entry.content = content;
    let id = entry.entity_id.to_string();
    if let Some(contribution) = report.focals.iter_mut().find(|focal| focal.entity_id == id) {
        contribution.focal_tokens = cost;
    }
    true
}

/// How strongly the pack wants to keep this focal, by [`focal_rank`].
fn focal_rank_in(report: &MultiFocalReport, id: &EntityId) -> f64 {
    let id = id.to_string();
    report
        .focals
        .iter()
        .find(|focal| focal.entity_id == id)
        .map_or(f64::INFINITY, |focal| focal_rank(&focal.resolution))
}

/// Settle the measured token count against the bytes carrying it, and return
/// what the rendering the report now describes actually costs.
///
/// See [`MEASURED_TOKEN_PASSES`] for why this converges. The returned number is
/// always the cost of the rendering produced by the report's final method line,
/// so the budget decision is made against the real output rather than against
/// an earlier draft of it.
fn settle_measurement(pack: &ContextPack, report: &mut MultiFocalReport) -> usize {
    let mut measured = report.measured_tokens;
    for _ in 0..MEASURED_TOKEN_PASSES {
        report.measured_tokens = measured;
        report.method = method_line(report);
        let rendered = estimate_tokens(&render_multi_focal_lines(pack, report).join("\n"));
        if rendered == measured {
            return measured;
        }
        measured = rendered;
    }
    report.measured_tokens = measured;
    report.method = method_line(report);
    estimate_tokens(&render_multi_focal_lines(pack, report).join("\n"))
}

/// Rows one group still carries.
fn kept_in_group(pack: &ContextPack, group_name: &str) -> usize {
    match group_name {
        FOCAL_GROUP => pack.focal_entities.len(),
        group::DEPENDENCIES | group::DEPENDENTS => pack.dependency_signatures.len(),
        group::TESTS => pack.tests.len(),
        group::CONTRACTS => pack.contracts.len(),
        ROUTE_GROUP => pack
            .transitive_deps
            .iter()
            .filter(|entry| entry.content.starts_with(ROUTE_MARKER))
            .count(),
        _ => pack
            .transitive_deps
            .iter()
            .filter(|entry| !entry.content.starts_with(ROUTE_MARKER))
            .count(),
    }
}

fn record_elision(
    elisions: &mut BTreeMap<String, PackElision>,
    group_name: &str,
    elided: usize,
    kept: usize,
) {
    if elided == 0 {
        return;
    }
    let entry = elisions
        .entry(group_name.to_string())
        .or_insert_with(|| PackElision {
            elided: 0,
            kept,
            total: kept,
            reason: ELISION_REASON_TOKEN_BUDGET.to_string(),
        });
    entry.elided += elided;
    entry.kept = kept;
    entry.total = kept + entry.elided;
}

/// What a focal row actually CARRIES, which is not always what its
/// [`ProjectionLevel`] is called.
///
/// `ProjectionLevel::FullBody` reports `header_and_signature`, because that is
/// what [`crate::builder::project_full_body`] renders and that function's own
/// doc comment says so: this crate has no source-reading capability, bodies
/// live in content-addressed blobs a layer above, and what it produces is a
/// header, a doc summary and a signature.
///
/// The old name was `full_body`. Measured on psf/requests at v0.7.2,
/// `get_context_pack` returned `Session.merge_environment_settings` and
/// `HTTPAdapter.build_connection_pool_key_attributes` labelled `full_body` and
/// carrying 85 and 652 tokens of header, for methods spanning 38 and 51 lines,
/// with nothing in the response saying the label and the content disagreed. The
/// level is unchanged, because the budget ladder is what it is for; only the
/// word this surface publishes about it changes, to one that is true.
fn projection_name(level: ProjectionLevel) -> &'static str {
    match level {
        ProjectionLevel::FullBody => FULL_BODY_PROJECTION_NAME,
        ProjectionLevel::SignatureOnly => "signature_only",
        ProjectionLevel::NameAndKind => "name_and_kind",
    }
}

/// One sentence naming every focal, how it was resolved, what it contributed,
/// how the pack was merged, what it measured, and what the ranking behind it
/// could see.
///
/// It is one line on purpose. A reader scanning a pack should be able to tell
/// at a glance whether the selection is trustworthy, and a reader who wants the
/// detail has the same facts in the JSON under the same names.
pub fn method_line(report: &MultiFocalReport) -> String {
    if report.method_style == "compact" {
        return compact_method_line(report);
    }
    let focals: Vec<String> = report
        .focals
        .iter()
        .map(|contribution| {
            let carried = if contribution.projection == "elided" {
                "did not fit".to_string()
            } else {
                format!(
                    "{} plus {} row{}",
                    contribution.projection,
                    contribution.neighborhood_rows,
                    if contribution.neighborhood_rows == 1 {
                        ""
                    } else {
                        "s"
                    }
                )
            };
            // Named by entity, never by where it is stored: the line reaches
            // agents, and a coordinate in it withheld the whole method from
            // them. The JSON beside it keeps each focal's id.
            format!(
                "{}, {}, {}, {}",
                contribution.name,
                contribution.kind,
                contribution.resolution.describe(),
                carried
            )
        })
        .collect();

    let route_text = if report.routes.iter().all(|route| route.via.is_empty()) {
        if report.route_search.bounded {
            format!(
                "no route material admitted, and a route search stopped at its {ROUTE_VISIT_MAX}-entity \
                 bound, so an absent route is not evidence there is none"
            )
        } else if report.route_search.pairs_connected > 0 {
            "the pairs the graph joins are joined by a direct edge, so there is no material between them"
                .to_string()
        } else {
            format!(
                "the graph joins no pair of them within {} hops",
                report.route_search.max_hops
            )
        }
    } else {
        let pairs: Vec<String> = report
            .routes
            .iter()
            .filter(|route| !route.via.is_empty())
            .map(|route| {
                // "in 2 hops through X" is what a reader had before, and it is
                // the sentence that made a pair of `References` edges pointing
                // into one node read as a chain. The edges are the difference
                // between a route and a coincidence, so they are in the
                // sentence rather than only in the JSON beside it.
                let over = render_route_edges(&route.edges);
                let over = if over.is_empty() {
                    String::new()
                } else {
                    format!(", over {over}")
                };
                format!(
                    "{} to {} in {} hop{} through {}{}",
                    route.from_name,
                    route.to_name,
                    route.hops,
                    if route.hops == 1 { "" } else { "s" },
                    route.via_names.join(", "),
                    over
                )
            })
            .collect();
        format!("the route between them first ({})", pairs.join("; "))
    };

    let coverage = match &report.coverage {
        Some(note) => format!("; semantic coverage: {note}"),
        None => String::new(),
    };

    format!(
        "Method: {} focal{} at depth {} ({}), {}, neighbourhoods merged by water-filling; \
         measured {} of {} tokens by kin's structure-aware estimator{}",
        report.focals.len(),
        if report.focals.len() == 1 { "" } else { "s" },
        report.neighborhood_depth,
        focals.join("; "),
        route_text,
        report.measured_tokens,
        report.budget_tokens,
        coverage
    )
}

/// The method line with everything but the load-bearing facts removed.
///
/// Which focals, how the pack was merged, and what it measured. What goes is
/// the per-focal detail (kind, file, resolution, contribution), which the JSON
/// still carries in full under `focals`: a reader on a budget this tight has
/// room for the answer or for the description of the answer, and the answer
/// wins.
fn compact_method_line(report: &MultiFocalReport) -> String {
    let names: Vec<&str> = report
        .focals
        .iter()
        .map(|contribution| contribution.name.as_str())
        .collect();
    let carried = report
        .focals
        .iter()
        .filter(|contribution| contribution.projection != "elided")
        .count();
    // Which focals did not fit is never "detail". It is the answer coming back
    // narrower than the question, and a compact line that hid it would be
    // exactly the silence the full line exists to prevent.
    let count = if carried == report.focals.len() {
        format!(
            "{} focal{}",
            report.focals.len(),
            if report.focals.len() == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "{carried} of {} focals carried, the rest did not fit",
            report.focals.len()
        )
    };
    format!(
        "Method: {count} ({}), route material first, neighbourhoods water-filled; measured {} of \
         {} tokens; per-focal detail withheld to fit the budget, and carried in the JSON",
        names.join(", "),
        report.measured_tokens,
        report.budget_tokens
    )
}

/// The human rendering of a multi-focal pack, header and body.
///
/// The whole output, because the budget is a claim about the whole output. The
/// header and the method line are tokens a reader pays for just as the entries
/// are, so counting only the entries would be the same understatement this
/// module exists to remove.
pub fn render_multi_focal_lines(pack: &ContextPack, report: &MultiFocalReport) -> Vec<String> {
    let route_rows = kept_in_group(pack, ROUTE_GROUP);
    let selected_rows = |section: &str| {
        report
            .row_provenance
            .values()
            .filter(|row| row.section == section)
            .count()
    };
    let mut lines = vec![
        format!(
            "Context pack for {} focal entit{}:",
            report.focals.len(),
            if report.focals.len() == 1 { "y" } else { "ies" }
        ),
        report.method.clone(),
        format!("  Focals: {} entries", pack.focal_entities.len()),
        format!(
            "  Route: {route_rows} entries across {} connected pair{}{}",
            report.route_search.pairs_connected,
            if report.route_search.pairs_connected == 1 {
                ""
            } else {
                "s"
            },
            match report.route_search.routes_withheld {
                0 => String::new(),
                1 => " (1 route description withheld by the token budget)".to_string(),
                withheld =>
                    format!(" ({withheld} route descriptions withheld by the token budget)"),
            }
        ),
        format!(
            "  Dependencies: {} entries",
            selected_rows(group::DEPENDENCIES)
        ),
        format!("  Dependents: {} entries", selected_rows(group::DEPENDENTS)),
        format!(
            "  Contextual neighbours (dependency unestablished): {} entries",
            selected_rows(group::CONTEXTUAL_NEIGHBORS)
        ),
        format!(
            "  Transitive: {} entries",
            kept_in_group(pack, group::TRANSITIVE_DEPS)
        ),
        format!("  Tests: {} entries", pack.tests.len()),
        format!("  Contracts: {} entries", pack.contracts.len()),
        format!(
            "  Budget: {}/{} tokens measured",
            report.measured_tokens, report.budget_tokens
        ),
    ];
    let unavailable = pack
        .dependency_signatures
        .iter()
        .filter(|entry| !report.row_provenance.contains_key(&entry.entity_id))
        .count();
    if unavailable > 0 {
        lines.push(format!(
            "  Selection provenance unavailable: {unavailable} entries"
        ));
    }

    let total_elided: usize = report.elisions.values().map(|elision| elision.elided).sum();
    if total_elided > 0 {
        lines.push(format!(
            "  Raise --budget above {} to recover the {total_elided} {} the token budget withheld.",
            report.budget_tokens,
            if total_elided == 1 {
                "entry"
            } else {
                "entries"
            }
        ));
    }

    lines.push(String::new());
    lines.push("--- Context Pack ---".to_string());
    lines.push("Selection reasons below do not enumerate all graph edges.".to_string());
    for entry in &pack.focal_entities {
        append_context_entry(&mut lines, entry, report);
    }
    for entry in &pack.transitive_deps {
        if entry.content.starts_with(ROUTE_MARKER) {
            append_context_entry(&mut lines, entry, report);
        }
    }
    for entry in &pack.dependency_signatures {
        append_context_entry(&mut lines, entry, report);
    }
    for entry in &pack.transitive_deps {
        if !entry.content.starts_with(ROUTE_MARKER) {
            append_context_entry(&mut lines, entry, report);
        }
    }
    for entry in &pack.tests {
        append_context_entry(&mut lines, entry, report);
    }
    for entry in &pack.contracts {
        append_context_entry(&mut lines, entry, report);
    }
    lines
}

fn append_context_entry(lines: &mut Vec<String>, entry: &ContextEntry, report: &MultiFocalReport) {
    if let Some(provenance) = report.row_provenance.get(&entry.entity_id) {
        for reason in &provenance.reasons {
            let focal = reason.focal_entity_id;
            let explanation = match reason.dependency_relation {
                Some(DependencyRelation::DependencyEdge) => {
                    format!("{focal} depends on this entity")
                }
                Some(DependencyRelation::DependentEdge) => {
                    format!("this entity depends on {focal}")
                }
                Some(DependencyRelation::SameFileNeighbor) => {
                    format!("contextual neighbour of {focal}; dependency unestablished")
                }
                None if reason.group == group::TRANSITIVE_DEPS => {
                    format!("graph context around {focal}; dependency chain unestablished")
                }
                None => format!("{} context around {focal}", reason.group),
            };
            lines.push(format!("// Context: {explanation}"));
        }
    } else {
        lines.push("// Context selection provenance unavailable".to_string());
    }
    lines.push(entry.content.clone());
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::relation::{Relation, RelationKind, RelationOrigin};
    use kin_model::*;

    fn make_entity(name: &str, file: &str) -> Entity {
        Entity {
            id: EntityId::new(),
            kind: EntityKind::Function,
            name: name.to_string(),
            language: LanguageId::Rust,
            fingerprint: SemanticFingerprint {
                algorithm: FingerprintAlgorithm::V1TreeSitter,
                ast_hash: Hash256::from_bytes([0; 32]),
                signature_hash: Hash256::from_bytes([0; 32]),
                behavior_hash: Hash256::from_bytes([0; 32]),
                equivalence_hash: Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(FilePathId::new(file)),
            span: None,
            signature: format!("fn {name}()"),
            visibility: Visibility::Public,
            role: EntityRole::Source,
            doc_summary: Some(format!("Does {name} things")),
            metadata: EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    fn calls(store: &kin_db::InMemoryGraph, src: &Entity, dst: &Entity) {
        store
            .upsert_relation(&Relation {
                id: kin_model::ids::RelationId::new(),
                kind: RelationKind::Calls,
                src: GraphNodeId::Entity(src.id),
                dst: GraphNodeId::Entity(dst.id),
                confidence: 1.0,
                origin: RelationOrigin::Parsed,
                created_in: None,
                import_source: None,
                evidence: Vec::new(),
            })
            .unwrap();
    }

    /// A `References` edge, which [`is_dependency_edge`] admits exactly like a
    /// call and which a route walk therefore crosses exactly like one. That it
    /// is NOT a call is the whole content of the finding these tests pin.
    fn references(store: &kin_db::InMemoryGraph, src: &Entity, dst: &Entity) {
        store
            .upsert_relation(&Relation {
                id: kin_model::ids::RelationId::new(),
                kind: RelationKind::References,
                src: GraphNodeId::Entity(src.id),
                dst: GraphNodeId::Entity(dst.id),
                confidence: 1.0,
                origin: RelationOrigin::Parsed,
                created_in: None,
                import_source: None,
                evidence: Vec::new(),
            })
            .unwrap();
    }

    /// A five-link chain, each entity in its own file so no same-file fallback
    /// can supply what only the route should.
    fn chain(len: usize) -> (kin_db::InMemoryGraph, Vec<Entity>) {
        chain_named(len, "link")
    }

    /// [`chain`] with each link named `<stem>_<index>`.
    fn chain_named(len: usize, stem: &str) -> (kin_db::InMemoryGraph, Vec<Entity>) {
        let store = kin_db::InMemoryGraph::new();
        let mut links = Vec::new();
        for index in 0..len {
            let entity = make_entity(
                &format!("{stem}_{index}"),
                &format!("src/{stem}_{index}.rs"),
            );
            store.upsert_entity(&entity).unwrap();
            links.push(entity);
        }
        for pair in links.windows(2) {
            calls(&store, &pair[0], &pair[1]);
        }
        (store, links)
    }

    /// The smallest budget this pack can be built in, taken from the refusal
    /// itself.
    ///
    /// The builder degrades until it cannot degrade further and then refuses
    /// with what the rendering costs at that point, so the number in the
    /// refusal is a budget that works. Reading it here rather than writing a
    /// constant is what keeps these tests honest when the header or the method
    /// line changes shape.
    fn floor_for(store: &kin_db::InMemoryGraph, ids: &[EntityId]) -> usize {
        match build_multi_focal_pack(store, ids, &opts(1)) {
            Err(ContextError::BudgetExceeded { actual, .. }) => actual,
            other => panic!("a one-token budget must refuse with its floor, got {other:?}"),
        }
    }

    fn opts(budget: usize) -> MultiFocalOptions {
        MultiFocalOptions {
            budget: TokenBudget::Custom(budget),
            ..MultiFocalOptions::default()
        }
    }

    // ---- water-filling -------------------------------------------------

    #[test]
    fn water_filling_hands_a_small_claim_surplus_back_to_a_large_one() {
        // Equal shares would be 50 each and the large claim would be cut at 50
        // while the small one held 40 it could not use.
        let allowances = water_fill(&[10, 190], 100);
        assert_eq!(
            allowances,
            vec![10, 90],
            "the small claim takes what it needs and the rest goes to the large one"
        );
    }

    #[test]
    fn water_filling_splits_evenly_when_nobody_fits_a_share() {
        assert_eq!(water_fill(&[500, 500, 500], 90), vec![30, 30, 30]);
    }

    #[test]
    fn water_filling_over_several_rounds_keeps_handing_surplus_up() {
        // Shares: 25 each. Only 5 fits, leaving 95 over three, share 31: 20 and
        // 30 fit, leaving 45 for the last one.
        assert_eq!(water_fill(&[5, 20, 30, 400], 100), vec![5, 20, 30, 45]);
    }

    #[test]
    fn water_filling_never_hands_out_more_than_capacity() {
        for capacity in [0usize, 1, 7, 99, 1000] {
            let allowances = water_fill(&[3, 60, 900, 1], capacity);
            let total: usize = allowances.iter().sum();
            assert!(
                total <= capacity,
                "handed out {total} of {capacity}: {allowances:?}"
            );
        }
    }

    // ---- several focals ------------------------------------------------

    #[test]
    fn every_focal_reaches_the_pack() {
        let (store, links) = chain(5);
        let focal_ids = vec![links[0].id, links[2].id, links[4].id];
        let (pack, report) = build_multi_focal_pack(&store, &focal_ids, &opts(8000)).unwrap();

        let in_pack: Vec<EntityId> = pack
            .focal_entities
            .iter()
            .map(|entry| entry.entity_id)
            .collect();
        assert_eq!(
            in_pack, focal_ids,
            "every focal is in the pack, in the order the caller named them"
        );
        assert_eq!(report.focals.len(), 3);
        assert!(
            report
                .focals
                .iter()
                .all(|focal| focal.projection == "header_and_signature"),
            "a generous budget carries every focal whole: {:?}",
            report
                .focals
                .iter()
                .map(|focal| focal.projection.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_repeated_focal_is_carried_once() {
        let (store, links) = chain(3);
        let (pack, report) =
            build_multi_focal_pack(&store, &[links[0].id, links[0].id], &opts(8000)).unwrap();
        assert_eq!(pack.focal_entities.len(), 1);
        assert_eq!(report.focals.len(), 1);
    }

    #[test]
    fn no_focal_is_a_refusal_rather_than_an_empty_pack() {
        let store = kin_db::InMemoryGraph::new();
        let error = build_multi_focal_pack(&store, &[], &opts(8000)).unwrap_err();
        assert!(
            error.to_string().contains("at least one focal"),
            "says what was missing: {error}"
        );
    }

    #[test]
    fn a_focal_id_the_graph_does_not_carry_is_named_in_the_refusal() {
        let (store, links) = chain(2);
        let missing = EntityId::new();
        let error =
            build_multi_focal_pack(&store, &[links[0].id, missing], &opts(8000)).unwrap_err();
        assert!(
            error.to_string().contains(&missing.to_string()),
            "names the id that did not resolve: {error}"
        );
    }

    // ---- the route between connected focals -----------------------------

    #[test]
    fn the_material_between_two_connected_focals_reaches_the_pack_first() {
        // link_0 -> link_1 -> link_2 -> link_3 -> link_4, focals at the ends.
        // link_2 is two hops from either focal, so no depth-1 neighbourhood
        // reaches it: if it is in the pack, the route put it there.
        let (store, links) = chain(5);
        // A neighbour of link_0 that is not on the route, so the neighbourhood
        // sections have something of their own to contribute. Without it every
        // neighbour IS route material, and the ordering assertion below would
        // have nothing to order against.
        let aside = make_entity("aside", "src/aside.rs");
        store.upsert_entity(&aside).unwrap();
        calls(&store, &links[0], &aside);

        let (pack, report) =
            build_multi_focal_pack(&store, &[links[0].id, links[4].id], &opts(8000)).unwrap();

        assert_eq!(report.route_search.pairs_connected, 1);
        assert_eq!(report.routes.len(), 1);
        let route = &report.routes[0];
        assert_eq!(
            route.via_names,
            vec![
                "link_1".to_string(),
                "link_2".to_string(),
                "link_3".to_string()
            ],
            "the route is carried in walk order from one focal to the other"
        );
        assert_eq!(route.hops, 4);

        let middle = pack
            .transitive_deps
            .iter()
            .find(|entry| entry.entity_id == links[2].id)
            .expect("the entity in the middle of the chain is in the pack");
        assert!(
            middle.content.starts_with(ROUTE_MARKER),
            "a route row says which pair it joins: {}",
            middle.content
        );

        let rendered = render_multi_focal_lines(&pack, &report).join("\n");
        let body = rendered
            .split_once("--- Context Pack ---")
            .expect("a rendered pack has a body")
            .1;
        let route_at = body.find(ROUTE_MARKER).expect("a route row in the body");
        // A row from a section that is not the route, matched on its own exact
        // content so the position cannot land inside a route row that happens
        // to quote the same signature.
        let neighbour = pack
            .dependency_signatures
            .first()
            .expect("the neighbourhoods contribute rows too");
        let neighbour_at = body
            .find(neighbour.content.trim())
            .expect("the neighbourhood row is rendered");
        assert!(
            route_at < neighbour_at,
            "route material is rendered before the neighbourhoods: route at {route_at}, \
             neighbour at {neighbour_at}"
        );
    }

    #[test]
    fn two_unconnected_focals_report_no_route_rather_than_inventing_one() {
        let store = kin_db::InMemoryGraph::new();
        let lonely = make_entity("lonely", "src/a.rs");
        let stranger = make_entity("stranger", "src/b.rs");
        store.upsert_entity(&lonely).unwrap();
        store.upsert_entity(&stranger).unwrap();

        let (pack, report) =
            build_multi_focal_pack(&store, &[lonely.id, stranger.id], &opts(8000)).unwrap();

        assert_eq!(report.route_search.pairs_examined, 1);
        assert_eq!(report.route_search.pairs_connected, 0);
        assert!(!report.route_search.bounded, "nothing stopped the search");
        assert!(report.routes.is_empty());
        assert!(
            pack.transitive_deps
                .iter()
                .all(|entry| !entry.content.starts_with(ROUTE_MARKER)),
            "no route rows without a route"
        );
        assert!(
            report.method.contains("joins no pair of them"),
            "the method line says the graph joins them nowhere: {}",
            report.method
        );
    }

    #[test]
    fn a_route_beyond_the_hop_bound_is_not_reported_as_connected() {
        // Longer than ROUTE_MAX_HOPS, so the walk ends without reaching the far
        // end. That is "not within the bound", never "unrelated".
        let (store, links) = chain(ROUTE_MAX_HOPS + 3);
        let last = links.len() - 1;
        let (_, report) =
            build_multi_focal_pack(&store, &[links[0].id, links[last].id], &opts(8000)).unwrap();
        assert_eq!(report.route_search.pairs_connected, 0);
        assert!(report.method.contains("within 4 hops"), "{}", report.method);
    }

    #[test]
    fn a_co_change_edge_cannot_manufacture_a_route() {
        let store = kin_db::InMemoryGraph::new();
        let left = make_entity("left", "src/a.rs");
        let right = make_entity("right", "src/b.rs");
        store.upsert_entity(&left).unwrap();
        store.upsert_entity(&right).unwrap();
        store
            .upsert_relation(&Relation {
                id: kin_model::ids::RelationId::new(),
                kind: RelationKind::CoChanges,
                src: GraphNodeId::Entity(left.id),
                dst: GraphNodeId::Entity(right.id),
                confidence: 1.0,
                origin: RelationOrigin::Inferred,
                created_in: None,
                import_source: None,
                evidence: Vec::new(),
            })
            .unwrap();

        let (_, report) =
            build_multi_focal_pack(&store, &[left.id, right.id], &opts(8000)).unwrap();
        assert_eq!(
            report.route_search.pairs_connected, 0,
            "co-change is a coincidence, not a route"
        );
    }

    #[test]
    fn a_route_is_found_whichever_way_the_edges_point() {
        // Both focals call into the middle, so no directed walk from either one
        // reaches the other. The chain still joins them and is still the answer.
        let store = kin_db::InMemoryGraph::new();
        let left = make_entity("left", "src/a.rs");
        let middle = make_entity("middle", "src/m.rs");
        let right = make_entity("right", "src/b.rs");
        for entity in [&left, &middle, &right] {
            store.upsert_entity(entity).unwrap();
        }
        calls(&store, &left, &middle);
        calls(&store, &right, &middle);

        let (_, report) =
            build_multi_focal_pack(&store, &[left.id, right.id], &opts(8000)).unwrap();
        assert_eq!(report.route_search.pairs_connected, 1);
        assert_eq!(report.routes[0].via_names, vec!["middle".to_string()]);
    }

    /// A route says what it is made OF, so a reader can tell a chain from a
    /// coincidence.
    ///
    /// The measured shape, on psf/requests at v0.7.2. `get_context_pack` joined
    /// `Session.merge_environment_settings` to
    /// `HTTPAdapter.build_connection_pool_key_attributes` "in 2 hops through
    /// `RequestEncodingMixin._encode_params`" and the two hops are both
    /// `References` edges pointing INTO that middle node, which is not a path
    /// from one focal to the other at all. `kin graph inspect` on the middle
    /// node reported 149 relations, almost all of them inbound references from
    /// unrelated code, so a two-hop walk through it joins nearly any pair in
    /// the repository. Every particular of that was invisible in the response.
    #[test]
    fn a_route_reports_the_edges_it_walked_and_which_way_they_point() {
        let store = kin_db::InMemoryGraph::new();
        let left = make_entity("left", "src/a.rs");
        let middle = make_entity("middle", "src/m.rs");
        let right = make_entity("right", "src/b.rs");
        for entity in [&left, &middle, &right] {
            store.upsert_entity(entity).unwrap();
        }
        // Both ends REFERENCE the middle. Neither reaches the other.
        references(&store, &left, &middle);
        references(&store, &right, &middle);

        let (pack, report) =
            build_multi_focal_pack(&store, &[left.id, right.id], &opts(8000)).unwrap();
        let route = &report.routes[0];

        assert_eq!(
            route.edges.len(),
            route.hops,
            "a route reports one edge per hop, got {:?}",
            route.edges
        );
        assert!(
            route.edges.iter().all(|edge| edge.kind == "References"),
            "the kinds are the graph's own, got {:?}",
            route.edges
        );
        assert_eq!(
            route.edges[0].direction, ROUTE_EDGE_FORWARD,
            "the walk followed left's own reference"
        );
        assert_eq!(
            route.edges[1].direction, ROUTE_EDGE_REVERSE,
            "and read right's backwards, which is the fact that makes this a \
             co-reference rather than a chain"
        );

        // The rendered line is where a reader actually sees it, and both arrows
        // point at the middle.
        let line = render_route_edges(&route.edges);
        assert_eq!(
            line, "left -References-> middle <-References- right",
            "got {line}"
        );

        // It reaches the surfaces a caller reads, not just the JSON.
        assert!(
            report.method.contains("<-References- right"),
            "the method sentence carries the edges: {}",
            report.method
        );
        let rendered = render_multi_focal_lines(&pack, &report).join("\n");
        assert!(
            rendered.contains("<-References- right"),
            "and so does the route row itself:\n{rendered}"
        );
    }

    /// A real chain reads as a chain: every edge forward, nothing to warn about.
    ///
    /// The positive control for the test above. Without it, that one could pass
    /// on a build that labelled every edge `reverse`.
    #[test]
    fn a_directed_chain_reports_every_edge_forward() {
        let (store, links) = chain(3);
        let (_, report) =
            build_multi_focal_pack(&store, &[links[0].id, links[2].id], &opts(8000)).unwrap();
        let route = &report.routes[0];
        assert!(
            route
                .edges
                .iter()
                .all(|edge| edge.direction == ROUTE_EDGE_FORWARD && edge.kind == "Calls"),
            "got {:?}",
            route.edges
        );
        assert_eq!(
            render_route_edges(&route.edges),
            "link_0 -Calls-> link_1 -Calls-> link_2"
        );
    }

    // ---- the budget bound ------------------------------------------------

    #[test]
    fn the_pack_measures_under_the_budget_it_was_given() {
        let (store, links) = chain(6);
        let focal_ids: Vec<EntityId> = links.iter().map(|link| link.id).collect();
        let floor = floor_for(&store, &focal_ids);
        for budget in [floor, floor + 40, 350, 500, 1500] {
            let (pack, report) = build_multi_focal_pack(&store, &focal_ids, &opts(budget)).unwrap();
            let rendered = render_multi_focal_lines(&pack, &report).join("\n");
            let measured = estimate_tokens(&rendered);
            assert!(
                measured <= budget,
                "at {budget} tokens the rendering cost {measured}:\n{rendered}"
            );
            assert_eq!(
                report.measured_tokens, measured,
                "the reported size is the size of the bytes returned"
            );
            assert_eq!(report.budget_tokens, budget);
        }
    }

    /// The sentence describing the pack is the last thing to shrink, and it
    /// shrinks visibly rather than the pack quietly going over budget.
    #[test]
    fn a_budget_too_tight_for_the_method_line_shortens_it_rather_than_overspending() {
        let (store, links) = chain(6);
        let focal_ids: Vec<EntityId> = links.iter().map(|link| link.id).collect();
        let full = {
            let (_, report) = build_multi_focal_pack(&store, &focal_ids, &opts(8000)).unwrap();
            report
        };
        assert_eq!(
            full.method_style, "full",
            "a generous budget keeps the detail"
        );
        let tight_budget = floor_for(&store, &focal_ids);

        let (pack, report) = build_multi_focal_pack(&store, &focal_ids, &opts(tight_budget))
            .expect("a pack that fits by shortening its own description");
        assert_eq!(report.method_style, "compact");
        assert!(
            report.method.contains("detail withheld"),
            "the shortening says so: {}",
            report.method
        );
        let measured = estimate_tokens(&render_multi_focal_lines(&pack, &report).join("\n"));
        assert!(
            measured <= tight_budget,
            "measured {measured} of {tight_budget}"
        );
    }

    /// The number the refusal names is a budget that works. A refusal reporting
    /// a floor nobody can build at sends the caller round the loop again, which
    /// is the same as not reporting one.
    #[test]
    fn the_floor_a_refusal_names_is_a_budget_that_builds() {
        let (store, links) = chain(6);
        let focal_ids: Vec<EntityId> = links.iter().map(|link| link.id).collect();
        let floor = floor_for(&store, &focal_ids);
        let (pack, report) = build_multi_focal_pack(&store, &focal_ids, &opts(floor))
            .expect("the floor the refusal named must build");
        let measured = estimate_tokens(&render_multi_focal_lines(&pack, &report).join("\n"));
        assert!(
            measured <= floor,
            "measured {measured} at its own floor {floor}"
        );
        assert!(
            !pack.focal_entities.is_empty(),
            "a pack at its floor still carries a focal"
        );
    }

    #[test]
    fn a_budget_too_small_for_one_focal_is_a_refusal_rather_than_an_overspend() {
        let (store, links) = chain(3);
        let error = build_multi_focal_pack(&store, &[links[0].id], &opts(2)).unwrap_err();
        assert!(
            matches!(error, ContextError::BudgetExceeded { .. }),
            "refuses rather than returning a pack over its budget: {error}"
        );
    }

    #[test]
    fn a_focal_the_budget_cannot_carry_is_reported_rather_than_dropped() {
        // Qualified names, each segment a token, so six names cost more than
        // saying five did not fit. With one-word names every focal fits at its
        // name and kind alone wherever one does, and no budget leaves one out.
        let (store, links) = chain_named(6, "pkg.sub.module.Klass.Inner.method");
        let focal_ids: Vec<EntityId> = links.iter().map(|link| link.id).collect();
        let floor = floor_for(&store, &focal_ids);
        let (pack, report) = build_multi_focal_pack(&store, &focal_ids, &opts(floor)).unwrap();
        assert!(
            pack.focal_entities.len() < focal_ids.len(),
            "this budget cannot carry six focals, which is the case under test"
        );
        let elided = report
            .focals
            .iter()
            .filter(|focal| focal.projection == "elided")
            .count();
        assert!(elided > 0);
        assert_eq!(
            report.elisions.get(FOCAL_GROUP).map(|e| e.elided),
            Some(elided),
            "the focals that did not fit are named in the elision map"
        );
        assert_eq!(
            report.elisions[FOCAL_GROUP].reason,
            ELISION_REASON_TOKEN_BUDGET
        );
        assert!(
            report.method.contains("did not fit"),
            "and the method line says so, in whichever form it is rendering: {}",
            report.method
        );
        assert!(
            report.method.contains(&format!(
                "{} of {} focals carried",
                pack.focal_entities.len(),
                report.focals.len()
            )),
            "with the count a reader can act on: {}",
            report.method
        );
    }

    /// A focal too expensive to carry whole is carried smaller, not dropped.
    ///
    /// The fixture puts the weight in the doc summary, which is what separates
    /// the three projections: the full body and the signature form both carry
    /// it, and the name-and-kind form does not.
    #[test]
    fn a_tight_budget_degrades_a_focal_rather_than_losing_it() {
        let store = kin_db::InMemoryGraph::new();
        let mut wordy = make_entity("wordy", "src/wordy.rs");
        wordy.doc_summary = Some("a documented behaviour ".repeat(60));
        store.upsert_entity(&wordy).unwrap();

        let full = estimate_tokens(&project_full_body(&wordy));
        let name_only = estimate_tokens(&project_name_and_kind(&wordy));
        assert!(
            name_only * 4 < full,
            "the fixture needs a ladder worth walking down: {name_only} against {full}"
        );

        let floor = floor_for(&store, &[wordy.id]);
        let (pack, report) = build_multi_focal_pack(&store, &[wordy.id], &opts(floor + 10))
            .expect("a budget above the floor builds");
        assert_eq!(
            report.focals[0].projection, "name_and_kind",
            "the cheapest projection that fits is the one taken"
        );
        assert_eq!(pack.focal_entities.len(), 1, "and the focal is still there");
        let measured = estimate_tokens(&render_multi_focal_lines(&pack, &report).join("\n"));
        assert!(measured <= floor + 10, "measured {measured}");
    }

    /// The fit loop walks a focal down its ladder before it drops one.
    ///
    /// Reaching that rung at all takes a specific budget, and finding it was
    /// the point. Two earlier attempts could not: the water-filled first pass
    /// already admits a focal at its cheapest projection whenever the budget is
    /// tight enough to matter, so at the floor, or with one focal, the fit
    /// loop's rung is dead code the mutation cannot touch. It fires only when
    /// the bodies DO fit their shares and the pack's own header and method line
    /// are what overflow. So the budget here is the two bodies plus a little,
    /// which admits both whole and leaves no room for the frame around them.
    #[test]
    fn the_fit_shrinks_a_focal_before_it_drops_one() {
        let store = kin_db::InMemoryGraph::new();
        let mut left = make_entity("left", "src/left.rs");
        left.doc_summary = Some("the left behaviour described at length ".repeat(12));
        let mut right = make_entity("right", "src/right.rs");
        right.doc_summary = Some("the right behaviour described at length ".repeat(12));
        store.upsert_entity(&left).unwrap();
        store.upsert_entity(&right).unwrap();
        calls(&store, &left, &right);

        let ids = vec![left.id, right.id];
        let bodies = estimate_tokens(&project_full_body(&left))
            + estimate_tokens(&project_full_body(&right));
        let budget = bodies + 20;

        let generous = build_multi_focal_pack(&store, &ids, &opts(8000)).unwrap().1;
        assert!(
            generous
                .focals
                .iter()
                .all(|focal| focal.projection == "header_and_signature"),
            "control: both focals are admitted whole when there is room"
        );

        let (pack, report) =
            build_multi_focal_pack(&store, &ids, &opts(budget)).expect("this budget builds");
        assert_eq!(
            pack.focal_entities.len(),
            2,
            "both focals survive, because shrinking comes before dropping: {}",
            report.method
        );
        assert!(
            report
                .focals
                .iter()
                .any(|focal| focal.projection != "header_and_signature"),
            "and at least one was walked down to make the frame fit: {:?}",
            report
                .focals
                .iter()
                .map(|focal| focal.projection.clone())
                .collect::<Vec<_>>()
        );
        let measured = estimate_tokens(&render_multi_focal_lines(&pack, &report).join("\n"));
        assert!(measured <= budget, "measured {measured} of {budget}");
    }

    // ---- the method line --------------------------------------------------

    #[test]
    fn the_method_line_names_every_focal_and_how_it_was_resolved() {
        let (store, links) = chain(3);
        let mut options = opts(8000);
        options.resolutions = vec![
            FocalResolution::by_name("link_0"),
            FocalResolution::from_question(0.8125),
        ];
        options.coverage = Some("3 of 3 entities embedded".to_string());

        let (_, report) =
            build_multi_focal_pack(&store, &[links[0].id, links[2].id], &options).unwrap();

        let method = &report.method;
        assert!(method.starts_with("Method: 2 focals"), "{method}");
        assert!(
            method.contains("link_0, Function, ") && !method.contains("src/link_0.rs"),
            "{method}"
        );
        assert!(method.contains("named as link_0"), "{method}");
        assert!(
            method.contains("located from the question, score 0.81"),
            "the question route carries its own score: {method}"
        );
        assert!(method.contains("water-filling"), "{method}");
        assert!(
            method.contains("semantic coverage: 3 of 3 entities embedded"),
            "the coverage the ranking had is part of the method: {method}"
        );
        assert!(
            method.contains(&format!("of {} tokens", report.budget_tokens)),
            "{method}"
        );
        assert!(!method.contains('\n'), "the method is one line: {method}");
    }

    #[test]
    fn the_rendered_pack_carries_the_method_line_and_the_measured_budget() {
        let (store, links) = chain(3);
        let (pack, report) =
            build_multi_focal_pack(&store, &[links[0].id, links[2].id], &opts(8000)).unwrap();
        let lines = render_multi_focal_lines(&pack, &report);
        assert!(lines[0].contains("2 focal entities"), "{:?}", lines[0]);
        assert_eq!(lines[1], report.method);
        assert!(
            lines.iter().any(|line| line
                == &format!(
                    "  Budget: {}/{} tokens measured",
                    report.measured_tokens, report.budget_tokens
                )),
            "the header states the measured cost: {lines:?}"
        );
    }

    // ---- neighbourhood shaping -------------------------------------------

    #[test]
    fn one_focal_keeps_the_single_focal_depth_and_several_shrink() {
        assert_eq!(neighborhood_depth_for(1, 2), 2);
        assert_eq!(neighborhood_depth_for(2, 2), 1);
        assert_eq!(neighborhood_depth_for(5, 3), 1);
    }

    /// A provider serving a fixed body per entity, and none for the rest.
    struct FixedBodies(HashMap<EntityId, String>);

    impl ContextProjectionProvider for FixedBodies {
        fn full_body(
            &mut self,
            entity: &Entity,
            _limits: ProjectionLimits,
        ) -> crate::error::Result<BodyCandidate> {
            Ok(match self.0.get(&entity.id) {
                Some(body) => BodyCandidate::Exact { body: body.clone() },
                None => BodyCandidate::Unavailable {
                    reason: "no body in this fixture".into(),
                },
            })
        }
    }

    fn content_tokens(pack: &ContextPack) -> usize {
        pack.focal_entities
            .iter()
            .chain(&pack.dependency_signatures)
            .chain(&pack.transitive_deps)
            .chain(&pack.tests)
            .chain(&pack.contracts)
            .map(|entry| estimate_tokens(&entry.content))
            .sum()
    }

    fn build_with_bodies(
        store: &kin_db::InMemoryGraph,
        ids: &[EntityId],
        opts: &MultiFocalOptions,
        bodies: HashMap<EntityId, String>,
    ) -> (ContextPack, MultiFocalReport, ProjectionReport) {
        let mut provider = FixedBodies(bodies);
        build_multi_focal_pack_with_provider(
            store,
            ids,
            opts,
            &mut provider,
            ProjectionLimits::default(),
            |pack, _, _| Ok(content_tokens(pack)),
        )
        .unwrap()
    }

    #[test]
    fn focal_bodies_off_serves_every_focal_as_its_signature() {
        let (store, links) = chain(3);
        let bodies: HashMap<EntityId, String> = links
            .iter()
            .map(|link| (link.id, format!("fn {}() {{ work(); }}", link.name)))
            .collect();
        let ids = [links[0].id, links[2].id];
        let with = build_with_bodies(&store, &ids, &opts(8000), bodies.clone());
        assert!(with
            .0
            .focal_entities
            .iter()
            .all(|entry| entry.projection_level == ProjectionLevel::FullBody));

        let without = MultiFocalOptions {
            focal_bodies: false,
            ..opts(8000)
        };
        let (pack, _, projections) = build_with_bodies(&store, &ids, &without, bodies);
        assert!(pack
            .focal_entities
            .iter()
            .all(|entry| entry.projection_level == ProjectionLevel::SignatureOnly));
        assert!(
            projections.budget_withheld.is_empty(),
            "a body nobody asked for is not a withheld body: {projections:?}"
        );
    }

    #[test]
    fn neighbour_bodies_are_served_when_asked_and_named_when_they_do_not_fit() {
        let (store, links) = chain(3);
        let body = |name: &str| format!("fn {name}() {{ {} }}", "step(); ".repeat(40));
        let bodies: HashMap<EntityId, String> = links
            .iter()
            .map(|link| (link.id, body(&link.name)))
            .collect();
        let focal = [links[0].id];
        let asked = MultiFocalOptions {
            neighbor_bodies: true,
            ..opts(8000)
        };
        let (pack, report, projections) = build_with_bodies(&store, &focal, &asked, bodies.clone());
        let neighbour = pack
            .dependency_signatures
            .iter()
            .find(|entry| entry.entity_id == links[1].id)
            .expect("the direct neighbour is in the pack");
        assert_eq!(neighbour.projection_level, ProjectionLevel::FullBody);
        assert_eq!(neighbour.content, body(&links[1].name));
        assert!(projections.full_bodies.contains(&links[1].id));
        assert!(
            report.withheld_rows.is_empty(),
            "{:?}",
            report.withheld_rows
        );

        // Room for the focal's body and a signature, not a second body.
        let focal_tokens = estimate_tokens(&body(&links[0].name));
        let tight = MultiFocalOptions {
            neighbor_bodies: true,
            ..opts(focal_tokens + 20)
        };
        let (pack, report, projections) = build_with_bodies(&store, &focal, &tight, bodies);
        assert_eq!(
            pack.focal_entities[0].projection_level,
            ProjectionLevel::FullBody,
            "the focal's body comes first"
        );
        let neighbour = pack
            .dependency_signatures
            .iter()
            .find(|entry| entry.entity_id == links[1].id)
            .expect("the neighbour stays as its signature");
        assert_eq!(neighbour.projection_level, ProjectionLevel::SignatureOnly);
        assert!(projections.budget_withheld.contains(&links[1].id));
        assert!(
            report.withheld_rows.iter().any(
                |row| row.entity_id == links[1].id.to_string() && row.withheld == WITHHELD_BODY
            ),
            "the withheld body is named by id: {:?}",
            report.withheld_rows
        );
    }

    #[test]
    fn shared_neighbor_bodies_are_priced_once_and_read_under_the_retention_limit() {
        struct ScopedBody {
            entity: EntityId,
            body: String,
            reads: Vec<ProjectionLimits>,
        }
        impl ContextProjectionProvider for ScopedBody {
            fn full_body(
                &mut self,
                entity: &Entity,
                limits: ProjectionLimits,
            ) -> crate::Result<BodyCandidate> {
                assert_eq!(
                    entity.id, self.entity,
                    "focals and routes need no neighbour probe"
                );
                self.reads.push(limits);
                Ok(
                    if self.body.len() > limits.max_candidate_bytes.min(limits.max_retained_bytes) {
                        BodyCandidate::OverLimit {
                            body_bytes: self.body.len(),
                        }
                    } else {
                        BodyCandidate::Exact {
                            body: self.body.clone(),
                        }
                    },
                )
            }
        }
        let (store, links) = chain(3);
        // The direct focal route leaves link_1 as a shared neighbour rather
        // than reserving it as route material before neighbour admission.
        calls(&store, &links[0], &links[2]);
        let body = format!("fn neighbor() {{ {} }}", "work(); ".repeat(8));
        for retain_body in [true, false] {
            let limit = if retain_body {
                body.len() + 1
            } else {
                body.len() - 1
            };
            let mut provider = ScopedBody {
                entity: links[1].id,
                body: body.clone(),
                reads: Vec::new(),
            };
            let options = MultiFocalOptions {
                focal_bodies: false,
                neighbor_bodies: true,
                ..opts(8000)
            };
            let (pack, report, projections) = build_multi_focal_pack_with_provider(
                &store,
                &[links[0].id, links[2].id],
                &options,
                &mut provider,
                ProjectionLimits {
                    max_candidate_bytes: 256,
                    max_retained_bytes: limit,
                },
                |pack, _, _| Ok(content_tokens(pack)),
            )
            .unwrap();
            assert_eq!(
                provider.reads[0].max_retained_bytes, 256,
                "one temporary probe"
            );
            assert_eq!(provider.reads.len(), if retain_body { 2 } else { 1 });
            if retain_body {
                assert_eq!(provider.reads[1].max_retained_bytes, limit);
                assert!(projections.full_bodies.contains(&links[1].id));
            } else {
                assert!(projections.budget_withheld.contains(&links[1].id));
            }
            let row = pack
                .dependency_signatures
                .iter()
                .find(|row| row.entity_id == links[1].id)
                .unwrap();
            assert_eq!(
                row.projection_level == ProjectionLevel::FullBody,
                retain_body
            );
            assert_eq!(
                report
                    .focals
                    .iter()
                    .map(|focal| focal.neighborhood_rows)
                    .sum::<usize>(),
                1
            );
            assert!(report.focals.iter().any(|focal| focal.shared_rows > 0));
        }
    }

    #[test]
    fn rows_the_budget_refuses_are_named_by_id() {
        let (store, links) = chain(5);
        let focal = [links[2].id];
        let bodies: HashMap<EntityId, String> = links
            .iter()
            .map(|link| (link.id, format!("fn {}() {{ work(); }}", link.name)))
            .collect();
        let generous = build_with_bodies(&store, &focal, &opts(8000), bodies.clone());
        let carried = generous.0.dependency_signatures.len() + generous.0.transitive_deps.len();
        assert!(carried >= 2, "the fixture needs neighbours to refuse");
        let focal_tokens = estimate_tokens(&generous.0.focal_entities[0].content);
        let first_row = generous
            .0
            .dependency_signatures
            .first()
            .map_or(0, |entry| estimate_tokens(&entry.content));
        let (pack, report, _) =
            build_with_bodies(&store, &focal, &opts(focal_tokens + first_row), bodies);
        let kept = pack.dependency_signatures.len() + pack.transitive_deps.len();
        assert!(kept < carried, "the budget refused something");
        let in_pack: HashSet<String> = pack
            .dependency_signatures
            .iter()
            .chain(&pack.transitive_deps)
            .map(|entry| entry.entity_id.to_string())
            .collect();
        let named: Vec<&WithheldRow> = report
            .withheld_rows
            .iter()
            .filter(|row| row.withheld == WITHHELD_ROW)
            .collect();
        assert_eq!(named.len(), carried - kept, "{:?}", report.withheld_rows);
        assert!(named.iter().all(|row| !in_pack.contains(&row.entity_id)));
    }

    #[test]
    fn a_row_two_focals_share_is_carried_once_and_counted_as_shared() {
        // link_1 is a direct neighbour of both link_0 and link_2.
        let (store, links) = chain(3);
        let (pack, report) =
            build_multi_focal_pack(&store, &[links[0].id, links[2].id], &opts(8000)).unwrap();

        let carried = pack
            .dependency_signatures
            .iter()
            .chain(pack.transitive_deps.iter())
            .filter(|entry| entry.entity_id == links[1].id)
            .count();
        assert_eq!(carried, 1, "the shared entity is in the pack once");
        assert!(
            report.focals.iter().any(|focal| focal.shared_rows > 0),
            "and the overlap is reported rather than hidden: {:?}",
            report.focals
        );
    }

    /// Every pair of `len` focals joined by a direct call, so each pair has a
    /// route description and no route material competes with the focals.
    fn meshed(len: usize) -> (kin_db::InMemoryGraph, Vec<Entity>) {
        let store = kin_db::InMemoryGraph::new();
        let mut links = Vec::new();
        for index in 0..len {
            let entity = make_entity(&format!("focal_{index}"), &format!("src/focal_{index}.rs"));
            store.upsert_entity(&entity).unwrap();
            links.push(entity);
        }
        for left in 0..len {
            for right in (left + 1)..len {
                calls(&store, &links[left], &links[right]);
            }
        }
        (store, links)
    }

    #[test]
    fn an_advertised_minimum_keeps_the_highest_ranked_focal_on_retry() {
        let (store, focals) = meshed(3);
        let ids: Vec<_> = focals.iter().map(|focal| focal.id).collect();
        let build = |budget| {
            let options = MultiFocalOptions {
                resolutions: [0.1, 0.9, 0.5]
                    .into_iter()
                    .map(FocalResolution::from_question)
                    .collect(),
                ..opts(budget)
            };
            let mut measure = whole_reply;
            build_multi_focal_pack_inner(
                &store,
                &ids,
                &options,
                None,
                ProjectionLimits::default(),
                Some(&mut measure),
            )
        };
        let floor = match build(1).unwrap_err() {
            ContextError::BudgetExceeded { actual, budget: 1 } => actual,
            other => panic!("expected a measured minimum, got {other:?}"),
        };
        for budget in [floor, floor + 1] {
            let (pack, report, _) = build(budget).unwrap();
            assert!(pack
                .focal_entities
                .iter()
                .any(|entry| entry.entity_id == focals[1].id));
            assert_eq!(
                report
                    .focals
                    .iter()
                    .map(|focal| focal.shared_rows)
                    .sum::<usize>(),
                pack.focal_entities.len() * 2
            );
            if let Some(elision) = report.elisions.get(FOCAL_GROUP) {
                assert_eq!(elision.total, 3);
            }
            assert!(report.measured_tokens <= budget);
        }
    }

    /// Build with a caller-supplied measurement, as `kin context` and
    /// `get_context_pack` do.
    fn build_measured(
        store: &kin_db::InMemoryGraph,
        ids: &[EntityId],
        budget: usize,
        measure: &mut MultiMeasure<'_>,
    ) -> crate::Result<(ContextPack, MultiFocalReport)> {
        build_multi_focal_pack_inner(
            store,
            ids,
            &opts(budget),
            None,
            ProjectionLimits::default(),
            Some(measure),
        )
        .map(|(pack, report, _)| (pack, report))
    }

    /// The reply `kin context --json` holds to its budget: the rendered lines
    /// and the serialized report beside them.
    fn whole_reply(
        pack: &ContextPack,
        report: &MultiFocalReport,
        _: &ProjectionReport,
    ) -> crate::Result<usize> {
        Ok(
            estimate_tokens(&render_multi_focal_lines(pack, report).join("\n"))
                + estimate_tokens(&serde_json::to_string_pretty(report).unwrap()),
        )
    }

    /// The code alone, which is what `get_context_pack` bounds.
    fn answer_only(
        pack: &ContextPack,
        _: &MultiFocalReport,
        _: &ProjectionReport,
    ) -> crate::Result<usize> {
        Ok(pack
            .focal_entities
            .iter()
            .chain(&pack.dependency_signatures)
            .chain(&pack.transitive_deps)
            .map(|entry| estimate_tokens(&entry.content))
            .sum())
    }

    /// Five focals joined pairwise carry ten route descriptions. Held to the
    /// whole reply, those descriptions cost more than the focals' signatures,
    /// and a fit that could not shed them refused budgets the answer fitted
    /// easily. They go before any focal loses its signature, and the report
    /// says how many went.
    #[test]
    fn route_descriptions_go_before_any_focal_loses_its_signature() {
        let (store, links) = meshed(5);
        let ids: Vec<EntityId> = links.iter().map(|link| link.id).collect();
        let (_, generous) = build_measured(&store, &ids, 20_000, &mut whole_reply).unwrap();
        assert_eq!(generous.route_search.pairs_connected, 10);
        assert_eq!(generous.routes.len(), 10);
        assert_eq!(generous.route_search.routes_withheld, 0);

        let mut shed_any = false;
        for budget in (100..generous.measured_tokens).rev().step_by(25) {
            let Ok((pack, report)) = build_measured(&store, &ids, budget, &mut whole_reply) else {
                continue;
            };
            assert!(
                report.measured_tokens <= budget,
                "{budget}: measured {}",
                report.measured_tokens
            );
            assert_eq!(
                report.routes.len() + report.route_search.routes_withheld,
                report.route_search.pairs_connected,
                "{budget}: every connected pair is described or counted"
            );
            let lost_signature = report
                .focals
                .iter()
                .any(|focal| matches!(focal.projection.as_str(), "name_and_kind" | "elided"));
            if lost_signature {
                assert!(
                    report.routes.is_empty(),
                    "{budget}: a focal lost its signature while {} route descriptions stayed",
                    report.routes.len()
                );
            }
            if report.route_search.routes_withheld > 0 {
                shed_any = true;
                let lines = render_multi_focal_lines(&pack, &report).join("\n");
                assert!(
                    lines.contains("route description"),
                    "{budget}: the human form says descriptions were withheld: {lines}"
                );
            }
        }
        assert!(
            shed_any,
            "some budget in the sweep sheds a route description"
        );
    }

    /// A caller that bounds only the code gains nothing from shedding a route
    /// description, so it keeps every one, however tight the budget.
    #[test]
    fn a_caller_bounding_only_the_code_keeps_every_route_description() {
        let (store, links) = meshed(5);
        let ids: Vec<EntityId> = links.iter().map(|link| link.id).collect();
        for budget in [2_000, 400, 120, 60, 30] {
            let Ok((_, report)) = build_measured(&store, &ids, budget, &mut answer_only) else {
                continue;
            };
            assert_eq!(report.route_search.routes_withheld, 0, "{budget}");
            assert_eq!(
                report.routes.len(),
                report.route_search.pairs_connected,
                "{budget}"
            );
        }
    }

    /// The list of withheld rows is capped by the caller, focals first, and the
    /// rows it stops naming are still counted.
    #[test]
    fn capping_the_withheld_list_names_focals_first_and_counts_the_rest() {
        let row = |id: &str, group: &str| WithheldRow {
            entity_id: id.to_string(),
            group: group.to_string(),
            withheld: WITHHELD_ROW.to_string(),
        };
        let (store, links) = chain(2);
        let (_, mut report) = build_multi_focal_pack(&store, &[links[0].id], &opts(8000)).unwrap();
        report.withheld_rows = vec![
            row("a", group::DEPENDENCIES),
            row("b", group::TESTS),
            row("c", FOCAL_GROUP),
            row("d", group::DEPENDENTS),
        ];
        report.list_withheld_rows_at_most(2);
        let named: Vec<&str> = report
            .withheld_rows
            .iter()
            .map(|row| row.entity_id.as_str())
            .collect();
        assert_eq!(named, vec!["c", "a"]);
        assert_eq!(report.withheld_rows_unlisted, 2);
        report.list_withheld_rows_at_most(1);
        assert_eq!(report.withheld_rows.len(), 1);
        assert_eq!(
            report.withheld_rows_unlisted, 3,
            "a second cap keeps the count"
        );
    }
}
