// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Whether a focal escapes as a value: the census behind
//! [`kin_model::FocalEscape`].
//!
//! A call that never spells a focal's name can still reach it: the focal is
//! held as a value (`handler = target`, then `handler()`), fetched by a name
//! computed at run time, or invoked by the language itself. Whether any
//! unsettled call site that does not spell the name could be such a call turns
//! on which of those can happen to this focal.
//!
//! So [`focal_escape_evidence_batch`] decides it per focal from the records
//! each file's adapter wrote at parse time ([`kin_model::FileEscapeRecord`]):
//! every non-call use of a name, every reflective site and every
//! protocol-dispatch site the file holds. It reads those records, the
//! entities that carry them and the import edges that land on the focal,
//! through the graph it is handed, and never a file's bytes, so no file or
//! byte allowance bounds it. A reader of a selected or replayed generation
//! gets that generation's answer.
//!
//! The answer lists what it found ([`kin_model::EscapeResidue`]): the uses
//! that hold the focal as a value, the names it is aliased under, the
//! reflective sites that could fetch it, the syntax that invokes it
//! implicitly, and any region no record covers. What the records cannot speak
//! for at all reads as unknown, which every consumer treats as escaping.
//!
//! Beside that, the census matches every non-call read that could read the
//! focal (a mention, a value use, an alias or import site) against the
//! `Calls`, `Imports` and `References` rows the graph stores into the focal
//! ([`kin_model::MentionResidue`]): a read a stored row covers is already in a
//! reference answer that asks for the row's kind, and one no row covers is
//! listed by file and line. That reading is measured only over records that
//! list their mentions ([`kin_model::FileEscapeRecord::lists_mentions`]); over
//! older records it says so, and everything else answers as it always did.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use kin_model::graph::GraphStore;
use kin_model::{
    calling_languages, language_of_path, readable_record_version, record_value_lists_mentions,
    Entity, EntityFilter, EntityId, EntityKind, EntityRole, EscapeResidue, FilePathId, FocalEscape,
    Hash256, LanguageId, LookupReceiver, MentionForm, MentionResidue, OccurrenceBinding,
    OccurrenceContext, OutsideReceiver, ProtocolForm, ProtocolMember, ProtocolResidue,
    ReceiverKind, ReflectiveForm, ReflectiveReach, RelationEvidence, RelationKind, ResidueSite,
    SourceSpan, TreeEntry, Visibility, NON_ENUMERABLE_MEMBER_KEY, VALUE_ESCAPE_RECORD_KEY,
};
use serde_json::Value;

mod outside_imports;

pub use outside_imports::{
    receiver_import, ImportRoots, ImportedName, ReceiverImport, ReceiverImports,
};

/// Why the census found a focal escaping.
pub const ESCAPE_NON_CALL_OCCURRENCE: &str =
    "a use of its name holds it as a value: see residue.values";

/// The most alias names the census follows for one focal before it stops
/// widening and answers unknown.
pub const ESCAPE_ALIAS_NAMES_MAX: usize = 64;

/// One file of the census's domain, as the selected tree records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CensusFile {
    pub path: FilePathId,
    /// The language the file is parsed as.
    pub language: LanguageId,
    /// The content identity of its bytes in the tree.
    pub blob: Hash256,
    /// Whether the graph records a parse of the file: `Some(true)` when it
    /// holds the file's layout, `None` when this reader cannot tell. A parsed
    /// file with no entity holds nothing a record would list, since an
    /// adapter always gives a non-empty record an entity to ride on.
    pub parsed: Option<bool>,
}

/// What the census reads from a graph, and nothing more, so a reader of a
/// replayed or selected generation that is not a [`GraphStore`] can answer
/// from that generation.
pub trait EscapeCensusGraph {
    /// Every file of `languages` in the selected tree, including files with
    /// no entities, or `None` when the tree or any in-domain source path cannot
    /// be read. Unreadable source paths must never be omitted from this inventory.
    fn census_files(&self, languages: &[LanguageId]) -> Option<Vec<CensusFile>>;
    /// Every entity of the file, or `None` when the entity index cannot be
    /// read.
    fn entities_in(&self, path: &FilePathId) -> Option<Vec<Entity>>;
    /// The evidence of every `Imports` edge that lands on `target`, or `None`
    /// when its relations cannot be read.
    fn import_evidence_into(&self, target: EntityId) -> Option<Vec<RelationEvidence>>;
    /// Every `Calls`, `Imports` and `References` edge that lands on `target`,
    /// as the mention census matches non-call reads against it, or `None`
    /// when they cannot be read. A graph that cannot say leaves the mention
    /// reading unmeasured, never matched.
    fn uses_into(&self, _target: EntityId) -> Option<Vec<StoredUse>> {
        None
    }
    /// The member reads of `file` this graph holds a proof for: each is no
    /// call, and reads the member of a type outside the repository, because
    /// the facts of the function it sits in type its receiver so. Empty when
    /// the graph holds none for the file's current body or cannot say, which
    /// leaves every read to the binding rule.
    fn outside_member_reads(&self, _file: &CensusFile) -> Vec<OutsideMemberRead> {
        Vec::new()
    }
    /// The member reads of `file` whose receiver this graph types as a class
    /// of the repository: the facts of the function the read sits in declare,
    /// construct or return it as that class. Empty when the graph holds none
    /// for the file's current body or cannot say. It says what the receiver
    /// is; whether the read could get a given member is the census's to
    /// decide from the class records.
    fn typed_member_reads(&self, _file: &CensusFile) -> Vec<TypedMemberRead> {
        Vec::new()
    }
    /// What the import statements written in `file` land on, where this
    /// graph resolved them: one entry for each `Imports` edge whose evidence
    /// sits in the file. Empty when the graph holds none or cannot say,
    /// which leaves every import to the name rule.
    fn imports_written_in(&self, _file: &CensusFile) -> Vec<WrittenImport> {
        Vec::new()
    }
}

/// One import statement a graph resolved (see
/// [`EscapeCensusGraph::imports_written_in`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenImport {
    /// Where the edge's evidence starts in the importing file, in bytes.
    pub start: usize,
    /// Where it ends.
    pub end: usize,
    /// The local name the evidence says the statement binds, when it says.
    pub token: Option<String>,
    /// The declaration the edge lands on.
    pub target: EntityId,
    /// Whether the edge proves the statement binds that declaration's own
    /// definition and nothing else: the edge is proven, it is not one of an
    /// open set a build chooses from, and the declaration is a function, a
    /// method or a class, never a variable, which may hold what another
    /// declaration defines.
    pub exact: bool,
}

/// One member read whose receiver a graph types as a class of the repository
/// (see [`EscapeCensusGraph::typed_member_reads`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedMemberRead {
    /// The line of the read, counted from zero as a value-escape record
    /// counts it.
    pub line: u32,
    /// The member read.
    pub member: String,
    /// The receiver it is read off, as written.
    pub receiver: String,
    /// The class the receiver's value is typed as, by its own name.
    pub class: String,
}

/// One member read a graph holds a proof for (see
/// [`EscapeCensusGraph::outside_member_reads`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutsideMemberRead {
    /// The line of the read, counted from zero as a value-escape record
    /// counts it.
    pub line: u32,
    /// The member read.
    pub member: String,
    /// The receiver it is read off, as written.
    pub receiver: String,
    /// The type outside the repository whose member it is.
    pub owner: String,
}

/// The member reads a graph proves outside, read once for each file a
/// followed name is read in.
struct ProvenOutside<'a> {
    graph: &'a dyn EscapeCensusGraph,
    files: HashMap<&'a str, &'a CensusFile>,
    by_file: std::cell::RefCell<HashMap<String, Vec<OutsideMemberRead>>>,
    /// The member reads a graph types the receiver of, read the same way.
    typed_by_file: std::cell::RefCell<HashMap<String, Vec<TypedMemberRead>>>,
    /// What a graph resolved each file's import statements to, read the same
    /// way.
    imports_by_file: std::cell::RefCell<HashMap<String, Vec<WrittenImport>>>,
}

impl<'a> ProvenOutside<'a> {
    fn new(graph: &'a dyn EscapeCensusGraph, domain: &[&'a DomainFile]) -> Self {
        Self {
            graph,
            files: domain
                .iter()
                .map(|file| (file.file.path.0.as_str(), &file.file))
                .collect(),
            by_file: Default::default(),
            typed_by_file: Default::default(),
            imports_by_file: Default::default(),
        }
    }

    /// Whether the import statement of `path` spanning `range` that binds
    /// `local` is one the graph resolved to exactly one declaration's own
    /// definition, and that declaration is not `focal`. Such a statement
    /// binds the other declaration, whatever name the two share.
    fn import_binds_another(
        &self,
        path: &str,
        range: (u32, u32),
        local: &str,
        focal: EntityId,
    ) -> bool {
        let Some(file) = self.files.get(path) else {
            return false;
        };
        let mut by_file = self.imports_by_file.borrow_mut();
        let imports = by_file
            .entry(path.to_string())
            .or_insert_with(|| self.graph.imports_written_in(file));
        let landed: Vec<&WrittenImport> = imports
            .iter()
            .filter(|import| {
                range.0 as usize <= import.start
                    && import.start < import.end
                    && import.end <= range.1 as usize
                    && import.token.as_deref() == Some(local)
            })
            .collect();
        let Some(first) = landed.first() else {
            return false;
        };
        landed
            .iter()
            .all(|import| import.exact && import.target == first.target)
            && first.target != focal
    }

    /// The class the graph types the receiver of the member read
    /// `occurrence` as, when it does for that read of `name` in `path`.
    fn typed_class(
        &self,
        path: &str,
        name: &str,
        occurrence: &kin_model::ValueOccurrence,
    ) -> Option<String> {
        if occurrence.binding != OccurrenceBinding::Member {
            return None;
        }
        let receiver = occurrence.receiver.as_deref()?;
        let file = self.files.get(path)?;
        let mut by_file = self.typed_by_file.borrow_mut();
        let reads = by_file
            .entry(path.to_string())
            .or_insert_with(|| self.graph.typed_member_reads(file));
        reads
            .iter()
            .find(|read| {
                read.line == occurrence.line && read.member == name && read.receiver == receiver
            })
            .map(|read| read.class.clone())
    }

    /// The receiver and the outside type of the member read `occurrence` is,
    /// when the graph holds a proof for that read of `name` in `path`.
    fn of(
        &self,
        path: &str,
        name: &str,
        occurrence: &kin_model::ValueOccurrence,
    ) -> Option<(String, String)> {
        if occurrence.binding != OccurrenceBinding::Member {
            return None;
        }
        let receiver = occurrence.receiver.as_deref()?;
        let file = self.files.get(path)?;
        let mut by_file = self.by_file.borrow_mut();
        let reads = by_file
            .entry(path.to_string())
            .or_insert_with(|| self.graph.outside_member_reads(file));
        reads
            .iter()
            .find(|read| {
                read.line == occurrence.line && read.member == name && read.receiver == receiver
            })
            .map(|read| (receiver.to_string(), read.owner.clone()))
    }
}

