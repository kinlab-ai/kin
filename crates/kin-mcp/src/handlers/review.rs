// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use std::collections::HashMap;

use super::repository_authority::RequestRepositoryAuthority;
use kin_model::graph::GraphStore;
use kin_model::ids::SemanticChangeId;
use kin_review::write::{PlannedReviewEvent, ReviewGroup, ReviewWrite};
use kin_review::SemanticReview;

use crate::error::{McpError, Result};
use crate::session::SessionRegistry;
use crate::types::ToolCallResult;

use super::common::*;

/// Source capability for one answer's batch containment observation. Exact
/// bytes and graph records are read by the common selected-authority census;
/// the review engine receives observations, never a file-reading capability.
pub struct AnalysisEscapeEvidence<'store, G: GraphStore> {
    held: HeldSourceAuthority<'store, G>,
    scope: EntitySourceScope,
}

impl<'store, G: GraphStore> AnalysisEscapeEvidence<'store, G> {
    pub fn new(
        store: &'store G,
        authority: Option<&RequestRepositoryAuthority>,
        scope: EntitySourceScope,
    ) -> Self {
        Self {
            held: HeldSourceAuthority::new(store, authority),
            scope,
        }
    }

    pub fn observe(
        &self,
        targets: &[kin_model::Entity],
        at: Option<SemanticChangeId>,
    ) -> kin_review::enrichment::EscapeEvidenceBatch {
        let scope = at.map_or(self.scope, EntitySourceScope::At);
        let selected_change = match scope {
            EntitySourceScope::WorkspaceHead => None,
            EntitySourceScope::At(change) => Some(change),
        };
        let focals: Vec<_> = targets
            .iter()
            .map(|target| (target, kin_model::focal_call_names(target)))
            .collect();
        let readings = if focals.is_empty() {
            Vec::new()
        } else {
            self.held.escape_evidence_batch(scope, &focals)
        };
        kin_review::enrichment::EscapeEvidenceBatch {
            selected_change,
            readings,
        }
    }
}

pub const SEMANTIC_DIFF_DESC: &str = "\
Return structured entity and relation changes: added declarations, modified old/new \
records, removed declarations and relation deltas. Select one scope: base/head semantic \
change IDs, entity_ids (current state versus their history), or change_ids to combine. \
The diff preserves semantic identities and signatures. Use impact_analysis or \
semantic_review when you also need downstream effects or risk.";

pub fn handle_semantic_diff<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<ToolCallResult> {
    if let Some(refusal) = external_entity_ids_refusal(
        args,
        store,
        "semantic_diff",
        "has no revisions of it here to diff",
    )? {
        return Ok(refusal);
    }
    let diff = resolve_diff(args, store)?;
    // Keep typed records until the public boundary has qualified and projected
    // them. The operator's text formatter includes materialization metadata;
    // sending that prose would make the public diagnostic guard discard the
    // semantic result together with those coordinates.
    let mut result = serde_json::json!({"diff": semantic_metadata_json(&diff)?});
    // Private compatibility callers retain the migration notice. Public MCP
    // selectors are refused before this handler runs.
    if names_files(args) {
        record_files_deprecation(&mut result, "semantic_diff");
    }
    let json = serde_json::to_string_pretty(&result).map_err(McpError::Json)?;
    Ok(ToolCallResult::text(json))
}

/// Whether the call named `files`, the file-path targeting these tools keep
/// answering for one more release.
fn names_files(args: &HashMap<String, serde_json::Value>) -> bool {
    get_optional_string_array(args, "files").is_some_and(|files| !files.is_empty())
}

fn record_files_deprecation(payload: &mut serde_json::Value, tool: &str) {
    crate::budget::record_deprecation(
        payload,
        tool,
        "files",
        "entity_ids",
        crate::budget::DEPRECATION_REMOVED_AFTER,
    );
}

/// The refusal a change tool gives an `entity_ids` list naming a symbol
/// outside the repository, or an address naming nothing held.
///
/// Asked before the diff is built, because the diff reads an id no entity
/// carries as a removed entity, and a symbol declared outside the repository
/// was never removed from it. The whole call is refused, so no answer about
/// the other ids reads as covering it.
fn external_entity_ids_refusal<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
    tool: &str,
    why: &str,
) -> Result<Option<ToolCallResult>> {
    let Some(ids) = get_optional_string_array(args, "entity_ids").filter(|ids| !ids.is_empty())
    else {
        return Ok(None);
    };
    super::external_symbols::external_ids_refusal(store, &ids, tool, "entity_ids", why)
}

pub const IMPACT_ANALYSIS_DESC: &str = "\
Analyze the downstream impact of a change: starting from what changed, walk the \
relation graph to find every entity that could be affected. Target it four ways (one at \
a time): base/head change IDs, entity_ids, file paths, or a list of change_ids to \
combine. Optionally include active agent traffic on the impacted entities so you can see \
who else is working nearby. Reach for it before merging or refactoring to gauge blast \
radius, answering \"if I change this, what else might break?\", from the graph in one \
call instead of hand-tracing callers. Pair it with semantic_diff (what changed) or use \
semantic_review when you want diff + impact + risk together in a single report. \
Per-entity, `consumer_count` is every direct inbound consumer, and it is never narrowed \
without saying so: `external_consumer_count`, `test_consumer_count` and \
`derived_consumer_count` name each class beside it and sum to it, so a zero is a zero and \
an exclusion has a name. One set sits outside it and is reported rather than dropped: a \
consumer changed in this same diff is counted in `consumers_migrated_in_diff`, which is \
why this count matches what `find_references` reports for the same entity id except where \
a consumer co-changed, and the migrated count is exactly that difference. Read a \
break against `external_consumer_count`, since a test that breaks with the code it tests \
was never stranded, and read a used/unused claim against `consumer_count`. \
`proven_consumer_count` narrows the external count to edges resolved above `name_only`. \
`covering_tests` is a \
graph-observed lower bound, labeled beside every count rather than a claim about every \
test in the working copy, and it also counts tests two hops out, so it is wider than \
`test_consumer_count` and cannot be subtracted from anything. This response carries \
counts and buckets rather than ranked paths: the per-hop ranked report, where every step \
carries its own `resolution` and a confidence score, is produced only by \
`kin impact --json` on the CLI and is not reachable from here. Read that used/unused claim \
against the proven count as well: a \
call edge matched by bare method name is a candidate, not a fact. The response also \
carries an additive `negative` object whose `safe_to_conclude_absent` flag says whether \
this graph could have seen the impact it reports missing: the verdicts are read off \
cross-file call, import and reference edges, so on a language whose reference edges this \
build cannot produce, or on a graph holding none of them, an empty blast radius means \
the query could not observe what it was asked about rather than that nothing depends on \
the change. Every entity reported with no consumers is also read in the top-level \
`caller_arrival` block, the reading `find_references` publishes: when a file that can \
reach it holds call sites that became no edge, the verdict is inconclusive and names that \
file rather than certifying the zero. Check `safe_to_conclude_absent` before reading a \
zero consumer count as safe to change. Methods that override a changed method, directly \
or through another override, are listed in `affected_overrides` and are not consumers. \
A call to an override reaches no other edge, so its callers are listed in \
`affected_override_callers`, each naming the override under `calls_override`, with \
name-only matches in `override_caller_candidates`; for each changed method, \
`implementation_family` states what a signature change obliges across those rows. \
An entity that names a changed entity or an override without calling it, holding it as a \
value, is listed in `affected_value_uses` with the line that names it as `code`, and a \
caller row carries the call it makes as `code`. `include_tests: false` leaves test code out \
of every list and counts what it left out under `tests_excluded`. \
Rows are compact unless `compact: false`. An answer larger than `max_chars` is paged: \
repeat the call with `cursor` set to `next_cursor` until it is null.";

/// The field a serialized impact row carries beside `covering_tests`, and the
/// one value it takes.
///
/// Named once rather than spelled at each surface. The budget-survival test in
/// [`crate::envelope`] builds its impact rows by hand, so a literal there would
/// go on asserting a spelling this producer had renamed, and the pair would
/// drift with both halves green.
pub(crate) const COVERING_TESTS_BOUND_KEY: &str = "covering_tests_bound";
/// See [`COVERING_TESTS_BOUND_KEY`].
pub(crate) const COVERING_TESTS_BOUND: &str = "graph_observed_lower_bound";

/// The blast-radius buckets of an [`kin_review::ImpactReport`] that serve
/// arrays of entity rows, paired with their key in the response object. The
/// override-caller buckets serialize as a caller beside its calls and are
/// served as entity rows by
/// [`super::implementation_family::flatten_rows`] before anything here reads
/// them.
const IMPACT_ENTITY_BUCKETS: [&str; 7] = [
    "affected_callers",
    "affected_overrides",
    super::implementation_family::OVERRIDE_CALLERS_KEY,
    super::implementation_family::OVERRIDE_CALLER_CANDIDATES_KEY,
    "affected_dependents",
    "affected_contract_consumers",
    "affected_tests",
];

/// Add the presentation fields and explicit bounds an impact response needs.
///
/// `ImpactReport` holds raw `Entity` values, so serializing it exposes only the
/// nested `span`, whose rows are the graph's 0-based tree-sitter positions. An
/// agent reading those numbers to locate an affected caller lands one line above
/// it. The convention used everywhere else applies here: `span` stays a faithful
/// serialization of graph truth (its byte offsets are read as offsets), and the
/// top-level `start_line`/`end_line` carry the editor-ready position.
fn annotate_impact_presentation_lines(
    result: &mut serde_json::Value,
    impact: &kin_review::ImpactReport,
) {
    let [override_callers, override_caller_candidates] =
        super::implementation_family::bucket_entities(impact);
    let buckets: [Vec<&kin_model::Entity>; 7] = [
        impact.affected_callers.iter().collect(),
        impact.affected_overrides.iter().collect(),
        override_callers,
        override_caller_candidates,
        impact.affected_dependents.iter().collect(),
        impact.affected_contract_consumers.iter().collect(),
        impact.affected_tests.iter().collect(),
    ];
    for (key, entities) in IMPACT_ENTITY_BUCKETS.iter().zip(buckets) {
        let Some(serde_json::Value::Array(rows)) = result.get_mut(*key) else {
            continue;
        };
        // `to_value` preserves order, so a positional zip stays aligned with the
        // entities the report actually carries.
        for (row, entity) in rows.iter_mut().zip(entities) {
            let Some(object) = row.as_object_mut() else {
                continue;
            };
            object.insert(
                "start_line".to_string(),
                serde_json::json!(entity_presentation_start_line(entity)),
            );
            object.insert(
                "end_line".to_string(),
                serde_json::json!(entity_presentation_end_line(entity)),
            );
        }
    }

    let Some(entity_impacts) = result
        .get_mut("entity_impacts")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for row in entity_impacts {
        let Some(object) = row.as_object_mut() else {
            continue;
        };
        if object.contains_key("covering_tests") {
            object.insert(
                COVERING_TESTS_BOUND_KEY.to_string(),
                serde_json::json!(COVERING_TESTS_BOUND),
            );
        }
    }
}

/// Replace each blast-radius row, a whole serialized `Entity`, with the fields
/// a reader acts on: its id, name, kind, role, signature, the file it is
/// projected into and its presentation lines.
///
/// A raw row carried the fingerprint hashes, the span's byte offsets, the
/// language, visibility, lineage and every metadata key the parser stamped on
/// the entity, which is most of its bytes and none of the answer. An impact
/// reply on a method with five affected units came back cut, with no cursor,
/// because those bytes filled the budget first. `compact: false` keeps the
/// raw rows for a caller that wants them.
///
/// An override row also names the edge that found it, the same way
/// `find_references` names it under `overridden_by`: its resolution, and the
/// method it overrides when that is not the changed entity itself.
fn compact_impact_rows<G: GraphStore>(
    store: &G,
    result: &mut serde_json::Value,
    impact: &kin_review::ImpactReport,
) -> Result<()> {
    // Keyed by the id as the row serializes it, which is how the row is found.
    let mut override_edges: HashMap<String, serde_json::Value> = HashMap::new();
    if !impact.affected_overrides.is_empty() {
        for changed in &impact.changed_ids {
            for row in super::overrides::collect_overriders(store, changed)?.rows {
                override_edges
                    .entry(row.entity.id.to_string())
                    .or_insert_with(|| {
                        let mut edge = serde_json::json!({"resolution": row.resolution.as_str()});
                        if let Some(base) = &row.overrides {
                            edge["overrides"] = serde_json::json!(base);
                        }
                        if row.implements {
                            edge["relation"] = serde_json::json!("implements");
                        }
                        edge
                    });
            }
        }
    }
    // The dotted module each row is declared in, read once per file from
    // the graph's module records: what places a row in its package without a
    // second call, as a reference row's `module` does. Never a file path.
    // With it, whether the row's name is one its source unit declares more
    // than once, so the row is written with its id.
    let mut modules: HashMap<String, Option<String>> = HashMap::new();
    let mut module_of = |id: Option<&str>| -> Option<(String, bool)> {
        let id = kin_model::ids::EntityId(uuid::Uuid::parse_str(id?).ok()?);
        let entity = store.get_entity(&id).ok().flatten()?;
        let file = entity.file_origin.as_ref()?.0.clone();
        let shared = crate::semantic_owners::name_is_shared(store, &entity);
        if let Some(known) = modules.get(&file) {
            return known.clone().map(|module| (module, shared));
        }
        let named = crate::semantic_owners::entity_module(store, &entity);
        modules.insert(file, named.clone());
        named.map(|module| (module, shared))
    };
    for key in IMPACT_ENTITY_BUCKETS {
        let Some(serde_json::Value::Array(rows)) = result.get_mut(key) else {
            continue;
        };
        for row in rows.iter_mut() {
            let Some(object) = row.as_object() else {
                continue;
            };
            let field = |name: &str| object.get(name).cloned().unwrap_or(serde_json::Value::Null);
            let mut compact = serde_json::json!({
                "id": field("id"),
                "name": field("name"),
                "kind": field("kind"),
                "role": field("role"),
                "signature": field("signature"),
                "projection": { "path": field("file_origin") },
                "start_line": field("start_line"),
                "end_line": field("end_line"),
            });
            if let Some((module, shared)) =
                module_of(object.get("id").and_then(serde_json::Value::as_str))
            {
                compact["module"] = serde_json::json!(module);
                if shared {
                    compact[crate::semantic_owners::NAME_SHARED_KEY] = serde_json::json!(true);
                }
            }
            if key == "affected_overrides" {
                let edge = object
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|id| override_edges.get(id));
                if let Some(edge) = edge {
                    for (name, value) in edge.as_object().into_iter().flatten() {
                        compact[name] = value.clone();
                    }
                }
            }
            // An override caller keeps the override it calls and how firmly,
            // which is what the row answers.
            if super::implementation_family::OVERRIDE_CALLER_BUCKETS.contains(&key) {
                for label in super::implementation_family::OVERRIDE_CALLER_LABELS {
                    if let Some(value) = object.get(label) {
                        compact[label] = value.clone();
                    }
                }
            }
            *row = compact;
        }
    }
    Ok(())
}

/// Where a compact impact reply says how many rows each of its row lists
/// holds in all, by the list's key.
pub const IMPACT_ROW_TOTALS_KEY: &str = "affected_row_totals";

/// Where an impact reply asked to leave test code out says how many affected
/// entities it left out for that, under `entities`.
pub const IMPACT_TESTS_EXCLUDED_KEY: &str = "tests_excluded";

/// Present, and true, on an impact reply whose caller asked for the lists of
/// what depends on the callers. Those lists answer what else a change could
/// reach, which a reader asking what must change with an entity did not ask
/// for, so a plain reply counts them unless this says they were asked for.
/// The machine reply holds them either way.
pub const IMPACT_DEPENDENTS_ASKED_KEY: &str = "dependents_asked";

/// The argument that asks for [`IMPACT_DEPENDENTS_ASKED_KEY`].
pub const IMPACT_DEPENDENTS_ARGUMENT: &str = "dependents";

/// Where an impact reply confined to a module path says so: the path as the
/// graph spells it under `module`, and under `outside` how many affected
/// entities sat outside it and are in no list.
pub const IMPACT_ROW_SCOPE_KEY: &str = "row_scope";

/// Keep, in every row list of `result`, the rows whose entity `within`
/// lists, and return how many distinct entities that left out. A row whose
/// entity cannot be placed is kept, so nothing is dropped unseen.
fn confine_impact_rows<G: GraphStore>(
    store: &G,
    result: &mut serde_json::Value,
    within: &crate::trace_scope::RowScope,
) -> usize {
    let mut outside: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut placed: HashMap<String, bool> = HashMap::new();
    for key in IMPACT_ENTITY_BUCKETS.into_iter().chain([
        IMPACT_VALUE_USES_KEY,
        crate::impact_sections::SAME_NAME_KEY,
        crate::impact_sections::INTERFACE_IMPLEMENTATIONS_KEY,
    ]) {
        let Some(serde_json::Value::Array(rows)) = result.get_mut(key) else {
            continue;
        };
        rows.retain(|row| {
            let Some(id) = row.get("id").and_then(serde_json::Value::as_str) else {
                return true;
            };
            let listed = *placed
                .entry(id.to_string())
                .or_insert_with(|| within.lists_id(store, id));
            if !listed {
                outside.insert(id.to_string());
            }
            listed
        });
    }
    outside.len()
}

/// The entities that hold a changed entity, or a method that overrides one,
/// as a value: each names it without calling it, by a `References` edge, the
/// rows `find_references` serves as references. A change to a signature
/// reaches them as it reaches a caller, since whatever is handed the value
/// calls it with the old one. Each row carries the line that names it as
/// `code`, and `uses` when what it names is an override.
pub const IMPACT_VALUE_USES_KEY: &str = "affected_value_uses";

/// How many [`IMPACT_VALUE_USES_KEY`] rows the store held past
/// [`IMPACT_VALUE_USES_MAX`].
pub const IMPACT_VALUE_USES_WITHHELD_KEY: &str = "affected_value_uses_withheld";

/// How many value-use rows one answer lists; the rest are counted.
pub const IMPACT_VALUE_USES_MAX: usize = 50;

/// How many rows of one list are quoted with the line that makes them a row.
/// Each quote reads its holder's body once; a list past this keeps its rows
/// and leaves the later ones unquoted.
const IMPACT_CODE_ROWS_MAX: usize = 60;

/// The line of `holder` on which it names one of `targets` by a relation of
/// one of `kinds`, quoted from the holder's own text: the first such line in
/// the holder. `None` when the edge records no site in the holder's file or
/// its text cannot be read.
fn site_code<G: GraphStore, T: super::external_symbols::SiteText + ?Sized>(
    store: &G,
    quoter: &crate::trace_reach::LineQuoter<'_, T>,
    holder: &kin_model::Entity,
    targets: &[kin_model::EntityId],
    kinds: &[kin_model::relation::RelationKind],
) -> Option<String> {
    let line = store
        .get_relations(&holder.id, kinds)
        .ok()?
        .iter()
        .filter(|relation| {
            relation.src == kin_model::GraphNodeId::Entity(holder.id)
                && targets
                    .iter()
                    .any(|target| relation.dst == kin_model::GraphNodeId::Entity(*target))
        })
        .flat_map(|relation| relation_reference_lines(relation, holder.file_origin.as_ref()).lines)
        .min()?;
    quoter.line(holder, line)
}

/// Put on each caller row the call it makes, as `code`: the line of the
/// caller that calls a changed entity, or the override the row names. A
/// reader then sees what a change to the signature has to rewrite without
/// reading the caller.
fn attach_call_code<G: GraphStore, T: super::external_symbols::SiteText + ?Sized>(
    store: &G,
    result: &mut serde_json::Value,
    changed: &[kin_model::EntityId],
    text: &T,
) {
    use kin_model::relation::RelationKind;
    let quoter = crate::trace_reach::LineQuoter::new(text);
    let entity_of = |id: Option<&serde_json::Value>| {
        let id = uuid::Uuid::parse_str(id?.as_str()?).ok()?;
        store.get_entity(&kin_model::EntityId(id)).ok().flatten()
    };
    let lists = ["affected_callers"]
        .into_iter()
        .chain(super::implementation_family::OVERRIDE_CALLER_BUCKETS);
    for key in lists {
        let Some(serde_json::Value::Array(rows)) = result.get_mut(key) else {
            continue;
        };
        for row in rows.iter_mut().take(IMPACT_CODE_ROWS_MAX) {
            let Some(holder) = entity_of(row.get("id")) else {
                continue;
            };
            // An override caller's call is the one to the override it names.
            let named = entity_of(row.get("calls_override_id")).map(|named| vec![named.id]);
            let targets = named.as_deref().unwrap_or(changed);
            if let Some(code) = site_code(store, &quoter, &holder, targets, &[RelationKind::Calls])
            {
                row["code"] = serde_json::json!(code);
            }
        }
    }
}

