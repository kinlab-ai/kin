// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Read-only answers the workspace page explores a repository with: an
//! overview, one question answered by meaning and by name, with the chain of
//! calls behind it when it asks how or where something happens, one entity
//! in independently fallible sections, and a map of the whole repository.
//!
//! Every answer speaks of entities. No field carries a place in the checkout,
//! even where the daemon's own answer holds one, and a graph miss is reported
//! as a gap in plain words rather than answered from anywhere else. Each
//! daemon read is bounded on its own, so one slow read leaves its section
//! empty with a reason and never blanks the rest of the page.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::graph_export::{GraphExportNode, GraphExportPayload};
use super::review_ui::{author_display, UiState};
use crate::daemon_client::DaemonClient;

/// How long one daemon read may take before its section reports a gap.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// The reference, impact and history walks read more of the graph and the
/// recorded history, so each is given longer than a point read.
const WALK_TIMEOUT: Duration = Duration::from_secs(20);
/// The whole-graph read behind the overview and the map does real work on a
/// large repository, so it is given longer than a point read.
const GRAPH_TIMEOUT: Duration = Duration::from_secs(30);
/// A per-result enrichment read on the ask answer. A miss only drops the
/// result's container, so it is kept short.
const ENRICH_TIMEOUT: Duration = Duration::from_secs(4);
const ENRICH_CONCURRENCY: usize = 8;

const QUERY_MAX_BYTES: usize = 256;
const ASK_LIMIT_DEFAULT: usize = 20;
const ASK_LIMIT_MAX: usize = 50;

const OVERVIEW_CONTAINERS: usize = 12;
const OVERVIEW_RELIED_ON: usize = 10;
/// How many of the most relied-on rows are read, so the ones with no
/// definition here can be left out and the list still fills.
const OVERVIEW_RELIED_ON_READ: usize = 40;
/// How many of the entities others rely on that no test reaches are shown.
const OVERVIEW_UNTESTED: usize = 12;
const OVERVIEW_CHANGES: usize = 5;

const ENTITY_LIST_CAP: usize = 50;
const NEIGHBORHOOD_LIMIT: usize = 120;
const IMPACT_DEPTH: u32 = super::verify::WORK_TEST_REACH_DEPTH;
const IMPACT_TOP: usize = 8;
const HISTORY_CAP: usize = 10;
/// Inventory rows read to find one entity's doc summary by its name.
const SUMMARY_LOOKUP_ROWS: usize = 100;

pub(super) const MAP_NODE_CAP: usize = 150;
pub(super) const MAP_EDGE_CAP: usize = 400;

const UNPROVEN_SITES: &str = "Kin may have missed some callers, so there could be more.";
const INCOMPLETE_COVERAGE: &str = "Kin is still mapping this code, so there could be more.";

pub(super) fn routes() -> Router<Arc<UiState>> {
    Router::new()
        .route("/api/workspace/overview", get(serve_overview))
        .route("/api/workspace/ask", get(serve_ask))
        .route("/api/workspace/entity", get(serve_entity))
        .route("/api/workspace/map", get(serve_map))
}

// ---------------------------------------------------------------------------
// Shared wording and shapes
// ---------------------------------------------------------------------------

/// Text a reader sees, with the dashes this page never shows replaced.
pub(super) fn plain(text: &str) -> String {
    text.replace(" \u{2014} ", ", ")
        .replace('\u{2014}', ", ")
        .trim()
        .to_string()
}

/// One sentence for a section that could not be read: what was not read, then
/// the reason as the daemon or client gave it.
fn gap(what: &str, error: &anyhow::Error) -> String {
    let mut reason = readable(&format!("{error:#}"));
    if let Some(first) = reason.get(..1) {
        let upper = first.to_uppercase();
        reason.replace_range(..1, &upper);
    }
    if !reason.ends_with(['.', '!', '?']) {
        reason.push('.');
    }
    plain(&format!("{what} {reason}"))
}

fn section(what: &str, answer: Result<Map<String, Value>>) -> Value {
    match answer {
        Ok(mut fields) => {
            fields.insert("ok".into(), Value::Bool(true));
            Value::Object(fields)
        }
        Err(error) => json!({ "ok": false, "reason": gap(what, &error) }),
    }
}

fn fields(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// A daemon refusal as a sentence. A graph tool refuses with a JSON object
/// whose `message` is the sentence; anything else is used as given.
fn readable(error: &str) -> String {
    for (at, _) in error.match_indices('{') {
        if let Ok(value) = serde_json::from_str::<Value>(&error[at..]) {
            if let Some(message) = value["message"].as_str() {
                return message.trim().to_string();
            }
        }
    }
    error.to_string()
}

/// Bound one daemon read so a slow one reports a gap instead of holding the
/// whole answer.
async fn bounded<T>(limit: Duration, read: impl Future<Output = Result<T>>) -> Result<T> {
    match tokio::time::timeout(limit, read).await {
        Ok(answer) => answer,
        Err(_) => anyhow::bail!("it did not answer within {} seconds", limit.as_secs()),
    }
}

fn read_at() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

fn daemon_unreachable(error: anyhow::Error) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({ "error": plain(&format!("{error:#}")) })),
    )
        .into_response()
}

/// The graph's spelling of a kind or relation, as the page reads it:
/// `TraitDef`, `trait_def` and `traitdef` are all `trait_def`, `Calls` is
/// `calls`.
pub(super) fn kind_label(raw: &str) -> String {
    let mut label = String::with_capacity(raw.len() + 4);
    for character in raw.trim().chars() {
        if character.is_ascii_uppercase() {
            if !label.is_empty() && !label.ends_with('_') {
                label.push('_');
            }
            label.push(character.to_ascii_lowercase());
        } else if character == ' ' || character == '-' {
            label.push('_');
        } else {
            label.push(character);
        }
    }
    // Locate publishes a kind as its variant name lowercased, which runs a
    // compound kind together: `typealias` is the `type_alias` every other
    // answer says.
    match RUN_TOGETHER.iter().find(|(joined, _)| *joined == label) {
        Some((_, spelled)) => (*spelled).to_string(),
        None => label,
    }
}

/// Each compound kind and relation as a lowercased variant name spells it,
/// beside the one spelling the page reads.
const RUN_TOGETHER: [(&str, &str); 21] = [
    ("traitdef", "trait_def"),
    ("typealias", "type_alias"),
    ("apiendpoint", "api_endpoint"),
    ("eventcontract", "event_contract"),
    ("documentnode", "document_node"),
    ("enumdef", "enum_def"),
    ("enumvariant", "enum_variant"),
    ("staticvar", "static_var"),
    ("usesmacro", "uses_macro"),
    ("usestype", "uses_type"),
    ("dependson", "depends_on"),
    ("emitsevent", "emits_event"),
    ("subscribesto", "subscribes_to"),
    ("definescontract", "defines_contract"),
    ("consumescontract", "consumes_contract"),
    ("sendsmessage", "sends_message"),
    ("cochanges", "co_changes"),
    ("derivedfrom", "derived_from"),
    ("documentedby", "documented_by"),
    ("ownedby", "owned_by"),
    ("ownedbyfile", "owned_by_file"),
];

/// A signature a reader is shown. A module, package or file entity's
/// recorded signature is the place its text is held, not a declaration, so
/// none is shown for one.
pub(super) fn shown_signature(kind: &str, signature: Option<&str>) -> Option<String> {
    if matches!(kind_label(kind).as_str(), "module" | "package" | "file") {
        return None;
    }
    signature
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn entity_ref(id: &str, name: &str, kind: &str) -> Value {
    json!({ "id": id, "name": name, "kind": kind_label(kind) })
}

/// The entity a daemon answer names as the owner of another, if it names one.
fn owner_ref(owners: &Value) -> Option<Value> {
    let owner = owners.as_array()?.first()?;
    Some(entity_ref(
        owner["entity_id"].as_str()?,
        owner["name"].as_str()?,
        owner["kind"].as_str().unwrap_or_default(),
    ))
}

fn text_field(value: &Value, key: &str) -> Option<String> {
    value[key]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// The JSON object a graph tool answered with.
fn tool_json(result: kin_mcp::ToolCallResult, name: &str) -> Result<Value> {
    anyhow::ensure!(
        result.content.len() == 1,
        "{name} answered with an unsupported content shape"
    );
    let kin_mcp::ContentBlock::Text { text } = &result.content[0];
    anyhow::ensure!(result.is_error != Some(true), "{text}");
    let value: Value = serde_json::from_str(text).context("the answer was not JSON")?;
    anyhow::ensure!(value.is_object(), "the answer was not an object");
    Ok(value)
}

fn validate_text(text: &str, what: &str) -> std::result::Result<(), String> {
    if text.len() > QUERY_MAX_BYTES {
        return Err(format!("{what} is limited to {QUERY_MAX_BYTES} bytes."));
    }
    if text.chars().any(char::is_control) {
        return Err(format!("{what} contains a control character."));
    }
    Ok(())
}

fn parse_entity_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    match uuid::Uuid::parse_str(trimmed) {
        Ok(id) if trimmed.len() == 36 => Some(id.to_string()),
        _ => None,
    }
}

async fn connect(
    state: &UiState,
    purpose: &str,
) -> Result<(DaemonClient, crate::daemon_client::HealthResponse)> {
    let client = DaemonClient::connect_for_workspace(purpose, &state.layout).await?;
    let health = bounded(READ_TIMEOUT, client.health()).await?;
    Ok((client, health))
}

// ---------------------------------------------------------------------------
// Containment, read from the graph export
// ---------------------------------------------------------------------------

/// How broad a kind is as a container: a package holds modules, a module
/// holds classes, a class holds methods. Lower is broader.
fn containment_rank(kind: &str) -> u8 {
    match kind_label(kind).as_str() {
        "package" => 0,
        "module" | "file" => 1,
        "class" | "interface" | "trait_def" | "enum_def" | "schema" | "document_node" => 2,
        _ => 3,
    }
}

fn is_contains(kind: &str) -> bool {
    kind_label(kind) == "contains"
}

/// Whether `member` is named under `container`, as `Signer.sign` is under
/// `Signer`. Member entities carry their owner-qualified name.
fn qualified_under(member: &str, container: &str) -> bool {
    member
        .strip_prefix(container)
        .is_some_and(|rest| rest.starts_with('.') || rest.starts_with("::"))
        && !container.is_empty()
}

/// What contains what, and how connected each entity is, over one export.
///
/// The export merges the two directions of a pair, so a `contains` link is
/// oriented here: the entity whose name the other is qualified under
/// contains it, and otherwise the broader kind contains the narrower. A link
/// that neither rule orients is counted in `unoriented` and placed nowhere.
/// Every walk up the result is bounded, so no recorded loop can hang it.
pub(super) struct Census<'a> {
    nodes: HashMap<&'a str, &'a GraphExportNode>,
    parent: HashMap<&'a str, &'a str>,
    children: HashMap<&'a str, Vec<&'a str>>,
    descendants: HashMap<&'a str, usize>,
    /// Relationships each entity takes part in, `contains` excluded.
    degree: HashMap<&'a str, usize>,
    pub(super) unoriented: usize,
}

impl<'a> Census<'a> {
    pub(super) fn new(payload: &'a GraphExportPayload) -> Self {
        let nodes: HashMap<&str, &GraphExportNode> = payload
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), node))
            .collect();
        let mut parent: HashMap<&str, &str> = HashMap::new();
        let mut degree: HashMap<&str, usize> = HashMap::new();
        let mut unoriented = 0;
        for link in &payload.links {
            let (Some(a), Some(b)) = (
                nodes.get(link.source.as_str()).copied(),
                nodes.get(link.target.as_str()).copied(),
            ) else {
                continue;
            };
            if a.id == b.id {
                continue;
            }
            if !is_contains(&link.kind) {
                *degree.entry(a.id.as_str()).or_default() += 1;
                *degree.entry(b.id.as_str()).or_default() += 1;
                continue;
            }
            let (rank_a, rank_b) = (containment_rank(&a.kind), containment_rank(&b.kind));
            // A member's name is qualified under its owner's, which settles
            // even a class defined inside a method. The kinds settle the rest.
            let oriented = if qualified_under(&b.name, &a.name) {
                Some((a, b))
            } else if qualified_under(&a.name, &b.name) {
                Some((b, a))
            } else if rank_a < rank_b {
                Some((a, b))
            } else if rank_b < rank_a {
                Some((b, a))
            } else {
                None
            };
            let Some((container, member)) = oriented else {
                unoriented += 1;
                continue;
            };
            // Two containers for one member keep the one the member is named
            // under, then the narrower, then the smaller id, so the answer
            // never depends on link order.
            let standing = |container: &GraphExportNode| {
                (
                    qualified_under(&member.name, &container.name),
                    containment_rank(&container.kind),
                    std::cmp::Reverse(container.id.clone()),
                )
            };
            let keep = match parent.get(member.id.as_str()) {
                None => true,
                Some(held) => standing(container) > standing(nodes[*held]),
            };
            if keep {
                parent.insert(member.id.as_str(), container.id.as_str());
            }
        }
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for (member, container) in &parent {
            children.entry(container).or_default().push(member);
        }
        for members in children.values_mut() {
            members.sort_by(|a, b| nodes[a].name.cmp(&nodes[b].name).then(a.cmp(b)));
        }
        let mut census = Self {
            nodes,
            parent,
            children,
            descendants: HashMap::new(),
            degree,
            unoriented,
        };
        let containers: Vec<&str> = census.children.keys().copied().collect();
        for container in containers {
            census.count_descendants(container, 0);
        }
        census
    }

    fn count_descendants(&mut self, id: &'a str, depth: usize) -> usize {
        if let Some(count) = self.descendants.get(id) {
            return *count;
        }
        let members = self.children.get(id).cloned().unwrap_or_default();
        let mut count = 0;
        if depth < 64 {
            for member in members {
                count += 1 + self.count_descendants(member, depth + 1);
            }
        }
        self.descendants.insert(id, count);
        count
    }

    fn node(&self, id: &str) -> Option<&'a GraphExportNode> {
        self.nodes.get(id).copied()
    }

    /// Whether the repository holds a definition of this entity. A name the
    /// code imports or refers to without defining it here carries no source.
    fn defined(&self, id: &str) -> bool {
        self.node(id).is_some_and(|node| node.file.is_some())
    }

    /// What contains an entity, and under `within` what contains that. Two
    /// containers can share a name and a kind, and the one a container sits
    /// in is what tells them apart.
    fn container_of(&self, id: &str) -> Option<Value> {
        let container = self.node(self.parent.get(id)?)?;
        let mut named = entity_ref(&container.id, &container.name, &container.kind);
        let outer = self
            .parent
            .get(container.id.as_str())
            .and_then(|outer| self.node(outer));
        if let Some(outer) = outer {
            named["within"] = entity_ref(&outer.id, &outer.name, &outer.kind);
        }
        Some(named)
    }

    fn top(&self, id: &'a str) -> &'a str {
        let mut current = id;
        for _ in 0..64 {
            match self.parent.get(current) {
                Some(container) => current = *container,
                None => break,
            }
        }
        current
    }

    fn members(&self, id: &str) -> usize {
        self.children.get(id).map_or(0, Vec::len)
    }

    fn size(&self, id: &str) -> usize {
        self.descendants.get(id).copied().unwrap_or(0)
    }

    fn degree(&self, id: &str) -> usize {
        self.degree.get(id).copied().unwrap_or(0)
    }

    /// Defined entities that contain another, largest first.
    fn containers(&self) -> Vec<&'a GraphExportNode> {
        let mut containers: Vec<&GraphExportNode> = self
            .children
            .keys()
            .filter(|id| self.defined(id))
            .filter_map(|id| self.node(id))
            .collect();
        containers.sort_by(|a, b| {
            self.size(&b.id)
                .cmp(&self.size(&a.id))
                .then_with(|| self.members(&b.id).cmp(&self.members(&a.id)))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.id.cmp(&b.id))
        });
        containers
    }

    /// Containers that sit inside nothing.
    fn top_containers(&self) -> Vec<&'a GraphExportNode> {
        self.containers()
            .into_iter()
            .filter(|node| !self.parent.contains_key(node.id.as_str()))
            .collect()
    }
}

fn kind_counts(payload: &GraphExportPayload) -> Vec<Value> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for node in &payload.nodes {
        *counts.entry(kind_label(&node.kind)).or_default() += 1;
    }
    let mut rows: Vec<(String, usize)> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.into_iter()
        .map(|(kind, count)| json!({ "kind": kind, "count": count }))
        .collect()
}

fn without_definition_notice(count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some("1 entity is a name the code imports or refers to that has no definition in this repository, such as an imported module.".to_string()),
        count => Some(format!("{count} entities are names the code imports or refers to that have no definition in this repository, such as an imported module.")),
    }
}

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

pub(super) fn overview_counts(payload: &GraphExportPayload) -> Map<String, Value> {
    let without = payload
        .nodes
        .iter()
        .filter(|node| node.file.is_none())
        .count();
    let mut counts = fields(json!({
        "entities": payload.entity_count,
        "relationships": payload.relation_count,
        "defined_here": payload.entity_count.saturating_sub(without),
        "without_definition": without,
        "by_kind": kind_counts(payload),
    }));
    if let Some(notice) = without_definition_notice(without) {
        counts.insert("notice".into(), Value::String(notice));
    }
    counts
}

/// The daemon's own counts, for when the whole-graph read did not answer.
fn overview_counts_from_summary(summary: &Value) -> Result<Map<String, Value>> {
    let entities = summary["entities"]
        .as_u64()
        .context("the overview answer omitted its entity count")?;
    let mut by_kind: Vec<(String, u64)> = summary["kinds"]
        .as_object()
        .map(|kinds| {
            kinds
                .iter()
                .filter_map(|(kind, count)| Some((kind_label(kind), count.as_u64()?)))
                .collect()
        })
        .unwrap_or_default();
    by_kind.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(fields(json!({
        "entities": entities,
        "relationships": summary["edges"],
        "by_kind": by_kind.into_iter().map(|(kind, count)| json!({"kind": kind, "count": count})).collect::<Vec<_>>(),
    })))
}

fn overview_languages(summary: &Value) -> Result<Map<String, Value>> {
    let languages = summary["languages"].as_object().context(
        "This repository's daemon does not report languages. It predates this version of kin ui.",
    )?;
    let mut rows: Vec<(String, u64)> = languages
        .iter()
        .filter_map(|(language, count)| Some((language.to_lowercase(), count.as_u64()?)))
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(fields(json!({
        "items": rows.into_iter().map(|(language, entities)| json!({"language": language, "entities": entities})).collect::<Vec<_>>(),
    })))
}

/// Whether counts of what relies on an entity are exact or lower bounds,
/// from the daemon's store-wide call-site reading in its summary: `exact`
/// when every call site the store holds is settled, `at_least` while some
/// are not, since a call Kin cannot prove may be one more caller.
///
/// `None` when the summary was not read or its daemon does not report the
/// reading, so the page is told no bound at all rather than a guessed one.
fn relied_on_bound(summary: Option<&Value>) -> Option<&'static str> {
    let settled = summary?.get("call_sites_settled")?.as_bool()?;
    Some(if settled { "exact" } else { "at_least" })
}

/// `section` with `bound` under `key`, or as it was when there is none.
fn with_bound(
    mut section: Map<String, Value>,
    key: &str,
    bound: Option<&'static str>,
) -> Map<String, Value> {
    if let Some(bound) = bound {
        section.insert(key.into(), Value::String(bound.into()));
    }
    section
}

pub(super) fn overview_containers(census: &Census<'_>, limit: usize) -> Map<String, Value> {
    let containers = census.containers();
    let total = containers.len();
    let items: Vec<Value> = containers
        .into_iter()
        .take(limit)
        .map(|node| {
            json!({
                "id": node.id, "name": node.name, "kind": kind_label(&node.kind),
                "members": census.members(&node.id), "entities": census.size(&node.id),
                "container": census.container_of(&node.id),
                // What tells two containers of one name apart, since no
                // answer here says where either is. Null from a daemon that
                // does not report it.
                "language": node.language.as_deref().map(str::to_lowercase),
            })
        })
        .collect();
    fields(json!({ "shown": items.len(), "items": items, "total": total }))
}

