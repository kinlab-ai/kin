// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Guarded patches to a module's own code, and to a class's.
//!
//! A class too large to be served whole is served the way a module is
//! ([`kin_mcp::module_code::serves_class_code`]): its own code with one marker
//! line per member. A patch whose source base names such a class is planned
//! here too, and proven to have changed nothing but that code.
//!
//! A patch whose source base names a module surface addresses that module's
//! own code: its imports and the top-level statements outside every entity it
//! holds ([`kin_mcp::module_code`]). This planner resolves the module's exact
//! bytes from the blob store or repository CAS (never the working copy),
//! applies the anchored edits to the code as it was served, puts every held
//! entity back as its exact bytes, and admits the result through the same
//! reconcile path every source edit takes, so the commit publishes and
//! projects it exactly as it does an entity edit.
//!
//! The footprint is then proven: every function, class and member the module
//! held keeps its identity and its exact bytes, and the only entities the
//! patch adds or drops are top-level values such as a constant, which are
//! module-level code themselves. A patch therefore cannot overwrite a file,
//! and cannot create or remove a function or class.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use kin_mcp::module_code::{kind_word, ModuleCode};
use kin_mcp::source_base::EntitySourcePatch;
use kin_mcp::{McpMutationOperation, McpMutationPayload};
use kin_model::{
    Entity, EntityFilter, EntityStore, FileLayout, FilePathId, Hash256, LocatedEntry, RepoPath,
    TransactionDelta, TreeDelta, TreeEntry,
};

use crate::repository_commit::{load_native_source_blob_from, NativeCommitBase};
use crate::state::DaemonState;

/// The repository authority a commit already holds, which reads a committed
/// body without opening the store again.
type HeldAuthority = kin_db::RepositoryAuthorityManager<kin_db::LocalFileBackend>;

/// The module patch an operation carries: an anchored patch whose source base
/// names a module whose own code Kin serves, as repository authority holds it.
fn module_patch<'a>(
    base: &NativeCommitBase,
    operation: &'a McpMutationOperation,
) -> Result<Option<(&'a EntitySourcePatch, Entity)>, String> {
    let Some(McpMutationPayload::EntitySourcePatch(patch)) = operation.payload.as_ref() else {
        return Ok(None);
    };
    let entity = base
        .graph
        .get_entity(&patch.source_base.entity_id)
        .map_err(|error| error.to_string())?;
    Ok(entity
        .filter(|entity| serves_own_code(&base.graph, entity))
        .map(|module| (patch, module)))
}

/// Whether `entity` is served, and so patched, as its own code: a module
/// whose code Kin serves, or a class too large to be served whole.
pub(crate) fn serves_own_code(graph: &kin_db::InMemoryGraph, entity: &Entity) -> bool {
    kin_model::serves_module_code(entity) || kin_mcp::module_code::serves_class_code(graph, entity)
}

/// Whether `operation` patches a module's own code, which
/// [`plan_module_patches`] plans, rather than an entity body.
pub(crate) fn is_module_patch(base: &NativeCommitBase, operation: &McpMutationOperation) -> bool {
    matches!(module_patch(base, operation), Ok(Some(_)))
}

/// The exact bytes of `hash`: a body this transaction wrote is in the blob
/// store, and a committed one is in repository CAS.
fn load_bytes(
    blobs: Option<&kin_blobs::BlobStore>,
    authority: &HeldAuthority,
    hash: Hash256,
) -> Result<Vec<u8>, String> {
    if let Some(blobs) = blobs {
        let digest = kin_blobs::Hash256::from_bytes(*hash.as_bytes());
        if blobs.exists(&digest).map_err(|error| error.to_string())? {
            return blobs.read(&digest).map_err(|error| error.to_string());
        }
    }
    load_native_source_blob_from(authority, hash).map_err(|error| error.to_string())
}

/// One module's file as a graph holds it: its artifact, bytes and code.
struct HeldModule {
    artifact: kin_model::ResolvedArtifact,
    hash: Hash256,
    executable: bool,
    bytes: Vec<u8>,
    entities: Vec<Entity>,
    code: ModuleCode,
}

fn reread(module: &Entity) -> String {
    format!(
        "read {} again with source and patch against that read",
        module.name
    )
}

