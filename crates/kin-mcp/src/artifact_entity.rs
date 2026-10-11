// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A tracked thing Kin does not parse, served as an entity.
//!
//! Every member of a repository's tree has an identity, exact content and a
//! history in the graph, whether or not a parser reads it. A licence, a
//! changelog, a manifest and an image had all of that and no way to be asked
//! for: a name search certified that no `LICENSE` was held, and a description
//! search dropped the artifacts it had ranked. This module is the surface they
//! were missing. It adds nothing to the graph. It reads the tree and the
//! content the graph already holds and presents each such member as an
//! entity:
//!
//! - it is addressed `module :: name` with an id, like code. The id is the
//!   artifact's own, the name is its own name (`LICENSE`), and the module is
//!   the place code beside it has ([`namespace`]). Where it is projected is
//!   one line of a reply, [`PROJECTED_AT_KEY`], and never an address;
//! - it is found by `locate`, by name or by description. There is no
//!   listing of everything held;
//! - it is read at the finest grain Kin holds. What Kin holds is the
//!   artifact's [`Level`]: its text, when it is text with no parts Kin
//!   knows, or only its bytes. Text is read like a long entity, whole when
//!   small and in excerpts when not. Bytes are never returned: an artifact
//!   held as bytes is described, by its kind, what its header states, its
//!   size, its content id and its history;
//! - its text is changed by the same guarded write an entity's is, against
//!   the `source_base` its read returned: an anchored patch, or the whole
//!   text when it is short enough to have been read whole.
//!
//! Code is not served here. A source Kin parses is read as its entities, one
//! at a time, and asking for it whole by its own name is refused with the
//! calls that read it ([`held_as_code`]).

use std::path::Path;
use std::sync::Arc;

use kin_index::{FileClassification, FileClassifier};
use kin_model::graph::GraphStore;
use kin_model::{
    ArtifactId, ChangeStore, RepoPath, ResolvedArtifact, ResolvedTree, SemanticChangeId, TreeEntry,
};
use serde_json::{json, Map, Value};

use crate::error::{McpError, Result};
use crate::handlers::common::{EntitySourceScope, HeldSourceAuthority};
use crate::source_base::{EntitySourceBase, SourceBaseSchema};

/// What an unparsed entity is: one registry of formats, which the indexer
/// also reads for the media type it stores.
pub use kin_index::kind;
pub mod mentions;
pub mod namespace;
pub mod parts;

/// The key a reply about an artifact carries beside its answer: its level and
/// its size.
pub const REPLY_KEY: &str = "artifact";

/// The key that states an artifact's level on a row and in a reply.
pub const HELD_AS_KEY: &str = "held_as";

/// The one key that says where an artifact is projected. Its value is a
/// place in the projected tree, which no argument of any tool takes.
pub const PROJECTED_AT_KEY: &str = "projected_at";

/// Fewest hex digits a short id carries.
const SHORT_ID_MIN: usize = 8;

/// How much of a tracked thing Kin holds. Code is the level above these,
/// and a thing held as code is not served by this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Its content, as bytes Kin does not read.
    Bytes,
    /// Its content as text, with no parts Kin knows.
    Text,
    /// Its content as text, in sections Kin knows by their headings
    /// ([`parts`]), each read and changed by itself.
    Parts,
}

impl Level {
    /// The level as a row states it: `text`, `bytes`, `parts`.
    pub fn word(self) -> &'static str {
        match self {
            Self::Bytes => "bytes",
            Self::Text => "text",
            Self::Parts => "parts",
        }
    }

    /// The level of `content`: text when it is UTF-8 with no NUL, the test
    /// the indexer classifies content by.
    pub fn of(content: &[u8]) -> Self {
        if content.contains(&0) || std::str::from_utf8(content).is_err() {
            Self::Bytes
        } else {
            Self::Text
        }
    }

    /// The clause that states this level and what Kin knows at it. Every
    /// reply about an artifact says it once.
    pub fn held(self) -> &'static str {
        match self {
            Self::Parts => {
                "held with parts (Kin knows its sections by their headings, its history and what \
                 refers to it, and reads and changes each section by itself)"
            }
            Self::Text => {
                "held as text (Kin knows its name, its history and what refers to it, not its \
                 structure)"
            }
            Self::Bytes => {
                "held as bytes (Kin knows its name, type, size, history and what refers to it, \
                 and never returns its bytes in a reply)"
            }
        }
    }
}

/// Whether the tree member projected at `path` is source Kin parses into
/// entities. Decided by the classifier the indexer uses, on the name alone.
pub fn is_code(path: &RepoPath) -> bool {
    path.as_utf8().is_some_and(|place| {
        matches!(
            FileClassifier::classify(Path::new(place)),
            FileClassification::EntitySource
        )
    })
}

/// One tree member as an entity: its artifact, its own name and its module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactEntity {
    pub artifact: ResolvedArtifact,
    pub name: String,
    pub module: Option<String>,
}