/// The most relied-on entities from the daemon's importance ranking, leaving
/// out names with no definition here.
pub(super) fn overview_relied_on(
    inventory: &Value,
    census: Option<&Census<'_>>,
    limit: usize,
) -> Result<Map<String, Value>> {
    let rows = inventory["entities"]
        .as_array()
        .context("the ranking omitted its rows")?;
    let mut excluded = 0;
    let mut items = Vec::new();
    for row in rows {
        if items.len() >= limit {
            break;
        }
        let Some(id) = row["id"].as_str() else {
            continue;
        };
        if row["file_path"].is_null() {
            excluded += 1;
            continue;
        }
        let mut item = fields(entity_ref(
            id,
            row["name"].as_str().unwrap_or_default(),
            row["kind"].as_str().unwrap_or_default(),
        ));
        if let Some(signature) = shown_signature(
            row["kind"].as_str().unwrap_or_default(),
            row["signature"].as_str(),
        ) {
            item.insert("signature".into(), Value::String(signature));
        }
        item.insert(
            "container".into(),
            census
                .and_then(|census| census.container_of(id))
                .unwrap_or(Value::Null),
        );
        item.insert("dependents".into(), row["dependents"].clone());
        items.push(Value::Object(item));
    }
    Ok(fields(json!({
        "shown": items.len(),
        "items": items,
        "ranked": inventory["total"],
        "excluded_without_definition": excluded,
        "basis": "Ranked by how many distinct entities call, import or reference each one.",
    })))
}

/// The entities many others rely on that no test reaches, most relied-on
/// first, from the daemon's reading of the whole repository.
///
/// The daemon reads coverage in the pass that ranks, with the rule `kin
/// verify --missing` reads it with, and counts dependents as the ranking
/// does, so this list cannot disagree with either. It leaves out names with
/// no definition here, tests and modules. A daemon that does not report the
/// reading is a gap, said as one: nothing here guesses at coverage.
pub(super) fn overview_untested(
    inventory: &Value,
    census: Option<&Census<'_>>,
) -> Result<Map<String, Value>> {
    let untested = inventory
        .get("untested")
        .filter(|value| value.is_object())
        .context(
            "this repository's daemon does not report it and predates this version of kin ui",
        )?;
    let rows = untested["entities"]
        .as_array()
        .context("the reading omitted its rows")?;
    let total = untested["total"]
        .as_u64()
        .context("the reading omitted its total")?;
    let items: Vec<Value> = rows
        .iter()
        .filter_map(|row| {
            let id = row["id"].as_str()?;
            let mut item = fields(entity_ref(
                id,
                row["name"].as_str().unwrap_or_default(),
                row["kind"].as_str().unwrap_or_default(),
            ));
            item.insert(
                "container".into(),
                census
                    .and_then(|census| census.container_of(id))
                    .unwrap_or(Value::Null),
            );
            item.insert("dependents".into(), row["dependents"].clone());
            Some(Value::Object(item))
        })
        .collect();
    Ok(fields(json!({
        "items": items,
        "total": total,
        "scope": "recorded_graph",
        "bound": "observed",
        "basis": "Entities defined here that others rely on and that no recorded test reaches through the supported semantic relationships. Tests and modules are left out. Read across the whole repository, most relied-on first. These are recorded graph gaps, not evidence that complete runtime test coverage is known.",
    })))
}

/// The untested section, or its gap in one sentence: what was not read and
/// why, with no list guessed in its place.
fn untested_section(answer: Result<Map<String, Value>>) -> Value {
    match answer {
        Ok(mut fields) => {
            fields.insert("ok".into(), Value::Bool(true));
            Value::Object(fields)
        }
        Err(error) => {
            let mut why = readable(&format!("{error:#}"));
            while why.ends_with(['.', '!', '?', ' ']) {
                why.pop();
            }
            json!({
                "ok": false,
                "reason": plain(&format!(
                    "Kin could not read which entities no test reaches: {why}."
                )),
            })
        }
    }
}

fn recent_changes(report: crate::commands::log::LogReport) -> Map<String, Value> {
    let items: Vec<Value> = report
        .entries
        .into_iter()
        .map(|entry| {
            json!({
                "kin_checkpoint": super::review_ui::is_kin_checkpoint(&entry),
                "id": entry.change_id.to_string(),
                "title": entry.message.lines().next().unwrap_or_default().trim(),
                "author": author_display(&entry.author.to_string()),
                "agent": super::workspace_agents::agent_ref(&entry.author.to_string()),
                "time": entry.timestamp.to_string(),
            })
        })
        .collect();
    fields(json!({ "shown": items.len(), "items": items, "truncated": report.truncated }))
}

async fn serve_overview(State(state): State<Arc<UiState>>) -> Response {
    let (client, health) = match connect(&state, "ui overview").await {
        Ok(connected) => connected,
        Err(error) => return daemon_unreachable(error),
    };
    let repo_id = health.repo_id.clone().unwrap_or_default();
    let summary_request = super::overview::OverviewRequest {
        json: true,
        compact: true,
    };
    let (export, summary, ranking, branches, log) = tokio::join!(
        // With each entity's language, which the container rows carry.
        bounded(
            GRAPH_TIMEOUT,
            client.graph_export("limit=0&include=language")
        ),
        bounded(READ_TIMEOUT, async {
            let response = client.overview(&summary_request).await?;
            let line = response
                .lines
                .first()
                .context("the overview answer was empty")?;
            serde_json::from_str::<Value>(line).context("the overview answer was not JSON")
        }),
        bounded(READ_TIMEOUT, async {
            anyhow::ensure!(
                !repo_id.is_empty(),
                "the daemon did not identify its repository"
            );
            // One ranking pass in the daemon answers both the most relied-on
            // list and what no test reaches, so the overview asks once.
            client
                .workspace_entities_and_untested(
                    &repo_id,
                    "",
                    OVERVIEW_RELIED_ON_READ,
                    0,
                    Some(OVERVIEW_UNTESTED),
                )
                .await
        }),
        bounded(READ_TIMEOUT, async {
            client
                .branch(&super::branch::BranchRequest::List)
                .await?
                .report
                .context("the branch list omitted its report")
        }),
        bounded(READ_TIMEOUT, async {
            client
                .log(&super::log::LogRequest {
                    count: OVERVIEW_CHANGES,
                    skip: 0,
                })
                .await?
                .report
                .context("the history answer omitted its report")
        }),
    );
    let census = export.as_ref().ok().map(Census::new);
    let bound = relied_on_bound(summary.as_ref().ok());
    let counts = match (&export, &summary) {
        (Ok(payload), _) => Ok(overview_counts(payload)),
        (Err(_), Ok(summary)) => overview_counts_from_summary(summary),
        (Err(error), Err(_)) => Err(anyhow::anyhow!("{error:#}")),
    }
    .map(|counts| with_bound(counts, "relationships_bound", bound));
    let containers = match (&census, &export) {
        (Some(census), _) => Ok(overview_containers(census, OVERVIEW_CONTAINERS)),
        (None, Err(error)) => Err(anyhow::anyhow!("{error:#}")),
        (None, Ok(_)) => Err(anyhow::anyhow!("the graph read was empty")),
    };
    let languages = summary.and_then(|summary| overview_languages(&summary));
    let (relied_on, untested) = match ranking {
        Ok(inventory) => (
            overview_relied_on(&inventory, census.as_ref(), OVERVIEW_RELIED_ON)
                .map(|relied_on| with_bound(relied_on, "bound", bound)),
            overview_untested(&inventory, census.as_ref()),
        ),
        Err(error) => {
            let reason = format!("{error:#}");
            (Err(error), Err(anyhow::anyhow!(reason)))
        }
    };
    let branch = branches.ok().and_then(|report| {
        report
            .branches
            .into_iter()
            .find(|branch| branch.active)
            .map(|branch| {
                let full = branch.name.to_string();
                full.strip_prefix("refs/heads/")
                    .unwrap_or(&full)
                    .to_string()
            })
    });
    Json(json!({
        "schema": "kin.workspace.overview.v1",
        "repository": super::workspace_ui::repository_identity(&state, json!({
            "branch": branch,
            "repository_id": health.repo_id, "workspace_id": health.workspace_id,
        })),
        "counts": section("Kin could not count this repository's entities.", counts),
        "languages": section("Kin could not read this repository's languages.", languages),
        "containers": section("Kin could not read what contains what.", containers),
        "most_relied_on": section("Kin could not rank the most relied-on entities.", relied_on),
        "untested": untested_section(untested),
        "recent_changes": section("Kin could not read recent changes.", log.map(recent_changes)),
        "read_at": read_at(),
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Ask
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AskQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    limit: Option<usize>,
}

/// The question and how many results it asks for, or why it is refused
/// before the daemon is asked anything.
fn ask_request(query: &AskQuery) -> std::result::Result<(String, usize), String> {
    let text = query.q.trim();
    if text.is_empty() {
        return Err("Ask needs a question or a name.".into());
    }
    validate_text(text, "A question")?;
    let limit = query
        .limit
        .unwrap_or(ASK_LIMIT_DEFAULT)
        .clamp(1, ASK_LIMIT_MAX);
    Ok((text.to_string(), limit))
}

/// How a result met the question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Meaning {
    Embedding,
    Words,
}

#[derive(Debug, Clone)]
pub(super) struct Hit {
    id: String,
    name: String,
    kind: String,
    signature: Option<String>,
    summary: Option<String>,
    defined_here: bool,
    by_name: bool,
    meaning: Option<Meaning>,
    /// How many distinct entities rely on it, from the importance ranking,
    /// when the name read carried it.
    dependents: Option<u64>,
}

impl Hit {
    fn why(&self) -> &'static str {
        match (self.by_name, self.meaning) {
            (true, Some(_)) => "Its name matches the question, and Kin also ranked it by meaning.",
            (true, None) => "Its name matches the question.",
            (false, Some(Meaning::Embedding)) => "Its embedding is close to the question.",
            (false, _) => "Its description and code share words with the question.",
        }
    }

    fn matched(&self) -> Vec<&'static str> {
        let mut matched = Vec::new();
        if self.by_name {
            matched.push("name");
        }
        if self.meaning.is_some() {
            matched.push("meaning");
        }
        matched
    }

    fn merge(&mut self, other: Hit) {
        self.by_name |= other.by_name;
        self.meaning = self.meaning.or(other.meaning);
        self.signature = self.signature.take().or(other.signature);
        self.summary = self.summary.take().or(other.summary);
        self.defined_here |= other.defined_here;
        self.dependents = self.dependents.or(other.dependents);
    }
}

/// Whether `name` is the name asked for, whole or as its last segment.
fn names_exactly(name: &str, query: &str) -> bool {
    let last = name.rsplit(['.', ':']).next().unwrap_or(name);
    name.eq_ignore_ascii_case(query) || last.eq_ignore_ascii_case(query)
}

fn meaning_hits(located: &crate::commands::locate::LocateResult) -> Vec<Hit> {
    use crate::commands::locate::{LocateIdSpace, LocateMatchKind};
    located
        .entities
        .iter()
        .filter(|row| row.id_space == LocateIdSpace::Entity && !row.entity_id.is_empty())
        .map(|row| Hit {
            id: row.entity_id.clone(),
            name: row.name.clone(),
            kind: kind_label(&row.kind),
            signature: shown_signature(&row.kind, Some(&row.signature)),
            summary: None,
            defined_here: row.provenance.file.is_some(),
            by_name: row.match_kind == Some(LocateMatchKind::Name),
            meaning: match row.match_kind {
                Some(LocateMatchKind::Name) => None,
                Some(LocateMatchKind::Semantic) => Some(Meaning::Embedding),
                _ => Some(Meaning::Words),
            },
            dependents: None,
        })
        .collect()
}

