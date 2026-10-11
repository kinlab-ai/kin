// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The closed answer to a reach question: everything that can end up calling
//! a focal, under a policy the answer declares.
//!
//! A callers walk returns a chain cut to a depth and a per-step cap. That is
//! the right shape for "how does this get here" and the wrong one for "which
//! functions can end up calling this": a chain that held every caller still
//! read "complete except indirect uses", and an agent holding the right set
//! read the source of every member to check what the reply had not said, and
//! ran out of time without answering. So a walk that reads callers also
//! answers the reach question itself, as the payload's `reach` block:
//!
//! - `members`: every entity that reaches the focal through a followed edge,
//!   each once, package by package and nearest first within one. `packages`
//!   says how far each package was read when there is more than one. `via`
//!   names the entity it calls on its way to
//!   the focal, and `line` and `code` the call it makes to `via`, in its own
//!   body. Every row is one hop, so a member's witness path is its `via`
//!   chain, and paths share their tails instead of repeating them.
//! - `name_only`: the entities that reach the focal only by a path with a
//!   hop matched by a bare name, as rows of the same shape. A resolver tied
//!   none of those hops to its target, so each is a possible caller and
//!   never counted among the members: `self.options.get("subdomain")`
//!   reached a method named `get` this way. An entity with a path free of
//!   such a hop is a member by that path.
//! - `also_referenced_by`: entities that name a member without calling it (a
//!   reference, a type use, a construction, or a use as a value), each with
//!   its site. Never mixed into `members`.
//! - `frontier`: the concrete places the reading could not settle under the
//!   policy, each with its code and why: a call that spells a member's name
//!   and no resolver proved, a caller whose calls nobody has checked, an
//!   invocation with no call expression, a lookup by a computed name that can
//!   fetch a member, a file with no parse on record, and a method a member
//!   overrides. Empty means the closure is complete under the policy.
//! - `bounds`: each limit that stopped the closure or its reading, and where.
//!   A bound never reads as an exhaustive answer: `state` is `bounded`.
//!
//! The policy follows `Calls` edges at every resolution, call sites a scan
//! promotes because their name can reach only the member, and callbacks and
//! dynamic dispatch a call-site ledger records with a dispatch set that names
//! a member. It does not follow a member held as a value: every place one is
//! held is listed under `also_referenced_by` with the use, and the calls that
//! could run it later are counted under `blind_spots`, not listed.
//!
//! Nothing here reads a language: edges, ledgers and the escape census are
//! the same records for every adapter.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use kin_index::RelationResolution;
use kin_model::graph::GraphStore;
use kin_model::{
    CallSiteState, CallerSites, Entity, EntityId, EntityKind, EntityRole, FocalEscape, GraphNodeId,
    RelationKind, ResolutionRecordId, SiteStateKind,
};

use crate::call_sites::{CandidateSite, FocalScan};
use crate::handlers::common::{EntitySourceScope, HeldSourceAuthority};
use crate::handlers::external_symbols::{CalleeText, SiteText};
use crate::trace_scope::{Placement, ScopeMode, TraceScope};

/// The key every trace payload carries the block under.
pub const REACH_KEY: &str = "reach";

/// Join the complete reach reading with this page's rows. A page may carry
/// both; choosing the row object alone loses the closure's bounds and gaps.
/// Conflicting or incomplete evidence cannot establish a closure answer.
pub(crate) fn answer_view(machine: &Map<String, Value>) -> Option<Map<String, Value>> {
    if machine.contains_key("record_fragment") {
        return None;
    }
    let mut direction = machine.get("direction");
    let mut reach = machine
        .get(REACH_KEY)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut seen = false;
    if let Some(readings) = machine.get("readings").and_then(Value::as_array) {
        for reading in readings {
            match reading.get("key").and_then(Value::as_str) {
                Some("direction") => {
                    let value = reading.get("value")?;
                    if direction.is_some_and(|current| current != value) {
                        return None;
                    }
                    direction = Some(value);
                }
                Some(REACH_KEY) => {
                    if seen {
                        return None;
                    }
                    seen = true;
                    for (key, value) in reading.get("value")?.as_object()? {
                        if reach.get(key).is_some_and(|current| current != value) {
                            return None;
                        }
                        reach.entry(key.clone()).or_insert_with(|| value.clone());
                    }
                }
                _ => {}
            }
        }
    }
    if direction?.as_str()? != "callers"
        || !matches!(reach.get("state")?.as_str()?, "closed" | "open" | "bounded")
        || !reach.get("bounds")?.is_array()
        || !reach.get("frontier_total")?.is_u64()
        || !reach.get("members_total")?.is_u64()
    {
        return None;
    }
    Some(reach)
}

/// Whether every recorded answer row is present, independently of whether
/// the graph established closure. Missing rows and fragments stay paged.
pub(crate) fn rows_delivered(reach: &Map<String, Value>) -> bool {
    [
        ("members", "members_total"),
        ("frontier", "frontier_total"),
        ("also_referenced_by", "also_referenced_total"),
    ]
    .iter()
    .all(|(rows, total)| {
        match (
            reach.get(*rows).and_then(Value::as_array),
            reach.get(*total).and_then(Value::as_u64),
        ) {
            (Some(rows), Some(total)) => rows.len() as u64 == total,
            (None, Some(0)) => true,
            _ => false,
        }
    }) && reach
        .get("frontier_withheld")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
        // Rows matched by name alone, when the reading has any.
        && reach
            .get(NAME_ONLY_TOTAL_KEY)
            .and_then(Value::as_u64)
            .is_none_or(|total| {
                reach
                    .get(NAME_ONLY_KEY)
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len) as u64
                    == total
            })
}

/// Members one closure admits before it reports the bound.
pub const MEMBERS_MAX: usize = 2_000;

/// Members whose unresolved calls one closure reads, nearest first. Each read
/// is a store-wide scan, the cost one `find_references` pays, so the reading
/// is bounded by this count and by the walk's work budget, and a member past
/// either is named under `bounds`.
pub const FRONTIER_READS_MAX: usize = 24;

/// Frontier rows one block lists before it counts the rest per member.
pub const FRONTIER_ROWS_MAX: usize = 60;

/// Entities `also_referenced_by` lists before it counts the rest.
pub const REFERRERS_MAX: usize = 60;

/// Sites one residue list contributes before the rest are counted.
const RESIDUE_SAMPLE: usize = 20;

/// Bodies one closure reads to quote its sites.
const CODE_READS_MAX: usize = 500;

/// Characters of one quoted line.
const CODE_CHARS_MAX: usize = 160;

/// Entities a bound names as where it stopped.
const BOUND_AT_MAX: usize = 5;

/// What stops a closure's work: a caller that stopped waiting, or the walk's
/// budget of examined relations. The walk that owns the budget implements
/// this, so a closure spends the same count as the chain it rides with.
///
/// Both are amounts of work, the same on a busy machine as on an idle one.
/// The walk's clock is deliberately not among them: a closure of nine members
/// answered `complete` or `partial` for one graph depending on how loaded the
/// machine was, because the clock that started with the walk ran out while
/// the last members were read. What bounds the reading is counted: members
/// admitted, members read and relations examined. The one clock left is
/// [`Limits::safety_time`], far above what any of those counts needs.
pub trait ReachMeter {
    /// Charge one examined relation; the name of the bound when work must stop.
    fn charge_edge(&mut self) -> Option<&'static str>;
    /// The name of the bound when work must stop before any more starts.
    /// Never a clock.
    fn stop(&self) -> Option<&'static str>;
}

/// A budget of examined relations for a walk that carries none of its own.
pub struct WorkMeter {
    edges: usize,
    max_edges: usize,
}

impl WorkMeter {
    pub fn new(max_edges: usize) -> Self {
        Self {
            edges: 0,
            max_edges,
        }
    }

    /// The budget a trace walk ships with.
    pub fn shipped() -> Self {
        Self::new(250_000)
    }
}

impl ReachMeter for WorkMeter {
    fn charge_edge(&mut self) -> Option<&'static str> {
        self.edges = self.edges.saturating_add(1);
        self.stop()
    }

    fn stop(&self) -> Option<&'static str> {
        (self.edges >= self.max_edges).then_some("edge_budget")
    }
}

/// The name of the bound [`Limits::safety_time`] reports.
pub const SAFETY_TIME_LIMIT: &str = "safety_time_limit";

/// How long one closure may take before it stops whatever its counts say.
/// A closure of a few members reads in seconds on a busy machine, so this is
/// no bound a healthy reading meets; it exists so a reading something has
/// gone wrong with still answers.
pub const SAFETY_TIME: Duration = Duration::from_secs(60);

/// The closure's own bounds, echoed under `policy.bounds`.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub members_max: usize,
    pub frontier_reads_max: usize,
    /// The one clock a closure reads, counted from when its reading starts
    /// (see [`SAFETY_TIME`]).
    pub safety_time: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            members_max: MEMBERS_MAX,
            frontier_reads_max: FRONTIER_READS_MAX,
            safety_time: SAFETY_TIME,
        }
    }
}

/// Where a block that left test code out says so, and counts what it left:
/// `{"call_sites": n, "callers": m}`. `call_sites` counts each test caller
/// once per member it calls, or could call by name; `callers` counts the
/// test entities. Present, with zeros, on every block read without tests.
pub const TESTS_EXCLUDED_KEY: &str = "tests_excluded";

// ── Proof of one hop ──────────────────────────────────────────────────────

/// How one hop was established, weakest first, so the weakest hop on a path
/// is its `min`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Proof {
    /// A ledger records that a call through a value binding may hold it.
    Callback,
    /// The edge was matched by a bare name.
    NameOnly,
    /// An unresolved site spells the one name only it carries.
    NameUnique,
    /// A ledger records a dynamic call whose dispatch set names it.
    Dispatch,
    ImportScoped,
    TypeResolved,
}

impl Proof {
    fn of(resolution: RelationResolution) -> Self {
        match resolution {
            RelationResolution::NameOnly => Self::NameOnly,
            RelationResolution::ImportScoped => Self::ImportScoped,
            RelationResolution::TypeResolved => Self::TypeResolved,
        }
    }

    fn wire(self) -> &'static str {
        match self {
            Self::Callback => "callback",
            Self::NameOnly => "name_only",
            Self::NameUnique => "name_unique",
            Self::Dispatch => "dispatch",
            Self::ImportScoped => "import_scoped",
            Self::TypeResolved => "type_resolved",
        }
    }

    fn proven(self) -> bool {
        self >= Self::ImportScoped
    }
}

// ── Sites ─────────────────────────────────────────────────────────────────

/// Where a site is, in whatever terms the record that named it gave, until
/// its line is quoted from the entity that holds it.
#[derive(Debug, Clone, Default)]
struct SiteRef {
    /// 1-based file line.
    line: Option<u32>,
    /// File byte the site starts at.
    byte: Option<usize>,
    /// The site as the record that named it quoted it, served only when the
    /// holder's own line cannot be read.
    code: Option<String>,
}

/// Reads and quotes the lines sites sit on, each holder's body once.
struct Quoter<'t, T: SiteText + ?Sized> {
    text: &'t T,
    bodies: RefCell<HashMap<EntityId, Option<String>>>,
}

impl<'t, T: SiteText + ?Sized> Quoter<'t, T> {
    fn new(text: &'t T) -> Self {
        Self {
            text,
            bodies: RefCell::new(HashMap::new()),
        }
    }

    fn body(&self, entity: &Entity) -> Option<String> {
        if let Some(body) = self.bodies.borrow().get(&entity.id) {
            return body.clone();
        }
        let body = (self.bodies.borrow().len() < CODE_READS_MAX)
            .then(|| crate::site_expression::whole_text(self.text, entity).ok())
            .flatten();
        self.bodies.borrow_mut().insert(entity.id, body.clone());
        body
    }

    /// The site's 1-based line and its line of code, read from `holder`: the
    /// whole source line the site sits on, or the record's own quote when the
    /// holder's text cannot be read.
    fn quote(&self, holder: &Entity, site: &SiteRef) -> (Option<u32>, Option<String>) {
        let fallback = site.code.as_deref().map(clip);
        let Some(span) = holder.span.as_ref() else {
            return (site.line, fallback);
        };
        let Some(body) = self.body(holder) else {
            return (site.line, fallback);
        };
        // By byte first: it names the site exactly, where a line can be off by
        // the holder's own first line.
        if let Some(offset) = site
            .byte
            .and_then(|byte| byte.checked_sub(span.start_byte))
            .filter(|offset| *offset <= body.len())
        {
            let offset = floor_boundary(&body, offset);
            let line = span.start_line + body[..offset].matches('\n').count() as u32 + 1;
            return (Some(line), line_around(&body, offset).or(fallback));
        }
        let text = site
            .line
            .and_then(|line| line.checked_sub(span.start_line + 1))
            .and_then(|index| body.lines().nth(index as usize))
            .map(|line| clip(line.trim()))
            .filter(|line| !line.is_empty());
        (site.line, text.or(fallback))
    }
}