fn held_module(
    blobs: Option<&kin_blobs::BlobStore>,
    authority: &HeldAuthority,
    graph: &kin_db::InMemoryGraph,
    tree: &kin_model::ResolvedTree,
    module_id: kin_model::EntityId,
) -> Result<(Entity, RepoPath, HeldModule), String> {
    let module = graph
        .get_entity(&module_id)
        .map_err(|error| error.to_string())?
        .filter(|entity| serves_own_code(graph, entity))
        .ok_or_else(|| {
            format!(
                "entity {module_id} is no longer a module or a class Kin serves as its own \
                 code, so read it again with source and patch against that read"
            )
        })?;
    let file = module
        .file_origin
        .clone()
        .ok_or_else(|| format!("module {} has no source unit", module.name))?;
    let path = RepoPath::from_utf8(file.0.clone()).map_err(|error| error.to_string())?;
    let artifact = tree
        .artifact_at_path(&path)
        .cloned()
        .ok_or_else(|| format!("the source of module {} is no longer tracked", module.name))?;
    let TreeEntry::Blob { hash, executable } = artifact.entry else {
        return Err(format!(
            "the source of module {} is a link rather than source",
            module.name
        ));
    };
    let bytes = load_bytes(blobs, authority, hash)?;
    let entities = graph
        .query_entities(&EntityFilter {
            file_path: Some(file.clone()),
            ..Default::default()
        })
        .map_err(|error| error.to_string())?;
    // The code is cut along every recorded span of the module, so each must
    // be derived from exactly these bytes, as it was for the read.
    for entity in entities.iter().filter(|entity| entity.span.is_some()) {
        let coherent =
            kin_mcp::handlers::common::span_source_coherence(entity, &hash, &file.0).ok();
        if coherent != Some(kin_mcp::handlers::common::SpanCoherence::DigestVerified) {
            return Err(format!(
                "Kin has not derived every entity of module {} from its current source, so its \
                 module-level code cannot be cut exactly; {}",
                module.name,
                reread(&module)
            ));
        }
    }
    let code = ModuleCode::build(&module, &bytes, &entities)?;
    Ok((
        module,
        path,
        HeldModule {
            artifact,
            hash,
            executable,
            bytes,
            entities,
            code,
        },
    ))
}

/// Check the base a patch to a module's own code carries, as repository
/// authority holds the module now: the same artifact, and module-level code
/// of the length and digest the caller read.
///
/// A module's base guards its own code with its entities collapsed, never the
/// bytes of its span, which are its file. So it holds through an edit to the
/// body of any entity in the module, as an entity's base holds through edits
/// beside it, and is refused once the module's own code has changed.
pub(crate) fn require_module_base(
    authority: &HeldAuthority,
    base: &NativeCommitBase,
    module: &Entity,
    expected: &kin_mcp::source_base::EntitySourceBase,
) -> Result<(), String> {
    let (module, _, held) = held_module(None, authority, &base.graph, &base.tree, module.id)?;
    if held.artifact.artifact_id != expected.artifact_id {
        return Err(format!(
            "the source artifact for {} {} changed",
            held.code.noun(),
            module.name
        ));
    }
    if !held.code.is_guarded_by(expected) {
        return Err(format!(
            "{} differs from the caller's source read",
            held.code.label()
        ));
    }
    Ok(())
}