fn name_hits(inventory: &Value) -> Vec<Hit> {
    inventory["entities"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    Some(Hit {
                        id: row["id"].as_str()?.to_string(),
                        name: row["name"].as_str()?.to_string(),
                        kind: kind_label(row["kind"].as_str().unwrap_or_default()),
                        signature: shown_signature(
                            row["kind"].as_str().unwrap_or_default(),
                            row["signature"].as_str(),
                        ),
                        summary: text_field(row, "summary"),
                        defined_here: !row["file_path"].is_null(),
                        by_name: true,
                        meaning: None,
                        dependents: row["dependents"].as_u64(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One list from both searches: exact names first, then meaning in Kin's
/// ranking order, then the other name matches by how relied on they are.
/// Each entity appears once, carrying every way it matched.
pub(super) fn merge_hits(
    query: &str,
    meaning: Vec<Hit>,
    names: Vec<Hit>,
    limit: usize,
) -> Vec<Hit> {
    let mut merged: Vec<Hit> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    let mut place = |hit: Hit, merged: &mut Vec<Hit>| match at.get(&hit.id) {
        Some(index) => merged[*index].merge(hit),
        None => {
            at.insert(hit.id.clone(), merged.len());
            merged.push(hit);
        }
    };
    let (exact_meaning, rest_meaning): (Vec<Hit>, Vec<Hit>) = meaning
        .into_iter()
        .partition(|hit| hit.by_name && names_exactly(&hit.name, query));
    let (exact_names, rest_names): (Vec<Hit>, Vec<Hit>) = names
        .into_iter()
        .partition(|hit| names_exactly(&hit.name, query));
    for hit in exact_meaning
        .into_iter()
        .chain(exact_names)
        .chain(rest_meaning)
        .chain(rest_names)
    {
        place(hit, &mut merged);
    }
    merged.truncate(limit);
    merged
}

fn ask_notice(
    coverage: Option<&crate::commands::locate::SemanticCoverage>,
) -> (Option<String>, &'static str) {
    use crate::commands::locate::EmbeddingState;
    let Some(coverage) = coverage else {
        return (None, "descriptions");
    };
    match coverage.embedding_state {
        EmbeddingState::Present => (None, "embeddings"),
        EmbeddingState::Partial => (
            Some(format!(
                "Embeddings cover {} of {} entities so far, so some meaning matches rest on descriptions and code alone.",
                coverage.indexed, coverage.total
            )),
            "embeddings_partial",
        ),
        EmbeddingState::NotUsed => (
            Some("Meaning search ran without embeddings here, comparing the question with each entity's description and code.".to_string()),
            "descriptions",
        ),
        EmbeddingState::Absent => (
            Some("No embeddings are built for this repository yet, so meaning search compared the question with each entity's description and code.".to_string()),
            "descriptions",
        ),
        EmbeddingState::Unknown => (None, "descriptions"),
    }
}

/// Where each result sits, read from its own source answer. A read that
/// fails or is slow only leaves that result without a container.
async fn enrich(client: &DaemonClient, ids: Vec<String>) -> HashMap<String, Value> {
    let gate = Arc::new(tokio::sync::Semaphore::new(ENRICH_CONCURRENCY));
    let mut reads = tokio::task::JoinSet::new();
    for id in ids {
        let client = client.clone();
        let gate = gate.clone();
        reads.spawn(async move {
            let _permit = gate.acquire_owned().await.ok()?;
            let source = bounded(ENRICH_TIMEOUT, client.workspace_entity_source(&id))
                .await
                .ok()?;
            Some((id, source))
        });
    }
    let mut found = HashMap::new();
    while let Some(read) = reads.join_next().await {
        if let Ok(Some((id, source))) = read {
            found.insert(id, source);
        }
    }
    found
}

/// What contains each result's container, by the container's id, read from
/// the container's own answer. Two results can sit in containers of one name
/// and kind, and this is what tells those apart. A container that sits in
/// nothing, or whose read fails or is slow, has no entry.
async fn outer_containers(
    client: &DaemonClient,
    sources: &HashMap<String, Value>,
) -> HashMap<String, Value> {
    let containers: HashSet<String> = sources
        .values()
        .filter_map(|source| owner_ref(&source["semantic_owners"]))
        .filter_map(|container| parse_entity_id(container["id"].as_str()?))
        .collect();
    let gate = Arc::new(tokio::sync::Semaphore::new(ENRICH_CONCURRENCY));
    let mut reads = tokio::task::JoinSet::new();
    for id in containers {
        let client = client.clone();
        let gate = gate.clone();
        reads.spawn(async move {
            let _permit = gate.acquire_owned().await.ok()?;
            let answer = bounded(ENRICH_TIMEOUT, client.workspace_entity_owners(&id))
                .await
                .ok()?;
            Some((id, owner_ref(&answer["semantic_owners"])?))
        });
    }
    let mut found = HashMap::new();
    while let Some(read) = reads.join_next().await {
        if let Ok(Some((id, outer))) = read {
            found.insert(id, outer);
        }
    }
    found
}

fn hit_json(hit: &Hit, source: Option<&Value>, outer: &HashMap<String, Value>) -> Value {
    let mut row = fields(json!({
        "id": hit.id, "name": hit.name, "kind": hit.kind,
        "matched": hit.matched(), "why": hit.why(), "defined_here": hit.defined_here,
    }));
    if let Some(signature) = &hit.signature {
        row.insert("signature".into(), Value::String(signature.clone()));
    }
    if let Some(summary) = hit.summary.as_deref().map(plain_doc) {
        let sentence = first_sentence(&summary);
        if !sentence.is_empty() {
            row.insert("summary".into(), Value::String(sentence));
        }
    }
    if let Some(dependents) = hit.dependents {
        row.insert("dependents".into(), json!(dependents));
    }
    if let Some(source) = source {
        if let Some(mut container) = owner_ref(&source["semantic_owners"]) {
            let within = container["id"].as_str().and_then(|id| outer.get(id));
            if let Some(within) = within {
                container["within"] = within.clone();
            }
            row.insert("container".into(), container);
        }
        if let Some(module) = text_field(source, "module") {
            row.insert("module".into(), Value::String(module));
        }
    }
    Value::Object(row)
}

// ---------------------------------------------------------------------------
// Ask: the chain behind a how or where question
// ---------------------------------------------------------------------------
//
// A question that asks how or where something happens is answered with a
// short chain of entities when the graph holds one Kin can stand behind, and
// with the ranked list alone when it does not. Every hop of a chain is a call
// the graph resolved, read from the graph's own record of what calls each
// entity, and the chain as a whole is confirmed by the route query behind
// `kin path`. Which chain is the question's is decided by the graph's own
// names, under one of two rules:
//
// - named ends: the chain ends at a ranked result whose name carries some of
//   the question's words and begins at the nearest caller whose name carries
//   the rest, so both ends are named by the question;
// - sole callers: the chain ends at a ranked result whose name carries every
//   word of the question, and each entity on it is called by the one above
//   it and by nothing else, so no choice between callers was made.
//
// Anything else would be a guess, and a guess is not returned.

/// The ranked results one answer climbs from, best first.
const ANSWER_SEEDS: usize = 4;
/// How long the reads behind an answer may take together. Past it the
/// question is answered with its list alone.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(3);
const ANSWER_MIN_STEPS: usize = 3;
const ANSWER_MAX_STEPS: usize = 8;
/// How many calls above a ranked result its callers are read.
const ANSWER_HOPS: usize = 6;
/// The entities whose callers are read above one ranked result.
const ANSWER_READS: usize = 40;
/// The fewest words of a question that must name something before a chain is
/// tied to it. One word names a topic, not a question.
const ANSWER_MIN_TERMS: usize = 2;

/// Words that ask or connect and name nothing in a repository.
const ANSWER_STOP_WORDS: &[&str] = &[
    "a",
    "about",
    "actually",
    "all",
    "an",
    "and",
    "any",
    "are",
    "as",
    "at",
    "be",
    "been",
    "being",
    "by",
    "can",
    "code",
    "codebase",
    "come",
    "comes",
    "could",
    "did",
    "do",
    "does",
    "done",
    "each",
    "end",
    "ends",
    "every",
    "exactly",
    "explain",
    "flow",
    "flows",
    "for",
    "from",
    "get",
    "gets",
    "getting",
    "go",
    "goes",
    "got",
    "had",
    "happen",
    "happening",
    "happens",
    "has",
    "have",
    "here",
    "how",
    "i",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "kin",
    "me",
    "my",
    "not",
    "now",
    "of",
    "on",
    "one",
    "or",
    "our",
    "out",
    "over",
    "pass",
    "passes",
    "place",
    "please",
    "reach",
    "reaches",
    "repo",
    "repository",
    "should",
    "show",
    "so",
    "something",
    "tell",
    "that",
    "the",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "thing",
    "things",
    "this",
    "those",
    "to",
    "up",
    "us",
    "was",
    "way",
    "we",
    "were",
    "what",
    "when",
    "where",
    "which",
    "who",
    "why",
    "will",
    "with",
    "work",
    "working",
    "works",
    "would",
    "you",
    "your",
];

/// What a question asks for, when it asks how or where something happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asks {
    How,
    Where,
}

impl Asks {
    fn word(self) -> &'static str {
        match self {
            Self::How => "how",
            Self::Where => "where",
        }
    }
}

/// The words of a question or of one of the graph's names, lowercased:
/// split wherever a letter or digit run ends and wherever a capital begins a
/// new word, so `Reply.serializeInput` is `reply`, `serialize` and `input`.
fn words_of(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    for part in text.split(|character: char| !character.is_alphanumeric()) {
        let letters: Vec<char> = part.chars().collect();
        let mut start = 0;
        for index in 1..=letters.len() {
            let ends = index == letters.len() || {
                let (before, here) = (letters[index - 1], letters[index]);
                (before.is_lowercase() && here.is_uppercase())
                    || before.is_alphabetic() != here.is_alphabetic()
                    || (before.is_uppercase()
                        && here.is_uppercase()
                        && letters
                            .get(index + 1)
                            .is_some_and(|next| next.is_lowercase()))
            };
            if ends {
                words.push(
                    letters[start..index]
                        .iter()
                        .collect::<String>()
                        .to_lowercase(),
                );
                start = index;
            }
        }
    }
    words
}

/// Whether a question asks how or where something happens. The asking word
/// leads, after at most two words such as "show me". "How many" and its kind
/// ask for an amount, which no chain answers.
fn asks(question: &str) -> Option<Asks> {
    let words = words_of(question);
    for (index, word) in words.iter().enumerate().take(3) {
        match word.as_str() {
            "where" => return Some(Asks::Where),
            "how" => {
                let amount = matches!(
                    words.get(index + 1).map(String::as_str),
                    Some(
                        "many"
                            | "much"
                            | "long"
                            | "often"
                            | "big"
                            | "large"
                            | "small"
                            | "old"
                            | "far"
                            | "fast"
                            | "slow"
                    )
                );
                return (!amount).then_some(Asks::How);
            }
            _ => {}
        }
    }
    None
}

/// One word reduced so its forms meet: `serialized`, `serializer` and
/// `serialization` all become `serializ`, `routes` and `routing` become
/// `rout`. It only has to agree with itself, since both the question and the
/// graph's names pass through it.
fn stem(word: &str) -> String {
    let mut text = match word {
        "sent" => "send",
        "built" => "build",
        "ran" => "run",
        "made" => "make",
        "wrote" | "written" => "write",
        "threw" | "thrown" => "throw",
        "caught" => "catch",
        "found" => "find",
        "kept" => "keep",
        "held" => "hold",
        "chose" | "chosen" => "choose",
        other => other,
    }
    .to_string();
    let replaced = [
        ("ization", "ize"),
        ("isation", "ize"),
        ("izing", "ize"),
        ("ized", "ize"),
        ("izer", "ize"),
        ("izes", "ize"),
        ("ies", "y"),
        ("sses", "ss"),
    ]
    .into_iter()
    .find(|(suffix, _)| text.ends_with(suffix) && text.len() > suffix.len() + 2);
    if let Some((suffix, with)) = replaced {
        text.truncate(text.len() - suffix.len());
        text.push_str(with);
    } else if text.ends_with('s') && !text.ends_with("ss") && text.len() > 3 {
        text.pop();
    }
    for (suffix, longer_than) in [("ing", 6), ("ed", 4), ("er", 5), ("or", 5)] {
        if text.ends_with(suffix) && text.len() > longer_than {
            text.truncate(text.len() - suffix.len());
            // `formatted` and `running` double a consonant before the suffix.
            let mut letters = text.chars().rev();
            if let (Some(last), Some(before)) = (letters.next(), letters.next()) {
                if text.len() > 3 && last == before && !"aeiouls".contains(last) {
                    text.pop();
                }
            }
            break;
        }
    }
    if text.ends_with('e') && text.len() > 4 {
        text.pop();
    }
    if text.ends_with('y') && text.len() > 3 {
        text.pop();
        text.push('i');
    }
    text
}

/// The words of a question that could name something in a repository,
/// reduced, in the order asked and once each.
fn question_terms(question: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in words_of(question) {
        if word.chars().count() < 2 || ANSWER_STOP_WORDS.contains(&word.as_str()) {
            continue;
        }
        let term = stem(&word);
        if !terms.contains(&term) {
            terms.push(term);
        }
    }
    terms
}

/// Whether a word of a name is a word of the question: the same once
/// reduced, or one beginning with the other when that is at least four
/// letters, which is how `regist` meets `register`.
fn term_matches(term: &str, word: &str) -> bool {
    let (shorter, longer) = if term.len() <= word.len() {
        (term, word)
    } else {
        (word, term)
    };
    shorter == longer || (shorter.chars().count() >= 4 && longer.starts_with(shorter))
}

/// Which of the question's words a name carries, by their place in `terms`.
fn matched_terms(name: &str, terms: &[String]) -> HashSet<usize> {
    let words: Vec<String> = words_of(name).iter().map(|word| stem(word)).collect();
    terms
        .iter()
        .enumerate()
        .filter(|(_, term)| words.iter().any(|word| term_matches(term, word)))
        .map(|(index, _)| index)
        .collect()
}

fn is_callable(kind: &str) -> bool {
    matches!(kind, "function" | "method")
}

/// One entity of an answer's chain.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Link {
    id: String,
    name: String,
    kind: String,
}

/// What the graph records as calling one entity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Callers {
    /// The functions and methods that call it by a call the graph resolved,
    /// once each and by name, so one question always reads one order.
    proven: Vec<Link>,
    /// How many distinct entities call it at all, resolved or not.
    all: usize,
    /// Whether the read was cut short, so there may be more.
    cut: bool,
}

impl Callers {
    /// The one entity that calls it, when exactly one does and that call is
    /// resolved.
    fn only(&self) -> Option<&Link> {
        (!self.cut && self.all == 1 && self.proven.len() == 1).then(|| &self.proven[0])
    }
}

/// What calls `id`, from the graph's own incoming relations for it.
fn callers_of(incoming: &Value, id: &str) -> Callers {
    let entities: HashMap<&str, &Value> = incoming["entities"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| Some((row["id"].as_str()?, row)))
        .collect();
    let mut all: HashSet<&str> = HashSet::new();
    let mut proven: Vec<Link> = Vec::new();
    for relation in incoming["relations"].as_array().into_iter().flatten() {
        let calls = relation["direction"] == "incoming"
            && kind_label(relation["kind"].as_str().unwrap_or_default()) == "calls";
        let Some(caller) = relation["src"]["Entity"].as_str().filter(|_| calls) else {
            continue;
        };
        if caller == id {
            continue;
        }
        all.insert(caller);
        let Some(row) = entities.get(caller) else {
            continue;
        };
        let kind = kind_label(row["kind"].as_str().unwrap_or_default());
        let named = (
            parse_entity_id(caller),
            text_field(row, "name"),
            relation["resolution"] == "type_resolved" && is_callable(&kind),
        );
        if let (Some(id), Some(name), true) = named {
            if proven.iter().all(|seen| seen.id != id) {
                proven.push(Link { id, name, kind });
            }
        }
    }
    proven.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
    Callers {
        proven,
        all: all.len(),
        cut: incoming["truncated"] != false,
    }
}

/// What was read above one ranked result: who calls it, who calls those, and
/// so on, by resolved calls only.
#[derive(Debug, Default)]
struct Climb {
    /// Each caller found, nearest first, with the entity it calls on its
    /// shortest way down to the result.
    above: Vec<(Link, String)>,
    /// What calls each entity whose callers were read, by its id.
    callers: HashMap<String, Callers>,
}

/// Read who calls `target`, and who calls those, level by level. Each read
/// is one entity's incoming relations. It stops [`ANSWER_HOPS`] calls up or
/// after [`ANSWER_READS`] entities, and a read that fails or is slow only
/// leaves that entity's callers unknown.
async fn climb(client: &DaemonClient, target: &Link) -> Climb {
    let mut climb = Climb::default();
    let mut seen: HashSet<String> = HashSet::from([target.id.clone()]);
    let mut level = vec![target.id.clone()];
    let mut reads = 0;
    for _ in 0..ANSWER_HOPS {
        level.truncate(ANSWER_READS - reads);
        if level.is_empty() {
            break;
        }
        reads += level.len();
        let answers = futures_util::future::join_all(level.iter().map(|id| async move {
            bounded(
                ENRICH_TIMEOUT,
                client.workspace_entity_neighborhood(id, "in", NEIGHBORHOOD_LIMIT),
            )
            .await
            .ok()
        }))
        .await;
        let mut next = Vec::new();
        for (id, answer) in level.iter().zip(answers) {
            let Some(answer) = answer else {
                continue;
            };
            let callers = callers_of(&answer, id);
            for caller in &callers.proven {
                if seen.insert(caller.id.clone()) {
                    climb.above.push((caller.clone(), id.clone()));
                    next.push(caller.id.clone());
                }
            }
            climb.callers.insert(id.clone(), callers);
        }
        level = next;
    }
    climb
}

/// Whether a name reads as test code. The graph's fast reads do not say
/// which entities are tests, and a test calls into the code it tests, so it
/// would otherwise stand as the place a chain begins.
fn names_a_test(name: &str) -> bool {
    words_of(name).iter().any(|word| {
        matches!(
            word.as_str(),
            "test" | "tests" | "spec" | "specs" | "fixture" | "fixtures" | "mock" | "mocks"
        )
    })
}

/// The shortest chain of resolved calls that ends at `target` and begins at
/// a caller whose name carries a word of the question `target`'s own name
/// does not, with every word of the question carried by some name on it.
fn named_ends_chain(
    target: &Link,
    climb: &Climb,
    terms: &[String],
    tests_asked: bool,
) -> Option<Vec<Link>> {
    let down: HashMap<&str, (&Link, &str)> = climb
        .above
        .iter()
        .map(|(link, next)| (link.id.as_str(), (link, next.as_str())))
        .collect();
    let target_terms = matched_terms(&target.name, terms);
    // Nearest first, so the first chain that fits is the shortest.
    climb.above.iter().find_map(|(entry, _)| {
        let mut chain = Vec::new();
        let mut at = entry.id.as_str();
        while at != target.id {
            let &(link, next) = down.get(at)?;
            if chain.len() >= ANSWER_MAX_STEPS {
                return None;
            }
            chain.push(link.clone());
            at = next;
        }
        chain.push(target.clone());
        let named: HashSet<usize> = chain
            .iter()
            .flat_map(|link| matched_terms(&link.name, terms))
            .collect();
        let entry_adds = matched_terms(&entry.name, terms)
            .iter()
            .any(|term| !target_terms.contains(term));
        let fits = (ANSWER_MIN_STEPS..=ANSWER_MAX_STEPS).contains(&chain.len())
            && named.len() == terms.len()
            && entry_adds
            && (tests_asked || !chain.iter().any(|link| names_a_test(&link.name)));
        fits.then_some(chain)
    })
}

/// The chain above `target` while each entity has exactly one caller and
/// that call is resolved. It stops where an entity has several callers,
/// none, or callers that were not read whole.
fn sole_caller_chain(target: &Link, climb: &Climb, tests_asked: bool) -> Option<Vec<Link>> {
    let mut upward = vec![target.clone()];
    while upward.len() < ANSWER_MAX_STEPS {
        let below = &upward[upward.len() - 1];
        let Some(only) = climb.callers.get(&below.id).and_then(Callers::only) else {
            break;
        };
        if upward.iter().any(|seen| seen.id == only.id)
            || (!tests_asked && names_a_test(&only.name))
        {
            break;
        }
        upward.push(only.clone());
    }
    upward.reverse();
    (upward.len() >= ANSWER_MIN_STEPS).then_some(upward)
}

/// `a`, `a and b`, `a, b and c`.
fn listed(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [only] => (*only).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// One sentence that says a chain, in the graph's own names and nothing else.
fn answer_sentence(asks: Asks, chain: &[Link]) -> String {
    let names: Vec<&str> = chain.iter().map(|link| link.name.as_str()).collect();
    let (Some(entry), Some(target)) = (names.first(), names.last()) else {
        return String::new();
    };
    let between = listed(&names[1..names.len() - 1]);
    match asks {
        Asks::Where => {
            format!("This happens in {target}, which {entry} reaches through {between}.")
        }
        Asks::How if names.len() <= 4 => {
            let mut sentence = format!("{entry} calls {}", names[1]);
            for name in &names[2..] {
                sentence.push_str(&format!(", which calls {name}"));
            }
            sentence.push('.');
            sentence
        }
        Asks::How => {
            format!("{entry} reaches {target} through {between}, each calling the next.")
        }
    }
}

fn answer_json(asks: Asks, basis: &str, chain: &[Link]) -> Value {
    let steps: Vec<Value> = chain
        .iter()
        .enumerate()
        .map(|(index, link)| {
            json!({
                "id": link.id,
                "name": link.name,
                "kind": link.kind,
                "relation_to_next": (index + 1 < chain.len()).then_some("calls"),
            })
        })
        .collect();
    json!({
        "asks": asks.word(),
        "sentence": plain(&answer_sentence(asks, chain)),
        "basis": basis,
        "steps": steps,
    })
}

/// The chain the calls above one ranked result give the question, and the
/// rule it stands on.
fn chain_from(
    target: &Link,
    climb: &Climb,
    terms: &[String],
    tests_asked: bool,
) -> Option<(&'static str, Vec<Link>)> {
    if matched_terms(&target.name, terms).len() < terms.len() {
        let chain = named_ends_chain(target, climb, terms, tests_asked)?;
        Some(("named_ends", chain))
    } else {
        Some((
            "sole_callers",
            sole_caller_chain(target, climb, tests_asked)?,
        ))
    }
}

/// The ranked results an answer may end at: functions and methods defined
/// here whose names carry a word of the question, best first.
fn answer_seeds(hits: &[Hit], terms: &[String]) -> Vec<Link> {
    hits.iter()
        .filter(|hit| {
            hit.defined_here
                && is_callable(&hit.kind)
                && !matched_terms(&hit.name, terms).is_empty()
        })
        .take(ANSWER_SEEDS)
        .map(|hit| Link {
            id: hit.id.clone(),
            name: hit.name.clone(),
            kind: hit.kind.clone(),
        })
        .collect()
}

/// Whether the route query behind `kin path` finds `chain`'s first entity
/// reaching its last within the chain's own length.
async fn route_confirmed(client: &DaemonClient, chain: &[Link]) -> bool {
    let (Some(entry), Some(target)) = (chain.first(), chain.last()) else {
        return false;
    };
    let request = kin_mcp::handlers::path::PathRequest {
        from: entry.id.clone(),
        to: target.id.clone(),
        max_depth: Some(chain.len() - 1),
        limit: Some(1),
        direction: Some(kin_mcp::handlers::path::PathDirection::Forward),
        ..Default::default()
    };
    client
        .path(&request)
        .await
        .is_ok_and(|route| route["found"] == true)
}

/// The answer to a how or where question, or nothing when the graph holds
/// no chain Kin can stand behind. The best-ranked result with a chain wins.
async fn answer(client: &DaemonClient, question: &str, hits: &[Hit]) -> Option<Value> {
    let asks = asks(question)?;
    let terms = question_terms(question);
    if terms.len() < ANSWER_MIN_TERMS {
        return None;
    }
    let tests_asked = names_a_test(question);
    let read = async {
        let seeds = answer_seeds(hits, &terms);
        let climbs =
            futures_util::future::join_all(seeds.iter().map(|seed| climb(client, seed))).await;
        for (target, climb) in seeds.iter().zip(&climbs) {
            let Some((basis, chain)) = chain_from(target, climb, &terms, tests_asked) else {
                continue;
            };
            if route_confirmed(client, &chain).await {
                return Some(answer_json(asks, basis, &chain));
            }
        }
        None
    };
    tokio::time::timeout(ANSWER_TIMEOUT, read)
        .await
        .ok()
        .flatten()
}

async fn serve_ask(State(state): State<Arc<UiState>>, Query(query): Query<AskQuery>) -> Response {
    let (text, limit) = match ask_request(&query) {
        Ok(request) => request,
        Err(reason) => return bad_request(&reason),
    };
    let (client, health) = match connect(&state, "ui ask").await {
        Ok(connected) => connected,
        Err(error) => return daemon_unreachable(error),
    };
    let repo_id = health.repo_id.clone().unwrap_or_default();
    let locate_request = crate::daemon_client::LocateRequest {
        text: text.clone(),
        queries: None,
        explain: None,
        max_files: 10,
        max_files_explicit: false,
        reference: None,
        snippets: false,
        snippet_lines: None,
        entity_surface: true,
        cursor: None,
        page_size: Some(limit),
        snippet_override: None,
        include_tests: None,
    };
    let (located, named) = tokio::join!(
        bounded(READ_TIMEOUT, client.locate(&locate_request)),
        bounded(READ_TIMEOUT, async {
            anyhow::ensure!(
                !repo_id.is_empty(),
                "the daemon did not identify its repository"
            );
            client.workspace_entities(&repo_id, &text, limit, 0).await
        }),
    );
    let (notice, basis) = match &located {
        Ok(located) => ask_notice(located.semantic_coverage.as_ref()),
        Err(_) => (None, "descriptions"),
    };
    let mode = match (&located, &named) {
        (Ok(_), Ok(_)) => "meaning_and_name",
        (Err(_), Ok(_)) => "name_only",
        (Ok(_), Err(_)) => "meaning_only",
        (Err(_), Err(_)) => "unavailable",
    };
    let notice = match mode {
        "name_only" => {
            Some("Meaning search could not answer, so these are name matches only.".to_string())
        }
        "meaning_only" => Some(match notice {
            Some(notice) => {
                format!("Name search could not answer, so these are meaning matches only. {notice}")
            }
            None => "Name search could not answer, so these are meaning matches only.".to_string(),
        }),
        "unavailable" => Some("Neither meaning search nor name search could answer.".to_string()),
        _ => notice,
    };
    let hits = merge_hits(
        &text,
        located.as_ref().map(meaning_hits).unwrap_or_default(),
        named.as_ref().map(name_hits).unwrap_or_default(),
        limit,
    );
    // The chain behind a how or where question is read while the results
    // are placed, so it adds no wait of its own unless its reads are slower.
    let ((sources, outer), answer) = tokio::join!(
        async {
            let sources = enrich(&client, hits.iter().map(|hit| hit.id.clone()).collect()).await;
            let outer = outer_containers(&client, &sources).await;
            (sources, outer)
        },
        answer(&client, &text, &hits),
    );
    let results: Vec<Value> = hits
        .iter()
        .map(|hit| hit_json(hit, sources.get(&hit.id), &outer))
        .collect();
    let meaning = located.map(|located| {
        fields(json!({
            "ranked": located.total_ranked.max(located.entities.len()),
            "basis": basis,
        }))
    });
    let names = named.map(|inventory| fields(json!({ "total": inventory["total"] })));
    Json(json!({
        "schema": "kin.workspace.ask.v1",
        "query": text,
        "mode": mode,
        "notice": notice.map(|notice| plain(&notice)),
        "meaning": section("Meaning search could not answer.", meaning),
        "names": section("Name search could not answer.", names),
        "answer": answer,
        "total": results.len(),
        "results": results,
        "read_at": read_at(),
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// One entity
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EntityQuery {
    id: String,
    /// Comma-separated section names, to read only those. Every section
    /// when absent.
    #[serde(default)]
    sections: Option<String>,
}

pub(super) const ENTITY_SECTIONS: [&str; 9] = [
    "header",
    "source",
    "relied_on_by",
    "relies_on",
    "members",
    "tests",
    "impact",
    "history",
    "editor",
];

/// The sections a request asks for, or why it is refused.
fn entity_sections(raw: Option<&str>) -> std::result::Result<Vec<&'static str>, String> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(ENTITY_SECTIONS.to_vec());
    };
    let mut asked = Vec::new();
    for name in raw
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let Some(known) = ENTITY_SECTIONS.iter().find(|known| **known == name) else {
            return Err(format!(
                "Unknown section '{name}'. Sections are {}.",
                ENTITY_SECTIONS.join(", ")
            ));
        };
        if !asked.contains(known) {
            asked.push(*known);
        }
    }
    Ok(asked)
}

/// The first sentence of a doc summary, for a header line. The summary as
/// recorded stays in `doc`.
/// A docstring as plain words: a Sphinx role such as ``:class:`Signer` `` or
/// ``:doc:`/concepts` `` reads as its text, and literal markup as the literal.
pub(super) fn plain_doc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(['`', ':']) {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        // A role, `:name:` directly followed by a backquoted target.
        if let Some(role) = tail.strip_prefix(':') {
            let name_len = role
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == ':'))
                .unwrap_or(role.len());
            let name = &role[..name_len];
            if name.len() > 1 && name.ends_with(':') && role[name_len..].starts_with('`') {
                rest = &role[name_len..];
                continue;
            }
            out.push(':');
            rest = role;
            continue;
        }
        let ticks = tail.len() - tail.trim_start_matches('`').len();
        let inner = &tail[ticks..];
        match inner.find(&"`".repeat(ticks)) {
            Some(end) if end > 0 => {
                let literal = inner[..end].trim_start_matches('~');
                // A cross-reference names its target last: `text <target>`.
                let literal = match (literal.find(" <"), literal.ends_with('>')) {
                    (Some(open), true) => &literal[..open],
                    _ => literal,
                };
                out.push_str(literal.trim_start_matches('/'));
                rest = &inner[end + ticks..];
            }
            _ => {
                out.push_str(&tail[..ticks]);
                rest = inner;
            }
        }
    }
    out.push_str(rest);
    out
}