impl ArtifactEntity {
    pub fn id(&self) -> ArtifactId {
        self.artifact.artifact_id
    }

    /// Where it is projected, as text.
    pub fn place(&self) -> String {
        String::from_utf8_lossy(self.artifact.path.as_bytes()).into_owned()
    }

    /// Its address as a row writes it: `module :: name`, or its name alone.
    pub fn label(&self) -> String {
        match &self.module {
            Some(module) => format!("{module} :: {}", self.name),
            None => self.name.clone(),
        }
    }

    /// The first `digits` hex digits of its id.
    pub fn short_id(&self, digits: usize) -> String {
        self.id()
            .0
            .to_string()
            .chars()
            .filter(|c| *c != '-')
            .take(digits)
            .collect()
    }
}

/// What an address names among the members of a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Named {
    /// One artifact Kin does not parse.
    One(ArtifactEntity),
    /// One source Kin parses, which is read as its entities.
    Code(ArtifactEntity),
    /// Several artifacts, none more exactly than another.
    Several(Vec<ArtifactEntity>),
    Nothing,
}

/// The members of one tree, each placed.
pub struct Held<'t> {
    tree: &'t ResolvedTree,
    namespaces: namespace::Namespaces,
    /// Whether the project name was read. A reader that could not read it
    /// takes any one-segment module of a label for the root's.
    root_read: bool,
}

impl<'t> Held<'t> {
    /// Read the placement of every member of `tree` from `store`. `root` is
    /// the project name the root manifest declares, when it was read.
    pub fn load<G: GraphStore + ?Sized>(
        store: &G,
        tree: &'t ResolvedTree,
        root: Option<String>,
    ) -> std::result::Result<Self, String> {
        Ok(Self {
            tree,
            root_read: root.is_some(),
            namespaces: namespace::Namespaces::load(store, tree, root)?,
        })
    }

    /// `artifact` as an entity, placed the way every member of this tree is.
    pub fn placed(&self, artifact: &ResolvedArtifact) -> ArtifactEntity {
        self.entity(artifact)
    }

    fn entity(&self, artifact: &ResolvedArtifact) -> ArtifactEntity {
        let place = String::from_utf8_lossy(artifact.path.as_bytes()).into_owned();
        ArtifactEntity {
            artifact: artifact.clone(),
            name: place.rsplit('/').next().unwrap_or(&place).to_string(),
            module: self.namespaces.of(&place),
        }
    }

