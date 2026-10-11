// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Guarded changes to an entity Kin holds without parsing it.
//!
//! Such an entity is read as one ([`kin_mcp::artifact_entity`]) and changed
//! as one: by the same operations, against the `source_base` its read
//! returned. A base whose entity is the artifact itself marks the operation
//! as one of these. Its text is patched or replaced; content Kin already
//! holds, another unparsed entity's or its own at an earlier change, replaces
//! it or is given to a new one created beside an entity ([`EntityContent`]);
//! and it is removed. This planner resolves every content it reads from
//! repository authority (never the working copy), checks that the entity is
//! still what the caller read, and admits each result as one blob and one
//! tree transition, so the commit publishes and projects it exactly as it
//! does an entity edit.
//!
//! No parser runs, because none reads these: the change moves the tree and
//! nothing in the semantic graph. Source Kin parses is refused here by name,
//! since a change to it is a change to its entities.

use std::collections::{BTreeMap, BTreeSet};

use kin_mcp::artifact_entity::{self, EntityContent, Level};
use kin_mcp::source_base::{EntitySourceBase, EntitySourcePatch};
use kin_mcp::{McpMutationOperation, McpMutationPayload};
use kin_model::{
    ArtifactId, ChangeStore, EntityStore, FilePathId, Hash256, LocatedEntry, RepoPath,
    ResolvedArtifact, ResolvedTree, SemanticChange, SemanticChangeId, TransactionDelta, TreeDelta,
    TreeEntry,
};

use crate::repository_commit::{load_native_source_blob_from, NativeCommitBase};
use crate::state::DaemonState;

/// The repository authority a commit already holds, which reads a committed
/// body without opening the store again.
type HeldAuthority = kin_db::RepositoryAuthorityManager<kin_db::LocalFileBackend>;

/// The most changes one operation reads to find the change `at` names.
const CHANGE_WALK_MAX: usize = 20_000;

