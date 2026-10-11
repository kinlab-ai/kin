// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Structural identity cues from the selected graph, separate from dependencies.

use kin_model::entity::{Entity, EntityKind};
use kin_model::graph::{EntityFilter, EntityPage, GraphStore};
use kin_model::relation::{GraphNodeId, RelationKind};
use serde_json::{json, Value};

const MAX_OWNERS: usize = 64;

/// The key a row carries when the entity it names shares its name with
/// another declaration of the same source unit: a property and its setter,
/// the overloads of a function, a function named as its module is.
///
/// Such an entity has no label of its own. `module :: name` is the label of
/// each of them, so a reply that printed one of them under the bare label
/// would hand its reader an address that reads back as either, or, on a
/// connection that remembers what it served, silently as the one it was
/// shown. The graph is where that is known, whichever of them a reply holds,
/// so the row says it and every surface writes the row with its id:
/// `module :: name #id`.
pub const NAME_SHARED_KEY: &str = "name_shared";

/// Whether another entity declared in `entity`'s source unit carries its
/// name. `false` when the graph cannot be read, as for an entity of a
/// repository this graph is not.
pub fn name_is_shared<G: GraphStore + ?Sized>(store: &G, entity: &Entity) -> bool {
    let Some(file) = entity.file_origin.as_ref() else {
        return false;
    };
    store
        .query_entities(&EntityFilter {
            file_path: Some(file.clone()),
            name_pattern: Some(entity.name.clone()),
            ..Default::default()
        })
        .is_ok_and(|held| {
            held.iter()
                .any(|other| other.id != entity.id && other.name == entity.name)
        })
}

/// Set [`NAME_SHARED_KEY`] on `row` when the entity it names shares its
/// name, for a row whose module is attached some other way than by
/// [`RecordedOwners::attach`].
pub fn mark_shared_name<G: GraphStore + ?Sized>(store: &G, entity: &Entity, row: &mut Value) {
    if let Some(object) = row.as_object_mut() {
        if name_is_shared(store, entity) {
            object.insert(NAME_SHARED_KEY.into(), json!(true));
        } else {
            object.remove(NAME_SHARED_KEY);
        }
    }
}

/// A bounded observation, frozen before a context pack retries its size fit.
/// `member_lookup_query` names a query for the owned member through its module,
/// not the owner itself. The query does not establish a unique entity.
#[derive(Debug, Clone)]
pub struct RecordedOwners {
    /// The dotted module the entity's own file is the surface of, such as
    /// `flask.ctx` for `AppContext._get_session`.
    module: Option<String>,
    /// Whether another declaration of the entity's source unit carries its
    /// name ([`NAME_SHARED_KEY`]).
    shared_name: bool,
    owners: Vec<Value>,
    gap: Option<&'static str>,
    omitted: usize,
    candidates: usize,
    /// The level Kin holds a tracked thing it does not parse at
    /// ([`crate::artifact_entity`]). Such a thing is placed by its module and
    /// owned by no entity, which is no gap.
    held_as: Option<&'static str>,
}

impl RecordedOwners {
    /// Ownership could not be observed in the selected graph.
    pub fn unavailable() -> Self {
        Self {
            module: None,
            shared_name: false,
            owners: Vec::new(),
            gap: Some("owner_observation_unavailable"),
            omitted: 0,
            candidates: 0,
            held_as: None,
        }
    }

    /// A tracked thing Kin does not parse: placed under `module`, held at the
    /// level `held_as` names, and owned by no entity.
    pub fn artifact(module: Option<String>, held_as: &'static str) -> Self {
        Self {
            module,
            gap: None,
            held_as: Some(held_as),
            ..Self::unavailable()
        }
    }

    /// A foreign caller's graph is not selected by this local read.
    pub(crate) fn foreign() -> Self {
        Self {
            gap: Some("owner_repository_not_selected"),
            ..Self::unavailable()
        }
    }