    /// Every member Kin does not parse, in the order of where it is
    /// projected.
    pub fn artifacts(&self) -> impl Iterator<Item = ArtifactEntity> + '_ {
        self.tree
            .artifacts_by_path()
            .filter(|artifact| !is_code(&artifact.path))
            .map(|artifact| self.entity(artifact))
    }

    /// The member with the id `id`, parsed or not.
    fn member(&self, id: &ArtifactId) -> Named {
        match self.tree.get(id) {
            Some(artifact) if is_code(&artifact.path) => Named::Code(self.entity(artifact)),
            Some(artifact) => Named::One(self.entity(artifact)),
            None => Named::Nothing,
        }
    }

    /// What `written` names: an id, the leading digits of one, alone or
    /// after `#`, a `module :: name` label, or a name. A name is the member's
    /// own name exactly; nothing here reads a path.
    pub fn named(&self, written: &str) -> Named {
        let written = written.trim();
        if written.is_empty() || written.contains('/') || written.contains('\\') {
            return Named::Nothing;
        }
        if let Ok(id) = uuid::Uuid::parse_str(written) {
            return self.member(&ArtifactId(id));
        }
        // `name #9c1e77a0`, `#9c1e77a0`, or the digits alone. Digits that
        // start no id are read as the name they also are.
        let (label, prefix) = match written.rsplit_once('#') {
            Some((label, digits)) if short_id(digits).is_some() => (label.trim(), short_id(digits)),
            _ => (written, short_id(written)),
        };
        if let Some(prefix) = prefix {
            let mut with_prefix = self
                .tree
                .artifacts_by_path()
                .filter(|artifact| id_starts_with(&artifact.artifact_id, &prefix));
            if let (Some(one), None) = (with_prefix.next(), with_prefix.next()) {
                return self.member(&one.artifact_id);
            }
        }
        let (module, name) = split_label(label);
        if name.is_empty() {
            return Named::Nothing;
        }
        let mut code = Vec::new();
        let mut artifacts = Vec::new();
        for artifact in self.tree.artifacts_by_path() {
            let place = String::from_utf8_lossy(artifact.path.as_bytes());
            if place.rsplit('/').next() != Some(name) {
                continue;
            }
            let entity = self.entity(artifact);
            let unread_root = |module: &str| {
                !self.root_read
                    && entity.module.is_none()
                    && !artifact.path.as_bytes().contains(&b'/')
                    && !module.contains('.')
            };
            if module.is_some_and(|module| {
                entity.module.as_deref() != Some(module) && !unread_root(module)
            }) {
                continue;
            }
            if is_code(&artifact.path) {
                code.push(entity);
            } else {
                artifacts.push(entity);
            }
        }
        match (artifacts.len(), code.len()) {
            (1, _) => Named::One(artifacts.remove(0)),
            (0, 1) => Named::Code(code.remove(0)),
            (0, _) => Named::Nothing,
            _ => Named::Several(artifacts),
        }
    }

    /// The artifacts whose own name holds `query`, the closest first: one
    /// named exactly so, then one named so before its extension, then one
    /// whose name holds it. Case is not read. A `module :: name` query is
    /// held to its module.
    pub fn named_like(&self, query: &str) -> Vec<ArtifactEntity> {
        let (module, wanted) = split_label(query.trim());
        let wanted = wanted.to_lowercase();
        if wanted.is_empty() || wanted.contains('/') || wanted.contains('\\') {
            return Vec::new();
        }
        let mut found: Vec<(u8, ArtifactEntity)> = self
            .artifacts()
            .filter(|entity| module.is_none_or(|module| entity.module.as_deref() == Some(module)))
            .filter_map(|entity| {
                let name = entity.name.to_lowercase();
                let stem = name
                    .rsplit_once('.')
                    .map_or(name.as_str(), |(stem, _)| stem);
                let tier = if name == wanted {
                    0
                } else if stem == wanted && !stem.is_empty() {
                    1
                } else if name.contains(&wanted) {
                    2
                } else {
                    return None;
                };
                Some((tier, entity))
            })
            .collect();
        found.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.artifact.path.cmp(&right.1.artifact.path))
        });
        found.into_iter().map(|(_, entity)| entity).collect()
    }

    /// The artifact projected at `place`, when Kin does not parse it. For a
    /// ranking that names its candidates by where they are projected; never
    /// for an argument a caller wrote.
    pub fn projected_at(&self, place: &str) -> Option<ArtifactEntity> {
        let path = RepoPath::from_utf8(place.to_string()).ok()?;
        let artifact = self.tree.artifact_at_path(&path)?;
        (!is_code(&artifact.path)).then(|| self.entity(artifact))
    }

    /// Each of `several` by an address that reads back as it alone: its
    /// label, with its short id where two share one.
    pub fn addresses(&self, several: &[ArtifactEntity]) -> Vec<String> {
        several
            .iter()
            .map(|entity| {
                let label = entity.label();
                let shared = several
                    .iter()
                    .filter(|other| other.label() == label)
                    .count()
                    > 1;
                if shared || entity.module.is_none() {
                    format!("`{label} #{}`", entity.short_id(SHORT_ID_MIN))
                } else {
                    format!("`{label}`")
                }
            })
            .collect()
    }
}

/// A label as its module and name: `shop.static :: logo.png`, or the same
/// without its spaces, the spelling a reader of names is handed. A name
/// alone has no module.
fn split_label(label: &str) -> (Option<&str>, &str) {
    match label.rsplit_once("::") {
        Some((module, name)) if !module.trim().is_empty() => (Some(module.trim()), name.trim()),
        _ => (None, label.trim()),
    }
}

/// Whether `written` is a name alone: no id, no short id after `#`, and no
/// module before it.
fn is_bare_name(written: &str) -> bool {
    let written = written.trim();
    uuid::Uuid::parse_str(written).is_err()
        && !written.contains("::")
        && written
            .rsplit_once('#')
            .is_none_or(|(_, digits)| short_id(digits).is_none())
}

/// `text` as the leading hex digits of an id, when it is shaped like them.
fn short_id(text: &str) -> Option<String> {
    let text = text.trim();
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    (hex.len() >= SHORT_ID_MIN
        && hex.len() <= 32
        && hex.chars().all(|c| c.is_ascii_hexdigit())
        && !text.starts_with('-')
        && !text.ends_with('-'))
    .then(|| hex.to_ascii_lowercase())
}

fn id_starts_with(id: &ArtifactId, prefix: &str) -> bool {
    crate::entity_address::id_starts_with(&id.0.to_string(), prefix)
}

/// The id of the unparsed entity `written` names in the tree `store` holds,
/// for an argument that takes an id: an id, a short id or a label. The
/// project name is not read here, so a label's one-segment module is taken
/// for the root's. `None` when it names no one unparsed entity.
pub fn address_in_tree<G: GraphStore + ?Sized>(store: &G, written: &str) -> Option<String> {
    let tree = store.resolved_tree_snapshot().ok()??;
    match Held::load(store, &tree, None).ok()?.named(written) {
        Named::One(entity) => Some(entity.id().0.to_string()),
        _ => None,
    }
}

