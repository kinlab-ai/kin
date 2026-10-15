// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Unit-addressed declaration creation and import management.
//!
//! A caller names a source unit by language identity and a declaration by its
//! name and kind. This planner resolves the unit's projection path from the
//! prospective tree's own module layout, reads the unit's exact bytes from the
//! blob store or repository CAS (never the working copy), lets the language's
//! unit editor place the declarations and rewrite the import block, and admits
//! the result through the same reconcile path every source edit takes. The
//! footprint is then proven: every existing declaration in the unit keeps its
//! exact bytes, and the only new entities are the requested declarations and
//! the members nested inside them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use kin_mcp::entity_lifecycle::EntityCreate;
use kin_mcp::source_unit::{RepositoryBase, SourceUnit, UnitImports, UnitRole};
use kin_mcp::{McpMutationOperation, McpMutationPayload};
use kin_model::{
    EntityKind, EntityStore, FileLayout, FilePathId, Hash256, LanguageId, LocatedEntry, RepoPath,
    TransactionDelta, TreeDelta, TreeEntry,
};
use kin_parser::unit_editor::{UnitDeclaration, UnitEdit, UnitEditor, UnitImport};

use crate::local_repository_authority::LocalRepositoryAuthorityContext;
use crate::repository_commit::load_native_source_blob;
use crate::state::DaemonState;

enum UnitRequest<'a> {
    Create(&'a EntityCreate),
    Imports(&'a UnitImports),
}

fn unit_request(operation: &McpMutationOperation) -> Option<(&SourceUnit, UnitRequest<'_>)> {
    match operation.payload.as_ref()? {
        McpMutationPayload::EntityCreate(create) => create
            .unit_target()
            .map(|(_, unit)| (unit, UnitRequest::Create(create))),
        McpMutationPayload::UnitImports(imports) => {
            Some((&imports.unit, UnitRequest::Imports(imports)))
        }
        _ => None,
    }
}

/// The repository base a unit-addressed operation carries.
pub(crate) fn repository_base(operation: &McpMutationOperation) -> Option<&RepositoryBase> {
    match operation.payload.as_ref()? {
        McpMutationPayload::EntityCreate(create) => create.unit_target().map(|(base, _)| base),
        McpMutationPayload::UnitImports(imports) => Some(&imports.repository_base),
        _ => None,
    }
}

/// The unit an operation addresses, resolved to its projection path against
/// `tree`, or `None` when the operation is not unit-addressed.
pub(crate) fn unit_path(
    tree: &kin_model::ResolvedTree,
    operation: &McpMutationOperation,
) -> Option<Result<RepoPath, String>> {
    let (unit, _) = unit_request(operation)?;
    Some(kin_mcp::source_unit::unit_projection_path(unit, tree))
}

/// The workspace instant a unit-addressed operation must name, for replies.
pub(crate) fn current_repository_base(state: &DaemonState) -> Option<RepositoryBase> {
    let context = LocalRepositoryAuthorityContext::from_state(state).ok()?;
    let authority = crate::api::held_repository_authority(state).ok()?;
    repository_base_from(&authority, context.workspace_id())
}

/// The change this daemon's workspace stands on, resolved the way a diff of
/// the working copy resolves it: a head naming an imported commit reads as
/// the change that commit became.
pub(crate) fn workspace_base_change(state: &DaemonState) -> Option<kin_model::SemanticChangeId> {
    let context = LocalRepositoryAuthorityContext::from_state(state).ok()?;
    let authority = crate::api::held_repository_authority(state).ok()?;
    workspace_state_from(&authority, context.workspace_id(), |lease, workspace| {
        lease
            .resolve_target_change_id(workspace.base_target.as_ref()?)
            .ok()
    })
}

/// Read one workspace's state from an already-held authority.
fn workspace_state_from<T>(
    authority: &kin_db::RepositoryAuthorityManager<kin_db::LocalFileBackend>,
    workspace_id: kin_model::WorkspaceId,
    read: impl FnOnce(&kin_db::RepositoryAuthorityState, &kin_model::WorkspaceState) -> Option<T>,
) -> Option<T> {
    let lease = authority.read_authority();
    let workspace = lease
        .metadata()
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == workspace_id)?;
    read(&lease, workspace)
}

