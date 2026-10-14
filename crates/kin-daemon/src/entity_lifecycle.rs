// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Source-bound declaration lifecycle planning, before native publication.
//! All locations come from exact graph/CAS authority; payloads name entities.
//!
//! A top-level leaf function is created beside another and removed in every
//! language whose adapter implements
//! [`kin_parser::declaration_lifecycle::DeclarationLifecycle`]. In every
//! language whose adapter implements
//! [`kin_parser::member_placement::MemberPlacement`], a method is also
//! created beside a sibling method or as the last member of a class named as
//! its container, a method is removed, and a class or a function is created
//! after a top-level class or function. None of those reads or resends the
//! class: the anchor's source base binds the write, and the new declaration
//! takes its indentation from where it goes (see
//! [`kin_mcp::member_placement`]). This planner names no language.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use kin_mcp::entity_lifecycle::{EntityCreate, EntityPlacement};
use kin_mcp::{McpMutationOperation, McpMutationPayload};
use kin_model::{
    Entity, EntityKind, EntityStore, FileLayout, FilePathId, Hash256, LanguageId, LocatedEntry,
    ParseState, RepoPath, TransactionDelta, TreeDelta, TreeEntry,
};

use kin_parser::declaration_lifecycle::{
    declaration_lifecycle_for, first_nested_declaration, lifecycle_languages,
    prove_top_level_function, DeclarationLifecycle,
};
use kin_parser::member_placement::{member_placement_for, member_placement_languages};

use crate::local_repository_authority::LocalRepositoryAuthorityContext;
use crate::repository_commit::load_native_source_blob;
use crate::state::DaemonState;

pub(crate) struct LifecyclePlan {
    pub snapshot: kin_db::GraphSnapshot,
    pub authored_files: BTreeSet<RepoPath>,
    pub layouts: Vec<FileLayout>,
}

/// Anchored creation and removal. Unit-addressed creation is planned by
/// `unit_lifecycle`, after entity edits, against the prospective graph.
pub(crate) fn is_lifecycle(operation: &McpMutationOperation) -> bool {
    match operation.payload.as_ref() {
        Some(McpMutationPayload::EntityCreate(create)) => create.anchor().is_some(),
        // An entity Kin holds without parsing it is removed by its own
        // planner ([`crate::artifact_commit`]).
        Some(McpMutationPayload::EntityRemove(remove)) => {
            !kin_mcp::artifact_entity::is_unparsed_base(&remove.source_base)
        }
        _ => false,
    }
}

/// The lifecycle hook of `language`'s adapter, or the refusal that names the
/// languages that have one.
fn lifecycle_for(language: LanguageId) -> Result<&'static dyn DeclarationLifecycle, String> {
    declaration_lifecycle_for(language).ok_or_else(|| {
        format!(
            "entity lifecycle currently supports top-level leaf functions in {}",
            lifecycle_languages("and")
        )
    })
}

/// A top-level leaf function proven in a unit's exact bytes.
struct Proven {
    /// Its entity span.
    span: Range<usize>,
    /// What goes with it: its whole statement and the doc comment its
    /// language reads as its own.
    extent: Range<usize>,
    /// What a new unit beside this one must open with.
    prelude: Option<String>,
}

/// Prove the exact declaration is top-level syntax, not merely an entity whose
/// name happens to match. Parser-widened docs/attributes must be inside its
/// span, and any other declaration prefix inside its statement.
fn function_span(
    source: &[u8],
    file: &FilePathId,
    language: LanguageId,
    name: &str,
) -> Result<Proven, String> {
    let lifecycle = lifecycle_for(language)?;
    let registry = kin_parser::AdapterRegistry::new();
    let adapter = registry
        .get_by_language(language)
        .ok_or("missing lifecycle parser")?;
    let tree = adapter.parse(source).map_err(|error| error.to_string())?;
    if tree.root_node().has_error() {
        return Err("entity lifecycle requires complete valid source syntax".into());
    }
    let parsed = adapter
        .extract(&tree, source, file)
        .map_err(|error| error.to_string())?;
    if !matches!(parsed.parse_state, ParseState::Valid) {
        return Err("entity lifecycle refuses an incomplete parse".into());
    }
    let matches = parsed
        .entities
        .iter()
        .filter(|entity| entity.kind == EntityKind::Function && entity.name == name)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(format!(
            "lifecycle function {name:?} is absent or ambiguous in its exact owner"
        ));
    }
    let entity = matches[0];
    let span = entity.span.start_byte..entity.span.end_byte;
    let proven = (span.start < span.end && span.end <= source.len())
        .then(|| prove_top_level_function(lifecycle, &tree, source, &span, name))
        .flatten()
        .ok_or("entity lifecycle requires a proven top-level function including its declaration prefix")?;
    // Some adapters intentionally do not extract declarations nested inside
    // function bodies. Prove the leaf property from the actual syntax too.
    if first_nested_declaration(lifecycle, proven.declaration).is_some() {
        return Err(
            "entity lifecycle does not yet support functions owning nested declarations".into(),
        );
    }
    if parsed.entities.iter().any(|other| {
        !(std::ptr::eq(other, entity)
            // Adapter-created whole-file module metadata is not a nested declaration.
            || (matches!(other.kind, EntityKind::Module | EntityKind::Package)
                && ["module ", "package ", "namespace "].iter().any(|prefix| other.signature.strip_prefix(prefix).is_some_and(|tail| tail == file.0)))
        )
            && other.span.start_byte >= span.start
            && other.span.end_byte <= span.end
    }) {
        return Err(
            "entity lifecycle does not yet support functions owning nested declarations".into(),
        );
    }
    let prelude = lifecycle.unit_prelude(&tree, source)?;
    Ok(Proven {
        span,
        extent: proven.extent,
        prelude,
    })
}