/// The rows of [`IMPACT_VALUE_USES_KEY`], and how many more the store held
/// past [`IMPACT_VALUE_USES_MAX`]: every entity with a `References` edge to
/// one of `changed` or of `overrides`, each once, in the order the targets
/// are given and then by name. An entity already listed as a caller is left
/// to that row, and a target is never a use of itself.
fn value_use_rows<G: GraphStore, T: super::external_symbols::SiteText + ?Sized>(
    store: &G,
    changed: &[kin_model::Entity],
    overrides: &[kin_model::Entity],
    listed: &std::collections::HashSet<kin_model::EntityId>,
    text: &T,
) -> (Vec<serde_json::Value>, usize) {
    use kin_model::relation::RelationKind;
    let quoter = crate::trace_reach::LineQuoter::new(text);
    let targets: std::collections::HashSet<kin_model::EntityId> = changed
        .iter()
        .chain(overrides)
        .map(|entity| entity.id)
        .collect();
    let mut seen = listed.clone();
    let mut rows = Vec::new();
    let mut withheld = 0usize;
    for (target, is_override) in changed
        .iter()
        .map(|entity| (entity, false))
        .chain(overrides.iter().map(|entity| (entity, true)))
    {
        // Every relation of the target, both ways: the ones that end on it
        // are its referrers.
        let Ok(relations) = store.get_all_relations_for_entity(&target.id) else {
            continue;
        };
        let mut holders: Vec<kin_model::Entity> = relations
            .iter()
            .filter(|relation| {
                relation.kind == RelationKind::References
                    && relation.dst == kin_model::GraphNodeId::Entity(target.id)
            })
            .filter_map(|relation| match relation.src {
                kin_model::GraphNodeId::Entity(holder) => Some(holder),
                _ => None,
            })
            .filter(|holder| !targets.contains(holder) && seen.insert(*holder))
            .filter_map(|holder| store.get_entity(&holder).ok().flatten())
            .collect();
        holders.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        for holder in holders {
            if rows.len() >= IMPACT_VALUE_USES_MAX {
                withheld += 1;
                continue;
            }
            let mut row = serde_json::json!({
                "id": holder.id.to_string(),
                "name": holder.name,
                "kind": holder.kind,
                "role": holder.role,
            });
            if let Some(module) = crate::semantic_owners::entity_module(store, &holder) {
                row["module"] = serde_json::json!(module);
            }
            crate::semantic_owners::mark_shared_name(store, &holder, &mut row);
            if is_override || changed.len() > 1 {
                row["uses"] = serde_json::json!(target.name);
            }
            if let Some(code) = site_code(
                store,
                &quoter,
                &holder,
                &[target.id],
                &[RelationKind::References],
            ) {
                row["code"] = serde_json::json!(code);
            }
            rows.push(row);
        }
    }
    (rows, withheld)
}

/// How many rows each row list of `result` holds before any page cuts one,
/// so a reader of one page can tell a whole list from the first part of one.
fn impact_row_totals(result: &serde_json::Value) -> serde_json::Value {
    let mut totals = serde_json::Map::new();
    for key in IMPACT_ENTITY_BUCKETS
        .into_iter()
        .chain([IMPACT_VALUE_USES_KEY])
    {
        if let Some(serde_json::Value::Array(rows)) = result.get(key) {
            totals.insert(key.to_string(), serde_json::json!(rows.len()));
        }
    }
    serde_json::Value::Object(totals)
}

/// Take every row whose entity's stored role is Test out of the row lists of
/// `result`, and return how many distinct entities that was. A reader who
/// asked for no test code is answered about the rest: the rows are gone
/// rather than marked, and the count says what the answer leaves out.
fn exclude_test_rows(result: &mut serde_json::Value) -> usize {
    let mut excluded: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut unnamed = 0usize;
    for key in IMPACT_ENTITY_BUCKETS.into_iter().chain([
        IMPACT_VALUE_USES_KEY,
        crate::impact_sections::SAME_NAME_KEY,
        crate::impact_sections::INTERFACE_IMPLEMENTATIONS_KEY,
    ]) {
        let Some(serde_json::Value::Array(rows)) = result.get_mut(key) else {
            continue;
        };
        rows.retain(|row| {
            let test = row
                .get("role")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|role| role.eq_ignore_ascii_case("test"));
            if test {
                match row.get("id").and_then(serde_json::Value::as_str) {
                    Some(id) => {
                        excluded.insert(id.to_string());
                    }
                    None => unnamed += 1,
                }
            }
            !test
        });
    }
    excluded.len() + unnamed
}

pub async fn handle_impact_analysis<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
    sessions: &SessionRegistry,
) -> Result<ToolCallResult> {
    handle_impact_analysis_with_source(
        args,
        store,
        sessions,
        None,
        EntitySourceScope::WorkspaceHead,
    )
    .await
}

pub async fn handle_impact_analysis_with_source<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
    sessions: &SessionRegistry,
    repository_authority: Option<&RequestRepositoryAuthority>,
    source_scope: EntitySourceScope,
) -> Result<ToolCallResult> {
    // What a change to a symbol outside the repository reaches is its callers
    // here, and find_references lists them with each call's proof. The
    // impact walk and the consumer counts it reports are keyed by repository
    // entities, so the symbol is refused by what it is.
    if let Some(refusal) = external_entity_ids_refusal(
        args,
        store,
        "impact_analysis",
        "has no change of its own here to analyze, and what a change to it reaches is \
         its callers",
    )? {
        return Ok(refusal);
    }
    let include_traffic = get_optional_bool(args, "include_traffic", true);
    let include_tests = get_optional_bool(args, "include_tests", true);
    // A scope lists the affected entities inside one module path. Resolved
    // before the analysis runs, so one that names no module path is refused
    // rather than answered as though nothing in it were affected.
    let within = crate::trace_scope::RowScope::from_arguments(store, args)
        .map_err(McpError::InvalidParams)?;
    let depth = get_optional_u64(args, "depth", 3) as u32;
    let diff = resolve_diff(args, store)?;

    let impact = {
        let source = AnalysisEscapeEvidence::new(store, repository_authority, source_scope);
        let evidence = |targets: &[kin_model::Entity], at| source.observe(targets, at);
        // A caller with no ledger reads the way the store-wide status reads
        // it: unproven for want of a resolver on this host, not owed work.
        let no_resolver = crate::call_sites::host_no_resolver();
        kin_review::impact::analyze_impact_with_sources(
            store,
            &diff,
            kin_review::enrichment::ObservationSources {
                escape_evidence: Some(&evidence),
                no_resolver: Some(&no_resolver),
            },
        )
        .map_err(|e| McpError::Review(e.to_string()))?
    };

    let mut result = semantic_metadata_json(&impact)?;
    super::implementation_family::flatten_rows(&mut result, &impact);
    annotate_impact_presentation_lines(&mut result, &impact);
    let compact = crate::budget::ResponseBudget::from_arguments(args).compact;
    if compact {
        compact_impact_rows(store, &mut result, &impact)?;
    }
    // A class the parser saw and did not admit, whose bases name a changed
    // method's owner, could override it with no entity to carry the edge.
    // Named beside `affected_overrides`, the way `find_references` names it.
    let mut not_admitted = Vec::new();
    for id in &impact.changed_ids {
        if let Some(entity) = store.get_entity(id).map_err(McpError::graph)? {
            not_admitted.extend(super::overrides::unadmitted_overriders(store, &entity)?);
        }
    }
    if !not_admitted.is_empty() {
        result["overrides_not_admitted"] = serde_json::json!(not_admitted);
    }
    if names_files(args) {
        record_files_deprecation(&mut result, "impact_analysis");
    }

    if include_traffic {
        // Collect traffic for all changed entities.
        let mut all_traffic = Vec::new();
        for change in &diff.entity_changes {
            let traffic = sessions.get_traffic_near_entity(&change.entity_id);
            for summary in traffic {
                if !all_traffic
                    .iter()
                    .any(|t: &kin_model::session::IntentSummary| t.intent_id == summary.intent_id)
                {
                    all_traffic.push(summary);
                }
            }
        }
        if !all_traffic.is_empty() {
            result["active_traffic"] =
                serde_json::to_value(&all_traffic).map_err(McpError::Json)?;
        }
    }

    // ── Cross-repo federation via spine ──────────────────────────────────
    let repo_id = std::env::var("KIN_REPO_ID").unwrap_or_else(|_| "unknown".into());
    let changed_ids = diff.changed_entity_ids();
    let mut cross_repo_nodes: Vec<kin_spine::FederatedNode> = Vec::new();
    let mut spine_unavailable: Option<String> = None;

    for eid in &changed_ids {
        match fetch_spine_impact_typed(&repo_id, eid, depth).await {
            kin_spine::SpineQuery::Found(federated) => {
                for node in federated.nodes {
                    if node.repo_id != repo_id {
                        cross_repo_nodes.push(node);
                    }
                }
            }
            // Configured but a query failed — record the first reason so the
            // result reports the gap rather than silently omitting cross-repo
            // impact (which would read as "analyzed, none").
            kin_spine::SpineQuery::Unavailable(reason) => {
                spine_unavailable.get_or_insert(reason);
            }
            // No spine in this context (local-only MCP server): a quiet absence
            // of cross-repo impact is correct, so stay non-noisy.
            kin_spine::SpineQuery::NotConfigured => {}
        }
    }

    if !cross_repo_nodes.is_empty() {
        result["cross_repo_impact"] =
            serde_json::to_value(&cross_repo_nodes).map_err(McpError::Json)?;
    }
    if let Some(reason) = spine_unavailable {
        // Additive, failure-only field: present only when the spine was
        // expected but unreachable, so an empty/absent cross_repo_impact is
        // never mistaken for a healthy "no cross-repo impact" result.
        result["cross_repo_impact_status"] =
            serde_json::json!(format!("spine_unavailable: {reason}"));
    }

    // FIR-2452. Every `entity_impacts` row carrying no consumers is a
    // used/unused verdict a caller reads before changing or deleting something,
    // and it is read off the same cross-file reference edges `find_references`
    // reads. Until this observation existed, `impact_analysis` was the one
    // retrieval surface with no `negative` object at all, so the tool with the
    // highest blast radius per wrong absence was the only one outside the gate
    // every smaller one passes.
    //
    // The languages are the CHANGED entities' own. A verdict covers all of them
    // and the weakest governs, which is the same rule the batch reachability
    // surface applies for the same reason: one language whose reference edges
    // were never produced must not have its absences certified by a sibling
    // language that links cleanly. Unresolvable ids contribute no language
    // rather than a guessed one.
    let impact_languages = crate::edge_coverage::languages_of(
        &impact
            .changed_ids
            .iter()
            .filter_map(|id| store.get_entity(id).ok().flatten())
            .collect::<Vec<_>>(),
    );
    result[crate::edge_coverage::EDGE_COVERAGE_KEY] =
        crate::edge_coverage::observe_cross_file_reference_coverage_for_languages(
            store,
            &impact_languages,
            &IMPACT_REFERENCE_KINDS,
        );

    // Whether a caller could have reached an entity this answer reports with no
    // consumers through a call the graph holds no edge for. Coverage above says
    // the graph holds edges of each class; it cannot see a caller whose own call
    // became no edge, which is how a live export came back `consumer_count: 0`
    // under a certified verdict. The negative gate reads this block before it
    // certifies any of those zeros, the way it reads `find_references`'s own.
    //
    // The arrival is read over the same store-wide reading of each changed
    // entity's possible callers that `find_references` reads its own over, so
    // the two answers about one focal read its family the same way.
    let held = HeldSourceAuthority::new(store, repository_authority);
    let (mut scans, changed) = direct_scans(
        store,
        &impact.changed_ids,
        &held,
        source_scope,
        include_tests,
        within.as_ref(),
    )?;
    let without_consumers: Vec<kin_model::EntityId> = impact
        .entity_impacts
        .iter()
        .filter(|row| row.consumer_count == 0)
        .map(|row| row.entity_id)
        .collect();
    result[crate::caller_arrival::CALLER_ARRIVAL_KEY] =
        crate::caller_arrival::observe_impact_arrival_with(store, &without_consumers, &scans.scans);

    // A changed method's signature is its whole family's: each override, and
    // the callers of every member. Each entry says what changing it obliges
    // and where those rows are, and the callers of the method itself are
    // labelled proven or not beside the callers of its overrides.
    let families = super::implementation_family::read(store, &changed, &impact, &scans.scans)?;
    families.label_direct_callers(&mut result);
    families.attach(&mut result);

    // Each caller row shows the call it makes, and the places that hold a
    // changed entity or an override as a value are listed beside the callers:
    // a change reaches them the same way, and no call edge names them.
    {
        let text = super::external_symbols::CalleeText::with_read_limit_at(
            &held,
            crate::call_sites::SCAN_TEXT_READS_MAX,
            source_scope,
        );
        attach_call_code(store, &mut result, &impact.changed_ids, &text);
        let listed: std::collections::HashSet<kin_model::EntityId> = impact
            .affected_callers
            .iter()
            .map(|caller| caller.id)
            .collect();
        let (uses, withheld) =
            value_use_rows(store, &changed, &impact.affected_overrides, &listed, &text);
        if !uses.is_empty() {
            result[IMPACT_VALUE_USES_KEY] = serde_json::json!(uses);
            if withheld > 0 {
                result[IMPACT_VALUE_USES_WITHHELD_KEY] = serde_json::json!(withheld);
            }
        }
    }

    // A JavaScript or TypeScript function swapped in by module, such as a
    // renderer's host config, has siblings no edge links to it. They are
    // named beside the overrides as name matches, and never graded.
    let (same_name, withheld) = crate::impact_sections::same_name_implementations(
        store,
        &changed,
        &impact.affected_overrides,
    );
    if !same_name.is_empty() {
        result[crate::impact_sections::SAME_NAME_KEY] = serde_json::json!(same_name);
        result[crate::impact_sections::SAME_NAME_NOTE_KEY] =
            serde_json::json!(crate::impact_sections::SAME_NAME_NOTE);
        if withheld > 0 {
            result[crate::impact_sections::SAME_NAME_WITHHELD_KEY] = serde_json::json!(withheld);
        }
        // The siblings no row can name: a variable bound to a function, or
        // a re-export, of the same name.
        let not_held = crate::impact_sections::same_name_not_held(store, &changed);
        if not_held > 0 {
            result[crate::impact_sections::SAME_NAME_NOT_HELD_KEY] = serde_json::json!(not_held);
        }
    }

    // A Go interface method is implemented by every type whose method set
    // satisfies its interface, and no edge records that, so the overrides
    // above never hold them. They are listed beside the overrides, and a walk
    // that failed is said in the direct section's reading rather than read as
    // one no method implements.
    let implementations = crate::impact_sections::interface_implementation_rows(
        store,
        &changed,
        &impact.affected_overrides,
    );
    scans.implementations_unread = implementations.unread;
    if !implementations.rows.is_empty() {
        result[crate::impact_sections::INTERFACE_IMPLEMENTATIONS_KEY] =
            serde_json::json!(implementations.rows);
        result[crate::impact_sections::INTERFACE_IMPLEMENTATIONS_NOTE_KEY] =
            serde_json::json!(crate::impact_sections::INTERFACE_IMPLEMENTATIONS_NOTE);
    }

    // Confined to a module path, the lists hold what sits inside it, and
    // what they left out is counted once.
    if let Some(within) = &within {
        let outside = confine_impact_rows(store, &mut result, within);
        result[IMPACT_ROW_SCOPE_KEY] =
            serde_json::json!({ "module": within.module(), "outside": outside });
    }
    // Test code a caller asked to leave out is in no list and in no reading
    // of the direct callers above; what the lists left out is counted once.
    if !include_tests {
        let entities = exclude_test_rows(&mut result);
        result[IMPACT_TESTS_EXCLUDED_KEY] = serde_json::json!({ "entities": entities });
    }
    if compact {
        result[IMPACT_ROW_TOTALS_KEY] = impact_row_totals(&result);
    }
    if get_optional_bool(args, IMPACT_DEPENDENTS_ARGUMENT, false) {
        result[IMPACT_DEPENDENTS_ASKED_KEY] = serde_json::json!(true);
    }

    // Two questions, two verdicts: what must change with the changed
    // entities, read off their call sites the way `find_references` reads
    // them, and what else the change could reach, read off the wider walk.
    // The envelope grades each section from what is named here.
    let sections = crate::impact_sections::sections_json(&result, scans.call_sites_block());
    result[crate::impact_sections::SECTIONS_KEY] = sections;

    let json = serde_json::to_string_pretty(&result).map_err(McpError::Json)?;
    Ok(ToolCallResult::text(json))
}

/// The store-wide reading of every caller that could call each changed
/// entity, taken the way `find_references` takes one for its focal: the
/// census of whether it escapes as a value at `source_scope`, then every
/// possible caller read through the one site-state reading, with each
/// caller's exact text read within [`crate::call_sites::SCAN_TEXT_READS_MAX`].
/// With `include_tests` false, code whose stored role is Test is in neither
/// the census nor the reading, as a reference answer that leaves tests out
/// reads it, and under `within` a caller outside that module path holds no
/// site in the reading. Also the changed entities the store holds, in the
/// answer's order.
fn direct_scans<G: GraphStore>(
    store: &G,
    changed_ids: &[kin_model::EntityId],
    held: &HeldSourceAuthority<'_, G>,
    source_scope: EntitySourceScope,
    include_tests: bool,
    within: Option<&crate::trace_scope::RowScope>,
) -> Result<(crate::impact_sections::DirectScans, Vec<kin_model::Entity>)> {
    use crate::impact_sections::{DirectScans, DIRECT_SCAN_FOCALS_MAX};
    let mut scans = DirectScans {
        changed: changed_ids.len(),
        ..DirectScans::default()
    };
    let mut changed = Vec::with_capacity(changed_ids.len());
    for id in changed_ids {
        match store.get_entity(id).map_err(McpError::graph)? {
            Some(entity) => changed.push(entity),
            None => scans.missing += 1,
        }
    }
    let read = &changed[..changed.len().min(DIRECT_SCAN_FOCALS_MAX)];
    scans.unscanned = changed.len() - read.len();
    let focals: Vec<(&kin_model::Entity, Vec<String>)> = read
        .iter()
        .map(|focal| (focal, kin_model::focal_call_names(focal)))
        .collect();
    let mut escapes = if focals.is_empty() {
        Vec::new()
    } else {
        held.escape_evidence_batch_scoped(source_scope, &focals, include_tests)
    };
    if escapes.len() != focals.len() {
        // A census that does not answer for every focal answers for none.
        escapes = focals
            .iter()
            .map(|_| crate::call_sites::NO_CENSUS)
            .collect();
    }
    for ((focal, _), escape) in focals.iter().zip(escapes) {
        let text = super::external_symbols::CalleeText::with_read_limit_at(
            held,
            crate::call_sites::SCAN_TEXT_READS_MAX,
            source_scope,
        );
        match crate::call_sites::scan_focal_scoped(store, focal, &text, escape, include_tests) {
            Ok(mut scan) => {
                // The reading speaks of the callers the question is about.
                if let Some(within) = within {
                    scan.confine(|caller| within.lists(caller));
                }
                scans.scans.insert(focal.id, std::sync::Arc::new(scan));
                scans.focals.push((*focal).clone());
            }
            Err(_) => scans.unreadable.push(focal.name.clone()),
        }
    }
    Ok((scans, changed))
}

/// The cross-file reference classes an impact verdict is read off, matching what
/// [`crate::negative::absence_cross_unit_classes`] declares `impact_analysis`
/// depends on. Declaring one set and observing another is how a gate comes to
/// judge a class the answer never measured.
///
/// Public because the CLI's `kin impact` renders the same verdict over the same
/// classes (FIR-2524). Two consumers reading one declaration is the whole point;
/// a second copy in `kin-cli` would be the drift this comment already warns
/// about, arriving by the door it does not watch.
pub const IMPACT_REFERENCE_KINDS: [kin_model::relation::RelationKind; 3] = [
    kin_model::relation::RelationKind::Calls,
    kin_model::relation::RelationKind::Imports,
    kin_model::relation::RelationKind::References,
];