/// What one operation asks of an unparsed entity.
enum Asked<'a> {
    /// Anchored replacements in its text.
    Patch(&'a EntitySourcePatch),
    /// Its whole text, replaced.
    Text {
        base: &'a EntitySourceBase,
        text: &'a str,
    },
    /// Its content, replaced by content Kin already holds.
    Replace(&'a EntityContent),
    /// A new entity beside the one the base names.
    Create(&'a EntityContent),
    /// The entity, removed.
    Remove(&'a EntitySourceBase),
}

impl Asked<'_> {
    fn base(&self) -> &EntitySourceBase {
        match self {
            Self::Patch(patch) => &patch.source_base,
            Self::Text { base, .. } | Self::Remove(base) => base,
            Self::Replace(content) | Self::Create(content) => &content.source_base,
        }
    }

    /// Whether this asks something of one section of a text held with
    /// parts, rather than of a whole entity.
    fn of_section(&self) -> bool {
        artifact_entity::parts::is_part_id(&self.base().entity_id.0)
    }
}

/// What `operation` asks of an unparsed entity, or `None` for an operation on
/// anything else. A whole replacement with no text is an entity's to refuse.
fn asked(operation: &McpMutationOperation) -> Option<Asked<'_>> {
    match operation.payload.as_ref()? {
        McpMutationPayload::EntitySourcePatch(patch)
            if artifact_entity::is_unparsed_base(&patch.source_base) =>
        {
            Some(Asked::Patch(patch))
        }
        McpMutationPayload::EntitySourceBase(base) if artifact_entity::is_unparsed_base(base) => {
            Some(Asked::Text {
                base,
                text: operation.body.as_deref()?,
            })
        }
        McpMutationPayload::EntityContent(content) => {
            if operation.verb.trim().eq_ignore_ascii_case("create") {
                Some(Asked::Create(content))
            } else {
                Some(Asked::Replace(content))
            }
        }
        McpMutationPayload::EntityRemove(remove)
            if artifact_entity::is_unparsed_base(&remove.source_base) =>
        {
            Some(Asked::Remove(&remove.source_base))
        }
        _ => None,
    }
}

/// Whether `operation` changes an unparsed entity, which
/// [`plan_artifact_operations`] plans, rather than an entity of code.
pub(crate) fn is_artifact_operation(operation: &McpMutationOperation) -> bool {
    asked(operation).is_some()
}

/// One unparsed entity as repository authority holds it: its tree member,
/// its own name and its exact content.
struct Held {
    artifact: ResolvedArtifact,
    name: String,
    executable: bool,
    content: Vec<u8>,
}

/// The name an entity is read by: its own, never where it is projected.
fn name_of(path: &RepoPath) -> String {
    let place = String::from_utf8_lossy(path.as_bytes());
    place.rsplit('/').next().unwrap_or(&place).to_string()
}

fn reread(name: &str) -> String {
    format!(
        "read {name} again with source and send your change with the source_base that read returns"
    )
}

/// The unparsed entity a base names, as `tree` and `authority` hold it.
fn held(
    authority: &HeldAuthority,
    tree: &ResolvedTree,
    artifact_id: &ArtifactId,
) -> Result<Held, String> {
    let artifact = tree.get(artifact_id).cloned().ok_or(
        "the entity this source_base names is no longer held, so find it again with locate \
         and read it with source",
    )?;
    let name = name_of(&artifact.path);
    if artifact_entity::is_code(&artifact.path) {
        return Err(format!(
            "{name} is held as code, and a change to it is a change to its functions and \
             classes: read one with source and patch it with the source_base that read returns"
        ));
    }
    let TreeEntry::Blob { hash, executable } = artifact.entry else {
        return Err(format!(
            "{name} is a link, which Kin holds as bytes and does not change as text"
        ));
    };
    let content =
        load_native_source_blob_from(authority, hash).map_err(|error| error.to_string())?;
    Ok(Held {
        artifact,
        name,
        executable,
        content,
    })
}

/// Whether `expected` was read from exactly this content.
fn guards(held: &Held, expected: &EntitySourceBase) -> bool {
    expected.start_byte == 0
        && expected.end_byte == held.content.len()
        && kin_blobs::digest(&held.content).to_string() == expected.body_hash
}

/// Check the base an operation on an unparsed entity carries, as repository
/// authority holds the entity now: the same entity, with content of the
/// length and digest the caller read.
pub(crate) fn require_artifact_base(
    authority: &HeldAuthority,
    base: &NativeCommitBase,
    expected: &EntitySourceBase,
) -> Result<(), String> {
    let held = held(authority, &base.tree, &expected.artifact_id)?;
    if !guards(&held, expected) {
        return Err(format!(
            "{} differs from the caller's source read",
            held.name
        ));
    }
    Ok(())
}

/// One section of a text held with parts, found by the id its base names
/// in the text repository authority holds now.
struct HeldSection {
    label: String,
    start: usize,
    end: usize,
    /// Whether its text is the text the base was read from.
    unchanged: bool,
}

/// The section `expected` names in `read`, whose placed entity is `entity`.
fn section(
    read: &Held,
    entity: &artifact_entity::ArtifactEntity,
    expected: &EntitySourceBase,
) -> Result<HeldSection, String> {
    let text = std::str::from_utf8(&read.content).map_err(|_| {
        format!(
            "{} is held as bytes now and has no sections, so {}",
            read.name,
            reread(&read.name)
        )
    })?;
    let kind = artifact_entity::kind::describe(
        &entity.place(),
        &entity.name,
        true,
        &read.content[..read.content.len().min(4096)],
    )
    .kind;
    let sections = artifact_entity::parts::reader_for(&kind)
        .map(|reader| artifact_entity::parts::parts(reader, text))
        .unwrap_or_default();
    let ids = artifact_entity::parts::part_ids(entity.id(), &sections);
    let index = ids
        .iter()
        .position(|id| *id == expected.entity_id.0)
        .ok_or_else(|| {
            format!(
                "the section this source_base names is no longer in {}: its heading changed or \
                 it was removed, so {}",
                read.name,
                reread(&read.name)
            )
        })?;
    let part = &sections[index];
    Ok(HeldSection {
        label: format!(
            "{} :: {}",
            artifact_entity::parts::module_of(entity),
            part.title
        ),
        start: part.start,
        end: part.end,
        unchanged: kin_blobs::digest(&read.content[part.start..part.end]).to_string()
            == expected.body_hash,
    })
}

/// Check the base an operation on one section carries, as repository
/// authority holds its text now: the same section, with the text the caller
/// read. The rest of the text may have changed since.
pub(crate) fn require_section_base(
    authority: &HeldAuthority,
    base: &NativeCommitBase,
    expected: &EntitySourceBase,
) -> Result<(), String> {
    let read = held(authority, &base.tree, &expected.artifact_id)?;
    let members = members_of(authority, &base.graph, &base.tree)?;
    let found = section(&read, &members.placed(&read.artifact), expected)?;
    if !found.unchanged {
        return Err(format!(
            "{} differs from the caller's source read",
            found.label
        ));
    }
    Ok(())
}

/// The text the operations on sections of one text leave it with: each
/// section's text patched, replaced or removed, against the text repository
/// authority holds now, which may have changed outside these sections since
/// they were read.
fn changed_sections(
    read: &Held,
    entity: &artifact_entity::ArtifactEntity,
    asks: &[Asked<'_>],
) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(&read.content).map_err(|error| error.to_string())?;
    let mut changes: Vec<(usize, usize, String, String)> = Vec::new();
    for asked in asks {
        let found = section(read, entity, asked.base())?;
        if !found.unchanged {
            return Err(format!(
                "{} changed since it was read, so {}",
                found.label,
                reread(&found.label)
            ));
        }
        let current = &text[found.start..found.end];
        let new = match asked {
            Asked::Patch(patch) => {
                artifact_entity::apply_edits(&found.label, current, &patch.edits)?
            }
            Asked::Text {
                text: replacement, ..
            } => {
                if current.len() > kin_mcp::source_pages::FULL_BODY_MAX_BYTES {
                    return Err(format!(
                        "{} holds more text than one reply carries, so it is changed by patch \
                         and never replaced whole. Send a patch whose old_text is copied from an \
                         excerpt source returned",
                        found.label
                    ));
                }
                if *replacement == current {
                    return Err(format!(
                        "the update leaves the text of {} as it is, so nothing was written",
                        found.label
                    ));
                }
                replacement.to_string()
            }
            Asked::Remove(_) => String::new(),
            Asked::Replace(_) | Asked::Create(_) => {
                return Err(format!(
                    "{} is a section, and takes a patch, an update or a removal; content Kin \
                     holds replaces a whole entity",
                    found.label
                ))
            }
        };
        changes.push((found.start, found.end, new, found.label));
    }
    changes.sort_by_key(|change| change.0);
    for pair in changes.windows(2) {
        if pair[1].0 < pair[0].1 {
            return Err(format!(
                "{} holds {}, so change it in one operation on {}",
                pair[0].3, pair[1].3, pair[0].3
            ));
        }
    }
    let mut changed = text.to_string();
    for (start, end, new, _) in changes.into_iter().rev() {
        changed.replace_range(start..end, &new);
    }
    Ok(changed.into_bytes())
}

/// The text an operation leaves an entity with, or why it cannot.
fn changed_text(held: &Held, asked: &Asked<'_>) -> Result<Vec<u8>, String> {
    let name = &held.name;
    if Level::of(&held.content) != Level::Text {
        return Err(format!(
            "{name} is held as bytes and has no text to change. Replace it or remove it"
        ));
    }
    let text = std::str::from_utf8(&held.content).map_err(|error| error.to_string())?;
    match asked {
        Asked::Patch(patch) => {
            artifact_entity::apply_edits(name, text, &patch.edits).map(String::into_bytes)
        }
        Asked::Text {
            text: replacement, ..
        } => {
            if text.len() > kin_mcp::source_pages::FULL_BODY_MAX_BYTES {
                return Err(format!(
                    "{name} holds more text than one reply carries, so it is changed by patch \
                     and never replaced whole. Send a patch whose old_text is copied from an \
                     excerpt source returned"
                ));
            }
            if *replacement == text {
                return Err(format!(
                    "the update leaves the text of {name} as it is, so nothing was written"
                ));
            }
            Ok(replacement.as_bytes().to_vec())
        }
        _ => unreachable!("only a patch or an update changes text"),
    }
}

/// The unparsed entities of `tree`, each placed, with the project name the
/// tree's root manifest declares read from repository authority.
fn members_of<'t>(
    authority: &HeldAuthority,
    graph: &kin_db::InMemoryGraph,
    tree: &'t ResolvedTree,
) -> Result<artifact_entity::Held<'t>, String> {
    let root = artifact_entity::namespace::root_manifest(tree).and_then(|(manifest, hash)| {
        let content = load_native_source_blob_from(authority, hash).ok()?;
        artifact_entity::namespace::declared_project_name(
            &manifest,
            std::str::from_utf8(&content).ok()?,
        )
    });
    artifact_entity::Held::load(graph, tree, root)
}