/// One stored edge that lands on a focal, as the mention census reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredUse {
    /// `Calls`, `Imports` or `References`.
    pub kind: RelationKind,
    /// Where the edge's source entity is declared, when the graph holds it.
    /// A read inside it is covered by an edge that records no site of its
    /// own.
    pub source: Option<SourceSpan>,
    /// Where each of the edge's evidence records sits.
    pub sites: Vec<SourceSpan>,
}

impl StoredUse {
    /// The kinds of edge a mention census reads.
    pub const KINDS: [RelationKind; 3] = [
        RelationKind::Calls,
        RelationKind::Imports,
        RelationKind::References,
    ];

    /// `relation` as a stored use when it is one of [`Self::KINDS`] and
    /// lands on `target`, its source declared at `source`.
    pub fn of(
        relation: &kin_model::Relation,
        target: EntityId,
        source: Option<SourceSpan>,
    ) -> Option<Self> {
        (Self::KINDS.contains(&relation.kind) && relation.dst.as_entity() == Some(target)).then(
            || Self {
                kind: relation.kind,
                source,
                sites: relation
                    .evidence
                    .iter()
                    .filter_map(|evidence| evidence.source_span.clone())
                    .collect(),
            },
        )
    }

    /// Whether this edge covers a read on the 0-based `line` of `file`: one
    /// of its sites spans the line, or it records no site and its source
    /// entity does.
    fn covers(&self, file: &str, line: u32) -> bool {
        let spans = |span: &SourceSpan| {
            span.file.0 == file && span.start_line <= line && line <= span.end_line
        };
        if self.sites.is_empty() {
            self.source.as_ref().is_some_and(spans)
        } else {
            self.sites.iter().any(spans)
        }
    }
}

/// The wire word of a stored use's kind.
fn use_kind_wire(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Calls => "calls",
        RelationKind::Imports => "imports",
        _ => "references",
    }
}

/// A [`GraphStore`] read as an [`EscapeCensusGraph`]: its resolved tree, its
/// entities and its relations.
pub struct StoreCensus<'s, G: GraphStore + ?Sized> {
    pub store: &'s G,
}

impl<G: GraphStore + ?Sized> EscapeCensusGraph for StoreCensus<'_, G> {
    fn census_files(&self, languages: &[LanguageId]) -> Option<Vec<CensusFile>> {
        let tree = self.store.resolved_tree_snapshot().ok()??;
        let mut files = Vec::new();
        for artifact in tree.artifacts() {
            let TreeEntry::Blob { hash, .. } = artifact.entry else {
                continue;
            };
            // Classify the extension before decoding the entire path. A
            // raw-byte asset is outside the domain; a raw-byte source path
            // makes its inventory unreadable rather than silently smaller.
            let bytes = artifact.path.as_bytes();
            let language = bytes
                .iter()
                .rposition(|byte| *byte == b'.')
                .and_then(|at| std::str::from_utf8(&bytes[at..]).ok())
                .and_then(language_of_path)
                .filter(|language| languages.contains(language));
            let Some(language) = language else { continue };
            let path = FilePathId::new(artifact.path.as_utf8()?);
            let parsed = Some(self.store.get_file_layout(&path).ok().flatten().is_some());
            files.push(CensusFile {
                path,
                language,
                blob: hash,
                parsed,
            });
        }
        Some(files)
    }

    fn entities_in(&self, path: &FilePathId) -> Option<Vec<Entity>> {
        self.store
            .query_entities(&EntityFilter {
                file_path: Some(path.clone()),
                ..Default::default()
            })
            .ok()
    }

    fn import_evidence_into(&self, target: EntityId) -> Option<Vec<RelationEvidence>> {
        let relations = self.store.get_all_relations_for_entity(&target).ok()?;
        Some(
            relations
                .into_iter()
                .filter(|relation| {
                    relation.kind == RelationKind::Imports
                        && relation.dst.as_entity() == Some(target)
                })
                .flat_map(|relation| relation.evidence)
                .collect(),
        )
    }

    fn uses_into(&self, target: EntityId) -> Option<Vec<StoredUse>> {
        let relations = self.store.get_all_relations_for_entity(&target).ok()?;
        let mut uses = Vec::new();
        for relation in &relations {
            if !StoredUse::KINDS.contains(&relation.kind)
                || relation.dst.as_entity() != Some(target)
            {
                continue;
            }
            let source = match relation.src.as_entity() {
                Some(source) => self.store.get_entity(&source).ok()?.and_then(|e| e.span),
                None => None,
            };
            uses.extend(StoredUse::of(relation, target, source));
        }
        Some(uses)
    }
}

/// [`focal_escape_evidence_batch`] for one focal of a [`GraphStore`].
pub fn focal_escape_evidence<G: GraphStore + ?Sized>(
    store: &G,
    focal: &Entity,
    names: &[String],
) -> FocalEscape {
    focal_escape_evidence_in(&StoreCensus { store }, focal, names)
}

/// [`focal_escape_evidence_batch`] for one focal of any census graph.
pub fn focal_escape_evidence_in(
    graph: &dyn EscapeCensusGraph,
    focal: &Entity,
    names: &[String],
) -> FocalEscape {
    focal_escape_evidence_batch(graph, &[(focal, names.to_vec())])
        .pop()
        .unwrap_or(FocalEscape::Unknown {
            reason: "the census answered no focal",
        })
}

/// The source digest an entity records for the bytes it was parsed from.
fn blob_of(entity: &Entity) -> Option<Hash256> {
    entity
        .metadata
        .extra
        .get("blob_hash")
        .and_then(Value::as_str)
        .and_then(|hex| Hash256::from_hex(hex).ok())
}

/// One file of the domain as the census reads it.
enum FileReading {
    /// The file's record, carried by an entity parsed from the selected
    /// bytes, in a version this build reads.
    Recorded(Value),
    /// The graph records a parse of the file and no entity: nothing to list.
    Empty,
    /// No parse of the file is on record, so nothing speaks for it.
    NoParse,
    /// The file has entities and none carries a current record.
    Unrecorded,
    /// Its entities could not be read.
    Unreadable,
}

struct DomainFile {
    file: CensusFile,
    entities: usize,
    /// The parser's line binding cannot distinguish same-named declarations
    /// in different scopes on one source line.
    unique_declarations: HashSet<EntityId>,
    /// Current graph owners, available only for an explicit role-scoped read.
    occurrence_owners: Vec<OccurrenceOwner>,
    /// The classes the file's entities declare, by the lines each covers, or
    /// `None` when an entity of the file does not speak for the bytes its
    /// record was written from.
    class_spans: Option<Vec<ClassSpan>>,
    reading: FileReading,
}

/// One class entity of a file: its own name and the lines its span covers.
struct ClassSpan {
    name: String,
    start_line: u32,
    end_line: u32,
}

/// The class entities of `file`, or `None` when any of them was parsed from
/// other bytes than the file holds, so its lines say nothing about the
/// record's.
fn class_spans(file: &CensusFile, entities: &[Entity]) -> Option<Vec<ClassSpan>> {
    let mut spans = Vec::new();
    for entity in entities.iter().filter(|entity| holds_members(entity.kind)) {
        let span = entity.span.as_ref()?;
        if span.file != file.path
            || span.start_line > span.end_line
            || blob_of(entity) != Some(file.blob)
        {
            return None;
        }
        let name = entity
            .name
            .rsplit(['.', ':'])
            .find(|segment| !segment.is_empty())?;
        spans.push(ClassSpan {
            name: name.to_string(),
            start_line: span.start_line,
            end_line: span.end_line,
        });
    }
    Some(spans)
}

/// The class a member read off the instance receiver of `occurrence` is read
/// on: the innermost class whose body holds the line, for a receiver the
/// adapter classed as the instance a member runs on and that is written as
/// exactly that name (`self`, `cls`), never a path off it.
fn instance_receiver_class<'s>(
    occurrence: &kin_model::ValueOccurrence,
    spans: Option<&&'s [ClassSpan]>,
) -> Option<&'s ClassSpan> {
    if occurrence.binding != OccurrenceBinding::Member
        || occurrence.receiver_kind != Some(ReceiverKind::Instance)
    {
        return None;
    }
    let receiver = occurrence.receiver.as_deref()?;
    if receiver.is_empty() || !receiver.chars().all(|c| c == '_' || c.is_alphanumeric()) {
        return None;
    }
    enclosing_class(spans?, occurrence.line)
}