pub const SEMANTIC_REVIEW_DESC: &str = "\
Return one structured semantic report containing entity and relation changes, downstream \
impact, risk and inline findings. Select one scope: base/head semantic change IDs, \
entity_ids, or change_ids. Both formats retain the complete structured report. The \
default format='text' adds a concise semantic prose message; format='json' omits that \
message. Optionally include active agent traffic on the reviewed entities. Use \
semantic_diff or impact_analysis when you need only that part of the report.";

pub fn handle_semantic_review<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
    sessions: &SessionRegistry,
) -> Result<ToolCallResult> {
    handle_semantic_review_with_source(
        args,
        store,
        sessions,
        None,
        EntitySourceScope::WorkspaceHead,
    )
}

pub fn handle_semantic_review_with_source<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
    sessions: &SessionRegistry,
    repository_authority: Option<&RequestRepositoryAuthority>,
    source_scope: EntitySourceScope,
) -> Result<ToolCallResult> {
    if let Some(refusal) = external_entity_ids_refusal(
        args,
        store,
        "semantic_review",
        "has no change of its own here to review",
    )? {
        return Ok(refusal);
    }
    let include_traffic = get_optional_bool(args, "include_traffic", true);
    let format = match args.get("format") {
        None => "text",
        Some(serde_json::Value::String(format)) if matches!(format.as_str(), "text" | "json") => {
            format.as_str()
        }
        Some(_) => {
            return Err(McpError::InvalidParams(
                "semantic_review format must be text or json".into(),
            ))
        }
    };
    let diff = resolve_diff(args, store)?;

    let source = AnalysisEscapeEvidence::new(store, repository_authority, source_scope);
    let evidence = |targets: &[kin_model::Entity], at| source.observe(targets, at);
    let review = SemanticReview::review_from_diff_with_source(diff, store, Some(&evidence))
        .map_err(|e| McpError::Review(e.to_string()))?;

    // Both formats carry the same typed evidence through qualification. Prose
    // is composed from semantic counts, never parsed out of operator output.
    let mut result = semantic_metadata_json(&review)?;
    if let Some(impact) = result.get_mut("impact") {
        annotate_impact_presentation_lines(impact, &review.impact);
    }
    result["summary"] = serde_json::json!(format!("Risk: {:?}", review.risk.overall_risk));
    if format == "text" {
        result["message"] = serde_json::json!(format!(
            "Semantic review: {} entity changes and {} relationship changes. Risk: {:?}. Read the structured diff, impact, risk and inline findings with their verdicts and coverage limits.",
            review.diff.entity_changes.len(), review.diff.relation_changes.len(), review.risk.overall_risk,
        ));
    }
    if include_traffic {
        let traffic = collect_review_traffic_lines(&review, sessions);
        if !traffic.is_empty() {
            result["active_traffic"] = serde_json::json!(traffic);
        }
    }
    if names_files(args) {
        record_files_deprecation(&mut result, "semantic_review");
    }
    let json = serde_json::to_string_pretty(&result).map_err(McpError::Json)?;
    Ok(ToolCallResult::text(json))
}

fn collect_review_traffic_lines(
    review: &kin_review::Review,
    sessions: &SessionRegistry,
) -> Vec<String> {
    let mut traffic_lines = Vec::new();
    for change in &review.diff.entity_changes {
        let traffic = sessions.get_traffic_near_entity(&change.entity_id);
        for summary in &traffic {
            traffic_lines.push(format!(
                "  {} ({}) is {} entity {} [{}]",
                summary.vendor,
                summary.session_id,
                summary.task_description,
                change.entity_id,
                summary.lock_type_label(),
            ));
        }
    }
    traffic_lines
}

pub const SHADOW_GATE_REPORT_DESC: &str = "\
Run the shadow-mode merge gate over a PR-shaped change (base ref .. head ref) and return \
ONE report: changed entities, graph-proven blast radius, the policy verdict the gate \
WOULD have issued (report-only — shadow mode never blocks), the repair context a reviewer \
or agent needs to fix findings, explicit evidence gaps, and audit evidence for the \
evaluation. Refs accept branch names and semantic change IDs; imported Git commit SHAs \
resolve when their history has been imported into the graph. When the graph cannot prove \
something — unparsed files, missing spans, an empty impact signal — the report says so in \
`evidence_gaps` instead of passing silently. Reach for it to evaluate an AI-authored \
change before merge, or to feed a merge-gate dashboard.";

fn resolve_shadow_ref<G: GraphStore>(
    store: &G,
    reference: &str,
    repository_authority: Option<&RequestRepositoryAuthority>,
) -> Result<SemanticChangeId> {
    if let Some(branch_name) = reference.strip_prefix("branch:") {
        return resolve_shadow_branch(store, branch_name, repository_authority);
    }

    if let Some(change_ref) = reference
        .strip_prefix("kin:")
        .or_else(|| reference.strip_prefix("change:"))
    {
        return resolve_shadow_change(store, change_ref);
    }

    if let Some(git_oid) = reference.strip_prefix("git:") {
        return resolve_shadow_git(store, git_oid, repository_authority);
    }

    if reference.len() == 40 {
        return resolve_shadow_git(store, reference, repository_authority);
    }

    if reference.len() == 64 {
        return resolve_shadow_change(store, reference);
    }

    resolve_shadow_branch(store, reference, repository_authority)
}

fn resolve_shadow_branch<G: GraphStore>(
    store: &G,
    branch_name: &str,
    repository_authority: Option<&RequestRepositoryAuthority>,
) -> Result<SemanticChangeId> {
    let authority = repository_authority
        .ok_or_else(|| {
            McpError::Context(
                "graph authority gap: shadow ref resolution requires a startup-pinned local \
                 repository authority binding"
                    .to_string(),
            )
        })?
        .open()?;
    let ref_name = super::repository_authority::parse_branch_ref(branch_name)?;
    let change_id = authority.resolve_named_ref(&ref_name)?;
    ensure_shadow_change(store, change_id, branch_name)
}

fn resolve_shadow_change<G: GraphStore>(store: &G, change_ref: &str) -> Result<SemanticChangeId> {
    let change_id = parse_change_id(change_ref)?;
    ensure_shadow_change(store, change_id, change_ref)
}

fn ensure_shadow_change<G: GraphStore>(
    store: &G,
    change_id: SemanticChangeId,
    reference: &str,
) -> Result<SemanticChangeId> {
    match store.get_change(&change_id).map_err(McpError::graph)? {
        Some(_) => Ok(change_id),
        None => Err(McpError::InvalidParams(format!(
            "change '{}' resolved to {}, which is not materialized in graph authority",
            reference, change_id
        ))),
    }
}

fn resolve_shadow_git<G: GraphStore>(
    store: &G,
    git_oid: &str,
    repository_authority: Option<&RequestRepositoryAuthority>,
) -> Result<SemanticChangeId> {
    let oid = super::repository_authority::parse_git_object_id(git_oid)?;
    let authority = repository_authority
        .ok_or_else(|| {
            McpError::Context(
                "graph authority gap: Git alias resolution requires a startup-pinned local \
                 repository authority binding"
                    .to_string(),
            )
        })?
        .open()?;
    let change_id = authority.resolve_git_oid(oid)?;
    ensure_shadow_change(store, change_id, git_oid)
}

pub fn handle_shadow_gate_report<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
    repository_authority: Option<&RequestRepositoryAuthority>,
) -> Result<ToolCallResult> {
    let base_ref = get_string_param(args, "base")?;
    let head_ref = get_string_param(args, "head")?;
    let resolved_base = resolve_shadow_ref(store, &base_ref, repository_authority)?;
    let resolved_head = resolve_shadow_ref(store, &head_ref, repository_authority)?;

    let request = kin_review::ShadowRequest {
        base_ref,
        head_ref,
        resolved_base,
        resolved_head,
        title: get_optional_string_param(args, "title"),
        source_url: get_optional_string_param(args, "source_url"),
        author: get_optional_string_param(args, "author"),
        actor: get_optional_string_param(args, "actor").unwrap_or_else(|| "mcp-client".into()),
    };

    let report = kin_review::build_shadow_report(store, &request)
        .map_err(|e| McpError::Review(e.to_string()))?;

    let json =
        serde_json::to_string_pretty(&semantic_metadata_json(&report)?).map_err(McpError::Json)?;
    Ok(ToolCallResult::text(json))
}

pub const ENTITY_HISTORY_DESC: &str = "\
Return a bounded, chronological page of changes to one entity, oldest first with \
change-ID tie breaks. result contains focal-entity projections, not replayable whole \
commits: original IDs, origins and parents are exact; unrelated change payloads are \
represented by counts. change_id is the printable native semantic ID for semantic_diff \
or impact_analysis. offset/limit page through history (default 20, maximum 100); follow \
next_offset and compare change_count/latest_change_id between calls to detect concurrent \
changes. An offset, limit or max_chars outside its bounds, or not an integer, is refused. Oversized focal details become an explicit summary, never silently empty history. \
max_chars bounds serialized JSON payload UTF-8 bytes (including the MCP envelope, excluding \
JSON-RPC escaping), default 45000, maximum 60000. Budget cuts retain their disclosures and \
next_offset advances only past rows actually returned. An impossible metadata budget is \
an error. Read negative.safe_to_conclude_absent before concluding no history exists; \
retired entities with recorded history remain queryable.";

/// The page and budget fields `entity_history` takes, with the bounds its
/// registered schema declares: `(name, minimum, maximum)`. In name order, the
/// order the routed tool's schema check reports problems in.
const HISTORY_BOUNDED_FIELDS: [(&str, u64, Option<u64>); 3] = [
    ("limit", 1, Some(100)),
    (
        "max_chars",
        crate::budget::RESPONSE_MIN_MAX_CHARS as u64,
        Some(crate::budget::RESPONSE_MAX_MAX_CHARS as u64),
    ),
    ("offset", 0, None),
];

/// Everything wrong with `entity_history`'s page and budget fields, in the
/// words the routed tool's schema check uses for the same field, so a call is
/// refused the same way by whichever name reached it. A value out of range, or
/// not an integer, is refused rather than clamped: a clamped page answers a
/// different question than the one asked. A null reads as absent.
fn history_parameter_problems(args: &HashMap<String, serde_json::Value>) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, minimum, maximum) in HISTORY_BOUNDED_FIELDS {
        let Some(value) = args.get(name).filter(|value| !value.is_null()) else {
            continue;
        };
        if !(value.is_i64() || value.is_u64()) {
            problems.push(format!("{name} must be an integer"));
        } else if value.as_i64().is_some_and(|number| number < minimum as i64) {
            problems.push(format!("{name} must be at least {minimum}"));
        } else if let (Some(number), Some(maximum)) = (value.as_u64(), maximum) {
            if number > maximum {
                problems.push(format!("{name} must be at most {maximum}"));
            }
        }
    }
    problems
}

/// The structured refusal for page or budget fields that are out of range or
/// the wrong type. Its message is the sentence the routed tool refuses with.
fn history_parameter_refusal(problems: &[String]) -> ToolCallResult {
    ToolCallResult::error(
        serde_json::json!({"error": {
            "code": "history_parameters_out_of_range",
            // The tool's name first, as the routed tool's refusal names it.
            "message": format!("{}: {}.", "entity_history", problems.join("; ")),
            "problems": problems,
            "accepted": {
                "offset": {"type": "integer", "minimum": 0, "default": 0},
                "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20},
                "max_chars": {
                    "type": "integer",
                    "minimum": crate::budget::RESPONSE_MIN_MAX_CHARS,
                    "maximum": crate::budget::RESPONSE_MAX_MAX_CHARS,
                    "default": crate::budget::RESPONSE_DEFAULT_MAX_CHARS,
                },
            },
        }})
        .to_string(),
    )
}

pub fn handle_entity_history<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<ToolCallResult> {
    let id_str = get_string_param(args, "entity_id")?;
    if let Some(refusal) = super::external_symbols::external_id_refusal(
        store,
        &id_str,
        "entity_history",
        "entity_id",
        "has no revisions of it here to list",
    )? {
        return Ok(refusal);
    }
    let entity_id = parse_entity_id(&id_str)?;
    let problems = history_parameter_problems(args);
    if !problems.is_empty() {
        return Ok(history_parameter_refusal(&problems));
    }
    let offset = usize::try_from(get_optional_u64(args, "offset", 0)).unwrap_or(usize::MAX);
    let limit = get_optional_u64(args, "limit", 20) as usize;
    let page = store
        .get_entity_history_page(&entity_id, offset, limit)
        .map_err(McpError::graph)?;
    if page.change_count == 0
        && store
            .get_entity(&entity_id)
            .map_err(McpError::graph)?
            .is_none()
    {
        return Ok(ToolCallResult::error(format!(
            "no entity exists with ID '{id_str}' and no change history is recorded against it; \
             resolve the entity ID before requesting history"
        )));
    }
    if let Some((index, entry)) = page
        .entries
        .iter()
        .enumerate()
        .find(|(_, entry)| entry.parents.is_none())
    {
        // Named by its place in the history, with the pages around it, so a
        // caller reads every row it can rather than losing the whole page.
        let offending = offset.saturating_add(index);
        let mut around = Vec::new();
        if index > 0 {
            around.push(serde_json::json!({"offset": offset, "limit": index}));
        }
        if offending.saturating_add(1) < page.change_count {
            around.push(serde_json::json!({"offset": offending + 1, "limit": limit}));
        }
        return Ok(ToolCallResult::error(serde_json::json!({"error": {
            "code": "history_ancestry_exceeds_limit",
            "change_id": entry.id.to_string(),
            "parent_count": entry.metadata_omissions.get("parents"),
            "requested_offset": offset,
            "requested_limit": limit,
            "offending_offset": offending,
            "pages_around_it": around,
            "message": format!(
                "the change at offset {offending} has more parents than bounded history metadata \
                 can carry exactly, so this page was not emitted rather than emitted with partial \
                 ancestry; pages_around_it reads every other row"
            ),
        }}).to_string()));
    }
    let rows = page.entries.iter().enumerate().map(|(index, entry)| {
        let mut row = semantic_metadata_json(entry)?;
        row["change_id"] = serde_json::json!(entry.id.to_string());
        row["scope"] = serde_json::json!("focal_entity");
        row["omitted_sections"] = serde_json::json!({
            "unrelated_entity_deltas": entry.entity_delta_count.saturating_sub(entry.focal_delta_count),
            "relation_deltas": entry.relation_delta_count,
            "tree_deltas": entry.tree_delta_count,
            "external_reference_deltas": entry.external_reference_delta_count,
            "projected_files": entry.projected_file_count,
            "evidence": entry.evidence_count,
            "admission_policy_delta": usize::from(entry.admission_policy_changed),
            "risk_summary": usize::from(entry.risk_summary_present),
        });
        if entry.entity_deltas_omitted > 0 {
            row["detail_summary"] = serde_json::json!({
                "reason": "focal_detail_limit",
                "omitted_entity_deltas": entry.entity_deltas_omitted,
                "detail_limit_bytes": 12000,
                "largest_view": {
                    "tool": "entity_history",
                    "arguments": {
                        "entity_id": id_str,
                        "offset": offset.saturating_add(index),
                        "limit": 1,
                        "max_chars": crate::budget::RESPONSE_MAX_MAX_CHARS,
                    },
                },
                "disclosure": "the focal detail past 12,000 bytes is not available from history at \
                    any budget; the largest view of this row is the one-row page above, and its \
                    operations and original counts are exact",
            });
        }
        Ok(row)
    }).collect::<Result<Vec<_>>>()?;
    let returned = rows.len();
    let next = offset.saturating_add(returned);
    let payload = serde_json::json!({
        "result": rows,
        "entity_id": entity_id.to_string(),
        "scope": "focal_entity",
        "ordering": "timestamp_ascending_then_change_id",
        "change_count": page.change_count,
        "latest_change_id": page.latest_change_id.map(|id| id.to_string()),
        "offset": offset,
        "limit": limit,
        "returned": returned,
        "next_offset": (next < page.change_count).then_some(next),
        "truncated": offset > 0 || next < page.change_count,
        "snapshot_check": "compare change_count and latest_change_id before combining pages",
    });
    Ok(ToolCallResult::text(
        serde_json::to_string_pretty(&payload).map_err(McpError::Json)?,
    ))
}

// ── Review mutation handlers (Phase 11) ──

pub const REVIEW_CREATE_DESC: &str = "\
Open a new review over a set of changes and persist it in the graph. Scope it by \
base/head refs (branch names or change IDs), by KinLab-style scope_type + entity_ids, \
or by raw semantic scopes — and optionally seed a title, description, creator identity, \
and an initial reviewer list. Reach for it to start a code-review workflow that lives \
in graph truth (so decisions, notes, and discussions attach to entities, not just \
files), whether driven by a human, an assistant, or CI. Returns the new review's ID, \
which the other kin_review_* tools (decide, note_add, discuss, assign, get) operate on. \
A scope or entity id naming a symbol outside the repository is refused with \
external_symbol_not_served.";

pub fn handle_review_create<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<ToolCallResult> {
    apply_planned(plan_review_create(args, store)?, store)
}

fn plan_review_create<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<PlannedReviewTool> {
    use kin_model::review::{
        Review, ReviewAssignment, ReviewCompletionState, ReviewDecisionState, ReviewId,
    };
    use kin_model::timestamp::Timestamp;

    let title = get_string_param(args, "title")?;
    let base = get_optional_string_param(args, "base").unwrap_or_else(|| "working-tree".into());
    let head = get_optional_string_param(args, "head").unwrap_or_else(|| "working-tree".into());
    let scopes = parse_review_create_scopes(args, store)?;
    let created_by = parse_identity_arg(args, "created_by", "created_by_kind", "mcp-client");
    // Optional, as the tool's schema says: a review may open with no reviewer.
    let reviewers = parse_optional_reviewer_list(args)?;
    let now = Timestamp::now();

    let review = Review {
        review_id: ReviewId::new(),
        title,
        base_ref: base,
        head_ref: head,
        state: ReviewDecisionState::Pending,
        completion: ReviewCompletionState::InReview,
        scopes,
        created_by: created_by.clone(),
        created_at: now.clone(),
        updated_at: now.clone(),
    };
    let review_id = review.review_id;
    let assignments = (!reviewers.is_empty()).then(|| ReviewGroup {
        review_id,
        entries: reviewers
            .into_iter()
            .map(|reviewer| ReviewAssignment {
                review_id,
                reviewer: kin_model::IdentityRef::human(reviewer),
                assigned_at: now.clone(),
                assigned_by: created_by.clone(),
            })
            .collect(),
    });

    let result = serde_json::json!({
        "review_id": review_id.to_string(),
        "title": review.title,
        "state": "pending",
    });
    let json = serde_json::to_string_pretty(&result).map_err(McpError::Json)?;
    Ok(PlannedReviewEvent {
        action: "review.create",
        review_id,
        details: format!(
            "review_id={}; title={}; base={}; head={}",
            review_id, review.title, review.base_ref, review.head_ref
        ),
        actor_label: created_by.name.clone(),
        refs: Some((review.base_ref.clone(), review.head_ref.clone())),
        write: ReviewWrite {
            review: Some(review),
            assignments,
            ..ReviewWrite::default()
        },
        answer: ToolCallResult::text(json),
    })
}

pub const REVIEW_DECIDE_DESC: &str = "\
Record a reviewer's verdict on a review: approved, needs_work, or blocked, with an \
optional explanatory comment and reviewer identity. Reach for it to land the outcome of \
a review in graph truth so downstream gates (like kin_release_check's approval \
requirement) and other agents can see where the review stands. The decision is appended \
to the review's history rather than overwriting prior verdicts.";

pub fn handle_review_decide<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<ToolCallResult> {
    apply_planned(plan_review_decide(args, store)?, store)
}