/// The change on the first-parent line of `head` whose id begins with the
/// hex digits `at`.
fn change_named(
    graph: &kin_db::InMemoryGraph,
    head: SemanticChangeId,
    at: &str,
) -> Result<SemanticChangeId, String> {
    let digits = at.trim().to_ascii_lowercase();
    if digits.len() < 8 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "at `{at}` is not a change id: pass at least eight of its hex digits, as a history \
             line shows them"
        ));
    }
    let mut current = Some(head);
    let mut read = 0;
    while let Some(id) = current {
        if id.to_string().starts_with(&digits) {
            return Ok(id);
        }
        read += 1;
        if read >= CHANGE_WALK_MAX {
            break;
        }
        current = graph
            .get_change(&id)
            .ok()
            .flatten()
            .and_then(|change| change.parents.first().copied());
    }
    Err(format!(
        "at `{at}` names no change in the history Kin holds for this branch; pass a change id \
         as a history line shows it"
    ))
}

/// The content `content` gives: what `content_of` holds now, or held at the
/// change `at` names, read from repository authority.
fn given_content(
    authority: &HeldAuthority,
    base: &NativeCommitBase,
    content: &EntityContent,
) -> Result<Vec<u8>, String> {
    if let Some(text) = &content.text {
        return Ok(text.as_bytes().to_vec());
    }
    let source = content
        .content_of
        .as_deref()
        .ok_or("EntityContent needs content_of or text")?;
    let members = members_of(authority, &base.graph, &base.tree)?;
    let entity = match members.named(source) {
        artifact_entity::Named::One(entity) => entity,
        artifact_entity::Named::Code(code) => {
            return Err(format!(
                "{} is held as code, and its source is not given to an unparsed entity",
                code.name
            ))
        }
        artifact_entity::Named::Several(several) => {
            return Err(format!(
                "content_of `{source}` names {} unparsed entities, and takes one. Pass one of: \
                 {}",
                several.len(),
                members.addresses(&several).join(", ")
            ))
        }
        artifact_entity::Named::Nothing => {
            return Err(format!(
                "content_of `{source}` names no unparsed entity Kin holds, so find it with \
                 locate and pass the id or the module :: name its row shows"
            ))
        }
    };
    let entry = match &content.at {
        None => entity.artifact.entry,
        Some(at) => {
            let head = base
                .head
                .ok_or("the workspace holds no change yet, so at names none")?;
            let change = change_named(&base.graph, head, at)?;
            let then = base
                .graph
                .resolve_tree_at(&change)
                .map_err(|error| format!("read the tree at change {at}: {error}"))?;
            then.get(&entity.id())
                .ok_or_else(|| format!("{} was not held at change {at}", entity.name))?
                .entry
        }
    };
    let TreeEntry::Blob { hash, .. } = entry else {
        return Err(format!(
            "{} is a link, whose content Kin does not give to another entity",
            entity.name
        ));
    };
    load_native_source_blob_from(authority, hash).map_err(|error| error.to_string())
}