/// Quotes the source line a site sits on from the entity that holds it, each
/// holder's body read once: the `code` a chain step serves beside its
/// `reference_lines`, so a step shows the call that proves it.
pub struct LineQuoter<'t, T: SiteText + ?Sized>(Quoter<'t, T>);

impl<'t, T: SiteText + ?Sized> LineQuoter<'t, T> {
    pub fn new(text: &'t T) -> Self {
        Self(Quoter::new(text))
    }

    /// The code on 1-based file `line` inside `holder`, or `None` when its
    /// text cannot be read.
    pub fn line(&self, holder: &Entity, line: u32) -> Option<String> {
        self.0
            .quote(
                holder,
                &SiteRef {
                    line: Some(line),
                    byte: None,
                    code: None,
                },
            )
            .1
    }
}

/// Whether a chain step with `role` holds its own call site. An inward step
/// (a caller, referrer, importer or type user) names its parent from its own
/// body; a callee's site sits in its parent.
pub fn step_holds_its_site(role: &str) -> bool {
    role != "callee"
}

/// Fill each step's `code` in a JSON `chain`: the line of its first
/// reference line, read from the entity that holds the site, or null.
pub fn quote_chain<G: GraphStore + ?Sized, T: SiteText + ?Sized>(
    store: &G,
    focal: &Entity,
    chain: &mut [Value],
    text: &T,
) {
    let quoter = LineQuoter::new(text);
    let ids: Vec<Option<String>> = chain
        .iter()
        .map(|step| step["entity_id"].as_str().map(str::to_string))
        .collect();
    let mut holders: HashMap<String, Option<Entity>> = HashMap::new();
    for step in chain.iter_mut() {
        let line = step["reference_lines"]
            .as_array()
            .and_then(|lines| lines.iter().filter_map(Value::as_u64).min())
            .and_then(|line| u32::try_from(line).ok());
        let holder_id = if step_holds_its_site(step["role"].as_str().unwrap_or("callee")) {
            ids.get(step["step"].as_u64().unwrap_or(0).saturating_sub(1) as usize)
                .cloned()
                .flatten()
        } else {
            match step["parent_step"].as_u64() {
                Some(0) => Some(focal.id.to_string()),
                Some(parent) => ids.get(parent as usize - 1).cloned().flatten(),
                None => None,
            }
        };
        let code = match (line, holder_id) {
            (Some(line), Some(id)) => holders
                .entry(id.clone())
                .or_insert_with(|| {
                    uuid::Uuid::parse_str(&id)
                        .ok()
                        .and_then(|uuid| store.get_entity(&EntityId(uuid)).ok().flatten())
                })
                .as_ref()
                .and_then(|holder| quoter.line(holder, line)),
            _ => None,
        };
        step["code"] = json!(code);
    }
}

fn floor_boundary(text: &str, mut offset: usize) -> usize {
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn line_around(body: &str, offset: usize) -> Option<String> {
    let start = body[..offset].rfind('\n').map_or(0, |at| at + 1);
    let end = body[offset..]
        .find('\n')
        .map_or(body.len(), |at| offset + at);
    let line = body[start..end].trim();
    (!line.is_empty()).then(|| clip(line))
}

fn clip(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= CODE_CHARS_MAX {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(CODE_CHARS_MAX).collect();
    clipped.push('…');
    clipped
}

/// The first site of `relation` in `file`, as a site: a proven one when the
/// edge holds any, else any site it records, because the hop's `proof`
/// already says how far it was proven and a name-only hop still has a line.
fn relation_site(relation: &kin_model::Relation, file: Option<&kin_model::FilePathId>) -> SiteRef {
    let (proven, _) = crate::handlers::common::proven_relation_reference_lines(relation, file);
    let spans = if proven.spans.is_empty() {
        crate::handlers::common::relation_reference_lines(relation, file).spans
    } else {
        proven.spans
    };
    spans
        .iter()
        .min_by_key(|span| (span.start_line, span.start_byte))
        .map(|span| SiteRef {
            line: Some(span.start_line + 1),
            byte: Some(span.start_byte),
            code: None,
        })
        .unwrap_or_default()
}

/// The 1-based file line of a site `line_in_entity` lines into `caller`.
fn candidate_line(caller: Option<&Entity>, site: &CandidateSite) -> Option<u32> {
    let start = caller?.span.as_ref()?.start_line;
    site.line_in_entity
        .and_then(|line| u32::try_from(line).ok())
        .map(|line| start + line + 1)
}

// ── What one scan says about reaching its member ──────────────────────────

/// One place a scan could not settle, before the closure knows its members.
#[derive(Debug, Clone)]
struct Open {
    kind: &'static str,
    /// The entity the place is in: the caller holding the site, or the
    /// declaration a member overrides.
    within: Option<(EntityId, String)>,
    file: Option<String>,
    site: SiteRef,
    why: String,
    /// The reason this place is counted rather than listed, for a kind of
    /// place that never held a caller the walk missed: it is not part of the
    /// answer's uncertainty, and it is never an absence either.
    fold: Option<&'static str>,
}

/// A caller a scan found that reaches its member by a hop an edge does not
/// record.
#[derive(Debug, Clone)]
struct Offer {
    caller: Entity,
    proof: Proof,
    site: SiteRef,
}

/// A place the census lists the member held as a value.
#[derive(Debug, Clone)]
struct ValueUse {
    inside: Option<EntityId>,
    file: String,
    line: u32,
    context: &'static str,
    code: Option<String>,
}

/// What one member's scan says about reaching it, kept once the scan itself
/// is dropped.
#[derive(Debug, Clone, Default)]
pub struct MemberReading {
    opens: Vec<Open>,
    /// Places the scan listed beyond what `opens` carries, counted.
    unlisted: usize,
    offers: Vec<Offer>,
    values: Vec<ValueUse>,
    /// Places the census lists the member held as a value beyond `values`,
    /// counted: no holder row can name them.
    values_unlisted: usize,
    value_call_sites: u64,
    /// Whether every caller that could spell the member's call names has a
    /// current call-site ledger: nothing owed, stale, unverified or without a
    /// resolver ([`FocalScan::named_complete`]).
    named_complete: bool,
    unkeyed: BTreeSet<(String, u32)>,
    /// Why no census was taken for the member, when none was: whether it is
    /// held as a value or run without a call expression is then not listed.
    uncensused: Option<&'static str>,
    /// Test callers a scan that excludes tests left out for the member.
    tests_left_out: Vec<EntityId>,
}

impl MemberReading {
    /// Read `scan` for the closure. `text` quotes residue lines.
    fn of<G: GraphStore + ?Sized, T: SiteText + ?Sized>(
        store: &G,
        member: &Entity,
        scan: &FocalScan,
        text: &T,
    ) -> Self {
        let mut reading = Self {
            value_call_sites: scan.value_calls.sites,
            named_complete: scan.named_complete(),
            tests_left_out: scan.excluded_test_callers.clone(),
            ..Self::default()
        };
        let callers: HashMap<EntityId, &Entity> = scan
            .callers
            .iter()
            .map(|caller| (caller.entity.id, &caller.entity))
            .collect();
        for site in &scan.named {
            let caller = callers.get(&site.caller).copied();
            reading.opens.push(Open {
                kind: "unresolved_call",
                within: Some((site.caller, site.caller_name.clone())),
                file: site.caller_file.clone(),
                site: SiteRef {
                    line: candidate_line(caller, site),
                    byte: None,
                    code: site.expr.clone(),
                },
                why: unresolved_why(site, &member.name),
                fold: matches!(
                    site.reason,
                    kin_model::call_site_reading::REACH_TEXT_UNKNOWN
                        | kin_model::call_site_reading::REACH_NAME_UNKNOWN
                )
                .then_some(FOLD_TEXT_NOT_READ),
            });
        }
        for caller in scan.callers.iter().filter(|caller| caller.named) {
            if let Some(why) = unchecked_why(&caller.reading) {
                reading.opens.push(Open {
                    kind: "caller_unchecked",
                    within: Some((caller.entity.id, caller.entity.name.clone())),
                    file: caller
                        .entity
                        .file_origin
                        .as_ref()
                        .map(|path| path.0.clone()),
                    site: SiteRef {
                        line: caller.entity.span.as_ref().map(|span| span.start_line + 1),
                        byte: None,
                        code: None,
                    },
                    why,
                    fold: None,
                });
            }
        }
        for site in &scan.promoted {
            if let Some(caller) = callers.get(&site.caller) {
                reading.offers.push(Offer {
                    caller: (*caller).clone(),
                    proof: Proof::NameUnique,
                    site: SiteRef {
                        line: candidate_line(Some(caller), site),
                        byte: None,
                        code: site.expr.clone(),
                    },
                });
            }
        }
        reading.read_forwarding(store, member, scan);
        reading.read_census(store, member, scan, text);
        reading
    }

    /// Callers whose ledger records a call through a value, or a dynamic
    /// call, whose dispatch set names `member`.
    fn read_forwarding<G: GraphStore + ?Sized>(
        &mut self,
        store: &G,
        member: &Entity,
        scan: &FocalScan,
    ) {
        let mut sets: HashMap<ResolutionRecordId, bool> = HashMap::new();
        let mut names_member = |set: ResolutionRecordId| -> bool {
            *sets.entry(set).or_insert_with(|| {
                store
                    .lookup_resolution_record(&set)
                    .ok()
                    .flatten()
                    .and_then(|record| record.as_dispatch_set().cloned())
                    .is_some_and(|dispatch| {
                        dispatch.members.contains(&GraphNodeId::Entity(member.id))
                    })
            })
        };
        for caller in &scan.callers {
            if caller.entity.id == member.id {
                continue;
            }
            let Some(ledger) = caller.reading.ledger() else {
                continue;
            };
            for site in &ledger.sites {
                let (set, proof) = match &site.state {
                    CallSiteState::Binding {
                        may_call: Some(set),
                    } => (*set, Proof::Callback),
                    CallSiteState::ProvenDeclaration {
                        declaration,
                        set: Some(set),
                    } if *declaration != member.id => (*set, Proof::Dispatch),
                    _ => continue,
                };
                if !names_member(set) {
                    continue;
                }
                self.offers.push(Offer {
                    caller: caller.entity.clone(),
                    proof,
                    site: SiteRef {
                        line: None,
                        byte: caller
                            .entity
                            .span
                            .as_ref()
                            .map(|span| span.start_byte + site.offset as usize),
                        code: None,
                    },
                });
                break;
            }
        }
    }

    /// The census residue: values the closure lists as references, and
    /// places that can run the member without a call that spells its name.
    fn read_census<G: GraphStore + ?Sized, T: SiteText + ?Sized>(
        &mut self,
        store: &G,
        member: &Entity,
        scan: &FocalScan,
        text: &T,
    ) {
        let residue = match &scan.escape {
            FocalEscape::Unknown { reason } => {
                self.uncensused = Some(reason);
                return;
            }
            escape => match escape.residue() {
                Some(residue) => residue,
                None => return,
            },
        };
        let lines =
            crate::site_expression::residue_lines(store, text, &scan.escape, RESIDUE_SAMPLE);
        let line_of = |site: &kin_model::value_escape::ResidueSite| {
            lines.get(&(site.file.clone(), site.line))
        };
        let within_of = |site: &kin_model::value_escape::ResidueSite| {
            line_of(site).and_then(|line| line.inside.clone())
        };
        let code_of = |site: &kin_model::value_escape::ResidueSite| {
            line_of(site)
                .and_then(|line| line.text.as_ref().ok())
                .map(|text| clip(text))
                .or_else(|| site.detail.clone())
        };
        for site in residue.values.iter().take(RESIDUE_SAMPLE) {
            self.values.push(ValueUse {
                inside: within_of(site).map(|(id, _)| id),
                file: site.file.clone(),
                line: site.line,
                context: site.kind,
                code: code_of(site),
            });
        }
        let values_unlisted = residue.values.len().saturating_sub(RESIDUE_SAMPLE);
        self.unlisted += values_unlisted;
        self.values_unlisted += values_unlisted;
        let name = member
            .name
            .rsplit(['.', ':'])
            .next()
            .unwrap_or(&member.name);
        // The places the language runs a constructor or a call protocol from
        // are the class's construction and the instance's own call sites,
        // which the block reports apart; they are counted, not listed.
        let dunder = dunder_fold(name);
        let mut listed =
            |kind: &'static str,
             sites: &[kin_model::value_escape::ResidueSite],
             fold: Option<&'static str>,
             why: &dyn Fn(&kin_model::value_escape::ResidueSite) -> String| {
                let shown = if fold.is_some() {
                    sites.len()
                } else {
                    RESIDUE_SAMPLE
                };
                for site in sites.iter().take(shown) {
                    self.opens.push(Open {
                        kind,
                        within: within_of(site),
                        file: Some(site.file.clone()),
                        site: SiteRef {
                            line: Some(site.line),
                            byte: None,
                            code: code_of(site),
                        },
                        why: why(site),
                        fold,
                    });
                }
                self.unlisted += sites.len().saturating_sub(shown);
            };
        listed("implicit_call", &residue.implicit_calls, dunder, &|site| {
            format!(
                "`{}` syntax runs `{name}` without a call expression, so no call edge records it",
                site.kind
            )
        });
        listed("reflective", &residue.reflective, None, &|_| {
            format!("a lookup by a name computed at run time can fetch `{name}` here")
        });
        listed("unrecorded", &residue.unrecorded, None, &|site| {
            format!(
                "no parse is on record for this region, so a call to `{name}` in it is unknown{}",
                site.detail
                    .as_deref()
                    .map(|detail| format!(" ({detail})"))
                    .unwrap_or_default()
            )
        });
        if let Some(protocol) = &residue.protocol {
            if protocol.sites.is_empty() {
                self.opens.push(Open {
                    kind: "protocol",
                    within: None,
                    file: member.file_origin.as_ref().map(|path| path.0.clone()),
                    site: SiteRef {
                        line: member.span.as_ref().map(|span| span.start_line + 1),
                        byte: None,
                        code: Some(member.signature.clone()).filter(|text| !text.is_empty()),
                    },
                    why: format!(
                        "the language runs `{name}` through {}, which spells no name, so those \
                         places are not listed",
                        protocol.through.as_deref().unwrap_or("its own syntax")
                    ),
                    fold: dunder,
                });
            } else {
                listed("protocol", &protocol.sites, dunder, &|site| {
                    format!("`{}` syntax runs `{name}` here", site.kind)
                });
            }
        } else if scan.implicit_dispatch {
            self.opens.push(Open {
                kind: "implicit_dispatch",
                within: None,
                file: member.file_origin.as_ref().map(|path| path.0.clone()),
                site: SiteRef {
                    line: member.span.as_ref().map(|span| span.start_line + 1),
                    byte: None,
                    code: Some(member.signature.clone()).filter(|text| !text.is_empty()),
                },
                why: format!(
                    "the language can run `{name}` without a call that spells its name, and no \
                     record lists where"
                ),
                fold: dunder,
            });
        }
        for site in &residue.unkeyed_lookups {
            self.unkeyed.insert((site.file.clone(), site.line));
        }
    }
}

/// A named site whose caller's text Kin could not read, so whether it calls
/// the member at all is unknown. Such places are the repository's coverage-gap
/// inventory: the same ones stand beside every member, and none held a caller.
pub(crate) const FOLD_TEXT_NOT_READ: &str = "caller_text_not_read";

/// A place the language runs `__call__` from: a call of an instance.
pub(crate) const FOLD_CALL_PROTOCOL: &str = "call_protocol";

/// A place the language runs `__init__` from: a construction of its class,
/// which the block reports apart as an instantiation.
pub(crate) const FOLD_INIT_DISPATCH: &str = "constructor_dispatch";

/// The fold for the protocol and implicit places of a member named `name`.
fn dunder_fold(name: &str) -> Option<&'static str> {
    match name {
        "__call__" => Some(FOLD_CALL_PROTOCOL),
        "__init__" => Some(FOLD_INIT_DISPATCH),
        _ => None,
    }
}

/// Why a named leftover could not settle, in plain words.
fn unresolved_why(site: &CandidateSite, member: &str) -> String {
    let name = member.rsplit(['.', ':']).next().unwrap_or(member);
    let what = match site.reason {
        kin_model::call_site_reading::REACH_CALLEE_SPELLS => {
            format!("calls `{}`", site.callee.as_deref().unwrap_or(name))
        }
        kin_model::call_site_reading::REACH_BODY_SPELLS => {
            format!("its body spells `{name}` at a call Kin could not tie to a name")
        }
        _ => format!("it could call `{name}` but its text was not read"),
    };
    let state = match site.state {
        SiteStateKind::Unresolved => "no resolver proved where the call goes",
        SiteStateKind::ServerFailed => "the language server failed on it",
        SiteStateKind::NotInBuild => "no build of the repository compiles it",
        SiteStateKind::OwedDerivation | SiteStateKind::OwedEnrichment => {
            "its calls are not checked yet"
        }
        SiteStateKind::ProofContextStale => "it was checked under an older build setup",
        SiteStateKind::ProofContextUnverified => {
            "it was checked under a build setup that is not validated"
        }
        SiteStateKind::UnprovenNoResolver => "no language server can check it on this machine",
        SiteStateKind::Binding => "the callee is a value Kin cannot pin down",
        _ => "its target is not proven",
    };
    let receiver = match site.receiver_hint {
        Some("names_owner") | Some("typed_as_owner") => {
            "; its receiver can hold the member's owner"
        }
        Some("literal") | Some("builtin") => "; its receiver is a literal or a builtin",
        _ => "",
    };
    format!("{what}, and {state}{receiver}")
}

/// Why a caller in a member's named scope has no call Kin checked, or `None`
/// for one whose calls are read.
fn unchecked_why(reading: &CallerSites) -> Option<String> {
    Some(match reading {
        CallerSites::OwedDerivation => {
            "its file changed since Kin read it, so its calls are not read yet".to_string()
        }
        CallerSites::OwedEnrichment => {
            "it could call the member by name and its calls are not checked yet".to_string()
        }
        CallerSites::NoResolver { why, .. } => format!(
            "it could call the member by name and no language server can check its calls \
             here ({})",
            why.wire()
        ),
        CallerSites::NoSites
        | CallerSites::Stale(_)
        | CallerSites::Unverified { .. }
        | CallerSites::Current(_) => return None,
    })
}

// ── Scans shared across the readers of one walk ───────────────────────────

/// One member's scan, as both readers of a walk keep it.
#[derive(Debug, Clone)]
pub struct Scanned {
    method: crate::trace_named_scope::MethodSummary,
    reach: MemberReading,
}

/// The scans one walk took, by member, so the named-scope reading and the
/// reach reading scan each member once. `None` is a scan the graph could not
/// answer.
#[derive(Debug)]
pub struct Scans {
    entries: HashMap<EntityId, Option<Scanned>>,
    /// Whether the walk reads code whose stored role is Test. When it does
    /// not, a test is never a member, a caller or a place that could add
    /// one: no scan reads it, the closure does not follow it, no row names
    /// it, and the block counts what it left out under
    /// [`TESTS_EXCLUDED_KEY`] instead of grading it.
    include_tests: bool,
}

impl Default for Scans {
    fn default() -> Self {
        Self::reading_tests(true)
    }
}

impl Scans {
    /// The scans of a walk that reads test code, or leaves it out.
    pub fn reading_tests(include_tests: bool) -> Self {
        Self {
            entries: HashMap::new(),
            include_tests,
        }
    }

    /// Whether the walk these scans belong to reads test code.
    pub fn reads_tests(&self) -> bool {
        self.include_tests
    }

    /// Whether `id` was scanned, well or not.
    pub fn contains(&self, id: &EntityId) -> bool {
        self.entries.contains_key(id)
    }

    /// Scan `entity` under `escape`, its census answer, reading text through
    /// `text`, and keep what both readers need.
    pub fn scan<G: GraphStore + ?Sized, T: SiteText + ?Sized>(
        &mut self,
        store: &G,
        entity: &Entity,
        escape: FocalEscape,
        text: &T,
    ) {
        let scanned =
            crate::call_sites::scan_focal_scoped(store, entity, text, escape, self.include_tests)
                .ok()
                .map(|scan| Scanned {
                    method: crate::trace_named_scope::MethodSummary::of(&scan),
                    reach: MemberReading::of(store, entity, &scan, text),
                });
        self.entries.insert(entity.id, scanned);
    }

    /// The named-scope summary of `id`, when it was scanned well.
    pub fn method(&self, id: &EntityId) -> Option<&crate::trace_named_scope::MethodSummary> {
        self.entries
            .get(id)?
            .as_ref()
            .map(|scanned| &scanned.method)
    }

    fn reach(&self, id: &EntityId) -> Option<Option<&MemberReading>> {
        self.entries
            .get(id)
            .map(|scanned| scanned.as_ref().map(|scanned| &scanned.reach))
    }
}

// ── The closure ───────────────────────────────────────────────────────────

struct Member {
    entity: Entity,
    distance: usize,
    via: EntityId,
    proof: Proof,
    path: Proof,
    /// Whether some hop of the witness path was matched by a bare name: the
    /// member is then a possible caller, listed apart from the proven ones.
    by_name: bool,
    site: SiteRef,
}

struct Ref {
    to: EntityId,
    how: &'static str,
    site: SiteRef,
}

struct Referrer {
    id: EntityId,
    refs: Vec<Ref>,
}

struct Bound {
    which: &'static str,
    limit: Option<usize>,
    at: Vec<EntityId>,
    left: usize,
    detail: String,
}

struct Closure<'s, G: GraphStore + ?Sized> {
    store: &'s G,
    focal: &'s Entity,
    limits: Limits,
    members: Vec<Member>,
    index: HashMap<EntityId, usize>,
    referrers: Vec<Referrer>,
    referrer_index: HashMap<EntityId, usize>,
    importers: HashSet<EntityId>,
    overrides: Vec<(EntityId, EntityId)>,
    bounds: Vec<Bound>,
    stopped: bool,
    reads: usize,
    /// When the reading started, for [`Limits::safety_time`] alone.
    started: Instant,
    /// Whether the reading takes code whose stored role is Test
    /// ([`Scans::reads_tests`]).
    include_tests: bool,
    /// Test callers a reading that excludes tests refused, each with the
    /// member it calls.
    tests_left_out: BTreeSet<(EntityId, EntityId)>,
    /// The scope the closure is confined to, when the walk names one.
    scope: Option<&'s TraceScope>,
    /// Callers outside the scope the closure did not follow.
    outside: HashSet<EntityId>,
    /// Callers whose file has no module record, each with the member it
    /// calls: whether they are in scope is unknown, so each is a frontier
    /// place rather than a member or a silent drop.
    unplaced: Vec<(EntityId, Open)>,
    unplaced_ids: HashSet<EntityId>,
}