/// The project name the root manifest of `tree` declares, read through
/// `held`. `None` when the tree holds no such manifest, it declares no name,
/// or its content cannot be read.
pub fn root_name<G: GraphStore>(
    held: &HeldSourceAuthority<'_, G>,
    tree: &ResolvedTree,
) -> Option<String> {
    let (manifest, hash) = namespace::root_manifest(tree)?;
    let content = held.tree_content(hash).ok()?;
    namespace::declared_project_name(&manifest, std::str::from_utf8(&content).ok()?)
}

/// The refusal for a whole read or write of source Kin parses, addressed as
/// an artifact by its own name or id.
pub fn held_as_code<G: GraphStore + ?Sized>(store: &G, code: &ArtifactEntity) -> String {
    let module = code
        .artifact
        .path
        .as_utf8()
        .and_then(|place| crate::semantic_owners::file_module(store, place));
    match module {
        Some(module) => format!(
            "{} is held as code: Kin serves its functions and classes one at a time and never \
             the whole source. Its module is {module}; call source on {module} for what it \
             holds, or find a function or class with locate and read it with source.",
            code.name
        ),
        None => format!(
            "{} is held as code: Kin serves its functions and classes one at a time and never \
             the whole source. Find a function or class in it with locate and read it with \
             source.",
            code.name
        ),
    }
}

/// The refusal for an address several artifacts answer to.
fn several_named(held: &Held<'_>, written: &str, several: &[ArtifactEntity]) -> String {
    format!(
        "`{written}` names {} unparsed entities, and this call takes one. Pass one of: {}.",
        several.len(),
        held.addresses(several).join(", ")
    )
}

/// One artifact with its content, read at one instant of the workspace.
pub struct Read {
    pub entity: ArtifactEntity,
    pub level: Level,
    pub described: kind::Described,
    /// The content, for a regular member. A link has none to serve.
    pub content: Option<Arc<Vec<u8>>>,
    /// Its sections, for a text held with parts.
    pub parts: Option<Arc<Vec<parts::Part>>>,
}

impl Read {
    fn of<G: GraphStore>(
        held: &HeldSourceAuthority<'_, G>,
        entity: ArtifactEntity,
    ) -> Result<Self> {
        let mut sections = None;
        let (content, level, described) = match entity.artifact.entry {
            TreeEntry::Blob { hash, .. } => {
                let content = held.tree_content(hash)?;
                let mut level = Level::of(&content);
                let described = kind::describe(
                    &entity.place(),
                    &entity.name,
                    level == Level::Text,
                    &content[..content.len().min(4096)],
                );
                if level == Level::Text {
                    let kept = Arc::clone(&content);
                    sections = parts::parts_of(&described.kind, hash, move || Some(kept))
                        .filter(|found| !found.is_empty());
                    if sections.is_some() {
                        level = Level::Parts;
                    }
                }
                (Some(content), level, described)
            }
            TreeEntry::Symlink { .. } => (
                None,
                Level::Bytes,
                kind::Described {
                    kind: "symbolic link".into(),
                    detail: None,
                    media_type: None,
                },
            ),
            TreeEntry::Gitlink { .. } => (
                None,
                Level::Bytes,
                kind::Described {
                    kind: "Git submodule pointer".into(),
                    detail: None,
                    media_type: None,
                },
            ),
        };
        Ok(Self {
            entity,
            level,
            described,
            content,
            parts: sections,
        })
    }

    /// The fields every row and reply names this artifact by.
    fn identity(&self) -> Map<String, Value> {
        let mut row = Map::new();
        row.insert("id".into(), json!(self.entity.id().0));
        row.insert("name".into(), json!(self.entity.name));
        if let Some(module) = &self.entity.module {
            row.insert("module".into(), json!(module));
        }
        row.insert("kind".into(), json!(self.described.kind));
        row.insert(HELD_AS_KEY.into(), json!(self.level.word()));
        row
    }
}

/// The base a guarded write to an artifact carries back: the workspace
/// instant, the artifact, and the length and digest of its content. An
/// artifact's base names its own id as its entity, which is what tells it
/// from the base of an entity inside a source ([`is_artifact_base`]).
fn source_base(
    context: crate::source_base::SourceBaseContext,
    entity: &ArtifactEntity,
    hash: kin_model::Hash256,
    content: &[u8],
) -> std::result::Result<EntitySourceBase, String> {
    let base = EntitySourceBase {
        schema: SourceBaseSchema::V1,
        context,
        entity_id: kin_model::EntityId(entity.id().0),
        artifact_id: entity.id(),
        source_blob_hash: hash.to_string(),
        start_byte: 0,
        end_byte: content.len(),
        body_hash: kin_blobs::digest(content).to_string(),
    };
    base.validate()?;
    Ok(base)
}

/// Whether `base` is an artifact's: it names the artifact as its entity.
pub fn is_artifact_base(base: &EntitySourceBase) -> bool {
    base.entity_id.0 == base.artifact_id.0
}

/// Whether `base` is an unparsed entity's: a whole artifact's, or one
/// section's of a text held with parts ([`parts::is_part_id`]).
pub fn is_unparsed_base(base: &EntitySourceBase) -> bool {
    is_artifact_base(base) || parts::is_part_id(&base.entity_id.0)
}

