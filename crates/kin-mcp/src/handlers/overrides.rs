// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Where a method is overridden, read off the graph's `Overrides` edges.
//!
//! An override declares the method again rather than calling it, so no
//! reference edge reaches the method from it and a reference list is correctly
//! silent about it. It is still the first thing that has to change when the
//! method's signature changes: asked which functions must change if
//! `SessionInterface.open_session` gains a required parameter, a reply that
//! listed the two callers and none of the three overrides answered two fifths
//! of the question while reading as the whole of it.
//!
//! The rows here are published under their own key, beside the references and
//! never counted in them, for the reason the interface implementation block
//! gives: folding declarations into "who references this" would both lie about
//! what an edge proves and pad the answer to one question with the answer to
//! another.

use std::collections::{HashMap, HashSet};

use kin_index::RelationResolution;
use kin_model::entity::{Entity, EntityKind};
use kin_model::graph::GraphStore;
use kin_model::ids::{EntityId, LanguageId};
use kin_model::relation::{GraphNodeId, RelationKind};

use crate::error::{McpError, Result};

/// The key the overriding methods are published under on a `find_references`
/// reply.
pub(crate) const OVERRIDDEN_BY_KEY: &str = "overridden_by";

/// The array inside [`OVERRIDDEN_BY_KEY`] that holds one row per override. The
/// reference page transport addresses it as `overridden_by.overriders`.
pub(crate) const OVERRIDERS_KEY: &str = "overriders";

/// One method that overrides the focal, directly or through another override.
#[derive(Debug, Clone)]
pub(crate) struct OverrideRow {
    pub entity: Entity,
    /// The method this one overrides, named only when it is not the focal:
    /// an override of an override says which link of the chain it hangs off.
    pub overrides: Option<String>,
    /// How firmly the edge that found this row settled its base, on the same
    /// ladder a reference row reports.
    pub resolution: RelationResolution,
    /// The row implements a declared contract member rather than overriding
    /// an inherited one.
    pub implements: bool,
    /// The dotted module the overriding method is declared in.
    pub module: Option<String>,
    /// Whether another declaration of its source unit carries its name, so
    /// its row is written with its id
    /// ([`crate::semantic_owners::NAME_SHARED_KEY`]).
    pub name_shared: bool,
}

/// Every override of one method, in breadth-first order.
#[derive(Debug, Clone, Default)]
pub(crate) struct Overriders {
    pub rows: Vec<OverrideRow>,
    /// The walk stopped at a bound before it ran out of overrides.
    pub bounded: bool,
    /// Classes the parser saw but did not admit as entities whose declared
    /// bases name the focal's owner, so each could override the focal with no
    /// entity to carry the edge. Named rather than guessed about.
    pub not_admitted: Vec<serde_json::Value>,
    /// The focal's file was parsed by an adapter that takes no census of the
    /// definitions it leaves out, so an override declared where that adapter
    /// does not look cannot be ruled out.
    pub census_unmeasured: bool,
    /// What the repository-wide census of the owner's recorded subclasses
    /// found, when it ran within its bounds.
    pub repository: Option<RepositoryCensus>,
    /// The callers of the overrides that are not callers of the focal, when
    /// the reply reads them. A focused override answer does not.
    pub callers: Option<kin_review::OverrideCallers>,
}

/// The repository-wide check behind `repository_completeness`.
#[derive(Debug, Clone, Default)]
pub(crate) struct RepositoryCensus {
    /// Recorded subclasses and implementers of the focal's owner, transitively.
    pub subclasses: usize,
    /// Recorded classes whose declared bases were read.
    pub classes_read: usize,
    /// Same-named methods declared by a recorded subclass with no override
    /// edge to the focal: each could be an override the closure missed.
    pub unlisted: Vec<serde_json::Value>,
    /// Recorded classes that spell the owner's or a subclass's name among
    /// their bases with no recorded inheritance edge to it, and that declare a
    /// member named like the target: each could hold an override the closure
    /// missed.
    pub unlinked: Vec<serde_json::Value>,
    /// Classes that spell a base the same way but declare no member named
    /// like the target. Counted, not listed: none could hold an override
    /// except through a member Kin does not hold, and the count never makes
    /// override coverage complete.
    pub unlinked_without_member: usize,
}

impl RepositoryCensus {
    fn complete(&self) -> bool {
        self.unlisted.is_empty() && self.unlinked.is_empty() && self.unlinked_without_member == 0
    }
}

/// Where a census counts the classes that spell a base without an edge and
/// declare no member named like the target.
pub(crate) const UNLINKED_WITHOUT_MEMBER_KEY: &str = "classes_spelling_base_without_member";