/// The repository base an already-held authority names for one workspace.
pub(crate) fn repository_base_from(
    authority: &kin_db::RepositoryAuthorityManager<kin_db::LocalFileBackend>,
    workspace_id: kin_model::WorkspaceId,
) -> Option<RepositoryBase> {
    workspace_state_from(authority, workspace_id, |_, workspace| {
        RepositoryBase::from_workspace(workspace).ok()
    })
}

/// One member of a unit's namespace the graph already holds.
struct Member {
    name: String,
    kind: EntityKind,
}

/// What one directory's units of a language already declare, read from
/// graph truth: the members of the namespace named `name`, and the unit
/// names its source and test units declare.
#[derive(Default)]
struct DirectoryUnits {
    members: Vec<Member>,
    source_units: BTreeSet<String>,
    test_units: BTreeSet<String>,
}

fn directory_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(directory, _)| directory)
}

/// The units of `language` in the directory of the unit at `path`, and the
/// members of the namespace the unit named `name` declares into: every unit
/// of the directory where the language shares one namespace across them,
/// else the unit's own.
fn directory_units(
    before: &kin_db::GraphSnapshot,
    editor: &dyn UnitEditor,
    language: LanguageId,
    path: &str,
    name: &str,
) -> DirectoryUnits {
    let directory = directory_of(path);
    let mut units = DirectoryUnits::default();
    for entity in before.entities.values() {
        let Some(origin) = entity.file_origin.as_ref() else {
            continue;
        };
        if entity.language != language || directory_of(&origin.0) != directory {
            continue;
        }
        let own_namespace = editor.directory_shares_names() || origin.0 == path;
        let declared = editor.declared_unit(entity);
        if let Some(declared) = declared {
            if editor.is_test_unit(&origin.0) {
                units.test_units.insert(declared.to_string());
            } else {
                units.source_units.insert(declared.to_string());
            }
        }
        if kin_model::is_file_module_surface(entity) || !own_namespace {
            continue;
        }
        // A test unit's own namespace (Go's external test package) shares
        // the directory but not the namespace.
        if declared.is_none_or(|declared| declared == name) {
            units.members.push(Member {
                name: entity.name.clone(),
                kind: entity.kind,
            });
        }
    }
    units
}

struct PlannedUnit<'a> {
    unit: &'a SourceUnit,
    editor: &'static dyn UnitEditor,
    creates: Vec<&'a EntityCreate>,
    add: Vec<UnitImport>,
    remove: Vec<String>,
}

fn load_unit_bytes(
    state: &DaemonState,
    authority_context: &LocalRepositoryAuthorityContext,
    hash: Hash256,
) -> Result<Vec<u8>, String> {
    let digest = kin_blobs::Hash256::from_bytes(*hash.as_bytes());
    if state
        .blobs
        .exists(&digest)
        .map_err(|error| error.to_string())?
    {
        return state.blobs.read(&digest).map_err(|error| error.to_string());
    }
    load_native_source_blob(authority_context, hash).map_err(|error| error.to_string())
}