/// A candidate hop found while expanding one layer: the best one a caller
/// was offered so far.
struct Candidate {
    via: EntityId,
    proof: Proof,
    path: Proof,
    by_name: bool,
    relations: Vec<kin_model::Relation>,
}

impl Candidate {
    /// How well a hop is established, best last: a path no hop of which was
    /// matched by name alone, then the strongest weakest link, then the
    /// strongest hop.
    fn standing(&self) -> (bool, Proof, Proof) {
        (!self.by_name, self.path, self.proof)
    }
}

impl<'s, G: GraphStore + ?Sized> Closure<'s, G> {
    fn path_of(&self, id: &EntityId) -> Proof {
        if *id == self.focal.id {
            return Proof::TypeResolved;
        }
        self.index
            .get(id)
            .map_or(Proof::TypeResolved, |at| self.members[*at].path)
    }

    /// Whether the witness path of `id` holds a hop matched by name alone.
    fn by_name_of(&self, id: &EntityId) -> bool {
        self.index
            .get(id)
            .is_some_and(|at| self.members[*at].by_name)
    }

    fn distance_of(&self, id: &EntityId) -> usize {
        self.index
            .get(id)
            .map_or(0, |at| self.members[*at].distance)
    }

    fn is_in(&self, id: &EntityId) -> bool {
        *id == self.focal.id || self.index.contains_key(id)
    }