fn plan_review_decide<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<PlannedReviewTool> {
    use kin_model::review::ReviewDecision;
    use kin_model::timestamp::Timestamp;

    let review_id = parse_review_id(args, "review_id")?;
    let state_str = get_string_param(args, "state")?;
    let comment_str = get_optional_string_param(args, "comment")
        .or_else(|| get_optional_string_param(args, "summary"))
        .unwrap_or_default();
    let reviewer = parse_identity_arg(args, "reviewer", "reviewer_kind", "mcp-client");

    let state = parse_review_decision_state(&state_str)?;
    let mut review = existing_review(store, &review_id)?;

    let decision = ReviewDecision {
        state,
        comment: if comment_str.is_empty() {
            None
        } else {
            Some(comment_str)
        },
        reviewer: reviewer.clone(),
        decided_at: Timestamp::now(),
    };
    // The decision is history and the review's state is where it stands now, so
    // one event moves both: without this an approved review reads pending on
    // every surface that prints its state.
    review.state = state;
    review.updated_at = decision.decided_at.clone();
    let mut history = store
        .get_review_decisions(&review_id)
        .map_err(|e| McpError::Other(e.to_string()))?;
    history.push(decision);

    let result = serde_json::json!({
        "review_id": review_id.to_string(),
        "state": state_str,
        "reviewer": reviewer.name,
    });
    let json = serde_json::to_string_pretty(&result).map_err(McpError::Json)?;
    Ok(PlannedReviewEvent {
        action: "review.decide",
        review_id,
        details: format!("review_id={review_id}; decision={state_str}"),
        actor_label: reviewer.name,
        refs: None,
        write: ReviewWrite {
            review: Some(review),
            decisions: Some(ReviewGroup {
                review_id,
                entries: history,
            }),
            ..ReviewWrite::default()
        },
        answer: ToolCallResult::text(json),
    })
}

pub const REVIEW_NOTE_ADD_DESC: &str = "\
Attach a standalone note to a review, optionally anchored to a specific entity or file \
(and line). Reach for it to leave a non-blocking observation or comment that doesn't \
need a back-and-forth thread — \"FYI this also affects X\". Because notes can be scoped \
to an entity, they travel with that declaration in graph truth rather than being pinned \
to a line number that drifts. For a comment that expects replies, start a thread with \
kin_review_discuss instead. A scope naming a symbol outside the repository is refused \
with external_symbol_not_served.";

pub fn handle_review_note_add<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<ToolCallResult> {
    apply_planned(plan_review_note_add(args, store)?, store)
}

fn plan_review_note_add<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<PlannedReviewTool> {
    use kin_model::review::{ReviewNote, ReviewNoteId};
    use kin_model::timestamp::Timestamp;

    let review_id = parse_review_id(args, "review_id")?;
    let body = get_string_param(args, "body")?;
    let scope = parse_optional_scope_arg(args, store, "kin_review_note_add")?;
    let author = parse_identity_arg(args, "author", "author_kind", "mcp-client");
    existing_review(store, &review_id)?;

    let note = ReviewNote {
        note_id: ReviewNoteId::new(),
        review_id,
        body: body.clone(),
        scope: scope.clone(),
        authored_by: author.clone(),
        created_at: Timestamp::now(),
    };

    let mut result = serde_json::json!({
        "note_id": note.note_id.to_string(),
        "review_id": review_id.to_string(),
        "scope": scope.map(|s| s.to_string()),
        "author": author.name,
    });
    record_path_anchor_deprecations(&mut result, args, "kin_review_note_add");
    let json = serde_json::to_string_pretty(&result).map_err(McpError::Json)?;
    Ok(PlannedReviewEvent {
        action: "review.note",
        review_id,
        details: format!("review_id={review_id}; note_id={}", note.note_id),
        actor_label: author.name,
        refs: None,
        write: ReviewWrite {
            notes: vec![note],
            ..ReviewWrite::default()
        },
        answer: ToolCallResult::text(json),
    })
}

pub const REVIEW_DISCUSS_DESC: &str = "\
Open a discussion thread on a review with an initial message, optionally anchored to a \
specific entity or file/line. Reach for it when a point needs conversation — a question \
or concern others should reply to and eventually resolve — rather than a one-off note. \
Returns the new discussion's ID; reply with kin_review_discuss_reply and close it out \
with kin_review_discuss_resolve. Anchoring to an entity keeps the thread attached to \
the code in graph truth as it evolves. A scope naming a symbol outside the repository is \
refused with external_symbol_not_served.";

pub fn handle_review_discuss<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<ToolCallResult> {
    apply_planned(plan_review_discuss(args, store)?, store)
}

fn plan_review_discuss<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<PlannedReviewTool> {
    use kin_model::review::{
        ReviewComment, ReviewDiscussion, ReviewDiscussionId, ReviewDiscussionState,
    };
    use kin_model::timestamp::Timestamp;

    let review_id = parse_review_id(args, "review_id")?;
    let body = get_string_param(args, "body")?;
    let scope = parse_optional_scope_arg(args, store, "kin_review_discuss")?;
    let author = parse_identity_arg(args, "author", "author_kind", "mcp-client");
    existing_review(store, &review_id)?;

    let now = Timestamp::now();
    let discussion_id = ReviewDiscussionId::new();
    let discussion = ReviewDiscussion {
        discussion_id,
        review_id,
        scope: scope.clone(),
        state: ReviewDiscussionState::Open,
        comments: vec![ReviewComment {
            body: body.clone(),
            authored_by: author.clone(),
            created_at: now.clone(),
        }],
        created_at: now,
    };

    let mut result = serde_json::json!({
        "discussion_id": discussion_id.to_string(),
        "review_id": review_id.to_string(),
        "scope": scope.map(|s| s.to_string()),
        "state": "open",
        "author": author.name,
    });
    record_path_anchor_deprecations(&mut result, args, "kin_review_discuss");
    let json = serde_json::to_string_pretty(&result).map_err(McpError::Json)?;
    Ok(PlannedReviewEvent {
        action: "review.discuss",
        review_id,
        details: format!("review_id={review_id}; discussion_id={discussion_id}"),
        actor_label: author.name,
        refs: None,
        write: ReviewWrite {
            discussions: vec![discussion],
            ..ReviewWrite::default()
        },
        answer: ToolCallResult::text(json),
    })
}

pub const REVIEW_DISCUSS_REPLY_DESC: &str = "\
Append a reply to an existing review discussion thread, identified by its discussion \
ID. Reach for it to continue a conversation started with kin_review_discuss — the reply \
is added in order with its author recorded, so the thread reads as a coherent exchange. \
When the conversation has reached a conclusion, resolve it with \
kin_review_discuss_resolve.";

pub fn handle_review_discuss_reply<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<ToolCallResult> {
    apply_planned(plan_review_discuss_reply(args, store)?, store)
}

fn plan_review_discuss_reply<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<PlannedReviewTool> {
    use kin_model::review::ReviewComment;
    use kin_model::timestamp::Timestamp;

    let discussion_id = parse_discussion_id(args, "discussion_id")?;
    let body = get_string_param(args, "body")?;
    let author = parse_identity_arg(args, "author", "author_kind", "mcp-client");
    let mut discussion = existing_discussion(store, &discussion_id)?;
    discussion.comments.push(ReviewComment {
        body: body.clone(),
        authored_by: author.clone(),
        created_at: Timestamp::now(),
    });

    let result = serde_json::json!({
        "discussion_id": discussion_id.to_string(),
        "replied": true,
        "author": author.name,
    });
    let json = serde_json::to_string_pretty(&result).map_err(McpError::Json)?;
    Ok(PlannedReviewEvent {
        action: "review.reply",
        review_id: discussion.review_id,
        details: format!("discussion_id={discussion_id}"),
        actor_label: author.name,
        refs: None,
        write: ReviewWrite {
            discussions: vec![discussion],
            ..ReviewWrite::default()
        },
        answer: ToolCallResult::text(json),
    })
}

pub const REVIEW_DISCUSS_RESOLVE_DESC: &str = "\
Resolve a review discussion thread (or reopen one) by its discussion ID. Reach for it \
to mark a conversation as settled once its concern is addressed, or to reopen it if the \
issue resurfaces. Tracking resolution in graph truth lets a review report which threads \
are still outstanding versus done.";

pub fn handle_review_discuss_resolve<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<ToolCallResult> {
    apply_planned(plan_review_discuss_resolve(args, store)?, store)
}

fn plan_review_discuss_resolve<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<PlannedReviewTool> {
    use kin_model::review::ReviewDiscussionState;

    let discussion_id = parse_discussion_id(args, "discussion_id")?;
    let resolved = match get_optional_string_param(args, "state") {
        Some(state) if state.eq_ignore_ascii_case("resolved") => true,
        Some(state) if state.eq_ignore_ascii_case("open") => false,
        Some(state) => {
            return Err(McpError::InvalidParams(format!(
                "invalid discussion state: {}. Valid values: resolved, open",
                state
            )))
        }
        None => get_optional_bool(args, "resolved", true),
    };

    let new_state = if resolved {
        ReviewDiscussionState::Resolved
    } else {
        ReviewDiscussionState::Open
    };
    let mut discussion = existing_discussion(store, &discussion_id)?;
    let review_id = discussion.review_id;
    // A thread already in the asked-for state changes nothing, so nothing is
    // written and no transaction is spent on it.
    let write = if discussion.state == new_state {
        ReviewWrite::default()
    } else {
        discussion.state = new_state;
        ReviewWrite {
            discussions: vec![discussion],
            ..ReviewWrite::default()
        }
    };

    let label = if resolved { "resolved" } else { "open" };
    let result = serde_json::json!({
        "discussion_id": discussion_id.to_string(),
        "state": label,
    });
    let json = serde_json::to_string_pretty(&result).map_err(McpError::Json)?;
    Ok(PlannedReviewEvent {
        action: "review.resolve",
        review_id,
        details: format!("discussion_id={discussion_id}; state={label}"),
        actor_label: "mcp-client".to_string(),
        refs: None,
        write,
        answer: ToolCallResult::text(json),
    })
}

pub const REVIEW_ASSIGN_DESC: &str = "\
Assign one or more reviewers to a review. Pass a single `reviewer` or a batch via \
`reviewers`, and optionally who assigned them. Reach for it to route a review to the \
people (or agents) who should weigh in, so the request shows up as their responsibility \
in graph truth. Remove an assignment with kin_review_unassign.";

pub fn handle_review_assign<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<ToolCallResult> {
    apply_planned(plan_review_assign(args, store)?, store)
}

fn plan_review_assign<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<PlannedReviewTool> {
    use kin_model::review::ReviewAssignment;
    use kin_model::timestamp::Timestamp;

    let review_id = parse_review_id(args, "review_id")?;
    let reviewers = parse_reviewer_list(args)?;
    let assigner = parse_identity_arg(args, "assigned_by", "assigned_by_kind", "mcp-client");
    existing_review(store, &review_id)?;
    let assigned_at = Timestamp::now();

    // The stored set, which is an add log: it only grows, a removal is recorded
    // as its own event, and a review's reviewers are derived from the two.
    let mut entries = store
        .get_review_assignments(&review_id)
        .map_err(|e| McpError::Other(e.to_string()))?;
    let assigned: Vec<ReviewAssignment> = reviewers
        .iter()
        .map(|reviewer| ReviewAssignment {
            review_id,
            reviewer: kin_model::IdentityRef::human(reviewer.clone()),
            assigned_at: assigned_at.clone(),
            assigned_by: assigner.clone(),
        })
        .collect();
    let tags: Vec<_> = assigned
        .iter()
        .map(kin_review::assignments::AssignmentTag::of)
        .collect();
    entries.extend(assigned);

    let details = format!(
        "review_id={review_id}; reviewers={}; {}",
        reviewers.join(","),
        kin_review::assignments::tag_details(&tags)
    );
    let result = serde_json::json!({
        "review_id": review_id.to_string(),
        "reviewers": reviewers,
        "assigned_by": assigner.name,
        "assigned": true,
    });
    let json = serde_json::to_string_pretty(&result).map_err(McpError::Json)?;
    Ok(PlannedReviewEvent {
        action: "review.assign",
        review_id,
        details,
        actor_label: assigner.name,
        refs: None,
        write: ReviewWrite {
            assignments: Some(ReviewGroup { review_id, entries }),
            ..ReviewWrite::default()
        },
        answer: ToolCallResult::text(json),
    })
}

pub const REVIEW_UNASSIGN_DESC: &str = "\
Remove a reviewer's assignment from a review. Reach for it when someone is no longer \
expected to review — reassigned, out, or added by mistake — so the review's outstanding \
reviewer list stays accurate in graph truth. Add assignments with kin_review_assign.";

pub fn handle_review_unassign<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<ToolCallResult> {
    apply_planned(plan_review_unassign(args, store)?, store)
}

fn plan_review_unassign<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<PlannedReviewTool> {
    let review_id = parse_review_id(args, "review_id")?;
    let reviewer = get_string_param(args, "reviewer")?;
    existing_review(store, &review_id)?;

    // The reviewers this review has, so removing someone already removed
    // records nothing a second time.
    let live = kin_review::assignments::current_assignments(store, &review_id)
        .map_err(|e| McpError::Other(e.to_string()))?;
    let removed: Vec<_> = live
        .into_iter()
        .filter(|assignment| assignment.reviewer.name == reviewer)
        .collect();
    let tags: Vec<_> = removed
        .iter()
        .map(kin_review::assignments::AssignmentTag::of)
        .collect();
    let write = if removed.is_empty() {
        // Not assigned, so there is nothing to remove and nothing to record.
        ReviewWrite::default()
    } else {
        // The removal is the audit event this write carries, which the review
        // writer records for a write that carries records. It is not a smaller
        // set: the set is an add log, a delta refuses an empty one, and shrinking
        // it would leave a later re-assignment of the same reviewer unable to
        // reach the live graph. So the set is written unchanged.
        ReviewWrite {
            assignments: Some(ReviewGroup {
                review_id,
                entries: store
                    .get_review_assignments(&review_id)
                    .map_err(|e| McpError::Other(e.to_string()))?,
            }),
            ..ReviewWrite::default()
        }
    };

    let result = serde_json::json!({
        "review_id": review_id.to_string(),
        "reviewer": reviewer,
        "unassigned": true,
    });
    let json = serde_json::to_string_pretty(&result).map_err(McpError::Json)?;
    Ok(PlannedReviewEvent {
        action: kin_review::assignments::UNASSIGN_ACTION,
        review_id,
        details: format!(
            "review_id={review_id}; reviewer={reviewer}; {}",
            kin_review::assignments::tag_details(&tags)
        ),
        actor_label: "mcp-client".to_string(),
        refs: None,
        write,
        answer: ToolCallResult::text(json),
    })
}

/// A review write tool planned as the records it writes and the answer it gives
/// once they are written.
pub type PlannedReviewTool = PlannedReviewEvent<ToolCallResult>;

/// The review tools that write review state.
pub const REVIEW_MUTATION_TOOLS: [&str; 8] = [
    "kin_review_create",
    "kin_review_decide",
    "kin_review_note_add",
    "kin_review_discuss",
    "kin_review_discuss_reply",
    "kin_review_discuss_resolve",
    "kin_review_assign",
    "kin_review_unassign",
];

/// Whether `tool` writes review state.
pub fn is_review_mutation(tool: &str) -> bool {
    REVIEW_MUTATION_TOOLS.contains(&tool)
}