/// Plan every module patch onto `prospective` and return the paths written.
/// Nothing is published here; a refusal leaves authority as it was, because
/// the caller only publishes a plan that returned.
///
/// Anchors are read against the code as repository authority holds it, which
/// is the code the patch's source base was read from. Other operations of the
/// same transaction may already have changed the module's file in
/// `prospective` (an entity edit, a function created beside another); the
/// patch then lands on that code, and is refused when it no longer reads the
/// same around its anchors.
pub(crate) fn plan_module_patches(
    state: &DaemonState,
    authority: &HeldAuthority,
    base: &NativeCommitBase,
    prospective: &kin_db::InMemoryGraph,
    operations: &[McpMutationOperation],
    layouts: &mut Vec<FileLayout>,
) -> Result<BTreeSet<RepoPath>, String> {
    let mut patches: BTreeMap<kin_model::EntityId, (&EntitySourcePatch, Entity)> = BTreeMap::new();
    for operation in operations {
        let Some((patch, module)) = module_patch(base, operation)? else {
            continue;
        };
        if patches.insert(module.id, (patch, module.clone())).is_some() {
            return Err(format!(
                "{} is patched more than once in one transaction, so send every edit to its \
                 own code in one patch's edits",
                module.name
            ));
        }
    }
    if patches.is_empty() {
        return Ok(BTreeSet::new());
    }

    let mut reconciler = kin_reconcile::Reconciler::new(PathBuf::new());
    reconciler.seed_cross_file_linker_from_graph(prospective);
    let mut written = BTreeSet::new();
    for (module_id, (patch, _)) in patches {
        // What the footprint of this patch is proven against: the graph as
        // the operations and patches before it left it.
        let before = prospective.to_snapshot();
        // The code the caller read: the module as authority holds it, which
        // the base the patch carries must still guard.
        let (module, path, read) =
            held_module(None, authority, &base.graph, &base.tree, module_id)?;
        if read.artifact.artifact_id != patch.source_base.artifact_id
            || !read.code.is_guarded_by(&patch.source_base)
        {
            return Err(format!(
                "{} changed since it was read, so {}",
                read.code.label(),
                reread(&module)
            ));
        }
        let patched = read.code.apply(&read.bytes, &patch.edits)?;

        // The same module as this transaction's earlier operations left it.
        let tree = prospective.resolved_tree();
        let (_, _, current) = held_module(
            Some(state.blobs.as_ref()),
            authority,
            prospective,
            &tree,
            module_id,
        )?;
        let projected = if current.hash == read.hash {
            patched
        } else {
            current
                .code
                .apply(&current.bytes, &patch.edits)
                .map_err(|reason| {
                    format!(
                        "another operation of this transaction changed the code of {} this \
                         patch anchors on ({reason}), so send this patch in a transaction of \
                         its own",
                        module.name
                    )
                })?
        };

        // Never publish source that does not parse: read the result as its
        // language before anything of it is stored.
        kin_mcp::syntax_gate::refuse_unparseable(
            module.language,
            Some(&current.bytes),
            &projected,
            &[kin_mcp::syntax_gate::Sent::changed_run(
                format!("{} as patched", current.code.label()),
                &current.bytes,
                &projected,
            )],
        )?;
        let file_id = FilePathId::new(path.to_string());
        let digest = state
            .blobs
            .write(&projected)
            .map_err(|error| format!("store module {}: {error}", module.name))?;
        prospective
            .apply_transaction_delta(&TransactionDelta {
                tree_deltas: vec![TreeDelta::Updated {
                    artifact_id: current.artifact.artifact_id,
                    old: current.artifact.located_entry(),
                    new: LocatedEntry::new(
                        path.clone(),
                        TreeEntry::blob(Hash256::from_bytes(digest.0), current.executable),
                    ),
                }],
                ..TransactionDelta::default()
            })
            .map_err(|error| {
                format!("install module {} in the exact tree: {error}", module.name)
            })?;
        // The shared index, so the same bytes are parsed once in this
        // process however many readers ask for them, named under the
        // manifests of the tree they now stand in.
        let manifests =
            crate::source_cas::graph_manifests(prospective, state.blobs.as_ref(), Some(authority));
        let indexed = kin_index::shared_index::index_any(&file_id, &projected, digest, &manifests)
            .map_err(|error| format!("parse patched module {}: {error}", module.name))?;
        let kin_index::IndexedAny::EntitySource(indexed) = indexed else {
            return Err(format!(
                "patched module {} no longer classifies as source, so the patch was not admitted",
                module.name
            ));
        };
        let reconcile = reconciler
            .reconcile_indexed_content(&indexed, state.blobs.as_ref(), prospective)
            .map_err(|error| match error {
                kin_reconcile::ReconcileError::IncompleteParseWouldDelete { .. }
                | kin_reconcile::ReconcileError::BrokenAstRejected { .. } => {
                    // Reached only where the module already held a syntax
                    // problem beside the patch: one the patch brings is
                    // refused above, by the line of code it is on.
                    tracing::warn!(file = %file_id, "a patched module does not parse completely");
                    format!(
                        "module {} already has a syntax problem, and with this patch Kin can \
                         no longer tell which of its declarations remain, so fix that problem \
                         first with a patch to the declaration that holds it",
                        module.name
                    )
                }
                other => format!("derive patched module {}: {other}", module.name),
            })?;
        prospective
            .apply_transaction_delta(&reconcile.delta)
            .map_err(|error| format!("apply patched module {}: {error}", module.name))?;
        let layout = reconciler
            .projection()
            .get_layout(&file_id)
            .cloned()
            .ok_or_else(|| format!("patched module {} produced no file layout", module.name))?;
        prospective
            .upsert_file_layout(&layout)
            .map_err(|error| format!("install layout for module {}: {error}", module.name))?;
        layouts.retain(|held| held.file_id != file_id);
        layouts.push(layout);

        validate_module_footprint(
            &before,
            prospective,
            &module,
            &file_id,
            &current,
            &projected,
        )?;
        written.insert(path);
    }
    Ok(written)
}