/// Where a new entity named `name` is placed beside the entity whose
/// artifact is `anchor`: the place that entity is projected in.
fn placed_beside(tree: &ResolvedTree, anchor: &ArtifactId, name: &str) -> Result<RepoPath, String> {
    artifact_entity::valid_name(name)?;
    let beside = tree.get(anchor).ok_or(
        "the entity this source_base names is no longer held, so find it again with locate \
         and read it with source",
    )?;
    let place = String::from_utf8_lossy(beside.path.as_bytes()).into_owned();
    let place = match place.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{name}"),
        None => name.to_string(),
    };
    let path = RepoPath::from_utf8(place).map_err(|error| error.to_string())?;
    kin_core::validate_source_paths([&path])
        .map_err(|error| format!("`{name}` cannot be held beside it: {error}"))?;
    if artifact_entity::is_code(&path) {
        return Err(format!(
            "{name} names source Kin parses, which is created through its entities with \
             EntityCreate, not as an unparsed entity"
        ));
    }
    Ok(path)
}

/// Plan every operation on an unparsed entity onto `prospective` and return
/// the places written. Nothing is published here; a refusal leaves authority
/// as it was, because the caller only publishes a plan that returned.
///
/// Each change is read against the content repository authority holds, which
/// is the content the operation's base was read from, and each entity takes
/// one operation in a transaction.
pub(crate) fn plan_artifact_operations(
    state: &DaemonState,
    authority: &HeldAuthority,
    base: &NativeCommitBase,
    prospective: &kin_db::InMemoryGraph,
    operations: &[McpMutationOperation],
) -> Result<BTreeSet<RepoPath>, String> {
    let mut planned: BTreeMap<ArtifactId, Asked<'_>> = BTreeMap::new();
    // Sections of one text are planned together, so several change it in one
    // transaction.
    let mut sections: BTreeMap<ArtifactId, Vec<Asked<'_>>> = BTreeMap::new();
    let mut created = Vec::new();
    for operation in operations {
        let Some(asked) = asked(operation) else {
            continue;
        };
        if let Asked::Create(content) = asked {
            created.push(content);
            continue;
        }
        let artifact_id = asked.base().artifact_id;
        let twice = if asked.of_section() {
            sections.entry(artifact_id).or_default().push(asked);
            planned.contains_key(&artifact_id)
        } else {
            planned.insert(artifact_id, asked).is_some() || sections.contains_key(&artifact_id)
        };
        if twice {
            return Err(
                "one entity is changed more than once in one transaction, so send every edit \
                 to it in one patch's edits"
                    .to_string(),
            );
        }
    }
    let mut written = BTreeSet::new();
    let store = |content: &[u8], name: &str| {
        state
            .blobs
            .write(content)
            .map(|digest| Hash256::from_bytes(digest.0))
            .map_err(|error| format!("store {name}: {error}"))
    };
    let install = |delta: TreeDelta, name: &str| {
        prospective
            .apply_transaction_delta(&TransactionDelta {
                tree_deltas: vec![delta],
                ..TransactionDelta::default()
            })
            .map_err(|error| format!("install {name} in the exact tree: {error}"))
    };
    for (artifact_id, asked) in planned {
        let read = held(authority, &base.tree, &artifact_id)?;
        if !guards(&read, asked.base()) {
            return Err(format!(
                "{} changed since it was read, so {}",
                read.name,
                reread(&read.name)
            ));
        }
        let content = match &asked {
            Asked::Remove(_) => {
                install(
                    TreeDelta::Removed {
                        artifact_id,
                        old: read.artifact.located_entry(),
                    },
                    &read.name,
                )?;
                written.insert(read.artifact.path.clone());
                continue;
            }
            Asked::Replace(content) => {
                let given = given_content(authority, base, content)?;
                if given == read.content {
                    return Err(format!(
                        "the replacement leaves {} as it is, so nothing was written",
                        read.name
                    ));
                }
                given
            }
            _ => changed_text(&read, &asked)?,
        };
        let hash = store(&content, &read.name)?;
        install(
            TreeDelta::Updated {
                artifact_id,
                old: read.artifact.located_entry(),
                new: LocatedEntry::new(
                    read.artifact.path.clone(),
                    TreeEntry::blob(hash, read.executable),
                ),
            },
            &read.name,
        )?;
        written.insert(read.artifact.path.clone());
    }
    if !sections.is_empty() {
        let members = members_of(authority, &base.graph, &base.tree)?;
        for (artifact_id, asks) in sections {
            let read = held(authority, &base.tree, &artifact_id)?;
            let content = changed_sections(&read, &members.placed(&read.artifact), &asks)?;
            let hash = store(&content, &read.name)?;
            install(
                TreeDelta::Updated {
                    artifact_id,
                    old: read.artifact.located_entry(),
                    new: LocatedEntry::new(
                        read.artifact.path.clone(),
                        TreeEntry::blob(hash, read.executable),
                    ),
                },
                &read.name,
            )?;
            written.insert(read.artifact.path.clone());
        }
    }
    for content in created {
        let name = content.name.as_deref().unwrap_or_default();
        let path = placed_beside(&base.tree, &content.source_base.artifact_id, name)?;
        if prospective
            .resolved_tree()
            .artifact_at_path(&path)
            .is_some()
        {
            return Err(format!(
                "`{name}` already stands beside it, so replace that entity, or create under \
                 another name"
            ));
        }
        let given = given_content(authority, base, content)?;
        let hash = store(&given, name)?;
        install(
            TreeDelta::Added {
                artifact_id: ArtifactId::new(),
                new: LocatedEntry::new(path.clone(), TreeEntry::blob(hash, false)),
            },
            name,
        )?;
        written.insert(path);
    }
    Ok(written)
}