    /// `entity` must come from this selected graph. No foreign UUID is resolved
    /// against a local store, and shared source placement is never ownership.
    pub fn observe<G: GraphStore>(store: &G, entity: &Entity) -> Self {
        let module = entity_module(store, entity);
        let shared_name = name_is_shared(store, entity);
        let Ok(relations) = store.get_all_relations_for_entity(&entity.id) else {
            return Self {
                module,
                shared_name,
                owners: Vec::new(),
                gap: Some("owner_relations_unavailable"),
                omitted: 0,
                candidates: 0,
                held_as: None,
            };
        };
        let mut contains: Vec<_> = relations
            .into_iter()
            .filter(|relation| {
                relation.kind == RelationKind::Contains
                    && relation.dst == GraphNodeId::Entity(entity.id)
                    && relation.src != relation.dst
            })
            .collect();
        contains.sort_by_key(|relation| (relation.src.to_string(), relation.id));
        contains.dedup_by_key(|relation| relation.id);
        let omitted = contains.len().saturating_sub(MAX_OWNERS);
        let mut gap = if contains.is_empty() {
            Some("no_recorded_owner")
        } else if omitted > 0 {
            Some("recorded_owners_partial")
        } else {
            None
        };
        // Apply endpoint authority only to the bounded ownership edges, not
        // every dependency of a frequently used declaration.
        contains.truncate(MAX_OWNERS);
        let observed = contains.len();
        if kin_index::relation_read::project_relations_for_read(store, &mut contains).is_err() {
            return Self {
                module,
                shared_name,
                owners: Vec::new(),
                gap: Some("owner_entities_unavailable"),
                omitted,
                candidates: 0,
                held_as: None,
            };
        }
        if contains.len() < observed {
            gap = Some("recorded_owners_partial");
        }
        let mut owners = Vec::new();
        let mut candidates = 0;
        for relation in contains {
            let GraphNodeId::Entity(owner_id) = relation.src else {
                gap = Some("recorded_owners_partial");
                continue;
            };
            let resolution = kin_index::RelationResolution::of(&relation);
            if !resolution.is_proven() {
                candidates += 1;
                gap = Some("recorded_owners_partial");
                continue;
            }
            let mut owner = json!({
                "entity_id":owner_id,"relation_id":relation.id,"relationship":"Contains",
                "resolution":resolution.as_str()
            });
            match store.get_entity(&owner_id) {
                Ok(Some(recorded)) if recorded.id == owner_id => {
                    // Compatibility containers and generated coordinate labels
                    // retain their graph identity, never a fabricated name.
                    if recorded.kind != EntityKind::File {
                        owner["kind"] = json!(recorded.kind);
                    }
                    let safe_name = recorded.kind != EntityKind::File
                        && !recorded.name.is_empty()
                        && (!matches!(recorded.kind, EntityKind::Module | EntityKind::Package)
                            || (!recorded
                                .name
                                .chars()
                                .any(|ch| ch == '/' || ch == '\\' || ch.is_control())
                                && !crate::file_coordinate_query::looks_like_coordinate(
                                    &recorded.name,
                                )
                                && recorded
                                    .file_origin
                                    .as_ref()
                                    .is_none_or(|origin| recorded.name != origin.0)));
                    if safe_name {
                        owner["name"] = json!(recorded.name);
                        // Two modules can share a leaf name (`flask.app` and
                        // `flask.sansio.app`); the dotted name from the graph's
                        // own module records tells them apart without a path.
                        let qualified = qualified_module_name(store, &recorded);
                        if let Some(qualified) = &qualified {
                            owner["qualified_name"] = json!(qualified);
                        }
                        if let Some(query) =
                            crate::module_name_query::member_lookup(&recorded, entity)
                        {
                            let query = match &qualified {
                                Some(qualified) if *qualified != recorded.name => {
                                    format!("{qualified}.{}", entity.name)
                                }
                                _ => query,
                            };
                            owner["member_lookup_query"] = json!(query);
                        }
                    } else {
                        owner["semantic_identity_gap"] = json!("owner_name_unavailable");
                        gap = Some("recorded_owners_partial");
                    }
                }
                _ => {
                    gap = Some("recorded_owners_partial");
                    continue;
                }
            }
            owners.push(owner);
        }
        Self {
            module,
            shared_name,
            owners,
            gap,
            omitted,
            candidates,
            held_as: None,
        }
    }
}

/// Most module entities one file carries; a file's module surface is the
/// widest of them.
const MAX_MODULES_PER_FILE: usize = 64;

/// [`entity_module`] for a caller outside this crate: the module a row's
/// label is written with (`flask.app` in `flask.app :: Flask.route`). Read
/// by the file the entity is declared in, so it answers for an entity a
/// commit has just removed while its file's module is still recorded.
pub fn module_label<G: GraphStore + ?Sized>(store: &G, entity: &Entity) -> Option<String> {
    entity_module(store, entity)
}

/// The dotted module of the file `entity` is declared in, read from the
/// graph's module records. A module entity names itself.
pub(crate) fn entity_module<G: GraphStore + ?Sized>(store: &G, entity: &Entity) -> Option<String> {
    if entity.kind == EntityKind::Module {
        return qualified_module_name(store, entity);
    }
    module_of_file(store, entity.file_origin.as_ref()?)
}

/// The module each of a reply's entities is declared in, as [`entity_module`]
/// names it, read once per source unit.
///
/// A trace names the module of its focal and of every step, and its steps
/// cluster in few units, so one module lookup serves every entity a unit
/// declares. A module entity names itself and is read on its own.
pub struct DeclaringModules<'store, G: GraphStore + ?Sized> {
    store: &'store G,
    by_unit: std::collections::HashMap<kin_model::FilePathId, Option<String>>,
}