pub(super) fn first_sentence(text: &str) -> String {
    const MAX_CHARS: usize = 280;
    let end = text.find(". ").map_or(text.len(), |index| index + 1);
    let sentence = text[..end].trim();
    if sentence.chars().count() <= MAX_CHARS {
        return sentence.to_string();
    }
    let cut: String = sentence.chars().take(MAX_CHARS).collect();
    match cut.rfind(' ') {
        Some(space) => format!("{}\u{2026}", cut[..space].trim_end()),
        None => format!("{cut}\u{2026}"),
    }
}

pub(super) fn header_section(
    source: Option<&Value>,
    recorded: Option<&Value>,
    summary: Option<String>,
) -> Result<Map<String, Value>> {
    let mut header = Map::new();
    let from = source
        .or(recorded)
        .context("neither its source nor its record could be read")?;
    header.insert(
        "name".into(),
        Value::String(text_field(from, "name").context("the answer named no entity")?),
    );
    header.insert(
        "kind".into(),
        Value::String(kind_label(from["kind"].as_str().unwrap_or_default())),
    );
    if let Some(source) = source {
        if let Some(language) = text_field(source, "language") {
            header.insert("language".into(), Value::String(language.to_lowercase()));
        }
        if let Some(module) = text_field(source, "module") {
            header.insert("module".into(), Value::String(module));
        }
        let kind = source["kind"].as_str().unwrap_or_default();
        if let Some(signature) = shown_signature(kind, source["signature"].as_str()) {
            header.insert("signature".into(), Value::String(signature));
        }
        if let Some(container) = owner_ref(&source["semantic_owners"]) {
            header.insert("container".into(), container);
        }
    }
    if let Some(role) = recorded.and_then(|recorded| text_field(recorded, "role")) {
        header.insert("role".into(), Value::String(role));
    }
    if let Some(doc) = summary {
        header.insert("summary".into(), Value::String(first_sentence(&doc)));
        header.insert("doc".into(), Value::String(doc));
    }
    Ok(header)
}

/// The entity's own text for the working tree. Prefers the guide's read,
/// which carries the start line; falls back to the plain source read.
///
/// The guide's exact read also carries `body_hash`, the digest of the span it
/// cut, the same one a guide step reports as `after.body_hash`. It is passed
/// on so the page can tell whether something said about the entity at a
/// recorded change still describes it as it is now. The plain source read is
/// not an exact span, so it carries no digest.
pub(super) fn source_section(
    guide_source: Option<&Value>,
    source: Option<&Value>,
) -> Result<Map<String, Value>> {
    let language = source
        .and_then(|source| text_field(source, "language"))
        .map(|language| language.to_lowercase());
    if let Some(guide_source) = guide_source {
        let body = &guide_source["body"];
        if body["state"].as_str() == Some("recorded") {
            if let Some(text) = body["text"].as_str() {
                let start = body["start_line"].as_u64();
                let lines = text.lines().count().max(1) as u64;
                let mut section = fields(json!({
                    "text": text,
                    "start_line": start,
                    "end_line": start.map(|start| start + lines - 1),
                    "language": language,
                    "working": body["working"].as_bool().unwrap_or(false),
                }));
                if let Some(digest) = body["body_hash"]
                    .as_str()
                    .filter(|digest| is_digest(digest))
                {
                    section.insert("body_hash".into(), Value::String(digest.to_string()));
                }
                return Ok(section);
            }
        }
        if source.and_then(|source| source["body"].as_str()).is_none() {
            let reason = text_field(body, "reason")
                .unwrap_or_else(|| "Kin holds no text of its own for this entity.".to_string());
            anyhow::bail!("{reason}");
        }
    }
    let source = source.context("its text could not be read")?;
    let text = source["body"]
        .as_str()
        .context("Kin holds no text of its own for this entity.")?;
    Ok(fields(json!({
        "text": text, "start_line": Value::Null, "end_line": Value::Null,
        "language": language, "working": true,
    })))
}

/// Whether `text` is a whole content digest: 64 hex digits.
fn is_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn coverage_bound(answer: &Value) -> (&'static str, Option<&'static str>) {
    let exact = answer["_kin"]["completeness"]["bound"].as_str() == Some("exact");
    if exact {
        return ("exact", None);
    }
    let unproven = answer["call_sites"]["callers_unproven_no_resolver"]
        .as_u64()
        .unwrap_or(0);
    (
        "at_least",
        Some(if unproven > 0 {
            UNPROVEN_SITES
        } else {
            INCOMPLETE_COVERAGE
        }),
    )
}

/// One entity that reaches the focal, as the reads below describe it.
#[derive(Default)]
struct Relier {
    name: String,
    kind: String,
    relationships: Vec<String>,
    sites: Option<u64>,
    module: Option<String>,
    container: Option<Value>,
    is_test: Option<bool>,
    confirmed: bool,
    /// Kin's reference read proved it refers to the focal entity.
    referenced: bool,
}

/// The connections that make one entity rely on another, the same set the
/// daemon's importance ranking counts as dependents: a call, an import or a
/// reference. A subclass or an override builds on the entity and is listed
/// apart, so every count of what relies on an entity is one number.
const RELIANCE_KINDS: [&str; 3] = ["calls", "imports", "references"];

impl Relier {
    /// Whether it relies on the focal entity in the sense every count uses.
    fn relies(&self) -> bool {
        self.confirmed
            && (self.referenced
                || self
                    .relationships
                    .iter()
                    .any(|kind| RELIANCE_KINDS.contains(&kind.as_str())))
    }
}

impl Relier {
    fn primary_relationship(&self) -> Option<&String> {
        self.relationships.iter().min_by(|left, right| {
            relationship_precedence(left)
                .cmp(&relationship_precedence(right))
                .then_with(|| left.cmp(right))
        })
    }

    fn relate(&mut self, kind: String) {
        if !kind.is_empty() && !self.relationships.contains(&kind) {
            self.relationships.push(kind);
        }
    }

    fn describe(&mut self, row: &Value) {
        if self.name.is_empty() {
            self.name = row["name"].as_str().unwrap_or_default().to_string();
            self.kind = kind_label(row["kind"].as_str().unwrap_or_default());
        }
        if let Some(kinds) = row["relation_kinds"].as_array() {
            for kind in kinds.iter().filter_map(Value::as_str) {
                self.relate(kind_label(kind));
            }
        }
        if self.sites.is_none() {
            self.sites = row["site_count"].as_u64();
        }
        if self.module.is_none() {
            self.module = text_field(row, "module");
        }
        if self.container.is_none() {
            self.container = owner_ref(&row["semantic_owners"]);
        }
        if let Some(role) = row["role"].as_str() {
            self.is_test = Some(role == "test");
        }
    }
}

fn relationship_precedence(kind: &str) -> u8 {
    let mut variant = String::new();
    for part in kind.split('_') {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            variant.extend(first.to_uppercase());
            variant.extend(chars);
        }
    }
    serde_json::from_value::<kin_model::RelationKind>(Value::String(variant))
        .map(super::review_guide::connection_precedence)
        .unwrap_or(u8::MAX)
}

/// What reaches the entity, one row per entity: every relationship the graph
/// records into it (`contains` excluded), the references Kin proved, with
/// their sites, and the matches by name alone it could not prove, marked
/// `confirmed: false`. Any one of the reads is enough to answer.
pub(super) fn relied_on_by_section(
    id: &str,
    incoming: Option<&Value>,
    references: Option<&Value>,
    tests: &HashMap<String, bool>,
) -> Result<Map<String, Value>> {
    anyhow::ensure!(
        incoming.is_some() || references.is_some(),
        "neither its relationships nor its references could be read"
    );
    let mut reliers: BTreeMap<String, Relier> = BTreeMap::new();
    if let Some(incoming) = incoming {
        let entities: HashMap<&str, &Value> = incoming["entities"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| Some((row["id"].as_str()?, row)))
                    .collect()
            })
            .unwrap_or_default();
        for relation in incoming["relations"].as_array().into_iter().flatten() {
            if relation["direction"].as_str() != Some("incoming") {
                continue;
            }
            let kind = kind_label(relation["kind"].as_str().unwrap_or_default());
            let Some(other) = relation["src"]["Entity"].as_str() else {
                continue;
            };
            if kind == "contains" || other == id {
                continue;
            }
            let Some(row) = entities.get(other) else {
                continue;
            };
            let relier = reliers.entry(other.to_string()).or_default();
            relier.describe(row);
            relier.relate(kind);
            relier.confirmed |= relation["resolution"].as_str() != Some("name_only");
        }
    }
    if let Some(references) = references {
        for (list, confirmed) in [("references", true), ("candidates", false)] {
            for row in references[list].as_array().into_iter().flatten() {
                let Some(other) = row["entity_id"].as_str() else {
                    continue;
                };
                let relier = reliers.entry(other.to_string()).or_default();
                relier.describe(row);
                relier.confirmed |= confirmed;
                relier.referenced |= confirmed;
            }
        }
    }
    let total = reliers.len();
    let confirmed = reliers.values().filter(|relier| relier.confirmed).count();
    let items: Vec<Value> = reliers
        .into_iter()
        .map(|(other, relier)| {
            let is_test = relier.is_test.or_else(|| tests.get(&other).copied());
            let relies = relier.relies();
            let mut item = fields(json!({
                "id": other, "name": relier.name, "kind": relier.kind,
                "relationship": relier.primary_relationship(),
                "relationships": relier.relationships,
                "is_test": is_test, "confirmed": relier.confirmed, "sites": relier.sites,
                "relies": relies,
            }));
            if let Some(module) = relier.module {
                item.insert("module".into(), Value::String(module));
            }
            if let Some(container) = relier.container {
                item.insert("container".into(), container);
            }
            Value::Object(item)
        })
        .collect();
    let mut items = fold_same_name(items);
    // Counted before the list is capped: every entity that relies on it in
    // the one sense the whole page counts, each declaration of an overload
    // on its own.
    let relies_total: u64 = items
        .iter()
        .filter(|item| item["relies"].as_bool() == Some(true))
        .map(|item| item["declarations"].as_u64().unwrap_or(1))
        .sum();
    items.sort_by(|a, b| {
        (a["is_test"].as_bool() == Some(true))
            .cmp(&(b["is_test"].as_bool() == Some(true)))
            .then_with(|| b["confirmed"].as_bool().cmp(&a["confirmed"].as_bool()))
            .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });
    items.truncate(ENTITY_LIST_CAP);
    let shown: usize = items
        .iter()
        .map(|item| item["declarations"].as_u64().unwrap_or(1) as usize)
        .sum();
    let truncated = incoming.is_some_and(|incoming| incoming["truncated"].as_bool() == Some(true));
    let (bound, coverage) = match references {
        Some(references) => coverage_bound(references),
        None => ("at_least", Some(INCOMPLETE_COVERAGE)),
    };
    let bound = if truncated || incoming.is_none() {
        "at_least"
    } else {
        bound
    };
    let mut said: Vec<String> = Vec::new();
    if incoming.is_none() {
        said.push(
            "Kin could not read its other relationships, such as subclasses and overrides."
                .to_string(),
        );
    }
    if references.is_none() {
        // Its rows still came from its relationships, matches by name alone
        // among them, so the sentence says only what the reference read adds.
        said.push(
            "Kin could not read its references, so where each one uses it is not shown, and \
             there could be more."
                .to_string(),
        );
    } else if let Some(coverage) = coverage {
        said.push(coverage.to_string());
    } else if truncated {
        said.push(INCOMPLETE_COVERAGE.to_string());
    }
    match total - confirmed {
        0 => {}
        1 => said.push(
            "1 of them matches by name alone, and Kin could not prove it uses this one.".into(),
        ),
        unconfirmed => said.push(format!(
            "{unconfirmed} of them match by name alone, and Kin could not prove they use this one."
        )),
    }
    let mut section = fields(json!({
        "shown": shown, "items": items, "total": total, "confirmed": confirmed, "bound": bound,
        "relies_total": relies_total,
    }));
    if !said.is_empty() {
        section.insert("notice".into(), Value::String(said.join(" ")));
    }
    Ok(section)
}

/// Rows for entities of one name and kind in one container, such as the
/// overload declarations of one callable, read as one row. It keeps the id of
/// the entity with the most sites, counts the rest in `declarations`, lists
/// every id in `ids`, and adds up their sites.
fn fold_same_name(items: Vec<Value>) -> Vec<Value> {
    let mut folded: Vec<Value> = Vec::new();
    // The sites of the entity each row is kept under, apart from the sum.
    let mut kept_sites: Vec<Option<u64>> = Vec::new();
    let mut at: HashMap<(String, String, String), usize> = HashMap::new();
    for item in items {
        let place = item["container"]["id"]
            .as_str()
            .or_else(|| item["module"].as_str())
            .unwrap_or_default();
        let key = (
            item["name"].as_str().unwrap_or_default().to_string(),
            item["kind"].as_str().unwrap_or_default().to_string(),
            place.to_string(),
        );
        let Some(index) = at.get(&key).copied() else {
            at.insert(key, folded.len());
            kept_sites.push(item["sites"].as_u64());
            folded.push(item);
            continue;
        };
        let held = &mut folded[index];
        let id = item["id"].clone();
        if held["ids"].is_null() {
            held["ids"] = json!([held["id"]]);
            held["declarations"] = json!(1);
        }
        if let Some(ids) = held["ids"].as_array_mut() {
            ids.push(id.clone());
        }
        held["declarations"] = json!(held["declarations"].as_u64().unwrap_or(1) + 1);
        if item["confirmed"].as_bool() == Some(true) {
            held["confirmed"] = Value::Bool(true);
        }
        if item["relies"].as_bool() == Some(true) {
            held["relies"] = Value::Bool(true);
        }
        let (mine, theirs) = (held["sites"].as_u64(), item["sites"].as_u64());
        if theirs > kept_sites[index] {
            held["id"] = id;
            kept_sites[index] = theirs;
        }
        if let (Some(mine), Some(theirs)) = (mine, theirs) {
            held["sites"] = json!(mine + theirs);
        }
        if let Some(kinds) = item["relationships"].as_array() {
            for kind in kinds {
                let present = held["relationships"]
                    .as_array()
                    .is_some_and(|held| held.contains(kind));
                if !present {
                    if let Some(held) = held["relationships"].as_array_mut() {
                        held.push(kind.clone());
                    }
                }
            }
        }
    }
    folded
}

/// The most members whose connections a class's page reads and rolls up.
const ROLLUP_MEMBERS_MAX: usize = 60;

/// Kinds whose members' connections are the entity's own: a type and its
/// methods. A module's members are read on their own pages instead.
fn rolls_up_members(kind: &str) -> bool {
    matches!(
        kind,
        "class" | "struct" | "interface" | "trait_def" | "trait" | "enum_def" | "enum" | "impl"
    )
}

/// Each member's outgoing neighbourhood, read a few at a time. A read that
/// fails or is slow leaves that member out, and the roll-up says so.
async fn member_neighborhoods(
    client: &DaemonClient,
    ids: Vec<String>,
) -> HashMap<String, Option<Value>> {
    let gate = Arc::new(tokio::sync::Semaphore::new(ENRICH_CONCURRENCY));
    let mut reads = tokio::task::JoinSet::new();
    for id in ids {
        let client = client.clone();
        let gate = gate.clone();
        reads.spawn(async move {
            let _permit = gate.acquire_owned().await.ok();
            let answer = bounded(
                READ_TIMEOUT,
                client.workspace_entity_neighborhood(&id, "out", NEIGHBORHOOD_LIMIT),
            )
            .await
            .ok();
            (id, answer)
        });
    }
    let mut found = HashMap::new();
    while let Some(read) = reads.join_next().await {
        if let Ok((id, answer)) = read {
            found.insert(id, answer);
        }
    }
    found
}

/// What a type relies on through its members, added to what it relies on
/// itself: each entity outside the type that a member calls, imports or
/// refers to, once, with the members that reach it in `through`.
pub(super) fn rolled_up_relies_on(
    own: Map<String, Value>,
    id: &str,
    members: &[String],
    members_not_read: usize,
    reads: &HashMap<String, Option<Value>>,
) -> Map<String, Value> {
    let inside: HashSet<&str> = members
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(id))
        .collect();
    let mut items: Vec<Value> = own["items"].as_array().cloned().unwrap_or_default();
    let mut at: HashMap<String, usize> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| Some((item["id"].as_str()?.to_string(), index)))
        .collect();
    let mut unread = members_not_read;
    let mut truncated = own["bound"].as_str() == Some("at_least");
    for member in members {
        let Some(Some(answer)) = reads.get(member) else {
            unread += 1;
            continue;
        };
        let Ok((relies, _)) = outgoing_sections(answer, member) else {
            unread += 1;
            continue;
        };
        truncated |= relies["bound"].as_str() == Some("at_least");
        let member_name = answer["entities"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"].as_str() == Some(member)))
            .and_then(|row| row["name"].as_str())
            .unwrap_or_default()
            .to_string();
        for item in relies["items"].as_array().into_iter().flatten() {
            let Some(target) = item["id"].as_str() else {
                continue;
            };
            if inside.contains(target) {
                continue;
            }
            let index = *at.entry(target.to_string()).or_insert_with(|| {
                let mut row = item.clone();
                row["through"] = json!([]);
                items.push(row);
                items.len() - 1
            });
            if let Some(through) = items[index]["through"].as_array_mut() {
                if !member_name.is_empty() && !through.contains(&json!(member_name)) {
                    through.push(json!(member_name));
                }
            }
        }
    }
    let by_name = |a: &Value, b: &Value| a["name"].as_str().cmp(&b["name"].as_str());
    items.sort_by(by_name);
    let total = items.len();
    items.truncate(ENTITY_LIST_CAP);
    let mut section = fields(json!({
        "shown": items.len(), "items": items, "total": total,
        "bound": if truncated || unread > 0 { "at_least" } else { "exact" },
        "through_members": true,
    }));
    if unread > 0 {
        section.insert(
            "notice".into(),
            Value::String(format!(
                "Kin could not read what {unread} of its members rely on, so there could be more."
            )),
        );
    } else if truncated {
        section.insert(
            "notice".into(),
            Value::String("Kin read only part of its connections, so there could be more.".into()),
        );
    }
    section
}

