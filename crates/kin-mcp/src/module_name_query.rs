// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A two-part spelling backed by a recorded module and direct containment.
//! This is not import-alias or package resolution. The original name predicate
//! remains valid; this adds bare declarations their stored module qualifies.

use std::collections::{BTreeMap, HashSet};

use kin_model::entity::{Entity, EntityKind, EntityRole};
use kin_model::graph::{EntityFilter, EntityPage, EntityStore, GraphStore};
use kin_model::relation::{GraphNodeId, RelationKind};
use kin_model::{EntityId, FilePathId};

use crate::error::{McpError, Result};

const MAX_MODULES: usize = 64;
const MAX_CANDIDATES_PER_FILE: usize = 256;
const MAX_CANDIDATES: usize = 4096;
const MAX_CONTAINMENT_RELATIONS: usize = 8192;

fn identifier(part: &str) -> bool {
    !part.is_empty() && part.chars().all(|ch| ch == '_' || ch.is_alphanumeric())
}

/// A module path segment may also carry `-`, as a JavaScript package does.
fn module_segment(part: &str) -> bool {
    !part.is_empty()
        && part
            .chars()
            .all(|ch| ch == '_' || ch == '-' || ch.is_alphanumeric())
}

/// `module.leaf` or `package.module.leaf`: the module path's segments,
/// outermost first, and the leaf it qualifies.
pub(crate) fn qualified_parts(query: &str) -> Option<(Vec<&str>, &str)> {
    let (module, leaf) = query.trim().rsplit_once('.')?;
    let segments: Vec<&str> = module.split('.').collect();
    (identifier(leaf) && segments.iter().all(|segment| module_segment(segment)))
        .then_some((segments, leaf))
}

/// The innermost module name and the leaf of a module-qualified spelling.
pub(crate) fn parts(query: &str) -> Option<(&str, &str)> {
    let (segments, leaf) = qualified_parts(query)?;
    Some((*segments.last()?, leaf))
}

/// Whether a recorded dotted module path ends with the queried segments.
fn path_ends_with(path: &str, segments: &[&str]) -> bool {
    let recorded: Vec<&str> = path.split('.').collect();
    recorded.len() >= segments.len()
        && recorded[recorded.len() - segments.len()..]
            .iter()
            .zip(segments)
            .all(|(have, want)| have.eq_ignore_ascii_case(want))
}

/// A supported two-part lookup spelling for a recorded direct module member.
/// The caller must have observed the module's Contains edge to this entity.
/// This names a query, not a unique entity: same-named modules remain ambiguous.
pub(crate) fn member_lookup(module: &Entity, member: &Entity) -> Option<String> {
    if module.kind != EntityKind::Module
        || module.file_origin.is_none()
        || module.file_origin != member.file_origin
    {
        return None;
    }
    let query = format!("{}.{}", module.name, member.name);
    if parts(&query).is_none() || crate::file_coordinate_query::looks_like_coordinate(&query) {
        return None;
    }
    Some(query)
}

pub(crate) struct ModuleNameQuery {
    leaf: String,
    /// No ranking or first-module preference: identical module names in
    /// distinct files remain distinct candidates.
    members_by_file: BTreeMap<String, HashSet<EntityId>>,
    /// Files of modules sharing the innermost name whose recorded dotted path
    /// does not end with the query's: read and known not to be the module.
    ruled_out_files: Vec<String>,
    /// Modules the whole query names as a dotted module path (`flask.ctx`),
    /// as a reply's `module` field spells them.
    named_modules: Vec<Entity>,
}

fn bounded<S: EntityStore>(
    store: &S,
    filter: &EntityFilter,
    maximum: usize,
) -> Result<Vec<Entity>> {
    let page = store
        .query_entities_page(filter, &EntityPage::first(maximum))
        .map_err(McpError::graph)?;
    if page.total_matching > maximum || page.next_offset.is_some() {
        return Err(McpError::Other(format!(
            "module-qualified locate exceeded its candidate bound ({maximum}); no complete match set or absence is established"
        )));
    }
    Ok(page.entities)
}

