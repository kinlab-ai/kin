// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A trace confined to a semantic scope, applied while the walk runs.
//!
//! "Which functions in `react-dom-bindings` can end up calling X" restricts
//! the paths as well as the rows: a function in the package that reaches X
//! only through a function in another package is no answer to it. A walk over
//! the whole graph followed by a filter on its rows gets that wrong, and it
//! hands the agent hundreds of rows to adjudicate. Measured on a scored
//! package-scoped reach question, the first unscoped trace advertised 295
//! reachable functions across the repository; the agent spent 11 context
//! calls and five trace pages deciding which were in the package, about
//! 930,000 tokens, and ran out of time with no answer.
//!
//! So `scope` names a module path prefix, and the walk decides membership at
//! every hop:
//!
//! - Membership is read from the graph's own module records, never from a
//!   file path: an entity is in scope when its module path
//!   ([`kin_ranking::module_qualifier::ModuleIndex::path`], the dotted name a
//!   reply's `module` field spells, such as `flask.sansio.app`) is the scope
//!   or starts with it at a `.`. A scope that is not the start of any module
//!   path but sits inside exactly one (`sansio`) is read as that one
//!   (`flask.sansio`); one that sits inside several is refused with each, and
//!   one that names no module path at all is refused, so a typo never reads
//!   as an answer that nothing in scope calls the focal.
//! - `paths` (the default) restricts the traversal: the walk follows no edge to
//!   an entity outside the scope, so every listed row reaches the focal by a
//!   path that stays inside it. `rows` walks the whole graph and lists only the
//!   rows in scope, keeping the out-of-scope entities a witness path runs
//!   through so every `via` can still be followed.
//! - The focal is the walk's root wherever it sits; only the hops away from
//!   it are held to the scope.
//! - An entity whose file has no module record cannot be placed. Whether it is
//!   in scope is unknown, so it is never silently dropped: it is a frontier
//!   row, and the answer is open until it is settled.
//!
//! The reply carries a `scope` block naming the scope as resolved, the mode,
//! how many out-of-scope entities the walk met and did not follow, and the
//! scoped answer's state. A callers walk's answer is its `reach` block (see
//! [`crate::trace_reach`]), which the scope narrows during the closure; a calls
//! walk's answer is its chain, whose frontier here lists each in-scope step the
//! walk did not read past, with why. A cut at the depth bound is a frontier
//! row, never completeness.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use kin_model::graph::{EntityFilter, GraphStore};
use kin_model::{Entity, EntityId, EntityKind, EntityRole};
use kin_ranking::module_qualifier::ModuleIndex;

/// The key a trace payload carries the block under.
pub const SCOPE_KEY: &str = "scope";

/// The argument naming the scope.
pub const SCOPE_ARGUMENT: &str = "scope";

/// The argument choosing what the scope restricts.
pub const SCOPE_MODE_ARGUMENT: &str = "scope_mode";

/// Out-of-scope entities a block names before it counts the rest.
const OUTSIDE_SAMPLE: usize = 5;

/// Frontier rows a block lists before it counts the rest.
const FRONTIER_ROWS_MAX: usize = 40;

/// Module paths a refusal names before it counts the rest.
const SUGGESTIONS_MAX: usize = 8;

/// The deepest a trace walk goes, as the walk clamps `depth`.
const MAX_DEPTH: usize = 8;

/// The key an unscoped walk carries its module census under.
pub const MODULES_KEY: &str = "modules";

/// Packages a census names before it counts the rest.
const CENSUS_PACKAGES_MAX: usize = 6;