fn declaration_body(
    create: &EntityCreate,
    anchor: &Entity,
    prelude: Option<&str>,
) -> Result<Vec<u8>, String> {
    create.validate()?;
    let prefix = prelude
        .map(|prelude| format!("{prelude}\n\n"))
        .unwrap_or_default();
    let candidate = format!("{prefix}{}", create.body);
    let file = anchor
        .file_origin
        .as_ref()
        .ok_or("anchor has no source owner")?;
    let proven = function_span(candidate.as_bytes(), file, anchor.language, &create.name)?;
    if proven.extent.start < prefix.len()
        || !candidate.as_bytes()[prefix.len()..proven.extent.start]
            .iter()
            .all(u8::is_ascii_whitespace)
        || !candidate.as_bytes()[proven.extent.end..]
            .iter()
            .all(u8::is_ascii_whitespace)
    {
        return Err("EntityCreate body must contain exactly the named function, not package/import statements or sibling declarations".into());
    }
    Ok(candidate.as_bytes()[proven.span].to_vec())
}

fn new_unit_path(anchor: &Entity, name: &str) -> Result<RepoPath, String> {
    let origin = anchor
        .file_origin
        .as_ref()
        .ok_or("anchor has no source owner")?;
    kin_mcp::entity_lifecycle::generated_source_path(origin, anchor.language, name)
}

/// Refuse a new unit beside the anchor's when its language cannot place one
/// there (see [`DeclarationLifecycle::check_new_unit_owner`]).
fn require_new_unit_owner(
    lifecycle: &dyn DeclarationLifecycle,
    anchor: &Entity,
    source: &[u8],
) -> Result<(), String> {
    let origin = anchor.file_origin.as_ref().ok_or("anchor has no owner")?;
    lifecycle.check_new_unit_owner(&origin.0, source)
}

/// What one lifecycle operation must leave in its source unit, checked
/// against the reconciled graph before anything is published.
struct Expected {
    file: FilePathId,
    /// The declaration created: its name as the graph names it, its kind,
    /// and its exact bytes.
    created: Option<(String, EntityKind, Vec<u8>)>,
    removed: Option<kin_model::EntityId>,
    /// The removed declaration's old bytes. A declaration nested in them
    /// goes with it.
    removed_span: Option<Range<usize>>,
    /// Declarations that hold the place written, a class around a new
    /// method: they keep their identity, and their bytes change.
    enclosing: BTreeSet<kin_model::EntityId>,
    /// Whether the created declaration may own declarations nested in it.
    nested: bool,
}

/// One planned write to a source unit.
struct Planned {
    projected: Vec<u8>,
    created: Option<(String, EntityKind, Vec<u8>)>,
    removed: Option<kin_model::EntityId>,
    removed_span: Option<Range<usize>>,
    enclosing: BTreeSet<kin_model::EntityId>,
}

/// The entities of `file` whose bytes hold `inner` and are not it: the
/// class around a method, and whatever that class is declared in.
fn enclosing_declarations(
    before: &kin_db::GraphSnapshot,
    file: &FilePathId,
    inner: &Range<usize>,
    inner_id: Option<kin_model::EntityId>,
) -> BTreeSet<kin_model::EntityId> {
    before
        .entities
        .values()
        .filter(|entity| {
            entity.file_origin.as_ref() == Some(file)
                && !kin_model::is_file_module_surface(entity)
                && Some(entity.id) != inner_id
                && entity.span.as_ref().is_some_and(|span| {
                    span.start_byte <= inner.start
                        && inner.end <= span.end_byte
                        && (span.start_byte..span.end_byte) != *inner
                })
        })
        .map(|entity| entity.id)
        .collect()
}