/// The innermost class of `spans` whose lines hold `line`, when exactly one
/// class is innermost there.
fn enclosing_class(spans: &[ClassSpan], line: u32) -> Option<&ClassSpan> {
    let mut covering = spans
        .iter()
        .filter(|class| class.start_line <= line && line <= class.end_line);
    let mut innermost = covering.next()?;
    let mut tied = false;
    for class in covering {
        let (lines, best) = (
            class.end_line - class.start_line,
            innermost.end_line - innermost.start_line,
        );
        if lines < best {
            innermost = class;
            tied = false;
        } else if lines == best {
            tied = true;
        }
    }
    (!tied).then_some(innermost)
}

struct OccurrenceOwner {
    span: SourceSpan,
    role: EntityRole,
}

/// Line-only census records cannot locate a token on a declaration boundary.
/// Retain those occurrences, and every ambiguous or stale owner, in scope.
fn occurrence_owners(
    file: &CensusFile,
    entities: &[Entity],
    record: &Value,
) -> Vec<OccurrenceOwner> {
    if file.parsed != Some(true)
        || serde_json::from_value::<kin_model::FileEscapeRecord>(record.clone()).is_err()
    {
        return Vec::new();
    }
    let digest = file.blob.to_string();
    let mut ids = HashSet::new();
    let mut owners = Vec::with_capacity(entities.len());
    for entity in entities {
        let Some(span) = entity.span.as_ref() else {
            return Vec::new();
        };
        if !ids.insert(entity.id)
            || entity.language != file.language
            || entity.file_origin.as_ref() != Some(&file.path)
            || span.file != file.path
            || span.start_byte >= span.end_byte
            || span.start_line > span.end_line
            || kin_model::is_derived_member(entity)
            || entity
                .metadata
                .extra
                .get("blob_hash")
                .and_then(Value::as_str)
                != Some(digest.as_str())
            || entity
                .metadata
                .extra
                .get(VALUE_ESCAPE_RECORD_KEY)
                .is_some_and(|held| held != record)
        {
            return Vec::new();
        }
        owners.push(OccurrenceOwner {
            span: span.clone(),
            role: entity.role,
        });
    }
    owners
}

fn test_owned_occurrence(
    owners: &HashMap<&str, &[OccurrenceOwner]>,
    file: &str,
    line: u32,
) -> bool {
    let Some(owners) = owners.get(file) else {
        return false;
    };
    let covering: Vec<_> = owners
        .iter()
        .filter(|owner| owner.span.start_line <= line && line <= owner.span.end_line)
        .collect();
    let inside =
        |a: &SourceSpan, b: &SourceSpan| b.start_byte <= a.start_byte && a.end_byte <= b.end_byte;
    for (index, owner) in covering.iter().enumerate() {
        for other in &covering[index + 1..] {
            if (owner.span.start_byte == other.span.start_byte
                && owner.span.end_byte == other.span.end_byte)
                || (!inside(&owner.span, &other.span) && !inside(&other.span, &owner.span))
            {
                return false;
            }
        }
    }
    covering
        .iter()
        .min_by_key(|owner| owner.span.end_byte - owner.span.start_byte)
        .is_some_and(|owner| {
            owner.role == EntityRole::Test
                && owner.span.start_line < line
                && line < owner.span.end_line
        })
}

fn declaration_line(entity: &Entity) -> Option<u32> {
    entity
        .metadata
        .extra
        .get("declaration_line")
        .and_then(Value::as_u64)
        .map(|line| line as u32)
        .or_else(|| entity.span.as_ref().map(|span| span.start_line))
}

/// Read every file of `languages` once for a whole batch.
fn read_domain(
    graph: &dyn EscapeCensusGraph,
    languages: &[LanguageId],
    include_tests: bool,
) -> Option<Vec<DomainFile>> {
    let mut files = graph.census_files(languages)?;
    files.sort_by(|left, right| left.path.0.cmp(&right.path.0));
    Some(
        files
            .into_iter()
            .map(|file| {
                let mut unique_declarations = HashSet::new();
                let mut scoped_owners = Vec::new();
                let mut classes = None;
                let (entities, reading) = match graph.entities_in(&file.path) {
                    None => (0, FileReading::Unreadable),
                    Some(entities) if entities.is_empty() => (
                        0,
                        if file.parsed == Some(true) {
                            FileReading::Empty
                        } else {
                            FileReading::NoParse
                        },
                    ),
                    Some(entities) => {
                        let mut declarations: HashMap<(&str, u32), Vec<EntityId>> = HashMap::new();
                        for entity in entities
                            .iter()
                            .filter(|entity| entity.kind != EntityKind::Module)
                        {
                            if let Some(line) = declaration_line(entity) {
                                let name = entity
                                    .name
                                    .rsplit(['.', ':'])
                                    .next()
                                    .unwrap_or(&entity.name);
                                declarations
                                    .entry((name, line))
                                    .or_default()
                                    .push(entity.id);
                            }
                        }
                        unique_declarations.extend(
                            declarations
                                .values()
                                .filter(|ids| ids.len() == 1)
                                .map(|ids| ids[0]),
                        );
                        let record = entities.iter().find_map(|entity| {
                            let value = entity.metadata.extra.get(VALUE_ESCAPE_RECORD_KEY)?;
                            (blob_of(entity) == Some(file.blob)
                                && value
                                    .get("v")
                                    .and_then(Value::as_u64)
                                    .is_some_and(readable_record_version))
                            .then(|| value.clone())
                        });
                        if !include_tests {
                            if let Some(record) = &record {
                                scoped_owners = occurrence_owners(&file, &entities, record);
                            }
                        }
                        classes = class_spans(&file, &entities);
                        (
                            entities.len(),
                            match record {
                                Some(record) => FileReading::Recorded(record),
                                None => FileReading::Unrecorded,
                            },
                        )
                    }
                };
                DomainFile {
                    file,
                    entities,
                    unique_declarations,
                    occurrence_owners: scoped_owners,
                    class_spans: classes,
                    reading,
                }
            })
            .collect(),
    )
}

/// Whether each focal, spelled by its call names
/// ([`kin_model::focal_call_names`]), escapes as a value, one answer per
/// focal in order. Every file is read at most once for the whole batch.
///
/// A focal's domain is every file of its calling languages in the selected
/// tree, whether or not it holds entities. Each file speaks through the record
/// its adapter wrote; a file with entities and no current record, or whose
/// entities cannot be read, makes the focal unknown. Within the records:
///
/// - every non-call use of a call name that could read the focal (by its
///   lexical binding, its receiver and the import edges landing on the focal)
///   is an alias, a same-named binding, or a use that holds the focal as a
///   value. Aliases widen the names followed, until no new one appears;
/// - every reflective site that could fetch the focal, whether or not its
///   value is called on the spot, is listed by what it can read (see
///   [`site_reaches`]): a computed member lookup reads members of its
///   receiver, so a module-level declaration only through a receiver that
///   may be its module object, and a member only through one that is not a
///   module object; a scope table reads its own module; a module-private
///   declaration is read only by its own file's sites unless an alias or an
///   import binds it elsewhere. A site whose record is evidence about a
///   followed name ([`kin_model::ReflectiveSite::names_key`]) is listed under
///   `reflective`; one that looks up a name no record follows, and spells no
///   followed name, reaches the focal only as it reaches anything its
///   receiver holds and is counted under `unkeyed_lookups`;
/// - a protocol member lists how the language invokes it, and every implicit
///   call that spells a call name (a JSX element, a decorator, `raise Name`,
///   `f.call(...)`) is listed;
/// - an unparsed region that spells a call name, and a file with no parse on
///   record, are listed as unrecorded.
///
/// The focal escapes when any use holds it as a value, and is contained
/// otherwise. A focal with no call name is unknown.
pub fn focal_escape_evidence_batch(
    graph: &dyn EscapeCensusGraph,
    focals: &[(&Entity, Vec<String>)],
) -> Vec<FocalEscape> {
    focal_escape_evidence_batch_scoped(graph, focals, true)
}

/// The same census with an explicit caller-role scope. When tests are excluded,
/// omit only direct mentions and implicit-call sites with a unique current Test
/// owner. Value, alias and reflective reach remain global: a Test can pass a
/// callable into a non-Test caller. Missing census and uncertain owners remain
/// in scope, and the default entry point continues to include every role.
pub fn focal_escape_evidence_batch_scoped(
    graph: &dyn EscapeCensusGraph,
    focals: &[(&Entity, Vec<String>)],
    include_tests: bool,
) -> Vec<FocalEscape> {
    let mut answers: Vec<Option<FocalEscape>> = Vec::with_capacity(focals.len());
    let mut languages: Vec<LanguageId> = Vec::new();
    for (entity, names) in focals {
        let answer = if let Some(proof) = non_callable_binding(graph, entity) {
            Some(proof)
        } else if names.iter().all(|name| name.is_empty()) {
            Some(FocalEscape::Unknown {
                reason: "the focal has no name a call spells",
            })
        } else {
            None
        };
        if answer.is_none() {
            for language in calling_languages(entity.language) {
                if !languages.contains(&language) {
                    languages.push(language);
                }
            }
        }
        answers.push(answer);
    }
    if languages.is_empty() {
        return answers
            .into_iter()
            .map(|answer| answer.expect("every focal answered"))
            .collect();
    }
    let domain = read_domain(graph, &languages, include_tests);
    let roots = ImportRoots::of(
        domain
            .iter()
            .flatten()
            .map(|file| file.file.path.0.as_str()),
    );
    focals
        .iter()
        .zip(answers)
        .map(|((focal, names), answer)| {
            if let Some(answer) = answer {
                return answer;
            }
            let Some(domain) = domain.as_deref() else {
                return FocalEscape::Unknown {
                    reason: "the selected tree could not be read",
                };
            };
            census_one(graph, domain, &roots, focal, names)
        })
        .collect()
}