/// Where an unscoped walk's steps sit, counted by package, and the scope
/// that keeps the largest one.
///
/// A package is the module path one segment below the prefix every placed
/// step shares, so React's steps count by `packages.react_reconciler` and
/// `packages.react_dom` rather than all under `packages`. Placement is read
/// the way `scope` reads it, from the graph's module records and never from a
/// file path, so the module named as `largest` is a scope that resolves. A
/// step with no module record, or outside the repository, is counted as
/// unplaced. `None` when the steps sit in one package, or fewer than two
/// are placed: there is nothing a scope would narrow.
pub fn module_census<G: GraphStore + ?Sized>(store: &G, steps: &[&str]) -> Option<Value> {
    if steps.len() < 2 {
        return None;
    }
    let modules = store
        .query_entities(&EntityFilter {
            kinds: Some(vec![EntityKind::Module]),
            ..Default::default()
        })
        .ok()?;
    let index = ModuleIndex::from_store_modules(store, modules);
    let mut placed: Vec<Vec<String>> = Vec::new();
    let mut unplaced = 0usize;
    for step in steps {
        let entity = uuid::Uuid::parse_str(step)
            .ok()
            .and_then(|uuid| store.get_entity(&EntityId(uuid)).ok().flatten());
        let path = entity
            .filter(|entity| entity.role != EntityRole::External && entity.file_origin.is_some())
            .map(|entity| index.path(&entity))
            .filter(|path| !path.is_empty());
        match path {
            Some(path) => placed.push(path),
            None => unplaced += 1,
        }
    }
    if placed.len() < 2 {
        return None;
    }
    let shared = placed[1..].iter().fold(placed[0].len(), |shared, path| {
        placed[0][..shared]
            .iter()
            .zip(path)
            .take_while(|(left, right)| left == right)
            .count()
    });
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for path in &placed {
        let package = path[..path.len().min(shared + 1)].join(".");
        *counts.entry(package).or_default() += 1;
    }
    if counts.len() < 2 {
        return None;
    }
    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let (largest, kept) = ranked[0].clone();
    let mut census = json!({
        "steps": steps.len(),
        "packages": ranked
            .iter()
            .take(CENSUS_PACKAGES_MAX)
            .map(|(module, steps)| json!({"module": module, "steps": steps}))
            .collect::<Vec<_>>(),
        "largest": largest,
        "outside_largest": steps.len() - kept,
    });
    if ranked.len() > CENSUS_PACKAGES_MAX {
        census["other_packages"] = json!(ranked.len() - CENSUS_PACKAGES_MAX);
    }
    if unplaced > 0 {
        census["unplaced"] = json!(unplaced);
    }
    Some(census)
}

/// What a scope restricts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeMode {
    /// The walk follows no edge out of the scope.
    #[default]
    Paths,
    /// The walk is whole and only the rows it lists are held to the scope.
    Rows,
}

impl ScopeMode {
    /// The mode `raw` names, with the spellings a caller is likely to use.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "paths" | "path" | "traversal" | "walk" | "restrict" | "" => Ok(Self::Paths),
            "rows" | "row" | "output" | "filter" | "results" => Ok(Self::Rows),
            other => Err(format!(
                "invalid scope_mode '{other}': expected paths (the walk stays inside the scope) \
                 or rows (the walk is whole and only the rows are held to the scope)"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Paths => "paths",
            Self::Rows => "rows",
        }
    }
}

/// A scope as a caller asked for it, before the graph resolves it. Both trace
/// arms receive this shape, the in-process walk from its arguments and the
/// daemon's walker inside its request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeRequest {
    pub module: String,
    #[serde(default)]
    pub mode: ScopeMode,
}

impl ScopeRequest {
    /// The scope `arguments` name, `None` when they name none. A `scope_mode`
    /// without a `scope`, an empty scope and a scope that is a file path are
    /// refused rather than read as an unscoped walk.
    pub fn from_arguments(arguments: &HashMap<String, Value>) -> Result<Option<Self>, String> {
        let mode = match arguments.get(SCOPE_MODE_ARGUMENT) {
            None | Some(Value::Null) => None,
            Some(Value::String(raw)) => Some(ScopeMode::parse(raw)?),
            Some(_) => return Err("scope_mode must be a string: paths or rows".into()),
        };
        let module = match arguments.get(SCOPE_ARGUMENT) {
            None | Some(Value::Null) => {
                return match mode {
                    Some(_) => Err("scope_mode needs scope, the module path the walk is \
                                    confined to"
                        .into()),
                    None => Ok(None),
                };
            }
            Some(Value::String(raw)) => raw.clone(),
            Some(_) => return Err("scope must be a string: a module path such as pkg.sub".into()),
        };
        Self::new(&module, mode.unwrap_or_default()).map(Some)
    }