    /// The scope when it restricts the closure's paths.
    fn confining(&self) -> Option<&'s TraceScope> {
        self.scope.filter(|scope| scope.restricts_paths())
    }

    /// Whether the closure may follow `caller`, which calls `to` at `site`.
    /// Under a confining scope a caller outside it is counted and not
    /// followed, and one that cannot be placed is a frontier place.
    fn follows(&mut self, caller: &Entity, to: EntityId, site: impl FnOnce() -> SiteRef) -> bool {
        let Some(scope) = self.confining() else {
            return true;
        };
        if self.is_in(&caller.id) {
            return true;
        }
        match scope.place(caller) {
            Placement::Inside => true,
            Placement::Outside => {
                scope.record_outside(caller);
                self.outside.insert(caller.id);
                false
            }
            Placement::Unplaced => {
                if self.unplaced_ids.insert(caller.id) {
                    let open = unplaced_open(caller, &self.name_of(&to), scope.module(), site());
                    self.unplaced.push((to, open));
                }
                false
            }
        }
    }

    fn name_of(&self, id: &EntityId) -> String {
        if *id == self.focal.id {
            return self.focal.name.clone();
        }
        self.index
            .get(id)
            .map(|at| self.members[*at].entity.name.clone())
            .unwrap_or_default()
    }

    /// The safety limit's name once the reading has run past it.
    fn past_safety_time(&self) -> Option<&'static str> {
        (self.started.elapsed() >= self.limits.safety_time).then_some(SAFETY_TIME_LIMIT)
    }

    /// What stopped the work, in the words a bound's detail leads with.
    fn stopped_by(&self, which: &str) -> String {
        if which == SAFETY_TIME_LIMIT {
            format!(
                "Kin stopped this reading after {} seconds, a safety limit far above what it \
                 normally takes; asking again usually completes it. It stopped",
                self.limits.safety_time.as_secs()
            )
        } else {
            format!("the walk's {} ran out", which.replace('_', " "))
        }
    }

    fn stop_work(&mut self, which: &'static str, left: &[EntityId]) {
        self.stopped = true;
        self.bounds.push(Bound {
            which,
            limit: None,
            at: left.iter().take(BOUND_AT_MAX).copied().collect(),
            left: left.len(),
            detail: format!(
                "{} with {} {} not yet expanded, so callers beyond them are not in members",
                self.stopped_by(which),
                left.len(),
                if left.len() == 1 {
                    "entity"
                } else {
                    "entities"
                }
            ),
        });
    }

    fn refer(&mut self, from: EntityId, to: EntityId, how: &'static str, site: SiteRef) {
        let at = *self.referrer_index.entry(from).or_insert_with(|| {
            self.referrers.push(Referrer {
                id: from,
                refs: Vec::new(),
            });
            self.referrers.len() - 1
        });
        self.referrers[at].refs.push(Ref { to, how, site });
    }

    /// Expand every node of `layer`, and admit each new caller once, by its
    /// best hop: the strongest weakest link, then the earliest node.
    fn expand<M: ReachMeter + ?Sized>(
        &mut self,
        layer: &[EntityId],
        meter: &mut M,
    ) -> Vec<EntityId> {
        let mut offers: HashMap<EntityId, Candidate> = HashMap::new();
        let mut order: Vec<EntityId> = Vec::new();
        for (position, node) in layer.iter().enumerate() {
            if let Some(which) = meter.stop().or_else(|| self.past_safety_time()) {
                self.stop_work(which, &layer[position..]);
                break;
            }
            let relations = match kin_index::relation_read::relations_for_read(self.store, node) {
                Ok(relations) => relations,
                Err(_) => {
                    self.bounds.push(Bound {
                        which: "graph_read",
                        limit: None,
                        at: vec![*node],
                        left: 1,
                        detail: format!(
                            "the graph could not list the relations of {}, so its callers are \
                             not in members",
                            self.name_of(node)
                        ),
                    });
                    continue;
                }
            };
            let node_path = self.path_of(node);
            let node_by_name = self.by_name_of(node);
            for relation in &relations {
                if let Some(which) = meter.charge_edge() {
                    self.stop_work(which, &layer[position..]);
                    break;
                }
                let (Some(src), dst) = (relation.src.as_entity(), relation.dst.as_entity()) else {
                    continue;
                };
                if relation.kind == RelationKind::Overrides && src == *node {
                    if let Some(base) = dst.filter(|base| base != node) {
                        self.overrides.push((*node, base));
                    }
                    continue;
                }
                if dst != Some(*node) || src == *node {
                    continue;
                }
                match relation.kind {
                    RelationKind::Calls => {
                        // A caller the scope already ruled on is not loaded
                        // again for each of its edges.
                        if self.outside.contains(&src) || self.unplaced_ids.contains(&src) {
                            continue;
                        }
                        if self.confining().is_some()
                            && !self.is_in(&src)
                            && !offers.contains_key(&src)
                        {
                            let Ok(Some(caller)) = self.store.get_entity(&src) else {
                                continue;
                            };
                            let file = caller.file_origin.clone();
                            if !self
                                .follows(&caller, *node, || relation_site(relation, file.as_ref()))
                            {
                                continue;
                            }
                        }
                        let proof = Proof::of(RelationResolution::of(relation));
                        let offered = Candidate {
                            via: *node,
                            proof,
                            path: node_path.min(proof),
                            by_name: node_by_name || proof == Proof::NameOnly,
                            relations: vec![relation.clone()],
                        };
                        match offers.get_mut(&src) {
                            Some(best) if best.via == *node => {
                                if proof > best.proof {
                                    best.proof = proof;
                                    best.path = offered.path;
                                    best.by_name = offered.by_name;
                                }
                                best.relations.push(relation.clone());
                            }
                            Some(best) => {
                                if offered.standing() > best.standing() {
                                    *best = offered;
                                }
                            }
                            None => {
                                order.push(src);
                                offers.insert(src, offered);
                            }
                        }
                    }
                    RelationKind::References
                    | RelationKind::UsesType
                    | RelationKind::Instantiates => {
                        let how = match relation.kind {
                            RelationKind::UsesType => "type",
                            RelationKind::Instantiates => "instantiates",
                            _ => "reference",
                        };
                        // The site is quoted from the referrer, which is loaded
                        // only if it is served.
                        self.refer(src, *node, how, relation_site(relation, None));
                    }
                    RelationKind::Imports => {
                        self.importers.insert(src);
                    }
                    _ => {}
                }
            }
            if self.stopped {
                break;
            }
        }
        let mut admitted = Vec::new();
        let mut fresh: Vec<(EntityId, Candidate)> = order
            .into_iter()
            .filter(|id| !self.is_in(id))
            .filter_map(|id| offers.remove(&id).map(|offer| (id, offer)))
            .collect();
        // Stronger paths first, so a cap keeps the members best proven.
        fresh.sort_by(|(left_id, left), (right_id, right)| {
            right
                .standing()
                .cmp(&left.standing())
                .then_with(|| left_id.cmp(right_id))
        });
        let mut dropped: Vec<EntityId> = Vec::new();
        for (id, offer) in fresh {
            if self.members.len() >= self.limits.members_max {
                if !dropped.contains(&offer.via) {
                    dropped.push(offer.via);
                }
                continue;
            }
            let Ok(Some(entity)) = self.store.get_entity(&id) else {
                continue;
            };
            let site = offer
                .relations
                .iter()
                .map(|relation| relation_site(relation, entity.file_origin.as_ref()))
                .filter(|site| site.line.is_some())
                .min_by_key(|site| (site.line, site.byte))
                .unwrap_or_default();
            if self.admit(
                entity,
                offer.via,
                offer.proof,
                offer.path,
                offer.by_name,
                site,
            ) {
                admitted.push(id);
            }
        }
        if !dropped.is_empty() {
            self.cap_bound(&dropped);
        }
        admitted
    }

    fn cap_bound(&mut self, at: &[EntityId]) {
        self.stopped = true;
        if self.bounds.iter().any(|bound| bound.which == "members_max") {
            return;
        }
        self.bounds.push(Bound {
            which: "members_max",
            limit: Some(self.limits.members_max),
            at: at.iter().take(BOUND_AT_MAX).copied().collect(),
            left: at.len(),
            detail: format!(
                "the closure holds at most {} members, so callers of the entities named here \
                 past that count are not listed",
                self.limits.members_max
            ),
        });
    }

    fn admit(
        &mut self,
        entity: Entity,
        via: EntityId,
        proof: Proof,
        path: Proof,
        by_name: bool,
        site: SiteRef,
    ) -> bool {
        if self.is_in(&entity.id) {
            return false;
        }
        // A test is no member of a reading that leaves tests out, so nothing
        // that calls it is followed either.
        if !self.include_tests && entity.role == EntityRole::Test {
            self.tests_left_out.insert((via, entity.id));
            return false;
        }
        let distance = self.distance_of(&via) + 1;
        self.index.insert(entity.id, self.members.len());
        self.members.push(Member {
            entity,
            distance,
            via,
            proof,
            path,
            by_name,
            site,
        });
        true
    }

    /// Entities whose unresolved calls the frontier reads: the focal, then
    /// every callable member, nearest first.
    fn readable(&self) -> Vec<&Entity> {
        std::iter::once(self.focal)
            .chain(self.members.iter().map(|member| &member.entity))
            .filter(|entity| matches!(entity.kind, EntityKind::Function | EntityKind::Method))
            .collect()
    }

    /// Scan every readable entity not scanned yet, within the read bound and
    /// the budget, and admit what their scans offer. Returns the admitted.
    fn read_round<M, E, S, T>(
        &mut self,
        meter: &mut M,
        scans: &mut Scans,
        census: &E,
        scan_text: &S,
    ) -> Vec<EntityId>
    where
        M: ReachMeter + ?Sized,
        E: Fn(&[(&Entity, Vec<String>)]) -> Vec<FocalEscape> + ?Sized,
        S: Fn() -> T + ?Sized,
        T: SiteText,
    {
        let mut pending: Vec<Entity> = self
            .readable()
            .into_iter()
            .filter(|entity| !scans.contains(&entity.id))
            .cloned()
            .collect();
        // Code that is not a test is read first, each kind still nearest
        // first. When the read bound cuts, it cuts among the tests, so the
        // packages a question is most often about are read whole: on Flask
        // the bound of 24 fell on 102 of 126 members in an order that left
        // the seven callers in `flask` itself unread.
        pending.sort_by_key(|entity| entity.role == EntityRole::Test);
        let room = self.limits.frontier_reads_max.saturating_sub(self.reads);
        let (now, later) = pending.split_at(pending.len().min(room));
        if !later.is_empty() {
            let ids: Vec<EntityId> = later.iter().map(|entity| entity.id).collect();
            self.stopped = true;
            self.bounds.push(Bound {
                which: "frontier_reads_max",
                limit: Some(self.limits.frontier_reads_max),
                at: ids.iter().take(BOUND_AT_MAX).copied().collect(),
                left: ids.len(),
                detail: format!(
                    "the closure reads the unresolved calls of at most {} members, so {} more \
                     were not read and calls that could reach them are not in frontier",
                    self.limits.frontier_reads_max,
                    ids.len()
                ),
            });
        }
        if !now.is_empty() {
            let focals: Vec<(&Entity, Vec<String>)> = now
                .iter()
                .map(|entity| (entity, kin_model::focal_call_names(entity)))
                .collect();
            let escapes = census(&focals);
            let mut escapes = escapes.into_iter();
            for (position, entity) in now.iter().enumerate() {
                if let Some(which) = meter.stop().or_else(|| self.past_safety_time()) {
                    let left: Vec<EntityId> = now[position..].iter().map(|e| e.id).collect();
                    self.stopped = true;
                    self.bounds.push(Bound {
                        which,
                        limit: None,
                        at: left.iter().take(BOUND_AT_MAX).copied().collect(),
                        left: left.len(),
                        detail: format!(
                            "{} before the unresolved calls of {} {} were read, so calls that \
                             could reach them are not in frontier",
                            self.stopped_by(which),
                            left.len(),
                            if left.len() == 1 { "member" } else { "members" }
                        ),
                    });
                    break;
                }
                let escape = escapes.next().unwrap_or(crate::call_sites::NO_CENSUS);
                scans.scan(self.store, entity, escape, &scan_text());
                self.reads += 1;
            }
        }
        // What every scan offers, in reading order, now that the members it
        // could name are known.
        let mut admitted = Vec::new();
        let readable: Vec<EntityId> = self.readable().iter().map(|entity| entity.id).collect();
        for id in readable {
            let Some(Some(reading)) = scans.reach(&id) else {
                continue;
            };
            let offers = reading.offers.clone();
            for offer in offers {
                if self.members.len() >= self.limits.members_max {
                    if !self.is_in(&offer.caller.id) {
                        self.cap_bound(&[id]);
                    }
                    continue;
                }
                let site = offer.site.clone();
                if !self.follows(&offer.caller, id, || site) {
                    continue;
                }
                let path = self.path_of(&id).min(offer.proof);
                let by_name = self.by_name_of(&id);
                let caller = offer.caller.id;
                if self.admit(offer.caller, id, offer.proof, path, by_name, offer.site) {
                    admitted.push(caller);
                }
            }
        }
        admitted
    }
}

/// Read the reach of `focal`'s callers, sharing `scans` with the other
/// readers of the walk, and return the `reach` block.
pub fn read<G: GraphStore, M: ReachMeter + ?Sized>(
    store: &G,
    held: &HeldSourceAuthority<'_, G>,
    scope: EntitySourceScope,
    focal: &Entity,
    limits: Limits,
    meter: &mut M,
    scans: &mut Scans,
) -> Value {
    read_in_scope(store, held, scope, focal, limits, meter, scans, None)
}

/// [`read`] confined to `within`, a semantic scope the walk names: callers
/// outside it are not followed (or, under `rows`, not listed), and the block
/// says so under `scope`. `None` reads exactly as [`read`].
#[allow(clippy::too_many_arguments)]
pub fn read_in_scope<G: GraphStore, M: ReachMeter + ?Sized>(
    store: &G,
    held: &HeldSourceAuthority<'_, G>,
    scope: EntitySourceScope,
    focal: &Entity,
    limits: Limits,
    meter: &mut M,
    scans: &mut Scans,
    within: Option<&TraceScope>,
) -> Value {
    let code = CalleeText::with_read_limit_at(held, CODE_READS_MAX, scope);
    let include_tests = scans.reads_tests();
    read_with_in_scope(
        store,
        &|focals: &[(&Entity, Vec<String>)]| {
            held.escape_evidence_batch_scoped(scope, focals, include_tests)
        },
        &|| CalleeText::with_read_limit_at(held, crate::call_sites::SCAN_TEXT_READS_MAX, scope),
        &code,
        focal,
        limits,
        meter,
        scans,
        within,
    )
}