/// How many unparsed entities `operations` write: each one changed, created
/// or removed, counted once however many of its sections change. None of
/// them is a source unit.
pub(crate) fn unparsed_written(operations: &[McpMutationOperation]) -> usize {
    let mut owners = BTreeSet::new();
    let mut created = 0;
    for asked in operations.iter().filter_map(asked) {
        match asked {
            Asked::Create(_) => created += 1,
            other => {
                owners.insert(other.base().artifact_id);
            }
        }
    }
    owners.len() + created
}

/// Bring the retrieval record of every unparsed entity `change` wrote level
/// with its new content, in the running graph, so a search finds what was
/// just written. The record of one it removed or moved went with its tree
/// transition. The record is derived, never published: a daemon derives the
/// same one when it next opens the store.
pub(crate) fn refresh_retrieval_records(
    state: &DaemonState,
    change: &SemanticChange,
) -> Result<(), String> {
    for delta in &change.tree_deltas {
        let Some(new) = delta.new_state() else {
            continue;
        };
        if artifact_entity::is_code(&new.path) {
            continue;
        }
        let (Some(place), TreeEntry::Blob { hash, .. }) = (new.path.as_utf8(), new.entry) else {
            continue;
        };
        let digest = kin_blobs::Hash256::from_bytes(*hash.as_bytes());
        let Ok(content) = state.blobs.read(&digest) else {
            // Not in the staging store: the next open derives the record.
            continue;
        };
        let file_id = FilePathId::new(place);
        let project = kin_index::GraphProjectFiles::new(state.graph.as_ref(), state.blobs.as_ref());
        let indexed = kin_index::shared_index::index_any(&file_id, &content, digest, &project)
            .map_err(|error| format!("derive the retrieval record of {place}: {error}"))?;
        if matches!(indexed, kin_index::IndexedAny::EntitySource(_)) {
            continue;
        }
        crate::loop_runner::persist_prepared_non_entity_enrichment(state, indexed)
            .map_err(|error| format!("record {place} for retrieval: {error}"))?;
    }
    Ok(())
}