    /// A checked request: a dotted module path, never a file path.
    pub fn new(module: &str, mode: ScopeMode) -> Result<Self, String> {
        let module = module.trim().trim_matches('.').to_string();
        if module.is_empty() {
            return Err(
                "scope must name a module path, such as the start of a row's module field \
                 (pkg or pkg.sub)"
                    .into(),
            );
        }
        if module.chars().any(char::is_whitespace) {
            return Err(format!(
                "scope '{module}' is not a module path: name modules with dots, as a row's \
                 module field spells them (pkg.sub)"
            ));
        }
        Ok(Self { module, mode })
    }
}

/// Where an entity stands against a scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Inside,
    Outside,
    /// Its file has no module record, so whether it is in scope is unknown.
    Unplaced,
}

/// One entity the walk did not follow because it lies outside the scope.
#[derive(Debug, Clone)]
struct Outside {
    name: String,
    module: Option<String>,
}

/// An entity next to the walk that could not be placed, and the step whose
/// neighbour it is.
#[derive(Debug, Clone)]
struct Unplaced {
    step: usize,
    id: EntityId,
    name: String,
}

/// A scope the graph resolved, with the module records it is matched
/// against and what the walk met outside it.
pub struct TraceScope {
    /// The scope as the graph spells it.
    module: String,
    /// The scope as the caller typed it.
    asked: String,
    mode: ScopeMode,
    /// `module`, lowercased, for a match that ignores case as locate's does.
    folded: String,
    index: ModuleIndex,
    placed: RefCell<HashMap<EntityId, Placement>>,
    outside: RefCell<BTreeMap<EntityId, Outside>>,
    external_outside: RefCell<usize>,
    unplaced: RefCell<Vec<Unplaced>>,
}

impl std::fmt::Debug for TraceScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TraceScope")
            .field("module", &self.module)
            .field("mode", &self.mode)
            .finish()
    }
}

/// Every module path prefix that ends with the lowercased `segments` at `.`
/// boundaries, as the graph spells it: `sansio` in `flask.sansio.app` is
/// `flask.sansio`.
fn enclosing(paths: &BTreeSet<String>, segments: &str) -> BTreeSet<String> {
    let mut found: BTreeSet<String> = BTreeSet::new();
    if segments.is_empty() {
        return found;
    }
    for path in paths {
        let lowered = path.to_lowercase();
        if lowered.len() != path.len() {
            continue;
        }
        let mut from = 0;
        while let Some(at) = lowered[from..].find(segments) {
            let start = from + at;
            let end = start + segments.len();
            let starts = start == 0 || lowered.as_bytes()[start - 1] == b'.';
            let ends = end == lowered.len() || lowered.as_bytes()[end] == b'.';
            if starts && ends {
                found.insert(path[..end].to_string());
            }
            from = start + 1;
        }
    }
    found
}

/// Whether the dotted, lowercased `path` is `prefix` or starts with it at a
/// `.`.
fn under(path: &str, prefix: &str) -> bool {
    path == prefix
        || (path.len() > prefix.len()
            && path.starts_with(prefix)
            && path.as_bytes()[prefix.len()] == b'.')
}

/// A resolved scope as a plain test of where an entity sits, for an answer
/// that lists rows rather than walks: a reference answer or an impact
/// answer about one part of the repository.
///
/// It is the scope a walk resolves ([`TraceScope::resolve`]), spelled as the
/// graph spells it and refused the same way when it names no module path or
/// several, without the walk's record of what it met. It holds no interior
/// state, so a handler can hold it across an await.
#[derive(Debug, Clone)]
pub struct RowScope {
    module: String,
    folded: String,
    index: ModuleIndex,
}