/// What the census knows about the focal itself.
struct FocalFacts<'f> {
    file: Option<&'f str>,
    /// Declared inside a class or an object, reached through a receiver.
    member: bool,
    /// Reachable from outside its own file only by a value use, never by a
    /// reflective lookup or a member read of its module.
    module_private: bool,
    /// A member no enumeration of an object's own enumerable properties
    /// yields (see [`kin_model::NON_ENUMERABLE_MEMBER_KEY`]).
    non_enumerable: bool,
    declaration_line: Option<u32>,
    declaration_unique: bool,
    /// The import edges landing on the focal: file, byte range and local name.
    imports: Vec<(String, usize, usize, Option<String>)>,
}

/// Where the census found the focal bound under another name, which decides
/// what else a reflective lookup could read it through.
#[derive(Debug, Clone, Copy, Default)]
struct AliasReach {
    /// Bound by a module- or class-scope assignment or stored into a named
    /// attribute: a member lookup could read it under the alias.
    member: bool,
    /// Bound under a module-level name of some module (an import, an export
    /// or a module-scope assignment): a lookup on that module could read it.
    module: bool,
}

impl AliasReach {
    fn any(self) -> bool {
        self.member || self.module
    }
}

/// Whether a reflective site could fetch the focal, by what the site can read
/// and what the focal is. `own_file` says whether the site is in the focal's
/// own file, `aliases` where the census found the focal bound under another
/// name.
///
/// A computed member lookup reads members of its receiver and nothing else:
///
/// - a member is read through any receiver that is not a module object;
/// - a module-level declaration is a member only of its module object (and of
///   a module that binds it under an alias), so only a lookup whose receiver
///   may be a module object reads it, and a module-private one only from its
///   own file;
/// - a receiver the file declares as a class or function holds its own
///   members, never a module-level declaration, and a receiver that is a
///   module object holds module-level bindings, never a class member.
///
/// A class the site's file declares holds its own members and its bases',
/// which `declared_holds` answers for the focal (see [`ClassTable`]).
///
/// A scope table (`globals()`) reads its own module's module-level bindings,
/// and an evaluation of code reads anything a lookup could. A site that
/// yields only own enumerable properties never reads a non-enumerable member.
fn site_reaches(
    site: &kin_model::ReflectiveSite,
    facts: &FocalFacts<'_>,
    own_file: bool,
    aliases: AliasReach,
    declared_holds: impl FnOnce(&str, u32) -> bool,
) -> bool {
    // Read as a member: the focal is one, or an alias stored it where a member
    // lookup reads.
    let as_member = facts.member || aliases.member;
    // Read as a module-level binding of some module object.
    let as_module_binding =
        (!facts.member && (!facts.module_private || own_file)) || aliases.module;
    if site.enumerable_only && facts.non_enumerable && !aliases.any() {
        return false;
    }
    match site.reach {
        ReflectiveReach::Nothing => false,
        ReflectiveReach::OwnModule => own_file && (!facts.member || aliases.module),
        ReflectiveReach::Members => as_member,
        ReflectiveReach::Any => match &site.receiver {
            Some(LookupReceiver::Declared { name, line }) => {
                aliases.member || (facts.member && declared_holds(name, *line))
            }
            Some(LookupReceiver::Module) => as_module_binding,
            None => as_member || as_module_binding,
        },
    }
}

/// Every class the domain's records declare, by name, so a lookup on a class
/// can be told which classes' members it reads: its own and its bases',
/// transitively. Built on first use.
#[derive(Default)]
struct ClassTable {
    /// Each file's classes by name and declaration line.
    by_file: HashMap<String, Vec<kin_model::ClassBases>>,
    /// Every class declared under a name, in any file.
    by_name: HashMap<String, Vec<kin_model::ClassBases>>,
    /// Every class name that names a base, by that base's name.
    derived: HashMap<String, Vec<String>>,
}

/// How many classes a hierarchy reading visits before it answers that the
/// records do not say.
const HIERARCHY_CLASSES_MAX: usize = 512;

/// How deep a chain of classes declared only for a type checker, each named
/// as a base of the next, is followed.
const TYPING_ONLY_BASES_MAX: usize = 4;

impl ClassTable {
    fn build(records: &[(&str, &Value)]) -> Option<Self> {
        let mut table = Self::default();
        for (path, record) in records {
            let Some(classes) = record.get("classes") else {
                continue;
            };
            let classes: Vec<kin_model::ClassBases> =
                serde_json::from_value(classes.clone()).ok()?;
            for class in &classes {
                table
                    .by_name
                    .entry(class.name.clone())
                    .or_default()
                    .push(class.clone());
                for base in &class.bases {
                    table
                        .derived
                        .entry(base.clone())
                        .or_default()
                        .push(class.name.clone());
                }
            }
            table.by_file.insert((*path).to_string(), classes);
        }
        Some(table)
    }

    /// Whether no value of the class `receiver` can be an instance of
    /// `owner`: no class the records declare is, or derives from, both. A
    /// value of `receiver` is an instance of it or of a class derived from
    /// it, and a member read off such a value is found on that class or one
    /// of its bases, so it is a member of `owner` only when the value's
    /// class derives from `owner` too.
    ///
    /// A class declared only for a type checker is not a class any value
    /// has: a value declared as one is an instance of what its bases
    /// promise (see [`Self::value_classes`]).
    ///
    /// Classes are told apart by name, so two classes declared under one
    /// name count as one and only make this answer "no" more often. It is
    /// `false` whenever the records do not say: either name declares no
    /// class, a class of either hierarchy (the class, its bases, the classes
    /// derived from it and their bases, transitively) has a base no declared
    /// name settles, or one that a value of `receiver` could be an instance
    /// of answers attribute reads by name (`dynamic`).
    fn shares_no_instance(&self, receiver: &str, owner: &str, dynamic: &HashSet<&str>) -> bool {
        if receiver == owner || !self.by_name.contains_key(receiver) {
            return false;
        }
        // A member of a structural type is every matching class's member,
        // and a class for the type checker alone declares none that runs.
        if self.by_name.get(owner).is_none_or(|declared| {
            declared
                .iter()
                .any(|class| class.structural || class.typing_only)
        }) {
            return false;
        }
        let (Some(below_receiver), Some(below_owner)) =
            (self.value_classes(receiver, 0), self.derived_from(owner))
        else {
            return false;
        };
        if below_receiver
            .iter()
            .any(|class| below_owner.contains(class))
        {
            return false;
        }
        // Everything a value of `receiver` finds members on, and everything
        // a member of `owner` could be inherited through: all of it has to be
        // declared, or be a base outside the repository.
        let (Some(receiver_line), Some(owner_line)) = (
            self.with_bases(&below_receiver),
            self.with_bases(&below_owner),
        ) else {
            return false;
        };
        let settled = |name: &&str| {
            self.by_name
                .get(*name)
                .is_none_or(|declared| declared.iter().all(|class| !class.open))
        };
        receiver_line
            .iter()
            .all(|name| settled(name) && !dynamic.contains(name))
            && owner_line.iter().all(|name| settled(name))
    }

    /// The classes a value declared as `name` may be an instance of: `name`
    /// and every class derived from it, or `None` when the records do not
    /// bound them.
    ///
    /// A class declared only for a type checker stands for what its bases
    /// promise: the value is an instance of something in every base's
    /// hierarchy at once, so the classes it may be are those of one base
    /// narrowed by the next. A structural base and a base no file declares
    /// promise nothing about the value's class; when no base promises
    /// anything, or no declared class is in every promising base's hierarchy,
    /// neither does the class, and the answer is `None`. Flask's
    /// `SessionMixinProxy(ProxyMixin[SessionMixin], SessionMixin)` comes to
    /// everything in `SessionMixin`'s hierarchy.
    fn value_classes<'t>(&'t self, name: &'t str, depth: usize) -> Option<HashSet<&'t str>> {
        let declared = self.by_name.get(name)?;
        if depth > TYPING_ONLY_BASES_MAX || declared.iter().any(|class| class.structural) {
            return None;
        }
        let mut classes = self.derived_from(name)?;
        for class in declared.iter().filter(|class| class.typing_only) {
            let mut promised: Option<HashSet<&str>> = None;
            for base in &class.bases {
                let Some(of_base) = self.value_classes(base, depth + 1) else {
                    continue;
                };
                promised = Some(match promised {
                    None => of_base,
                    Some(so_far) => so_far.intersection(&of_base).copied().collect(),
                });
            }
            // Bases no declared class satisfies at once promise a class
            // the records do not hold.
            classes.extend(promised.filter(|promised| !promised.is_empty())?);
        }
        Some(classes)
    }

    /// `name` and every class name the records derive from it, or `None`
    /// past [`HIERARCHY_CLASSES_MAX`].
    fn derived_from<'t>(&'t self, name: &'t str) -> Option<HashSet<&'t str>> {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut frontier = vec![name];
        while let Some(class) = frontier.pop() {
            if !seen.insert(class) {
                continue;
            }
            if seen.len() > HIERARCHY_CLASSES_MAX {
                return None;
            }
            frontier.extend(
                self.derived
                    .get(class)
                    .into_iter()
                    .flatten()
                    .map(String::as_str),
            );
        }
        Some(seen)
    }