/// [`read`] over explicit readers: `census` answers the escape census for a
/// batch, `scan_text` makes the text reader each scan reads through, and
/// `code` quotes the sites the block serves.
#[allow(clippy::too_many_arguments)]
pub fn read_with<G, M, E, S, T, C>(
    store: &G,
    census: &E,
    scan_text: &S,
    code: &C,
    focal: &Entity,
    limits: Limits,
    meter: &mut M,
    scans: &mut Scans,
) -> Value
where
    G: GraphStore + ?Sized,
    M: ReachMeter + ?Sized,
    E: Fn(&[(&Entity, Vec<String>)]) -> Vec<FocalEscape> + ?Sized,
    S: Fn() -> T + ?Sized,
    T: SiteText,
    C: SiteText + ?Sized,
{
    read_with_in_scope(
        store, census, scan_text, code, focal, limits, meter, scans, None,
    )
}

/// [`read_with`] confined to `within`, as [`read_in_scope`] is.
#[allow(clippy::too_many_arguments)]
pub fn read_with_in_scope<G, M, E, S, T, C>(
    store: &G,
    census: &E,
    scan_text: &S,
    code: &C,
    focal: &Entity,
    limits: Limits,
    meter: &mut M,
    scans: &mut Scans,
    within: Option<&TraceScope>,
) -> Value
where
    G: GraphStore + ?Sized,
    M: ReachMeter + ?Sized,
    E: Fn(&[(&Entity, Vec<String>)]) -> Vec<FocalEscape> + ?Sized,
    S: Fn() -> T + ?Sized,
    T: SiteText,
    C: SiteText + ?Sized,
{
    let mut closure = Closure {
        store,
        focal,
        limits,
        members: Vec::new(),
        index: HashMap::new(),
        referrers: Vec::new(),
        referrer_index: HashMap::new(),
        importers: HashSet::new(),
        overrides: Vec::new(),
        bounds: Vec::new(),
        stopped: false,
        reads: 0,
        started: Instant::now(),
        include_tests: scans.reads_tests(),
        tests_left_out: BTreeSet::new(),
        scope: within,
        outside: HashSet::new(),
        unplaced: Vec::new(),
        unplaced_ids: HashSet::new(),
    };
    let mut layer = vec![focal.id];
    loop {
        while !layer.is_empty() && !closure.stopped {
            layer = closure.expand(&layer, meter);
        }
        if closure.stopped {
            break;
        }
        layer = closure.read_round(meter, scans, census, scan_text);
        if layer.is_empty() {
            break;
        }
    }
    render(&closure, scans, code)
}

/// A caller the closure did not follow because its file has no module
/// record, as a frontier place: whether it is in `scope` is unknown.
fn unplaced_open(caller: &Entity, to_name: &str, scope: &str, site: SiteRef) -> Open {
    Open {
        kind: "unplaced",
        within: Some((caller.id, caller.name.clone())),
        file: caller.file_origin.as_ref().map(|path| path.0.clone()),
        site,
        why: format!(
            "`{}` calls `{to_name}`, but its file has no module record, so whether it is in \
             {scope} is unknown and its callers were not followed",
            caller.name
        ),
        fold: None,
    }
}

// ── The block ─────────────────────────────────────────────────────────────

fn entity_head(entity: &Entity) -> Map<String, Value> {
    let mut row = Map::new();
    row.insert("id".into(), json!(entity.id.to_string()));
    row.insert("name".into(), json!(entity.name));
    row.insert("kind".into(), json!(format!("{:?}", entity.kind)));
    row.insert(
        "file".into(),
        json!(entity.file_origin.as_ref().map(|path| path.0.clone())),
    );
    if entity.role != EntityRole::Source {
        row.insert(
            "role".into(),
            json!(format!("{:?}", entity.role).to_lowercase()),
        );
    }
    row
}

fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 {
        one
    } else {
        many
    }
}

/// An entity that names a member without calling it, with each site, one
/// per member and line.
struct Holder {
    entity: Entity,
    refs: Vec<HeldRef>,
}

#[derive(Clone)]
struct HeldRef {
    to: EntityId,
    how: &'static str,
    value: Option<&'static str>,
    site: SiteRef,
}

/// How strongly a kind of reference says what the site does, strongest first.
fn how_rank(how: &str) -> u8 {
    match how {
        "reference" => 0,
        "instantiates" => 1,
        "value" => 2,
        _ => 3,
    }
}

impl Holder {
    /// Record a site, merged into the one already recorded for the same
    /// member on the same line: the strongest kind, the value use the census
    /// names, and the first quote any record gave.
    fn add(&mut self, to: EntityId, how: &'static str, value: Option<&'static str>, site: SiteRef) {
        match self
            .refs
            .iter_mut()
            .find(|reference| reference.to == to && reference.site.line == site.line)
        {
            Some(existing) => {
                if how_rank(how) < how_rank(existing.how) {
                    existing.how = how;
                }
                existing.value = existing.value.or(value);
                if existing.site.code.is_none() {
                    existing.site.code = site.code;
                }
                if existing.site.byte.is_none() {
                    existing.site.byte = site.byte;
                }
            }
            None => self.refs.push(HeldRef {
                to,
                how,
                value,
                site,
            }),
        }
    }

    /// Name the value use at `line` on a site an edge already recorded there.
    /// Whether any recorded site took it.
    fn annotate(
        &mut self,
        to: EntityId,
        line: u32,
        context: &'static str,
        code: Option<&str>,
    ) -> bool {
        let mut taken = false;
        for reference in self
            .refs
            .iter_mut()
            .filter(|reference| reference.to == to && reference.site.line == Some(line))
        {
            reference.value = reference.value.or(Some(context));
            if reference.site.code.is_none() {
                reference.site.code = code.map(str::to_string);
            }
            taken = true;
        }
        taken
    }
}

/// What makes two frontier places one row: the same kind of place, inside
/// the same entity, at the same recorded site, that could add callers to the
/// same member for the same reason.
#[derive(PartialEq, Eq, Hash)]
struct PlaceKey {
    kind: &'static str,
    to: EntityId,
    within: Option<EntityId>,
    file: Option<String>,
    line: Option<u32>,
    byte: Option<usize>,
    code: Option<String>,
    why: String,
}

impl PlaceKey {
    fn of(to: EntityId, open: &Open) -> Self {
        Self {
            kind: open.kind,
            to,
            within: open.within.as_ref().map(|(id, _)| *id),
            file: open.file.clone(),
            line: open.site.line,
            byte: open.site.byte,
            code: open.site.code.clone(),
            why: open.why.clone(),
        }
    }
}

/// The frontier as the block lists it: one row per distinct place, each
/// with how many recorded sites it stands for when more than one, up to
/// [`FRONTIER_ROWS_MAX`] rows, and the places past that counted by the
/// member each could add callers to.
#[derive(Default)]
struct Frontier {
    rows: Vec<Value>,
    /// Distinct places, listed or not.
    total: usize,
    withheld: Vec<(EntityId, usize)>,
    listed: HashMap<PlaceKey, usize>,
    counted: HashSet<PlaceKey>,
    /// Listed rows whose place sits in code whose stored role is Test.
    in_tests: usize,
    /// Where each distinct place sits, listed or not: the entity it is in
    /// and its file, as far as its record says.
    places: Vec<Located>,
    /// Places counted without a record of where they sit.
    unlocated: usize,
    /// Places in test code a reading that excludes tests left out, each
    /// with the member it could add a caller to.
    tests_left_out: BTreeSet<(EntityId, EntityId)>,
}

/// Where a place that could add a caller sits.
type Located = (Option<EntityId>, Option<String>);

impl Frontier {
    /// Places counted without a row, such as residue past its sample.
    fn count(&mut self, to: EntityId, places: usize) {
        if places == 0 {
            return;
        }
        self.total += places;
        self.unlocated += places;
        self.withhold(to, places);
    }

    fn withhold(&mut self, to: EntityId, places: usize) {
        match self.withheld.iter_mut().find(|(id, _)| *id == to) {
            Some((_, count)) => *count += places,
            None => self.withheld.push((to, places)),
        }
    }

    /// Add `open`, a place that could add callers to `to`. A place already
    /// listed adds a site to its row; one already counted adds nothing.
    fn push<G: GraphStore + ?Sized, C: SiteText + ?Sized>(
        &mut self,
        closure: &Closure<'_, G>,
        quoter: &Quoter<'_, C>,
        to: EntityId,
        open: &Open,
    ) {
        let key = PlaceKey::of(to, open);
        if let Some(at) = self.listed.get(&key) {
            let row = &mut self.rows[*at];
            let sites = row.get("sites").and_then(Value::as_u64).unwrap_or(1) + 1;
            row["sites"] = json!(sites);
            return;
        }
        if self.counted.contains(&key) {
            return;
        }
        let holder = open
            .within
            .as_ref()
            .and_then(|(id, _)| closure.store.get_entity(id).ok().flatten());
        // A place in test code is not a place of a reading that leaves tests
        // out: it is counted as left out and never listed or graded.
        if !closure.include_tests {
            if let Some(test) = holder
                .as_ref()
                .filter(|holder| holder.role == EntityRole::Test)
            {
                self.tests_left_out.insert((to, test.id));
                return;
            }
        }
        self.total += 1;
        self.places
            .push((open.within.as_ref().map(|(id, _)| *id), open.file.clone()));
        if self.rows.len() >= FRONTIER_ROWS_MAX {
            self.counted.insert(key);
            self.withhold(to, 1);
            return;
        }
        let (line, text) = match &holder {
            Some(holder) => quoter.quote(holder, &open.site),
            None => (open.site.line, open.site.code.as_deref().map(clip)),
        };
        let mut row = json!({
            "kind": open.kind,
            "to": to.to_string(),
            "to_name": closure.name_of(&to),
            "in": open.within.as_ref().map(|(id, _)| id.to_string()),
            "in_name": open.within.as_ref().map(|(_, name)| name.clone()),
            "file": open.file,
            "line": line,
            "code": text,
            "why": open.why,
        });
        // The stored role of the code the place sits in, when it is not
        // source: a reader whose question leaves tests out can set aside a
        // place in test code without reading it.
        if let Some(role) = holder
            .as_ref()
            .map(|holder| holder.role)
            .filter(|role| *role != EntityRole::Source)
        {
            row["in_role"] = json!(format!("{role:?}").to_lowercase());
            if role == EntityRole::Test {
                self.in_tests += 1;
            }
        }
        self.listed.insert(key, self.rows.len());
        self.rows.push(row);
    }
}

/// Where the census listed a member held as a value, read against the rows
/// the block serves: calls through a value that spell no member's name can
/// run a member only through a value taken at one of these places.
#[derive(Default)]
struct ValueHolders {
    /// `also_referenced_by` rows that hold a member as a value.
    rows: usize,
    /// Members (or the focal) that themselves hold a member as a value, each
    /// with the member held and the line.
    in_members: Vec<(EntityId, EntityId, u32)>,
    /// Places the census listed that no served row names: past a sample, past
    /// the referrer rows, outside a scope, or in no entity the graph holds.
    unlisted: usize,
}

/// The holder row for `id`, added when the graph holds the entity.
fn holder_for<G: GraphStore + ?Sized>(
    store: &G,
    holders: &mut Vec<Holder>,
    at_of: &mut HashMap<EntityId, usize>,
    id: EntityId,
) -> Option<usize> {
    if let Some(at) = at_of.get(&id) {
        return Some(*at);
    }
    let entity = store.get_entity(&id).ok().flatten()?;
    holders.push(Holder {
        entity,
        refs: Vec::new(),
    });
    at_of.insert(id, holders.len() - 1);
    Some(holders.len() - 1)
}