impl RowScope {
    /// The scope `arguments` name, resolved against the module records
    /// `store` holds, or `None` when they name none.
    pub fn from_arguments<G: GraphStore + ?Sized>(
        store: &G,
        arguments: &HashMap<String, Value>,
    ) -> Result<Option<Self>, String> {
        let Some(request) = ScopeRequest::from_arguments(arguments)? else {
            return Ok(None);
        };
        TraceScope::resolve(store, &request).map(|scope| {
            Some(Self {
                module: scope.module,
                folded: scope.folded,
                index: scope.index,
            })
        })
    }

    /// The scope as the graph spells it.
    pub fn module(&self) -> &str {
        &self.module
    }

    /// Where `entity` stands, as [`TraceScope::place`] reads it.
    pub fn place(&self, entity: &Entity) -> Placement {
        if entity.role == EntityRole::External || entity.file_origin.is_none() {
            return Placement::Outside;
        }
        let path = self.index.path(entity);
        if path.is_empty() {
            Placement::Unplaced
        } else if under(&path.join(".").to_lowercase(), &self.folded) {
            Placement::Inside
        } else {
            Placement::Outside
        }
    }

    /// Whether a row for `entity` stays listed: it is inside the scope, or
    /// its file has no module record, in which case where it sits is unknown
    /// and it is kept rather than dropped unseen.
    pub fn lists(&self, entity: &Entity) -> bool {
        self.place(entity) != Placement::Outside
    }

    /// [`Self::lists`] for a row that names only the dotted module it sits
    /// in. A row with no module is kept, since where it sits is unknown.
    pub fn lists_module(&self, module: Option<&str>) -> bool {
        module.is_none_or(|module| under(&module.to_lowercase(), &self.folded))
    }

    /// [`Self::lists`] for the entity a row names by `id`. A row whose
    /// entity the graph does not hold is kept.
    pub fn lists_id<G: GraphStore + ?Sized>(&self, store: &G, id: &str) -> bool {
        uuid::Uuid::parse_str(id)
            .ok()
            .and_then(|uuid| store.get_entity(&EntityId(uuid)).ok().flatten())
            .is_none_or(|entity| self.lists(&entity))
    }
}

impl TraceScope {
    /// Resolve `request` against the module records `store` holds.
    pub fn resolve<G: GraphStore + ?Sized>(
        store: &G,
        request: &ScopeRequest,
    ) -> Result<Self, String> {
        let modules = store
            .query_entities(&EntityFilter {
                kinds: Some(vec![EntityKind::Module]),
                ..Default::default()
            })
            .map_err(|error| format!("scope could not read the graph's module records: {error}"))?;
        let index = ModuleIndex::from_store_modules(store, modules.iter().cloned());
        let mut paths: BTreeSet<String> = BTreeSet::new();
        for module in &modules {
            if module.role == EntityRole::External {
                continue;
            }
            let path = index.path(module);
            if !path.is_empty() {
                paths.insert(path.join("."));
            }
        }
        let asked = request.module.clone();
        // A path names no module. Say which module paths its segments do, so
        // the next call names one.
        if asked.contains(['/', '\\']) {
            let mut named: BTreeSet<String> = BTreeSet::new();
            for segment in asked
                .split(['/', '\\'])
                .filter(|segment| !segment.is_empty())
            {
                named.extend(enclosing(&paths, &segment.to_lowercase()));
            }
            // The innermost a path names stands for the ones around it.
            let innermost: Vec<&str> = named
                .iter()
                .filter(|path| {
                    !named.iter().any(|other| {
                        other.len() > path.len()
                            && under(&other.to_lowercase(), &path.to_lowercase())
                    })
                })
                .map(String::as_str)
                .take(SUGGESTIONS_MAX)
                .collect();
            return Err(format!(
                "scope '{asked}' is not a module path: a scope names modules with dots, as a \
                 row's module field spells them{}",
                if innermost.is_empty() {
                    String::new()
                } else {
                    format!("; this one holds {}", innermost.join(", "))
                }
            ));
        }
        let folded = asked.to_lowercase();
        // The scope as the graph spells it: the start of a module path.
        let anchored = paths.iter().find_map(|path| {
            let lowered = path.to_lowercase();
            (lowered.len() == path.len() && under(&lowered, &folded))
                .then(|| path[..folded.len()].to_string())
        });
        let module = match anchored {
            Some(module) => module,
            None => {
                // Not the start of any path: read it as the one enclosing
                // path it sits inside at segment boundaries, when there is
                // exactly one.
                let enclosing = enclosing(&paths, &folded);
                match enclosing.len() {
                    1 => enclosing.into_iter().next().unwrap_or_default(),
                    0 => {
                        let sample: Vec<&str> = paths
                            .iter()
                            .map(|path| path.split('.').next().unwrap_or(path))
                            .collect::<BTreeSet<_>>()
                            .into_iter()
                            .take(SUGGESTIONS_MAX)
                            .collect();
                        return Err(if sample.is_empty() {
                            format!(
                                "scope '{asked}' cannot be matched: the graph holds no module \
                                 records to place entities by"
                            )
                        } else {
                            format!(
                                "scope '{asked}' names no module path in this graph; a scope \
                                 is the start of a row's module field, such as {}",
                                sample.join(", ")
                            )
                        });
                    }
                    several => {
                        let listed: Vec<&str> = enclosing
                            .iter()
                            .map(String::as_str)
                            .take(SUGGESTIONS_MAX)
                            .collect();
                        return Err(format!(
                            "scope '{asked}' sits inside {several} module paths: {}{}; pass the \
                             one you mean",
                            listed.join(", "),
                            if several > listed.len() { ", ..." } else { "" }
                        ));
                    }
                }
            }
        };
        Ok(Self {
            folded: module.to_lowercase(),
            module,
            asked,
            mode: request.mode,
            index,
            placed: RefCell::new(HashMap::new()),
            outside: RefCell::new(BTreeMap::new()),
            external_outside: RefCell::new(0),
            unplaced: RefCell::new(Vec::new()),
        })
    }