/// Whether an operation takes the member and type path: its anchor is a
/// method or a type, or it creates one, in a language whose adapter places
/// members.
fn is_member_declaration(anchor: &Entity, create: Option<&EntityCreate>) -> bool {
    use kin_mcp::source_unit::DeclarationKind;
    member_placement_for(anchor.language).is_some()
        && (matches!(anchor.kind, EntityKind::Method | EntityKind::Class)
            || create.is_some_and(|create| {
                matches!(
                    create.kind,
                    DeclarationKind::Method | DeclarationKind::Class
                )
            }))
}

/// Plan a method or type operation against the unit's exact bytes. The
/// anchor is proven to be one whole declaration at its recorded span; the
/// new declaration is written at the indentation of the place it goes.
fn plan_member_declaration(
    before: &kin_db::GraphSnapshot,
    anchor: &Entity,
    file: &FilePathId,
    original: &[u8],
    create: Option<(&EntityCreate, EntityPlacement)>,
) -> Result<Planned, String> {
    use kin_mcp::member_placement::{self as placement, Shape};
    use kin_mcp::source_unit::DeclarationKind;
    let language = anchor.language;
    let display = language.profile().display_name;
    let hook = member_placement_for(language).ok_or("the anchor's language places no member")?;
    let held = anchor
        .span
        .as_ref()
        .ok_or("lifecycle anchor has no source span")?;
    let anchor_span = held.start_byte..held.end_byte;
    let site = hook
        .locate(original, anchor_span.clone())
        .map_err(|error| format!("lifecycle anchor: {error}"))?;
    let anchored_as_recorded = match anchor.kind {
        EntityKind::Method => site.shape == Shape::Function && site.member,
        EntityKind::Class => site.shape == Shape::Class && (site.top_level || site.member),
        EntityKind::Function => site.shape == Shape::Function && site.top_level,
        _ => false,
    };
    if !anchored_as_recorded {
        return Err(format!(
            "entity lifecycle in {display} anchors on a method, a class, or a top-level function"
        ));
    }
    let occupied = |name: &str| {
        before.entities.values().any(|entity| {
            entity.file_origin.as_ref() == Some(file)
                && entity.name == name
                && !kin_model::is_file_module_surface(entity)
        })
    };
    let Some((create, how)) = create else {
        // Removal: a method, with whatever is declared inside it.
        if anchor.kind != EntityKind::Method {
            return Err(
                "entity lifecycle removes a method or a top-level leaf function; remove a class \
                 by patching the declarations around it"
                    .into(),
            );
        }
        // A class header with nothing under it may still parse, so the
        // tree is asked whether the method is all its class must hold.
        if site.only_statement {
            return Err(
                "removing this method would leave its class without a body; patch the class \
                 instead, so it keeps a statement"
                    .into(),
            );
        }
        let cut = placement::member_removal(original, &site);
        let projected = kin_projection::apply_splices(
            original,
            vec![kin_projection::Splice {
                byte_range: cut,
                new_content: Vec::new(),
            }],
        )
        .map_err(|error| error.to_string())?;
        if !hook.parses(&projected) {
            return Err(
                "removing this method would leave its source unit unable to parse; patch the \
                 class instead"
                    .into(),
            );
        }
        return Ok(Planned {
            projected,
            created: None,
            removed: Some(anchor.id),
            removed_span: Some(site.extent.clone()),
            enclosing: enclosing_declarations(before, file, &anchor_span, Some(anchor.id)),
        });
    };
    create.validate()?;
    // The owner and its member are joined the way the graph names them.
    let separator = kin_parser::name_syntax::name_syntax_for(language)
        .unwrap_or(kin_parser::name_syntax::NameSyntax::UNSTATED)
        .member_separator;
    let (kind, insertion, enclosing, name) = match (create.kind, how, anchor.kind) {
        // A method beside a method of the same type.
        (DeclarationKind::Method, EntityPlacement::SiblingAfter, EntityKind::Method) => {
            let owner = anchor
                .name
                .rsplit_once(separator)
                .map(|(owner, _)| owner)
                .ok_or("the anchor method names no class")?;
            (
                EntityKind::Method,
                placement::sibling_after(
                    language,
                    hook,
                    &site,
                    &create.body,
                    &create.name,
                    Shape::Function,
                )?,
                enclosing_declarations(before, file, &anchor_span, Some(anchor.id)),
                format!("{owner}{separator}{}", create.name),
            )
        }
        // A method as the last member of the type named as its container.
        (DeclarationKind::Method, EntityPlacement::Member, EntityKind::Class) => {
            let mut enclosing = enclosing_declarations(before, file, &anchor_span, Some(anchor.id));
            enclosing.insert(anchor.id);
            (
                EntityKind::Method,
                placement::last_member(
                    language,
                    hook,
                    &site,
                    &create.body,
                    &create.name,
                    Shape::Function,
                )?,
                enclosing,
                format!("{}{separator}{}", anchor.name, create.name),
            )
        }
        // A type or a function after a top-level type or function.
        (
            DeclarationKind::Class | DeclarationKind::Function,
            EntityPlacement::SiblingAfter,
            EntityKind::Class | EntityKind::Function,
        ) if site.top_level => {
            let (shape, kind) = match create.kind {
                DeclarationKind::Class => (Shape::Class, EntityKind::Class),
                _ => (Shape::Function, EntityKind::Function),
            };
            (
                kind,
                placement::sibling_after(language, hook, &site, &create.body, &create.name, shape)?,
                BTreeSet::new(),
                create.name.clone(),
            )
        }
        (DeclarationKind::Method, EntityPlacement::Member, _) => {
            return Err(
                "placement member needs a class as its anchor: send the class's source_base, \
                 or a sibling method's with placement sibling_after"
                    .into(),
            );
        }
        (DeclarationKind::Method, EntityPlacement::SiblingAfter, _) => {
            return Err(
                "a method is created beside a method of its class, with that method's \
                 source_base and placement sibling_after, or with the class's source_base and \
                 placement member"
                    .into(),
            );
        }
        (_, EntityPlacement::NewSourceUnit, _) => {
            return Err(
                "placement new_source_unit creates one top-level function; create a class or \
                 a method with sibling_after or member"
                    .into(),
            );
        }
        _ => {
            return Err(format!(
                "{display} creates a method beside a method (sibling_after) or in a class \
                 (member), and a class or a function after a top-level class or function \
                 (sibling_after)"
            ));
        }
    };
    if occupied(&name) {
        return Err(format!(
            "{name} already exists in the anchor's source unit; patch it instead of creating it"
        ));
    }
    let projected = kin_projection::apply_splices(
        original,
        vec![kin_projection::Splice {
            byte_range: insertion.at..insertion.at,
            new_content: insertion.text.clone().into_bytes(),
        }],
    )
    .map_err(|error| error.to_string())?;
    if !hook.parses(&projected) {
        return Err(format!(
            "the created declaration does not leave valid {display} where it goes"
        ));
    }
    Ok(Planned {
        projected,
        created: Some((name, kind, insertion.declaration.into_bytes())),
        removed: None,
        removed_span: None,
        enclosing,
    })
}