    /// `classes` and every base name any of them names, transitively, or
    /// `None` past [`HIERARCHY_CLASSES_MAX`].
    fn with_bases<'t>(&'t self, classes: &HashSet<&'t str>) -> Option<HashSet<&'t str>> {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut frontier: Vec<&str> = classes.iter().copied().collect();
        while let Some(class) = frontier.pop() {
            if !seen.insert(class) {
                continue;
            }
            if seen.len() > HIERARCHY_CLASSES_MAX {
                return None;
            }
            for declared in self.by_name.get(class).into_iter().flatten() {
                frontier.extend(declared.bases.iter().map(String::as_str));
            }
        }
        Some(seen)
    }

    /// Whether the class `file` declares as `name` at `line` may hold a
    /// member of `owner`: `owner` is the class or one of its bases, or a base
    /// on the way is one no declared name settles. A declaration the table
    /// does not hold as a class (a function, or a file whose record names no
    /// classes) may hold anything. A base no file declares is outside the
    /// repository and inherits from nothing in it.
    fn may_hold_member_of(&self, file: &str, name: &str, line: u32, owner: &str) -> bool {
        let Some(start) = self.by_file.get(file).and_then(|classes| {
            classes
                .iter()
                .find(|class| class.name == name && class.line == line)
        }) else {
            return true;
        };
        Self::inherits(self, start, owner)
    }

    /// Whether a name bound in the body of the class `span` covers in `file`
    /// may stand in one hierarchy with a member of `owner`: the class is
    /// `owner`, derives from it and so may replace its member, or is a base
    /// of it and so declares what its member overrides. A class the table
    /// cannot place, or an `owner` it holds no declaration of, may.
    fn may_share_a_member(&self, file: &str, span: &ClassSpan, owner: &str) -> bool {
        let declared: Vec<&kin_model::ClassBases> = self
            .by_file
            .get(file)
            .into_iter()
            .flatten()
            .filter(|class| {
                class.name == span.name
                    && span.start_line <= class.line
                    && class.line <= span.end_line
            })
            .collect();
        // The span's own declaration is the first line of its name inside
        // it; a nested class of the same name sits further down.
        let Some(start) = declared.iter().min_by_key(|class| class.line) else {
            return true;
        };
        if self.inherits(start, owner) {
            return true;
        }
        match self.by_name.get(owner) {
            Some(owners) => owners
                .iter()
                .any(|declared| self.inherits(declared, &span.name)),
            None => true,
        }
    }

    /// Whether `start` is `owner` or inherits from it, or from a base no
    /// declared name settles.
    fn inherits(&self, start: &kin_model::ClassBases, owner: &str) -> bool {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut frontier = vec![start];
        while let Some(class) = frontier.pop() {
            if class.name == owner || class.open {
                return true;
            }
            if !seen.insert(class.name.as_str()) {
                continue;
            }
            for base in &class.bases {
                if base == owner {
                    return true;
                }
                if let Some(declared) = self.by_name.get(base) {
                    frontier.extend(declared.iter());
                }
            }
        }
        false
    }
}

impl FocalFacts<'_> {
    /// Whether a module- or class-scope binding in `file` declared at
    /// `target_line` could be the focal.
    fn declared_here(&self, file: &str, target_line: Option<u32>) -> bool {
        self.file == Some(file)
            && match (target_line, self.declaration_line) {
                (Some(target), Some(declared)) => target == declared,
                _ => true,
            }
    }

    /// Whether an import statement of `file` spanning `range` lands on the
    /// focal.
    fn imported_by(&self, file: &str, range: (u32, u32)) -> bool {
        self.imports.iter().any(|(path, start, end, _)| {
            path == file && *start < range.1 as usize && range.0 as usize <= *end
        })
    }

    /// Alias metadata asserts identity, unlike the conservative name closure
    /// used to find every possible call. Keep only names whose binding is
    /// proved by this declaration or by an import edge into this exact focal.
    fn proven_aliases(
        &self,
        file: &str,
        name: &str,
        occurrence: &kin_model::ValueOccurrence,
    ) -> Vec<String> {
        let declared = matches!(
            occurrence.binding,
            OccurrenceBinding::Module | OccurrenceBinding::Class | OccurrenceBinding::Local
        ) && self.declaration_unique
            && self.file == Some(file)
            && occurrence.target_line.is_some()
            && occurrence.target_line == self.declaration_line;
        occurrence
            .aliases
            .iter()
            .filter(|alias| {
                if declared {
                    return true;
                }
                let Some((first, last)) = occurrence.import else {
                    return false;
                };
                let local = if occurrence.context == OccurrenceContext::Import {
                    alias.as_str()
                } else {
                    name
                };
                self.imports.iter().any(|(path, start, end, token)| {
                    path == file
                        && first as usize <= *start
                        && *start < *end
                        && *end <= last as usize
                        && token.as_deref() == Some(local)
                })
            })
            .cloned()
            .collect()
    }
}