/// Whether `class` declares a member whose own name is `leaf`.
fn declares_member_named<G: GraphStore>(store: &G, class: &Entity, leaf: &str) -> Result<bool> {
    for rel in
        kin_index::relation_read::relations_for_read(store, &class.id).map_err(McpError::graph)?
    {
        if rel.kind != RelationKind::Contains || rel.src != GraphNodeId::Entity(class.id) {
            continue;
        }
        let Some(member) = rel.dst.as_entity() else {
            continue;
        };
        if let Some(member) = store.get_entity(&member).map_err(McpError::graph)? {
            if leaf_name(&member.name) == leaf {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Bounds on the repository-wide census. Past either, completeness stays
/// unmeasured rather than partly measured.
const MAX_SUBCLASS_CLOSURE: usize = 4096;
const MAX_CLASSES_READ: usize = 100_000;

/// Whether `signature`, a class header such as `class B(A):` or
/// `class B extends A implements C`, spells `base` as a whole word after the
/// class's own name.
fn spells_base(signature: &str, class_leaf: &str, base: &str) -> bool {
    let after = signature
        .find(class_leaf)
        .map_or(signature, |at| &signature[at + class_leaf.len()..]);
    after
        .split(|ch: char| !(ch == '_' || ch == '$' || ch.is_alphanumeric()))
        .any(|word| word == base)
}

/// Census the recorded classes that could hold an override of `target`.
///
/// Walks incoming `Extends` and `Implements` edges from the target's owner to
/// every recorded subclass, and checks that each subclass declaring a method
/// of the target's name is one `found` lists. Then reads every recorded
/// class's declared bases for a spelling of the owner's or a subclass's name
/// that has no inheritance edge behind it, which is where an unresolved base
/// would hide. `None` when the owner is unknown or a bound is reached.
fn repository_census<G: GraphStore>(
    store: &G,
    target: &Entity,
    found: &Overriders,
) -> Result<Option<RepositoryCensus>> {
    // The census reads bases off class headers, so it runs only where a class
    // header spells them: Rust's `impl Trait for T` and Go's structural
    // interfaces declare no base in the type's own header.
    let header_spells_bases = matches!(
        target.language,
        LanguageId::Python
            | LanguageId::JavaScript
            | LanguageId::TypeScript
            | LanguageId::Java
            | LanguageId::Kotlin
            | LanguageId::CSharp
    );
    if found.bounded || !header_spells_bases {
        return Ok(None);
    }
    let Some(owner) = owner_of(store, &target.id)? else {
        return Ok(None);
    };
    let method_leaf = leaf_name(&target.name).to_string();
    let listed: HashSet<EntityId> = found.rows.iter().map(|row| row.entity.id).collect();
    let mut closure: HashMap<EntityId, Entity> = HashMap::new();
    let mut frontier = vec![owner.id];
    let mut census = RepositoryCensus::default();
    while let Some(current) = frontier.pop() {
        for rel in kin_index::relation_read::relations_for_read(store, &current)
            .map_err(McpError::graph)?
        {
            if !matches!(rel.kind, RelationKind::Extends | RelationKind::Implements)
                || rel.dst != GraphNodeId::Entity(current)
            {
                continue;
            }
            let Some(child) = rel.src.as_entity() else {
                continue;
            };
            if child == owner.id || closure.contains_key(&child) {
                continue;
            }
            let Some(entity) = store.get_entity(&child).map_err(McpError::graph)? else {
                continue;
            };
            if closure.len() >= MAX_SUBCLASS_CLOSURE {
                return Ok(None);
            }
            closure.insert(child, entity);
            frontier.push(child);
        }
    }
    census.subclasses = closure.len();
    for (id, subclass) in &closure {
        for rel in
            kin_index::relation_read::relations_for_read(store, id).map_err(McpError::graph)?
        {
            if rel.kind != RelationKind::Contains || rel.src != GraphNodeId::Entity(*id) {
                continue;
            }
            let Some(member) = rel.dst.as_entity() else {
                continue;
            };
            if listed.contains(&member) {
                continue;
            }
            let Some(method) = store.get_entity(&member).map_err(McpError::graph)? else {
                continue;
            };
            if overridable(&method) && leaf_name(&method.name) == method_leaf {
                let mut row = serde_json::json!({
                    "entity_id": method.id,
                    "name": method.name,
                    "class": subclass.name,
                    "module": crate::semantic_owners::entity_module(store, &method),
                });
                crate::semantic_owners::mark_shared_name(store, &method, &mut row);
                census.unlisted.push(row);
            }
        }
    }
    let mut bases: HashSet<String> = HashSet::from([leaf_name(&owner.name).to_string()]);
    bases.extend(
        closure
            .values()
            .map(|class| leaf_name(&class.name).to_string()),
    );
    let page = store
        .query_entities_page(
            &kin_model::graph::EntityFilter {
                kinds: Some(vec![EntityKind::Class, EntityKind::Interface]),
                ..Default::default()
            },
            &kin_model::graph::EntityPage::first(MAX_CLASSES_READ),
        )
        .map_err(McpError::graph)?;
    if page.total_matching > MAX_CLASSES_READ || page.next_offset.is_some() {
        return Ok(None);
    }
    census.classes_read = page.entities.len();
    for class in page.entities {
        if class.id == owner.id || closure.contains_key(&class.id) {
            continue;
        }
        let class_leaf = leaf_name(&class.name);
        let spelled: Vec<&String> = bases
            .iter()
            .filter(|base| {
                base.as_str() != class_leaf && spells_base(&class.signature, class_leaf, base)
            })
            .collect();
        if spelled.is_empty() {
            continue;
        }
        // A base spelled like the owner but bound to another class of that
        // name is that class's subclass, not an unresolved one.
        let mut resolved_elsewhere: HashSet<String> = HashSet::new();
        for rel in kin_index::relation_read::relations_for_read(store, &class.id)
            .map_err(McpError::graph)?
        {
            if matches!(rel.kind, RelationKind::Extends | RelationKind::Implements)
                && rel.src == GraphNodeId::Entity(class.id)
            {
                if let Some(base) = rel.dst.as_entity() {
                    if let Some(base) = store.get_entity(&base).map_err(McpError::graph)? {
                        resolved_elsewhere.insert(leaf_name(&base.name).to_string());
                    }
                }
            }
        }
        if spelled
            .iter()
            .all(|base| resolved_elsewhere.contains(base.as_str()))
        {
            continue;
        }
        // A class that declares nothing named like the target holds no
        // override Kin could list; it is counted rather than listed.
        if !declares_member_named(store, &class, &method_leaf)? {
            census.unlinked_without_member += 1;
            continue;
        }
        let mut row = serde_json::json!({
            "entity_id": class.id,
            "name": class.name,
            "module": crate::semantic_owners::entity_module(store, &class),
        });
        crate::semantic_owners::mark_shared_name(store, &class, &mut row);
        census.unlinked.push(row);
    }
    Ok(Some(census))
}

/// Walk incoming `Overrides` edges from `focal` through the graph's read view,
/// which drops an override attributed to an inferred member, so this block and
/// `impact_analysis`'s `affected_overrides` bucket read the same edges.
///
/// Bounded by the impact walk's own constants, so the two surfaces cannot
/// disagree about how deep a chain is followed.
pub(crate) fn collect_overriders<G: GraphStore>(store: &G, focal: &EntityId) -> Result<Overriders> {
    let mut seen: HashSet<EntityId> = HashSet::from([*focal]);
    let mut names: HashMap<EntityId, String> = HashMap::new();
    let mut out = Overriders::default();
    let mut frontier = vec![*focal];
    for _ in 0..kin_review::impact::OVERRIDE_CLOSURE_DEPTH {
        let mut next = Vec::new();
        let level_start = out.rows.len();
        for current in frontier {
            for rel in kin_index::relation_read::relations_for_read(store, &current)
                .map_err(McpError::graph)?
            {
                if !matches!(rel.kind, RelationKind::Overrides | RelationKind::Implements)
                    || rel.dst != GraphNodeId::Entity(current)
                {
                    continue;
                }
                let Some(child) = rel.src.as_entity() else {
                    continue;
                };
                if seen.contains(&child) {
                    continue;
                }
                let Some(entity) = store.get_entity(&child).map_err(McpError::graph)? else {
                    continue;
                };
                if !kin_review::impact::is_member_redeclaration(&rel, &entity) {
                    continue;
                }
                seen.insert(child);
                if out.rows.len() >= kin_review::impact::OVERRIDE_CLOSURE_MAX {
                    out.bounded = true;
                    return Ok(out);
                }
                names.insert(child, entity.name.clone());
                out.rows.push(OverrideRow {
                    overrides: (current != *focal).then(|| names[&current].clone()),
                    resolution: RelationResolution::of(&rel),
                    implements: rel.kind == RelationKind::Implements,
                    module: crate::semantic_owners::entity_module(store, &entity),
                    name_shared: crate::semantic_owners::name_is_shared(store, &entity),
                    entity,
                });
                next.push(child);
            }
        }
        // Edges come off the store in relation-id order, which carries no
        // meaning. Each level is listed by file, then name, then id, so two
        // reads of one graph list one answer the same way.
        out.rows[level_start..].sort_by(|left, right| {
            let key = |row: &OverrideRow| {
                (
                    row.entity.file_origin.as_ref().map(|path| path.0.clone()),
                    row.entity.name.clone(),
                    row.entity.id,
                )
            };
            key(left).cmp(&key(right))
        });
        if next.is_empty() {
            return Ok(out);
        }
        frontier = next;
    }
    // Levels remained when the depth bound was reached.
    out.bounded = frontier_has_more(store, &out)?;
    Ok(out)
}

/// Whether any row at the deepest level itself has an override the walk did
/// not follow. Checked rather than assumed, so a chain that ends exactly at
/// the bound is not reported as cut.
fn frontier_has_more<G: GraphStore>(store: &G, found: &Overriders) -> Result<bool> {
    let known: HashSet<EntityId> = found.rows.iter().map(|row| row.entity.id).collect();
    for row in &found.rows {
        for rel in kin_index::relation_read::relations_for_read(store, &row.entity.id)
            .map_err(McpError::graph)?
        {
            if matches!(rel.kind, RelationKind::Overrides | RelationKind::Implements)
                && rel.dst == GraphNodeId::Entity(row.entity.id)
                && rel.src.as_entity().is_some_and(|id| !known.contains(&id))
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The last segment of a qualified name, whichever separator wrote it.
fn leaf_name(name: &str) -> &str {
    let after_path = name.rsplit("::").next().unwrap_or(name);
    after_path.rsplit('.').next().unwrap_or(after_path)
}

/// The class that holds `method`, read off its incoming `Contains` edge.
fn owner_of<G: GraphStore>(store: &G, method: &EntityId) -> Result<Option<Entity>> {
    for rel in
        kin_index::relation_read::relations_for_read(store, method).map_err(McpError::graph)?
    {
        if rel.kind != RelationKind::Contains || rel.dst != GraphNodeId::Entity(*method) {
            continue;
        }
        let Some(owner) = rel.src.as_entity() else {
            continue;
        };
        if let Some(entity) = store.get_entity(&owner).map_err(McpError::graph)? {
            if !matches!(entity.kind, EntityKind::Function | EntityKind::Method) {
                return Ok(Some(entity));
            }
        }
    }
    Ok(None)
}

/// The definitions an adapter recorded on `entity` as nested in its body and
/// not admitted, which are classes whose bases name `owner_leaf`.
fn unadmitted_subclasses_on(entity: &Entity, owner_leaf: &str) -> Vec<serde_json::Value> {
    let Some(records) = entity
        .metadata
        .extra
        .get(kin_parser::NESTED_DEFINITIONS_NOT_ADMITTED_KEY)
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    records
        .iter()
        .filter(|record| {
            record["kind"] == "class"
                && record["bases"].as_array().is_some_and(|bases| {
                    bases
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .any(|base| leaf_name(base) == owner_leaf)
                })
        })
        .map(|record| {
            let mut row = serde_json::json!({
                "name": record["name"],
                "enclosing": entity.name,
                "enclosing_id": entity.id,
                "placement": placement_of(entity.kind),
                "projection": {
                    "path": entity.file_origin.as_ref().map(|path| path.to_string()),
                },
            });
            // The line the class is declared on, 1-based as every served line
            // is, so a reader can see where it sits without reading its file.
            if let Some(line) = record["start_line"].as_u64() {
                row["line"] = serde_json::json!(line + 1);
            }
            row
        })
        .collect()
}

/// Where an unadmitted class sits, from the kind of entity whose body holds
/// it, which every adapter records the same way: a block at the top level of
/// its file (such as an `if` or `try` block), a function body, or a class
/// body. A class in a function or class body can be read through the entity
/// around it; one in a file-level block has no entity a reader can read.
pub(crate) fn placement_of(enclosing: EntityKind) -> &'static str {
    match enclosing {
        EntityKind::Module | EntityKind::Package | EntityKind::File => FILE_LEVEL_BLOCK,
        EntityKind::Function | EntityKind::Method | EntityKind::Test => "function_body",
        _ => "class_body",
    }
}

/// The placement of a class declared in a block at the top level of its file.
pub(crate) const FILE_LEVEL_BLOCK: &str = "file_level_block";

/// Name what the walk above cannot see: a class the parser left out whose
/// declared bases name the focal's owner.
///
/// Such a class names its base, so whatever encloses it holds a reference to
/// the owner. The owner's incoming edges therefore reach every entity that
/// could carry one in its record, and nothing else is read.
pub(crate) fn unadmitted_overriders<G: GraphStore>(
    store: &G,
    target: &Entity,
) -> Result<Vec<serde_json::Value>> {
    let Some(owner) = owner_of(store, &target.id)? else {
        return Ok(Vec::new());
    };
    let owner_leaf = leaf_name(&owner.name);
    let mut seen: HashSet<EntityId> = HashSet::new();
    let mut named = Vec::new();
    for rel in
        kin_index::relation_read::relations_for_read(store, &owner.id).map_err(McpError::graph)?
    {
        if rel.dst != GraphNodeId::Entity(owner.id) {
            continue;
        }
        let Some(source) = rel.src.as_entity() else {
            continue;
        };
        if !seen.insert(source) {
            continue;
        }
        if let Some(entity) = store.get_entity(&source).map_err(McpError::graph)? {
            named.extend(unadmitted_subclasses_on(&entity, owner_leaf));
        }
    }
    Ok(named)
}

/// One override row: the declaration's id, its name, the file it is projected
/// into, and the resolution of the edge that found it. No line is served, as on
/// every reference row; `get_entity_source` reads the declaration by its id.
pub(crate) fn override_row_json(row: &OverrideRow) -> serde_json::Value {
    let mut value = serde_json::json!({
        "entity_id": row.entity.id,
        "name": row.entity.name,
        "projection": {
            "path": row.entity.file_origin.as_ref().map(|path| path.to_string()),
        },
        "resolution": row.resolution.as_str(),
        "proven": row.resolution.is_proven(),
    });
    if let Some(base) = &row.overrides {
        value["overrides"] = serde_json::json!(base);
    }
    if row.implements {
        value["relation"] = serde_json::json!("implements");
    }
    if let Some(module) = &row.module {
        value["module"] = serde_json::json!(module);
    }
    if row.name_shared {
        value[crate::semantic_owners::NAME_SHARED_KEY] = serde_json::json!(true);
    }
    value
}

/// The key the proven callers of the overrides are published under, inside
/// [`OVERRIDDEN_BY_KEY`]. The reference page transport addresses it as
/// `overridden_by.override_callers`.
pub(crate) const OVERRIDE_CALLERS_KEY: &str = "override_callers";

/// The key the callers matched to an override by name only are published
/// under, inside [`OVERRIDDEN_BY_KEY`].
pub(crate) const OVERRIDE_CALLER_CANDIDATES_KEY: &str = "override_caller_candidates";

/// One caller of an override: its id, its name, the file it is projected
/// into, the override it calls and how firmly the call settled it.
fn override_caller_row_json(row: &kin_review::OverrideCaller) -> serde_json::Value {
    let mut value = serde_json::json!({
        "entity_id": row.entity.id,
        "name": row.entity.name,
        "projection": {
            "path": row.entity.file_origin.as_ref().map(|path| path.to_string()),
        },
        "proven": row.proven(),
    });
    if let Some(call) = row.primary() {
        value["calls_override"] = serde_json::json!(call.override_name);
        value["calls_override_id"] = serde_json::json!(call.override_id);
        value["resolution"] = serde_json::json!(call.resolution);
    }
    let others: Vec<&str> = row
        .calls
        .iter()
        .skip(1)
        .map(|call| call.override_name.as_str())
        .collect();
    if !others.is_empty() {
        value["also_calls_overrides"] = serde_json::json!(others);
    }
    value
}

/// The callers of `found`'s overrides that a reply about `target` does not
/// already list: no member of the family, and no caller of `target` itself,
/// which the reply's references hold. A caller whose own edge to `target` is
/// a name match is listed here when its call to an override is proven.
fn override_callers_for<G: GraphStore>(
    store: &G,
    target: &Entity,
    found: &Overriders,
) -> Result<kin_review::OverrideCallers> {
    let mut family: HashSet<EntityId> = found.rows.iter().map(|row| row.entity.id).collect();
    family.insert(target.id);
    let mut direct: HashMap<EntityId, bool> = HashMap::new();
    for rel in
        kin_index::relation_read::relations_for_read(store, &target.id).map_err(McpError::graph)?
    {
        if rel.kind != RelationKind::Calls || rel.dst != GraphNodeId::Entity(target.id) {
            continue;
        }
        if let Some(caller) = rel.src.as_entity() {
            *direct.entry(caller).or_default() |= RelationResolution::of(&rel).is_proven();
        }
    }
    let overrides: Vec<Entity> = found.rows.iter().map(|row| row.entity.clone()).collect();
    kin_review::override_callers::override_callers(
        store,
        &[(target.id, overrides)],
        |caller, proven| {
            family.contains(&caller.id)
                || direct
                    .get(&caller.id)
                    .is_some_and(|direct_proven| *direct_proven || !proven)
        },
    )
    .map_err(|error| McpError::Review(error.to_string()))
}

/// The `overridden_by` block of a reply.
///
/// Emitted at zero as well, for every method focal, because a section that
/// appears only when it has rows is one no reader learns to look for, and the
/// reader who most needs it is the one about to change a method's signature.
pub(crate) fn overridden_by_json(found: &Overriders) -> serde_json::Value {
    let proven = found
        .rows
        .iter()
        .filter(|row| row.resolution.is_proven())
        .count();
    let mut block = serde_json::json!({
        "count": found.rows.len(),
        "proven": proven,
        OVERRIDERS_KEY: found.rows.iter().map(override_row_json).collect::<Vec<_>>(),
        "coverage": {
            "scope": "recorded_overrides_and_implements",
            "transitive": true,
            "recorded_closure": if found.bounded { "bounded" } else { "complete" },
            "nested_definitions": if found.census_unmeasured { "unmeasured" } else { "focal_file_measured" },
            "unadmitted_candidates": found.not_admitted.len(),
            "unproven_relations": found.rows.len() - proven,
            "repository_completeness": repository_completeness(found),
        },
        "detail": "Methods that override or implement this one, directly or through \
                   another override. Not references and not in total_upstream; each \
                   changes with this method's signature.",
    });
    if found.bounded {
        block["bounded"] = serde_json::json!(true);
    }
    if !found.not_admitted.is_empty() {
        block["not_admitted"] = serde_json::json!(found.not_admitted);
        block["not_admitted_detail"] = serde_json::json!(
            "Classes declared inside a body the parser did not admit as entities, whose \
             bases name this method's owner, with where each sits (`placement`) and its line. \
             Each could override it; none is in overriders. One in a `file_level_block` \
             overrides it only if that block runs, and a block that exists only for type \
             checkers never does."
        );
    }
    if found.census_unmeasured {
        block["nested_definitions"] = serde_json::json!("unmeasured");
    }
    // Read only where something overrides the method: with no override there
    // is no override to call, and a zero would say nothing.
    if let Some(callers) = found.callers.as_ref().filter(|_| !found.rows.is_empty()) {
        // Each changes with this method's signature as a reference does, and
        // none is one: no edge reaches this method from a call settled on an
        // override.
        block[OVERRIDE_CALLERS_KEY] = serde_json::json!(callers
            .proven
            .iter()
            .map(override_caller_row_json)
            .collect::<Vec<_>>());
        if !callers.candidates.is_empty() {
            block[OVERRIDE_CALLER_CANDIDATES_KEY] = serde_json::json!(callers
                .candidates
                .iter()
                .map(override_caller_row_json)
                .collect::<Vec<_>>());
        }
        block["detail"] = serde_json::json!(
            "Methods that override or implement this one, directly or through another \
             override. Not references and not in total_upstream; each changes with this \
             method's signature. So does each caller in override_callers, which calls an \
             override rather than this method and names it under calls_override; \
             override_caller_candidates were matched to an override by name only and are \
             not proof."
        );
    }
    if let Some(census) = &found.repository {
        block["coverage"]["recorded_subclasses_checked"] = serde_json::json!(census.subclasses);
        block["coverage"]["recorded_classes_read"] = serde_json::json!(census.classes_read);
        if !census.unlisted.is_empty() {
            block["unlisted_same_name_methods"] = serde_json::json!(census.unlisted);
        }
        if !census.unlinked.is_empty() {
            block["classes_spelling_base_without_edge"] = serde_json::json!(census.unlinked);
        }
        if census.unlinked_without_member > 0 {
            block["coverage"][UNLINKED_WITHOUT_MEMBER_KEY] =
                serde_json::json!(census.unlinked_without_member);
        }
    }
    block
}

/// `complete` only when the repository census ran within its bounds and found
/// nothing the recorded closure could have missed, and no unadmitted class
/// could hold an override.
fn repository_completeness(found: &Overriders) -> &'static str {
    match &found.repository {
        Some(census)
            if census.complete()
                && found.not_admitted.is_empty()
                && !found.census_unmeasured
                && !found.bounded =>
        {
            "complete"
        }
        Some(_) => "incomplete",
        None => "unmeasured",
    }
}

/// A focused override answer uses the same collector as the references
/// attachment, without querying callers or their call-site census. Finishing
/// this graph walk does not attest complete repository override coverage.
pub(crate) fn focused_overrides<G: GraphStore>(
    store: &G,
    target: &Entity,
) -> Result<serde_json::Value> {
    if !overridable(target) {
        return Err(McpError::InvalidParams(
            "view=overrides requires a method or function entity".into(),
        ));
    }
    let found = overriders_for_target(store, target)?;
    Ok(serde_json::json!({
        "view": "overrides",
        "focal_entity": {
            "entity_id": target.id,
            "name": target.name,
            "projection": {"path": target.file_origin.as_ref().map(|path| path.to_string())},
        },
        OVERRIDDEN_BY_KEY: overridden_by_json(&found),
    }))
}

pub(crate) fn overriders_for_target<G: GraphStore>(
    store: &G,
    target: &Entity,
) -> Result<Overriders> {
    let mut found = collect_overriders(store, &target.id)?;
    found.not_admitted = unadmitted_overriders(store, target)?;
    found.census_unmeasured = target
        .metadata
        .extra
        .get(kin_parser::FILE_NESTED_DEFINITIONS_NOT_ADMITTED_KEY)
        .and_then(serde_json::Value::as_u64)
        .is_none();
    found.repository = repository_census(store, target, &found)?;
    Ok(found)
}

/// Whether `entity` is a declaration an override can name.
fn overridable(entity: &Entity) -> bool {
    matches!(entity.kind, EntityKind::Method | EntityKind::Function)
}

/// Attach the `overridden_by` block to a reply about `target`.
///
/// A method focal always carries it. Any other callable carries it only when
/// something overrides it, which a language that models methods as functions
/// can still record.
pub(crate) fn attach_overridden_by<G: GraphStore>(
    store: &G,
    target: &Entity,
    result: &mut serde_json::Value,
) -> Result<()> {
    if !overridable(target) {
        return Ok(());
    }
    let mut found = overriders_for_target(store, target)?;
    if target.kind == EntityKind::Method || !found.rows.is_empty() {
        if !found.rows.is_empty() {
            found.callers = Some(override_callers_for(store, target, &found)?);
        }
        result[OVERRIDDEN_BY_KEY] = overridden_by_json(&found);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::entity::{
        EntityMetadata, EntityRole, FingerprintAlgorithm, SemanticFingerprint, Visibility,
    };
    use kin_model::ids::{FilePathId, Hash256, RelationId};
    use kin_model::relation::{Relation, RelationOrigin};
    use kin_model::EntityStore;
    use kin_model::LanguageId;

    fn method(name: &str, file: &str) -> Entity {
        Entity {
            id: EntityId::new(),
            kind: EntityKind::Method,
            name: name.to_string(),
            language: LanguageId::Python,
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
            signature: format!("def {name}(self)"),
            visibility: Visibility::Public,
            role: EntityRole::Source,
            doc_summary: None,
            metadata: EntityMetadata::default(),
            lineage_parent: None,
            created_in: None,
            superseded_by: None,
        }
    }

    fn overrides(child: &Entity, base: &Entity, confidence: f32) -> Relation {
        Relation {
            id: RelationId::new(),
            kind: RelationKind::Overrides,
            src: GraphNodeId::Entity(child.id),
            dst: GraphNodeId::Entity(base.id),
            confidence,
            origin: RelationOrigin::Parsed,
            created_in: None,
            import_source: None,
            evidence: vec![],
        }
    }

    #[test]
    fn focused_overrides_distinguish_empty_recorded_walk_from_unmeasured_census() {
        let graph = kin_db::InMemoryGraph::new();
        let mut target = method("Base.run", "base.py");
        for measured in [false, true] {
            if measured {
                target.metadata.extra.insert(
                    kin_parser::FILE_NESTED_DEFINITIONS_NOT_ADMITTED_KEY.into(),
                    serde_json::json!(0),
                );
            }
            graph.upsert_entity(&target).unwrap();
            let answer = focused_overrides(&graph, &target).unwrap();
            let block = &answer[OVERRIDDEN_BY_KEY];
            assert_eq!(block["count"], 0);
            assert_eq!(block[OVERRIDERS_KEY], serde_json::json!([]));
            assert_eq!(block["coverage"]["recorded_closure"], "complete");
            assert_eq!(
                block["coverage"]["nested_definitions"],
                if measured {
                    "focal_file_measured"
                } else {
                    "unmeasured"
                }
            );
            assert_eq!(block["coverage"]["repository_completeness"], "unmeasured");
            assert!(answer.get("references").is_none());
            assert!(answer.get("call_sites").is_none());
        }
    }

    fn class(name: &str, file: &str, header: &str) -> Entity {
        Entity {
            kind: EntityKind::Class,
            signature: header.to_string(),
            ..method(name, file)
        }
    }

    fn edge(kind: RelationKind, src: &Entity, dst: &Entity) -> Relation {
        Relation {
            kind,
            ..overrides(src, dst, 1.0)
        }
    }

    /// Base.run with Child(Base) overriding it, and a census-measured focal
    /// file, so only the repository census decides completeness.
    fn census_fixture() -> (kin_db::InMemoryGraph, Entity, Entity) {
        let graph = kin_db::InMemoryGraph::new();
        let base = class("Base", "pkg/base.py", "class Base:");
        let mut run = method("Base.run", "pkg/base.py");
        run.metadata.extra.insert(
            kin_parser::FILE_NESTED_DEFINITIONS_NOT_ADMITTED_KEY.into(),
            serde_json::json!(0),
        );
        let child = class("Child", "pkg/child.py", "class Child(Base):");
        let child_run = method("Child.run", "pkg/child.py");
        for entity in [&base, &run, &child, &child_run] {
            graph.upsert_entity(entity).unwrap();
        }
        for relation in [
            edge(RelationKind::Contains, &base, &run),
            edge(RelationKind::Contains, &child, &child_run),
            edge(RelationKind::Extends, &child, &base),
            overrides(&child_run, &run, 1.0),
        ] {
            graph.upsert_relation(&relation).unwrap();
        }
        (graph, run, child)
    }

    #[test]
    fn a_clean_census_of_recorded_subclasses_reports_repository_complete() {
        let (graph, run, _) = census_fixture();
        let answer = focused_overrides(&graph, &run).unwrap();
        let block = &answer[OVERRIDDEN_BY_KEY];
        assert_eq!(block["count"], 1);
        assert_eq!(block["coverage"]["recorded_subclasses_checked"], 1);
        assert_eq!(block["coverage"]["repository_completeness"], "complete");
    }

    #[test]
    fn a_same_named_method_without_an_override_edge_keeps_it_incomplete() {
        let (graph, run, _) = census_fixture();
        let base = graph
            .query_entities(&kin_model::graph::EntityFilter {
                name_pattern: Some("Base".into()),
                kinds: Some(vec![EntityKind::Class]),
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .find(|class| class.name == "Base")
            .unwrap();
        let other = class("Other", "pkg/other.py", "class Other(Base):");
        let other_run = method("Other.run", "pkg/other.py");
        for entity in [&other, &other_run] {
            graph.upsert_entity(entity).unwrap();
        }
        for relation in [
            edge(RelationKind::Contains, &other, &other_run),
            edge(RelationKind::Extends, &other, &base),
        ] {
            graph.upsert_relation(&relation).unwrap();
        }
        let block = &focused_overrides(&graph, &run).unwrap()[OVERRIDDEN_BY_KEY];
        assert_eq!(block["coverage"]["repository_completeness"], "incomplete");
        assert_eq!(block["unlisted_same_name_methods"][0]["name"], "Other.run");
    }

    #[test]
    fn a_class_spelling_the_base_without_an_edge_keeps_it_incomplete() {
        let (graph, run, _) = census_fixture();
        let stray = class("Stray", "tests/test_stray.py", "class Stray(Base):");
        let stray_run = method("Stray.run", "tests/test_stray.py");
        let bare = class("Bare", "tests/test_bare.py", "class Bare(Base):");
        let bare_other = method("Bare.other", "tests/test_bare.py");
        for entity in [&stray, &stray_run, &bare, &bare_other] {
            graph.upsert_entity(entity).unwrap();
        }
        for relation in [
            edge(RelationKind::Contains, &stray, &stray_run),
            edge(RelationKind::Contains, &bare, &bare_other),
        ] {
            graph.upsert_relation(&relation).unwrap();
        }
        let block = &focused_overrides(&graph, &run).unwrap()[OVERRIDDEN_BY_KEY];
        assert_eq!(block["coverage"]["repository_completeness"], "incomplete");
        // Only a class that declares a member named like the target is
        // listed; the other is counted, and the count keeps coverage open.
        let listed = block["classes_spelling_base_without_edge"]
            .as_array()
            .unwrap();
        assert_eq!(listed.len(), 1, "{block:#}");
        assert_eq!(listed[0]["name"], "Stray");
        assert_eq!(block["coverage"][UNLINKED_WITHOUT_MEMBER_KEY], 1);
        // A header that only mentions the name elsewhere is not a base.
        assert!(!spells_base("class Basement(Mixin):", "Basement", "Base"));
        assert!(spells_base(
            "class B extends Base implements C",
            "B",
            "Base"
        ));
    }

    /// Classes that spell the base but declare no member named like the
    /// target are counted, never listed, and never make coverage complete.
    #[test]
    fn classes_without_a_same_named_member_are_counted_and_never_complete_coverage() {
        let (graph, run, _) = census_fixture();
        for (name, file) in [
            ("CustomApp", "tests/test_a.py"),
            ("MyApp", "tests/test_b.py"),
        ] {
            let header = format!("class {name}(Base):");
            graph.upsert_entity(&class(name, file, &header)).unwrap();
        }
        let block = &focused_overrides(&graph, &run).unwrap()[OVERRIDDEN_BY_KEY];
        assert!(
            block.get("classes_spelling_base_without_edge").is_none(),
            "{block:#}"
        );
        assert_eq!(block["coverage"][UNLINKED_WITHOUT_MEMBER_KEY], 2);
        assert_eq!(block["coverage"]["repository_completeness"], "incomplete");
    }

    #[test]
    fn focused_overrides_keep_a_real_closure_bound() {
        let graph = kin_db::InMemoryGraph::new();
        let target = method("Base.run", "base.py");
        graph.upsert_entity(&target).unwrap();
        let mut previous = target.clone();
        for index in 0..=kin_review::impact::OVERRIDE_CLOSURE_DEPTH {
            let child = method(&format!("Child{index}.run"), &format!("child{index}.py"));
            graph.upsert_entity(&child).unwrap();
            graph
                .upsert_relation(&overrides(&child, &previous, 1.0))
                .unwrap();
            previous = child;
        }
        let answer = focused_overrides(&graph, &target).unwrap();
        let block = &answer[OVERRIDDEN_BY_KEY];
        assert_eq!(block["count"], kin_review::impact::OVERRIDE_CLOSURE_DEPTH);
        assert_eq!(block["bounded"], true);
        assert_eq!(block["coverage"]["recorded_closure"], "bounded");
        assert_eq!(block["coverage"]["repository_completeness"], "unmeasured");
    }

    #[tokio::test]
    async fn focused_overrides_route_returns_only_override_family_and_keeps_resolution() {
        let graph = kin_db::InMemoryGraph::new();
        let target = method("Base.run", "base.py");
        let child = method("Child.run", "child.py");
        let possible = method("Possible.run", "possible.py");
        let caller = method("invoke", "caller.py");
        for entity in [&target, &child, &possible, &caller] {
            graph.upsert_entity(entity).unwrap();
        }
        let mut call = overrides(&caller, &target, 1.0);
        call.kind = RelationKind::Calls;
        let mut implementation = overrides(&child, &target, 1.0);
        implementation.kind = RelationKind::Implements;
        for edge in [call, implementation, overrides(&possible, &target, 0.3)] {
            graph.upsert_relation(&edge).unwrap();
        }
        let args = HashMap::from([
            ("view".into(), serde_json::json!("overrides")),
            ("entity_id".into(), serde_json::json!(target.id)),
        ]);
        let result = crate::handlers::entities::handle_find_references(&args, &graph, None)
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true));
        let crate::ContentBlock::Text { text } = &result.content[0];
        let answer: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(answer["view"], "overrides");
        assert!(answer.get("references").is_none());
        assert!(answer.get("call_sites").is_none());
        let rows = answer[OVERRIDDEN_BY_KEY][OVERRIDERS_KEY]
            .as_array()
            .unwrap();
        assert_eq!(rows.len(), 2);
        let row = rows
            .iter()
            .find(|row| row["entity_id"] == child.id.to_string())
            .unwrap();
        assert_eq!(row["relation"], "implements");
        assert_eq!(row["proven"], true);
        let row = rows
            .iter()
            .find(|row| row["entity_id"] == possible.id.to_string())
            .unwrap();
        assert_eq!(row["proven"], false);
        assert_eq!(row["resolution"], "name_only");
    }

    #[test]
    fn focused_overrides_reject_invalid_views_and_ignored_reference_filters() {
        let base = HashMap::from([
            ("view".into(), serde_json::json!("overrides")),
            ("entity_id".into(), serde_json::json!(EntityId::new())),
        ]);
        assert!(crate::handlers::entities::validate_reference_proof_arguments(&base).is_ok());
        for view in [
            serde_json::json!("unknown"),
            serde_json::json!(true),
            serde_json::Value::Null,
        ] {
            let mut args = base.clone();
            args.insert("view".into(), view);
            assert!(crate::handlers::entities::validate_reference_proof_arguments(&args).is_err());
        }
        for key in [
            "query",
            "relation_kinds",
            "proof_for_caller",
            "min_resolution",
            "include_snippets",
        ] {
            let mut args = base.clone();
            args.insert(key.into(), serde_json::json!("ignored"));
            assert!(
                crate::handlers::entities::validate_reference_proof_arguments(&args).is_err(),
                "{key}"
            );
        }
        let mut args = base;
        args.remove("entity_id");
        assert!(crate::handlers::entities::validate_reference_proof_arguments(&args).is_err());
    }

    #[test]
    fn a_class_the_parser_left_out_is_named_when_its_bases_name_the_owner() {
        let graph = kin_db::InMemoryGraph::new();
        let mut owner = method("SessionInterface", "src/flask/sessions.py");
        owner.kind = EntityKind::Class;
        let mut base = method("SessionInterface.open_session", "src/flask/sessions.py");
        base.metadata.extra.insert(
            kin_parser::FILE_NESTED_DEFINITIONS_NOT_ADMITTED_KEY.into(),
            serde_json::json!(0),
        );
        let mut enclosing = method("test_guarded", "tests/test_guarded.py");
        enclosing.kind = EntityKind::Function;
        enclosing.metadata.extra.insert(
            kin_parser::NESTED_DEFINITIONS_NOT_ADMITTED_KEY.into(),
            serde_json::json!([
                {"name": "Guarded", "kind": "class", "start_line": 4,
                 "bases": ["flask.sessions.SessionInterface"]},
                {"name": "Unrelated", "kind": "class", "start_line": 9, "bases": ["Other"]},
                {"name": "helper", "kind": "function", "start_line": 12},
            ]),
        );
        for entity in [&owner, &base, &enclosing] {
            graph.upsert_entity(entity).unwrap();
        }
        let mut contains = overrides(&owner, &base, 1.0);
        contains.kind = RelationKind::Contains;
        contains.src = GraphNodeId::Entity(owner.id);
        contains.dst = GraphNodeId::Entity(base.id);
        let mut names_owner = overrides(&enclosing, &owner, 1.0);
        names_owner.kind = RelationKind::References;
        for relation in [contains, names_owner] {
            graph.upsert_relation(&relation).unwrap();
        }

        let mut result = serde_json::json!({});
        attach_overridden_by(&graph, &base, &mut result).unwrap();
        let block = &result[OVERRIDDEN_BY_KEY];
        assert_eq!(block["count"], 0, "{block}");
        let named = block["not_admitted"].as_array().unwrap();
        assert_eq!(named.len(), 1, "{block}");
        assert_eq!(named[0]["name"], "Guarded");
        assert_eq!(named[0]["enclosing"], "test_guarded");
        assert_eq!(named[0]["projection"]["path"], "tests/test_guarded.py");
        // Where it sits and its 1-based line, so a reader need not open the
        // file to find it.
        assert_eq!(named[0]["placement"], "function_body");
        assert_eq!(named[0]["line"], 5);
        // The focal's file was censused, so nothing is unmeasured.
        assert!(block.get("nested_definitions").is_none(), "{block}");

        // A focal from a file no census covered says so.
        let mut unmeasured = base.clone();
        unmeasured
            .metadata
            .extra
            .remove(kin_parser::FILE_NESTED_DEFINITIONS_NOT_ADMITTED_KEY);
        let mut result = serde_json::json!({});
        attach_overridden_by(&graph, &unmeasured, &mut result).unwrap();
        assert_eq!(
            result[OVERRIDDEN_BY_KEY]["nested_definitions"],
            "unmeasured"
        );
    }

    /// A class in a block at the top level of its file has no entity around it
    /// a reader can read; one in a function or class body does.
    #[test]
    fn a_placement_is_read_from_the_kind_of_the_entity_around_it() {
        for (kind, placement) in [
            (EntityKind::Module, FILE_LEVEL_BLOCK),
            (EntityKind::File, FILE_LEVEL_BLOCK),
            (EntityKind::Package, FILE_LEVEL_BLOCK),
            (EntityKind::Function, "function_body"),
            (EntityKind::Method, "function_body"),
            (EntityKind::Test, "function_body"),
            (EntityKind::Class, "class_body"),
        ] {
            assert_eq!(placement_of(kind), placement, "{kind:?}");
        }
    }

    #[test]
    fn overriders_follow_the_chain_and_label_each_edge() {
        let graph = kin_db::InMemoryGraph::new();
        let base = method("SessionInterface.open_session", "src/flask/sessions.py");
        let secure = method(
            "SecureCookieSessionInterface.open_session",
            "src/flask/sessions.py",
        );
        let nested = method(
            "test_session_error_pops_context.<locals>.FailingSessionInterface.open_session",
            "tests/test_reqctx.py",
        );
        let deeper = method("Deeper.open_session", "tests/test_deeper.py");
        let unrelated = method("Other.open_session", "src/other.py");
        for entity in [&base, &secure, &nested, &deeper, &unrelated] {
            graph.upsert_entity(entity).unwrap();
        }
        for relation in [
            overrides(&secure, &base, 1.0),
            overrides(&nested, &base, 1.0),
            overrides(&deeper, &secure, 0.3),
        ] {
            graph.upsert_relation(&relation).unwrap();
        }

        let found = collect_overriders(&graph, &base.id).unwrap();
        assert!(!found.bounded);
        let names: Vec<_> = found
            .rows
            .iter()
            .map(|row| row.entity.name.as_str())
            .collect();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"SecureCookieSessionInterface.open_session"));
        assert!(names.contains(&nested.name.as_str()));
        assert_eq!(names[2], "Deeper.open_session");

        let block = overridden_by_json(&found);
        assert_eq!(block["count"], 3);
        assert_eq!(block["proven"], 2);
        let rows = block[OVERRIDERS_KEY].as_array().unwrap();
        let deeper_row = rows
            .iter()
            .find(|row| row["name"] == "Deeper.open_session")
            .unwrap();
        assert_eq!(
            deeper_row["overrides"],
            "SecureCookieSessionInterface.open_session"
        );
        assert_eq!(deeper_row["resolution"], "name_only");
        assert!(rows
            .iter()
            .filter(|row| row["name"] != "Deeper.open_session")
            .all(|row| row.get("overrides").is_none() && row["resolution"] == "type_resolved"));

        // A member that implements the method as a declared contract member is
        // listed and says so; a type-level implements names no member.
        let implementer = method("NoopConfig.open_session", "src/noop.py");
        let mut class = method("NoopConfig", "src/noop.py");
        class.kind = EntityKind::Class;
        for entity in [&implementer, &class] {
            graph.upsert_entity(entity).unwrap();
        }
        let mut implements = overrides(&implementer, &base, 1.0);
        implements.kind = RelationKind::Implements;
        let mut type_level = overrides(&class, &base, 1.0);
        type_level.kind = RelationKind::Implements;
        for relation in [implements, type_level] {
            graph.upsert_relation(&relation).unwrap();
        }
        let block = overridden_by_json(&collect_overriders(&graph, &base.id).unwrap());
        assert_eq!(block["count"], 4, "{block}");
        let implemented = block[OVERRIDERS_KEY]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "NoopConfig.open_session")
            .unwrap();
        assert_eq!(implemented["relation"], "implements");
        assert!(!block.to_string().contains("\"NoopConfig\""));

        // A method nothing overrides still says so, at zero.
        let mut result = serde_json::json!({});
        attach_overridden_by(&graph, &unrelated, &mut result).unwrap();
        assert_eq!(result[OVERRIDDEN_BY_KEY]["count"], 0);
        assert_eq!(
            result[OVERRIDDEN_BY_KEY][OVERRIDERS_KEY],
            serde_json::json!([])
        );
    }
}