    /// The scope as the graph spells it.
    pub fn module(&self) -> &str {
        &self.module
    }

    pub fn mode(&self) -> ScopeMode {
        self.mode
    }

    /// Whether the walk itself is held to the scope.
    pub fn restricts_paths(&self) -> bool {
        self.mode == ScopeMode::Paths
    }

    /// The dotted module path that places `entity`, when it has one.
    pub fn module_of(&self, entity: &Entity) -> Option<String> {
        let path = self.index.path(entity);
        (!path.is_empty()).then(|| path.join("."))
    }

    /// Where `entity` stands. A symbol outside the repository, or one the
    /// graph holds no file for, belongs to no module and so to no scope.
    pub fn place(&self, entity: &Entity) -> Placement {
        if let Some(placed) = self.placed.borrow().get(&entity.id) {
            return *placed;
        }
        let placed = if entity.role == EntityRole::External || entity.file_origin.is_none() {
            Placement::Outside
        } else {
            let path = self.index.path(entity);
            if path.is_empty() {
                Placement::Unplaced
            } else if under(&path.join(".").to_lowercase(), &self.folded) {
                Placement::Inside
            } else {
                Placement::Outside
            }
        };
        self.placed.borrow_mut().insert(entity.id, placed);
        placed
    }

    /// Where the entity `id` names stands, or `None` when the graph holds no
    /// such entity.
    pub fn place_id<G: GraphStore + ?Sized>(&self, store: &G, id: &EntityId) -> Option<Placement> {
        if let Some(placed) = self.placed.borrow().get(id) {
            return Some(*placed);
        }
        let entity = store.get_entity(id).ok().flatten()?;
        Some(self.place(&entity))
    }

    /// Record `entity` as met outside the scope and not followed.
    pub fn record_outside(&self, entity: &Entity) {
        let module = self.module_of(entity);
        self.outside
            .borrow_mut()
            .entry(entity.id)
            .or_insert_with(|| Outside {
                name: entity.name.clone(),
                module,
            });
    }