fn census_one(
    graph: &dyn EscapeCensusGraph,
    domain: &[DomainFile],
    roots: &ImportRoots,
    focal: &Entity,
    names: &[String],
) -> FocalEscape {
    let base: Vec<String> = names
        .iter()
        .filter(|name| !name.is_empty())
        .cloned()
        .collect();
    let Some(imports) = graph.import_evidence_into(focal.id) else {
        return FocalEscape::Unknown {
            reason: "the focal's relations could not be read",
        };
    };
    let languages = calling_languages(focal.language);
    let in_domain: Vec<&DomainFile> = domain
        .iter()
        .filter(|file| languages.contains(&file.file.language))
        .collect();
    let occurrence_owners: HashMap<&str, &[OccurrenceOwner]> = in_domain
        .iter()
        .filter(|file| !file.occurrence_owners.is_empty())
        .map(|file| (file.file.path.0.as_str(), file.occurrence_owners.as_slice()))
        .collect();
    let mut entities_checked = 0usize;
    let mut records: Vec<(&str, &Value)> = Vec::new();
    let mut residue = EscapeResidue::default();
    for file in &in_domain {
        entities_checked += file.entities;
        match &file.reading {
            FileReading::Recorded(record) => records.push((file.file.path.0.as_str(), record)),
            FileReading::Empty => {}
            FileReading::NoParse => residue.unrecorded.push(ResidueSite {
                file: file.file.path.0.clone(),
                line: 1,
                kind: "unrecorded",
                name: None,
                detail: Some("the graph records no parse of the file".into()),
            }),
            FileReading::Unrecorded => {
                return FocalEscape::Unknown {
                    reason: "a source unit of the domain has entities and no value-occurrence record from this build; re-admit the repository",
                }
            }
            FileReading::Unreadable => {
                return FocalEscape::Unknown {
                    reason: "the entity index could not be read for a source unit of the domain",
                }
            }
        }
    }
    let file = focal
        .file_origin
        .as_ref()
        .or_else(|| focal.span.as_ref().map(|span| &span.file))
        .map(|file| file.0.as_str());
    let own_record = file.and_then(|file| {
        records
            .iter()
            .find(|(path, _)| *path == file)
            .map(|(_, record)| *record)
    });
    let member = matches!(focal.kind, EntityKind::Method | EntityKind::Field);
    let exported = own_record.is_some_and(|record| {
        let exports = record.get("exports").and_then(Value::as_array);
        let leaf = focal.name.rsplit(['.', ':']).next().unwrap_or(&focal.name);
        exports.is_some_and(|exports| exports.iter().any(|name| name.as_str() == Some(leaf)))
    });
    let facts = FocalFacts {
        file,
        member,
        module_private: !member && focal.visibility == Visibility::Private && !exported,
        non_enumerable: member
            && focal
                .metadata
                .extra
                .get(NON_ENUMERABLE_MEMBER_KEY)
                .and_then(Value::as_bool)
                == Some(true),
        declaration_line: declaration_line(focal),
        declaration_unique: in_domain
            .iter()
            .any(|file| file.unique_declarations.contains(&focal.id)),
        imports: imports
            .iter()
            .filter_map(|evidence| {
                evidence.source_span.as_ref().map(|span| {
                    (
                        span.file.0.clone(),
                        span.start_byte,
                        span.end_byte,
                        evidence.token.clone(),
                    )
                })
            })
            .collect(),
    };
    let module_scope_closed = own_record
        .and_then(|record| record.get("scope"))
        .and_then(Value::as_str)
        == Some("closed");

    // The class the focal is a member of, by name, which a lookup on a class
    // reaches only through that class's own hierarchy.
    let owner: Option<&str> = member
        .then(|| {
            let mut segments = focal
                .name
                .rsplit(['.', ':'])
                .filter(|segment| !segment.is_empty());
            segments.next();
            segments.next()
        })
        .flatten();
    let mut classes: Option<Option<ClassTable>> = None;
    let class_spans: HashMap<&str, &[ClassSpan]> = in_domain
        .iter()
        .filter_map(|file| Some((file.file.path.0.as_str(), file.class_spans.as_deref()?)))
        .collect();

    // Follow the names, widening by every alias found, until none is new.
    let mut followed: Vec<String> = base.clone();
    let mut alias_files: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut values: BTreeSet<ResidueSite> = BTreeSet::new();
    let mut bindings: BTreeSet<ResidueSite> = BTreeSet::new();
    let mut alias_sites: BTreeSet<ResidueSite> = BTreeSet::new();
    // Reads of a followed name on a receiver imported from outside the
    // repository, by receiver, module and name.
    let mut outside: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    // Reads of a followed name on a receiver whose value the graph proves to
    // be of a type outside the repository, by receiver, type and name.
    let proven = ProvenOutside::new(graph, &in_domain);
    let mut outside_values: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    // Reads of a followed member name off the instance receiver of a class
    // that shares no instance with the focal's owner, by receiver, class and
    // name.
    let mut unrelated: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    // Which classes answer attribute reads by name is known only where every
    // record states it on its classes and every file's class entities speak
    // for its record.
    let hooks_known = in_domain.iter().all(|file| {
        file.class_spans.is_some() || !matches!(file.reading, FileReading::Recorded(_))
    }) && records.iter().all(|(_, record)| {
        record
            .get("v")
            .and_then(Value::as_u64)
            .is_some_and(|version| version >= u64::from(kin_model::CLASS_MARKS_RECORD_VERSION))
    });
    let dynamic: HashSet<&str> = records
        .iter()
        .filter_map(|(_, record)| record.get("classes").and_then(Value::as_array))
        .flatten()
        .filter(|class| class.get("d").and_then(Value::as_bool) == Some(true))
        .filter_map(|class| class.get("n").and_then(Value::as_str))
        .collect();
    // A module that imports the focal binds it at its module level too.
    let mut alias_reach = AliasReach {
        member: false,
        module: !facts.imports.is_empty(),
    };
    let mut cursor = 0;
    while cursor < followed.len() {
        if followed.len() > ESCAPE_ALIAS_NAMES_MAX {
            return FocalEscape::Unknown {
                reason: "the focal is aliased under more names than the census follows",
            };
        }
        let name = followed[cursor].clone();
        cursor += 1;
        if !visited.insert(name.clone()) {
            continue;
        }
        let is_base = base.contains(&name);
        for (path, record) in &records {
            let Some(occurrences) = record
                .get("values")
                .and_then(|values| values.get(&name))
                .and_then(Value::as_array)
            else {
                continue;
            };
            for occurrence in occurrences {
                let Ok(occurrence) =
                    serde_json::from_value::<kin_model::ValueOccurrence>(occurrence.clone())
                else {
                    return FocalEscape::Unknown {
                        reason: "a value-occurrence record could not be read",
                    };
                };
                if occurrence.context == OccurrenceContext::Mention {
                    // A read whose value flows nowhere: neither an alias nor
                    // a value a call could reach it through. The mention
                    // reading below matches it against the stored rows.
                    continue;
                }
                if occurrence.context == OccurrenceContext::Binding {
                    // A same-named binding is another value a call spelling
                    // the name may reach: a member's for a member, a module
                    // name's or an attribute's for a module-level focal. It
                    // is listed only where it binds the focal's own name: in
                    // the focal's module, or in a class that shares the
                    // focal's hierarchy. Anywhere else it declares a name of
                    // its own, as a `def` of that name there would, and no
                    // census lists those.
                    let relevant = match occurrence.binding {
                        OccurrenceBinding::Class => {
                            facts.member
                                && owner.is_none_or(|owner| {
                                    let Some(class) = class_spans
                                        .get(path)
                                        .and_then(|spans| enclosing_class(spans, occurrence.line))
                                    else {
                                        return true;
                                    };
                                    classes
                                        .get_or_insert_with(|| ClassTable::build(&records))
                                        .as_ref()
                                        .is_none_or(|table| {
                                            table.may_share_a_member(path, class, owner)
                                        })
                                })
                        }
                        OccurrenceBinding::Member => true,
                        OccurrenceBinding::Module => {
                            !facts.member && facts.file.is_none_or(|own| own == *path)
                        }
                        _ => !facts.member,
                    };
                    if relevant {
                        bindings.insert(ResidueSite {
                            file: (*path).to_string(),
                            line: occurrence.line + 1,
                            kind: occurrence.context.wire(),
                            name: Some(name.clone()),
                            detail: occurrence.receiver.clone(),
                        });
                    }
                    continue;
                }
                // A member read off a value of a type some other project
                // declares is that type's member, whatever this repository
                // names the same, member or not.
                if let Some((receiver, owner)) = proven.of(path, &name, &occurrence) {
                    *outside_values
                        .entry((receiver, owner, name.clone()))
                        .or_insert(0) += 1;
                    continue;
                }
                // A member read off `self` in a class that shares no
                // instance with the focal's owner reads that class's own
                // hierarchy, never this member.
                if let (true, Some(owner), Some(class)) = (
                    hooks_known,
                    owner,
                    instance_receiver_class(&occurrence, class_spans.get(path)),
                ) {
                    let unrelated_class = classes
                        .get_or_insert_with(|| ClassTable::build(&records))
                        .as_ref()
                        .is_some_and(|table| {
                            table.shares_no_instance(&class.name, owner, &dynamic)
                        });
                    if unrelated_class {
                        let receiver = occurrence.receiver.clone().unwrap_or_default();
                        *unrelated
                            .entry((receiver, class.name.clone(), name.clone()))
                            .or_insert(0) += 1;
                        continue;
                    }
                }
                // The same for a receiver the graph types as a class: a
                // declared global, an annotated or constructed local.
                if let (true, Some(owner), Some(class)) = (
                    hooks_known,
                    owner,
                    proven.typed_class(path, &name, &occurrence),
                ) {
                    let unrelated_class = classes
                        .get_or_insert_with(|| ClassTable::build(&records))
                        .as_ref()
                        .is_some_and(|table| table.shares_no_instance(&class, owner, &dynamic));
                    if unrelated_class {
                        let receiver = occurrence.receiver.clone().unwrap_or_default();
                        *unrelated
                            .entry((receiver, class, name.clone()))
                            .or_insert(0) += 1;
                        continue;
                    }
                }
                // A member of a module some other project supplies is not a
                // module-level declaration of this one, whatever its name.
                if !facts.member {
                    if let Some((receiver, module)) =
                        outside_imports::outside_receiver(roots, path, record, &occurrence)
                    {
                        *outside.entry((receiver, module, name.clone())).or_insert(0) += 1;
                        continue;
                    }
                }
                let aliased_here = alias_files
                    .get(&name)
                    .is_some_and(|files| files.contains(*path));
                if !could_read_focal(
                    &facts,
                    path,
                    &name,
                    is_base,
                    aliased_here,
                    &occurrence,
                    &base,
                    module_scope_closed,
                ) {
                    continue;
                }
                let site = ResidueSite {
                    file: (*path).to_string(),
                    line: occurrence.line + 1,
                    kind: occurrence.context.wire(),
                    name: Some(name.clone()),
                    detail: occurrence.receiver.clone(),
                };
                match occurrence.context {
                    OccurrenceContext::Import
                    | OccurrenceContext::Alias
                    | OccurrenceContext::Export
                        if !occurrence.aliases.is_empty() =>
                    {
                        // An import or an export binds a module-level
                        // name; an assignment's record does not say whether
                        // it binds a module- or class-scope name or stores
                        // into an attribute, so it counts as both.
                        alias_reach.module = true;
                        if occurrence.context == OccurrenceContext::Alias {
                            alias_reach.member = true;
                        }
                        let mut widened = false;
                        for alias in &occurrence.aliases {
                            if alias.is_empty() {
                                continue;
                            }
                            // An import the graph resolved to another
                            // declaration of the name binds that declaration,
                            // and the name it gives it is no name of this one.
                            let local = if occurrence.context == OccurrenceContext::Import {
                                alias.as_str()
                            } else {
                                name.as_str()
                            };
                            if occurrence.import.is_some_and(|range| {
                                proven.import_binds_another(path, range, local, focal.id)
                            }) {
                                continue;
                            }
                            if matches!(
                                (occurrence.context, occurrence.binding),
                                (
                                    OccurrenceContext::Alias,
                                    OccurrenceBinding::Module
                                        | OccurrenceBinding::Class
                                        | OccurrenceBinding::Local
                                        | OccurrenceBinding::Import
                                        | OccurrenceBinding::Free
                                )
                            ) || occurrence.context == OccurrenceContext::Import
                            {
                                alias_files
                                    .entry(alias.clone())
                                    .or_default()
                                    .insert((*path).to_string());
                            }
                            if alias != &name && !followed.contains(alias) {
                                followed.push(alias.clone());
                                widened = true;
                            }
                        }
                        if widened || occurrence.context != OccurrenceContext::Export {
                            let proven = facts.proven_aliases(path, &name, &occurrence);
                            if !proven.is_empty() {
                                alias_sites.insert(ResidueSite {
                                    detail: Some(proven.join(", ")),
                                    ..site
                                });
                            }
                        }
                    }
                    _ => {
                        values.insert(site);
                    }
                }
            }
        }
    }
    residue.alias_names = followed
        .iter()
        .filter(|name| !base.contains(name))
        .cloned()
        .collect();
    residue.alias_sites = alias_sites.into_iter().collect();
    residue.values = values.into_iter().collect();
    residue.bindings = bindings.into_iter().collect();

    // Reflective sites, implicit calls, protocol dispatch and unparsed
    // regions, over every name followed.
    let protocol = ProtocolMember::from_entity(focal);
    let listed_forms: Vec<ProtocolForm> = match &protocol {
        Some(ProtocolMember::Listed(forms)) => forms.clone(),
        _ => Vec::new(),
    };
    let mut reflective = BTreeSet::new();
    let mut unkeyed = BTreeSet::new();
    let mut implicit = BTreeSet::new();
    let mut dispatch = BTreeSet::new();
    let mut unrecorded: BTreeSet<ResidueSite> = residue.unrecorded.drain(..).collect();
    for (path, record) in &records {
        let own_file = facts.file == Some(*path);
        for site in record
            .get("reflective")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Ok(site) = serde_json::from_value::<kin_model::ReflectiveSite>(site.clone()) else {
                return FocalEscape::Unknown {
                    reason: "a reflective-site record could not be read",
                };
            };
            // A site whose value is called on the spot is judged here too:
            // the call-site reading leaves a call through a computed lookup to
            // the census, which lists it only when it could reach the focal.
            if site.form == ReflectiveForm::DynamicImport {
                continue;
            }
            let declared_holds = |name: &str, line: u32| {
                let Some(owner) = owner else {
                    return true;
                };
                classes
                    .get_or_insert_with(|| ClassTable::build(&records))
                    .as_ref()
                    .is_none_or(|table| table.may_hold_member_of(path, name, line, owner))
            };
            if site_reaches(&site, &facts, own_file, alias_reach, declared_holds)
                && followed.iter().any(|name| site.key_admits(name))
            {
                // A site whose record says something about a followed name is
                // one a reader can check; one that says nothing about any name
                // reaches the focal only as it reaches every declaration its
                // receiver holds, and is counted apart.
                let keyed = followed.iter().any(|name| site.names_key(name));
                let entry = ResidueSite {
                    file: (*path).to_string(),
                    line: site.line + 1,
                    kind: site.form.wire(),
                    name: None,
                    detail: Some(site.excerpt),
                };
                if keyed {
                    reflective.insert(entry);
                } else {
                    unkeyed.insert(entry);
                }
            }
        }
        for site in record
            .get("protocol")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let form = site.get("f").and_then(Value::as_str);
            let spelled = site.get("n").and_then(Value::as_str);
            let names_it =
                spelled.is_some_and(|spelled| followed.iter().any(|name| name == spelled));
            let listed = listed_forms
                .iter()
                .any(|listed| Some(listed.wire()) == form);
            if !names_it && !listed {
                continue;
            }
            let Ok(site) = serde_json::from_value::<kin_model::ProtocolSite>(site.clone()) else {
                return FocalEscape::Unknown {
                    reason: "a protocol-site record could not be read",
                };
            };
            if test_owned_occurrence(&occurrence_owners, path, site.line) {
                continue;
            }
            let entry = ResidueSite {
                file: (*path).to_string(),
                line: site.line + 1,
                kind: site.form.wire(),
                name: site.name.clone(),
                detail: None,
            };
            if names_it {
                implicit.insert(entry.clone());
            }
            if listed {
                dispatch.insert(entry);
            }
        }
        for region in record
            .get("unparsed")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Ok(region) = serde_json::from_value::<kin_model::UnparsedRegion>(region.clone())
            else {
                return FocalEscape::Unknown {
                    reason: "an unparsed-region record could not be read",
                };
            };
            if followed.iter().any(|name| region.names.contains(name)) {
                unrecorded.insert(ResidueSite {
                    file: (*path).to_string(),
                    line: region.first + 1,
                    kind: "unparsed",
                    name: None,
                    detail: Some(format!(
                        "lines {}-{} did not parse and spell a call name",
                        region.first + 1,
                        region.last + 1
                    )),
                });
            }
        }
    }
    residue.reflective = reflective.into_iter().collect();
    residue.unkeyed_lookups = unkeyed.into_iter().collect();
    residue.implicit_calls = implicit.into_iter().collect();
    residue.unrecorded = unrecorded.into_iter().collect();
    residue.mentions = mention_reading(
        graph,
        focal,
        &MentionScope {
            records: &records,
            followed: &followed,
            base: &base,
            alias_files: &alias_files,
            facts: &facts,
            module_scope_closed,
            occurrence_owners: &occurrence_owners,
            roots,
            proven: &proven,
            class_spans: &class_spans,
            owner: owner.filter(|_| hooks_known),
            dynamic: &dynamic,
            classes: match owner {
                Some(_) => classes
                    .get_or_insert_with(|| ClassTable::build(&records))
                    .as_ref(),
                None => None,
            },
        },
        &residue.unrecorded,
        &mut outside,
        &mut outside_values,
        &mut unrelated,
    );
    residue.unrelated_receivers = unrelated
        .into_iter()
        .map(
            |((receiver, class, name), reads)| kin_model::UnrelatedReceiver {
                receiver,
                class,
                name,
                reads,
            },
        )
        .collect();
    residue.outside_values = outside_values
        .into_iter()
        .map(
            |((receiver, owner, name), reads)| kin_model::OutsideValueRead {
                receiver,
                owner,
                name,
                reads,
            },
        )
        .collect();
    residue.outside_receivers = outside
        .into_iter()
        .map(|((receiver, module, name), reads)| OutsideReceiver {
            receiver,
            module,
            name,
            reads,
        })
        .collect();
    residue.protocol = protocol.map(|protocol| match protocol {
        ProtocolMember::Listed(_) => ProtocolResidue {
            dispatch: "listed",
            through: None,
            sites: dispatch.into_iter().collect(),
        },
        ProtocolMember::Implicit(through) => ProtocolResidue {
            dispatch: "implicit",
            through: Some(through),
            sites: Vec::new(),
        },
    });
    if residue.values.is_empty() {
        FocalEscape::Contained {
            entities_checked,
            residue,
        }
    } else {
        FocalEscape::Escapes {
            reason: ESCAPE_NON_CALL_OCCURRENCE,
            residue,
        }
    }
}