/// The verbs [`EntityContent`] goes with.
pub const CONTENT_VERBS: [&str; 2] = ["replace", "create"];

/// Content Kin already holds, given to an entity it holds without parsing.
///
/// `replace` gives the entity its `source_base` names the content of
/// `content_of`: another unparsed entity's, or its own as it stood at the
/// change `at` names. `create` places a new unparsed entity named `name`
/// beside the entity its `source_base` names, with that content, or with
/// `text` when it is new text. New bytes never travel in a call: an image
/// Kin does not hold yet reaches it through the projected tree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityContent {
    pub source_base: EntitySourceBase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_of: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl EntityContent {
    /// The refusal for a shape that pairs with no verb, or `None`.
    pub fn validate(&self, verb: &str) -> std::result::Result<(), String> {
        self.source_base.validate()?;
        if self.at.is_some() && self.content_of.is_none() {
            return Err("EntityContent's at names a change of content_of, so it needs one".into());
        }
        match verb {
            "replace" => {
                if self.name.is_some() || self.text.is_some() {
                    return Err(
                        "EntityContent with verb replace takes content_of and at, and \
                                no name or text: change text with a patch or an update"
                            .into(),
                    );
                }
                if self.content_of.is_none() {
                    return Err("EntityContent with verb replace needs content_of: the \
                                unparsed entity whose content it takes"
                        .into());
                }
                Ok(())
            }
            "create" => {
                let name = self.name.as_deref().ok_or(
                    "EntityContent with verb create needs name: the new entity's own name",
                )?;
                valid_name(name)?;
                match (&self.content_of, &self.text) {
                    (Some(_), None) | (None, Some(_)) => Ok(()),
                    _ => Err(
                        "EntityContent with verb create takes one of content_of, for \
                              content Kin holds, and text, for new text"
                            .into(),
                    ),
                }
            }
            _ => Err(format!(
                "EntityContent goes with verb replace or create, and this operation says {verb}"
            )),
        }
    }
}

/// Whether `name` can name a new unparsed entity: one name, never a place.
pub fn valid_name(name: &str) -> std::result::Result<(), String> {
    let bad = name.is_empty()
        || name != name.trim()
        || matches!(name, "." | "..")
        || name
            .chars()
            .any(|c| matches!(c, '/' | '\\') || c.is_control());
    if bad {
        // The name is not echoed: one that holds a separator reads as a place.
        return Err(
            "the name given is not one name: an entity's name holds no separator, so \
                    create it beside an entity Kin holds in the place you mean"
                .into(),
        );
    }
    Ok(())
}