/// What the entity relies on and what it contains, from its outgoing
/// relations. Relations to anything that is not an entity here are left out.
pub(super) fn outgoing_sections(
    neighborhood: &Value,
    id: &str,
) -> Result<(Map<String, Value>, Map<String, Value>)> {
    let relations = neighborhood["relations"]
        .as_array()
        .context("the neighborhood answer omitted its relations")?;
    let entities: HashMap<&str, &Value> = neighborhood["entities"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| Some((row["id"].as_str()?, row)))
                .collect()
        })
        .unwrap_or_default();
    let mut relies_on: Vec<Value> = Vec::new();
    let mut members: Vec<Value> = Vec::new();
    let mut seen = HashSet::new();
    for relation in relations {
        if relation["direction"].as_str() != Some("outgoing") {
            continue;
        }
        let Some(other) = relation["dst"]["Entity"].as_str() else {
            continue;
        };
        if other == id {
            continue;
        }
        let kind = kind_label(relation["kind"].as_str().unwrap_or_default());
        if !seen.insert((other.to_string(), kind.clone())) {
            continue;
        }
        let Some(row) = entities.get(other) else {
            continue;
        };
        let mut item = fields(entity_ref(
            other,
            row["name"].as_str().unwrap_or_default(),
            row["kind"].as_str().unwrap_or_default(),
        ));
        if kind == "contains" {
            members.push(Value::Object(item));
        } else {
            item.insert("relationship".into(), Value::String(kind));
            relies_on.push(Value::Object(item));
        }
    }
    let by_name = |a: &Value, b: &Value| a["name"].as_str().cmp(&b["name"].as_str());
    relies_on.sort_by(by_name);
    members.sort_by(by_name);
    let truncated = neighborhood["truncated"].as_bool().unwrap_or(false);
    let list = |mut items: Vec<Value>| {
        let total = items.len();
        items.truncate(ENTITY_LIST_CAP);
        let mut section = fields(json!({
            "shown": items.len(), "items": items, "total": total,
            "bound": if truncated { "at_least" } else { "exact" },
        }));
        if truncated {
            section.insert(
                "notice".into(),
                Value::String(
                    "Kin read only part of its connections, so there could be more.".into(),
                ),
            );
        }
        section
    };
    Ok((list(relies_on), list(members)))
}

fn impact_bound(response: &super::impact::ImpactResponse) -> (&'static str, Option<&'static str>) {
    match &response.enrichment {
        Some(observation) if observation.status == "bounded" => {
            let unproven = observation
                .limitation
                .as_deref()
                .is_some_and(|limitation| limitation.contains("call_sites_unproven"));
            (
                "at_least",
                Some(if unproven {
                    UNPROVEN_SITES
                } else {
                    INCOMPLETE_COVERAGE
                }),
            )
        }
        _ => ("exact", None),
    }
}

/// What changing the entity reaches, and the tests among it.
pub(super) fn impact_sections(
    response: &super::impact::ImpactResponse,
) -> Result<(Map<String, Value>, Map<String, Value>)> {
    if response.resolution != "resolved" {
        let said = response.lines.join(" ");
        anyhow::bail!(
            "{}",
            if said.trim().is_empty() {
                format!("the impact walk answered '{}'", response.resolution)
            } else {
                said
            }
        );
    }
    let ranked = response
        .ranked
        .as_ref()
        .context("the impact answer omitted its ranking")?;
    let bucket = |candidate: &kin_review::RankedImpactCandidate| {
        serde_json::to_value(candidate.bucket)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_default()
    };
    let candidates = &ranked.candidates;
    let direct = candidates
        .iter()
        .filter(|candidate| candidate.hop == 1)
        .count();
    // Tests as the walk counts them, one per test, nearest and most direct
    // first: the same reading a guided review's step and verification use.
    let tests = &ranked.tests;
    let mut ordered: Vec<&kin_review::RankedImpactCandidate> = candidates.iter().collect();
    ordered.sort_by(|a, b| {
        b.priority_score
            .cmp(&a.priority_score)
            .then_with(|| a.hop.cmp(&b.hop))
            .then_with(|| a.identity.name.cmp(&b.identity.name))
    });
    // Tests are the tests section's rows, nearest first; listed here too, the
    // same test read twice on one page, once as further out.
    let top: Vec<Value> = ordered
        .into_iter()
        .filter(|candidate| bucket(candidate) != "test")
        .take(IMPACT_TOP)
        .map(|candidate| {
            let reason = bucket(candidate);
            json!({
                "id": candidate.entity_id.to_string(), "name": candidate.identity.name,
                "kind": kind_label(&candidate.identity.kind), "hops": candidate.hop,
                "relationship": kind_label(&format!("{:?}", candidate.relation)),
                "is_test": reason == "test", "reason": reason,
            })
        })
        .collect();
    let (bound, notice) = impact_bound(response);
    let mut impact = fields(json!({
        "direct": direct, "transitive": candidates.len(), "tests": tests.len(),
        "depth": ranked.max_depth, "top": top, "bound": bound,
    }));
    let mut test_items: Vec<Value> = tests
        .iter()
        .map(|test| {
            let mut item = json!({
                "id": test.entity_id.to_string(), "name": test.identity.name,
                "kind": kind_label(&test.identity.kind), "hops": test.hop,
            });
            if test.by_call() {
                item["by_call"] = Value::Bool(true);
            }
            // Tests of one name are told apart by the module each sits in,
            // or, for test modules of one name, the namespace each sits in.
            let places = |of: &kin_review::TestReach| {
                super::review_guide::enclosing_namespaces(
                    Some(&of.identity.file),
                    of.identity.kind != "module",
                )
            };
            let others: Vec<Vec<String>> = tests
                .iter()
                .filter(|other| {
                    other.entity_id != test.entity_id && other.identity.name == test.identity.name
                })
                .map(places)
                .collect();
            if !others.is_empty() {
                if let Some(place) = super::review_guide::distinct_place(&places(test), &others) {
                    item["module"] = Value::String(place);
                }
            }
            item
        })
        .collect();
    let total = test_items.len();
    test_items.truncate(ENTITY_LIST_CAP);
    let mut tests_section = fields(json!({
        "shown": test_items.len(), "items": test_items, "total": total, "bound": bound,
        "depth": ranked.max_depth,
    }));
    if let Some(notice) = notice {
        impact.insert("notice".into(), Value::String(notice.into()));
        tests_section.insert("notice".into(), Value::String(notice.into()));
    }
    Ok((impact, tests_section))
}

/// Recent recorded changes that changed the entity, newest first.
///
/// While the store holds only part of the repository's history
/// (`history_partial`), the change Kin's record of it starts at, when that is
/// the one change the store began with, is named as `record_start`: the page
/// says the record starts there, with the repository as it was, as the
/// repository's own history does.
pub(super) fn history_section(
    history: &Value,
    history_partial: bool,
) -> Result<Map<String, Value>> {
    let revisions = history["revisions"]
        .as_array()
        .context("the history answer omitted its revisions")?;
    let items: Vec<Value> = revisions
        .iter()
        .take(HISTORY_CAP)
        .map(|revision| {
            json!({
                "change_id": revision["id"],
                "title": revision["title"],
                "author": author_display(revision["author"].as_str().unwrap_or_default()),
                "time": revision["timestamp"],
            })
        })
        .collect();
    let total = history["revisions_total"]
        .as_u64()
        .unwrap_or(revisions.len() as u64);
    let mut section = fields(json!({
        "shown": items.len(), "items": items, "total": total,
        "unchanged": history["unchanged_revisions"].as_u64().unwrap_or(0),
    }));
    let start = history["record_start"].as_str();
    let listed_last = items.last().and_then(|item| item["change_id"].as_str());
    if let (true, Some(start)) = (history_partial, start) {
        if listed_last == Some(start) && items.len() as u64 == total {
            section.insert("record_start".into(), Value::String(start.to_string()));
        }
    }
    Ok(section)
}

fn review_json(response: Result<super::review::ReviewResponse>) -> Result<Value> {
    let json = response?
        .json
        .context("the daemon answered without its JSON")?;
    serde_json::from_str(&json).context("the daemon's answer was not JSON")
}

/// Whether the working tree's record says the entity is not in it.
fn is_absent(recorded: &Value) -> bool {
    recorded["entity"].is_null()
        && recorded["body"]["state"].as_str() == Some("unavailable")
        && recorded["body"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.ends_with("is not in the working tree"))
}

/// Run `read` only when one of the sections that need it was asked for.
async fn when<T>(wanted: bool, read: impl Future<Output = Result<T>>) -> Option<Result<T>> {
    if wanted {
        Some(read.await)
    } else {
        None
    }
}

/// A read a section needs that was not asked for. Never reaches the answer:
/// a section is only built when it was asked for, and its reads with it.
fn not_read<T>(read: Option<Result<T>>) -> Result<T> {
    read.unwrap_or_else(|| Err(anyhow::anyhow!("it was not read")))
}

fn split<A, B>(answer: Result<(A, B)>) -> (Result<A>, Result<B>) {
    match answer {
        Ok((first, second)) => (Ok(first), Ok(second)),
        Err(error) => {
            let again = anyhow::anyhow!("{error:#}");
            (Err(error), Err(again))
        }
    }
}

async fn serve_entity(
    State(state): State<Arc<UiState>>,
    Query(query): Query<EntityQuery>,
) -> Response {
    let Some(id) = parse_entity_id(&query.id) else {
        return bad_request("An entity UUID is required.");
    };
    let asked = match entity_sections(query.sections.as_deref()) {
        Ok(asked) => asked,
        Err(reason) => return bad_request(&reason),
    };
    let wants = |names: &[&str]| names.iter().any(|name| asked.contains(name));
    let (client, health) = match connect(&state, "ui entity").await {
        Ok(connected) => connected,
        Err(error) => return daemon_unreachable(error),
    };
    let repo_id = health.repo_id.clone().unwrap_or_default();
    let mut reference_arguments = HashMap::new();
    reference_arguments.insert("entity_id".to_string(), json!(id));
    let impact_request = super::impact::ImpactRequest {
        entity: id.clone(),
        depth: IMPACT_DEPTH,
        file: None,
        kind: None,
        signature: None,
        require_unique: true,
        dispatch_candidates: false,
    };
    // The working tree's own record of the entity, read first: it says
    // whether the entity exists at all, so an unknown id is answered as one
    // instead of as a page of sections that each found nothing.
    let recorded = bounded(READ_TIMEOUT, async {
        review_json(
            client
                .review(&super::review::ReviewRequest::GuideSource {
                    entity: id.clone(),
                    change: super::review_guide::WORKING.to_string(),
                })
                .await,
        )
    })
    .await;
    if recorded.as_ref().is_ok_and(is_absent) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": format!("No entity with id {id} is in this repository's current graph."),
            })),
        )
            .into_response();
    }
    let (source, references, incoming, outgoing, impact, history) = tokio::join!(
        // The source answer, then the doc summary the inventory holds for it.
        when(
            wants(&["header", "source", "editor"]),
            bounded(READ_TIMEOUT, async {
                let source = client.workspace_entity_source(&id).await?;
                anyhow::ensure!(
                    source["id"].as_str() == Some(id.as_str()),
                    "the source answer named another entity"
                );
                // Its row in the importance ranking carries its doc summary
                // and how many entities rely on it, the number Home shows.
                let ranked = match text_field(&source, "name") {
                    Some(name) if !repo_id.is_empty() && asked.contains(&"header") => client
                        .workspace_entities(&repo_id, &name, SUMMARY_LOOKUP_ROWS, 0)
                        .await
                        .ok()
                        .and_then(|inventory| {
                            inventory["entities"].as_array().and_then(|rows| {
                                rows.iter()
                                    .find(|row| row["id"].as_str() == Some(id.as_str()))
                                    .cloned()
                            })
                        }),
                    _ => None,
                };
                Ok((source, ranked))
            })
        ),
        when(
            wants(&["relied_on_by"]),
            bounded(WALK_TIMEOUT, async {
                tool_json(
                    client.reference_page(reference_arguments).await?,
                    "find_references",
                )
            })
        ),
        when(
            wants(&["relied_on_by"]),
            bounded(
                READ_TIMEOUT,
                client.workspace_entity_neighborhood(&id, "in", NEIGHBORHOOD_LIMIT)
            )
        ),
        when(
            wants(&["relies_on", "members"]),
            bounded(
                READ_TIMEOUT,
                client.workspace_entity_neighborhood(&id, "out", NEIGHBORHOOD_LIMIT)
            )
        ),
        when(
            wants(&["impact", "tests", "relied_on_by"]),
            bounded(WALK_TIMEOUT, client.impact(&impact_request))
        ),
        when(
            wants(&["history"]),
            bounded(WALK_TIMEOUT, async {
                review_json(
                    client
                        .review(&super::review::ReviewRequest::GuideHistory {
                            entity: id.clone(),
                            at: None,
                            since: None,
                        })
                        .await,
                )
            })
        ),
    );
    let (source, ranked, source_error) = match not_read(source) {
        Ok((source, ranked)) => (Some(source), ranked, None),
        Err(error) => (None, None, Some(error)),
    };
    let summary = ranked.as_ref().and_then(|row| text_field(row, "summary"));
    let dependents = ranked.as_ref().and_then(|row| row["dependents"].as_u64());
    let recorded_entity = recorded
        .as_ref()
        .ok()
        .map(|recorded| recorded["entity"].clone())
        .filter(Value::is_object);
    let impact = not_read(impact);
    // Which reaching entities are tests, from the impact walk, for the rows
    // whose own answer does not say.
    let test_roles: HashMap<String, bool> = impact
        .as_ref()
        .ok()
        .and_then(|response| response.ranked.as_ref())
        .map(|ranked| {
            ranked
                .candidates
                .iter()
                .map(|candidate| {
                    let bucket = serde_json::to_value(candidate.bucket).unwrap_or_default();
                    (candidate.entity_id.to_string(), bucket == "test")
                })
                .collect()
        })
        .unwrap_or_default();
    let (impact, tests) = split(impact.and_then(|response| impact_sections(&response)));
    let (relies_on, members) =
        split(not_read(outgoing).and_then(|answer| outgoing_sections(&answer, &id)));
    // A class relies on what its methods call: its own record holds only
    // what it contains, so the members' connections are read and rolled up.
    let kind = source
        .as_ref()
        .or(recorded_entity.as_ref())
        .map(|from| kind_label(from["kind"].as_str().unwrap_or_default()))
        .unwrap_or_default();
    let relies_on = match (relies_on, &members) {
        (Ok(own), Ok(held)) if asked.contains(&"relies_on") && rolls_up_members(&kind) => {
            let member_ids: Vec<String> = held["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| item["id"].as_str().map(str::to_string))
                .take(ROLLUP_MEMBERS_MAX)
                .collect();
            let member_total = held["total"]
                .as_u64()
                .map_or(member_ids.len(), |total| total as usize);
            if member_ids.is_empty() {
                Ok(own)
            } else {
                let reads = member_neighborhoods(&client, member_ids.clone()).await;
                Ok(rolled_up_relies_on(
                    own,
                    &id,
                    &member_ids,
                    member_total.saturating_sub(member_ids.len()),
                    &reads,
                ))
            }
        }
        (relies_on, _) => relies_on,
    };
    let references = not_read(references);
    let incoming = not_read(incoming);
    let history_partial = asked.contains(&"history") && {
        let layout = state.layout.clone();
        tokio::task::spawn_blocking(move || {
            matches!(
                kin_core::history_state::standing(&layout),
                kin_core::history_state::HistoryStanding::Partial { .. }
            )
        })
        .await
        .unwrap_or(false)
    };
    let mut history = Some(not_read(history));
    let mut relies_on = Some(relies_on);
    let mut members = Some(members);
    let mut impact = Some(impact);
    let mut tests = Some(tests);
    let mut answer = fields(json!({
        "schema": "kin.workspace.entity.v2",
        "id": id,
        "sections": asked,
        "read_at": read_at(),
    }));
    for name in &asked {
        let value = match *name {
            "header" => section(
                "Kin could not read this entity.",
                header_section(source.as_ref(), recorded_entity.as_ref(), summary.clone())
                    .map(|mut header| {
                        if let Some(dependents) = dependents {
                            header.insert("dependents".into(), json!(dependents));
                        }
                        header
                    })
                    .map_err(|error| match &source_error {
                        Some(source_error) => anyhow::anyhow!("{source_error:#}"),
                        None => error,
                    }),
            ),
            "source" => section(
                "Kin could not read this entity's text.",
                match (&recorded, &source) {
                    (Err(error), None) => Err(anyhow::anyhow!("{error:#}")),
                    _ => source_section(recorded.as_ref().ok(), source.as_ref()),
                },
            ),
            "relied_on_by" => section(
                "Kin could not read what relies on this entity.",
                match (&incoming, &references) {
                    (Err(error), Err(_)) => Err(anyhow::anyhow!("{error:#}")),
                    _ => relied_on_by_section(
                        &id,
                        incoming.as_ref().ok(),
                        references.as_ref().ok(),
                        &test_roles,
                    ),
                },
            ),
            "relies_on" => section(
                "Kin could not read what this entity relies on.",
                relies_on
                    .take()
                    .unwrap_or_else(|| Err(anyhow::anyhow!("it was not read"))),
            ),
            "members" => section(
                "Kin could not read what this entity contains.",
                members
                    .take()
                    .unwrap_or_else(|| Err(anyhow::anyhow!("it was not read"))),
            ),
            "tests" => section(
                "Kin could not read which tests reach this entity.",
                tests
                    .take()
                    .unwrap_or_else(|| Err(anyhow::anyhow!("it was not read"))),
            ),
            "impact" => section(
                "Kin could not walk what a change to this entity reaches.",
                impact
                    .take()
                    .unwrap_or_else(|| Err(anyhow::anyhow!("it was not read"))),
            ),
            "history" => section(
                "Kin could not read this entity's history.",
                history
                    .take()
                    .unwrap_or_else(|| Err(anyhow::anyhow!("it was not read")))
                    .and_then(|answer| history_section(&answer, history_partial)),
            ),
            "editor" => section(
                "This entity cannot be opened in an editor.",
                match &source {
                    Some(source) => {
                        super::workspace_graph::editor_uri(source, &id, &health, &state.root)
                            .map(|uri| fields(json!({ "uri": uri })))
                    }
                    None => Err(anyhow::anyhow!("its current source could not be read")),
                },
            ),
            _ => continue,
        };
        answer.insert((*name).to_string(), value);
    }
    Json(Value::Object(answer)).into_response()
}

// ---------------------------------------------------------------------------
// Map
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PairEdge {
    weight: usize,
    kinds: BTreeMap<String, usize>,
}

fn pair_key<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

fn edge_json(level: &str, (source, target): (&str, &str), edge: &PairEdge) -> Value {
    json!({
        "source": source, "target": target, "level": level,
        "weight": edge.weight, "kinds": edge.kinds,
    })
}

fn sorted_edges<'a>(
    edges: HashMap<(&'a str, &'a str), PairEdge>,
) -> Vec<((&'a str, &'a str), PairEdge)> {
    let mut edges: Vec<_> = edges.into_iter().collect();
    edges.sort_by(|a, b| b.1.weight.cmp(&a.1.weight).then_with(|| a.0.cmp(&b.0)));
    edges
}