    /// Whether a chain walk may step to `entity` from step `parent`. Under
    /// `paths`, an entity outside the scope is recorded and refused, and one
    /// that cannot be placed is recorded as a frontier of `parent` and
    /// refused; under `rows` every entity is walked.
    pub fn admits_step(&self, entity: &Entity, parent: usize) -> bool {
        if !self.restricts_paths() {
            if self.place(entity) == Placement::Outside {
                self.record_outside(entity);
            }
            return true;
        }
        match self.place(entity) {
            Placement::Inside => true,
            Placement::Outside => {
                self.record_outside(entity);
                false
            }
            Placement::Unplaced => {
                let mut unplaced = self.unplaced.borrow_mut();
                if !unplaced.iter().any(|row| row.id == entity.id) {
                    unplaced.push(Unplaced {
                        step: parent,
                        id: entity.id,
                        name: entity.name.clone(),
                    });
                }
                false
            }
        }
    }

    /// Whether a chain walk keeps `count` calls into symbols outside the
    /// repository. They belong to no module, so under `paths` they are
    /// counted and dropped.
    pub fn admits_external(&self, count: usize) -> bool {
        if count == 0 || !self.restricts_paths() {
            return true;
        }
        *self.external_outside.borrow_mut() += count;
        false
    }

    /// The membership rule, in one sentence.
    fn rule(&self) -> String {
        let membership = format!(
            "In scope: an entity whose module path is {0} or lies under it ({0}.x).",
            self.module
        );
        match self.mode {
            ScopeMode::Paths => format!(
                "{membership} The walk follows no edge to an entity outside it, so every row \
                 reaches the focal by a path that stays inside; the focal is the root wherever \
                 it sits."
            ),
            ScopeMode::Rows => format!(
                "{membership} The walk is whole and lists only rows in scope; scope.outside_steps \
                 and reach.scope.via_outside are the out-of-scope entities witness paths run \
                 through."
            ),
        }
    }