/// One row for each unparsed entity `operations` changed, created or
/// removed, for the commit's receipt: its id, its name, its kind, the module
/// it is placed under, its level, and what the operation did. Read from the
/// committed `change` and the tree the commit left, so a row names the
/// entity the way a read of it does.
pub(crate) fn change_rows(
    state: &DaemonState,
    operations: &[McpMutationOperation],
    change: &SemanticChange,
) -> Vec<serde_json::Value> {
    let asked: Vec<Asked<'_>> = operations.iter().filter_map(asked).collect();
    if asked.is_empty() {
        return Vec::new();
    }
    let tree = state.graph.resolved_tree();
    let content = |hash: Hash256| {
        state
            .blobs
            .read(&kin_blobs::Hash256::from_bytes(*hash.as_bytes()))
            .ok()
    };
    let root = artifact_entity::namespace::root_manifest(&tree).and_then(|(manifest, hash)| {
        artifact_entity::namespace::declared_project_name(
            &manifest,
            std::str::from_utf8(&content(hash)?).ok()?,
        )
    });
    let Ok(members) = artifact_entity::Held::load(state.graph.as_ref(), &tree, root) else {
        return Vec::new();
    };
    let row = |artifact: &ResolvedArtifact, what: &str| {
        let entity = members.placed(artifact);
        let head = match artifact.entry {
            TreeEntry::Blob { hash, .. } => content(hash).unwrap_or_default(),
            _ => Vec::new(),
        };
        let level = Level::of(&head);
        let described = artifact_entity::kind::describe(
            &entity.place(),
            &entity.name,
            level == Level::Text,
            &head[..head.len().min(4096)],
        );
        let mut row = serde_json::json!({
            "entity_id": entity.id().0,
            "name": entity.name,
            "kind": described.kind,
            "change": what,
            artifact_entity::HELD_AS_KEY: level.word(),
        });
        if let Some(module) = &entity.module {
            row["module"] = serde_json::json!(module);
        }
        row
    };
    // A section is named by its own row, read from its text as the commit
    // left it, or, removed, as it stood before. A section the change
    // retitled is named by its text's row.
    let section_row = |asked: &Asked<'_>| -> Option<serde_json::Value> {
        let base = asked.base();
        let (artifact, what) = match asked {
            Asked::Remove(_) => (
                change.tree_deltas.iter().find_map(|delta| match delta {
                    TreeDelta::Updated {
                        artifact_id, old, ..
                    } if *artifact_id == base.artifact_id => Some(ResolvedArtifact::new(
                        *artifact_id,
                        old.path.clone(),
                        old.entry,
                    )),
                    _ => None,
                })?,
                kin_mcp::write_reply::REMOVED,
            ),
            _ => (
                tree.get(&base.artifact_id).cloned()?,
                kin_mcp::write_reply::CHANGED,
            ),
        };
        let TreeEntry::Blob { hash, .. } = artifact.entry else {
            return None;
        };
        let text = content(hash)?;
        let entity = members.placed(&artifact);
        let kind = artifact_entity::kind::describe(
            &entity.place(),
            &entity.name,
            true,
            &text[..text.len().min(4096)],
        )
        .kind;
        let reader = artifact_entity::parts::reader_for(&kind)?;
        let sections = artifact_entity::parts::parts(reader, std::str::from_utf8(&text).ok()?);
        let ids = artifact_entity::parts::part_ids(entity.id(), &sections);
        let index = ids.iter().position(|id| *id == base.entity_id.0)?;
        let level = if sections.iter().any(|part| part.parent == Some(index)) {
            Level::Parts
        } else {
            Level::Text
        };
        Some(serde_json::json!({
            "entity_id": base.entity_id.0,
            "name": sections[index].title,
            "module": artifact_entity::parts::module_of(&entity),
            "kind": artifact_entity::parts::PART_KIND,
            "change": what,
            artifact_entity::HELD_AS_KEY: level.word(),
            artifact_entity::parts::PART_OF_KEY: entity.label(),
        }))
    };
    let mut rows = Vec::new();
    for asked in &asked {
        if asked.of_section() {
            match section_row(asked) {
                Some(found) => rows.push(found),
                None => {
                    if let Some(artifact) = tree.get(&asked.base().artifact_id) {
                        rows.push(row(artifact, kin_mcp::write_reply::CHANGED));
                    }
                }
            }
            continue;
        }
        match asked {
            Asked::Remove(base) => {
                let removed = change.tree_deltas.iter().find_map(|delta| match delta {
                    TreeDelta::Removed { artifact_id, old } if *artifact_id == base.artifact_id => {
                        Some(ResolvedArtifact::new(
                            *artifact_id,
                            old.path.clone(),
                            old.entry,
                        ))
                    }
                    _ => None,
                });
                if let Some(removed) = removed {
                    rows.push(row(&removed, kin_mcp::write_reply::REMOVED));
                }
            }
            Asked::Create(content) => {
                let name = content.name.as_deref().unwrap_or_default();
                let added = change.tree_deltas.iter().find_map(|delta| match delta {
                    TreeDelta::Added { artifact_id, new }
                        if name_of(&new.path) == name && !artifact_entity::is_code(&new.path) =>
                    {
                        tree.get(artifact_id).cloned()
                    }
                    _ => None,
                });
                if let Some(added) = added {
                    rows.push(row(&added, kin_mcp::write_reply::CREATED));
                }
            }
            changed => {
                if let Some(artifact) = tree.get(&changed.base().artifact_id) {
                    rows.push(row(artifact, kin_mcp::write_reply::CHANGED));
                }
            }
        }
    }
    rows
}