/// Keep cached admitted bodies. Missing bodies are recovered only from exact
/// repository CAS, as in authored merge preparation, never from host files.
fn ensure_source_cache(
    state: &DaemonState,
    context: &LocalRepositoryAuthorityContext,
    snapshot: &kin_db::GraphSnapshot,
) -> Result<(), String> {
    let paths = snapshot
        .entities
        .values()
        .filter_map(|entity| entity.file_origin.as_ref().map(|path| path.0.as_str()))
        .collect::<BTreeSet<_>>();
    let mut hashes = BTreeSet::new();
    for artifact in snapshot.resolved_tree.artifacts() {
        if paths.contains(artifact.path.to_string().as_str()) {
            if let TreeEntry::Blob { hash, .. } = artifact.entry {
                hashes.insert(hash);
            }
        }
    }
    for relation in snapshot
        .relations
        .values()
        .filter(|relation| kin_index::binding_debt::claims_local_binding_debt(relation))
    {
        let kin_model::GraphNodeId::Artifact(id) = relation.src else {
            return Err("binding debt has no source artifact".into());
        };
        let artifact = snapshot
            .resolved_tree
            .get(&id)
            .ok_or("binding debt artifact is absent")?;
        if let Some(debt) = kin_index::binding_debt::decode_local_binding_debt(
            &FilePathId::new(artifact.path.to_string()),
            id,
            relation,
        )
        .map_err(|error| error.to_string())?
        {
            hashes.extend(
                debt.obligations
                    .into_iter()
                    .map(|obligation| obligation.source_digest),
            );
        }
    }
    for hash in hashes {
        let digest = kin_blobs::Hash256::from_bytes(*hash.as_bytes());
        if !state
            .blobs
            .exists(&digest)
            .map_err(|error| error.to_string())?
        {
            let body = load_native_source_blob(context, hash).map_err(|error| error.to_string())?;
            if state
                .blobs
                .write(&body)
                .map_err(|error| error.to_string())?
                != digest
            {
                return Err("repository lifecycle source digest differs".into());
            }
        }
    }
    Ok(())
}