/// The `source` reply for the artifact `query` names, or `None` when it
/// names none, which leaves the answer to the read of entities. A history
/// read is answered there as well.
///
/// `Err` is a refusal in the words an agent reads: source Kin parses, asked
/// for whole, and a name several artifacts share.
pub fn source_reply_for_query<G: GraphStore>(
    held: &HeldSourceAuthority<'_, G>,
    query: &str,
    scope: EntitySourceScope,
) -> Result<Option<Value>> {
    if scope != EntitySourceScope::WorkspaceHead {
        return Ok(None);
    }
    // A name several declarations carry is theirs to answer, each listed by
    // its address; an artifact of that name is read by its label or its id.
    if is_bare_name(query) {
        use kin_ranking::entity_ranking::{CandidateReason, StrictNameResolution};
        if let Ok(StrictNameResolution::Candidates(
            CandidateReason::SameName | CandidateReason::SharedMemberName,
            _,
        )) = kin_ranking::entity_ranking::resolve_name_strictly(held.store(), query.trim())
        {
            return Ok(None);
        }
    }
    // With no workspace to read, nothing here can be named, and the read of
    // entities answers as it did.
    let Ok(sample) = held.workspace_sample() else {
        return Ok(None);
    };
    let Ok(members) = Held::load(held.store(), &sample.tree, root_name(held, &sample.tree)) else {
        return Ok(None);
    };
    let entity = match members.named(query) {
        // A section of a text held with parts answers to its own address.
        Named::Nothing => {
            return match parts::named(held, &members, query) {
                parts::NamedPart::Nothing => Ok(None),
                parts::NamedPart::One(part) => parts::source_reply(held, sample, &part).map(Some),
                parts::NamedPart::Several(several) => Err(McpError::Other(parts::several_named(
                    query.trim(),
                    &several,
                ))),
            };
        }
        Named::Code(code) => return Err(McpError::Other(held_as_code(held.store(), &code))),
        Named::Several(several) => {
            return Err(McpError::Other(several_named(
                &members,
                query.trim(),
                &several,
            )))
        }
        Named::One(entity) => entity,
    };
    let read = Read::of(held, entity)?;
    let mut reply = read.identity();
    reply.insert(PROJECTED_AT_KEY.into(), json!(read.entity.place()));
    reply.insert("source".into(), json!("graph"));
    let size = read.content.as_ref().map_or(0, |content| content.len());
    let mut about = json!({"level": read.level.word(), "bytes": size});
    if let (Some(content), TreeEntry::Blob { hash, .. }) =
        (read.content.as_ref(), read.entity.artifact.entry)
    {
        let base = match sample.source_base_context.clone() {
            Some(context) => {
                Some(source_base(context, &read.entity, hash, content).map_err(McpError::Context)?)
            }
            // Hosted source has no local workspace to change.
            None => None,
        };
        reply.insert("source_base".into(), json!(base));
    }
    match (read.level, read.content.as_ref()) {
        (Level::Text | Level::Parts, Some(content)) => {
            let text = std::str::from_utf8(content).map_err(|error| {
                McpError::Other(format!("{} is not text: {error}", read.entity.name))
            })?;
            reply.insert("body".into(), json!(text));
            // Its sections, for the presentation that serves a long text
            // held with parts as its outline.
            if let Some(sections) = &read.parts {
                reply.insert(
                    parts::PARTS_KEY.into(),
                    parts::rows(read.entity.id(), sections, None),
                );
            }
        }
        // Bytes are described, never returned.
        _ => {
            if let Some(detail) = &read.described.detail {
                about["detail"] = json!(detail);
            }
            if let TreeEntry::Blob { hash, .. } = read.entity.artifact.entry {
                reply.insert(
                    "content".into(),
                    json!({"sha256": short_digest(&hash.to_string())}),
                );
            }
            reply.insert(
                "history".into(),
                history(held.store(), &read.entity.id(), &sample.source_change_id).value(),
            );
            // What names it, the first few; `refs` lists every one.
            let (rows, total, _) = mentions::named_in(held, &members, &sample.tree, &read.entity);
            if total > 0 {
                let shown: Vec<Value> = rows
                    .into_iter()
                    .take(mentions::DESCRIPTION_ROWS_MAX)
                    .collect();
                reply.insert(mentions::NAMED_IN_KEY.into(), json!(shown));
                reply.insert(mentions::MATCHED_BY_KEY.into(), json!(mentions::BY_NAME));
                reply.insert("named_in_total".into(), json!(total));
            }
        }
    }
    reply.insert(REPLY_KEY.into(), about);
    stamp(&mut reply, sample);
    Ok(Some(Value::Object(reply)))
}

/// The instant of the workspace a reply was read at: the committed change,
/// or the tree and generation of uncommitted work.
fn stamp(
    reply: &mut Map<String, Value>,
    sample: &crate::handlers::repository_authority::AuthorityHeadReadSample,
) {
    if sample.committed {
        reply.insert(
            "source_change_id".into(),
            json!(sample.source_change_id.to_string()),
        );
    } else {
        reply.insert(
            "workspace_tree_hash".into(),
            json!(sample.tree_hash.to_string()),
        );
        reply.insert("workspace_generation".into(), json!(sample.generation));
    }
}

/// Apply a patch's edits to `text`, the whole text of the artifact `name`,
/// and return the text with exactly that change, or the one sentence that
/// says why not and what to send instead.
///
/// Every anchor is literal and addresses the text as it was read, never an
/// earlier replacement, the way an entity patch reads its anchors. Each must
/// stand exactly once in the whole text, so a patch written against an
/// excerpt changes the place its reader saw or is refused: an anchor that
/// also stands in a part the reader did not see is one that stands twice. An
/// artifact that holds no text yet takes its first text from one edit with
/// an empty anchor.
pub fn apply_edits(
    name: &str,
    text: &str,
    edits: &[crate::source_base::EntityTextEdit],
) -> std::result::Result<String, String> {
    if edits.is_empty() {
        return Err("a patch needs at least one exact text edit".into());
    }
    let unchanged =
        || format!("the patch leaves the text of {name} as it is, so nothing was written");
    if let [edit] = edits {
        if edit.old_text.is_empty() {
            if !text.is_empty() {
                return Err(format!(
                    "patch edit #0: old_text is empty, which only gives an entity with no \
                     text its first text, and {name} holds text, so anchor old_text on text \
                     copied from what source returned for {name}"
                ));
            }
            if edit.new_text.is_empty() {
                return Err(unchanged());
            }
            return Ok(edit.new_text.clone());
        }
    }
    let mut replacements = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(format!("patch edit #{index}: old_text must be nonempty"));
        }
        let Some(start) = text.find(&edit.old_text) else {
            return Err(format!(
                "patch edit #{index}: old_text is not in the text of {name}, so copy it exactly \
                 from what source returned for {name}, or read {name} again with source"
            ));
        };
        // Counted from one character after each match, so overlapping
        // occurrences ("aa" in "aaa") are ambiguous too.
        let step = edit.old_text.chars().next().map_or(1, char::len_utf8);
        let mut count = 1;
        let mut from = start + step;
        while let Some(at) = text[from..].find(&edit.old_text) {
            count += 1;
            from += at + step;
        }
        if count > 1 {
            return Err(format!(
                "patch edit #{index}: old_text occurs {count} times in the text of {name}, so \
                 extend it with the text beside the one you mean until it occurs once"
            ));
        }
        replacements.push((start, start + edit.old_text.len(), index, &edit.new_text));
    }
    replacements.sort_by_key(|(start, _, _, _)| *start);
    for pair in replacements.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(format!(
                "patch edits #{} and #{} overlap in the text of {name}, so send them as one edit",
                pair[0].2, pair[1].2
            ));
        }
    }
    let mut patched = text.to_owned();
    for (start, end, _, replacement) in replacements.into_iter().rev() {
        patched.replace_range(start..end, replacement);
    }
    if patched == text {
        return Err(unchanged());
    }
    Ok(patched)
}