/// Why a mention reading was not measured: some file's record predates the
/// mention listing, which `kin upgrade` re-derives.
pub const MENTIONS_UNLISTED: &str = "a source unit of the domain has a value-occurrence record that lists no mentions, because it predates them, so non-call reads there are unmeasured until `kin upgrade` re-derives the repository";

/// Why a mention reading was not measured: some file's record is current
/// and lists no mentions, because its language's reader records none. A
/// re-derivation would record none again, so nothing here says to run one.
pub const MENTIONS_NOT_READ_FOR_LANGUAGE: &str = "a source unit of the domain is written in a language whose reader records no mentions, so non-call reads there are unmeasured";

/// Why a mention reading was not measured: a region of the domain has no
/// record.
pub const MENTIONS_UNRECORDED: &str =
    "a region of the domain has no record (see residue.unrecorded), so non-call reads there are unmeasured";

/// Why a mention reading was not measured: the stored rows into the focal
/// could not be read.
pub const MENTIONS_ROWS_UNREADABLE: &str =
    "the stored rows into the focal could not be read, so no non-call read could be matched";

/// Why a mention reading was not measured: an occurrence record could not be
/// read.
pub const MENTIONS_RECORD_UNREADABLE: &str = "a value-occurrence record could not be read";

/// Why a mention reading was not measured: the focal is a module, which code
/// imports by the path of its file rather than by a name the census follows.
pub const MENTIONS_MODULE_PATH: &str = "the focal is a module, which code imports by the path of its file rather than by a name, and no census of names follows a path, so imports of it are unmeasured here";

/// Why a mention reading cannot speak for `references`: the focal holds
/// members, and a use of one through a value of its type is a reference to it
/// that spells no name the reading follows.
pub const MENTIONS_MEMBER_READS: &str = "the focal holds members, and a use of one of them through a value of its type is recorded as a reference to it only where a language server resolved that use, which spells no name a census of names follows";

/// Whether a focal of `kind` holds members a value of its type can be read
/// through, so a reference to it need not spell its name.
fn holds_members(kind: EntityKind) -> bool {
    matches!(
        kind,
        EntityKind::Class
            | EntityKind::Interface
            | EntityKind::TraitDef
            | EntityKind::TypeAlias
            | EntityKind::EnumDef
            | EntityKind::Schema
    )
}

/// What the mention reading reads beside the graph: the domain's records and
/// what the census already settled about the focal and its names.
struct MentionScope<'a, 'f> {
    records: &'a [(&'a str, &'a Value)],
    followed: &'a [String],
    base: &'a [String],
    alias_files: &'a HashMap<String, BTreeSet<String>>,
    facts: &'a FocalFacts<'f>,
    module_scope_closed: bool,
    occurrence_owners: &'a HashMap<&'a str, &'a [OccurrenceOwner]>,
    roots: &'a ImportRoots,
    proven: &'a ProvenOutside<'a>,
    class_spans: &'a HashMap<&'a str, &'a [ClassSpan]>,
    /// The focal's owner, when it is a member and every file's class
    /// entities speak for its record.
    owner: Option<&'a str>,
    dynamic: &'a HashSet<&'a str>,
    classes: Option<&'a ClassTable>,
}