/// Plan every unit-addressed operation onto `prospective` and return the unit
/// paths written. Nothing is published here; a refusal leaves authority as it
/// was, because the caller only publishes a plan that returned.
pub(crate) fn plan_unit_operations(
    state: &DaemonState,
    authority_context: &LocalRepositoryAuthorityContext,
    prospective: &kin_db::InMemoryGraph,
    operations: &[McpMutationOperation],
    layouts: &mut Vec<FileLayout>,
) -> Result<BTreeSet<RepoPath>, String> {
    let requests = operations
        .iter()
        .filter_map(unit_request)
        .collect::<Vec<_>>();
    if requests.is_empty() {
        return Ok(BTreeSet::new());
    }
    let tree = prospective.resolved_tree();
    let mut units: Vec<(RepoPath, PlannedUnit<'_>)> = Vec::new();
    for (unit, request) in requests {
        let path = kin_mcp::source_unit::unit_projection_path(unit, &tree)?;
        let index = match units.iter().position(|(held, _)| *held == path) {
            Some(index) => index,
            None => {
                units.push((
                    path.clone(),
                    PlannedUnit {
                        unit,
                        editor: kin_mcp::source_unit::unit_editor(unit)?,
                        creates: Vec::new(),
                        add: Vec::new(),
                        remove: Vec::new(),
                    },
                ));
                units.len() - 1
            }
        };
        let planned = &mut units[index].1;
        if planned.unit != unit {
            return Err(format!(
                "{} and {} resolve to one source unit; address it with one identity",
                planned.unit.describe(),
                unit.describe()
            ));
        }
        match request {
            UnitRequest::Create(create) => {
                planned.creates.push(create);
                planned
                    .add
                    .extend(create.imports.iter().map(|import| import.unit()));
            }
            UnitRequest::Imports(imports) => {
                planned
                    .add
                    .extend(imports.add.iter().map(|import| import.unit()));
                planned.remove.extend(
                    imports
                        .remove
                        .iter()
                        .map(|import| import.path().to_string()),
                );
            }
        }
    }

    let before = prospective.to_snapshot();
    // Which unit names may share a directory is the language's to say (one
    // Go package and its external test package). Checked across the graph
    // and every unit this transaction plans, so one transaction cannot
    // publish a mixed directory that no single unit would reveal.
    let mut directories: BTreeMap<
        String,
        (
            BTreeSet<String>,
            BTreeSet<String>,
            &SourceUnit,
            &dyn UnitEditor,
        ),
    > = BTreeMap::new();
    for (path, planned) in &units {
        let directory = directory_of(&path.to_string()).to_string();
        let (sources, tests, _, _) = directories.entry(directory.clone()).or_insert_with(|| {
            let held = directory_units(
                &before,
                planned.editor,
                planned.unit.language(),
                &path.to_string(),
                planned.unit.package_name(),
            );
            (
                held.source_units,
                held.test_units,
                planned.unit,
                planned.editor,
            )
        });
        match planned.unit.role() {
            UnitRole::Source => sources.insert(planned.unit.package_name().to_string()),
            UnitRole::Test => tests.insert(planned.unit.package_name().to_string()),
        };
    }
    for (sources, tests, unit, editor) in directories.values() {
        editor.check_directory(sources, tests).map_err(|error| {
            format!(
                "{} is occupied: {error}; no unit was written",
                unit.describe()
            )
        })?;
    }

    // Names and types every creation in this transaction declares, by package
    // directory and package name, so two units of one package cannot both
    // declare a name and a method may name a type created in the same
    // transaction.
    let mut created_names: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut created_types: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut parsed_creates: BTreeMap<*const EntityCreate, UnitDeclaration> = BTreeMap::new();
    for (path, planned) in &units {
        let package = (
            directory_of(&path.to_string()).to_string(),
            planned.unit.package_name().to_string(),
        );
        for create in &planned.creates {
            let declaration = planned
                .editor
                .parse_declaration(&create.body)
                .map_err(|error| format!("EntityCreate {}: {error}", create.name))?;
            let names = created_names.entry(package.clone()).or_default();
            for name in &declaration.names {
                if !names.insert(name.clone()) {
                    return Err(format!(
                        "two creations in this transaction declare {name} in {}",
                        planned.unit.describe()
                    ));
                }
            }
            if declaration.kind.declares_type() {
                created_types
                    .entry(package.clone())
                    .or_default()
                    .extend(declaration.names.iter().cloned());
            }
            parsed_creates.insert(*create as *const EntityCreate, declaration);
        }
    }

    let mut reconciler = kin_reconcile::Reconciler::new(PathBuf::new());
    reconciler.seed_cross_file_linker_from_graph(prospective);
    let mut written = BTreeSet::new();
    for (path, planned) in &units {
        let path_text = path.to_string();
        let directory = directory_of(&path_text).to_string();
        let name = planned.unit.package_name();
        let package = directory_units(
            &before,
            planned.editor,
            planned.unit.language(),
            &path_text,
            name,
        );
        let package_key = (directory.clone(), name.to_string());
        for create in &planned.creates {
            let declaration = &parsed_creates[&(*create as *const EntityCreate)];
            if let Some(taken) = declaration.names.iter().find(|created| {
                package
                    .members
                    .iter()
                    .any(|member| &member.name == *created)
            }) {
                return Err(format!(
                    "{taken} already names a declaration in {}; no declaration was overwritten",
                    planned.unit.describe()
                ));
            }
            if let Some(receiver) = declaration.receiver.as_deref() {
                let declared = package.members.iter().any(|member| {
                    member.name == receiver
                        && matches!(
                            member.kind,
                            EntityKind::Class | EntityKind::Interface | EntityKind::TypeAlias
                        )
                }) || created_types
                    .get(&package_key)
                    .is_some_and(|types| types.contains(receiver));
                if !declared {
                    return Err(format!(
                        "method {} names receiver type {receiver}, which {} does not declare; \
                         create the type first, in this transaction or an earlier one",
                        create.name,
                        planned.unit.describe()
                    ));
                }
            }
        }

        let artifact = tree.artifact_at_path(path).cloned();
        let (existing, executable) = match artifact.as_ref().map(|artifact| &artifact.entry) {
            None => (None, false),
            Some(TreeEntry::Blob { hash, executable }) => (
                Some(load_unit_bytes(state, authority_context, *hash)?),
                *executable,
            ),
            Some(_) => {
                return Err(format!(
                    "{} is occupied by a link rather than source; no existing artifact was \
                     overwritten",
                    planned.unit.describe()
                ))
            }
        };
        let edit = UnitEdit {
            declarations: planned
                .creates
                .iter()
                .map(|create| create.body.clone())
                .collect(),
            add_imports: planned.add.clone(),
            remove_imports: planned.remove.clone(),
        };
        let outcome = planned
            .editor
            .edit(existing.as_deref(), name, &edit)
            .map_err(|error| format!("{}: {error}", planned.unit.describe()))?;
        if existing.as_deref() == Some(outcome.source.as_slice()) {
            // Idempotent import management: nothing to publish for this unit.
            continue;
        }

        let file_id = FilePathId::new(path_text.clone());
        let digest = state
            .blobs
            .write(&outcome.source)
            .map_err(|error| format!("store {}: {error}", planned.unit.describe()))?;
        let hash = Hash256::from_bytes(digest.0);
        let delta = match artifact {
            Some(artifact) => TreeDelta::Updated {
                artifact_id: artifact.artifact_id,
                old: artifact.located_entry(),
                new: LocatedEntry::new(path.clone(), TreeEntry::blob(hash, executable)),
            },
            None => TreeDelta::Added {
                artifact_id: kin_model::ArtifactId::new(),
                new: LocatedEntry::new(path.clone(), TreeEntry::blob(hash, false)),
            },
        };
        prospective
            .apply_transaction_delta(&TransactionDelta {
                tree_deltas: vec![delta],
                ..TransactionDelta::default()
            })
            .map_err(|error| {
                format!(
                    "install {} in the exact tree: {error}",
                    planned.unit.describe()
                )
            })?;
        let manifests = kin_index::GraphProjectFiles::with_reader(
            prospective,
            crate::source_cas::source_reader_opening(state.blobs.as_ref(), authority_context),
        );
        let indexed =
            kin_index::shared_index::index_any(&file_id, &outcome.source, digest, &manifests)
                .map_err(|error| format!("parse {}: {error}", planned.unit.describe()))?;
        let kin_index::IndexedAny::EntitySource(indexed) = indexed else {
            return Err(format!(
                "{} does not classify as entity source",
                planned.unit.describe()
            ));
        };
        let reconcile = reconciler
            .reconcile_indexed_content(&indexed, state.blobs.as_ref(), prospective)
            .map_err(|error| format!("derive {}: {error}", planned.unit.describe()))?;
        prospective
            .apply_transaction_delta(&reconcile.delta)
            .map_err(|error| format!("apply {}: {error}", planned.unit.describe()))?;
        let layout = reconciler
            .projection()
            .get_layout(&file_id)
            .cloned()
            .ok_or_else(|| format!("{} produced no file layout", planned.unit.describe()))?;
        prospective
            .upsert_file_layout(&layout)
            .map_err(|error| format!("install layout for {}: {error}", planned.unit.describe()))?;
        layouts.retain(|held| held.file_id != file_id);
        layouts.push(layout);

        validate_unit_footprint(
            &before,
            prospective,
            &file_id,
            existing.as_deref(),
            &outcome.source,
            &outcome.created,
        )
        .map_err(|error| format!("{}: {error}", planned.unit.describe()))?;
        written.insert(path.clone());
    }
    Ok(written)
}

/// Prove the unit changed exactly as requested: existing declarations keep
/// their names, kinds and exact bytes, and every new entity is a requested
/// declaration or a member nested inside one.
fn validate_unit_footprint(
    before: &kin_db::GraphSnapshot,
    after: &kin_db::InMemoryGraph,
    file: &FilePathId,
    old_body: Option<&[u8]>,
    new_body: &[u8],
    created: &[UnitDeclaration],
) -> Result<(), String> {
    let mut held = BTreeSet::new();
    for old in before.entities.values().filter(|entity| {
        entity.file_origin.as_ref() == Some(file) && !kin_model::is_file_module_surface(entity)
    }) {
        held.insert(old.id);
        let new = after
            .get_entity(&old.id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("the edit removed existing declaration {}", old.name))?;
        let (Some(old_span), Some(new_span)) = (old.span.as_ref(), new.span.as_ref()) else {
            return Err(format!(
                "declaration {} lost its exact source span",
                old.name
            ));
        };
        if new.name != old.name
            || new.kind != old.kind
            || new.file_origin != old.file_origin
            || old_body.and_then(|body| body.get(old_span.start_byte..old_span.end_byte))
                != new_body.get(new_span.start_byte..new_span.end_byte)
        {
            return Err(format!(
                "the edit changed existing declaration {}; nothing was published",
                old.name
            ));
        }
    }
    let added = after
        .query_entities(&kin_model::EntityFilter {
            file_path: Some(file.clone()),
            ..Default::default()
        })
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|entity| !kin_model::is_file_module_surface(entity) && !held.contains(&entity.id))
        .collect::<Vec<_>>();
    for entity in &added {
        let span = entity
            .span
            .as_ref()
            .ok_or_else(|| format!("created entity {} has no source span", entity.name))?;
        if !created.iter().any(|declaration| {
            declaration.range.start <= span.start_byte && span.end_byte <= declaration.range.end
        }) {
            return Err(format!(
                "the edit derived {} outside the requested declarations",
                entity.name
            ));
        }
    }
    for declaration in created {
        for name in &declaration.names {
            if !added
                .iter()
                .any(|entity| &entity.name == name && entity.kind == declaration.kind.entity_kind())
            {
                return Err(format!(
                    "the requested {} {name} did not derive as exactly that entity",
                    declaration.kind.label()
                ));
            }
        }
    }
    Ok(())
}

/// Top-level names each staged creation declares, for the commit reply.
pub(crate) fn requested_names(operations: &[McpMutationOperation]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for operation in operations {
        let Some(McpMutationPayload::EntityCreate(create)) = operation.payload.as_ref() else {
            continue;
        };
        match create.unit_target() {
            Some((_, unit)) => {
                if let Some(declaration) = kin_mcp::source_unit::unit_editor(unit)
                    .ok()
                    .and_then(|editor| editor.parse_declaration(&create.body).ok())
                {
                    names.extend(declaration.names);
                }
            }
            None => {
                names.insert(create.name.clone());
            }
        }
    }
    names
}