/// The leading digits a reply shows of a content or change id.
fn short_digest(hex: &str) -> String {
    hex.chars().take(12).collect()
}

/// The most changes one description reads to count an artifact's history.
const HISTORY_WALK_MAX: usize = 20_000;

/// The changes on the first-parent line of a head that changed one artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    /// How many changes on the line changed the artifact.
    pub changes: usize,
    /// The newest of them and the oldest.
    pub latest: Option<SemanticChangeId>,
    pub first: Option<SemanticChangeId>,
    /// Whether the whole line was read. A line longer than the walk's bound,
    /// or one Kin could not read to its start, counts at least `changes`.
    pub complete: bool,
}

impl History {
    /// As a reply states it.
    fn value(&self) -> Value {
        let mut value = json!({"changes": self.changes});
        if let Some(latest) = self.latest {
            value["latest"] = json!(short_digest(&latest.to_string()));
        }
        if let Some(first) = self.first.filter(|first| Some(*first) != self.latest) {
            value["first"] = json!(short_digest(&first.to_string()));
        }
        if !self.complete {
            value["at_least"] = json!(true);
        }
        value
    }
}

/// The history of `artifact` on the first-parent line that reaches `head`:
/// every change whose tree transition names it, read from the graph's own
/// change records. Changes Kin did not admit, before the history it holds,
/// are not counted.
pub fn history<G: ChangeStore + ?Sized>(
    store: &G,
    artifact: &ArtifactId,
    head: &SemanticChangeId,
) -> History {
    let mut found = History {
        changes: 0,
        latest: None,
        first: None,
        complete: true,
    };
    let mut seen = std::collections::HashSet::new();
    let mut current = Some(*head);
    while let Some(change_id) = current {
        if seen.len() >= HISTORY_WALK_MAX || !seen.insert(change_id) {
            found.complete = false;
            break;
        }
        let Ok(Some(change)) = store.get_change(&change_id) else {
            found.complete = false;
            break;
        };
        if change
            .tree_deltas
            .iter()
            .any(|delta| delta.artifact_id() == *artifact)
        {
            found.changes += 1;
            found.latest.get_or_insert(change.id);
            found.first = Some(change.id);
        }
        current = change.parents.first().copied();
    }
    found
}

/// Whether a machine `source` reply is an artifact's.
pub fn is_artifact_reply(reply: &Map<String, Value>) -> bool {
    reply.get(REPLY_KEY).is_some_and(Value::is_object)
}

/// The level a reply or a row states, when it states one.
fn level_of(row: &Map<String, Value>) -> Option<Level> {
    match row.get(HELD_AS_KEY).and_then(Value::as_str)? {
        "text" => Some(Level::Text),
        "bytes" => Some(Level::Bytes),
        "parts" => Some(Level::Parts),
        _ => None,
    }
}

/// Whether a machine `source` reply describes an artifact held as bytes.
pub fn is_bytes_reply(reply: &Map<String, Value>) -> bool {
    is_artifact_reply(reply) && level_of(reply) == Some(Level::Bytes)
}

/// The article a kind takes: `an` before a vowel sound, which an acronym
/// such as `MP3` or `SVG` begins with when its first letter is said with one.
fn article(kind: &str) -> &'static str {
    let word = kind.split_whitespace().next().unwrap_or(kind);
    let first = word.chars().next().unwrap_or('x');
    let acronym = word.len() > 1
        && word
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    let vowel = if acronym {
        "AEFHILMNORSX".contains(first)
    } else {
        "AEIOUaeiou".contains(first)
    };
    if vowel {
        "an"
    } else {
        "a"
    }
}