    /// The `scope` block for a walk whose chain is `steps`, read after its
    /// terminals were classified. `reach` is the walk's reach block when it
    /// read callers; a callers walk's answer is that block, and a calls walk's
    /// is the chain.
    #[allow(clippy::too_many_arguments)]
    pub fn block(
        &self,
        focal: &Entity,
        focal_terminal: Option<&str>,
        focal_dropped: u64,
        steps: &[ChainStep<'_>],
        depth: usize,
        direction: &str,
        reach: Option<&Value>,
    ) -> Value {
        let chain_answers = direction != "callers";
        let reach_answers = direction != "calls";
        let mut frontier: Vec<Value> = Vec::new();
        let mut frontier_total = 0usize;
        let mut kinds: BTreeSet<&'static str> = BTreeSet::new();
        let mut push = |kind: &'static str,
                        step: usize,
                        id: String,
                        name: String,
                        frontier: &mut Vec<Value>| {
            frontier_total += 1;
            kinds.insert(kind);
            if frontier.len() < FRONTIER_ROWS_MAX {
                frontier.push(json!({"kind": kind, "step": step, "id": id, "name": name}));
            }
        };
        if chain_answers {
            let focal_step = ChainStep {
                step: 0,
                id: "",
                name: &focal.name,
                depth: 0,
                terminal: focal_terminal,
                fanout_dropped: focal_dropped,
            };
            let focal_id = focal.id.to_string();
            for step in std::iter::once(&focal_step).chain(steps) {
                let id = if step.step == 0 {
                    focal_id.clone()
                } else {
                    step.id.to_string()
                };
                let cut = match step.terminal {
                    Some("bound_reached") if step.depth >= depth => Some("depth"),
                    Some("bound_reached") => Some("work_budget"),
                    Some("coverage_gap") => Some("coverage_gap"),
                    _ => None,
                };
                if let Some(kind) = cut {
                    push(
                        kind,
                        step.step,
                        id.clone(),
                        step.name.to_string(),
                        &mut frontier,
                    );
                }
                if step.fanout_dropped > 0 {
                    push(
                        "limit_per_step",
                        step.step,
                        id,
                        step.name.to_string(),
                        &mut frontier,
                    );
                }
            }
            for row in self.unplaced.borrow().iter() {
                push(
                    "no_module",
                    row.step,
                    row.id.to_string(),
                    row.name.clone(),
                    &mut frontier,
                );
            }
        }
        let chain_state = if frontier_total > 0 {
            "open"
        } else {
            "complete"
        };
        let reach_state = reach
            .and_then(|reach| reach.get("state"))
            .and_then(Value::as_str)
            .map(|state| match state {
                "closed" => "complete",
                "open" => "open",
                _ => "bounded",
            });
        let state = match (chain_answers, reach_answers) {
            (true, false) => chain_state,
            (false, _) => reach_state.unwrap_or("bounded"),
            (true, true) => worst(chain_state, reach_state.unwrap_or("bounded")),
        };
        let answer = match (chain_answers, reach_answers) {
            (true, false) => "chain",
            (false, _) => "reach",
            (true, true) => "reach and chain",
        };

        let outside = self.outside.borrow();
        let external = *self.external_outside.borrow();
        let mut block = Map::new();
        block.insert("module".into(), json!(self.module));
        if !self.asked.eq_ignore_ascii_case(&self.module) {
            block.insert("asked".into(), json!(self.asked));
        }
        block.insert("mode".into(), json!(self.mode.as_str()));
        block.insert("rule".into(), json!(self.rule()));
        block.insert(
            "focal_in_scope".into(),
            json!(self.place(focal) == Placement::Inside),
        );
        block.insert("answer".into(), json!(answer));
        block.insert("state".into(), json!(state));
        block.insert(
            "statement".into(),
            json!(self.statement(state, answer, frontier_total, depth)),
        );
        // What the walk met outside the scope and did not follow, or under
        // `rows` walked and did not list. The chain and the closure share this
        // scope, so an entity both met counts once.
        block.insert("outside".into(), json!(outside.len()));
        if !outside.is_empty() {
            block.insert(
                "outside_sample".into(),
                Value::Array(
                    outside
                        .iter()
                        .take(OUTSIDE_SAMPLE)
                        .map(|(id, row)| {
                            json!({"id": id.to_string(), "name": row.name, "module": row.module})
                        })
                        .collect(),
                ),
            );
        }
        if external > 0 {
            block.insert("outside_external_calls".into(), json!(external));
        }
        if self.mode == ScopeMode::Rows {
            let outside_steps: Vec<usize> = steps
                .iter()
                .filter(|step| {
                    uuid::Uuid::parse_str(step.id)
                        .ok()
                        .and_then(|id| self.placed.borrow().get(&EntityId(id)).copied())
                        .is_none_or(|placed| placed != Placement::Inside)
                })
                .map(|step| step.step)
                .collect();
            block.insert("outside_steps".into(), json!(outside_steps));
        }
        if chain_answers {
            block.insert("frontier".into(), Value::Array(frontier));
            block.insert("frontier_total".into(), json!(frontier_total));
            if !kinds.is_empty() {
                block.insert(
                    "frontier_kinds".into(),
                    Value::Object(
                        kinds
                            .iter()
                            .map(|kind| ((*kind).to_string(), json!(frontier_why(kind, depth))))
                            .collect(),
                    ),
                );
            }
        }
        Value::Object(block)
    }

    fn statement(&self, state: &str, answer: &str, frontier: usize, depth: usize) -> String {
        let scope = &self.module;
        let paths = match self.mode {
            ScopeMode::Paths => format!(" by paths that stay inside {scope}"),
            ScopeMode::Rows => String::new(),
        };
        match (state, answer) {
            ("complete", "chain") => format!(
                "Complete: the chain lists every entity in {scope} the focal reaches{paths}; no \
                 step was cut by depth {depth}, a work budget or limit_per_step."
            ),
            ("complete", _) => format!(
                "Complete: reach.members lists every entity in {scope} that can end up calling \
                 the focal{paths}; the closure is not cut at depth, so the chain's depth does \
                 not limit it."
            ),
            ("open", "chain") => format!(
                "Open: {} in {scope} the walk did not read past {} in scope.frontier with why; \
                 the chain is a floor until each is followed.",
                count(frontier, "step", "steps"),
                if frontier == 1 { "is" } else { "are" }
            ),
            ("open", "reach") => format!(
                "Open: reach.members lists every entity in {scope} that a followed call \
                 reaches{paths}; places that could still add callers are in reach.frontier."
            ),
            ("open", _) => format!(
                "Open: places that could still add rows in {scope} are in reach.frontier and \
                 scope.frontier, each with why; the rows are a floor until each is followed."
            ),
            _ => format!(
                "Bounded: a work bound stopped the reading in {scope}, as reach.bounds says; \
                 the rows are a floor."
            ),
        }
    }
}

/// A chain step as [`TraceScope::block`] reads it, from either arm's chain.
#[derive(Debug, Clone, Copy)]
pub struct ChainStep<'a> {
    pub step: usize,
    pub id: &'a str,
    pub name: &'a str,
    pub depth: usize,
    pub terminal: Option<&'a str>,
    pub fanout_dropped: u64,
}