impl<'store, G: GraphStore + ?Sized> DeclaringModules<'store, G> {
    pub fn new(store: &'store G) -> Self {
        Self {
            store,
            by_unit: std::collections::HashMap::new(),
        }
    }

    /// The dotted module `entity` is declared in, or `None` when the graph's
    /// module records name none for it.
    pub fn of(&mut self, entity: &Entity) -> Option<String> {
        if entity.kind == EntityKind::Module {
            return entity_module(self.store, entity);
        }
        let unit = entity.file_origin.as_ref()?;
        if let Some(known) = self.by_unit.get(unit) {
            return known.clone();
        }
        let named = module_of_file(self.store, unit);
        self.by_unit.insert(unit.clone(), named.clone());
        named
    }
}

/// The dotted module whose surface is the source `file`, read from the
/// graph's module records; never the path itself.
pub(crate) fn file_module<G: GraphStore + ?Sized>(store: &G, file: &str) -> Option<String> {
    module_of_file(store, &kin_model::FilePathId::new(file))
}

fn module_of_file<G: GraphStore + ?Sized>(
    store: &G,
    file: &kin_model::FilePathId,
) -> Option<String> {
    qualified_module_name(store, &surface_module(store, file)?)
}

/// The module entity whose surface is the source `file`: the widest module
/// the file carries, when it carries a bounded number.
pub(crate) fn file_surface_module<G: GraphStore + ?Sized>(store: &G, file: &str) -> Option<Entity> {
    surface_module(store, &kin_model::FilePathId::new(file))
}

fn surface_module<G: GraphStore + ?Sized>(
    store: &G,
    file: &kin_model::FilePathId,
) -> Option<Entity> {
    let page = store
        .query_entities_page(
            &EntityFilter {
                kinds: Some(vec![EntityKind::Module]),
                file_path: Some(file.clone()),
                ..Default::default()
            },
            &EntityPage::first(MAX_MODULES_PER_FILE),
        )
        .ok()?;
    if page.total_matching > MAX_MODULES_PER_FILE {
        return None;
    }
    let width = |module: &Entity| {
        module
            .span
            .as_ref()
            .map(|span| span.end_byte.saturating_sub(span.start_byte))
            .unwrap_or(0)
    };
    page.entities
        .into_iter()
        .filter(|module| {
            module.kind == EntityKind::Module && module.file_origin.as_ref() == Some(file)
        })
        .max_by(|a, b| width(a).cmp(&width(b)).then_with(|| b.id.cmp(&a.id)))
}

/// A module owner's dotted name, when the graph's module records give one that
/// is safe to show: no path separator or coordinate spelling can pass.
fn qualified_module_name<G: GraphStore + ?Sized>(store: &G, module: &Entity) -> Option<String> {
    if module.kind != EntityKind::Module {
        return None;
    }
    let qualified = kin_ranking::module_qualifier::module_path(store, module).ok()??;
    let safe = !qualified.is_empty()
        && !qualified
            .chars()
            .any(|ch| ch == '/' || ch == '\\' || ch.is_control() || ch.is_whitespace())
        && !crate::file_coordinate_query::looks_like_coordinate(&qualified);
    safe.then_some(qualified)
}

impl RecordedOwners {
    /// Attach the frozen identity cue before response sizing or paging.
    pub fn attach(&self, row: &mut Value) {
        if let Some(held_as) = self.held_as {
            // An artifact's row states its level, and has no owner to list.
            if let Some(object) = row.as_object_mut() {
                for key in [
                    "semantic_owners",
                    "semantic_owner_gap",
                    "semantic_owners_omitted",
                    "semantic_owner_candidate_count",
                    NAME_SHARED_KEY,
                ] {
                    object.remove(key);
                }
                object.insert(crate::artifact_entity::HELD_AS_KEY.into(), json!(held_as));
                if let Some(module) = &self.module {
                    object.insert("module".into(), json!(module));
                }
            }
            return;
        }
        row["semantic_owners"] = json!(self.owners);
        if let Some(module) = &self.module {
            row["module"] = json!(module);
        }
        if self.shared_name {
            row[NAME_SHARED_KEY] = json!(true);
        }
        if let Some(object) = row.as_object_mut() {
            if !self.shared_name {
                object.remove(NAME_SHARED_KEY);
            }
            object.remove("semantic_owner_gap");
            object.remove("semantic_owners_omitted");
            object.remove("semantic_owner_candidate_count");
        }
        if let Some(gap) = self.gap {
            row["semantic_owner_gap"] = json!(gap);
        }
        if self.candidates > 0 {
            row["semantic_owner_candidate_count"] = json!(self.candidates);
        }
        if self.omitted > 0 {
            row["semantic_owners_omitted"] = json!(self.omitted);
        }
    }
}