/// A removal takes every line of the declaration with it, so an edit to
/// anything written inside it in the same transaction would be planned and
/// then silently dropped. Refuse that pairing instead of reporting a commit
/// that lost work.
fn refuse_dropping_an_edit(
    graph: &kin_db::GraphSnapshot,
    edited: &std::collections::HashSet<kin_model::EntityId>,
    removed: &Entity,
    file: &FilePathId,
    removal: &Range<usize>,
) -> Result<(), String> {
    let inside = graph.entities.values().find(|entity| {
        entity.id != removed.id
            && edited.contains(&entity.id)
            && entity.file_origin.as_ref() == Some(file)
            && entity.span.as_ref().is_some_and(|held| {
                removal.start <= held.start_byte && held.end_byte <= removal.end
            })
    });
    match inside {
        Some(entity) => Err(format!(
            "removing {} would drop the edit to {} written inside it; remove it and edit {} in separate transactions",
            removed.name, entity.name, entity.name
        )),
        None => Ok(()),
    }
}

pub(crate) fn prepare(
    state: &DaemonState,
    context: &LocalRepositoryAuthorityContext,
    prospective_graph: &kin_db::InMemoryGraph,
    edited_entities: &std::collections::HashSet<kin_model::EntityId>,
    operations: &[McpMutationOperation],
) -> Result<Option<LifecyclePlan>, String> {
    if !operations.iter().any(is_lifecycle) {
        return Ok(None);
    }
    let before = prospective_graph.to_snapshot();
    ensure_source_cache(state, context, &before)?;
    let prospective =
        kin_db::InMemoryGraph::from_snapshot(before.clone()).map_err(|error| error.to_string())?;
    let mut expected = Vec::new();
    let mut created_package_names = BTreeSet::new();
    let mut authored_files = BTreeSet::new();
    let mut old_bodies = BTreeMap::new();
    let mut bodies = BTreeMap::new();
    for operation in operations
        .iter()
        .filter(|operation| is_lifecycle(operation))
    {
        let (source_base, create) = match operation.payload.as_ref().unwrap() {
            McpMutationPayload::EntityCreate(create) => {
                let (source_base, placement) =
                    create.anchor().ok_or("anchored creation lost its anchor")?;
                (source_base, Some((create, placement)))
            }
            McpMutationPayload::EntityRemove(remove) => {
                if edited_entities.contains(&remove.source_base.entity_id) {
                    return Err(
                        "an entity cannot be both edited and removed in one transaction".into(),
                    );
                }
                (&remove.source_base, None)
            }
            _ => unreachable!(),
        };
        let anchor = before
            .entities
            .get(&source_base.entity_id)
            .cloned()
            .ok_or("lifecycle anchor no longer exists")?;
        kin_model::require_targeted_source(&anchor)?;
        let member_declaration = is_member_declaration(&anchor, create.map(|(create, _)| create));
        if anchor.kind != EntityKind::Function && !member_declaration {
            return Err(format!(
                "entity lifecycle requires a top-level leaf function, or in {} a method or a \
                 class",
                member_placement_languages("or")
            ));
        }
        let file = anchor
            .file_origin
            .as_ref()
            .ok_or("lifecycle anchor has no source owner")?;
        let path = RepoPath::from_utf8(file.0.clone()).map_err(|error| error.to_string())?;
        let artifact = before
            .resolved_tree
            .artifact_at_path(&path)
            .ok_or("lifecycle source artifact is absent")?;
        let TreeEntry::Blob { hash, executable } = artifact.entry else {
            return Err("lifecycle source is not a regular blob".into());
        };
        let digest = kin_blobs::Hash256::from_bytes(*hash.as_bytes());
        let original = if state
            .blobs
            .exists(&digest)
            .map_err(|error| error.to_string())?
        {
            state
                .blobs
                .read(&digest)
                .map_err(|error| error.to_string())?
        } else {
            load_native_source_blob(context, hash).map_err(|error| error.to_string())?
        };
        let mut removed_span = None;
        let mut enclosing = BTreeSet::new();
        let mut nested = false;
        let (target, projected, created, removed) = if member_declaration {
            let planned = plan_member_declaration(&before, &anchor, file, &original, create)?;
            if planned.removed.is_some() {
                if let Some(removal) = planned.removed_span.as_ref() {
                    refuse_dropping_an_edit(&before, edited_entities, &anchor, file, removal)?;
                }
            }
            removed_span = planned.removed_span;
            enclosing = planned.enclosing;
            nested = true;
            (
                path.clone(),
                planned.projected,
                planned.created,
                planned.removed,
            )
        } else if let Some((create, placement)) = create {
            // The declaration as it is written: at the margin, whichever way
            // the caller indented it, and without the blank lines around it.
            let placed = EntityCreate {
                body: kin_parser::code_placement::at_margin(&create.body),
                ..create.clone()
            };
            let create = &placed;
            let lifecycle = lifecycle_for(anchor.language)?;
            let proven = function_span(&original, file, anchor.language, &anchor.name)?;
            let span = proven.span.clone();
            let held = anchor
                .span
                .as_ref()
                .ok_or("lifecycle anchor has no source span")?;
            if span != (held.start_byte..held.end_byte) {
                return Err(
                    "lifecycle anchor span differs from its exact parsed declaration".into(),
                );
            }
            // Said by its line in what was sent, before its shape is asked
            // about.
            kin_mcp::syntax_gate::refuse_unparseable_code(
                anchor.language,
                &create.body,
                &format!("the code sent for {}", create.name),
            )?;
            let declaration = declaration_body(create, &anchor, proven.prelude.as_deref())?;
            if lifecycle.package_scoped_names()
                && !created_package_names.insert((
                    std::path::Path::new(&file.0).parent().map(|p| p.to_owned()),
                    create.name.clone(),
                ))
            {
                return Err(format!(
                    "two creations name the same {} package member",
                    anchor.language.profile().display_name
                ));
            }

            if before.entities.values().any(|entity| {
                entity.name == create.name
                    && (entity.file_origin.as_ref() == Some(file)
                        || (lifecycle.package_scoped_names()
                            && entity.language == anchor.language
                            && entity.file_origin.as_ref().is_some_and(|origin| {
                                std::path::Path::new(&origin.0).parent()
                                    == std::path::Path::new(&file.0).parent()
                            })))
            }) {
                return Err(
                    "a declaration with that name already occupies the anchor's owner".into(),
                );
            }
            match placement {
                EntityPlacement::SiblingAfter => {
                    // As far from its neighbours as they stand from one
                    // another; with none to read, as far as the language
                    // sets top-level declarations apart.
                    let (at, before) = kin_parser::code_placement::after_sibling(
                        &original,
                        proven.extent.clone(),
                        lifecycle.sibling_separation(),
                    );
                    let mut insertion = before.into_bytes();
                    insertion.extend_from_slice(create.body.as_bytes());
                    let projected = kin_projection::apply_splices(
                        &original,
                        vec![kin_projection::Splice {
                            byte_range: at..at,
                            new_content: insertion,
                        }],
                    )
                    .map_err(|error| error.to_string())?;
                    (
                        path.clone(),
                        projected,
                        Some((create.name.clone(), EntityKind::Function, declaration)),
                        None,
                    )
                }
                EntityPlacement::Member => {
                    return Err(
                        "placement member puts a method in a class; create a function with \
                         sibling_after or new_source_unit"
                            .into(),
                    );
                }
                EntityPlacement::NewSourceUnit => {
                    require_new_unit_owner(lifecycle, &anchor, &original)?;
                    let target = new_unit_path(&anchor, &create.name)?;
                    if prospective
                        .resolved_tree()
                        .artifact_at_path(&target)
                        .is_some()
                    {
                        return Err("the generated source unit is already occupied; no existing artifact was overwritten".into());
                    }
                    let prefix = proven
                        .prelude
                        .as_ref()
                        .map(|prelude| format!("{prelude}\n\n"))
                        .unwrap_or_default();
                    (
                        target,
                        format!("{prefix}{}\n", create.body).into_bytes(),
                        Some((create.name.clone(), EntityKind::Function, declaration)),
                        None,
                    )
                }
            }
        } else {
            let proven = function_span(&original, file, anchor.language, &anchor.name)?;
            let span = proven.span.clone();
            let held = anchor
                .span
                .as_ref()
                .ok_or("lifecycle anchor has no source span")?;
            if span != (held.start_byte..held.end_byte) {
                return Err(
                    "lifecycle anchor span differs from its exact parsed declaration".into(),
                );
            }
            // The declaration goes with its own lines, its whole statement and
            // its doc comment, and the space that set it apart, so what stood
            // around it is as far apart as before.
            let removal = kin_parser::code_placement::removal_of(&original, proven.extent.clone());
            refuse_dropping_an_edit(&before, edited_entities, &anchor, file, &removal)?;
            let projected = kin_projection::apply_splices(
                &original,
                vec![kin_projection::Splice {
                    byte_range: removal,
                    new_content: Vec::new(),
                }],
            )
            .map_err(|error| error.to_string())?;
            (path.clone(), projected, None, Some(anchor.id))
        };
        // Never publish source that does not parse: read the result as its
        // language before anything of it is stored.
        if target == path {
            let sent = match create {
                Some((create, _)) => kin_mcp::syntax_gate::Sent::placed(
                    format!("the code sent for {}", create.name),
                    &original,
                    &projected,
                ),
                None => kin_mcp::syntax_gate::Sent::removed(
                    format!("removing {}", anchor.name),
                    &original,
                    &projected,
                ),
            };
            kin_mcp::syntax_gate::refuse_unparseable(
                anchor.language,
                Some(&original),
                &projected,
                &[sent],
            )?;
        }
        if !authored_files.insert(target.clone()) {
            return Err("multiple lifecycle operations on one source unit are not yet supported; split them into guarded transactions".into());
        }
        let target_file = FilePathId::new(target.to_string());
        let digest = state
            .blobs
            .write(&projected)
            .map_err(|error| error.to_string())?;
        let new_hash = Hash256::from_bytes(digest.0);
        let delta = if target == path {
            old_bodies.insert(file.0.clone(), original);
            TreeDelta::Updated {
                artifact_id: artifact.artifact_id,
                old: artifact.located_entry(),
                new: LocatedEntry::new(target, TreeEntry::blob(new_hash, executable)),
            }
        } else {
            TreeDelta::Added {
                artifact_id: kin_model::ArtifactId::new(),
                new: LocatedEntry::new(target, TreeEntry::blob(new_hash, false)),
            }
        };
        prospective
            .apply_transaction_delta(&TransactionDelta {
                tree_deltas: vec![delta],
                ..Default::default()
            })
            .map_err(|error| error.to_string())?;
        bodies.insert(target_file.0.clone(), projected);
        expected.push(Expected {
            file: target_file,
            created,
            removed,
            removed_span,
            enclosing,
            nested,
        });
    }
    let predecessor = kin_model::graph::ResolvedGraphState {
        entities: before.entities.clone(),
        relations: before.relations.clone(),
        external_references: before.external_references.clone(),
        tree: before.resolved_tree.clone(),
        ..Default::default()
    };
    let files = expected
        .iter()
        .map(|item| item.file.clone())
        .collect::<Vec<_>>();
    let prepared = kin_reconcile::Reconciler::prepare_admitted_source_batch(
        prospective.to_snapshot(),
        &files,
        &state.blobs,
        &[predecessor],
    )
    .map_err(|error| {
        let error = error.to_string();
        // Reached only where a unit already held a syntax problem beside the
        // change: one the change brings is refused above, by its line.
        if error.contains("has syntax errors") {
            tracing::warn!(%error, "a lifecycle operation writes a unit that does not parse");
            "the source this change writes already has a syntax problem, and Kin creates or \
             removes a declaration only where every declaration beside it can be read, so fix \
             that problem first with a patch to the declaration that holds it"
                .to_string()
        } else {
            format!("reconcile source-bound lifecycle: {error}")
        }
    })?;
    validate_footprint(
        &before,
        prepared.snapshot(),
        &expected,
        &old_bodies,
        &bodies,
    )?;
    let layouts = prepared
        .sources()
        .iter()
        .map(|source| source.layout.clone())
        .collect();
    Ok(Some(LifecyclePlan {
        snapshot: prepared.into_snapshot(),
        authored_files,
        layouts,
    }))
}