/// Plan one review write tool against `store` without writing anything, or
/// `None` when `tool` writes no review state.
///
/// A daemon commits the planned records to repository authority before its live
/// graph sees them, so its dispatch calls this rather than the
/// `handle_review_*` functions, which apply the same plan straight to a store.
pub fn plan_review_mutation<G: GraphStore>(
    tool: &str,
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Option<Result<PlannedReviewTool>> {
    Some(match tool {
        "kin_review_create" => plan_review_create(args, store),
        "kin_review_decide" => plan_review_decide(args, store),
        "kin_review_note_add" => plan_review_note_add(args, store),
        "kin_review_discuss" => plan_review_discuss(args, store),
        "kin_review_discuss_reply" => plan_review_discuss_reply(args, store),
        "kin_review_discuss_resolve" => plan_review_discuss_resolve(args, store),
        "kin_review_assign" => plan_review_assign(args, store),
        "kin_review_unassign" => plan_review_unassign(args, store),
        _ => return None,
    })
}

/// Apply a planned review write straight to `store`, for a caller with no
/// repository authority behind it.
///
/// Nothing here records the audit event the daemon's review writer records, and
/// a removal is proven by that event. Most removals are also expressed by the
/// set they leave, so they land either way. The one that is not is a review's
/// last reviewer, whose removal leaves a set a collaboration delta cannot carry:
/// on this path it would leave no trace at all, so it is refused by name rather
/// than answered as done.
fn apply_planned<G: GraphStore>(planned: PlannedReviewTool, store: &G) -> Result<ToolCallResult> {
    planned
        .write
        .apply_to(store)
        .map_err(|error| McpError::Other(error.to_string()))?;
    // A removal is an audit event, and this path records none, because nothing
    // here holds repository authority. Its graph is its whole state, so the
    // reviewer comes off that graph directly, which is what this surface did
    // before removals were recorded at all.
    if planned.action == kin_review::assignments::UNASSIGN_ACTION {
        for tag in kin_review::assignments::tags_in_details(&planned.details) {
            store
                .remove_reviewer(&planned.review_id, &tag.reviewer)
                .map_err(|error| McpError::Other(error.to_string()))?;
        }
    }
    Ok(planned.answer)
}

fn existing_review<G: GraphStore>(
    store: &G,
    review_id: &kin_model::review::ReviewId,
) -> Result<kin_model::review::Review> {
    store
        .get_review(review_id)
        .map_err(|e| McpError::Other(e.to_string()))?
        .ok_or_else(|| McpError::InvalidParams(format!("review not found: {review_id}")))
}

/// The discussion with `discussion_id`, found through the reviews that hold
/// discussions, since the store answers discussions by review.
fn existing_discussion<G: GraphStore>(
    store: &G,
    discussion_id: &kin_model::review::ReviewDiscussionId,
) -> Result<kin_model::review::ReviewDiscussion> {
    let reviews = store
        .list_reviews(&kin_model::review::ReviewFilter::default())
        .map_err(|e| McpError::Other(e.to_string()))?;
    for review in reviews {
        let found = store
            .get_review_discussions(&review.review_id)
            .map_err(|e| McpError::Other(e.to_string()))?
            .into_iter()
            .find(|discussion| discussion.discussion_id == *discussion_id);
        if let Some(discussion) = found {
            return Ok(discussion);
        }
    }
    Err(McpError::InvalidParams(format!(
        "review discussion not found: {discussion_id}"
    )))
}

pub const REVIEW_LIST_DESC: &str = "\
List reviews, optionally filtered by state (pending, approved, needs_work, blocked). \
Each row is a compact summary — review ID, title, state, and base/head refs. Reach for \
it to see what reviews exist and triage them: what's awaiting a decision, what's \
blocked, what's done. Use kin_review_get to pull the full detail of any one review.";

pub fn handle_review_list<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<ToolCallResult> {
    let state = get_optional_string_param(args, "state");
    let state_filter = state
        .as_deref()
        .map(parse_review_decision_state)
        .transpose()?;

    // The shared read, so this tool, `kin review list` and the daemon's
    // repo-scoped listing cannot drift apart on what a review row is.
    let reviews = kin_review::records::list_stored_reviews(store, state_filter)
        .map_err(|e| McpError::Other(e.to_string()))?;
    let result: Vec<kin_review::records::ReviewSummaryView> = reviews
        .iter()
        .map(kin_review::records::ReviewSummaryView::from)
        .collect();

    // Printed through a `Value`, as the `json!` rows this replaced were, so
    // the key order is the one serde_json's `preserve_order` feature gives
    // `json!` in whichever build this is, and the text stays what it was.
    let value = serde_json::to_value(&result).map_err(McpError::Json)?;
    let json = serde_json::to_string_pretty(&value).map_err(McpError::Json)?;
    Ok(ToolCallResult::text(json))
}

pub const REVIEW_GET_DESC: &str = "\
Fetch one review in full by ID: its decisions, notes, discussion threads, and reviewer \
assignments together in a single response. Reach for it to see the complete state of a \
review — where it stands, what's been said, what's unresolved — in one call rather than \
piecing it together. Use kin_review_list first when you need to find the review ID.";

pub fn handle_review_get<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<ToolCallResult> {
    let review_id = parse_review_id(args, "review_id")?;

    // The shared read and its JSON view, the same object the daemon's
    // repo-scoped review route answers, so a client maps one shape.
    let record = kin_review::records::read_review_record(store, &review_id)
        .map_err(|e| McpError::Other(e.to_string()))?
        .ok_or_else(|| McpError::InvalidParams(format!("review not found: {}", review_id)))?;
    let result = kin_review::records::ReviewRecordView::from(&record);

    // Through a `Value` for the reason `handle_review_list` gives.
    let value = serde_json::to_value(&result).map_err(McpError::Json)?;
    let json = serde_json::to_string_pretty(&value).map_err(McpError::Json)?;
    Ok(ToolCallResult::text(json))
}

// ── Review ID parsing helpers ──

fn parse_review_id(
    args: &HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<kin_model::review::ReviewId> {
    let id_str = get_string_param(args, key)?;
    let uuid = uuid::Uuid::parse_str(&id_str)
        .map_err(|_| McpError::InvalidParams(format!("invalid {}: {}", key, id_str)))?;
    Ok(kin_model::review::ReviewId(uuid))
}

fn parse_discussion_id(
    args: &HashMap<String, serde_json::Value>,
    key: &str,
) -> Result<kin_model::review::ReviewDiscussionId> {
    let id_str = get_string_param(args, key)?;
    let uuid = uuid::Uuid::parse_str(&id_str)
        .map_err(|_| McpError::InvalidParams(format!("invalid {}: {}", key, id_str)))?;
    Ok(kin_model::review::ReviewDiscussionId(uuid))
}

fn parse_review_decision_state(s: &str) -> Result<kin_model::review::ReviewDecisionState> {
    kin_review::records::parse_review_decision_state(s).ok_or_else(|| {
        McpError::InvalidParams(format!(
            "invalid review state: {}. Valid values: pending, approved, needs_work, blocked",
            s
        ))
    })
}

/// Parse an optional work scope from a JSON value (string like "entity:ID").
fn parse_optional_work_scope(
    val: Option<&serde_json::Value>,
) -> Result<Option<kin_model::WorkScope>> {
    // A scope spelled in none of the documented forms is refused, not dropped:
    // dropping it would fall through to `file_path`, or to no scope at all, and
    // the note would anchor somewhere the caller did not ask for.
    val.and_then(|v| v.as_str())
        .map(parse_single_work_scope)
        .transpose()
}

/// The refusal a review write gives a scope naming a symbol outside the
/// repository, as the error its plan fails with. The error's text is the
/// refusal's own JSON, so the in-process tool and the daemon's review writer,
/// which both answer a failed plan with that text, refuse it in one shape.
fn external_review_scope_refusal<G: GraphStore>(
    store: &G,
    scopes: Option<&serde_json::Value>,
    tool: &str,
    argument: &str,
) -> Result<()> {
    let Some(scopes) = scopes else {
        return Ok(());
    };
    match super::external_symbols::external_scope_refusal(store, scopes, tool, argument)? {
        Some(refusal) => Err(McpError::Other(refusal)),
        None => Ok(()),
    }
}

fn parse_optional_scope_arg<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
    tool: &str,
) -> Result<Option<kin_model::WorkScope>> {
    external_review_scope_refusal(store, args.get("scope"), tool, "scope")?;
    if let Some(scope) = parse_optional_work_scope(args.get("scope"))? {
        return Ok(Some(scope));
    }

    if let Some(file_path) = get_optional_string_param(args, "file_path") {
        // A path and a line resolve to the entity that holds them, so the note
        // anchors on the declaration a reader is pointing at rather than on the
        // file it sits in. The anchor then survives the entity moving, which a
        // path and a line do not.
        let line = args
            .get("line")
            .and_then(serde_json::Value::as_u64)
            .map(|line| line as u32);
        if let Some(entity) = innermost_entity_at(store, &file_path, line)? {
            return Ok(Some(kin_model::WorkScope::Entity(entity)));
        }
        return Ok(Some(kin_model::WorkScope::Artifact(
            kin_model::FilePathId::new(file_path),
        )));
    }

    Ok(None)
}

/// The smallest entity in `file_path` whose span holds `line`, or `None` when no
/// line was given and when none holds it.
///
/// Smallest wins because spans nest: the class containing a method contains the
/// method's lines too, and a reader pointing at one of them means the method.
/// The comparison runs on the presentation lines every other surface reports, so
/// a caller's line means here what it means in the answers it read.
fn innermost_entity_at<G: GraphStore>(
    store: &G,
    file_path: &str,
    line: Option<u32>,
) -> Result<Option<kin_model::EntityId>> {
    let Some(line) = line else {
        return Ok(None);
    };
    let filter = kin_model::graph::EntityFilter {
        file_path: Some(kin_model::FilePathId::new(file_path)),
        ..Default::default()
    };
    let entities = store.query_entities(&filter).map_err(McpError::graph)?;
    Ok(entities
        .into_iter()
        .filter_map(|entity| {
            let start = entity_presentation_start_line(&entity)?;
            let end = entity_presentation_end_line(&entity)?;
            (start <= line && line <= end).then_some((end.saturating_sub(start), entity.id))
        })
        .min_by_key(|(height, _)| *height)
        .map(|(_, id)| id))
}

/// Note on the answer that the call anchored by path, once per parameter it
/// passed. The `scope` the answer already carries says what it resolved to, an
/// entity or the artifact, so a caller can see which happened.
fn record_path_anchor_deprecations(
    payload: &mut serde_json::Value,
    args: &HashMap<String, serde_json::Value>,
    tool: &str,
) {
    for parameter in ["file_path", "line"] {
        if args.contains_key(parameter) {
            crate::budget::record_deprecation(
                payload,
                tool,
                parameter,
                "scope: entity:<uuid>",
                crate::budget::DEPRECATION_REMOVED_AFTER,
            );
        }
    }
}

fn parse_review_create_scopes<G: GraphStore>(
    args: &HashMap<String, serde_json::Value>,
    store: &G,
) -> Result<Vec<kin_model::WorkScope>> {
    external_review_scope_refusal(store, args.get("scopes"), "kin_review_create", "scopes")?;
    let scopes = parse_work_scopes(args.get("scopes"))?;
    if !scopes.is_empty() {
        return Ok(scopes);
    }

    let Some(entity_ids) = args.get("entity_ids").and_then(|value| value.as_array()) else {
        return Ok(Vec::new());
    };

    entity_ids
        .iter()
        .map(|value| {
            let raw = value.as_str().ok_or_else(|| {
                McpError::InvalidParams("entity_ids entries must be strings".into())
            })?;
            // A symbol outside the repository is no review scope. Its address
            // would otherwise be stored as a file path below, and its bare id
            // as an entity no review reaches.
            external_review_scope_refusal(store, Some(value), "kin_review_create", "entity_ids")?;
            if raw.starts_with("entity:")
                || raw.starts_with("artifact:")
                || raw.starts_with("contract:")
            {
                return parse_single_work_scope(raw);
            }

            if let Ok(uuid) = uuid::Uuid::parse_str(raw) {
                return Ok(kin_model::WorkScope::Entity(kin_model::EntityId(uuid)));
            }

            Ok(kin_model::WorkScope::Artifact(kin_model::FilePathId::new(
                raw,
            )))
        })
        .collect()
}

/// Every reviewer the call names, deduplicated, possibly none.
///
/// `requested_reviewers` is the name the create tool's schema documents and
/// `reviewers` the assign tool's; both are read so a caller following either
/// schema is heard.
fn parse_optional_reviewer_list(args: &HashMap<String, serde_json::Value>) -> Result<Vec<String>> {
    let mut reviewers = Vec::new();

    if let Some(reviewer) = get_optional_string_param(args, "reviewer") {
        let trimmed = reviewer.trim();
        if !trimmed.is_empty() {
            reviewers.push(trimmed.to_string());
        }
    }

    for key in ["reviewers", "requested_reviewers"] {
        if let Some(values) = args.get(key).and_then(|value| value.as_array()) {
            for value in values {
                let reviewer = value.as_str().ok_or_else(|| {
                    McpError::InvalidParams(format!("{key} entries must be strings"))
                })?;
                let trimmed = reviewer.trim();
                if !trimmed.is_empty() {
                    reviewers.push(trimmed.to_string());
                }
            }
        }
    }

    reviewers.sort();
    reviewers.dedup();
    Ok(reviewers)
}

fn parse_reviewer_list(args: &HashMap<String, serde_json::Value>) -> Result<Vec<String>> {
    let reviewers = parse_optional_reviewer_list(args)?;
    if reviewers.is_empty() {
        return Err(McpError::InvalidParams(
            "missing reviewer assignment: provide reviewer or reviewers".into(),
        ));
    }

    Ok(reviewers)
}

fn parse_identity_arg(
    args: &HashMap<String, serde_json::Value>,
    name_key: &str,
    kind_key: &str,
    default_name: &str,
) -> kin_model::IdentityRef {
    let name =
        get_optional_string_param(args, name_key).unwrap_or_else(|| default_name.to_string());
    let kind = get_optional_string_param(args, kind_key).unwrap_or_default();
    if kind.eq_ignore_ascii_case("human") {
        kin_model::IdentityRef::human(name)
    } else {
        kin_model::IdentityRef::assistant(name)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::with_empty_test_repository;
    use super::*;

    #[test]
    fn semantic_diff_public_records_preserve_identities_code_and_relation_deltas() {
        use kin_model::change::{EntityDelta, RelationDelta};
        use kin_model::graph::ChangeStore;
        use kin_model::relation::{GraphNodeId, Relation, RelationKind, RelationOrigin};
        let store = kin_db::InMemoryGraph::new();
        let mut entity = entity_spanning("file", "private/implementation.rs", 8, 12);
        let body = "const RESOURCE: &str = \"./literal.rs\";";
        entity
            .metadata
            .extra
            .insert("source".into(), serde_json::json!(body));
        let target = entity_spanning("helper", "private/target.rs", 1, 3);
        let relation = Relation {
            id: kin_model::RelationId::new(),
            kind: RelationKind::Calls,
            src: GraphNodeId::Entity(entity.id),
            dst: GraphNodeId::Entity(target.id),
            confidence: 1.0,
            origin: RelationOrigin::Parsed,
            created_in: None,
            import_source: None,
            evidence: Vec::new(),
        };
        let mut change = history_change(
            None,
            vec![
                EntityDelta::Added {
                    new: entity.clone(),
                },
                EntityDelta::Added { new: target },
            ],
            "semantic diff",
            0,
        );
        change.relation_deltas.push(RelationDelta::Added {
            new: relation.clone(),
        });
        change.id = kin_model::compute_semantic_change_id(&change).unwrap();
        store.create_change(&change).unwrap();
        let args = HashMap::from([(
            "change_ids".into(),
            serde_json::json!([change.id.to_string()]),
        )]);
        let result = handle_semantic_diff(&args, &store).unwrap();
        let crate::types::ContentBlock::Text { text } = &result.content[0];
        let raw: serde_json::Value = serde_json::from_str(text).unwrap();
        assert!(
            text.contains("private/implementation.rs"),
            "private qualification inputs remain available"
        );
        let mut public = serde_json::to_value(result).unwrap();
        crate::semantic_surface::present_result(&mut public, "semantic_diff");
        let public: serde_json::Value =
            serde_json::from_str(public["content"][0]["text"].as_str().unwrap()).unwrap();
        let rows = public["diff"]["entity_changes"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        let row = rows
            .iter()
            .find(|row| row["entity_id"] == entity.id.to_string())
            .unwrap();
        let added = &row["kind"]["Added"];
        assert_eq!(added["id"], entity.id.to_string());
        assert_eq!(added["name"], "file");
        assert_eq!(added["signature"], entity.signature);
        assert_eq!(added["metadata"]["source"], body);
        assert!(added.get("file_origin").is_none());
        assert!(added.get("span").is_none());
        assert!(!public.to_string().contains("private/"));
        assert_eq!(
            public["diff"]["relation_changes"],
            raw["diff"]["relation_changes"]
        );
        assert_eq!(
            public["diff"]["relation_changes"][0]["kind"]["Added"]["id"],
            relation.id.to_string()
        );
    }

    /// Impact rows must locate an affected caller where an editor would.
    ///
    /// `ImpactReport` holds raw entities, so serializing it exposed only the
    /// nested 0-based graph span. An agent reading that to open the caller landed
    /// one line above it, which is the same off-by-one the other read surfaces
    /// carried before the presentation seam.
    #[test]
    fn impact_rows_carry_one_based_presentation_lines_beside_the_raw_span() {
        fn caller(name: &str, graph_row: u32) -> kin_model::Entity {
            let file = kin_model::ids::FilePathId::new("src/consumer.ts");
            let mut entity = kin_model::Entity {
                id: kin_model::EntityId::new(),
                kind: kin_model::EntityKind::Function,
                name: name.to_string(),
                language: kin_model::LanguageId::TypeScript,
                fingerprint: kin_model::entity::SemanticFingerprint {
                    algorithm: kin_model::entity::FingerprintAlgorithm::V1TreeSitter,
                    ast_hash: kin_model::Hash256::from_bytes([4; 32]),
                    signature_hash: kin_model::Hash256::from_bytes([5; 32]),
                    behavior_hash: kin_model::Hash256::from_bytes([6; 32]),
                    equivalence_hash: kin_model::Hash256::from_bytes([0; 32]),
                    stability_score: 1.0,
                },
                file_origin: Some(file.clone()),
                span: None,
                signature: format!("function {name}(): void"),
                visibility: kin_model::entity::Visibility::Public,
                role: kin_model::entity::EntityRole::Source,
                doc_summary: None,
                metadata: kin_model::entity::EntityMetadata::default(),
                lineage_parent: None,
                created_in: None,
                superseded_by: None,
            };
            entity.span = Some(kin_model::entity::SourceSpan {
                file,
                start_byte: 0,
                end_byte: 20,
                start_line: graph_row,
                start_col: 0,
                end_line: graph_row + 2,
                end_col: 1,
            });
            entity
        }

        let direct = caller("probe_direct_9ab1", 41);
        let spanless = {
            let mut entity = caller("probe_spanless_9ab1", 0);
            entity.span = None;
            entity
        };
        let report = kin_review::ImpactReport {
            enrichment: None,
            affected_callers: vec![direct.clone(), spanless.clone()],
            affected_dependents: vec![],
            affected_contract_consumers: vec![],
            affected_tests: vec![],
            affected_overrides: vec![],
            affected_override_callers: vec![],
            override_caller_candidates: vec![],
            affected_work_items: vec![],
            affected_annotations: vec![],
            changed_ids: vec![],
            unreviewed_agent_changes: vec![],
            actor_attribution: vec![],
            entity_impacts: vec![kin_review::EntityImpact {
                entity_id: direct.id,
                consumer_count: 0,
                external_consumer_count: 0,
                test_consumer_count: 0,
                derived_consumer_count: 0,
                strong_consumer_count: 0,
                proven_consumer_count: 0,
                contract_consumer_count: 0,
                consumer_files: vec![],
                external_consumer_files: vec![],
                covering_tests: 0,
                consumers_migrated_in_diff: 0,
                call_shapes: kin_review::impact::ConsumerCallShapeSummary::default(),
            }],
        };

        let mut value = serde_json::to_value(&report).unwrap();
        annotate_impact_presentation_lines(&mut value, &report);
        let rows = value["affected_callers"].as_array().unwrap();

        assert_eq!(
            rows[0]["start_line"], 42,
            "graph row 41 is line 42: {}",
            rows[0]
        );
        assert_eq!(rows[0]["end_line"], 44);
        // The raw span is untouched: its byte offsets are read as offsets, so it
        // stays a faithful serialization of graph truth.
        assert_eq!(rows[0]["span"]["start_line"], 41);
        assert_eq!(rows[0]["name"], "probe_direct_9ab1");

        // A spanless entity gets null rather than a fabricated line 1.
        assert!(
            rows[1]["start_line"].is_null(),
            "an entity with no span has no line to report: {}",
            rows[1]
        );

        let coverage = &value["entity_impacts"][0];
        assert_eq!(coverage["covering_tests"], 0);
        assert_eq!(
            coverage[COVERING_TESTS_BOUND_KEY], COVERING_TESTS_BOUND,
            "a zero must say beside the number that missing local-variable property edges can \
             make it a false negative: {coverage}"
        );
    }

    /// An export whose caller's call never became an edge, driven through the
    /// real handler and the whole envelope path an MCP response takes.
    ///
    /// `note_body` is defined in `storage.py`, and `test_storage.py` imports that
    /// module and calls it. The parser read two call sites in the test file and
    /// the linker recorded an edge for one of them, the call to `find_note`, so
    /// `note_body` holds no inbound edge and its row reports `consumer_count: 0`.
    /// Every other gate reads a healthy graph: the store links calls, imports and
    /// references across files and the daemon is ready. The zero used to come
    /// back certified here, `safe_to_conclude_absent: true`, while
    /// `find_references` refused the same absence on the same graph.
    ///
    /// The control accounts for the test file's one parsed call site. That
    /// clears the arrival gap, while this graph-only fixture still cannot
    /// certify non-call consumers without a mention census.
    #[tokio::test]
    async fn a_zero_consumer_count_a_caller_may_not_have_reached_is_not_certified() {
        use kin_model::graph::EntityStore as _;
        use kin_model::relation::{Relation, RelationEvidence, RelationKind, RelationOrigin};
        use kin_model::{EntityId, FilePathId, GraphNodeId, RelationId, SourceSpan};

        const FOCAL_FILE: &str = "src/notekeeper/storage.py";
        const CALLER_FILE: &str = "tests/test_storage.py";

        fn python_entity(
            name: &str,
            file: &str,
            kind: kin_model::EntityKind,
            parsed_call_sites: u64,
        ) -> kin_model::Entity {
            let mut metadata = kin_model::entity::EntityMetadata::default();
            metadata.extra.insert(
                kin_parser::FILE_PARSED_CALL_SITES_KEY.into(),
                serde_json::json!(parsed_call_sites),
            );
            kin_model::Entity {
                id: EntityId::from_content(file, name, &format!("{kind:?}"), 0),
                kind,
                name: name.to_string(),
                language: kin_model::LanguageId::Python,
                fingerprint: kin_model::entity::SemanticFingerprint {
                    algorithm: kin_model::entity::FingerprintAlgorithm::V1TreeSitter,
                    ast_hash: kin_model::Hash256::from_bytes([7; 32]),
                    signature_hash: kin_model::Hash256::from_bytes([8; 32]),
                    behavior_hash: kin_model::Hash256::from_bytes([9; 32]),
                    equivalence_hash: kin_model::Hash256::from_bytes([0; 32]),
                    stability_score: 1.0,
                },
                file_origin: Some(FilePathId::new(file)),
                span: None,
                signature: format!("def {name}()"),
                visibility: kin_model::entity::Visibility::Public,
                role: kin_model::entity::EntityRole::Source,
                doc_summary: None,
                metadata,
                lineage_parent: None,
                created_in: None,
                superseded_by: None,
            }
        }

        fn edge(kind: RelationKind, src: &kin_model::Entity, dst: &kin_model::Entity) -> Relation {
            let mut relation = Relation {
                id: RelationId::from_content(
                    &src.id.to_string(),
                    &dst.id.to_string(),
                    &format!("{kind:?}"),
                ),
                kind,
                src: GraphNodeId::Entity(src.id),
                dst: GraphNodeId::Entity(dst.id),
                confidence: 1.0,
                origin: RelationOrigin::Parsed,
                created_in: None,
                import_source: None,
                evidence: Vec::new(),
            };
            if kind == RelationKind::Calls {
                // The call site the parser read, so the arrival reading can
                // join this edge to one parsed site.
                relation.evidence.push(RelationEvidence {
                    source_span: Some(SourceSpan {
                        file: FilePathId::new(CALLER_FILE),
                        start_byte: 120,
                        end_byte: 140,
                        start_line: 6,
                        start_col: 4,
                        end_line: 6,
                        end_col: 24,
                    }),
                    occurrence_count: 1,
                    ..RelationEvidence::default()
                });
            }
            relation
        }

        async fn impact_verdict(caller_parsed_call_sites: u64) -> (serde_json::Value, EntityId) {
            use kin_model::EntityKind::{Function, Module};
            let store = kin_db::InMemoryGraph::new();
            let storage = python_entity("storage", FOCAL_FILE, Module, 0);
            let note_body = python_entity("note_body", FOCAL_FILE, Function, 0);
            let find_note = python_entity("find_note", FOCAL_FILE, Function, 0);
            let test_module = python_entity(
                "test_storage",
                CALLER_FILE,
                Module,
                caller_parsed_call_sites,
            );
            let test_fn = python_entity(
                "test_bodies_round_trip",
                CALLER_FILE,
                Function,
                caller_parsed_call_sites,
            );
            for entity in [&storage, &note_body, &find_note, &test_module, &test_fn] {
                store.upsert_entity(entity).unwrap();
            }
            for relation in [
                edge(RelationKind::Imports, &test_module, &storage),
                edge(RelationKind::Calls, &test_fn, &find_note),
                edge(RelationKind::References, &test_fn, &find_note),
            ] {
                store.upsert_relation(&relation).unwrap();
            }

            if caller_parsed_call_sites == 1 {
                // The positive control records the resolver's actual evidence,
                // so the repository-wide ledger observation agrees with the
                // parser-to-edge census used by the arrival gate.
                use crate::call_sites::fixture::{admit, ledger, proof_context};
                let context = proof_context(kin_model::LanguageId::Python, "test");
                let records = vec![
                    ledger(&test_module, "", context.id(), vec![]),
                    ledger(
                        &test_fn,
                        "find_note()",
                        context.id(),
                        vec![(
                            "find_note",
                            kin_model::CallSiteState::ProvenTarget {
                                target: find_note.id,
                            },
                        )],
                    ),
                    context,
                ];
                admit(&store, &[], records);
            }

            // A host that resolves Python, stated rather than inherited from
            // whoever runs the suite, so the only gate left to decide is the one
            // this test is about.
            let _host = crate::edge_coverage::test_support::scoped_language_servers(&[
                kin_model::LanguageId::Python,
            ]);
            let args = HashMap::from([
                (
                    "entity_ids".to_string(),
                    serde_json::json!([note_body.id.to_string()]),
                ),
                ("include_traffic".to_string(), serde_json::json!(false)),
            ]);
            let sessions = SessionRegistry::empty_for_test();
            let result = handle_impact_analysis(&args, &store, &sessions)
                .await
                .expect("impact answers");
            let envelope = crate::envelope::Envelope::daemon().with_health(&serde_json::json!({
                "initialized": true,
                "graph_loaded": true,
                "reconciliation_status": "clean",
            }));
            let annotated = crate::envelope::finalize_bounded(
                result,
                envelope,
                "impact_analysis",
                &crate::budget::ResponseBudget::default(),
            );
            let crate::types::ContentBlock::Text { text } =
                annotated.content.first().expect("one content block");
            (serde_json::from_str(text).expect("JSON"), note_body.id)
        }

        let (refused, note_body) = impact_verdict(2).await;
        let row = &refused["entity_impacts"][0];
        assert_eq!(row["entity_id"], serde_json::json!(note_body.to_string()));
        assert_eq!(
            row["consumer_count"], 0,
            "the fixture reproduces the zero: {refused}"
        );
        let arrival = &refused[crate::caller_arrival::CALLER_ARRIVAL_KEY];
        assert_eq!(arrival["state"], "unaccounted", "{arrival}");
        assert_eq!(
            arrival["entities"][0]["entity_id"],
            serde_json::json!(note_body.to_string()),
            "the reading is taken for the entity the zero is about: {arrival}"
        );
        let negative = &refused["negative"];
        assert_eq!(
            negative["safe_to_conclude_absent"],
            serde_json::json!(false),
            "a caller may sit in a call site that became no edge, so the zero is a floor: \
             {negative}"
        );
        let reason = negative["trust_reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains(crate::caller_arrival::UNRESOLVED_ARRIVAL_LIMITING_FACTOR)
                && reason.contains(CALLER_FILE)
                && reason.contains("note_body"),
            "the refusal names the gap, the entity and the file its caller may be in: {reason}"
        );
        let verdict = &refused["_kin"]["verdict"];
        assert_eq!(verdict["state"], "inconclusive", "{verdict}");
        assert!(
            verdict["limiting_factor"]
                .as_str()
                .is_some_and(|factor| factor
                    .contains(crate::caller_arrival::UNRESOLVED_ARRIVAL_LIMITING_FACTOR)),
            "the one verdict names the limiting factor: {verdict}"
        );

        let (accounted, _) = impact_verdict(1).await;
        assert_eq!(
            accounted[crate::caller_arrival::CALLER_ARRIVAL_KEY]["state"],
            "accounted",
            "{accounted}"
        );
        assert_eq!(
            accounted["negative"]["safe_to_conclude_absent"],
            serde_json::json!(false),
            "accounting for calls alone cannot certify a zero covering non-call consumers: {}",
            accounted["negative"]
        );
        let reason = accounted["negative"]["trust_reason"]
            .as_str()
            .expect("the remaining gap is named");
        assert!(
            reason.contains("reference_completion_unmeasured"),
            "{reason}"
        );
        assert!(
            !reason.contains(crate::caller_arrival::UNRESOLVED_ARRIVAL_LIMITING_FACTOR),
            "accounted callers clear their own gap: {reason}"
        );
        assert_eq!(accounted["_kin"]["verdict"]["state"], "inconclusive");
    }

    /// A review scope spelled in none of the documented forms is refused. Dropping
    /// it instead would fall through to `file_path`, or to no scope, and the note
    /// would anchor somewhere the caller did not ask for; a `scopes` entry would
    /// fall through to `entity_ids`.
    #[test]
    fn a_misspelled_review_scope_is_refused_not_dropped() {
        for args in [
            serde_json::json!({ "scope": "src/a.rs" }),
            serde_json::json!({ "scope": "src/a.rs", "file_path": "src/b.rs" }),
        ] {
            let args: HashMap<String, serde_json::Value> =
                serde_json::from_value(args).expect("an argument object");
            assert!(
                parse_optional_scope_arg(
                    &args,
                    &kin_db::InMemoryGraph::new(),
                    "kin_review_note_add"
                )
                .is_err(),
                "a misspelled scope must refuse: {args:?}"
            );
        }

        let mut args = HashMap::new();
        args.insert("scopes".into(), serde_json::json!(["src/a.rs"]));
        assert!(
            parse_review_create_scopes(&args, &kin_db::InMemoryGraph::new()).is_err(),
            "a misspelled scopes entry must refuse, not fall through to entity_ids"
        );
    }

    #[test]
    fn semantic_review_formats_preserve_the_same_public_evidence() {
        use kin_model::graph::EntityStore;
        let store = kin_db::InMemoryGraph::new();
        let entity = entity_spanning("file", "private/review.rs", 5, 10);
        store.upsert_entity(&entity).unwrap();
        let sessions = SessionRegistry::empty_for_test();
        let mut args = HashMap::from([
            ("entity_ids".into(), serde_json::json!([entity.id])),
            ("include_traffic".into(), serde_json::json!(false)),
        ]);
        let mut expected = None;
        for mode in [None, Some("text"), Some("json")] {
            if let Some(mode) = mode {
                args.insert("format".into(), serde_json::json!(mode));
            }
            let result = handle_semantic_review(&args, &store, &sessions).unwrap();
            assert_ne!(result.is_error, Some(true));
            let mut public = serde_json::to_value(result).unwrap();
            crate::semantic_surface::present_result(&mut public, "semantic_review");
            let mut public: serde_json::Value =
                serde_json::from_str(public["content"][0]["text"].as_str().unwrap()).unwrap();
            let row = &public["diff"]["entity_changes"][0]["kind"]["Added"];
            assert_eq!(row["id"], entity.id.to_string());
            assert_eq!(row["name"], "file");
            assert_eq!(row["signature"], entity.signature);
            for field in ["diff", "impact", "risk", "inline_comments"] {
                assert!(public.get(field).is_some(), "{mode:?}: {public}");
            }
            assert!(!public.to_string().contains("private/review.rs"));
            assert!(public.get("formatted").is_none());
            let message = public.as_object_mut().unwrap().remove("message");
            if mode == Some("json") {
                assert!(message.is_none());
            } else {
                assert!(message
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .starts_with("Semantic review: 1 entity changes and 0 relationship changes."));
            }
            if let Some(expected) = &expected {
                assert_eq!(&public, expected, "format must not alter semantic evidence");
            } else {
                expected = Some(public);
            }
        }
        for invalid in [
            serde_json::json!("yaml"),
            serde_json::json!(null),
            serde_json::json!(7),
        ] {
            args.insert("format".into(), invalid);
            assert!(matches!(
                handle_semantic_review(&args, &store, &sessions),
                Err(McpError::InvalidParams(_))
            ));
        }
        let registry = crate::tools::tool_definitions();
        let definition = registry
            .tools
            .iter()
            .find(|tool| tool.name == "semantic_review")
            .unwrap();
        assert_eq!(
            definition.input_schema["properties"]["format"]["enum"],
            serde_json::json!(["text", "json"])
        );
        assert!(definition
            .description
            .contains("Both formats retain the complete structured report"));
    }

    /// One entity per span, built here rather than borrowed, so the nesting this
    /// test is about is explicit.
    fn entity_spanning(
        name: &str,
        file: &str,
        start_line: u32,
        end_line: u32,
    ) -> kin_model::Entity {
        kin_model::Entity {
            id: kin_model::EntityId::new(),
            kind: kin_model::EntityKind::Function,
            name: name.to_string(),
            language: kin_model::LanguageId::Rust,
            fingerprint: kin_model::entity::SemanticFingerprint {
                algorithm: kin_model::entity::FingerprintAlgorithm::V1TreeSitter,
                ast_hash: kin_model::Hash256::from_bytes([0; 32]),
                signature_hash: kin_model::Hash256::from_bytes([0; 32]),
                behavior_hash: kin_model::Hash256::from_bytes([0; 32]),
                equivalence_hash: kin_model::Hash256::from_bytes([0; 32]),
                stability_score: 1.0,
            },
            file_origin: Some(kin_model::FilePathId::new(file)),
            span: Some(kin_model::entity::SourceSpan {
                file: kin_model::FilePathId::new(file),
                start_byte: 0,
                end_byte: 1,
                start_line,
                start_col: 0,
                end_line,
                end_col: 1,
            }),
            signature: format!("fn {name}()"),
            visibility: kin_model::Visibility::Public,
            role: kin_model::EntityRole::Source,
            doc_summary: None,
            metadata: kin_model::entity::EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    /// An impact answer on a method names its overrides in their own bucket,
    /// and every blast-radius row is compact: the fields a reader acts on,
    /// not the entity's fingerprint, span offsets and parser metadata.
    #[tokio::test]
    async fn impact_names_overrides_and_serves_compact_rows() {
        use kin_model::{EntityStore, GraphNodeId, RelationKind, RelationOrigin};
        let store = kin_db::InMemoryGraph::new();
        let mut base = entity_spanning("SessionInterface.open_session", "src/sessions.py", 10, 12);
        let mut secure = entity_spanning(
            "SecureCookieSessionInterface.open_session",
            "src/sessions.py",
            40,
            44,
        );
        let mut deeper = entity_spanning("Deeper.open_session", "tests/test_deeper.py", 3, 4);
        for method in [&mut base, &mut secure, &mut deeper] {
            method.kind = kin_model::EntityKind::Method;
        }
        let caller = entity_spanning("AppContext._get_session", "src/ctx.py", 5, 9);
        for entity in [&base, &secure, &deeper, &caller] {
            store.upsert_entity(entity).unwrap();
        }
        let edge = |kind, src: &kin_model::Entity, dst: &kin_model::Entity| kin_model::Relation {
            id: kin_model::RelationId::new(),
            kind,
            src: GraphNodeId::Entity(src.id),
            dst: GraphNodeId::Entity(dst.id),
            confidence: 1.0,
            origin: RelationOrigin::Parsed,
            created_in: None,
            import_source: None,
            evidence: vec![],
        };
        for relation in [
            edge(RelationKind::Overrides, &secure, &base),
            edge(RelationKind::Overrides, &deeper, &secure),
            edge(RelationKind::Calls, &caller, &base),
        ] {
            store.upsert_relation(&relation).unwrap();
        }
        let sessions = SessionRegistry::empty_for_test();
        let mut args = HashMap::from([
            (
                "entity_ids".into(),
                serde_json::json!([base.id.to_string()]),
            ),
            ("include_traffic".into(), serde_json::json!(false)),
        ]);
        let result = handle_impact_analysis(&args, &store, &sessions)
            .await
            .unwrap();
        let crate::types::ContentBlock::Text { text } = &result.content[0];
        let impact: serde_json::Value = serde_json::from_str(text).unwrap();

        let overrides = impact["affected_overrides"].as_array().unwrap();
        let names: Vec<_> = overrides.iter().map(|row| row["name"].clone()).collect();
        assert_eq!(
            names,
            [
                serde_json::json!("SecureCookieSessionInterface.open_session"),
                serde_json::json!("Deeper.open_session"),
            ],
            "{impact}"
        );
        assert_eq!(overrides[0]["resolution"], "type_resolved");
        assert!(overrides[0].get("overrides").is_none());
        assert_eq!(
            overrides[1]["overrides"],
            "SecureCookieSessionInterface.open_session"
        );
        assert_eq!(overrides[0]["projection"]["path"], "src/sessions.py");
        assert_eq!(overrides[0]["start_line"], 41);

        let callers = impact["affected_callers"].as_array().unwrap();
        assert_eq!(callers[0]["name"], "AppContext._get_session");
        for row in callers.iter().chain(overrides) {
            for bulk in [
                "fingerprint",
                "metadata",
                "span",
                "language",
                "lineage_parent",
            ] {
                assert!(row.get(bulk).is_none(), "{bulk} on a compact row: {row}");
            }
        }
        // Neither override is also filed as two-hop reach.
        assert!(!impact["affected_dependents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"]
                .as_str()
                .is_some_and(|name| name.ends_with("open_session"))));

        // `compact: false` serves the raw entity rows.
        args.insert("compact".into(), serde_json::json!(false));
        let result = handle_impact_analysis(&args, &store, &sessions)
            .await
            .unwrap();
        let crate::types::ContentBlock::Text { text } = &result.content[0];
        let raw: serde_json::Value = serde_json::from_str(text).unwrap();
        assert!(raw["affected_overrides"][0].get("fingerprint").is_some());
    }

    /// Asked to leave test code out, an impact answer holds no test entity
    /// in any list, counts the affected entities it left out for that, and
    /// counts each list as it now stands. Asked for the dependents, it says
    /// they were asked for. Neither is said by an answer nobody asked.
    #[tokio::test]
    async fn impact_leaves_test_code_out_when_asked_and_counts_it() {
        use kin_model::{EntityStore, GraphNodeId, RelationKind, RelationOrigin};
        let store = kin_db::InMemoryGraph::new();
        let mut base = entity_spanning("SessionInterface.open_session", "src/sessions.py", 10, 12);
        let mut secure = entity_spanning(
            "SecureCookieSessionInterface.open_session",
            "src/sessions.py",
            40,
            44,
        );
        let mut fake = entity_spanning("FakeInterface.open_session", "tests/test_ctx.py", 3, 4);
        for method in [&mut base, &mut secure, &mut fake] {
            method.kind = kin_model::EntityKind::Method;
        }
        let caller = entity_spanning("AppContext._get_session", "src/ctx.py", 5, 9);
        let mut test_caller = entity_spanning("test_get_session", "tests/test_ctx.py", 20, 24);
        // Two places hand the method on as a value without calling it.
        let holder = entity_spanning("Flask.register_interface", "src/app.py", 30, 33);
        let mut test_holder = entity_spanning("test_register", "tests/test_ctx.py", 40, 44);
        fake.role = kin_model::EntityRole::Test;
        test_caller.role = kin_model::EntityRole::Test;
        test_holder.role = kin_model::EntityRole::Test;
        for entity in [
            &base,
            &secure,
            &fake,
            &caller,
            &test_caller,
            &holder,
            &test_holder,
        ] {
            store.upsert_entity(entity).unwrap();
        }
        let edge = |kind, src: &kin_model::Entity, dst: &kin_model::Entity| kin_model::Relation {
            id: kin_model::RelationId::new(),
            kind,
            src: GraphNodeId::Entity(src.id),
            dst: GraphNodeId::Entity(dst.id),
            confidence: 1.0,
            origin: RelationOrigin::Parsed,
            created_in: None,
            import_source: None,
            evidence: vec![],
        };
        for relation in [
            edge(RelationKind::Overrides, &secure, &base),
            edge(RelationKind::Overrides, &fake, &base),
            edge(RelationKind::Calls, &caller, &base),
            edge(RelationKind::Calls, &test_caller, &base),
            edge(RelationKind::References, &holder, &base),
            edge(RelationKind::References, &test_holder, &secure),
        ] {
            store.upsert_relation(&relation).unwrap();
        }
        let sessions = SessionRegistry::empty_for_test();
        let mut args = HashMap::from([
            (
                "entity_ids".into(),
                serde_json::json!([base.id.to_string()]),
            ),
            ("include_traffic".into(), serde_json::json!(false)),
        ]);
        let asked = |args: HashMap<String, serde_json::Value>| {
            let (store, sessions) = (&store, &sessions);
            async move {
                let result = handle_impact_analysis(&args, store, sessions)
                    .await
                    .unwrap();
                let crate::types::ContentBlock::Text { text } = &result.content[0];
                serde_json::from_str::<serde_json::Value>(text).unwrap()
            }
        };
        let names = |impact: &serde_json::Value, list: &str| -> Vec<String> {
            let mut names: Vec<String> = impact[list]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|row| row["name"].as_str().map(str::to_string))
                .collect();
            names.sort();
            names
        };

        let whole = asked(args.clone()).await;
        assert_eq!(
            names(&whole, "affected_callers"),
            ["AppContext._get_session", "test_get_session"],
            "{whole}"
        );
        assert_eq!(names(&whole, "affected_overrides").len(), 2, "{whole}");
        // A holder of the method, and one of an override, which says which.
        assert_eq!(
            names(&whole, IMPACT_VALUE_USES_KEY),
            ["Flask.register_interface", "test_register"],
            "{whole}"
        );
        let uses = whole[IMPACT_VALUE_USES_KEY].as_array().unwrap();
        assert!(uses[0].get("uses").is_none(), "{whole}");
        assert_eq!(
            uses[1]["uses"], "SecureCookieSessionInterface.open_session",
            "{whole}"
        );
        assert_eq!(whole[IMPACT_ROW_TOTALS_KEY][IMPACT_VALUE_USES_KEY], 2);
        assert_eq!(
            whole[crate::impact_sections::SECTIONS_KEY][crate::impact_sections::DIRECT]["rows"],
            serde_json::json!([
                "affected_callers",
                IMPACT_VALUE_USES_KEY,
                "affected_overrides"
            ]),
            "{whole}"
        );
        assert_eq!(whole[IMPACT_ROW_TOTALS_KEY]["affected_callers"], 2);
        assert!(whole.get(IMPACT_TESTS_EXCLUDED_KEY).is_none(), "{whole}");
        assert!(whole.get(IMPACT_DEPENDENTS_ASKED_KEY).is_none(), "{whole}");

        args.insert("include_tests".into(), serde_json::json!(false));
        args.insert(IMPACT_DEPENDENTS_ARGUMENT.into(), serde_json::json!(true));
        let lean = asked(args.clone()).await;
        assert_eq!(
            names(&lean, "affected_callers"),
            ["AppContext._get_session"],
            "{lean}"
        );
        assert_eq!(
            names(&lean, "affected_overrides"),
            ["SecureCookieSessionInterface.open_session"],
            "{lean}"
        );
        assert_eq!(
            names(&lean, IMPACT_VALUE_USES_KEY),
            ["Flask.register_interface"],
            "{lean}"
        );
        assert_eq!(lean[IMPACT_TESTS_EXCLUDED_KEY]["entities"], 3, "{lean}");
        assert_eq!(lean[IMPACT_ROW_TOTALS_KEY]["affected_callers"], 1, "{lean}");
        assert_eq!(
            lean[IMPACT_ROW_TOTALS_KEY]["affected_overrides"], 1,
            "{lean}"
        );
        assert_eq!(lean[IMPACT_DEPENDENTS_ASKED_KEY], true, "{lean}");
        for list in IMPACT_ENTITY_BUCKETS
            .into_iter()
            .chain([IMPACT_VALUE_USES_KEY])
        {
            for row in lean[list].as_array().into_iter().flatten() {
                assert_ne!(row["role"], "test", "{list}: {row}");
            }
        }
    }

    /// Confined to a module path, an impact answer lists the affected
    /// entities inside it and counts the ones outside, and refuses a scope
    /// that names no module path rather than answering that nothing in it
    /// is affected.
    #[tokio::test]
    async fn impact_confined_to_a_module_path_lists_what_sits_inside_and_counts_the_rest() {
        use kin_model::{EntityStore, GraphNodeId, RelationKind, RelationOrigin};
        let store = kin_db::InMemoryGraph::new();
        let module = |name: &str, file: &str| {
            let mut module = entity_spanning(name, file, 0, 400);
            module.kind = kin_model::EntityKind::Module;
            module.language = kin_model::LanguageId::Python;
            module.span.as_mut().unwrap().end_byte = 10_000;
            module
        };
        let declared = |name: &str, file: &str, line: u32| {
            let mut entity = entity_spanning(name, file, line, line + 2);
            entity.language = kin_model::LanguageId::Python;
            entity
        };
        let base = declared("open_session", "flask/sessions.py", 10);
        let inside = declared("get_session", "flask/ctx.py", 5);
        let outside = declared("make_session", "examples/tutorial/app.py", 5);
        for entity in [
            &module("flask", "flask/__init__.py"),
            &module("sessions", "flask/sessions.py"),
            &module("ctx", "flask/ctx.py"),
            &module("app", "examples/tutorial/app.py"),
            &base,
            &inside,
            &outside,
        ] {
            store.upsert_entity(entity).unwrap();
        }
        for caller in [&inside, &outside] {
            store
                .upsert_relation(&kin_model::Relation {
                    id: kin_model::RelationId::new(),
                    kind: RelationKind::Calls,
                    src: GraphNodeId::Entity(caller.id),
                    dst: GraphNodeId::Entity(base.id),
                    confidence: 1.0,
                    origin: RelationOrigin::Parsed,
                    created_in: None,
                    import_source: None,
                    evidence: vec![],
                })
                .unwrap();
        }
        let sessions = SessionRegistry::empty_for_test();
        let asked = |scope: Option<&'static str>| {
            let (store, sessions) = (&store, &sessions);
            let id = base.id.to_string();
            async move {
                let mut args = HashMap::from([
                    ("entity_ids".to_string(), serde_json::json!([id])),
                    ("include_traffic".to_string(), serde_json::json!(false)),
                ]);
                if let Some(scope) = scope {
                    args.insert("scope".to_string(), serde_json::json!(scope));
                }
                handle_impact_analysis(&args, store, sessions).await
            }
        };
        let read = |result: ToolCallResult| -> serde_json::Value {
            let crate::types::ContentBlock::Text { text } = &result.content[0];
            serde_json::from_str(text).unwrap()
        };
        let callers = |impact: &serde_json::Value| -> Vec<String> {
            let mut names: Vec<String> = impact["affected_callers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["name"].as_str().unwrap().to_string())
                .collect();
            names.sort();
            names
        };

        let whole = read(asked(None).await.unwrap());
        assert_eq!(callers(&whole), ["get_session", "make_session"], "{whole}");
        assert!(whole.get(IMPACT_ROW_SCOPE_KEY).is_none(), "{whole}");

        let scoped = read(asked(Some("flask")).await.unwrap());
        assert_eq!(callers(&scoped), ["get_session"], "{scoped}");
        assert_eq!(
            scoped[IMPACT_ROW_SCOPE_KEY],
            serde_json::json!({"module": "flask", "outside": 1}),
            "{scoped}"
        );
        assert_eq!(scoped[IMPACT_ROW_TOTALS_KEY]["affected_callers"], 1);
        for list in IMPACT_ENTITY_BUCKETS {
            for row in scoped[list].as_array().into_iter().flatten() {
                assert_ne!(row["name"], "make_session", "{list}: {scoped}");
            }
        }

        let refused = asked(Some("django")).await.unwrap_err().to_string();
        assert!(
            refused.contains("scope 'django' names no module path"),
            "{refused}"
        );
    }

    /// A text reader holding the bodies a fixture wrote, the way the
    /// daemon's reader holds the repository's.
    #[derive(Default)]
    struct Bodies(HashMap<kin_model::EntityId, String>);

    impl crate::handlers::external_symbols::SiteText for Bodies {
        fn quote(
            &self,
            caller: &kin_model::Entity,
            site: &kin_model::SourceSpan,
        ) -> std::result::Result<String, &'static str> {
            let body = self.0.get(&caller.id).ok_or("caller_text_not_held")?;
            let start = caller.span.as_ref().map_or(0, |span| span.start_byte);
            crate::handlers::external_symbols::quote_site(caller, site, body, start)
        }
    }

    /// A caller row shows the call it makes, read from the caller's own
    /// text at the line its edge records. The places that hold the changed
    /// method or an override as a value, which no call edge names, are rows
    /// of their own with the line that hands it on: each entity once, a
    /// caller left to its caller row.
    #[test]
    fn impact_quotes_each_call_and_lists_the_places_that_hold_the_method_as_a_value() {
        use kin_model::{EntityStore, GraphNodeId, RelationKind, RelationOrigin};
        let store = kin_db::InMemoryGraph::new();
        let mut bodies = Bodies::default();
        // An entity whose text is `body`, starting on 0-based `line`.
        let mut written = |name: &str, file: &str, line: u32, body: &str| {
            let mut entity = entity_spanning(name, file, line, line + 1);
            entity.kind = kin_model::EntityKind::Method;
            entity.span.as_mut().unwrap().end_byte = body.len();
            bodies.0.insert(entity.id, body.to_string());
            store.upsert_entity(&entity).unwrap();
            entity
        };
        let base = written(
            "JSONProvider.dumps",
            "src/json/provider.py",
            10,
            "def dumps(self, obj):\n    raise NotImplementedError\n",
        );
        let default = written(
            "DefaultJSONProvider.dumps",
            "src/json/provider.py",
            40,
            "def dumps(self, obj):\n    return json.dumps(obj)\n",
        );
        let caller = written(
            "JSONProvider.response",
            "src/json/provider.py",
            20,
            "def response(self, obj):\n    filter = self.dumps\n    return self.dumps(obj)\n",
        );
        let jinja = written(
            "Flask.create_jinja_environment",
            "src/app.py",
            100,
            "def create_jinja_environment(self):\n    rv = Environment()\n    \
             rv.policies[\"json.dumps_function\"] = self.json.dumps\n    return rv\n",
        );
        let tag = written(
            "TaggedJSONSerializer.register",
            "src/json/tag.py",
            7,
            "def register(self):\n    self.default = DefaultJSONProvider.dumps\n",
        );
        // An edge whose site is `offset` lines into the entity that holds it.
        let edge = |kind, src: &kin_model::Entity, dst: &kin_model::Entity, offset: u32| {
            let span = src.span.as_ref().unwrap();
            store
                .upsert_relation(&kin_model::Relation {
                    id: kin_model::RelationId::new(),
                    kind,
                    src: GraphNodeId::Entity(src.id),
                    dst: GraphNodeId::Entity(dst.id),
                    confidence: 1.0,
                    origin: RelationOrigin::Parsed,
                    created_in: None,
                    import_source: None,
                    evidence: vec![kin_model::relation::RelationEvidence {
                        source_span: Some(kin_model::SourceSpan {
                            start_line: span.start_line + offset,
                            end_line: span.start_line + offset,
                            ..span.clone()
                        }),
                        ..Default::default()
                    }],
                })
                .unwrap();
        };
        edge(RelationKind::Overrides, &default, &base, 0);
        edge(RelationKind::Calls, &caller, &base, 2);
        // The caller also holds it as a value: its caller row stands for it.
        edge(RelationKind::References, &caller, &base, 1);
        edge(RelationKind::References, &jinja, &base, 2);
        edge(RelationKind::References, &tag, &default, 1);

        let mut result = serde_json::json!({
            "affected_callers": [{"id": caller.id.to_string(), "name": caller.name}],
        });
        attach_call_code(&store, &mut result, &[base.id], &bodies);
        assert_eq!(
            result["affected_callers"][0]["code"], "return self.dumps(obj)",
            "{result}"
        );

        let listed = std::collections::HashSet::from([caller.id]);
        let (rows, withheld) = value_use_rows(
            &store,
            std::slice::from_ref(&base),
            std::slice::from_ref(&default),
            &listed,
            &bodies,
        );
        assert_eq!(withheld, 0);
        assert_eq!(
            rows.iter()
                .map(|row| (
                    row["name"].as_str().unwrap(),
                    row["code"].as_str().unwrap(),
                    row.get("uses").and_then(serde_json::Value::as_str),
                ))
                .collect::<Vec<_>>(),
            [
                (
                    "Flask.create_jinja_environment",
                    "rv.policies[\"json.dumps_function\"] = self.json.dumps",
                    None
                ),
                (
                    "TaggedJSONSerializer.register",
                    "self.default = DefaultJSONProvider.dumps",
                    Some("DefaultJSONProvider.dumps")
                ),
            ],
            "{rows:?}"
        );
        // A text that cannot be read leaves the rows without a quote.
        let (rows, _) = value_use_rows(
            &store,
            std::slice::from_ref(&base),
            &[],
            &listed,
            &crate::call_sites::NoSiteText,
        );
        assert_eq!(rows.len(), 1);
        assert!(rows[0].get("code").is_none(), "{rows:?}");
    }

    #[tokio::test]
    async fn populated_impact_and_both_review_formats_disclose_recorded_enrichment_debt() {
        use crate::call_sites::fixture::{admit, ledger, proof_context};
        use kin_model::{CallSiteState, EntityStore, GraphNodeId, RelationKind, RelationOrigin};
        let store = kin_db::InMemoryGraph::new();
        let target = entity_spanning("target", "src/target.rs", 0, 0);
        let caller = entity_spanning("caller", "src/caller.rs", 0, 0);
        let unrelated = entity_spanning("unrelated_pending", "src/unrelated.rs", 41, 41);
        let context = proof_context(kin_model::LanguageId::Rust, "test");
        admit(
            &store,
            &[&target, &caller, &unrelated],
            vec![
                ledger(&target, "", context.id(), vec![]),
                ledger(
                    &caller,
                    "x()",
                    context.id(),
                    vec![("x", CallSiteState::ProvenTarget { target: target.id })],
                ),
                context,
            ],
        );
        store
            .upsert_relation(&kin_model::Relation {
                id: kin_model::RelationId::new(),
                kind: RelationKind::Calls,
                src: GraphNodeId::Entity(caller.id),
                dst: GraphNodeId::Entity(target.id),
                confidence: 1.0,
                origin: RelationOrigin::Parsed,
                created_in: None,
                import_source: None,
                evidence: vec![],
            })
            .unwrap();
        let args = HashMap::from([
            (
                "entity_ids".into(),
                serde_json::json!([target.id.to_string()]),
            ),
            ("include_traffic".into(), serde_json::json!(false)),
        ]);
        let sessions = SessionRegistry::empty_for_test();
        let parse = |result, tool| {
            let result = crate::envelope::finalize_bounded(
                result,
                crate::Envelope::daemon().with_health(&serde_json::json!({
                    "initialized": true, "graph_loaded": true, "reconciliation_status": "clean",
                })),
                tool,
                &crate::budget::ResponseBudget::default(),
            );
            let crate::types::ContentBlock::Text { text } = &result.content[0];
            serde_json::from_str::<serde_json::Value>(text).unwrap()
        };
        let impact = parse(
            handle_impact_analysis(&args, &store, &sessions)
                .await
                .unwrap(),
            "impact_analysis",
        );
        assert!(!impact["affected_callers"].as_array().unwrap().is_empty());
        let review_json = handle_semantic_review(
            &HashMap::from([("format".into(), serde_json::json!("json"))])
                .into_iter()
                .chain(args.clone())
                .collect(),
            &store,
            &sessions,
        )
        .unwrap();
        let review_text = handle_semantic_review(&args, &store, &sessions).unwrap();
        // The impact answer's own verdict is its direct section's. The
        // unrelated entity's owed enrichment bounds the dependents section,
        // under that section's verdict, and both states ride the answer's.
        assert_eq!(impact["_kin"]["verdict"]["scope"], "direct", "{impact}");
        assert_eq!(
            impact["_kin"]["verdict"]["inputs"]["enrichment"],
            "not_applicable"
        );
        assert_eq!(
            impact["_kin"]["verdict"]["sections"]["dependents"]["state"],
            "inconclusive"
        );
        assert!(
            impact["_kin"]["verdict"]["sections"]["dependents"]["limiting_factor"]
                .as_str()
                .unwrap()
                .contains("enrichment_incomplete"),
            "{impact}"
        );
        for (answer, pointer, verdict) in [
            (impact, "/enrichment", "/sections/dependents/verdict"),
            (
                parse(review_json, "semantic_review"),
                "/impact/enrichment",
                "/_kin/verdict",
            ),
            (
                parse(review_text, "semantic_review"),
                "/impact/enrichment",
                "/_kin/verdict",
            ),
        ] {
            let observation = answer
                .pointer(pointer)
                .expect("the structured observation survives finalization");
            assert_eq!(observation["status"], "bounded", "{answer}");
            if let Some(message) = answer.get("message").and_then(serde_json::Value::as_str) {
                assert!(message.starts_with("Semantic review:"), "{message}");
                assert!(message.contains("coverage limits"), "{message}");
                assert!(!message.contains("src/unrelated.rs"), "{message}");
            }
            assert!(answer.get("formatted").is_none());
            assert_eq!(observation["total_pending_entities"], 1);
            assert_eq!(
                observation["pending_entities"][0]["entity_id"],
                unrelated.id.to_string()
            );
            assert_eq!(
                observation["pending_entities"][0]["projection"]["path"],
                "src/unrelated.rs"
            );
            assert!(observation["pending_entities"][0]["projection"]
                .get("line")
                .is_none());
            let verdict = answer.pointer(verdict).expect("a verdict");
            assert_eq!(verdict["state"], "inconclusive", "{answer}");
            assert_eq!(verdict["inputs"]["enrichment"], "inconclusive");
            assert!(verdict["limiting_factor"]
                .as_str()
                .unwrap()
                .contains("enrichment_incomplete"));
        }
    }

    /// A note anchored by file and line lands on the entity that holds the line,
    /// and on the innermost one when spans nest. A line no entity holds, and a
    /// path with no line at all, fall back to the artifact rather than guessing.
    #[test]
    fn a_path_anchor_resolves_to_the_entity_that_holds_the_line() {
        use kin_model::graph::EntityStore;

        let file = "src/editor.rs";
        let store = kin_db::InMemoryGraph::new();
        let outer = entity_spanning("outer", file, 0, 40);
        let inner = entity_spanning("inner", file, 9, 12);
        store.upsert_entity(&outer).unwrap();
        store.upsert_entity(&inner).unwrap();

        // Read the line to ask for off the same presentation helpers the answer
        // reports, so this tests the rule and not a line-numbering convention.
        let inside_inner = entity_presentation_start_line(&inner).expect("inner is placed");
        let outside_every_span =
            entity_presentation_end_line(&outer).expect("outer is placed") + 100;

        let anchored = |line: Option<u32>| {
            let mut args = HashMap::new();
            args.insert("file_path".to_string(), serde_json::json!(file));
            if let Some(line) = line {
                args.insert("line".to_string(), serde_json::json!(line));
            }
            parse_optional_scope_arg(&args, &store, "kin_review_note_add")
                .expect("an anchor resolves")
                .expect("a file_path always anchors on something")
                .to_string()
        };

        assert_eq!(
            anchored(Some(inside_inner)),
            format!("entity:{}", inner.id),
            "the innermost span that holds the line wins"
        );
        assert_eq!(
            anchored(Some(outside_every_span)),
            format!("artifact:{file}"),
            "a line no entity holds falls back to the artifact"
        );
        assert_eq!(
            anchored(None),
            format!("artifact:{file}"),
            "a path with no line cannot name an entity"
        );
    }

    #[test]
    fn parse_review_create_scopes_accepts_uuid_and_paths() {
        let entity_id = uuid::Uuid::new_v4().to_string();
        let mut args = HashMap::new();
        args.insert(
            "entity_ids".into(),
            serde_json::json!([entity_id, "src/lib.rs", "artifact:README.md"]),
        );

        let scopes = parse_review_create_scopes(&args, &kin_db::InMemoryGraph::new()).unwrap();
        assert_eq!(scopes.len(), 3);
        assert!(matches!(scopes[0], kin_model::WorkScope::Entity(_)));
        assert_eq!(scopes[1].to_string(), "artifact:src/lib.rs");
        assert_eq!(scopes[2].to_string(), "artifact:README.md");
    }

    #[test]
    fn parse_optional_scope_arg_uses_file_anchor_when_scope_missing() {
        let mut args = HashMap::new();
        args.insert("file_path".into(), serde_json::json!("src/main.ts"));

        let scope =
            parse_optional_scope_arg(&args, &kin_db::InMemoryGraph::new(), "kin_review_note_add")
                .unwrap();
        assert_eq!(scope.unwrap().to_string(), "artifact:src/main.ts");
    }

    #[test]
    fn parse_reviewer_list_accepts_batch_assignments() {
        let mut args = HashMap::new();
        args.insert(
            "reviewers".into(),
            serde_json::json!(["alice", "bob", "alice"]),
        );

        let reviewers = parse_reviewer_list(&args).unwrap();
        assert_eq!(reviewers, vec!["alice".to_string(), "bob".to_string()]);
    }

    #[test]
    fn parse_identity_arg_maps_human_kind_to_human_identity() {
        let mut args = HashMap::new();
        args.insert("author".into(), serde_json::json!("troy"));
        args.insert("author_kind".into(), serde_json::json!("human"));

        let identity = parse_identity_arg(&args, "author", "author_kind", "mcp-client");
        assert_eq!(identity.name, "troy");
        assert!(matches!(identity.kind, kin_model::IdentityKind::Human));
    }

    #[test]
    fn shadow_gate_report_fails_loud_on_unknown_base_ref() {
        let store = kin_db::InMemoryGraph::new();
        let mut args = HashMap::new();
        args.insert("base".into(), serde_json::json!("branch:missing"));
        args.insert("head".into(), serde_json::json!("branch:missing"));

        let err = with_empty_test_repository(|authority| {
            handle_shadow_gate_report(&args, &store, Some(authority))
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "unknown branch must error, got: {err}"
        );
    }

    #[test]
    fn shadow_gate_report_fails_loud_on_unimported_git_sha() {
        let store = kin_db::InMemoryGraph::new();
        let mut args = HashMap::new();
        args.insert(
            "base".into(),
            serde_json::json!("1111111111111111111111111111111111111111"),
        );
        args.insert(
            "head".into(),
            serde_json::json!("2222222222222222222222222222222222222222"),
        );

        let err = with_empty_test_repository(|authority| {
            handle_shadow_gate_report(&args, &store, Some(authority))
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("no imported repository alias"),
            "unimported git sha must error, got: {err}"
        );
    }
    fn history_change(
        parent: Option<SemanticChangeId>,
        deltas: Vec<kin_model::change::EntityDelta>,
        message: &str,
        second: usize,
    ) -> kin_model::change::SemanticChange {
        let mut change = kin_model::change::SemanticChange {
            id: SemanticChangeId::from_hash(kin_model::Hash256::from_bytes([0; 32])),
            origin: kin_model::change::ChangeOrigin::Native,
            parents: parent.into_iter().collect(),
            timestamp: serde_json::from_value(serde_json::json!(format!(
                "2026-09-22T21:00:{second:02}Z"
            )))
            .unwrap(),
            author: kin_model::AuthorId::new("History regression"),
            message: message.into(),
            entity_deltas: deltas,
            relation_deltas: vec![],
            tree_deltas: vec![],
            admission_policy_delta: parent.is_none().then(|| {
                kin_model::AdmissionPolicyDelta::initialize(
                    kin_model::SharedAdmissionPolicy::empty(0),
                )
            }),
            projected_files: vec![],
            spec_link: None,
            evidence: vec![],
            risk_summary: None,
            external_reference_deltas: vec![],
            resolution_record_deltas: Vec::new(),
        };
        change.id = kin_model::compute_semantic_change_id(&change).unwrap();
        change
    }

    fn history_query(
        store: &kin_db::InMemoryGraph,
        id: kin_model::EntityId,
        extra: &[(&str, serde_json::Value)],
    ) -> (ToolCallResult, serde_json::Value) {
        let mut args = HashMap::from([("entity_id".into(), serde_json::json!(id.to_string()))]);
        args.extend(
            extra
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone())),
        );
        let raw = handle_entity_history(&args, store).unwrap();
        let mut envelope = crate::envelope::Envelope::daemon();
        envelope.graph_state.loaded = Some(true);
        envelope.graph_state.initialized = Some(true);
        let result = crate::envelope::finalize_bounded(
            raw,
            envelope,
            "entity_history",
            &crate::budget::ResponseBudget::from_arguments(&args),
        );
        let crate::types::ContentBlock::Text { text } = &result.content[0];
        let value = serde_json::from_str(text).unwrap();
        (result, value)
    }

    #[test]
    fn history_and_provenance_metadata_withhold_stored_source_previews() {
        use kin_model::change::EntityDelta;
        use kin_model::graph::ChangeStore;
        let store = kin_db::InMemoryGraph::new();
        let mut focal = entity_spanning("bounded_history", "history.rs", 0, 1);
        for key in [
            "embedding_body_preview",
            "file_import_context",
            "file_surface_context",
        ] {
            focal.metadata.extra.insert(
                key.into(),
                serde_json::json!(format!("private_source_{key}")),
            );
        }
        focal
            .metadata
            .extra
            .insert("retained_fact".into(), serde_json::json!(true));
        let change = history_change(
            None,
            vec![EntityDelta::Added { new: focal.clone() }],
            "metadata history",
            0,
        );
        store.create_change(&change).unwrap();
        let (history, value) = history_query(&store, focal.id, &[]);
        assert_ne!(history.is_error, Some(true), "{value}");
        let args = HashMap::from([
            ("entity_id".into(), serde_json::json!(focal.id)),
            ("compact".into(), serde_json::json!(false)),
        ]);
        let provenance = super::super::provenance::handle_provenance_query(&args, &store).unwrap();
        assert_ne!(provenance.is_error, Some(true));
        for result in [&history, &provenance] {
            let crate::types::ContentBlock::Text { text } = &result.content[0];
            assert!(text.contains("retained_fact"), "{text}");
            assert!(!text.contains("private_source_"), "{text}");
        }
        assert!(
            serde_json::to_string(&store.get_change(&change.id).unwrap())
                .unwrap()
                .contains("private_source_")
        );
    }

    #[test]
    fn entity_history_imported_snapshot_is_focal_and_preserves_native_identity() {
        use kin_model::change::{ChangeOrigin, EntityDelta};
        use kin_model::graph::ChangeStore;
        let store = kin_db::InMemoryGraph::new();
        let focal = entity_spanning("listRun", "list.go", 122, 211);
        let mut deltas = vec![EntityDelta::Added { new: focal.clone() }];
        for n in 0..1000 {
            let mut unrelated = entity_spanning(&format!("unrelated_{n}"), "other.go", 0, 1);
            unrelated.metadata.extra.insert(
                "body".into(),
                serde_json::json!("unrelated-body-marker".repeat(400)),
            );
            deltas.push(EntityDelta::Added { new: unrelated });
        }
        let mut imported = history_change(None, deltas, "Imported snapshot", 0);
        imported.origin = ChangeOrigin::GitCommit {
            oid: kin_model::ids::GitObjectId::sha1([3; 20]),
        };
        imported.id = kin_model::compute_semantic_change_id(&imported).unwrap();
        let mut revised = focal.clone();
        revised.signature = "fn listRun(new_options)".into();
        let native = history_change(
            Some(imported.id),
            vec![EntityDelta::Modified {
                old: focal.clone(),
                new: revised,
            }],
            "Native edit",
            1,
        );
        store.create_change(&imported).unwrap();
        store.create_change(&native).unwrap();
        let page = store.get_entity_history_page(&focal.id, 0, 1).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].entity_delta_count, 1001);
        assert_eq!(page.entries[0].entity_deltas.as_ref().unwrap().len(), 1);
        assert_eq!(page.latest_change_id, Some(native.id));
        let (result, value) = history_query(&store, focal.id, &[]);
        assert_ne!(result.is_error, Some(true));
        let crate::types::ContentBlock::Text { text } = &result.content[0];
        assert!(
            text.len() < 45_000,
            "focal history payload was {} bytes",
            text.len()
        );
        assert!(!text.contains("unrelated-body-marker"));
        assert_eq!(value["result"][0]["id"], serde_json::json!(imported.id));
        assert_eq!(
            value["result"][0]["origin"],
            serde_json::json!(imported.origin)
        );
        assert_eq!(
            value["result"][0]["omitted_sections"]["unrelated_entity_deltas"],
            1000
        );
        assert_eq!(
            value["result"][1]["parents"],
            serde_json::json!(native.parents)
        );
        assert_eq!(value["result"][1]["change_id"], native.id.to_string());
        assert_eq!(value["latest_change_id"], native.id.to_string());
        assert_eq!(value["change_count"], 2);
        assert_eq!(value["_kin"]["completeness"]["bound"], "exact");
    }

    #[test]
    fn entity_history_budget_pagination_reaches_every_change_and_latest_native() {
        use kin_model::change::EntityDelta;
        use kin_model::graph::ChangeStore;
        let store = kin_db::InMemoryGraph::new();
        let focal = entity_spanning("listRun", "list.go", 0, 1);
        let mut expected = Vec::new();
        let mut parent = None;
        let mut previous = focal.clone();
        for n in 0..35 {
            let delta = if n == 0 {
                EntityDelta::Added { new: focal.clone() }
            } else {
                let mut revised = previous.clone();
                revised.signature = format!("fn listRun(revision_{n})");
                let old = std::mem::replace(&mut previous, revised.clone());
                EntityDelta::Modified { old, new: revised }
            };
            let change =
                history_change(parent, vec![delta], &format!("{n}:{}", "界".repeat(150)), n);
            parent = Some(change.id);
            expected.push(change.id.to_string());
            store.create_change(&change).unwrap();
        }
        let mut offset = 0;
        let mut observed = Vec::new();
        for _ in 0..40 {
            let (result, value) = history_query(
                &store,
                focal.id,
                &[
                    ("offset", serde_json::json!(offset)),
                    ("max_chars", serde_json::json!(8000)),
                ],
            );
            assert_ne!(result.is_error, Some(true), "{value}");
            let crate::types::ContentBlock::Text { text } = &result.content[0];
            assert!(text.len() <= 8000, "actual payload bytes {}", text.len());
            assert!(
                text.len() > text.chars().count(),
                "non-ASCII fixture must exercise UTF-8 byte accounting"
            );
            assert_eq!(value["_kin"]["response"]["chars_after_budget"], text.len());
            let rows = value["result"].as_array().unwrap();
            assert!(!rows.is_empty());
            assert_eq!(value["returned"], rows.len());
            assert_eq!(
                value["_kin"]["completeness"]["counted"]["returned"],
                rows.len()
            );
            assert_eq!(value["latest_change_id"], expected.last().unwrap().as_str());
            assert_eq!(value["change_count"], 35);
            assert_eq!(value["_kin"]["completeness"]["bound"], "at_least");
            observed.extend(
                rows.iter()
                    .map(|row| row["change_id"].as_str().unwrap().to_string()),
            );
            match value["next_offset"].as_u64() {
                Some(next) => {
                    assert_eq!(next, offset + rows.len() as u64);
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(observed, expected);
        let (_, beyond) = history_query(&store, focal.id, &[("offset", serde_json::json!(1000))]);
        assert_eq!(beyond["result"], serde_json::json!([]));
        assert!(
            beyond.get("negative").is_none(),
            "an empty page is not no history"
        );
    }

    #[test]
    fn entity_history_empty_unknown_retired_and_timestamp_ties_are_distinct() {
        use kin_model::change::EntityDelta;
        use kin_model::graph::{ChangeStore, EntityStore};
        let store = kin_db::InMemoryGraph::new();
        let focal = entity_spanning("retired", "old.rs", 0, 1);
        let (missing, _) = history_query(&store, focal.id, &[]);
        assert_eq!(missing.is_error, Some(true));
        store.upsert_entity(&focal).unwrap();
        let (empty_result, empty) = history_query(&store, focal.id, &[]);
        assert_ne!(empty_result.is_error, Some(true));
        assert_eq!(empty["change_count"], 0);
        assert_eq!(empty["negative"]["kind"], "no_history");
        let first = history_change(
            None,
            vec![EntityDelta::Added { new: focal.clone() }],
            "one",
            0,
        );
        let second = history_change(
            Some(first.id),
            vec![EntityDelta::Removed { old: focal.clone() }],
            "two",
            0,
        );
        store.create_change(&first).unwrap();
        store.create_change(&second).unwrap();
        store.remove_entity(&focal.id).unwrap();
        let (retired_result, retired) = history_query(&store, focal.id, &[]);
        assert_ne!(retired_result.is_error, Some(true));
        let mut expected = vec![first.id.to_string(), second.id.to_string()];
        expected.sort();
        let observed = retired["result"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["change_id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(observed, expected);
    }

    /// A page or budget field out of range, or not an integer, is refused with
    /// the sentence the routed tool's schema check uses, never clamped; the
    /// bounds themselves are accepted.
    #[test]
    fn entity_history_refuses_out_of_range_and_non_integer_page_fields() {
        use kin_model::change::EntityDelta;
        use kin_model::graph::ChangeStore;
        let store = kin_db::InMemoryGraph::new();
        let focal = entity_spanning("paged", "paged.rs", 0, 1);
        store
            .create_change(&history_change(
                None,
                vec![EntityDelta::Added { new: focal.clone() }],
                "added",
                0,
            ))
            .unwrap();
        let refused = |extra: &[(&str, serde_json::Value)], expected: &str| {
            let (result, value) = history_query(&store, focal.id, extra);
            assert_eq!(result.is_error, Some(true), "{extra:?}: {value}");
            assert_eq!(
                value["error"]["code"], "history_parameters_out_of_range",
                "{extra:?}: {value}"
            );
            assert_eq!(value["error"]["message"], expected, "{extra:?}: {value}");
            assert!(value.get("result").is_none(), "{value}");
        };
        refused(
            &[("limit", serde_json::json!(0))],
            "entity_history: limit must be at least 1.",
        );
        refused(
            &[("limit", serde_json::json!(500))],
            "entity_history: limit must be at most 100.",
        );
        refused(
            &[("limit", serde_json::json!("ten"))],
            "entity_history: limit must be an integer.",
        );
        refused(
            &[("limit", serde_json::json!(20.5))],
            "entity_history: limit must be an integer.",
        );
        refused(
            &[("max_chars", serde_json::json!(1999))],
            "entity_history: max_chars must be at least 2000.",
        );
        refused(
            &[("max_chars", serde_json::json!(60001))],
            "entity_history: max_chars must be at most 60000.",
        );
        refused(
            &[("max_chars", serde_json::json!("big"))],
            "entity_history: max_chars must be an integer.",
        );
        refused(
            &[("offset", serde_json::json!(-1))],
            "entity_history: offset must be at least 0.",
        );
        refused(
            &[
                ("offset", serde_json::json!(-1)),
                ("limit", serde_json::json!(0)),
            ],
            "entity_history: limit must be at least 1; offset must be at least 0.",
        );
        // Every bound itself is accepted: whatever the answer is, it is not a
        // parameter refusal. At max_chars 2,000 the answer is the budget's own
        // refusal, because this change's metadata alone does not fit.
        for (field, bound) in [
            ("limit", 1),
            ("limit", 100),
            ("max_chars", 2000),
            ("max_chars", 60000),
            ("offset", 0),
        ] {
            let (_, value) = history_query(&store, focal.id, &[(field, serde_json::json!(bound))]);
            assert_ne!(
                value["error"]["code"], "history_parameters_out_of_range",
                "{field} {bound}: {value}"
            );
        }
        let (result, accepted) =
            history_query(&store, focal.id, &[("limit", serde_json::json!(100))]);
        assert_ne!(result.is_error, Some(true), "{accepted}");
        assert_eq!(accepted["change_count"], 1);
        let (_, null_limit) =
            history_query(&store, focal.id, &[("limit", serde_json::Value::Null)]);
        assert_eq!(
            null_limit["limit"], 20,
            "a null reads as absent: {null_limit}"
        );
    }

    /// The handler's own page: 20 rows by default and 100 at most, with
    /// next_offset following the rows returned.
    #[test]
    fn entity_history_pages_default_to_twenty_and_stop_at_one_hundred() {
        use kin_model::change::EntityDelta;
        use kin_model::graph::ChangeStore;
        let store = kin_db::InMemoryGraph::new();
        let focal = entity_spanning("hundreds", "hundreds.rs", 0, 1);
        let mut parent = None;
        let mut previous = focal.clone();
        for n in 0..120 {
            let delta = if n == 0 {
                EntityDelta::Added { new: focal.clone() }
            } else {
                let mut revised = previous.clone();
                revised.signature = format!("fn hundreds(revision_{n})");
                let old = std::mem::replace(&mut previous, revised.clone());
                EntityDelta::Modified { old, new: revised }
            };
            // The helper's clock is one minute of seconds; ties order by id,
            // which leaves the counts this test reads unchanged.
            let change = history_change(parent, vec![delta], &format!("{n}"), n % 60);
            parent = Some(change.id);
            store.create_change(&change).unwrap();
        }
        let raw = |extra: &[(&str, serde_json::Value)]| -> serde_json::Value {
            let mut args =
                HashMap::from([("entity_id".into(), serde_json::json!(focal.id.to_string()))]);
            args.extend(
                extra
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.clone())),
            );
            let result = handle_entity_history(&args, &store).unwrap();
            assert_ne!(result.is_error, Some(true));
            let crate::types::ContentBlock::Text { text } = &result.content[0];
            serde_json::from_str(text).unwrap()
        };
        let first = raw(&[]);
        assert_eq!(
            (first["limit"].as_u64(), first["returned"].as_u64()),
            (Some(20), Some(20))
        );
        assert_eq!(first["next_offset"], 20);
        assert_eq!(first["change_count"], 120);
        let widest = raw(&[("limit", serde_json::json!(100))]);
        assert_eq!(widest["result"].as_array().unwrap().len(), 100);
        assert_eq!(widest["next_offset"], 100);
        let last = raw(&[
            ("offset", serde_json::json!(100)),
            ("limit", serde_json::json!(100)),
        ]);
        assert_eq!(last["returned"], 20);
        assert!(last["next_offset"].is_null());
        assert_eq!(last["latest_change_id"], first["latest_change_id"]);
    }

    /// A change whose ancestry cannot be carried exactly is named by its place
    /// in the history, with the pages that read every other row.
    #[test]
    fn entity_history_names_the_offending_offset_and_the_pages_around_it() {
        use kin_model::change::EntityDelta;
        use kin_model::graph::ChangeStore;
        let store = kin_db::InMemoryGraph::new();
        let focal = entity_spanning("merged", "merged.rs", 0, 1);
        let root = history_change(
            None,
            vec![EntityDelta::Added { new: focal.clone() }],
            "root",
            0,
        );
        store.create_change(&root).unwrap();
        let mut revised = focal.clone();
        revised.signature = "fn merged(wide)".into();
        let mut wide = history_change(
            Some(root.id),
            vec![EntityDelta::Modified {
                old: focal.clone(),
                new: revised.clone(),
            }],
            "a merge too wide to carry",
            1,
        );
        wide.parents = std::iter::once(root.id)
            .chain((0..200u64).map(|seed| {
                let mut bytes = [0x77; 32];
                bytes[..8].copy_from_slice(&seed.to_le_bytes());
                SemanticChangeId::from_hash(kin_model::Hash256::from_bytes(bytes))
            }))
            .collect();
        wide.id = kin_model::compute_semantic_change_id(&wide).unwrap();
        store.create_change(&wide).unwrap();
        let mut last = revised.clone();
        last.signature = "fn merged(after)".into();
        let after = history_change(
            Some(wide.id),
            vec![EntityDelta::Modified {
                old: revised,
                new: last,
            }],
            "after",
            2,
        );
        store.create_change(&after).unwrap();

        let (result, value) = history_query(&store, focal.id, &[]);
        assert_eq!(result.is_error, Some(true), "{value}");
        let error = &value["error"];
        assert_eq!(error["code"], "history_ancestry_exceeds_limit");
        assert_eq!(error["change_id"], wide.id.to_string());
        assert_eq!(error["requested_offset"], 0);
        assert_eq!(error["requested_limit"], 20);
        assert_eq!(error["offending_offset"], 1);
        assert_eq!(error["parent_count"], 201);
        assert_eq!(
            error["pages_around_it"],
            serde_json::json!([{"offset": 0, "limit": 1}, {"offset": 2, "limit": 20}])
        );
        for page in error["pages_around_it"].as_array().unwrap() {
            let (result, value) = history_query(
                &store,
                focal.id,
                &[
                    ("offset", page["offset"].clone()),
                    ("limit", page["limit"].clone()),
                ],
            );
            assert_ne!(result.is_error, Some(true), "{page}: {value}");
            assert_eq!(value["returned"], 1, "{page}: {value}");
        }
    }

    #[test]
    fn entity_history_large_focal_message_and_author_are_summarized_before_clone() {
        use kin_model::change::EntityDelta;
        use kin_model::graph::ChangeStore;
        let store = kin_db::InMemoryGraph::new();
        let mut focal = entity_spanning("large", "large.rs", 0, 1);
        focal
            .metadata
            .extra
            .insert("body".into(), serde_json::json!("巨".repeat(100_000)));
        let message = "史".repeat(100_000);
        let mut change = history_change(
            None,
            vec![EntityDelta::Added { new: focal.clone() }],
            &message,
            0,
        );
        change.author = kin_model::AuthorId::new("名".repeat(100_000));
        change.id = kin_model::compute_semantic_change_id(&change).unwrap();
        store.create_change(&change).unwrap();
        let page = store.get_entity_history_page(&focal.id, 0, 20).unwrap();
        let entry = &page.entries[0];
        assert!(entry.entity_deltas.is_none());
        assert!(entry.message.is_none());
        assert!(entry.author.is_none());
        assert_eq!(
            entry.metadata_omissions["message_utf8_bytes"],
            message.len()
        );
        let (result, value) = history_query(&store, focal.id, &[]);
        assert_ne!(result.is_error, Some(true));
        assert_eq!(
            value["result"][0]["detail_summary"]["reason"],
            "focal_detail_limit"
        );
        // The pointer is a bounded history call, and it says plainly that the
        // detail past the focal limit is not available at any budget.
        assert_eq!(
            value["result"][0]["detail_summary"]["largest_view"],
            serde_json::json!({"tool": "entity_history", "arguments": {
                "entity_id": focal.id.to_string(), "offset": 0, "limit": 1,
                "max_chars": crate::budget::RESPONSE_MAX_MAX_CHARS}})
        );
        assert!(value["result"][0]["detail_summary"]["disclosure"]
            .as_str()
            .unwrap()
            .contains("not available from history at any budget"));
        assert!(
            !value["result"][0]["detail_summary"]
                .to_string()
                .contains("semantic_diff"),
            "no unbounded recovery is promised"
        );
        assert_eq!(value["result"][0]["entity_deltas_omitted"], 1);
        assert_eq!(value["result"][0]["focal_delta_operations"]["added"], 1);
        assert_eq!(value["result"][0]["change_id"], change.id.to_string());
        let mut ancestry = change.clone();
        ancestry.parents = (0..1000).map(|_| change.id).collect();
        let entry =
            kin_model::change::EntityHistoryEntry::for_entity(&ancestry, &focal.id).unwrap();
        assert!(entry.parents.is_none());
        assert_eq!(entry.metadata_omissions["parents"], 1000);
    }

    #[test]
    fn entity_history_irreducible_budget_refuses_on_raw_and_enveloped_surfaces() {
        let budget = crate::budget::ResponseBudget {
            max_chars: 2000,
            explicit_max_chars: true,
            ..Default::default()
        };
        let payload = serde_json::json!({"entity_id": "focal", "result": [{"id": "a".repeat(64),
            "parents": vec!["b".repeat(64); 1000]}], "change_count": 1, "latest_change_id": "a".repeat(64), "offset": 0 });
        let mut raw = payload.clone();
        crate::budget::enforce(&mut raw, "entity_history", &budget);
        assert!(!crate::budget::fit_history_payload(
            &mut raw,
            "entity_history",
            &budget
        ));
        assert_eq!(raw["error"]["code"], "history_metadata_exceeds_budget");
        assert!(crate::budget::measure(&raw) <= 2000);
        let result = crate::envelope::finalize_bounded(
            ToolCallResult::text(payload.to_string()),
            crate::envelope::Envelope::daemon(),
            "entity_history",
            &budget,
        );
        assert_eq!(result.is_error, Some(true));
        let crate::types::ContentBlock::Text { text } = &result.content[0];
        assert!(text.len() <= 2000);
        let error: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(error["error"]["code"], "history_metadata_exceeds_budget");
        assert!(error.get("result").is_none());
    }
}