/// The repository map: top-level containers sized by what they hold, the
/// most connected entities inside them, the relationships between containers
/// added up, and those among the shown entities.
pub(super) fn build_map(payload: &GraphExportPayload, node_cap: usize, edge_cap: usize) -> Value {
    let census = Census::new(payload);
    let top_containers = census.top_containers();
    let container_budget = top_containers.len().min((node_cap * 2 / 5).max(1));
    let shown_containers: Vec<&GraphExportNode> = top_containers
        .iter()
        .copied()
        .take(container_budget)
        .collect();
    let shown_groups: HashSet<&str> = shown_containers
        .iter()
        .map(|node| node.id.as_str())
        .collect();

    // The most connected defined entities inside the shown containers.
    let mut candidates: Vec<&GraphExportNode> = payload
        .nodes
        .iter()
        .filter(|node| node.file.is_some() && !shown_groups.contains(node.id.as_str()))
        .filter(|node| shown_groups.contains(census.top(&node.id)))
        .filter(|node| census.degree(&node.id) > 0 || census.members(&node.id) > 0)
        .collect();
    candidates.sort_by(|a, b| {
        census
            .degree(&b.id)
            .cmp(&census.degree(&a.id))
            .then_with(|| census.size(&b.id).cmp(&census.size(&a.id)))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
    let entity_budget = node_cap.saturating_sub(shown_containers.len());
    let total_candidates = candidates.len();
    candidates.truncate(entity_budget);

    let shown: HashSet<&str> = shown_containers
        .iter()
        .chain(candidates.iter())
        .map(|node| node.id.as_str())
        .collect();
    let nearest_shown = |id: &str| -> Option<String> {
        let mut current = census.parent.get(id).copied();
        for _ in 0..64 {
            let container = current?;
            if shown.contains(container) {
                return Some(container.to_string());
            }
            current = census.parent.get(container).copied();
        }
        None
    };
    let node_json = |node: &GraphExportNode, role: &str| {
        json!({
            "id": node.id, "name": node.name, "kind": kind_label(&node.kind), "role": role,
            "size": census.size(&node.id), "degree": census.degree(&node.id),
            "parent": nearest_shown(&node.id), "group": census.top(&node.id),
        })
    };
    let mut nodes: Vec<Value> = shown_containers
        .iter()
        .map(|node| node_json(node, "container"))
        .collect();
    nodes.extend(candidates.iter().map(|node| node_json(node, "entity")));

    // Relationships added up between top-level containers, and kept between
    // shown entities. `contains` is the hierarchy, carried by `parent`.
    let all_groups: HashSet<&str> = top_containers.iter().map(|node| node.id.as_str()).collect();
    let mut between_containers: HashMap<(&str, &str), PairEdge> = HashMap::new();
    let mut between_entities: HashMap<(&str, &str), PairEdge> = HashMap::new();
    let mut entity_pairs: HashSet<(&str, &str)> = HashSet::new();
    for link in &payload.links {
        if is_contains(&link.kind) || link.source == link.target {
            continue;
        }
        if !census.defined(&link.source) || !census.defined(&link.target) {
            continue;
        }
        let kind = kind_label(&link.kind);
        entity_pairs.insert(pair_key(&link.source, &link.target));
        let (group_a, group_b) = (census.top(&link.source), census.top(&link.target));
        if group_a != group_b && all_groups.contains(group_a) && all_groups.contains(group_b) {
            let edge = between_containers
                .entry(pair_key(group_a, group_b))
                .or_default();
            edge.weight += 1;
            *edge.kinds.entry(kind.clone()).or_default() += 1;
        }
        if shown.contains(link.source.as_str()) && shown.contains(link.target.as_str()) {
            let edge = between_entities
                .entry(pair_key(&link.source, &link.target))
                .or_default();
            edge.weight += 1;
            *edge.kinds.entry(kind).or_default() += 1;
        }
    }
    let container_edges_total = between_containers.len();
    let mut edges: Vec<Value> = sorted_edges(between_containers)
        .into_iter()
        .filter(|((a, b), _)| shown_groups.contains(a) && shown_groups.contains(b))
        .take(edge_cap)
        .map(|(pair, edge)| edge_json("container", pair, &edge))
        .collect();
    let container_edges_shown = edges.len();
    let room = edge_cap.saturating_sub(edges.len());
    let entity_edges_among_shown = between_entities.len();
    edges.extend(
        sorted_edges(between_entities)
            .into_iter()
            .take(room)
            .map(|(pair, edge)| edge_json("entity", pair, &edge)),
    );

    let without_definition = payload
        .nodes
        .iter()
        .filter(|node| node.file.is_none())
        .count();
    // Defined entities that sit in no container and contain nothing, so no
    // group holds them. Counted, never drawn as a group of their own.
    let ungrouped = payload
        .nodes
        .iter()
        .filter(|node| node.file.is_some())
        .filter(|node| !all_groups.contains(census.top(&node.id)))
        .count();
    let sampled = shown_containers.len() < top_containers.len()
        || candidates.len() < total_candidates
        || edges.len() < container_edges_shown + entity_edges_among_shown;
    let notice = if top_containers.is_empty() {
        "Kin recorded no entity that contains another here, so there is nothing to group by."
            .to_string()
    } else if shown_containers.len() < top_containers.len() {
        format!(
            "Showing the {} largest of {} top-level containers and the {} most connected entities inside them.",
            shown_containers.len(),
            top_containers.len(),
            candidates.len()
        )
    } else {
        format!(
            "Showing the {} top-level containers and the {} most connected entities inside them.",
            shown_containers.len(),
            candidates.len()
        )
    };
    json!({
        "schema": "kin.workspace.map.v1",
        "nodes": nodes,
        "edges": edges,
        "directed": false,
        "totals": {
            "entities": payload.entity_count,
            "relationships": payload.relation_count,
            "without_definition": without_definition,
            "ungrouped": ungrouped,
            "unplaced_containment": census.unoriented,
            "containers": top_containers.len(),
            "container_edges": container_edges_total,
            "entity_edges": entity_pairs.len(),
        },
        "shown": {
            "nodes": shown_containers.len() + candidates.len(),
            "edges": edges.len(),
            "containers": shown_containers.len(),
            "entities": candidates.len(),
        },
        "caps": { "nodes": node_cap, "edges": edge_cap },
        "sampled": sampled,
        "notice": notice,
    })
}

async fn serve_map(State(state): State<Arc<UiState>>) -> Response {
    let (client, _health) = match connect(&state, "ui map").await {
        Ok(connected) => connected,
        Err(error) => return daemon_unreachable(error),
    };
    match bounded(GRAPH_TIMEOUT, client.graph_export("limit=0")).await {
        Ok(payload) => {
            let mut map = build_map(&payload, MAP_NODE_CAP, MAP_EDGE_CAP);
            map["read_at"] = Value::String(read_at());
            Json(map).into_response()
        }
        Err(error) => daemon_unreachable(anyhow::anyhow!(gap(
            "Kin could not read the repository graph.",
            &error
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::super::graph_export::{GraphExportLink, GraphExportNode, GraphExportPayload};
    use super::*;

    fn node(id: &str, name: &str, kind: &str, defined: bool) -> GraphExportNode {
        GraphExportNode {
            id: id.into(),
            name: name.into(),
            kind: kind.into(),
            file: defined.then(|| "held/for/grouping/only.py".to_string()),
            degree: 0,
            line: None,
            signature: None,
            language: None,
        }
    }

    fn link(source: &str, target: &str, kind: &str) -> GraphExportLink {
        GraphExportLink {
            source: source.into(),
            target: target.into(),
            kind: kind.into(),
        }
    }

    /// Two modules, a class with two methods in the first, a function in the
    /// second, an imported name with no definition, and the calls among them.
    /// Links are given in the export's own merged order, smaller id first.
    fn fixture() -> GraphExportPayload {
        let nodes = vec![
            node("a-module", "signer", "Module", true),
            node("b-class", "Signer", "Class", true),
            node("c-method", "Signer.sign", "Method", true),
            node("d-method", "Signer.unsign", "Method", true),
            node("e-module", "encoding", "Module", true),
            node("f-function", "want_bytes", "Function", true),
            node("g-import", "t", "Module", false),
        ];
        let links = vec![
            link("a-module", "b-class", "Contains"),
            link("b-class", "c-method", "Contains"),
            link("b-class", "d-method", "Contains"),
            link("e-module", "f-function", "Contains"),
            link("c-method", "f-function", "Calls"),
            link("d-method", "f-function", "Calls"),
            link("c-method", "d-method", "Calls"),
            link("a-module", "g-import", "Imports"),
        ];
        GraphExportPayload {
            root_hash: "0".repeat(64),
            seq: 1,
            entity_count: nodes.len(),
            relation_count: links.len(),
            unresolved_links: 0,
            filtered_links: 0,
            sampled: false,
            limit: None,
            nodes,
            links,
        }
    }

    #[test]
    fn kinds_and_relations_read_in_one_spelling() {
        assert_eq!(kind_label("TraitDef"), "trait_def");
        assert_eq!(kind_label("trait_def"), "trait_def");
        assert_eq!(kind_label("Class"), "class");
        assert_eq!(kind_label("method"), "method");
        assert_eq!(kind_label("Calls"), "calls");
        assert_eq!(kind_label("ApiEndpoint"), "api_endpoint");
    }

    /// Every entity kind reads in the one spelling the graph serializes,
    /// whether it arrives as that spelling, as its variant name, or as the
    /// variant name lowercased the way locate publishes it.
    #[test]
    fn every_entity_kind_reads_as_its_serialized_word_however_it_arrives() {
        use kin_model::EntityKind::{self, *};
        let every = [
            Function,
            Class,
            Interface,
            TraitDef,
            TypeAlias,
            Module,
            Package,
            Test,
            Schema,
            ApiEndpoint,
            EventContract,
            File,
            DocumentNode,
            Method,
            EnumDef,
            EnumVariant,
            Constant,
            StaticVar,
            Macro,
            Field,
        ];
        // A kind added later fails to compile here until it is listed above.
        for kind in every {
            match kind {
                Function | Class | Interface | TraitDef | TypeAlias | Module | Package | Test
                | Schema | ApiEndpoint | EventContract | File | DocumentNode | Method | EnumDef
                | EnumVariant | Constant | StaticVar | Macro | Field => {}
            }
        }
        let serialized = |kind: EntityKind| {
            serde_json::to_value(kind)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        };
        for kind in every {
            let word = serialized(kind);
            let variant = format!("{kind:?}");
            assert_eq!(kind_label(&word), word);
            assert_eq!(kind_label(&variant), word, "{variant}");
            assert_eq!(kind_label(&variant.to_lowercase()), word, "{variant}");
        }
    }

    #[test]
    fn shown_text_never_carries_an_em_dash() {
        assert_eq!(
            plain("pass positionally \u{2014} no runtime break"),
            "pass positionally, no runtime break"
        );
        let said = gap(
            "Kin could not read this entity.",
            &anyhow::anyhow!("graph source gap \u{2014} entity absent"),
        );
        assert_eq!(
            said,
            "Kin could not read this entity. Graph source gap, entity absent."
        );
    }

    /// Containment is oriented from a merged export: the broader kind
    /// contains the narrower, so a method sits in its class and the class in
    /// its module, whichever end the export listed first.
    #[test]
    fn containment_is_oriented_by_kind_and_counted_through_nesting() {
        let payload = fixture();
        let census = Census::new(&payload);
        assert_eq!(census.parent.get("c-method"), Some(&"b-class"));
        assert_eq!(census.parent.get("b-class"), Some(&"a-module"));
        assert_eq!(census.top("c-method"), "a-module");
        assert_eq!(census.size("a-module"), 3);
        assert_eq!(census.members("a-module"), 1);
        assert_eq!(census.size("b-class"), 2);
        assert_eq!(census.unoriented, 0);
        // `contains` is hierarchy, not connection.
        assert_eq!(census.degree("b-class"), 0);
        assert_eq!(census.degree("f-function"), 2);
        let containers: Vec<&str> = census
            .containers()
            .iter()
            .map(|node| node.id.as_str())
            .collect();
        assert_eq!(containers, ["a-module", "b-class", "e-module"]);
    }

    /// Between two of one rank, a member is placed under the entity its name
    /// is qualified under, and a pair neither rule orients is placed nowhere.
    #[test]
    fn same_rank_containment_follows_the_qualified_name_or_stays_unplaced() {
        let mut payload = fixture();
        payload
            .nodes
            .push(node("h-nested", "Signer.Options", "Class", true));
        payload
            .nodes
            .push(node("i-peer", "Unrelated", "Class", true));
        payload.links.push(link("b-class", "h-nested", "Contains"));
        payload.links.push(link("h-nested", "i-peer", "Contains"));
        let census = Census::new(&payload);
        assert_eq!(census.parent.get("h-nested"), Some(&"b-class"));
        assert!(!census.parent.contains_key("i-peer"));
        assert_eq!(census.unoriented, 1);

        // A class defined inside a method is the method's, not its owner.
        let mut payload = fixture();
        payload.nodes.push(node(
            "j-local",
            "Signer.sign.<locals>.Helper",
            "Class",
            true,
        ));
        payload.links.push(link("c-method", "j-local", "Contains"));
        let census = Census::new(&payload);
        assert_eq!(census.parent.get("j-local"), Some(&"c-method"));
        assert_eq!(census.parent.get("c-method"), Some(&"b-class"));
        assert_eq!(census.top("j-local"), "a-module");
    }

    #[test]
    fn overview_counts_split_what_is_defined_here() {
        let payload = fixture();
        let counts = overview_counts(&payload);
        assert_eq!(counts["entities"], 7);
        assert_eq!(counts["defined_here"], 6);
        assert_eq!(counts["without_definition"], 1);
        assert_eq!(counts["by_kind"][0], json!({"kind": "module", "count": 3}));
        assert!(counts["notice"]
            .as_str()
            .unwrap()
            .starts_with("1 entity is a name"));
        let census = Census::new(&payload);
        let containers = overview_containers(&census, 2);
        assert_eq!(containers["total"], 3);
        assert_eq!(containers["shown"], 2);
        assert_eq!(containers["items"][1]["name"], "Signer");
        assert_eq!(containers["items"][1]["container"]["name"], "signer");
    }

    /// The most relied-on list and the relationship count say whether they
    /// are exact or lower bounds, from the daemon's store-wide call-site
    /// reading. A daemon that does not report the reading, or a summary that
    /// was not read, leaves both with no bound at all, never a guessed one.
    #[test]
    fn overview_counts_say_whether_they_can_miss_callers_only_when_the_daemon_says() {
        let payload = fixture();
        let census = Census::new(&payload);
        let inventory = json!({"total": 1, "entities": [
            {"id": "f-function", "name": "want_bytes", "kind": "Function",
             "file_path": "held/for/grouping/only.py", "dependents": 2},
        ]});
        let summary = |settled: Value| {
            let mut summary = json!({"entities": 7, "edges": 9, "kinds": {}, "languages": {}});
            if !settled.is_null() {
                summary["call_sites_settled"] = settled;
            }
            summary
        };
        let read = |summary: Option<&Value>| {
            let bound = relied_on_bound(summary);
            let counts = section(
                "counts",
                Ok(with_bound(
                    overview_counts(&payload),
                    "relationships_bound",
                    bound,
                )),
            );
            let relied_on = section(
                "relied on",
                overview_relied_on(&inventory, Some(&census), 10)
                    .map(|relied_on| with_bound(relied_on, "bound", bound)),
            );
            (counts, relied_on)
        };

        let unsettled = summary(json!(false));
        let (counts, relied_on) = read(Some(&unsettled));
        assert_eq!(counts["relationships_bound"], "at_least");
        assert_eq!(relied_on["bound"], "at_least");
        assert_eq!(counts["relationships"], payload.relation_count);
        assert_eq!(relied_on["items"][0]["dependents"], 2);

        let settled = summary(json!(true));
        let (counts, relied_on) = read(Some(&settled));
        assert_eq!(counts["relationships_bound"], "exact");
        assert_eq!(relied_on["bound"], "exact");

        // An older daemon, a reading that is not a yes or no, and a summary
        // that failed all say nothing about the bound.
        let older = summary(Value::Null);
        let malformed = summary(json!("settled"));
        for summary in [Some(&older), Some(&malformed), None] {
            let (counts, relied_on) = read(summary);
            assert_eq!(counts["ok"], true);
            assert!(counts.get("relationships_bound").is_none(), "{counts}");
            assert_eq!(relied_on["ok"], true);
            assert!(relied_on.get("bound").is_none(), "{relied_on}");
        }
    }

    /// Two entities can share a name, a kind and the name of what contains
    /// them. What their container sits in tells them apart, so each
    /// container says it, one level up and by name alone.
    #[test]
    fn a_container_says_what_it_sits_in() {
        let payload = fixture();
        let census = Census::new(&payload);
        // A method sits in a class, and the class in a module.
        assert_eq!(
            census.container_of("c-method"),
            Some(json!({
                "id": "b-class", "name": "Signer", "kind": "class",
                "within": {"id": "a-module", "name": "signer", "kind": "module"},
            }))
        );
        // A module here sits in nothing, so its members' container says
        // nothing more.
        assert_eq!(
            census.container_of("b-class"),
            Some(json!({"id": "a-module", "name": "signer", "kind": "module"}))
        );

        // A ranked result carries the same, read from its container's own
        // answer and never from where anything is kept.
        let owners = |id: &str, name: &str, kind: &str| json!([{"entity_id": id, "name": name, "kind": kind, "relationship": "Contains"}]);
        let sources: HashMap<String, Value> = [(SEND, HOOK), (REDIRECT, HOOK_END)]
            .into_iter()
            .map(|(id, class)| {
                let source = json!({
                    "name": "MultipartError.status", "kind": "Method",
                    "module": "extract.multipart",
                    "semantic_owners": owners(class, "MultipartError", "class"),
                    "file_path": "held/for/grouping/only.rs",
                });
                (id.to_string(), source)
            })
            .collect();
        let outer: HashMap<String, Value> = [(HOOK, "multipart"), (HOOK_END, "multipart_extra")]
            .into_iter()
            .map(|(class, module)| {
                let within = owner_ref(&owners(OTHER, module, "Module")).unwrap();
                (class.to_string(), within)
            })
            .collect();
        let rows: Vec<Value> = [SEND, REDIRECT]
            .into_iter()
            .map(|id| {
                let hit = hit(id, "MultipartError.status", true, None);
                hit_json(&hit, sources.get(id), &outer)
            })
            .collect();
        assert_eq!(rows[0]["container"]["name"], rows[1]["container"]["name"]);
        assert_eq!(
            rows[0]["container"]["within"],
            json!({"id": OTHER, "name": "multipart", "kind": "module"})
        );
        assert_eq!(rows[1]["container"]["within"]["name"], "multipart_extra");
        // A container whose own container is not known says nothing of it.
        let alone = hit_json(
            &hit(SEND, "MultipartError.status", true, None),
            sources.get(SEND),
            &HashMap::new(),
        );
        assert!(alone["container"].get("within").is_none());
        assert!(!json!(rows).to_string().contains("held/for/grouping"));
    }

    /// Two containers can share a name and a kind, as a JavaScript module and
    /// the TypeScript one beside it do, and no answer says where either is.
    /// Each row carries its language so a page can tell them apart, and null
    /// when the daemon that answered does not report one.
    #[test]
    fn a_container_row_carries_its_language_or_null() {
        let mut payload = fixture();
        for node in &mut payload.nodes {
            node.language = match node.id.as_str() {
                "a-module" => Some("JavaScript".to_string()),
                "e-module" => Some("typescript".to_string()),
                _ => None,
            };
        }
        let census = Census::new(&payload);
        let containers = overview_containers(&census, 10);
        let language = |name: &str| {
            containers["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["name"] == name)
                .unwrap_or_else(|| panic!("{name} is not a container row"))["language"]
                .clone()
        };
        assert_eq!(language("signer"), json!("javascript"));
        assert_eq!(language("encoding"), json!("typescript"));
        assert_eq!(language("Signer"), Value::Null);
    }

    /// No answer carries a place in the checkout, even though the export
    /// holds one per node.
    #[test]
    fn no_answer_carries_a_path() {
        let payload = fixture();
        let census = Census::new(&payload);
        let inventory = json!({"total": 2, "entities": [
            {"id": "g-import", "name": "t", "kind": "Module", "file_path": null, "dependents": 9},
            {"id": "f-function", "name": "want_bytes", "kind": "Function",
             "file_path": "held/for/grouping/only.py", "dependents": 2,
             "signature": "def want_bytes(s)"},
        ]});
        assert_eq!(
            shown_signature("Module", Some("module src/itsdangerous/signer.py")),
            None
        );
        assert_eq!(
            shown_signature("package", Some("package src/x/__init__.py")),
            None
        );
        assert_eq!(
            shown_signature("Method", Some(" def sign(self) ")).as_deref(),
            Some("def sign(self)")
        );
        let relied_on = overview_relied_on(&inventory, Some(&census), 10).unwrap();
        assert_eq!(relied_on["excluded_without_definition"], 1);
        assert_eq!(relied_on["items"][0]["name"], "want_bytes");
        assert_eq!(relied_on["items"][0]["container"]["name"], "encoding");
        let mut with_untested = inventory.clone();
        with_untested["untested"] = json!({"total": 1, "entities": [
            {"id": "f-function", "name": "want_bytes", "kind": "Function",
             "file_path": "held/for/grouping/only.py", "degree": 3, "dependents": 2},
        ]});
        let untested = overview_untested(&with_untested, Some(&census)).unwrap();
        for answer in [
            Value::Object(overview_counts(&payload)),
            Value::Object(overview_containers(&census, 10)),
            Value::Object(relied_on),
            Value::Object(untested),
            build_map(&payload, MAP_NODE_CAP, MAP_EDGE_CAP),
        ] {
            let text = answer.to_string();
            assert!(!text.contains("held/for/grouping"), "{text}");
            assert!(
                !text.contains("file_path") && !text.contains("\"file\""),
                "{text}"
            );
        }
    }

    /// What no test reaches is the daemon's own reading, shaped for the page:
    /// each entity by id, name, kind, container and how many depend on it,
    /// with the whole count and what the list means. A daemon that does not
    /// report the reading is a gap said in one sentence, never a guessed
    /// list.
    #[test]
    fn the_untested_section_is_the_daemons_reading_or_its_gap_in_one_sentence() {
        let payload = fixture();
        let census = Census::new(&payload);
        let inventory = json!({"total": 2, "entities": [], "untested": {"total": 7, "entities": [
            {"id": "f-function", "name": "want_bytes", "kind": "Function",
             "file_path": "held/for/grouping/only.py", "degree": 3, "dependents": 2},
        ]}});
        let read = untested_section(overview_untested(&inventory, Some(&census)));
        assert_eq!(read["ok"], true);
        assert_eq!(read["total"], 7);
        assert_eq!(
            read["items"],
            json!([{
                "id": "f-function", "name": "want_bytes", "kind": "function",
                "container": census.container_of("f-function"), "dependents": 2,
            }])
        );
        assert_eq!(read["items"][0]["container"]["name"], "encoding");
        let basis = read["basis"].as_str().unwrap();
        assert!(
            basis.contains("no recorded test") && basis.contains("whole repository"),
            "{basis}"
        );

        // An older daemon answers the ranking without the reading.
        let older = json!({"total": 2, "entities": []});
        let gap = untested_section(overview_untested(&older, Some(&census)));
        assert_eq!(gap["ok"], false);
        assert!(gap.get("items").is_none() && gap.get("total").is_none());
        let reason = gap["reason"].as_str().unwrap();
        assert!(reason.starts_with("Kin could not read which entities no test reaches: "));
        assert_eq!(reason.matches(". ").count(), 0, "one sentence: {reason}");
        assert!(reason.ends_with('.'), "{reason}");
        assert!(!reason.contains('`'), "no command to run: {reason}");

        // A refusal that ends its own sentence still reads as one.
        let refused = untested_section(Err(anyhow::anyhow!("the daemon did not answer in time.")));
        assert_eq!(
            refused["reason"],
            "Kin could not read which entities no test reaches: the daemon did not answer in time."
        );
    }

    #[test]
    fn the_map_adds_up_relationships_between_containers_and_reports_true_totals() {
        let payload = fixture();
        let map = build_map(&payload, MAP_NODE_CAP, MAP_EDGE_CAP);
        assert_eq!(map["totals"]["containers"], 2);
        assert_eq!(map["totals"]["without_definition"], 1);
        // Two calls from the signer module's methods reach want_bytes.
        let between = map["edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|edge| edge["level"] == "container")
            .unwrap();
        assert_eq!(between["weight"], 2);
        assert_eq!(between["kinds"]["calls"], 2);
        let method = map["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == "c-method")
            .unwrap();
        assert_eq!(method["parent"], "b-class");
        assert_eq!(method["group"], "a-module");
        assert!(map["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|node| node["id"] != "g-import"));
        assert_eq!(map["sampled"], false);
        assert_eq!(map["directed"], false);
    }

    #[test]
    fn a_capped_map_says_it_was_sampled_and_keeps_its_totals() {
        let payload = fixture();
        let map = build_map(&payload, 3, 1);
        assert_eq!(map["shown"]["nodes"], 3);
        assert_eq!(map["edges"].as_array().unwrap().len(), 1);
        assert_eq!(map["totals"]["entities"], 7);
        assert_eq!(map["sampled"], true);
    }

    fn hit(id: &str, name: &str, by_name: bool, meaning: Option<Meaning>) -> Hit {
        Hit {
            id: id.into(),
            name: name.into(),
            kind: "method".into(),
            signature: None,
            summary: None,
            defined_here: true,
            by_name,
            meaning,
            dependents: None,
        }
    }

    /// An exact name leads, meaning keeps Kin's order, and an entity both
    /// searches found appears once with both ways it matched.
    #[test]
    fn ask_merges_both_searches_without_repeating_an_entity() {
        let meaning = vec![
            hit("1", "Signer.verify_signature", false, Some(Meaning::Words)),
            hit("2", "Signer.sign", false, Some(Meaning::Words)),
        ];
        let mut sign = hit("2", "Signer.sign", true, None);
        sign.summary = Some("Signs the given string.".into());
        let names = vec![hit("3", "TimestampSigner.sign_more", true, None), sign];
        let merged = merge_hits("sign", meaning, names, 10);
        let ids: Vec<&str> = merged.iter().map(|hit| hit.id.as_str()).collect();
        assert_eq!(ids, ["2", "1", "3"]);
        assert_eq!(merged[0].matched(), ["name", "meaning"]);
        assert_eq!(
            merged[0].summary.as_deref(),
            Some("Signs the given string.")
        );
        assert_eq!(merged[1].matched(), ["meaning"]);
        assert_eq!(
            merged[1].why(),
            "Its description and code share words with the question."
        );
        assert_eq!(
            merge_hits("sign", Vec::new(), vec![hit("3", "x", true, None)], 0).len(),
            0
        );
    }

    #[test]
    fn ask_refuses_an_empty_or_unbounded_question_before_the_daemon() {
        let ask = |q: &str, limit: Option<usize>| ask_request(&AskQuery { q: q.into(), limit });
        assert!(ask("  ", None).is_err());
        assert!(ask(&"x".repeat(257), None).is_err());
        assert!(ask("a\nb", None).is_err());
        assert_eq!(ask(" verify ", None).unwrap(), ("verify".into(), 20));
        assert_eq!(ask("verify", Some(500)).unwrap().1, ASK_LIMIT_MAX);
        assert_eq!(ask("verify", Some(0)).unwrap().1, 1);
    }

    #[test]
    fn a_primary_call_preserves_every_recorded_relationship_in_any_order() {
        for kinds in [
            ["UsesType", "Calls", "References"],
            ["UsesType", "References", "Calls"],
            ["References", "UsesType", "Calls"],
            ["References", "Calls", "UsesType"],
            ["Calls", "References", "UsesType"],
            ["Calls", "UsesType", "References"],
        ] {
            let incoming = json!({
                "truncated": false,
                "entities": [
                    {"id": "focal", "name": "normalizeRequestId", "kind": "Function"},
                    {"id": "caller", "name": "buildOptionalHeaderReqId", "kind": "Function"},
                ],
                "relations": kinds.iter().map(|kind| json!({
                    "direction": "incoming", "kind": kind, "resolution": "type_resolved",
                    "src": {"Entity": "caller"}, "dst": {"Entity": "focal"},
                })).collect::<Vec<_>>(),
            });
            let section =
                relied_on_by_section("focal", Some(&incoming), None, &HashMap::new()).unwrap();
            assert_eq!(section["total"], 1);
            assert_eq!(section["confirmed"], 1);
            let caller = &section["items"][0];
            assert_eq!(caller["relationship"], "calls");
            assert_eq!(
                caller["relationships"],
                json!(kinds.map(kind_label)),
                "the primary label must not discard or reorder the graph's facts"
            );
        }
    }

    #[test]
    fn who_relies_on_an_entity_reads_one_row_per_entity_with_its_sites() {
        let answer = json!({
            "_kin": {"completeness": {"bound": "at_least"}},
            "call_sites": {"callers_unproven_no_resolver": 3},
            "total_upstream": 2,
            "references": [
                {"entity_id": "t1", "name": "TestSigner.test_sign", "kind": "Method",
                 "relation_kinds": ["calls"], "role": "test", "site_count": 2},
                {"entity_id": "s1", "name": "Serializer", "kind": "Class",
                 "relation_kinds": ["references", "calls"], "role": "source", "site_count": 4,
                 "module": "itsdangerous.serializer",
                 "semantic_owners": [{"entity_id": "m1", "name": "serializer", "kind": "module"}]},
            ],
            "candidates": [
                {"entity_id": "d1", "name": "Serializer.dumps", "kind": "Method",
                 "relation_kinds": ["calls"], "role": "source", "site_count": 1},
            ]
        });
        // The graph also records a subclass override and a guessed call.
        let incoming = json!({
            "truncated": false,
            "entities": [
                {"id": "focal", "name": "Signer.sign", "kind": "Method"},
                {"id": "o1", "name": "TimestampSigner.sign", "kind": "Method"},
                {"id": "d1", "name": "Serializer.dumps", "kind": "Method"},
                {"id": "x1", "name": "TestTimed.test_max_age", "kind": "Method"},
            ],
            "relations": [
                {"direction": "incoming", "kind": "Overrides", "resolution": "type_resolved",
                 "src": {"Entity": "o1"}, "dst": {"Entity": "focal"}},
                {"direction": "incoming", "kind": "Calls", "resolution": "name_only",
                 "src": {"Entity": "d1"}, "dst": {"Entity": "focal"}},
                {"direction": "incoming", "kind": "Calls", "resolution": "name_only",
                 "src": {"Entity": "x1"}, "dst": {"Entity": "focal"}},
                {"direction": "incoming", "kind": "Contains", "resolution": "type_resolved",
                 "src": {"Entity": "c"}, "dst": {"Entity": "focal"}},
            ]
        });
        let tests: HashMap<String, bool> = [("x1".to_string(), true)].into();
        let section =
            relied_on_by_section("focal", Some(&incoming), Some(&answer), &tests).unwrap();
        let names: Vec<&str> = section["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["name"].as_str().unwrap())
            .collect();
        // Not tests first, proven before guessed, then by name.
        assert_eq!(
            names,
            [
                "Serializer",
                "TimestampSigner.sign",
                "Serializer.dumps",
                "TestSigner.test_sign",
                "TestTimed.test_max_age"
            ]
        );
        assert_eq!(section["total"], 5);
        assert_eq!(section["confirmed"], 3);
        assert_eq!(section["bound"], "at_least");
        let notice = section["notice"].as_str().unwrap();
        assert!(notice.starts_with(UNPROVEN_SITES), "{notice}");
        assert!(notice.ends_with(
            "2 of them match by name alone, and Kin could not prove they use this one."
        ));
        let serializer = &section["items"][0];
        assert_eq!(serializer["relationship"], "calls");
        assert_eq!(serializer["relationships"], json!(["references", "calls"]));
        assert_eq!(serializer["sites"], 4);
        assert_eq!(serializer["container"]["name"], "serializer");
        assert_eq!(serializer["confirmed"], true);
        assert_eq!(section["items"][1]["relationship"], "overrides");
        assert_eq!(section["items"][1]["is_test"], Value::Null);
        assert_eq!(section["items"][2]["confirmed"], false);
        assert_eq!(section["items"][4]["is_test"], true);

        // With only the references answer, the list stands and says what is missing.
        let alone = relied_on_by_section("focal", None, Some(&answer), &HashMap::new()).unwrap();
        assert_eq!(alone["total"], 3);
        assert_eq!(alone["bound"], "at_least");
        assert!(alone["notice"]
            .as_str()
            .unwrap()
            .starts_with("Kin could not read its other relationships"));
        let alone = relied_on_by_section("focal", Some(&incoming), None, &HashMap::new()).unwrap();
        assert!(alone["notice"]
            .as_str()
            .unwrap()
            .starts_with("Kin could not read its references"));
        assert!(relied_on_by_section("focal", None, None, &HashMap::new()).is_err());
    }

    /// fastify's `serialize`, as the entity page read it while Kin could not
    /// read its references: three callers proven and one test matched by
    /// name alone. The notice said "sites and matches by name are missing"
    /// and then "1 of them matches by name alone". It now says only what the
    /// reference read would have added, and the match by name alone is still
    /// said to be one.
    #[test]
    fn without_its_references_the_notice_says_only_what_the_references_add() {
        let incoming = json!({
            "truncated": false,
            "entities": [
                {"id": "focal", "name": "serialize", "kind": "Function"},
                {"id": "r1", "name": "Reply.serialize", "kind": "Method"},
                {"id": "t1", "name": "reply-serialize.test", "kind": "Module", "role": "test"},
            ],
            "relations": [
                {"direction": "incoming", "kind": "Calls", "resolution": "type_resolved",
                 "src": {"Entity": "r1"}, "dst": {"Entity": "focal"}},
                {"direction": "incoming", "kind": "Calls", "resolution": "name_only",
                 "src": {"Entity": "t1"}, "dst": {"Entity": "focal"}},
            ]
        });
        let section =
            relied_on_by_section("focal", Some(&incoming), None, &HashMap::new()).unwrap();
        assert_eq!(section["total"], 2);
        assert_eq!(section["confirmed"], 1);
        assert_eq!(section["relies_total"], 1);
        assert_eq!(
            section["notice"],
            "Kin could not read its references, so where each one uses it is not shown, and \
             there could be more. 1 of them matches by name alone, and Kin could not prove it \
             uses this one."
        );
    }

    #[test]
    fn an_entity_history_names_where_the_record_starts_while_history_is_partial() {
        let import = "9dda05f2fa165dc5274f2b047c582df107fd0caf118989243c0c10db08bb4eb6";
        let answer = json!({
            "revisions_total": 1, "unchanged_revisions": 0, "record_start": import,
            "revisions": [{"id": import, "title": "docs: clarify the threat model (#6537)",
                "author": "Matteo Collina <hello@example.com>",
                "timestamp": "2026-03-11T10:57:14+00:00"}]
        });
        let partial = history_section(&answer, true).unwrap();
        assert_eq!(partial["record_start"], import);
        // A store that holds the whole history began where the repository did.
        assert!(history_section(&answer, false)
            .unwrap()
            .get("record_start")
            .is_none());
        // Nor is it named when the oldest revision listed is not the start.
        let mut later = answer.clone();
        later["revisions_total"] = json!(2);
        assert!(history_section(&later, true)
            .unwrap()
            .get("record_start")
            .is_none());
    }

    /// The overload declarations of one callable read as one row, kept under
    /// the declaration with the most sites, never under the running sum.
    #[test]
    fn overload_declarations_read_as_one_row() {
        let owner = json!([{"entity_id": "c", "name": "Serializer", "kind": "class"}]);
        let row = |id: &str, sites: u64, owners: &Value| {
            json!({"entity_id": id, "name": "Serializer.__init__", "kind": "Method",
                   "relation_kinds": ["references"], "role": "source", "site_count": sites,
                   "semantic_owners": owners})
        };
        let elsewhere = json!([{"entity_id": "e", "name": "Other", "kind": "class"}]);
        let answer = json!({"_kin": {"completeness": {"bound": "exact"}}, "references": [
            row("a1", 2, &owner), row("a2", 2, &owner), row("a3", 4, &owner),
            row("z9", 1, &elsewhere),
        ]});
        let nothing_else = json!({"truncated": false, "entities": [], "relations": []});
        let folded =
            relied_on_by_section("focal", Some(&nothing_else), Some(&answer), &HashMap::new())
                .unwrap();
        assert_eq!(folded["items"].as_array().unwrap().len(), 2);
        assert_eq!(folded["shown"], 4);
        assert_eq!(folded["total"], 4);
        let one = &folded["items"][0];
        assert_eq!(one["id"], "a3");
        assert_eq!(one["declarations"], 3);
        assert_eq!(one["ids"], json!(["a1", "a2", "a3"]));
        assert_eq!(one["sites"], 8);
        assert!(folded["items"][1].get("declarations").is_none());
        assert_eq!(folded["bound"], "exact");
        assert!(folded.get("notice").is_none());
    }

    #[test]
    fn outgoing_relations_split_into_what_it_uses_and_what_it_contains() {
        let answer = json!({
            "truncated": false,
            "entities": [
                {"id": "focal", "name": "Signer", "kind": "Class"},
                {"id": "m", "name": "Signer.sign", "kind": "Method"},
                {"id": "w", "name": "want_bytes", "kind": "Function"},
            ],
            "relations": [
                {"direction": "outgoing", "kind": "Contains", "dst": {"Entity": "m"}, "src": {"Entity": "focal"}},
                {"direction": "outgoing", "kind": "Calls", "dst": {"Entity": "w"}, "src": {"Entity": "focal"}},
                {"direction": "outgoing", "kind": "Calls", "dst": {"Entity": "w"}, "src": {"Entity": "focal"}},
                {"direction": "outgoing", "kind": "Calls", "dst": {"External": "x"}, "src": {"Entity": "focal"}},
                {"direction": "incoming", "kind": "Calls", "src": {"Entity": "w"}, "dst": {"Entity": "focal"}},
            ]
        });
        let (relies_on, members) = outgoing_sections(&answer, "focal").unwrap();
        assert_eq!(relies_on["total"], 1);
        assert_eq!(relies_on["items"][0]["relationship"], "calls");
        assert_eq!(relies_on["bound"], "exact");
        assert_eq!(members["items"][0]["name"], "Signer.sign");
    }

    #[test]
    fn an_entity_header_reads_its_owner_and_its_source_its_start_line() {
        let source = json!({
            "id": "e", "name": "Signer", "kind": "Class", "language": "python",
            "signature": "class Signer", "module": "itsdangerous.signer", "body": "class Signer:\n    pass",
            "semantic_owners": [{"entity_id": "m", "name": "signer", "kind": "module"}],
        });
        let recorded = json!({"entity": {"name": "Signer", "kind": "class", "role": "source"},
            "body": {"state": "recorded", "text": "class Signer:\n    pass", "start_line": 76, "working": true}});
        let header = header_section(
            Some(&source),
            Some(&recorded["entity"]),
            Some("A signer.".into()),
        )
        .unwrap();
        assert_eq!(header["kind"], "class");
        assert_eq!(header["container"]["id"], "m");
        assert_eq!(header["role"], "source");
        assert_eq!(header["summary"], "A signer.");
        let text = source_section(Some(&recorded), Some(&source)).unwrap();
        assert_eq!(text["start_line"], 76);
        assert_eq!(text["end_line"], 77);
        assert_eq!(text["language"], "python");
        // The plain source read stands in when the guide's read did not answer.
        let fallback = source_section(None, Some(&source)).unwrap();
        assert_eq!(fallback["start_line"], Value::Null);
        // A recorded gap is reported, never filled.
        let gap = json!({"body": {"state": "unavailable", "reason": "entity e is not in the working tree"}});
        assert!(source_section(Some(&gap), None).is_err());
    }

    /// The exact read carries the digest of the span it cut, the one a guide
    /// step reports, so the page can tell whether a flag set on a recorded
    /// change is still about the entity as it is now. A read that is not an
    /// exact span carries none, and nothing malformed is passed on.
    #[test]
    fn the_source_section_carries_the_exact_reads_digest_and_no_other() {
        let text = "class Signer:\n    pass";
        let digest = kin_blobs::digest(text.as_bytes()).to_string();
        let source = json!({"id": "e", "name": "Signer", "kind": "Class", "language": "python", "body": text});
        let recorded = json!({"body": {
            "state": "recorded", "text": text, "start_line": 76, "working": true,
            "body_hash": digest,
        }});
        let exact = source_section(Some(&recorded), Some(&source)).unwrap();
        assert_eq!(exact["body_hash"], json!(digest));

        // The plain source read is not an exact span, so it has no digest to
        // give, whether the guide's read was absent or refused.
        let fallback = source_section(None, Some(&source)).unwrap();
        assert!(fallback.get("body_hash").is_none(), "{fallback:?}");
        let refused = json!({"body": {"state": "no_source", "body_hash": digest}});
        let fallback = source_section(Some(&refused), Some(&source)).unwrap();
        assert!(fallback.get("body_hash").is_none(), "{fallback:?}");

        // A recorded read with no digest, or one that is not a whole digest,
        // passes none on.
        for body_hash in [Value::Null, json!("abc123"), json!(42)] {
            let mut read = recorded.clone();
            read["body"]["body_hash"] = body_hash;
            let section = source_section(Some(&read), Some(&source)).unwrap();
            assert!(section.get("body_hash").is_none(), "{section:?}");
        }
    }

    #[test]
    fn a_graph_tool_refusal_reads_as_its_sentence() {
        let refusal = anyhow::anyhow!(
            r#"get_entity_source: {{"binding_coverage":{{}},"message":"no entity exists with ID 'x'."}}"#
        );
        assert_eq!(
            gap("Kin could not read this entity.", &refusal),
            "Kin could not read this entity. No entity exists with ID 'x'."
        );
        assert_eq!(
            readable("plain refusal {not json"),
            "plain refusal {not json"
        );
    }

    #[test]
    fn an_unknown_entity_is_told_apart_from_one_without_text() {
        let absent = json!({"body": {"state": "unavailable",
            "reason": "entity 11111111-1111-4111-8111-111111111111 is not in the working tree"}});
        assert!(is_absent(&absent));
        let textless = json!({"entity": {"name": "t", "kind": "module"},
            "body": {"state": "no_source", "reason": "a module placeholder holds no text"}});
        assert!(!is_absent(&textless));
    }

    #[test]
    fn an_entity_request_names_the_sections_it_wants() {
        assert_eq!(entity_sections(None).unwrap(), ENTITY_SECTIONS.to_vec());
        assert_eq!(
            entity_sections(Some(" ")).unwrap(),
            ENTITY_SECTIONS.to_vec()
        );
        assert_eq!(
            entity_sections(Some("impact, history,impact")).unwrap(),
            ["impact", "history"]
        );
        assert!(entity_sections(Some("header,files"))
            .unwrap_err()
            .starts_with("Unknown section 'files'."));
    }

    #[test]
    fn a_header_summary_is_the_first_sentence_of_the_recorded_doc() {
        assert_eq!(
            first_sentence("A signer securely signs bytes. The secret key should be random."),
            "A signer securely signs bytes."
        );
        assert_eq!(
            first_sentence("Signs the given string."),
            "Signs the given string."
        );
        let long = first_sentence(&"word ".repeat(100));
        assert!(long.chars().count() <= 281 && long.ends_with('\u{2026}'));
    }

    #[test]
    fn entity_history_names_authors_without_their_address() {
        let answer = json!({
            "revisions_total": 3, "unchanged_revisions": 6,
            "revisions": [{"id": "83a2", "title": "support FIPS builds without SHA-1 (#378)",
                "author": "Grace Hopper <grace@example.com> 1713298162 -0700",
                "timestamp": "2024-04-16T20:09:22+00:00"}]
        });
        let section = history_section(&answer, false).unwrap();
        assert_eq!(section["items"][0]["author"], "Grace Hopper");
        assert_eq!(section["items"][0]["time"], "2024-04-16T20:09:22+00:00");
        assert_eq!(section["total"], 3);
        assert_eq!(section["unchanged"], 6);
        assert!(!Value::Object(section).to_string().contains('@'));
    }

    #[test]
    fn a_failed_section_says_why_and_the_rest_stand() {
        let failed = section(
            "Kin could not read this entity's history.",
            Err(anyhow::anyhow!("it did not answer within 8 seconds")),
        );
        assert_eq!(failed["ok"], false);
        assert_eq!(
            failed["reason"],
            "Kin could not read this entity's history. It did not answer within 8 seconds."
        );
        let answered = section("unused", Ok(fields(json!({"total": 0}))));
        assert_eq!(answered["ok"], true);
        assert_eq!(answered["total"], 0);
    }

    const SEND: &str = "7fe6951b-4071-4e59-a4d8-83ebecc63e4b";
    const HOOK: &str = "4086b425-a85e-4470-9e4f-7207dfdadbe6";
    const HOOK_END: &str = "15ca387d-91f0-4258-8053-ab6e664a38e1";
    const SERIALIZE: &str = "30677f6b-2a16-459e-a1da-ec3a66235429";
    const REDIRECT: &str = "c9620963-28ef-4698-9c11-5982e0e971f2";
    const OTHER: &str = "47460d30-4af2-4faa-888b-191f7804efd7";

    fn terms(question: &str) -> Vec<String> {
        question_terms(question)
    }

    fn entity(id: &str, name: &str) -> Link {
        Link {
            id: id.into(),
            name: name.into(),
            kind: if name.contains('.') {
                "method"
            } else {
                "function"
            }
            .into(),
        }
    }

    /// One entity's incoming relations, as the daemon answers them: each
    /// caller with how its call was matched.
    fn incoming(callers: &[(&str, &str, &str, &str)], truncated: bool) -> Value {
        let entities: Vec<Value> = callers
            .iter()
            .map(|(id, name, kind, _)| {
                json!({"id": id, "name": name, "kind": kind,
                       "signature": "held/for/grouping/only.js"})
            })
            .collect();
        let mut relations: Vec<Value> = callers
            .iter()
            .map(|(id, _, _, resolution)| {
                json!({"direction": "incoming", "kind": "Calls", "resolution": resolution,
                       "src": {"Entity": id}, "dst": {"Entity": SERIALIZE}})
            })
            .collect();
        // What holds the entity, and its own recursion, call nothing into it.
        relations.push(json!({"direction": "incoming", "kind": "Contains",
            "resolution": "type_resolved", "src": {"Entity": OTHER}, "dst": {"Entity": SERIALIZE}}));
        relations.push(json!({"direction": "incoming", "kind": "Calls",
            "resolution": "type_resolved", "src": {"Entity": SERIALIZE},
            "dst": {"Entity": SERIALIZE}}));
        json!({"truncated": truncated, "entities": entities, "relations": relations})
    }

    /// The calls above `serialize` in a web framework: a method that calls
    /// it directly, and the hook chain a reply's send runs through.
    fn serialize_climb() -> (Link, Climb) {
        let climb = Climb {
            above: vec![
                (entity(OTHER, "Reply.serialize"), SERIALIZE.into()),
                (
                    entity(HOOK_END, "preSerializationHookEnd"),
                    SERIALIZE.into(),
                ),
                (entity(HOOK, "preSerializationHook"), HOOK_END.into()),
                (entity(SEND, "Reply.send"), HOOK.into()),
                (entity(REDIRECT, "Reply.redirect"), SEND.into()),
            ],
            callers: HashMap::new(),
        };
        (entity(SERIALIZE, "serialize"), climb)
    }

    #[test]
    fn only_a_question_that_asks_how_or_where_is_given_an_answer() {
        assert_eq!(asks("how does a reply get serialized"), Some(Asks::How));
        assert_eq!(asks("Where are errors formatted?"), Some(Asks::Where));
        assert_eq!(asks("show me how a route is registered"), Some(Asks::How));
        assert_eq!(asks("how many routes are registered"), None);
        assert_eq!(asks("Reply.send"), None);
        assert_eq!(asks("what calls serialize"), None);
        assert_eq!(asks("list every entity that says where it is used"), None);
    }

    #[test]
    fn a_question_keeps_the_words_that_could_name_something() {
        assert_eq!(
            terms("how does a reply get serialized"),
            ["repli", "serializ"]
        );
        assert_eq!(terms("Where are errors formatted?"), ["error", "format"]);
        assert_eq!(terms("how is a 404 handled"), ["404", "handl"]);
        assert_eq!(terms("how is a reply sent"), ["repli", "send"]);
        assert_eq!(
            terms("how does contentTypeParser work"),
            ["content", "type", "pars"]
        );
        assert!(terms("how does it work").is_empty());
    }

    #[test]
    fn the_forms_of_a_word_meet_in_a_name() {
        let all = |question: &str, name: &str| {
            let terms = terms(question);
            matched_terms(name, &terms).len() == terms.len()
        };
        assert!(all("serialized replies", "Reply.serializeInput"));
        assert!(all("serialization", "preSerializationHookEnd"));
        assert!(all("registered", "registerPluginName"));
        assert!(all("routing", "fastify.<locals>.fastify.route"));
        assert!(all("formatted errors", "validateSchemaErrorFormatter"));
        assert!(all("cookies", "parseCookie"));
        assert!(all("HTTP2 server", "createHTTP2Server"));
        assert!(!all("serialized", "Reply.send"));
        // A short word is matched whole, never as the start of a longer one.
        assert!(!all("id", "identity"));
        assert!(!all("run", "runtime"));
    }

    #[test]
    fn what_calls_an_entity_is_read_from_its_incoming_relations() {
        let read = callers_of(
            &incoming(
                &[
                    (
                        HOOK_END,
                        "preSerializationHookEnd",
                        "Function",
                        "type_resolved",
                    ),
                    (OTHER, "Reply.serialize", "Method", "type_resolved"),
                    // Two call sites in one caller are one caller.
                    (OTHER, "Reply.serialize", "Method", "type_resolved"),
                    // A call matched by name alone, and a caller that is no
                    // function, call it but are no hop of a chain.
                    (SEND, "handleRequest", "Function", "name_only"),
                    (REDIRECT, "reply.test", "Module", "type_resolved"),
                ],
                false,
            ),
            SERIALIZE,
        );
        let names: Vec<&str> = read.proven.iter().map(|link| link.name.as_str()).collect();
        assert_eq!(names, ["Reply.serialize", "preSerializationHookEnd"]);
        assert_eq!(read.proven[0].kind, "method");
        assert_eq!((read.all, read.cut), (4, false));
        assert_eq!(read.only(), None);

        let one = [(HOOK, "buildRouting", "Function", "type_resolved")];
        assert_eq!(
            callers_of(&incoming(&one, false), SERIALIZE).only(),
            Some(&entity(HOOK, "buildRouting"))
        );
        // One caller of a read that was cut short may not be the only one,
        // and one caller matched by name alone is not known to call it.
        assert_eq!(callers_of(&incoming(&one, true), SERIALIZE).only(), None);
        let guessed = [(HOOK, "buildRouting", "Function", "name_only")];
        assert_eq!(
            callers_of(&incoming(&guessed, false), SERIALIZE).only(),
            None
        );
        assert_eq!(
            callers_of(&incoming(&[], false), SERIALIZE),
            Callers::default()
        );
    }

    #[test]
    fn a_chain_begins_and_ends_at_entities_the_question_names() {
        let (target, climb) = serialize_climb();
        let asked = terms("how does a reply get serialized");
        let (basis, chain) = chain_from(&target, &climb, &asked, false).expect("a chain");
        assert_eq!(basis, "named_ends");
        let names: Vec<&str> = chain.iter().map(|link| link.name.as_str()).collect();
        // The nearest caller that adds a word the last entity lacks:
        // `Reply.serialize` is one call away, which is not a chain, and
        // `Reply.redirect` reaches the same place by a longer way.
        assert_eq!(
            names,
            [
                "Reply.send",
                "preSerializationHook",
                "preSerializationHookEnd",
                "serialize"
            ]
        );
        let answer = answer_json(Asks::How, basis, &chain);
        assert_eq!(
            answer["sentence"],
            "Reply.send calls preSerializationHook, which calls preSerializationHookEnd, which \
             calls serialize."
        );
        assert_eq!(answer["asks"], "how");
        assert_eq!(
            answer["steps"][0],
            json!({
                "id": SEND, "name": "Reply.send", "kind": "method", "relation_to_next": "calls",
            })
        );
        assert_eq!(answer["steps"][3]["relation_to_next"], Value::Null);
        assert_eq!(answer["steps"][3]["kind"], "function");
        let text = answer.to_string();
        assert!(
            !text.contains("held/for/grouping") && !text.contains("file"),
            "{text}"
        );
        assert_eq!(
            answer_json(Asks::Where, basis, &chain)["sentence"],
            "This happens in serialize, which Reply.send reaches through preSerializationHook \
             and preSerializationHookEnd."
        );
    }

    #[test]
    fn a_question_the_names_do_not_carry_is_given_no_chain() {
        let (target, climb) = serialize_climb();
        // Nothing on any chain is named for a route.
        assert!(chain_from(&target, &climb, &terms("how is a route serialized"), false).is_none());
        // One word names a topic. It ties no chain to a question.
        assert!(question_terms("how does serialization work").len() < ANSWER_MIN_TERMS);
        // Nothing is known to call the result.
        let asked = terms("how does a reply get serialized");
        assert!(chain_from(&target, &Climb::default(), &asked, false).is_none());
    }

    #[test]
    fn a_test_is_never_where_a_chain_begins_unless_tests_are_asked_about() {
        let (target, mut climb) = serialize_climb();
        // The only caller named for a reply is a test of it.
        climb.above[3].0.name = "test_reply_send".into();
        climb.above[4].0.name = "ReplySpec.redirects".into();
        let asked = terms("how does a reply get serialized");
        assert!(chain_from(&target, &climb, &asked, false).is_none());
        assert!(!names_a_test("how does a reply get serialized"));
        let about_tests = "how does a reply test get serialized";
        assert!(names_a_test(about_tests));
        assert!(chain_from(&target, &climb, &terms(about_tests), true).is_some());
        assert!(names_a_test("TestTimed.test_max_age") && !names_a_test("attestation"));
    }

    /// `compileSchemas` is called only by `buildRouting`, which is called
    /// only by `fastify`, which several entities call.
    fn sole_climb() -> (Link, Climb) {
        let only = |caller: Link| Callers {
            proven: vec![caller],
            all: 1,
            cut: false,
        };
        let callers = HashMap::from([
            (SERIALIZE.to_string(), only(entity(HOOK, "buildRouting"))),
            (HOOK.to_string(), only(entity(SEND, "fastify"))),
            (
                SEND.to_string(),
                Callers {
                    proven: Vec::new(),
                    all: 3,
                    cut: false,
                },
            ),
        ]);
        let climb = Climb {
            above: vec![
                (entity(HOOK, "buildRouting"), SERIALIZE.into()),
                (entity(SEND, "fastify"), HOOK.into()),
            ],
            callers,
        };
        (entity(SERIALIZE, "compileSchemas"), climb)
    }

    #[test]
    fn an_entity_named_by_the_whole_question_is_reached_through_its_only_callers() {
        let asked = terms("where are schemas compiled");
        let (target, climb) = sole_climb();
        let (basis, chain) = chain_from(&target, &climb, &asked, false).expect("a chain");
        assert_eq!(basis, "sole_callers");
        let names: Vec<&str> = chain.iter().map(|link| link.name.as_str()).collect();
        assert_eq!(names, ["fastify", "buildRouting", "compileSchemas"]);
        assert_eq!(
            answer_json(Asks::Where, basis, &chain)["sentence"],
            "This happens in compileSchemas, which fastify reaches through buildRouting."
        );

        // A second caller anywhere on the way leaves a choice, so no chain.
        let (_, mut two) = sole_climb();
        two.callers.get_mut(HOOK).unwrap().all = 2;
        assert!(chain_from(&target, &two, &asked, false).is_none());
        // So does a caller list that was not read whole.
        let (_, mut cut) = sole_climb();
        cut.callers.get_mut(SERIALIZE).unwrap().cut = true;
        assert!(chain_from(&target, &cut, &asked, false).is_none());
        // And one call is not a chain.
        let (_, mut short) = sole_climb();
        short.callers.remove(HOOK);
        assert!(chain_from(&target, &short, &asked, false).is_none());
    }

    #[test]
    fn a_long_chain_is_said_in_one_sentence() {
        let chain: Vec<Link> = [
            "handleRequest",
            "handler",
            "preValidationCallback",
            "validate",
            "wrap",
        ]
        .iter()
        .map(|name| Link {
            id: SEND.into(),
            name: (*name).into(),
            kind: "function".into(),
        })
        .collect();
        assert_eq!(
            answer_sentence(Asks::How, &chain),
            "handleRequest reaches wrap through handler, preValidationCallback and validate, \
             each calling the next."
        );
        assert_eq!(
            answer_sentence(Asks::How, &chain[..3]),
            "handleRequest calls handler, which calls preValidationCallback."
        );
    }

    #[test]
    fn an_answer_ends_only_at_a_ranked_function_or_method_defined_here() {
        let mut function = hit(SERIALIZE, "serialize", false, Some(Meaning::Words));
        function.kind = "function".into();
        function.defined_here = true;
        let mut outside = function.clone();
        outside.id = OTHER.into();
        outside.defined_here = false;
        let mut class = function.clone();
        class.id = SEND.into();
        class.kind = "class".into();
        let mut unrelated = function.clone();
        unrelated.id = HOOK.into();
        unrelated.name = "fallbackErrorHandler".into();
        let hits = [outside, class, unrelated, function];
        let seeds = answer_seeds(&hits, &terms("how does a reply get serialized"));
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].id, SERIALIZE);
    }

    /// The explore routes refuse a malformed request before resolving a
    /// daemon, so these never start or reach one.
    #[tokio::test]
    async fn malformed_explore_requests_are_refused_before_any_daemon_read() {
        let listener = super::super::review_ui::bind(Some(0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let catalog = super::super::workspace_ui::empty_catalog();
        let base = "/repositories/only".to_string();
        let state = Arc::new(UiState::workspace(
            kin_core::KinLayout::new(std::env::temp_dir().join("kin-ui-explore/.kin")),
            port,
            base.clone(),
            catalog,
        ));
        let router = Router::new().nest(&base, super::super::review_ui::workspace_router(state));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        let origin = format!("http://127.0.0.1:{port}{base}");
        for (path, reason) in [
            ("/api/workspace/ask?q=", "Ask needs a question or a name."),
            (
                "/api/workspace/ask?q=%0A",
                "Ask needs a question or a name.",
            ),
            (
                "/api/workspace/entity?id=Signer",
                "An entity UUID is required.",
            ),
            (
                "/api/workspace/entity?id=e42b33a0-5812-4e86-a8e7-db0f26bf5266&sections=files",
                "Unknown section 'files'. Sections are header, source, relied_on_by, relies_on, members, tests, impact, history, editor.",
            ),
        ] {
            let refused = client.get(format!("{origin}{path}")).send().await.unwrap();
            assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{path}");
            let body: Value = refused.json().await.unwrap();
            assert_eq!(body["error"], reason, "{path}");
        }
        server.abort();
    }
}