/// Match every recorded non-call read of a followed name that could read
/// the focal, by the same binding rule the value reading applies
/// ([`could_read_focal`]), against the stored `Calls`, `Imports` and
/// `References` rows into the focal (see [`kin_model::MentionResidue`]).
///
/// Measured only when every record of the domain lists its mentions, no
/// region of it is unrecorded, and the stored rows can be read. A
/// same-named binding is no read and is left out.
fn mention_reading(
    graph: &dyn EscapeCensusGraph,
    focal: &Entity,
    scope: &MentionScope<'_, '_>,
    unrecorded: &[ResidueSite],
    outside: &mut BTreeMap<(String, String, String), usize>,
    outside_values: &mut BTreeMap<(String, String, String), usize>,
    unrelated: &mut BTreeMap<(String, String, String), usize>,
) -> MentionResidue {
    // `import x from './config'` and `export * from './config'` read the
    // module `config` without a single occurrence of its name, so a reading
    // over names would find nothing and call that whole.
    if matches!(
        focal.kind,
        EntityKind::Module | EntityKind::File | EntityKind::Package
    ) {
        return MentionResidue::unmeasured(MENTIONS_MODULE_PATH);
    }
    let mut unlisting = scope
        .records
        .iter()
        .filter(|(_, record)| !record_value_lists_mentions(record))
        .peekable();
    if unlisting.peek().is_some() {
        // A record of the current version that lists none was written by a
        // reader that records none: upgrading cannot add them.
        let current = u64::from(kin_model::value_escape::VALUE_ESCAPE_RECORD_VERSION);
        let predates =
            unlisting.any(|(_, record)| record.get("v").and_then(Value::as_u64) != Some(current));
        return MentionResidue::unmeasured(if predates {
            MENTIONS_UNLISTED
        } else {
            MENTIONS_NOT_READ_FOR_LANGUAGE
        });
    }
    if !unrecorded.is_empty() {
        return MentionResidue::unmeasured(MENTIONS_UNRECORDED);
    }
    let Some(uses) = graph.uses_into(focal.id) else {
        return MentionResidue::unmeasured(MENTIONS_ROWS_UNREADABLE);
    };
    let mut reading = MentionResidue {
        taken: true,
        measured: true,
        references_unmeasured: holds_members(focal.kind).then_some(MENTIONS_MEMBER_READS),
        ..MentionResidue::default()
    };
    let mut unmatched: BTreeSet<ResidueSite> = BTreeSet::new();
    let mut visited: HashSet<&str> = HashSet::new();
    for name in scope.followed {
        if !visited.insert(name.as_str()) {
            continue;
        }
        let is_base = scope.base.contains(name);
        for (path, record) in scope.records {
            let Some(occurrences) = record
                .get("values")
                .and_then(|values| values.get(name))
                .and_then(Value::as_array)
            else {
                continue;
            };
            for occurrence in occurrences {
                let Ok(occurrence) =
                    serde_json::from_value::<kin_model::ValueOccurrence>(occurrence.clone())
                else {
                    return MentionResidue::unmeasured(MENTIONS_RECORD_UNREADABLE);
                };
                if occurrence.context == OccurrenceContext::Binding {
                    continue;
                }
                if test_owned_occurrence(scope.occurrence_owners, path, occurrence.line) {
                    continue;
                }
                if let Some((receiver, owner)) = scope.proven.of(path, name, &occurrence) {
                    // As below: the value reading counted a read whose value
                    // flows on; only a mention is new here.
                    if occurrence.context == OccurrenceContext::Mention {
                        *outside_values
                            .entry((receiver, owner, name.clone()))
                            .or_insert(0) += 1;
                    }
                    continue;
                }
                if let (Some(owner), Some(table), Some(class)) = (
                    scope.owner,
                    scope.classes,
                    instance_receiver_class(&occurrence, scope.class_spans.get(path)),
                ) {
                    if table.shares_no_instance(&class.name, owner, scope.dynamic) {
                        // As above: only a mention is new here.
                        if occurrence.context == OccurrenceContext::Mention {
                            let receiver = occurrence.receiver.clone().unwrap_or_default();
                            *unrelated
                                .entry((receiver, class.name.clone(), name.clone()))
                                .or_insert(0) += 1;
                        }
                        continue;
                    }
                }
                if let (Some(owner), Some(table), Some(class)) = (
                    scope.owner,
                    scope.classes,
                    scope.proven.typed_class(path, name, &occurrence),
                ) {
                    if table.shares_no_instance(&class, owner, scope.dynamic) {
                        if occurrence.context == OccurrenceContext::Mention {
                            let receiver = occurrence.receiver.clone().unwrap_or_default();
                            *unrelated
                                .entry((receiver, class, name.clone()))
                                .or_insert(0) += 1;
                        }
                        continue;
                    }
                }
                if !scope.facts.member {
                    if let Some((receiver, module)) =
                        outside_imports::outside_receiver(scope.roots, path, record, &occurrence)
                    {
                        // The value reading counted a read whose value flows
                        // on; only a mention is new here.
                        if occurrence.context == OccurrenceContext::Mention {
                            *outside.entry((receiver, module, name.clone())).or_insert(0) += 1;
                        }
                        continue;
                    }
                }
                let aliased_here = scope
                    .alias_files
                    .get(name)
                    .is_some_and(|files| files.contains(*path));
                if !could_read_focal(
                    scope.facts,
                    path,
                    name,
                    is_base,
                    aliased_here,
                    &occurrence,
                    scope.base,
                    scope.module_scope_closed,
                ) {
                    continue;
                }
                let covering: Vec<RelationKind> = uses
                    .iter()
                    .filter(|stored| stored.covers(path, occurrence.line))
                    .map(|stored| stored.kind)
                    .collect();
                let natural = match (occurrence.context, occurrence.form) {
                    (_, Some(MentionForm::Import)) | (OccurrenceContext::Import, _) => {
                        RelationKind::Imports
                    }
                    (_, Some(MentionForm::Decorator)) => RelationKind::Calls,
                    _ => RelationKind::References,
                };
                let credited = if covering.contains(&natural) {
                    Some(natural)
                } else {
                    [
                        RelationKind::References,
                        RelationKind::Imports,
                        RelationKind::Calls,
                    ]
                    .into_iter()
                    .find(|kind| covering.contains(kind))
                };
                match credited {
                    Some(kind) => *reading.matched.entry(use_kind_wire(kind)).or_insert(0) += 1,
                    None => {
                        unmatched.insert(ResidueSite {
                            file: (*path).to_string(),
                            line: occurrence.line + 1,
                            kind: occurrence.residue_kind(),
                            name: Some(name.clone()),
                            detail: occurrence.receiver.clone(),
                        });
                    }
                }
            }
        }
    }
    reading.unmatched = unmatched.into_iter().collect();
    reading
}

/// Whether a recorded use of `name` in `path` could read the focal, by its
/// binding.
#[allow(clippy::too_many_arguments)]
fn could_read_focal(
    facts: &FocalFacts<'_>,
    path: &str,
    name: &str,
    is_base: bool,
    aliased_here: bool,
    occurrence: &kin_model::ValueOccurrence,
    base: &[String],
    module_scope_closed: bool,
) -> bool {
    let imported_original = occurrence
        .imported
        .clone()
        .unwrap_or_else(|| name.to_string());
    let import_could_land = |range: Option<(u32, u32)>| match range {
        Some(range) if facts.imported_by(path, range) => true,
        // An import that lands elsewhere, or nowhere the linker could
        // resolve, still reads the focal if it names it: only a default or
        // namespace import needs its edge.
        // A declaration its module does not export cannot be imported.
        _ if facts.module_private && module_scope_closed => false,
        _ => {
            !facts.member
                && !matches!(imported_original.as_str(), "default" | "*")
                && (base.contains(&imported_original) || !is_base)
        }
    };
    match occurrence.binding {
        OccurrenceBinding::Member => {
            if facts.member {
                return true;
            }
            if occurrence.receiver_kind == Some(ReceiverKind::Instance) {
                return false;
            }
            // A declaration its module does not export is no member of any
            // object unless a value use put it there.
            !(facts.module_private && module_scope_closed && facts.file != Some(path))
        }
        OccurrenceBinding::Local | OccurrenceBinding::Class => {
            aliased_here || facts.declared_here(path, occurrence.target_line)
        }
        OccurrenceBinding::Module => {
            if aliased_here {
                return true;
            }
            if occurrence.context == OccurrenceContext::Import
                || occurrence.module.is_some() && occurrence.import.is_some()
            {
                if import_could_land(occurrence.import) {
                    return true;
                }
            }
            !facts.member && facts.declared_here(path, occurrence.target_line)
        }
        OccurrenceBinding::Import => aliased_here || import_could_land(occurrence.import),
        OccurrenceBinding::Free => aliased_here || !facts.member && !facts.module_private,
    }
}

/// Read parser-produced binding evidence from the exact selected tree. This
/// never reparses source or trusts a Constant label or a signature heuristic.
fn non_callable_binding(graph: &dyn EscapeCensusGraph, focal: &Entity) -> Option<FocalEscape> {
    let binding = focal.metadata.extra.get("scalar_binding_v1")?;
    if binding.get("version")?.as_u64()? != 1
        || binding.get("language")?.as_str()? != focal.language.to_string()
    {
        return None;
    }
    // The adapter writes `immutable` only for a binding its language forbids
    // rebinding (a Rust or Go const, a JavaScript const, a Java final), and
    // counts writes only where it can see every one. A mutable binding is
    // therefore settled below only where every calling file carries that
    // count, whatever the language.
    let immutable = binding.get("immutable")?.as_bool()?;
    let name = binding.get("name")?.as_str()?;
    if name != focal.name.rsplit(['.', ':']).next()? {
        return None;
    }
    let file = focal.file_origin.as_ref()?;
    let files = graph.census_files(&calling_languages(focal.language))?;
    let selected = files.iter().find(|selected| &selected.path == file)?;
    if blob_of(focal) != Some(selected.blob) {
        return None;
    }
    let held = graph.entities_in(file)?;
    if !held.iter().any(|entity| {
        entity.id == focal.id
            && entity.metadata.extra.get("scalar_binding_v1") == Some(binding)
            && blob_of(entity) == Some(selected.blob)
    }) {
        return None;
    }
    if immutable {
        return Some(FocalEscape::NonCallable {
            reason: "the parser records an immutable scalar literal binding",
            entities_checked: 1,
        });
    }
    let mut writes = 0u64;
    let mut entities_checked = 0usize;
    for file in files {
        let entities = graph.entities_in(&file.path)?;
        entities_checked = entities_checked.checked_add(entities.len())?;
        let evidence = entities.iter().find_map(|entity| {
            (blob_of(entity) == Some(file.blob))
                .then(|| entity.metadata.extra.get("python_binding_census_v1"))
                .flatten()
        })?;
        if evidence.get("version")?.as_u64()? != 1 || evidence.get("blocked")?.as_bool()? {
            return None;
        }
        let counts = evidence.get("writes")?.as_object()?;
        writes = writes.checked_add(match counts.get(name) {
            Some(value) => value.as_u64()?,
            None => 0,
        })?;
        if writes > 1 {
            return None;
        }
    }
    (writes == 1).then_some(FocalEscape::NonCallable {
        reason: "one scalar assignment and no rebinding or reflective writes in the parsed domain",
        entities_checked,
    })
}

#[cfg(test)]
mod tests;