/// Prove the patch changed the holder's own code only.
///
/// For a module: the module is the entity it was, every entity it held
/// behind a marker keeps its name, kind and exact bytes, with everything
/// nested inside it, and the only entities that appear or disappear are
/// top-level values.
///
/// For a class: the same of the class and its members, and further, the
/// class still ends where its patched code ends, every entity beside the
/// class keeps its name, kind and exact bytes, every entity the class stands
/// in keeps its name and kind, and no entity appears.
fn validate_module_footprint(
    before: &kin_db::GraphSnapshot,
    after: &kin_db::InMemoryGraph,
    module: &Entity,
    file: &FilePathId,
    held: &HeldModule,
    new_bytes: &[u8],
) -> Result<(), String> {
    let name = &module.name;
    let noun = held.code.noun();
    let reparsed = after
        .get_entity(&module.id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("the patch would remove {noun} {name} itself"))?;
    if reparsed.name != module.name || reparsed.kind != module.kind {
        return Err(format!("the patch would change what {noun} {name} is"));
    }
    // Where a class stood, and where it must stand now: from the same first
    // byte to the same last line, longer or shorter by what the patch added
    // or took away. A line the patch left outside the class, a statement at
    // the margin after its last member, is no longer the class's code.
    let class_span = (noun == "class")
        .then(|| held.entities.iter().find(|entity| entity.id == module.id))
        .flatten()
        .and_then(|class| class.span.as_ref())
        .map(|span| span.start_byte..span.end_byte);
    if let Some(stood) = &class_span {
        let stands = reparsed
            .span
            .as_ref()
            .map(|span| span.start_byte..span.end_byte);
        let grown = new_bytes.len() as isize - held.bytes.len() as isize;
        let expected = stood.start..(stood.end as isize + grown) as usize;
        if stands.as_ref() != Some(&expected) {
            return Err(format!(
                "the patch would leave code outside class {name}: every line of a class's own \
                 code stays inside the class, indented under its header, so check the \
                 indentation of new_text"
            ));
        }
    }
    let marked: BTreeSet<kin_model::EntityId> = held.code.held().map(|entity| entity.id).collect();
    let marked_spans: Vec<&kin_model::SourceSpan> = held
        .entities
        .iter()
        .filter(|entity| marked.contains(&entity.id))
        .filter_map(|entity| entity.span.as_ref())
        .collect();
    let behind_a_marker = |entity: &Entity| {
        marked.contains(&entity.id)
            || entity.span.as_ref().is_some_and(|span| {
                marked_spans.iter().any(|outer| {
                    outer.start_byte <= span.start_byte && span.end_byte <= outer.end_byte
                })
            })
    };
    let keeps_its_bytes = |old: &Entity| -> Result<bool, String> {
        Ok(after
            .get_entity(&old.id)
            .map_err(|error| error.to_string())?
            .filter(|new| new.name == old.name && new.kind == old.kind)
            .and_then(|new| new.span)
            .zip(old.span.as_ref())
            .is_some_and(|(new_span, old_span)| {
                held.bytes.get(old_span.start_byte..old_span.end_byte)
                    == new_bytes.get(new_span.start_byte..new_span.end_byte)
            }))
    };
    let mut existing = BTreeSet::new();
    for old in before
        .entities
        .values()
        .filter(|entity| entity.file_origin.as_ref() == Some(file) && entity.id != module.id)
    {
        existing.insert(old.id);
        if behind_a_marker(old) {
            if !keeps_its_bytes(old)? {
                return Err(format!(
                    "the patch to {noun} {name} would change {} {}, which its {noun}-level code \
                     holds only as a marker, so change {} by its own id (entity_id {}) and keep \
                     the {noun} patch to the code around it",
                    kind_word(old.kind),
                    old.name,
                    old.name,
                    old.id
                ));
            }
            continue;
        }
        let Some(stood) = &class_span else {
            // A top-level value is module-level code: the patch may change
            // or remove it.
            continue;
        };
        // What the class stands in holds the class's bytes, which changed:
        // its source unit's surface, and a class or function around it.
        let around = old
            .span
            .as_ref()
            .is_none_or(|span| span.start_byte <= stood.start && stood.end <= span.end_byte);
        let kept = if around {
            after
                .get_entity(&old.id)
                .map_err(|error| error.to_string())?
                .is_some_and(|new| new.name == old.name && new.kind == old.kind)
        } else {
            keeps_its_bytes(old)?
        };
        if !kept {
            return Err(format!(
                "the patch to class {name} would change {} {}, which is not part of the class, \
                 so change {} by its own id (entity_id {}) and keep the class patch to the \
                 class's own code",
                kind_word(old.kind),
                old.name,
                old.name,
                old.id
            ));
        }
    }
    let added = after
        .query_entities(&EntityFilter {
            file_path: Some(file.clone()),
            ..Default::default()
        })
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|entity| entity.id != module.id && !existing.contains(&entity.id));
    for entity in added {
        if class_span.is_some() {
            return Err(format!(
                "the patch to class {name} would declare {} {} in its class-level code, and a \
                 class patch adds statements, never a method or a class, so create {} with \
                 EntityCreate and placement member",
                kind_word(entity.kind),
                entity.name,
                entity.name
            ));
        }
        if !kin_model::is_module_value_kind(entity.kind) {
            return Err(format!(
                "the patch to module {name} would declare {} {} in its module-level code, and a \
                 module patch adds statements, never a function or class, so create {} with \
                 EntityCreate",
                kind_word(entity.kind),
                entity.name,
                entity.name
            ));
        }
    }
    Ok(())
}