/// `count` with a comma between each group of three digits.
fn grouped(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// What an artifact's `source` reply is, in the words a plain reply leads
/// with: the text of the artifact, or for one held as bytes what it is, and
/// the level Kin holds it at.
pub fn plain_lead(name: &str, reply: &Map<String, Value>) -> String {
    if let Some(owner) = reply.get(parts::PART_OF_KEY).and_then(Value::as_str) {
        return format!(
            "this is the text of the section {name} of {owner}, {}",
            parts::HELD
        );
    }
    let level = level_of(reply).unwrap_or(Level::Text);
    if level != Level::Bytes {
        return format!("this is the text of {name}, {}", level.held());
    }
    let about = reply.get(REPLY_KEY);
    let kind = reply
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("binary");
    let size = about
        .and_then(|about| about.get("bytes"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let unit = if size == 1 { "byte" } else { "bytes" };
    let what = match about
        .and_then(|about| about.get("detail"))
        .and_then(Value::as_str)
    {
        Some(detail) => format!(
            "{} {kind} of {detail} and {} {unit}",
            article(kind),
            grouped(size)
        ),
        None => format!("{} {kind} of {} {unit}", article(kind), grouped(size)),
    };
    format!(
        "this is the description of {name}, {what}, {}",
        level.held()
    )
}

/// The refusal for the bytes of an artifact asked for in a reply.
pub fn inline_bytes_refusal(name: &str) -> String {
    format!(
        "{name} is held as bytes, and Kin does not return bytes in a reply. Call source \
         without full_body for its description: its type, size, history and what refers to it."
    )
}

/// The row a search writes for `read`: its id, name, module, kind and level,
/// and nothing that says where it is projected.
fn search_row(read: &Read) -> Value {
    let mut row = read.identity();
    if let Some(id) = row.remove("id") {
        row.insert("entity_id".into(), id);
    }
    Value::Object(row)
}

/// The key a name search counts the artifacts among its results under.
pub const NAME_MATCHES_KEY: &str = "artifact_matches";

/// Add to a name search's `payload` the artifacts whose name `query` holds,
/// after the declarations it found, within `limit` rows in all. The count of
/// every artifact matched goes under [`NAME_MATCHES_KEY`] and into the
/// reply's total, so a reply never says a held artifact is absent.
///
/// A store that cannot be read for artifacts leaves the search as it was:
/// declarations are still answered.
pub fn join_name_search<G: GraphStore>(
    payload: &mut Value,
    held: &HeldSourceAuthority<'_, G>,
    query: &str,
    limit: usize,
) {
    let Ok(sample) = held.workspace_sample() else {
        return;
    };
    let Ok(members) = Held::load(held.store(), &sample.tree, root_name(held, &sample.tree)) else {
        return;
    };
    let matched = members.named_like(query);
    // The sections of texts held with parts, found by their titles.
    let sections = parts::named_like(held, &members, query);
    if matched.is_empty() && sections.is_empty() {
        return;
    }
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    let shown = object
        .get("results")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let room = limit.saturating_sub(shown);
    // Only the rows the reply shows are read for their kind and level.
    let mut rows: Vec<Value> = matched
        .iter()
        .take(room)
        .filter_map(|entity| Read::of(held, entity.clone()).ok())
        .map(|read| search_row(&read))
        .collect();
    let room = room.saturating_sub(rows.len());
    rows.extend(sections.iter().take(room).map(parts::search_row));
    let found = matched.len() + sections.len();
    let total = object
        .get("total_matches")
        .and_then(Value::as_u64)
        .unwrap_or(shown as u64)
        + found as u64;
    object.insert("total_matches".into(), json!(total));
    object.insert(NAME_MATCHES_KEY.into(), json!(found));
    if rows.len() < found {
        object.insert("truncated".into(), json!(true));
    }
    match object.get_mut("results").and_then(Value::as_array_mut) {
        Some(results) => results.extend(rows),
        None => {
            object.insert("results".into(), json!(rows));
        }
    }
}

/// A ranked candidate as the artifact it is: the fields its row is written
/// from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranked {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub module: Option<String>,
    pub level: Level,
}

/// The artifacts among `places`, each keyed by the place a ranking named it
/// by. A ranking names its candidates by where they are projected; the row
/// served for one names the artifact and never that place. A place Kin
/// parses or does not hold is left out, and so is every place when the
/// workspace cannot be read.
pub fn ranked<G: GraphStore>(
    held: &HeldSourceAuthority<'_, G>,
    places: &[String],
) -> std::collections::HashMap<String, Ranked> {
    let mut found = std::collections::HashMap::new();
    if places.is_empty() {
        return found;
    }
    let Ok(sample) = held.workspace_sample() else {
        return found;
    };
    let Ok(members) = Held::load(held.store(), &sample.tree, root_name(held, &sample.tree)) else {
        return found;
    };
    for place in places {
        let Some(entity) = members.projected_at(place) else {
            continue;
        };
        let Ok(read) = Read::of(held, entity) else {
            continue;
        };
        found.insert(
            place.clone(),
            Ranked {
                id: read.entity.id().0.to_string(),
                name: read.entity.name.clone(),
                kind: read.described.kind.clone(),
                module: read.entity.module.clone(),
                level: read.level,
            },
        );
    }
    found
}

#[cfg(test)]
mod tests;