impl<'a> ChainStep<'a> {
    /// A step of the in-process walk's JSON chain.
    pub fn of_json(step: &'a Value) -> Self {
        Self {
            step: step["step"].as_u64().unwrap_or(0) as usize,
            id: step["entity_id"].as_str().unwrap_or_default(),
            name: step["entity_name"].as_str().unwrap_or_default(),
            depth: step["depth"].as_u64().unwrap_or(0) as usize,
            terminal: step["terminal"].as_str(),
            fanout_dropped: step["fanout_dropped"].as_u64().unwrap_or(0),
        }
    }
}

/// The more pessimistic of two states.
fn worst<'a>(left: &'a str, right: &'a str) -> &'a str {
    let rank = |state: &str| match state {
        "complete" => 0,
        "open" => 1,
        _ => 2,
    };
    if rank(right) > rank(left) {
        right
    } else {
        left
    }
}

fn frontier_why(kind: &str, depth: usize) -> String {
    match kind {
        "depth" if depth >= MAX_DEPTH => format!(
            "the walk stops at depth {depth}, the deepest it goes, so what lies past this step \
             was not read; trace from this step to read on"
        ),
        "depth" => format!(
            "the walk stops at depth {depth}, so what lies past this step was not read; a \
             larger depth reads it"
        ),
        "work_budget" => "the walk's work budget ran out before this step was read".into(),
        "limit_per_step" => "this step has more in-scope neighbours than limit_per_step keeps; \
                             raise it to follow them"
            .into(),
        "coverage_gap" => "Kin could not confirm every call at this step".into(),
        _ => "this neighbour's file has no module record, so whether it is in scope is unknown \
              and it was not followed"
            .into(),
    }
}

fn count(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// Settle a payload's `scope` block against the `reach` block it finally
/// carries: a reach answer a response bound cut or left out is no longer
/// complete.
pub fn reconcile(payload: &mut Value) {
    let Some(map) = payload.as_object_mut() else {
        return;
    };
    let reach = map.get(crate::trace_reach::REACH_KEY).cloned();
    if let Some(block) = map.get_mut(SCOPE_KEY) {
        reconcile_block(block, reach.as_ref());
    }
}

/// [`reconcile`] for a `scope` block held apart from its payload, beside the
/// `reach` block the reply finally carries.
pub fn reconcile_block(block: &mut Value, reach: Option<&Value>) {
    if block
        .get("answer")
        .and_then(Value::as_str)
        .is_none_or(|answer| answer == "chain")
    {
        return;
    }
    let reach_state = reach
        .and_then(|reach| reach.get("state"))
        .and_then(Value::as_str);
    if matches!(reach_state, Some("closed" | "open")) || block["state"] == "bounded" {
        return;
    }
    block["state"] = json!("bounded");
    block["statement"] = json!(format!(
        "Bounded: the reply's size ceiling cut the reach answer in {}, so the rows are a \
         floor; page the reply or ask with a larger max_output_bytes.",
        block["module"].as_str().unwrap_or_default()
    ));
}

#[cfg(test)]
#[path = "trace_scope_tests.rs"]
mod tests;