fn render<G: GraphStore + ?Sized, C: SiteText + ?Sized>(
    closure: &Closure<'_, G>,
    scans: &Scans,
    code: &C,
) -> Value {
    let quoter = Quoter::new(code);
    let focal = closure.focal;

    // Nearest first, then in file order, so a reader scans them the way the
    // code reads.
    let mut ordered: Vec<&Member> = closure.members.iter().collect();
    ordered.sort_by(|left, right| {
        let place = |member: &Member| {
            (
                member.distance,
                member
                    .entity
                    .file_origin
                    .as_ref()
                    .map(|path| path.0.clone()),
                member.entity.span.as_ref().map(|span| span.start_line),
                member.entity.name.clone(),
                member.entity.id,
            )
        };
        place(left).cmp(&place(right))
    });
    // Each member names the dotted module it is declared in, read once per
    // module from the graph's module records, so a reader can place a member
    // in its package without reading it. Never a file path: a member whose
    // module the records cannot name carries none.
    let modules: RefCell<HashMap<String, Option<String>>> = RefCell::default();
    let module_of = |entity: &Entity| -> Option<String> {
        let file = entity.file_origin.as_ref()?.0.clone();
        if let Some(known) = modules.borrow().get(&file) {
            return known.clone();
        }
        let named = crate::semantic_owners::entity_module(closure.store, entity);
        modules.borrow_mut().insert(file, named.clone());
        named
    };
    let member_row = |member: &Member| {
        let (line, text) = quoter.quote(&member.entity, &member.site);
        let mut row = entity_head(&member.entity);
        if let Some(module) = module_of(&member.entity) {
            row.insert("module".into(), json!(module));
        }
        if crate::semantic_owners::name_is_shared(closure.store, &member.entity) {
            row.insert(crate::semantic_owners::NAME_SHARED_KEY.into(), json!(true));
        }
        row.insert("distance".into(), json!(member.distance));
        row.insert("via".into(), json!(member.via.to_string()));
        row.insert("proof".into(), json!(member.proof.wire()));
        row.insert("proven_path".into(), json!(member.path.proven()));
        row.insert("line".into(), json!(line));
        row.insert("code".into(), json!(text));
        Value::Object(row)
    };
    // A scope lists only the members inside it. Under `paths` the closure
    // followed no other; under `rows` it walked them, and the ones a listed
    // member's witness path runs through are kept apart so every `via` can
    // still be followed to the focal.
    let scope = closure.scope;
    let placed = |entity: &Entity| scope.map_or(Placement::Inside, |scope| scope.place(entity));
    let (mut listed, unlisted): (Vec<&Member>, Vec<&Member>) = ordered
        .into_iter()
        .partition(|member| placed(&member.entity) == Placement::Inside);
    // Members by package: the module path one segment below the prefix all
    // of them share, as the module census counts a walk's steps. They are
    // listed package by package, the focal's own first, then the others
    // whose members are not all tests, then the tests, each still nearest
    // first. A reply that cannot hold every member then holds the packages
    // a question is most often about, and [`PACKAGES_KEY`] says how far
    // each one was read.
    let member_modules: Vec<(EntityId, Option<String>)> = listed
        .iter()
        .map(|member| (member.entity.id, module_of(&member.entity)))
        .collect();
    let shared = shared_segments(
        member_modules
            .iter()
            .filter_map(|(_, module)| module.as_deref()),
    );
    let member_package: HashMap<EntityId, Option<String>> = member_modules
        .into_iter()
        .map(|(id, module)| (id, module.map(|module| package_of(&module, shared))))
        .collect();
    let focal_package = module_of(focal).map(|module| package_of(&module, shared));
    let mut tests_only: HashMap<Option<String>, bool> = HashMap::new();
    for member in &listed {
        let all = tests_only
            .entry(member_package[&member.entity.id].clone())
            .or_insert(true);
        *all &= member.entity.role == EntityRole::Test;
    }
    let package_rank = |package: &Option<String>| {
        (
            package.is_none(),
            *package != focal_package,
            tests_only.get(package).copied().unwrap_or(false),
            package.clone(),
        )
    };
    listed.sort_by_cached_key(|member| package_rank(&member_package[&member.entity.id]));
    // A member some hop reached by a bare name is a possible caller: it is
    // listed apart, so `members` holds only what a resolver or a ledger
    // established.
    let members: Vec<Value> = listed
        .iter()
        .filter(|member| !member.by_name)
        .map(|member| member_row(member))
        .collect();
    let name_only: Vec<Value> = listed
        .iter()
        .filter(|member| member.by_name)
        .map(|member| member_row(member))
        .collect();
    let mut via_outside: Vec<Value> = Vec::new();
    if !unlisted.is_empty() {
        let mut witnesses: HashSet<EntityId> = HashSet::new();
        for member in &listed {
            // Each `via` is nearer the focal than the member naming it, so a
            // witness path ends; the bound only guards a malformed record.
            let mut via = member.via;
            let mut hops = 0usize;
            while via != focal.id && hops <= closure.members.len() {
                hops += 1;
                let Some(at) = closure.index.get(&via) else {
                    break;
                };
                let hop = &closure.members[*at];
                if placed(&hop.entity) != Placement::Inside {
                    witnesses.insert(via);
                }
                via = hop.via;
            }
        }
        via_outside = unlisted
            .iter()
            .filter(|member| witnesses.contains(&member.entity.id))
            .map(|member| {
                let mut row = member_row(member);
                if let Some(module) = scope.and_then(|scope| scope.module_of(&member.entity)) {
                    row["module"] = json!(module);
                }
                row
            })
            .collect();
    }
    let mut outside_members = 0usize;
    for member in &unlisted {
        if let Some(scope) = scope.filter(|_| placed(&member.entity) == Placement::Outside) {
            scope.record_outside(&member.entity);
            outside_members += 1;
        }
    }

    // Frontier rows, in reading order: the focal, then members nearest first.
    // The same place recorded more than once is one row with its site count.
    let mut frontier = Frontier::default();
    let mut settled_by_membership = 0usize;
    let mut value_call_sites = 0u64;
    let mut unkeyed: BTreeSet<(String, u32)> = BTreeSet::new();
    let mut spelled_lookups: HashSet<(String, u32)> = HashSet::new();
    let mut value_uses: Vec<(EntityId, &ValueUse)> = Vec::new();
    let mut value_holders = ValueHolders::default();
    let mut uncensused: Vec<Value> = Vec::new();
    let mut frontier_outside = 0usize;
    // Count observations across members, not distinct graph sites. Unread
    // text remains uncertainty even when no frontier row lists it. Paths
    // exclude known outside callers before this count; unplaced ones remain.
    let mut folded: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut folded_places: Vec<Located> = Vec::new();
    let readable: Vec<EntityId> = closure.readable().iter().map(|entity| entity.id).collect();
    for id in &readable {
        let Some(Some(reading)) = scans.reach(id) else {
            continue;
        };
        value_call_sites = value_call_sites.max(reading.value_call_sites);
        unkeyed.extend(reading.unkeyed.iter().cloned());
        if let Some(reason) = reading.uncensused {
            uncensused.push(json!({
                "id": id.to_string(),
                "name": closure.name_of(id),
                "why": reason,
            }));
        }
        frontier.count(*id, reading.unlisted);
        value_holders.unlisted += reading.values_unlisted;
        // A lookup whose key or text could spell this member's name is
        // checked against it, never counted as one that spells no name.
        spelled_lookups.extend(
            reading
                .opens
                .iter()
                .filter(|open| open.kind == "reflective")
                .filter_map(|open| Some((open.file.clone()?, open.site.line?))),
        );
        for open in &reading.opens {
            // A place inside an entity already in the closure cannot add to it:
            // whatever that call reaches, the entity holding it is a member.
            if open
                .within
                .as_ref()
                .is_some_and(|(within, _)| closure.is_in(within))
            {
                settled_by_membership += 1;
                continue;
            }
            // Under a confining scope, a place inside an entity outside it
            // cannot add a member: that entity would be a caller the closure
            // does not follow.
            if let Some(confining) = closure.confining() {
                if open.within.as_ref().is_some_and(|(within, _)| {
                    confining.place_id(closure.store, within) == Some(Placement::Outside)
                }) {
                    frontier_outside += 1;
                    continue;
                }
            }
            if let Some(reason) = open.fold {
                *folded.entry(reason).or_default() += 1;
                folded_places.push((open.within.as_ref().map(|(id, _)| *id), open.file.clone()));
                continue;
            }
            frontier.push(closure, &quoter, *id, open);
        }
        for value in &reading.values {
            value_uses.push((*id, value));
        }
    }
    // A method a member overrides: calls resolved to it can run the member.
    let mut seen_bases: HashSet<(EntityId, EntityId)> = HashSet::new();
    for (member, base) in &closure.overrides {
        if closure.is_in(base) || !seen_bases.insert((*member, *base)) {
            continue;
        }
        let Ok(Some(base_entity)) = closure.store.get_entity(base) else {
            continue;
        };
        let open = Open {
            kind: "dispatch",
            within: Some((base_entity.id, base_entity.name.clone())),
            file: base_entity.file_origin.as_ref().map(|path| path.0.clone()),
            site: SiteRef {
                line: base_entity.span.as_ref().map(|span| span.start_line + 1),
                byte: None,
                code: Some(base_entity.signature.clone()).filter(|text| !text.is_empty()),
            },
            why: format!(
                "`{}` overrides `{}`; a call Kin resolved to `{}` can run it at run time, and \
                 those calls are not followed",
                closure.name_of(member),
                base_entity.name,
                base_entity.name
            ),
            fold: None,
        };
        frontier.push(closure, &quoter, *member, &open);
    }

    // Callers no module record places, which a scope can neither follow nor
    // rule out: under `paths` the closure stopped at them, and under `rows`
    // it walked them but cannot say whether they are rows.
    for (to, open) in &closure.unplaced {
        frontier.push(closure, &quoter, *to, open);
    }
    if let Some(scope) = scope {
        for member in unlisted
            .iter()
            .filter(|member| placed(&member.entity) == Placement::Unplaced)
        {
            let open = unplaced_open(
                &member.entity,
                &closure.name_of(&member.via),
                scope.module(),
                member.site.clone(),
            );
            frontier.push(closure, &quoter, member.via, &open);
        }
    }

    // Referrers: graph references, then census value uses, each on the
    // entity that holds it.
    let mut holders: Vec<Holder> = Vec::new();
    let mut at_of: HashMap<EntityId, usize> = HashMap::new();
    for referrer in &closure.referrers {
        if closure.is_in(&referrer.id) {
            continue;
        }
        let Some(at) = holder_for(closure.store, &mut holders, &mut at_of, referrer.id) else {
            continue;
        };
        for reference in &referrer.refs {
            holders[at].add(reference.to, reference.how, None, reference.site.clone());
        }
    }
    // Each value use placed on a holder, kept to check below that a served
    // row names it: the member held, the holders that took it, its place.
    let mut held_on: Vec<(EntityId, Vec<EntityId>, &str, u32)> = Vec::new();
    for (to, value) in value_uses {
        let site = SiteRef {
            line: Some(value.line),
            byte: None,
            code: value.code.clone(),
        };
        match value.inside {
            Some(inside) if closure.is_in(&inside) => {
                value_holders.in_members.push((inside, to, value.line));
            }
            Some(inside) => match holder_for(closure.store, &mut holders, &mut at_of, inside) {
                Some(at) => {
                    holders[at].add(to, "value", Some(value.context), site);
                    held_on.push((to, vec![inside], &value.file, value.line));
                }
                None => value_holders.unlisted += 1,
            },
            // A use at a file's top level sits in no declaration; it names the
            // referrer an edge already recorded at the same line, when one did.
            None => {
                let took: Vec<EntityId> = holders
                    .iter_mut()
                    .filter(|holder| {
                        holder
                            .entity
                            .file_origin
                            .as_ref()
                            .is_some_and(|path| path.0 == value.file)
                    })
                    .filter_map(|holder| {
                        holder
                            .annotate(to, value.line, value.context, value.code.as_deref())
                            .then_some(holder.entity.id)
                    })
                    .collect();
                held_on.push((to, took, &value.file, value.line));
            }
        }
    }
    // A declaration that encloses another one restates that one's sites: a
    // class holds its methods' calls and definitions a second time. A site
    // inside a narrower entity the block already accounts for, as a member
    // or as a referrer of its own, is that entity's, not the container's.
    let claimed: Vec<&Entity> = std::iter::once(focal)
        .chain(closure.members.iter().map(|member| &member.entity))
        .chain(holders.iter().map(|holder| &holder.entity))
        .collect();
    let restated = |holder: &Entity, line: Option<u32>| -> bool {
        let (Some(outer), Some(line)) = (holder.span.as_ref(), line) else {
            return false;
        };
        let line = line.saturating_sub(1);
        claimed.iter().any(|inner| {
            inner.id != holder.id
                && inner.span.as_ref().is_some_and(|span| {
                    span.file == outer.file
                        && span.start_byte >= outer.start_byte
                        && span.end_byte <= outer.end_byte
                        && span.end_byte - span.start_byte < outer.end_byte - outer.start_byte
                        && span.start_line <= line
                        && line <= span.end_line
                })
        })
    };
    let kept: Vec<(Entity, Vec<HeldRef>)> = holders
        .iter()
        .map(|holder| {
            let mut refs: Vec<HeldRef> = holder
                .refs
                .iter()
                .filter(|reference| !restated(&holder.entity, reference.site.line))
                .cloned()
                .collect();
            refs.sort_by_key(|reference| (reference.site.line, reference.to));
            (holder.entity.clone(), refs)
        })
        .filter(|(_, refs)| !refs.is_empty())
        .collect();
    // A referrer in test code is no row of a reading that leaves tests out.
    let kept: Vec<(Entity, Vec<HeldRef>)> = kept
        .into_iter()
        .filter(|(entity, _)| closure.include_tests || entity.role != EntityRole::Test)
        .collect();
    let held_before = kept.len();
    let kept: Vec<(Entity, Vec<HeldRef>)> = kept
        .into_iter()
        .filter(|(entity, _)| placed(entity) == Placement::Inside)
        .collect();
    let referrers_outside = held_before - kept.len();
    let referrers_total = kept.len();
    // The value uses the served referrer rows name, and the rows that hold
    // a member as a value.
    let served_values: HashSet<(EntityId, EntityId, u32)> = kept
        .iter()
        .take(REFERRERS_MAX)
        .flat_map(|(entity, refs)| {
            refs.iter()
                .filter(|reference| reference.value.is_some())
                .filter_map(|reference| Some((entity.id, reference.to, reference.site.line?)))
        })
        .collect();
    value_holders.rows = kept
        .iter()
        .take(REFERRERS_MAX)
        .filter(|(_, refs)| refs.iter().any(|reference| reference.value.is_some()))
        .count();
    // The members each served row holds as a value, once per row.
    let held_by_rows: Vec<EntityId> = kept
        .iter()
        .take(REFERRERS_MAX)
        .flat_map(|(_, refs)| {
            refs.iter()
                .filter(|reference| reference.value.is_some())
                .map(|reference| reference.to)
                .collect::<BTreeSet<EntityId>>()
        })
        .collect();
    // A use no served row names is held inside a member when its line is,
    // and otherwise is a place the block does not list.
    let member_at = |file: &str, line: u32| -> Option<EntityId> {
        let line = line.checked_sub(1)?;
        std::iter::once(focal)
            .chain(closure.members.iter().map(|member| &member.entity))
            .filter_map(|entity| {
                let span = entity.span.as_ref()?;
                (span.file.0 == file && span.start_line <= line && line <= span.end_line)
                    .then_some((span.end_byte - span.start_byte, entity.id))
            })
            .min()
            .map(|(_, id)| id)
    };
    for (to, took, file, line) in held_on {
        if took
            .iter()
            .any(|holder| served_values.contains(&(*holder, to, line)))
        {
            continue;
        }
        match member_at(file, line) {
            Some(member) => value_holders.in_members.push((member, to, line)),
            None => value_holders.unlisted += 1,
        }
    }
    let also_referenced_by: Vec<Value> = kept
        .into_iter()
        .take(REFERRERS_MAX)
        .map(|(entity, refs)| {
            let mut row = entity_head(&entity);
            let refs: Vec<Value> = refs
                .into_iter()
                .map(|reference| {
                    let (line, text) = quoter.quote(&entity, &reference.site);
                    let mut served = json!({
                        "to": reference.to.to_string(),
                        "to_name": closure.name_of(&reference.to),
                        "how": reference.how,
                        "line": line,
                        "code": text,
                    });
                    if let Some(value) = reference.value {
                        served["value"] = json!(value);
                    }
                    served
                })
                .collect();
            row.insert("refs".into(), Value::Array(refs));
            Value::Object(row)
        })
        .collect();

    let bounds: Vec<Value> = closure
        .bounds
        .iter()
        .map(|bound| {
            json!({
                "which": bound.which,
                "limit": bound.limit,
                "left": bound.left,
                "at": bound.at.iter().map(|id| json!({
                    "id": id.to_string(),
                    "name": closure.name_of(id),
                })).collect::<Vec<_>>(),
                "detail": bound.detail,
            })
        })
        .collect();

    let folded_total: usize = folded.values().sum();
    let frontier_total = frontier.total;
    // A fold is never closure: a counted place could still hold a call, so
    // a walk whose only places are folded is open, and says why below.
    let state = if !closure.bounds.is_empty() {
        "bounded"
    } else if frontier_total > 0 || folded_total > 0 {
        "open"
    } else {
        "closed"
    };
    let count = members.len();
    let noun = plural(count, "entity", "entities");
    // Empty without a scope, so an unscoped block reads exactly as it did.
    let (in_scope, stays) = match scope {
        Some(scope) => (
            format!(" in {}", scope.module()),
            match scope.mode() {
                ScopeMode::Paths => format!(" on a path that stays in {}", scope.module()),
                ScopeMode::Rows => String::new(),
            },
        ),
        None => (String::new(), String::new()),
    };
    // What a reading without tests left out, by the member each test would
    // have called: the callers the walk refused, the ones each member's scan
    // skipped, and the places in test code the frontier did not take.
    let tests_excluded = (!closure.include_tests).then(|| {
        let mut left_out = closure.tests_left_out.clone();
        left_out.extend(frontier.tests_left_out.iter().copied());
        for entity in closure.readable() {
            if let Some(Some(reading)) = scans.reach(&entity.id) {
                left_out.extend(reading.tests_left_out.iter().map(|test| (entity.id, *test)));
            }
        }
        let callers: BTreeSet<EntityId> = left_out.iter().map(|(_, test)| *test).collect();
        (left_out.len(), callers.len())
    });
    let mut statement = match state {
        "closed" if count == 0 => format!(
            "Closed: under this policy nothing{in_scope} calls {}{stays}, and no call that \
             spells its name is left unresolved.",
            focal.name
        ),
        "closed" => format!(
            "Closed: under this policy these {count} {noun} are every one{in_scope} that can \
             end up calling {}{stays}, and no call to any of them is left unresolved.",
            focal.name
        ),
        "open" if frontier_total == 0 => format!(
            "Every entity{in_scope} that reaches {} through a followed call{stays} is in members \
             ({count}). No listed place could add a caller; frontier_folded counts the rest by \
             reason.",
            focal.name,
        ),
        "open" => format!(
            "Every entity{in_scope} that reaches {} through a followed call{stays} is in members \
             ({count}). {frontier_total} {} could still add callers; each is in frontier with \
             its code and why{}.",
            focal.name,
            plural(frontier_total, "place", "places"),
            match frontier.in_tests {
                0 => String::new(),
                tests if tests == frontier.rows.len() && tests == frontier_total => {
                    ", and every one sits in test code (in_role)".to_string()
                }
                tests => format!(", and {tests} of the listed rows sit in test code (in_role)"),
            },
        ),
        _ => format!(
            "Not exhaustive: {} The {count} {noun} in members are a floor.",
            closure
                .bounds
                .iter()
                .map(|bound| {
                    let mut detail = bound.detail.clone();
                    detail.push('.');
                    let mut chars = detail.chars();
                    match chars.next() {
                        Some(first) => first.to_uppercase().chain(chars).collect(),
                        None => detail,
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        ),
    };
    if let Some((call_sites, callers)) = tests_excluded {
        statement.push_str(&match call_sites {
            0 => {
                " Test code was left out, as asked; nothing in it calls or names these.".to_string()
            }
            _ => format!(
                " Test code was left out, as asked: {call_sites} {} in {callers} test {} not \
                 followed or listed.",
                plural(call_sites, "call site", "call sites"),
                plural(callers, "entity is", "entities are"),
            ),
        });
    }
    if !name_only.is_empty() {
        statement.push_str(&format!(
            " {} more {} only through a call matched by name alone, which no resolver tied to \
             its target; see {NAME_ONLY_KEY}.",
            name_only.len(),
            plural(name_only.len(), "entity reaches it", "entities reach it"),
        ));
    }
    if referrers_total > 0 {
        statement.push_str(&format!(
            " {referrers_total} more {} refer to them without calling them; see \
             also_referenced_by.",
            plural(referrers_total, "entity", "entities")
        ));
    }

    // Counted lookups spell no reached entity's name: one that could spell
    // any is a reflective place of that member, listed or settled above.
    unkeyed.retain(|site| !spelled_lookups.contains(site));
    let mut blind_spots = Map::new();
    if !unkeyed.is_empty() {
        blind_spots.insert("unkeyed_lookups".into(), json!(unkeyed.len()));
    }
    if value_call_sites > 0 {
        blind_spots.insert("calls_through_values".into(), json!(value_call_sites));
    }
    if !blind_spots.is_empty() {
        blind_spots.insert(
            "why".into(),
            json!(
                "counted, not listed: lookups by a name built at run time, and calls through a \
                 value that spell no member's name; any reader that does not run the program \
                 shares them, and where each member is held as a value is listed under \
                 also_referenced_by"
            ),
        );
    }
    // Every member read, none past a bound, each with a census: only then
    // can the places the census lists a member held as a value be all the
    // places a call through a value could have taken it from.
    // Every member read, none past a bound, and every caller that could spell
    // a member's call names read from a current ledger: the reach's own call
    // census measured what a language server's readiness stands in for.
    let calls_measured = matches!(focal.kind, EntityKind::Function | EntityKind::Method)
        && closure.bounds.is_empty()
        && readable
            .iter()
            .all(|id| matches!(scans.reach(id), Some(Some(reading)) if reading.named_complete));
    let all_censused = matches!(focal.kind, EntityKind::Function | EntityKind::Method)
        && closure.bounds.is_empty()
        && uncensused.is_empty()
        && readable
            .iter()
            .all(|id| matches!(scans.reach(id), Some(Some(_))));
    if value_call_sites > 0
        || value_holders.rows > 0
        || value_holders.unlisted > 0
        || !value_holders.in_members.is_empty()
    {
        let mut held = Map::new();
        held.insert("rows".into(), json!(value_holders.rows));
        if !value_holders.in_members.is_empty() {
            let mut seen: HashSet<(EntityId, EntityId, u32)> = HashSet::new();
            held.insert(
                "in_members".into(),
                Value::Array(
                    value_holders
                        .in_members
                        .iter()
                        .filter(|place| seen.insert(**place))
                        .map(|(member, to, line)| {
                            json!({
                                "id": member.to_string(),
                                "name": closure.name_of(member),
                                "to": to.to_string(),
                                "to_name": closure.name_of(to),
                                "line": line,
                            })
                        })
                        .collect(),
                ),
            );
        }
        if value_holders.unlisted > 0 {
            held.insert("unlisted".into(), json!(value_holders.unlisted));
        }
        held.insert(
            "complete".into(),
            json!(all_censused && value_holders.unlisted == 0),
        );
        held.insert("why".into(), json!(VALUE_HOLDERS_WHY));
        blind_spots.insert(VALUE_HOLDERS_KEY.into(), Value::Object(held));
    }
    if !uncensused.is_empty() {
        blind_spots.insert(
            "uncensused".into(),
            json!({
                "members": uncensused,
                "why": "no escape census was read for these, so places that hold them as a \
                        value or run them without a call expression are not listed",
            }),
        );
    }

    // How far each package was read. A place counts against the package it
    // sits in, and one no record places counts against every package.
    let package_of_place = |place: &Located| -> Option<String> {
        let module = match place {
            (Some(within), _) => closure
                .store
                .get_entity(within)
                .ok()
                .flatten()
                .and_then(|entity| module_of(&entity)),
            (None, Some(file)) => crate::semantic_owners::file_module(closure.store, file),
            (None, None) => None,
        };
        module.map(|module| package_of(&module, shared))
    };
    let read = |entity: &Entity| {
        !matches!(entity.kind, EntityKind::Function | EntityKind::Method)
            || matches!(scans.reach(&entity.id), Some(Some(_)))
    };
    let mut counts: HashMap<Option<String>, PackageCount> = HashMap::new();
    for member in &listed {
        let count = counts
            .entry(member_package[&member.entity.id].clone())
            .or_default();
        if member.by_name {
            count.name_only += 1;
        } else {
            count.members += 1;
        }
        count.unread += usize::from(!read(&member.entity));
    }
    let mut everywhere = PackageCount {
        unread: usize::from(!read(focal)),
        places: frontier.unlocated,
        held: value_holders.unlisted,
        ..PackageCount::default()
    };
    for place in frontier.places.iter().chain(&folded_places) {
        match package_of_place(place) {
            Some(package) => {
                if let Some(count) = counts.get_mut(&Some(package)) {
                    count.places += 1;
                }
            }
            None => everywhere.places += 1,
        }
    }
    // A value held where a later call can take it from: against the package
    // of the member held, and against every package when it is the focal.
    let mut hold = |to: &EntityId| match member_package.get(to) {
        Some(package) => {
            if let Some(count) = counts.get_mut(package) {
                count.held += 1;
            }
        }
        None => everywhere.held += 1,
    };
    held_by_rows.iter().for_each(&mut hold);
    value_holders
        .in_members
        .iter()
        .for_each(|(_, to, _)| hold(to));
    let members_cut = closure
        .bounds
        .iter()
        .any(|bound| bound.which != "frontier_reads_max");
    let mut ranked: Vec<(Option<String>, PackageCount)> = counts.into_iter().collect();
    ranked.sort_by_cached_key(|(package, _)| package_rank(package));
    let packages: Vec<Value> = ranked
        .iter()
        .map(|(package, count)| {
            let unread = count.unread + everywhere.unread;
            let places = count.places + everywhere.places;
            let held = count.held + everywhere.held;
            let state = if members_cut {
                "bounded"
            } else if unread > 0 {
                "unread"
            } else if places > 0 || held > 0 {
                "open"
            } else {
                "closed"
            };
            let mut row = json!({
                "module": package,
                "members": count.members,
                "state": state,
            });
            for (key, value) in [
                (NAME_ONLY_KEY, count.name_only),
                ("unread", unread),
                ("places", places),
                ("held", held),
            ] {
                if value > 0 {
                    row[key] = json!(value);
                }
            }
            if tests_only.get(package).copied().unwrap_or(false) {
                row["role"] = json!("test");
            }
            row
        })
        .collect();

    let mut block = json!({
        "question": format!("which entities{in_scope} can end up calling the focal{stays}"),
        // The entity every member reaches, named on the block itself so a
        // page that carries the block without the walk's head still says
        // what its members call.
        "target": {"entity_id": focal.id.to_string(), "name": focal.name},
        "policy": {
            "follows": [
                "Calls:type_resolved",
                "Calls:import_scoped",
                "Calls:name_only",
                "call_site:name_unique",
                "ledger:callback",
                "ledger:dispatch",
            ],
            "reported_apart": ["References", "UsesType", "Instantiates", "value_use"],
            "counted_only": ["Imports", "unkeyed_lookup", "call_through_value"],
            "not_followed": "calls made later through a member held as a value, and calls \
                             resolved to a method a member overrides",
            "bounds": {
                "members_max": closure.limits.members_max,
                "frontier_reads_max": closure.limits.frontier_reads_max,
            },
        },
        "state": state,
        "statement": statement,
        "witness_paths": "follow via from any member to the focal; each row's line and code \
                          are the call it makes to its via",
        "members_total": count,
        "members": members,
        "also_referenced_by": also_referenced_by,
        "also_referenced_total": referrers_total,
        "frontier": frontier.rows,
        "frontier_total": frontier_total,
        "settled_by_membership": settled_by_membership,
        "bounds": bounds,
        CALLS_MEASURED_KEY: calls_measured,
        "blind_spots": Value::Object(blind_spots),
    });
    if !name_only.is_empty() {
        block[NAME_ONLY_TOTAL_KEY] = json!(name_only.len());
        block[NAME_ONLY_KEY] = Value::Array(name_only);
    }
    if packages.len() > 1 {
        let more = packages.len().saturating_sub(PACKAGES_MAX);
        block[PACKAGES_KEY] = Value::Array(packages.into_iter().take(PACKAGES_MAX).collect());
        if more > 0 {
            block[PACKAGES_MORE_KEY] = json!(more);
        }
    }
    block["also_imported_by"] = json!(closure
        .importers
        .iter()
        .filter(|id| !closure.is_in(id))
        .filter(|id| {
            scope.is_none_or(|scope| scope.place_id(closure.store, id) == Some(Placement::Inside))
        })
        .count());
    if let Some(scope) = scope {
        let mut held = Map::new();
        held.insert("module".into(), json!(scope.module()));
        held.insert("mode".into(), json!(scope.mode().as_str()));
        // Callers outside the scope: not followed under `paths`, walked and
        // not listed under `rows`.
        held.insert(
            "outside".into(),
            json!(closure.outside.len() + outside_members),
        );
        if !via_outside.is_empty() {
            held.insert("via_outside".into(), Value::Array(via_outside));
        }
        if referrers_outside > 0 {
            held.insert("referrers_outside".into(), json!(referrers_outside));
        }
        if frontier_outside > 0 {
            held.insert("frontier_outside".into(), json!(frontier_outside));
        }
        block["scope"] = Value::Object(held);
    }
    // Places past the rows `frontier` lists, counted by the member they could
    // add callers to. They are the same kinds of place as the listed rows and
    // are already in `frontier_total`, so the count is the whole of what they
    // add; the block says that once rather than pointing at another query.
    if frontier.in_tests > 0 {
        block[FRONTIER_IN_TESTS_KEY] = json!(frontier.in_tests);
    }
    if let Some((call_sites, callers)) = tests_excluded {
        block[TESTS_EXCLUDED_KEY] = json!({"call_sites": call_sites, "callers": callers});
    }
    block["frontier_withheld"] = Value::Array(
        frontier
            .withheld
            .iter()
            .map(|(to, rows)| {
                json!({
                    "to": to.to_string(),
                    "to_name": closure.name_of(to),
                    "rows": rows,
                })
            })
            .collect(),
    );
    if !frontier.withheld.is_empty() {
        block[FRONTIER_WITHHELD_NOTE_KEY] = json!(FRONTIER_WITHHELD_NOTE);
    }
    if folded_total > 0 {
        let folded_scope = match closure.confining() {
            Some(scope) => json!({
                "mode": "paths",
                "module": scope.module(),
                "known_outside_callers": "excluded",
                "unplaced_callers": "retained",
            }),
            None => json!({"mode": "repository"}),
        };
        block[FRONTIER_FOLDED_KEY] = json!({
            "count": folded_total,
            "unit": "observations",
            "scope": folded_scope,
            "by_reason": folded,
            "why": FRONTIER_FOLDED_WHY,
        });
    }
    block
}

/// Where a reach block whose members sit in more than one package says how
/// far each package was read, in the order its member rows are listed.
///
/// Each entry names the package's `module` and counts its `members` and its
/// `name_only` rows. Its `state` is the state the block would have for that
/// package alone, on paths that stay inside it:
///
/// - `closed`: every member there was read for unresolved calls, no place
///   that could add a caller sits in the package, and none of its members is
///   held as a value. Its members are every entity in the package that
///   reaches the focal through calls inside the package.
/// - `open`: every member was read, and `places` places in the package or
///   `held` held values could still add a caller.
/// - `unread`: the read bound left `unread` of its members unread, so the
///   places that could add callers there are not known.
/// - `bounded`: a work bound cut the members themselves.
///
/// A place no record puts in a package, and a value held of the focal
/// itself, count against every package. A package whose members are all in
/// test code says `role: test`. Measured on Flask: a question about the
/// callers inside `flask` got 126 members of the whole repository under one
/// partial verdict, and the model paged for more though the seven it was
/// asked about were all there.
pub(crate) const PACKAGES_KEY: &str = "packages";

/// How many packages [`PACKAGES_KEY`] leaves out past [`PACKAGES_MAX`].
pub(crate) const PACKAGES_MORE_KEY: &str = "packages_more";

/// Packages a block describes before it counts the rest.
const PACKAGES_MAX: usize = 12;

/// What one package of a reach holds, and what could still add to it.
#[derive(Debug, Default, Clone)]
struct PackageCount {
    members: usize,
    name_only: usize,
    /// Callable members whose unresolved calls were not read.
    unread: usize,
    /// Places that could add a caller, listed, withheld or folded.
    places: usize,
    /// Values of its members held where a later call can take them from.
    held: usize,
}

/// How many leading dotted segments every one of `modules` shares.
fn shared_segments<'a>(modules: impl Iterator<Item = &'a str>) -> usize {
    let mut shared: Option<Vec<&str>> = None;
    for module in modules {
        let segments: Vec<&str> = module.split('.').collect();
        shared = Some(match shared {
            None => segments,
            Some(prefix) => prefix
                .into_iter()
                .zip(segments)
                .take_while(|(left, right)| left == right)
                .map(|(left, _)| left)
                .collect(),
        });
    }
    shared.map_or(0, |prefix| prefix.len())
}

/// The package a module sits in: its path one segment below the `shared`
/// prefix, or the whole path when it is no longer than that.
fn package_of(module: &str, shared: usize) -> String {
    module
        .split('.')
        .take(shared + 1)
        .collect::<Vec<_>>()
        .join(".")
}

/// Where a reach block lists the entities that reach the focal only through
/// a call matched by a bare name, apart from `members`: rows of the same
/// shape, each a possible caller. `self.options.get("subdomain")` reached
/// `Scaffold.get` that way, and counted among the members it read as a
/// proven caller of everything `get` reaches.
pub(crate) const NAME_ONLY_KEY: &str = "name_only";

/// How many rows [`NAME_ONLY_KEY`] holds in all, as `members_total` counts
/// the members. Absent when there are none.
pub(crate) const NAME_ONLY_TOTAL_KEY: &str = "name_only_total";

/// Where a reach block's `blind_spots` reads the places a member is held as
/// a value against the rows it serves.
pub(crate) const VALUE_HOLDERS_KEY: &str = "value_holders";

/// Where a reach block says its own call census measured every caller that
/// could reach a member: every member read, none past a bound, and every
/// caller that could spell a member's call names read from a current
/// call-site ledger. A pending language-server readiness then has nothing
/// left to stand in for in the reach answer.
pub(crate) const CALLS_MEASURED_KEY: &str = "calls_measured";

/// What `blind_spots.value_holders` says.
pub(crate) const VALUE_HOLDERS_WHY: &str = "A call through a value that spells no member's name \
     can run a member only through a value taken where the census lists the member held as one. \
     rows counts the also_referenced_by rows that hold a member as a value (each such ref carries \
     value), in_members names members that hold one themselves, and unlisted counts places no \
     served row names. complete is true when every member has a census and every such place is \
     served: those holders are then the places to check, not the calls_through_values count.";

/// Where a reach block counts its listed frontier rows that sit in code whose
/// stored role is Test.
pub(crate) const FRONTIER_IN_TESTS_KEY: &str = "frontier_in_tests";

/// Where a reach block says what its `frontier_withheld` counts are.
pub(crate) const FRONTIER_WITHHELD_NOTE_KEY: &str = "frontier_withheld_note";

/// What `frontier_withheld` counts: places of the kinds `frontier` lists that
/// did not fit as rows, by the member each could add callers to. Already in
/// `frontier_total`.
pub(crate) const FRONTIER_WITHHELD_NOTE: &str = "Each entry counts places that could add callers \
     to the member it names, of the same kinds as the frontier rows, that are not listed one by \
     one. They are already counted in frontier_total.";

/// Where a reach block counts the places it folded, by reason.
pub(crate) const FRONTIER_FOLDED_KEY: &str = "frontier_folded";

/// What the folded places are and the one way each could matter. Counted,
/// never listed as places to check and never read as an absence.
pub(crate) const FRONTIER_FOLDED_WHY: &str =
    "Counted observations, not distinct sites or callers, \
     and not in frontier_total. The same site can be observed for several reached members. \
     scope names the caller filter used for this count, separately from the blind_spots counts. \
     caller_text_not_read: code whose text Kin could not read, so whether it calls a member is \
     unknown; it could add a caller only if that unread code calls one. call_protocol and \
     constructor_dispatch: places the language runs __call__ or __init__ from, which could add a \
     caller only through a call of an instance or a construction of the class. refs with \
     details:true on a member lists its own.";

/// Room [`fit`] keeps for the bound it records.
const FIT_RESERVE: usize = 600;

/// Cut `block` to at most `room` serialized bytes for a reply that is bounded
/// rather than paged: frontier rows first, then referrers, then the farthest
/// members, each counted, and the cut recorded as a `response_budget` bound so
/// the block reads as a floor. A block that cannot fit even as its verdict is
/// left out: returns `false`, and the caller serves no block.
pub fn fit(block: &mut Value, room: usize) -> bool {
    let size = |block: &Value| serde_json::to_vec(block).map_or(usize::MAX, |bytes| bytes.len());
    if size(block) <= room {
        return true;
    }
    let target = room.saturating_sub(FIT_RESERVE);
    let mut cut = 0usize;
    for key in ["frontier", "also_referenced_by", NAME_ONLY_KEY, "members"] {
        while size(block) > target {
            let Some(rows) = block.get_mut(key).and_then(Value::as_array_mut) else {
                break;
            };
            if rows.pop().is_none() {
                break;
            }
            cut += 1;
        }
    }
    let bound = json!({
        "which": "response_budget",
        "limit": room,
        "left": cut,
        "at": [],
        "detail": format!(
            "the reply's size ceiling left room for part of this block, so {cut} rows are left \
             out; ask again with a larger max_response_chars, or page the reply"
        ),
    });
    if let Some(bounds) = block.get_mut("bounds").and_then(Value::as_array_mut) {
        bounds.push(bound);
    }
    block["state"] = json!("bounded");
    block["statement"] = json!(format!(
        "Not exhaustive here: the reply's size ceiling left out {cut} rows of this block. {}",
        block["statement"].as_str().unwrap_or_default()
    ));
    if size(block) <= room {
        return true;
    }
    let members_total = block["members_total"].clone();
    *block = json!({
        "state": "bounded",
        "statement": "The reply's size ceiling left no room for the reach answer; ask again \
                      with a larger max_response_chars, or page the reply.",
        "members_total": members_total,
    });
    size(block) <= room
}

#[cfg(test)]
#[path = "trace_reach_tests.rs"]
mod tests;