impl ModuleNameQuery {
    pub(crate) fn load<G: GraphStore>(store: &G, query: &str) -> Result<Option<Self>> {
        let Some((segments, leaf)) = qualified_parts(query) else {
            return Ok(None);
        };
        let module_name = *segments
            .last()
            .expect("qualified_parts yields a module segment");
        // Target kind/language/role filters belong to declarations, not the
        // qualifying module. The same context also serves relaxed-name counts
        // and the census, including files with no admitted matching leaf.
        let modules = bounded(
            store,
            &EntityFilter {
                kinds: Some(vec![EntityKind::Module]),
                name_pattern: Some(module_name.to_string()),
                ..Default::default()
            },
            MAX_MODULES,
        )?;
        let mut members_by_file: BTreeMap<String, HashSet<EntityId>> = BTreeMap::new();
        let mut ruled_out_files: Vec<String> = Vec::new();
        let mut relations_seen = 0usize;
        for module in modules
            .into_iter()
            .filter(|module| module.name.to_lowercase() == module_name.to_lowercase())
        {
            // External module stubs do not identify a repository declaration
            // surface and cannot invalidate a local namesake's identity.
            if module.role == EntityRole::External && module.file_origin.is_none() {
                continue;
            }
            let path = module.file_origin.as_ref().ok_or_else(|| {
                McpError::Other("module membership cannot be verified: recorded module has no source coordinate".into())
            })?;
            // `flask.sansio.app.x` names one of the modules called `app`: keep
            // only those whose dotted path from the graph's own module records
            // ends with the queried segments.
            if segments.len() > 1 {
                let Some(dotted) = kin_ranking::module_qualifier::module_path(store, &module)
                    .map_err(McpError::graph)?
                else {
                    return Err(McpError::Other("module-qualified locate could not read a module path within its bound; no complete match set or absence is established".into()));
                };
                if !path_ends_with(&dotted, &segments) {
                    ruled_out_files.push(path.0.clone());
                    continue;
                }
            }
            let relations = store
                .get_relations(&module.id, &[RelationKind::Contains])
                .map_err(McpError::graph)?;
            relations_seen = relations_seen.saturating_add(relations.len());
            if relations_seen > MAX_CONTAINMENT_RELATIONS {
                return Err(McpError::Other("module-qualified locate exceeded its containment bound; no complete match set or absence is established".into()));
            }
            let members = members_by_file.entry(path.0.clone()).or_default();
            for relation in relations {
                if relation.kind == RelationKind::Contains
                    && relation.src == GraphNodeId::Entity(module.id)
                {
                    if let GraphNodeId::Entity(child) = relation.dst {
                        members.insert(child);
                    }
                }
            }
        }
        // The whole query may itself be a module's dotted path: `flask.ctx`
        // names the module `ctx` whose path ends in it, not a member of
        // `flask`.
        let mut path_segments = segments.clone();
        path_segments.push(leaf);
        let mut named_modules = Vec::new();
        // Past its bound this route steps aside rather than failing a query
        // the member route answers (JavaScript holds hundreds of `index`).
        let same_named = bounded(
            store,
            &EntityFilter {
                kinds: Some(vec![EntityKind::Module]),
                name_pattern: Some(leaf.to_string()),
                ..Default::default()
            },
            MAX_MODULES,
        )
        .unwrap_or_default();
        for module in same_named
            .into_iter()
            .filter(|module| module.name.eq_ignore_ascii_case(leaf))
        {
            let dotted = kin_ranking::module_qualifier::module_path(store, &module)
                .map_err(McpError::graph)?;
            if dotted.is_some_and(|dotted| path_ends_with(&dotted, &path_segments)) {
                named_modules.push(module);
            }
        }
        Ok(Some(Self {
            leaf: leaf.to_string(),
            members_by_file,
            ruled_out_files,
            named_modules,
        }))
    }

    pub(crate) fn files(&self) -> impl Iterator<Item = &str> {
        self.members_by_file.keys().map(String::as_str)
    }

    pub(crate) fn ruled_out_files(&self) -> impl Iterator<Item = &str> {
        self.ruled_out_files.iter().map(String::as_str)
    }

    pub(crate) fn extend<G: GraphStore>(
        &self,
        store: &G,
        filter: &EntityFilter,
        entities: &mut Vec<Entity>,
    ) -> Result<()> {
        let mut seen: HashSet<_> = entities.iter().map(|entity| entity.id).collect();
        let module_kind_allowed = filter
            .kinds
            .as_ref()
            .is_none_or(|kinds| kinds.contains(&EntityKind::Module));
        for module in &self.named_modules {
            if module_kind_allowed && seen.insert(module.id) {
                entities.push(module.clone());
            }
        }
        let mut candidates_seen = 0usize;
        for (path, members) in &self.members_by_file {
            if filter
                .file_path
                .as_ref()
                .is_some_and(|wanted| wanted.0 != *path)
            {
                continue;
            }
            let candidates = bounded(
                store,
                &EntityFilter {
                    name_pattern: Some(self.leaf.clone()),
                    file_path: Some(FilePathId::new(path)),
                    ..filter.clone()
                },
                MAX_CANDIDATES_PER_FILE,
            )?;
            candidates_seen = candidates_seen.saturating_add(candidates.len());
            if candidates_seen > MAX_CANDIDATES {
                return Err(McpError::Other("module-qualified locate exceeded its total candidate bound; no complete match set or absence is established".into()));
            }
            for entity in candidates {
                // `module.leaf` cannot silently select `module.Type.leaf`.
                // Existing qualified entity names were handled by the original
                // whole-name query before this additional route.
                if entity.name.to_lowercase() != self.leaf.to_lowercase() {
                    continue;
                }
                if !members.contains(&entity.id) {
                    return Err(McpError::Other(format!(
                        "module membership is unproven for entity {}: no recorded direct Contains edge; no complete match set or absence is established",
                        entity.id
                    )));
                }
                if seen.insert(entity.id) {
                    entities.push(entity);
                }
            }
        }
        Ok(())
    }
}