fn validate_footprint(
    before: &kin_db::GraphSnapshot,
    after: &kin_db::GraphSnapshot,
    expected: &[Expected],
    old_bodies: &BTreeMap<String, Vec<u8>>,
    bodies: &BTreeMap<String, Vec<u8>>,
) -> Result<(), String> {
    for item in expected {
        let body = &bodies[&item.file.0];
        let old_body = old_bodies.get(&item.file.0);
        for old in before.entities.values().filter(|entity| {
            entity.file_origin.as_ref() == Some(&item.file)
                && !kin_model::is_file_module_surface(entity)
        }) {
            if item.removed == Some(old.id) {
                if after.entities.contains_key(&old.id) {
                    return Err("removed function remains in the prepared graph".into());
                }
                continue;
            }
            let old_span = old
                .span
                .as_ref()
                .ok_or("lifecycle owner has an unbounded sibling declaration")?;
            // A declaration written inside the removed one goes with it.
            if item.removed_span.as_ref().is_some_and(|removed| {
                removed.start <= old_span.start_byte && old_span.end_byte <= removed.end
            }) {
                if after.entities.contains_key(&old.id) {
                    return Err(
                        "a declaration nested in the removed one remains in the prepared graph"
                            .into(),
                    );
                }
                continue;
            }
            let new = after
                .entities
                .get(&old.id)
                .ok_or("lifecycle unexpectedly removed a sibling declaration")?;
            let new_span = new
                .span
                .as_ref()
                .ok_or("lifecycle lost a sibling's independent source")?;
            if new.name != old.name || new.kind != old.kind || new.file_origin != old.file_origin {
                return Err("lifecycle changed an unrelated sibling declaration".into());
            }
            // A declaration that holds the place written keeps its identity
            // and takes the new bytes; every other keeps its bytes exactly.
            if !item.enclosing.contains(&old.id)
                && old_body.and_then(|body| body.get(old_span.start_byte..old_span.end_byte))
                    != body.get(new_span.start_byte..new_span.end_byte)
            {
                return Err("lifecycle changed an unrelated sibling declaration".into());
            }
        }
        let added = after
            .entities
            .values()
            .filter(|entity| {
                entity.file_origin.as_ref() == Some(&item.file)
                    && !kin_model::is_file_module_surface(entity)
                    && !before.entities.contains_key(&entity.id)
            })
            .collect::<Vec<_>>();
        if let Some((name, kind, desired)) = &item.created {
            let created = added
                .iter()
                .filter(|entity| entity.kind == *kind && entity.name == *name)
                .collect::<Vec<_>>();
            let [created] = created.as_slice() else {
                return Err("creation did not derive exactly the requested new declaration".into());
            };
            let span = created
                .span
                .as_ref()
                .ok_or("created declaration has no source span")?;
            if body.get(span.start_byte..span.end_byte) != Some(desired.as_slice()) {
                return Err(
                    "created declaration body differs from the requested declaration".into(),
                );
            }
            // Anything else that appeared is declared inside the new
            // declaration, where the operation allows that.
            let stray = added.iter().any(|entity| {
                entity.id != created.id
                    && !(item.nested
                        && entity.span.as_ref().is_some_and(|inner| {
                            span.start_byte <= inner.start_byte && inner.end_byte <= span.end_byte
                        }))
            });
            if stray {
                return Err("creation did not derive exactly the requested new declaration".into());
            }
        } else if !added.is_empty() {
            return Err("removal unexpectedly created a declaration".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_lifecycle_proves_top_level_leaf_spans() {
        for (language, source, expected, extent) in [
            (
                LanguageId::Rust,
                "// header\npub fn value() -> u8 { 1 }\n",
                "pub fn value() -> u8 { 1 }",
                "pub fn value() -> u8 { 1 }",
            ),
            (
                LanguageId::Python,
                "# header\ndef value():\n    return 1\n",
                "def value():\n    return 1",
                "def value():\n    return 1",
            ),
            (
                LanguageId::Go,
                "package sample\n\n// value is one.\nfunc value() int { return 1 }\n",
                // A Go entity's span holds the comments directly above it.
                "// value is one.\nfunc value() int { return 1 }",
                "// value is one.\nfunc value() int { return 1 }",
            ),
            (
                LanguageId::TypeScript,
                "// header\n\nexport function value(): number {\n  return 1;\n}\n",
                "export function value(): number {\n  return 1;\n}",
                "export function value(): number {\n  return 1;\n}",
            ),
            (
                LanguageId::JavaScript,
                "/** One. */\nexport const value = () => 1;\n",
                "export const value = () => 1;",
                "/** One. */\nexport const value = () => 1;",
            ),
        ] {
            let proven = function_span(
                source.as_bytes(),
                &FilePathId::new("fixture"),
                language,
                "value",
            )
            .unwrap();
            assert_eq!(&source[proven.span], expected);
            assert_eq!(&source[proven.extent], extent);
        }
        assert_eq!(
            function_span(
                b"package sample\n\nfunc value() int { return 1 }\n",
                &FilePathId::new("fixture"),
                LanguageId::Go,
                "value",
            )
            .unwrap()
            .prelude
            .as_deref(),
            Some("package sample")
        );
    }

    #[test]
    fn entity_lifecycle_refuses_nested_or_ambiguous_syntax() {
        for (language, source, name) in [
            (LanguageId::Rust, "mod owner { fn value() {} }", "value"),
            (LanguageId::Rust, "fn value() { fn nested() {} }", "value"),
            (
                LanguageId::Python,
                "class Owner:\n    def value(self):\n        return 1\n",
                "value",
            ),
            (
                LanguageId::Python,
                "def value():\n    def nested():\n        return 1\n    return nested()\n",
                "value",
            ),
            (
                LanguageId::Go,
                "package sample\nfunc (owner Owner) value() {}",
                "value",
            ),
            (LanguageId::Rust, "fn value() {}\nfn value() {}", "value"),
            (LanguageId::Rust, "fn value() {", "value"),
            (
                LanguageId::TypeScript,
                "function value() {\n  function nested() {}\n  return nested();\n}\n",
                "value",
            ),
            (
                LanguageId::JavaScript,
                "class Owner {\n  value() {\n    return 1;\n  }\n}\n",
                "value",
            ),
        ] {
            assert!(
                function_span(
                    source.as_bytes(),
                    &FilePathId::new("fixture"),
                    language,
                    name
                )
                .is_err(),
                "{source}"
            );
        }
    }

    #[test]
    fn a_language_without_a_lifecycle_is_refused_by_name() {
        let refusal = function_span(
            b"class Owner { void value() {} }",
            &FilePathId::new("fixture"),
            LanguageId::Java,
            "value",
        )
        .err()
        .unwrap();
        assert_eq!(
            refusal,
            "entity lifecycle currently supports top-level leaf functions in TypeScript, \
             JavaScript, Python, Go and Rust"
        );
    }
}
