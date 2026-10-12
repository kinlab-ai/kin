// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use tracing::debug;

use kin_blobs::BlobStore;
use kin_model::{
    Entity, EntityRole, FileLayout, FilePathId, GraphStore, Hash256, LanguageId, OpaqueArtifact,
    ParseCompleteness, ParseState, Relation, RelationOrigin, StructuredArtifact,
};
use kin_parser::project_manifest::{NoProjectFiles, ProjectFiles, UnitContext};
use kin_parser::{
    attach_file_context_metadata, attach_file_nested_definition_record,
    attach_file_reference_parse_counts, parse_shallow_file, AdapterRegistry, ShallowFile,
};
use kin_projection::build_layout;

use tree_sitter::Tree;

use crate::artifacts;
use crate::classifier::{FileClassification, FileClassifier};
use crate::error::{IndexError, Result};
use crate::fingerprint::{
    behavior_equivalence_hash_in_file, language_supports_equivalence, FileEquivalenceContext,
};
use crate::linker::UnresolvedRelation;

/// Every declaration a unit holds keeps an identity of its own before anything
/// reads the identities: the graph holds entities by identity, so two that
/// share one would leave the graph holding fewer declarations than the parse
/// of the same bytes, and every reopen would refuse the unit's layout.
fn separate_unit_identities(entities: &mut [Entity], file_id: &FilePathId) {
    let separated = kin_parser::separate_shared_identities(entities, file_id);
    if separated > 0 {
        debug!(
            path = %file_id,
            separated,
            "declarations in this unit derived a shared identity; each later one took the \
             identity of its occurrence"
        );
    }
}

/// A complete C body as the partial-refresh proofs read it: its syntax tree,
/// and, once some proof asks, every declaration extracted from it.
struct PartialCSource {
    tree: Tree,
    /// Extracted on first use, since a proof whose function contexts already
    /// disagree never needs the declarations. `None` when extraction failed.
    entities: OnceLock<Option<Vec<Entity>>>,
}

impl PartialCSource {
    /// Every declaration in the body, each built with its own source bytes.
    ///
    /// `source` and `file` are the inputs the body is remembered under, so an
    /// extraction here is the one a fresh parse of them would produce.
    fn entities(&self, source: &[u8], file: &FilePathId) -> Option<&[Entity]> {
        use kin_parser::adapter::LanguageAdapter;
        self.entities
            .get_or_init(|| {
                let parsed = kin_parser::languages::CAdapter
                    .extract(&self.tree, source, file)
                    .ok()?;
                Some(
                    parsed
                        .entities
                        .into_iter()
                        .map(|entity| {
                            entity.into_entity_with_source(LanguageId::C, file, Some(source))
                        })
                        .collect(),
                )
            })
            .as_deref()
    }
}

/// How many C bodies the partial-refresh proofs remember.
///
/// A proof compares one earlier body with one current body, so two per file
/// being refreshed is the working set. A few files' worth covers a commit that
/// checks several partially parsed files without holding many syntax trees.
const REMEMBERED_PARTIAL_C_SOURCES: usize = 8;

type RememberedPartialCSource = (FilePathId, kin_blobs::Hash256, Option<Arc<PartialCSource>>);

/// C bodies the partial-refresh proofs parsed recently, keyed by the file they
/// were extracted as and the digest of their exact bytes, oldest first.
///
/// A proof over one incomplete file checks every held declaration, and every
/// relation of each, against the same two bodies. Each of those checks used to
/// parse and extract both whole bodies again, so a commit over one partially
/// parsed C file spent most of its time re-parsing it. Parsing and extraction
/// are pure functions of the bytes and the file, so a remembered body is the
/// one a fresh parse would produce, and the proofs answer exactly as before.
static PARTIAL_C_SOURCES: Mutex<VecDeque<RememberedPartialCSource>> = Mutex::new(VecDeque::new());

#[cfg(test)]
thread_local! {
    /// Parse every body afresh on this thread, so a test can hold the
    /// remembered answers against uncached ones.
    static FORGET_PARTIAL_C_SOURCES: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// The parsed C body `source`, as extracted for `file`, or `None` when it does
/// not parse. Remembered per exact bytes and file.
fn partial_c_source(file: &FilePathId, source: &[u8]) -> Option<Arc<PartialCSource>> {
    #[cfg(test)]
    if FORGET_PARTIAL_C_SOURCES.with(std::cell::Cell::get) {
        return parse_partial_c_source(source).map(Arc::new);
    }
    let digest = kin_blobs::digest(source);
    let remembered = |sources: &VecDeque<RememberedPartialCSource>| {
        sources
            .iter()
            .find(|(held, hash, _)| hash == &digest && held == file)
            .map(|(_, _, parsed)| parsed.clone())
    };
    if let Some(parsed) = remembered(
        &PARTIAL_C_SOURCES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    ) {
        return parsed;
    }
    // Parsed outside the lock, so one slow body does not hold up the others.
    let parsed = parse_partial_c_source(source).map(Arc::new);
    let mut sources = PARTIAL_C_SOURCES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(raced) = remembered(&sources) {
        return raced;
    }
    if sources.len() >= REMEMBERED_PARTIAL_C_SOURCES {
        sources.pop_front();
    }
    sources.push_back((file.clone(), digest, parsed.clone()));
    parsed
}

fn parse_partial_c_source(source: &[u8]) -> Option<PartialCSource> {
    use kin_parser::adapter::LanguageAdapter;
    let tree = kin_parser::languages::CAdapter.parse(source).ok()?;
    Some(PartialCSource {
        tree,
        entities: OnceLock::new(),
    })
}

/// Result of indexing a single file.
#[derive(Debug, Clone)]
pub struct IndexedFile {
    pub file_id: FilePathId,
    pub language: LanguageId,
    pub entities: Vec<Entity>,
    pub relations: Vec<Relation>,
    /// Cross-file relations that couldn't be fully resolved.
    /// These are collected for later linking by CrossFileLinker.
    pub unresolved_relations: Vec<UnresolvedRelation>,
    pub file_layout: FileLayout,
    pub parse_state: ParseState,
    pub blob_hash: kin_blobs::Hash256,
    /// Raw extracted relations preserved for cross-file linking.
    pub extracted_relations: Vec<kin_parser::ExtractedRelation>,
    /// Import declarations preserved for cross-file linking.
    pub imports: Vec<kin_parser::FileImport>,
}

impl IndexedFile {
    /// Give the parsed declaration `parsed` the graph identity `retained`
    /// everywhere this file names it: the entity, the relations it is an
    /// endpoint of, the unresolved relations it sources and its layout region.
    ///
    /// A semantic rename keeps the renamed declaration's identity although the
    /// parser mints a new id from the new name. Every fresh call edge carries
    /// occurrence certificates bound to its endpoints, so an edge moves through
    /// [`crate::occurrence::rebind_identity`] together with its certificates.
    /// Writing only the endpoint fields left each certificate naming the
    /// parser's id, and admission refuses that edge as invalid occurrence
    /// evidence. The relation id stays as parsed, since admission chooses the
    /// stable relation identity.
    ///
    /// The caller chooses which declaration keeps which identity and proves
    /// that `retained` names no other parsed entity. Invalid occurrence evidence
    /// refuses before any field changes.
    pub fn retain_entity_identity(
        &mut self,
        parsed: kin_model::EntityId,
        retained: kin_model::EntityId,
    ) -> std::result::Result<(), String> {
        let retain = |node: kin_model::GraphNodeId| {
            if node == kin_model::GraphNodeId::Entity(parsed) {
                kin_model::GraphNodeId::Entity(retained)
            } else {
                node
            }
        };
        let mut relations = self.relations.clone();
        for relation in &mut relations {
            let (src, dst) = (retain(relation.src), retain(relation.dst));
            if (src, dst) != (relation.src, relation.dst) {
                let id = relation.id;
                crate::occurrence::rebind_identity(relation, id, src, dst)?;
            }
        }
        self.relations = relations;
        for entity in &mut self.entities {
            if entity.id == parsed {
                entity.id = retained;
            }
        }
        for relation in &mut self.unresolved_relations {
            if relation.src_entity_id == parsed {
                relation.src_entity_id = retained;
            }
        }
        for region in &mut self.file_layout.regions {
            if let kin_model::SourceRegion::EntityRef { entity_id, .. } = region {
                if *entity_id == parsed {
                    *entity_id = retained;
                }
            }
        }
        Ok(())
    }
}

/// Indexed file plus parser-emitted tests.
///
/// This keeps test metadata available to ingestion callers without widening the
/// existing `IndexedFile` struct yet.
#[derive(Debug)]
pub struct IndexedFileWithTests {
    pub indexed_file: IndexedFile,
    pub tests: Vec<kin_parser::ExtractedTest>,
}

/// Result of indexing any file through the classifier.
#[derive(Debug)]
pub enum IndexedAny {
    /// File was parsed for entities (source code).
    EntitySource(IndexedFile),
    /// File was recognized as a structured artifact.
    StructuredArtifact(StructuredArtifact),
    /// File was parsed at C2 shallow syntax tier.
    ShallowSyntax(ShallowFile),
    /// File was tracked as an opaque blob.
    OpaqueArtifact(OpaqueArtifact),
}

/// The indexing pipeline: parses files, stores blobs, and updates the graph.
pub struct IndexPipeline {
    registry: AdapterRegistry,
}

impl IndexPipeline {
    /// Verify a conservative refresh against both complete, digest-bound C
    /// inputs. This proves only one existing declaration, never file coverage.
    pub fn supports_partial_refresh(
        &self,
        old: &Entity,
        new: &Entity,
        old_source: &[u8],
        source: &[u8],
    ) -> bool {
        if old.language != LanguageId::C
            || new.language != LanguageId::C
            || old.kind != kin_model::EntityKind::Function
            || new.kind != old.kind
            || old.name != new.name
            || old.signature != new.signature
            || old.file_origin != new.file_origin
        {
            return false;
        }
        let (Some(was), Some(now)) = (&old.span, &new.span) else {
            return false;
        };
        if was.file != now.file {
            return false;
        }
        let digest_matches = |entity: &Entity, bytes: &[u8]| {
            entity
                .metadata
                .extra
                .get("blob_hash")
                .and_then(|v| v.as_str())
                == Some(kin_blobs::digest(bytes).to_string().as_str())
        };
        if !digest_matches(old, old_source) || !digest_matches(new, source) {
            return false;
        }
        let (Some(before), Some(after)) = (
            partial_c_source(&was.file, old_source),
            partial_c_source(&was.file, source),
        ) else {
            return false;
        };
        let before_context = kin_parser::languages::c_lang::partial_function_context(
            &before.tree,
            old_source,
            was.start_byte,
            was.end_byte,
        );
        let after_context = kin_parser::languages::c_lang::partial_function_context(
            &after.tree,
            source,
            now.start_byte,
            now.end_byte,
        );
        if before_context.is_none() || before_context != after_context {
            return false;
        }
        // Old graph identity must actually describe its claimed source, rather
        // than a stale span coincidentally landing on another declaration.
        for (parsed, bytes, entity) in [(&before, old_source, old), (&after, source, new)] {
            let Some(entities) = parsed.entities(bytes, &was.file) else {
                return false;
            };
            let matches: Vec<_> = entities
                .iter()
                .filter(|e| e.name == entity.name && e.kind == entity.kind)
                .collect();
            if matches.len() != 1 {
                return false;
            }
            let verified = matches[0];
            if verified.span != entity.span
                || verified.signature != entity.signature
                || verified.fingerprint.behavior_hash != entity.fingerprint.behavior_hash
            {
                return false;
            }
        }
        true
    }

    /// Move only uniquely identified direct-call evidence through a proven
    /// partial declaration refresh. Resolution must name one stable, complete
    /// function in this same file; all other located evidence refuses admission.
    pub fn rebase_partial_relation(
        &self,
        old: &Entity,
        new: &Entity,
        old_source: &[u8],
        source: &[u8],
        relation: &Relation,
        file_entities: &[Entity],
    ) -> Option<Relation> {
        self.partial_relation_evidence(old, new, old_source, source, relation, file_entities, false)
    }

    /// Verify the coordinates persisted for an already admitted declaration.
    pub fn verifies_partial_relation(
        &self,
        old: &Entity,
        new: &Entity,
        old_source: &[u8],
        source: &[u8],
        relation: &Relation,
        file_entities: &[Entity],
    ) -> bool {
        self.partial_relation_evidence(old, new, old_source, source, relation, file_entities, true)
            .is_some_and(|expected| expected == *relation)
    }

    fn partial_relation_evidence(
        &self,
        old: &Entity,
        new: &Entity,
        old_source: &[u8],
        source: &[u8],
        relation: &Relation,
        file_entities: &[Entity],
        current: bool,
    ) -> Option<Relation> {
        let was = old.span.as_ref()?;
        let now = new.span.as_ref()?;
        if !relation
            .evidence
            .iter()
            .any(|e| e.source_span.as_ref().is_some_and(|s| s.file == was.file))
        {
            return Some(relation.clone());
        }
        if relation.src.as_entity() != Some(old.id)
            || relation.kind != kin_model::RelationKind::Calls
            || !self.supports_partial_refresh(old, new, old_source, source)
        {
            return None;
        }
        let target_id = relation.dst.as_entity()?;
        let target = file_entities.iter().find(|entity| entity.id == target_id)?;
        if target.kind != kin_model::EntityKind::Function || target.span.as_ref()?.file != was.file
        {
            return None;
        }
        let before = partial_c_source(&was.file, old_source)?;
        let after = partial_c_source(&was.file, source)?;
        let old_sites =
            kin_parser::languages::c_lang::partial_call_sites(&before.tree, old_source, was)?;
        let new_sites =
            kin_parser::languages::c_lang::partial_call_sites(&after.tree, source, now)?;
        let before_entities = before.entities(old_source, &was.file)?;
        let after_entities = after.entities(source, &was.file)?;
        for entities in [file_entities, before_entities, after_entities] {
            let matched: Vec<_> = entities
                .iter()
                .filter(|entity| entity.name == target.name && entity.kind == target.kind)
                .collect();
            if matched.len() != 1 || matched[0].signature != target.signature {
                return None;
            }
        }
        for (tree, bytes, entities) in [
            (&before.tree, old_source, before_entities),
            (&after.tree, source, after_entities),
        ] {
            let declaration = entities
                .iter()
                .find(|entity| entity.name == target.name && entity.kind == target.kind)?
                .span
                .as_ref()?;
            kin_parser::languages::c_lang::partial_function_context(
                tree,
                bytes,
                declaration.start_byte,
                declaration.end_byte,
            )?;
        }
        let span_matches =
            |observed: &kin_model::SourceSpan, syntax: &kin_model::SourceSpan, bytes: &[u8]| {
                if observed.start_byte != 0 || observed.end_byte != 0 {
                    return observed == syntax;
                }
                let coordinates_match = observed.file == syntax.file
                    && observed.start_line == syntax.start_line
                    && observed.end_line == syntax.end_line
                    && observed.start_col == syntax.start_col
                    && observed.end_col == syntax.end_col;
                // Zero-byte LSP positions can use UTF-16 columns. ASCII prefixes
                // make that representation identical to parser byte columns.
                coordinates_match
                    && [
                        (syntax.start_line, syntax.start_col),
                        (syntax.end_line, syntax.end_col),
                    ]
                    .iter()
                    .all(|(line, column)| {
                        bytes
                            .split(|byte| *byte == b'\n')
                            .nth(*line as usize)
                            .and_then(|line| line.get(..*column as usize))
                            .is_some_and(|prefix| prefix.is_ascii())
                    })
            };
        let mut updated = relation.clone();
        for evidence in &mut updated.evidence {
            let Some(span) = evidence.source_span.as_ref() else {
                continue;
            };
            if span.file != was.file {
                continue;
            }
            let (sites, bytes) = if current {
                (&new_sites, source)
            } else {
                (&old_sites, old_source)
            };
            let matched: Vec<_> = sites
                .iter()
                .filter_map(|site| {
                    if span_matches(span, &site.call_span, bytes) {
                        Some((site, true))
                    } else if span_matches(span, &site.callee_span, bytes) {
                        Some((site, false))
                    } else {
                        None
                    }
                })
                .collect();
            if matched.len() != 1 {
                return None;
            }
            let (site, whole_call) = matched[0];
            if site.callee != target.name {
                return None;
            }
            let same_call = |candidate: &&kin_parser::languages::c_lang::PartialCallSite| {
                candidate.callee == site.callee && candidate.expression == site.expression
            };
            if old_sites.iter().filter(same_call).count() != 1
                || new_sites.iter().filter(same_call).count() != 1
            {
                return None;
            }
            let placed = new_sites.iter().find(|candidate| {
                candidate.callee == site.callee && candidate.expression == site.expression
            })?;
            evidence.source_span = Some(if whole_call {
                placed.call_span.clone()
            } else {
                placed.callee_span.clone()
            });
        }
        crate::occurrence::rebind_verified_spans(relation, &mut updated).ok()?;
        Some(updated)
    }

    pub fn new() -> Self {
        Self {
            registry: AdapterRegistry::new(),
        }
    }

    /// Index a single file: parse it, store the blob, and return extracted entities/relations.
    pub fn index_file(&self, path: &Path, blob_store: &BlobStore) -> Result<IndexedFile> {
        let _span = tracing::info_span!(
            "kin.index.index_file",
            path = %path.display()
        )
        .entered();
        let source =
            std::fs::read(path).map_err(|e| IndexError::io(path.display().to_string(), e))?;
        let blob_hash = blob_store.write(&source)?;
        debug!(path = %path.display(), hash = %blob_hash, "stored source blob");
        let file_id = FilePathId::new(path.display().to_string());
        Ok(self
            .index_file_content_with_tests(&file_id, &source, blob_hash)?
            .indexed_file)
    }

    /// Index a single file and retain parser-emitted tests alongside the file result.
    pub fn index_file_with_tests(
        &self,
        path: &Path,
        blob_store: &BlobStore,
    ) -> Result<IndexedFileWithTests> {
        let _span = tracing::info_span!(
            "kin.index.index_file_with_tests",
            path = %path.display()
        )
        .entered();
        let source =
            std::fs::read(path).map_err(|e| IndexError::io(path.display().to_string(), e))?;
        let blob_hash = blob_store.write(&source)?;
        debug!(path = %path.display(), hash = %blob_hash, "stored source blob");
        let file_id = FilePathId::new(path.display().to_string());
        self.index_file_content_with_tests(&file_id, &source, blob_hash)
    }

    /// Index source content already loaded from blob storage, in a tree whose
    /// project manifests are not known, so every unit is named from its path.
    ///
    /// A caller indexing a unit of a tree it holds passes that tree's
    /// manifests to [`IndexPipeline::index_file_content_in`] instead: a Rust
    /// crate root takes its crate's name from them.
    pub fn index_file_content_with_tests(
        &self,
        file_id: &FilePathId,
        source: &[u8],
        blob_hash: kin_blobs::Hash256,
    ) -> Result<IndexedFileWithTests> {
        self.index_file_content_in(file_id, source, blob_hash, &NoProjectFiles)
    }

    /// What the unit at `file_id` takes from the manifests `project` holds,
    /// read by the adapter its extension names. A unit no adapter parses takes
    /// nothing. Read without the unit's bytes, so a caller can ask whether a
    /// parse it holds is still the one its project would give: the languages
    /// that share an extension and are told apart by content, C and C++ in a
    /// `.h`, take nothing from a project. A manifest the unit's name depends
    /// on that the tree holds and `project` cannot read refuses it.
    pub fn unit_context(
        &self,
        file_id: &FilePathId,
        project: &dyn ProjectFiles,
    ) -> Result<UnitContext> {
        match Path::new(&file_id.0)
            .extension()
            .and_then(|extension| extension.to_str())
            .and_then(|extension| self.registry.get_by_extension(extension))
        {
            Some(adapter) => Ok(adapter.unit_context(file_id, project)?),
            None => Ok(UnitContext::default()),
        }
    }

    /// Index source content already loaded from blob storage, as a unit of
    /// the tree whose project manifests `project` holds.
    ///
    /// This is used by historical/ref-scoped views that need to rebuild an
    /// entity graph from stored blobs without materializing files to disk.
    pub fn index_file_content_in(
        &self,
        file_id: &FilePathId,
        source: &[u8],
        blob_hash: kin_blobs::Hash256,
        project: &dyn ProjectFiles,
    ) -> Result<IndexedFileWithTests> {
        let unit = self.unit_context(file_id, project)?;
        self.index_file_content_with_unit(file_id, source, blob_hash, &unit)
    }

    /// Index source content as a unit whose project declared `unit` about it
    /// ([`IndexPipeline::unit_context`]).
    pub fn index_file_content_with_unit(
        &self,
        file_id: &FilePathId,
        source: &[u8],
        blob_hash: kin_blobs::Hash256,
        unit: &UnitContext,
    ) -> Result<IndexedFileWithTests> {
        let actual = kin_blobs::digest(source);
        if actual != blob_hash {
            return Err(IndexError::SourceDigestMismatch {
                path: file_id.0.clone(),
                expected: blob_hash,
                actual,
            });
        }
        #[cfg(any(test, feature = "test-support"))]
        crate::shared_index::count_parse(&file_id.0);
        let path = Path::new(&file_id.0);
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .ok_or_else(|| IndexError::UnsupportedFile(file_id.0.clone()))?;

        let adapter = self
            .registry
            .get_by_extension_and_content(ext, source)
            .ok_or_else(|| IndexError::UnsupportedFile(ext.to_string()))?;
        let language = adapter.language_id();

        let tree = adapter.parse(source)?;
        let mut output = adapter.extract_in(&tree, source, file_id, unit)?;
        kin_parser::file_evidence::attach_file_evidence(adapter, &tree, source, &mut output);
        let kin_parser::ParseOutput {
            entities: extracted_entities,
            derived_members,
            relations: extracted_relations,
            imports,
            tests,
            parse_state,
            parsed_call_sites,
            nested_definitions,
        } = output;

        let role = classify_file_role(&file_id.0);
        // A test declared inside a production file is invisible to a classifier
        // that only reads the path, and the harness that runs it leaves no edge
        // for the graph to record, so it reads as unreferenced to everything
        // downstream. The parser already recognized these by their attribute or
        // framework convention, so that finding becomes the entity's role.
        // Matching is by name within the file: a production declaration sharing
        // a name with a test in the same file is read as the test.
        let test_names: std::collections::HashSet<&str> =
            tests.iter().map(|test| test.name.as_str()).collect();
        let mut entities: Vec<Entity> = extracted_entities
            .into_iter()
            .map(|entity| {
                let mut ent = entity.into_entity_with_source(language, file_id, Some(source));
                ent.role = if role == EntityRole::Source && test_names.contains(ent.name.as_str()) {
                    EntityRole::Test
                } else {
                    role
                };
                ent
            })
            .collect();
        materialize_derived_members(
            &mut entities,
            derived_members,
            language,
            file_id,
            role,
            blob_hash,
        );
        // How the file uses names as values, for the escape census. A file
        // whose record holds something always carries an entity for it.
        let escape_census = kin_parser::value_escape::census_file(&tree, source, language);
        if entities.is_empty()
            && escape_census
                .as_ref()
                .is_some_and(|census| !census.record.is_empty())
        {
            if let Some(surface) =
                kin_parser::value_escape::module_surface(language, file_id, &tree, source)
            {
                let mut surface = surface.into_entity_with_source(language, file_id, Some(source));
                surface.role = role;
                entities.push(surface);
            }
        }
        separate_unit_identities(&mut entities, file_id);
        attach_span_source_digest(&mut entities, blob_hash);
        attach_own_code(&mut entities, source);
        attach_file_context_metadata(&mut entities, file_id, &imports);
        attach_file_reference_parse_counts(
            &mut entities,
            &extracted_relations,
            &imports,
            parsed_call_sites,
        );
        attach_file_nested_definition_record(&mut entities, nested_definitions.as_ref());
        attach_equivalence_class(&mut entities, &tree, source, language);
        kin_parser::binding_callability::attach_binding_callability(
            &tree,
            source,
            language,
            &mut entities,
        );
        kin_parser::value_origins::attach_value_origins(
            &tree,
            source,
            language,
            &file_id.0,
            &mut entities,
        );
        kin_parser::value_escape::attach(escape_census.as_ref(), &mut entities);
        if language == LanguageId::Go {
            kin_parser::attach_go_package_metadata(&tree, source, &mut entities);
            kin_parser::attach_go_command_effect_contract_metadata(&tree, source, &mut entities);
        }

        let parse_completeness = ParseCompleteness::from_parse_state(&parse_state);
        let (relations, unresolved_relations) = resolve_relations(
            &extracted_relations,
            &entities,
            file_id,
            &parse_completeness,
        );
        let file_layout = build_layout(file_id, &entities, source.len(), &[], parse_completeness);

        debug!(
            path = %file_id,
            entities = entities.len(),
            resolved_relations = relations.len(),
            unresolved_relations = unresolved_relations.len(),
            "indexed source content"
        );

        Ok(IndexedFileWithTests {
            indexed_file: IndexedFile {
                file_id: file_id.clone(),
                language,
                entities,
                relations,
                unresolved_relations,
                file_layout,
                extracted_relations,
                imports,
                parse_state,
                blob_hash,
            },
            tests,
        })
    }

    /// Index a single file with an optional incremental parse hint.
    ///
    /// When `old_tree` and `edit_hint` are both provided, uses tree-sitter's
    /// incremental parse for speed (<5ms vs 50-100ms on large files).
    /// Falls back to a full parse when hints are missing.
    ///
    /// Returns the indexed file together with the resulting tree-sitter Tree,
    /// which the caller should cache for future incremental parses.
    pub fn index_file_with_hint(
        &self,
        path: &Path,
        blob_store: &BlobStore,
        old_tree: Option<&tree_sitter::Tree>,
        edit_hint: Option<&kin_parser::EditHint>,
    ) -> Result<(IndexedFile, tree_sitter::Tree)> {
        let _span = tracing::info_span!(
            "kin.index.index_file_with_hint",
            path = %path.display(),
            incremental = old_tree.is_some() && edit_hint.is_some()
        )
        .entered();
        let file_id = FilePathId::new(path.display().to_string());
        let (indexed, tree) = self.index_file_hint_with_id(
            path,
            blob_store,
            &file_id,
            old_tree,
            edit_hint,
            &NoProjectFiles,
        )?;
        Ok((indexed.indexed_file, tree))
    }

    /// Index a file for incremental reconcile using an explicit logical
    /// `file_id` for all content-addressed identity, while reading source from
    /// `path` on disk.
    ///
    /// This is the single source of truth for the incremental parse/extract
    /// path. Because `EntityId` and `RelationId` are derived from the supplied
    /// `file_id`, absolute-path and normalized (relative) callers produce
    /// identical entity and relation IDs for identical content — the graph-first
    /// content-addressed contract that keeps init and reconcile in agreement.
    fn index_file_hint_with_id(
        &self,
        path: &Path,
        blob_store: &BlobStore,
        file_id: &FilePathId,
        old_tree: Option<&tree_sitter::Tree>,
        edit_hint: Option<&kin_parser::EditHint>,
        project: &dyn ProjectFiles,
    ) -> Result<(IndexedFileWithTests, tree_sitter::Tree)> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .ok_or_else(|| IndexError::UnsupportedFile(path.display().to_string()))?;

        let source =
            std::fs::read(path).map_err(|e| IndexError::io(path.display().to_string(), e))?;

        let adapter = self
            .registry
            .get_by_extension_and_content(ext, &source)
            .ok_or_else(|| IndexError::UnsupportedFile(ext.to_string()))?;

        // Store the raw source as a blob
        let blob_hash = blob_store.write(&source)?;
        debug!(path = %path.display(), hash = %blob_hash, "stored source blob");

        let language = adapter.language_id();

        // Parse: incremental when hints are available, full otherwise. An
        // edit can flip a content-disambiguated extension to another
        // adapter, and an old tree from the other grammar must never seed an
        // incremental parse — those files always take the full parse.
        let content_disambiguated = ext == "h";
        let tree = match (old_tree, edit_hint) {
            (Some(old), Some(hint)) if !content_disambiguated => {
                debug!(
                    path = %path.display(),
                    start = hint.start_byte,
                    old_end = hint.old_end_byte,
                    new_end = hint.new_end_byte,
                    "incremental parse"
                );
                adapter.parse_incremental(&source, old, hint)?
            }
            _ => adapter.parse(&source)?,
        };
        let unit = self.unit_context(file_id, project)?;
        let mut output = adapter.extract_in(&tree, &source, file_id, &unit)?;
        kin_parser::file_evidence::attach_file_evidence(adapter, &tree, &source, &mut output);
        let kin_parser::ParseOutput {
            entities: extracted_entities,
            derived_members,
            relations: extracted_relations,
            imports,
            tests,
            parse_state,
            parsed_call_sites,
            nested_definitions,
        } = output;

        // Convert extracted entities to model entities.
        //
        // The in-file test promotion below is the same one the full-parse path
        // applies, and it is here because its absence made role path-dependent:
        // a `#[test] fn` in `src/lib.rs` was Test after a full parse and Source
        // after an incremental reconcile, so the same code changed role
        // depending on which path last touched the file (FIR-1940). A promotion
        // on one path only is the shape kin#1123 caught in a different consumer.
        let role = classify_file_role(&file_id.0);
        let test_names: std::collections::HashSet<&str> =
            tests.iter().map(|test| test.name.as_str()).collect();
        let mut entities: Vec<Entity> = extracted_entities
            .into_iter()
            .map(|e| {
                let mut ent = e.into_entity_with_source(language, file_id, Some(&source));
                ent.role = if role == EntityRole::Source && test_names.contains(ent.name.as_str()) {
                    EntityRole::Test
                } else {
                    role
                };
                ent
            })
            .collect();
        materialize_derived_members(
            &mut entities,
            derived_members,
            language,
            file_id,
            role,
            blob_hash,
        );
        // How the file uses names as values, for the escape census. A file
        // whose record holds something always carries an entity for it.
        let escape_census = kin_parser::value_escape::census_file(&tree, &source, language);
        if entities.is_empty()
            && escape_census
                .as_ref()
                .is_some_and(|census| !census.record.is_empty())
        {
            if let Some(surface) =
                kin_parser::value_escape::module_surface(language, file_id, &tree, &source)
            {
                let mut surface = surface.into_entity_with_source(language, file_id, Some(&source));
                surface.role = role;
                entities.push(surface);
            }
        }
        separate_unit_identities(&mut entities, file_id);
        attach_span_source_digest(&mut entities, blob_hash);
        attach_own_code(&mut entities, &source);
        attach_file_context_metadata(&mut entities, file_id, &imports);
        attach_file_reference_parse_counts(
            &mut entities,
            &extracted_relations,
            &imports,
            parsed_call_sites,
        );
        attach_file_nested_definition_record(&mut entities, nested_definitions.as_ref());
        attach_equivalence_class(&mut entities, &tree, &source, language);
        kin_parser::binding_callability::attach_binding_callability(
            &tree,
            &source,
            language,
            &mut entities,
        );
        kin_parser::value_origins::attach_value_origins(
            &tree,
            &source,
            language,
            &file_id.0,
            &mut entities,
        );
        kin_parser::value_escape::attach(escape_census.as_ref(), &mut entities);
        if language == LanguageId::Go {
            kin_parser::attach_go_package_metadata(&tree, &source, &mut entities);
            kin_parser::attach_go_command_effect_contract_metadata(&tree, &source, &mut entities);
        }

        // Resolve extracted relations to model relations using entity name mapping
        let parse_completeness = ParseCompleteness::from_parse_state(&parse_state);
        let (relations, unresolved_relations) = resolve_relations(
            &extracted_relations,
            &entities,
            file_id,
            &parse_completeness,
        );
        let file_layout = build_layout(file_id, &entities, source.len(), &[], parse_completeness);

        debug!(
            path = %path.display(),
            entities = entities.len(),
            resolved_relations = relations.len(),
            unresolved_relations = unresolved_relations.len(),
            incremental = old_tree.is_some(),
            "indexed file"
        );

        Ok((
            IndexedFileWithTests {
                indexed_file: IndexedFile {
                    file_id: file_id.clone(),
                    language,
                    entities,
                    relations,
                    unresolved_relations,
                    file_layout,
                    extracted_relations,
                    imports,
                    parse_state,
                    blob_hash,
                },
                tests,
            },
            tree,
        ))
    }

    /// Index a single file with an optional incremental parse hint and retain parser tests.
    pub fn index_file_with_hint_with_tests(
        &self,
        path: &Path,
        blob_store: &BlobStore,
        old_tree: Option<&tree_sitter::Tree>,
        edit_hint: Option<&kin_parser::EditHint>,
    ) -> Result<(IndexedFileWithTests, tree_sitter::Tree)> {
        let _span = tracing::info_span!(
            "kin.index.index_file_with_hint_with_tests",
            path = %path.display(),
            incremental = old_tree.is_some() && edit_hint.is_some()
        )
        .entered();
        let file_id = FilePathId::new(path.display().to_string());
        self.index_file_hint_with_id(
            path,
            blob_store,
            &file_id,
            old_tree,
            edit_hint,
            &NoProjectFiles,
        )
    }

    /// Index a file with hint, normalizing its `FilePathId` relative to the given root.
    ///
    /// Same as `index_file_with_hint` but derives all identity from the
    /// root-relative path. `EntityId` and `RelationId` are computed from the
    /// normalized path, so a repo indexed at different absolute locations yields
    /// identical IDs and reconcile agrees with init.
    pub fn index_file_relative_with_hint(
        &self,
        path: &Path,
        blob_store: &BlobStore,
        root: &Path,
        old_tree: Option<&tree_sitter::Tree>,
        edit_hint: Option<&kin_parser::EditHint>,
    ) -> Result<(IndexedFile, tree_sitter::Tree)> {
        self.index_file_relative_with_hint_in(
            path,
            blob_store,
            root,
            old_tree,
            edit_hint,
            &NoProjectFiles,
        )
    }

    /// [`IndexPipeline::index_file_relative_with_hint`] for a unit of the tree
    /// whose project manifests `project` holds.
    pub fn index_file_relative_with_hint_in(
        &self,
        path: &Path,
        blob_store: &BlobStore,
        root: &Path,
        old_tree: Option<&tree_sitter::Tree>,
        edit_hint: Option<&kin_parser::EditHint>,
        project: &dyn ProjectFiles,
    ) -> Result<(IndexedFile, tree_sitter::Tree)> {
        let file_id = normalize_file_path_id(path, root);
        let (indexed, tree) =
            self.index_file_hint_with_id(path, blob_store, &file_id, old_tree, edit_hint, project)?;
        Ok((indexed.indexed_file, tree))
    }

    /// Index a file with hint, deriving identity from the root-relative path
    /// while retaining parser-emitted tests.
    pub fn index_file_relative_with_hint_with_tests(
        &self,
        path: &Path,
        blob_store: &BlobStore,
        root: &Path,
        old_tree: Option<&tree_sitter::Tree>,
        edit_hint: Option<&kin_parser::EditHint>,
    ) -> Result<(IndexedFileWithTests, tree_sitter::Tree)> {
        let file_id = normalize_file_path_id(path, root);
        self.index_file_hint_with_id(
            path,
            blob_store,
            &file_id,
            old_tree,
            edit_hint,
            &NoProjectFiles,
        )
    }

    /// Index a file, deriving all identity from the root-relative path.
    ///
    /// The `root` prefix is stripped and path separators normalized to forward
    /// slashes, producing a stable cross-platform `FilePathId` regardless of
    /// whether the caller passes an absolute or relative path. Entity and
    /// relation IDs are content-addressed from this normalized path, so the same
    /// repo content produces identical IDs across machines and across the
    /// init/reconcile paths.
    pub fn index_file_relative(
        &self,
        path: &Path,
        blob_store: &BlobStore,
        root: &Path,
    ) -> Result<IndexedFile> {
        self.index_file_relative_in(path, blob_store, root, &NoProjectFiles)
    }

    /// [`IndexPipeline::index_file_relative`] for a unit of the tree whose
    /// project manifests `project` holds.
    pub fn index_file_relative_in(
        &self,
        path: &Path,
        blob_store: &BlobStore,
        root: &Path,
        project: &dyn ProjectFiles,
    ) -> Result<IndexedFile> {
        let _span = tracing::info_span!(
            "kin.index.index_file_relative",
            path = %path.display(),
            root = %root.display()
        )
        .entered();
        Ok(self
            .index_file_relative_with_tests(path, blob_store, root, project)?
            .indexed_file)
    }

    /// Index a file, deriving identity from the root-relative path and retaining
    /// parser-emitted tests, as a unit of the tree whose project manifests
    /// `project` holds.
    pub fn index_file_relative_with_tests(
        &self,
        path: &Path,
        blob_store: &BlobStore,
        root: &Path,
        project: &dyn ProjectFiles,
    ) -> Result<IndexedFileWithTests> {
        let _span = tracing::info_span!(
            "kin.index.index_file_relative_with_tests",
            path = %path.display(),
            root = %root.display()
        )
        .entered();
        let source =
            std::fs::read(path).map_err(|e| IndexError::io(path.display().to_string(), e))?;
        let blob_hash = blob_store.write(&source)?;
        debug!(path = %path.display(), hash = %blob_hash, "stored source blob");
        let file_id = normalize_file_path_id(path, root);
        self.index_file_content_in(&file_id, &source, blob_hash, project)
    }

    /// Enrich exact bytes that have already been admitted into repository tree
    /// truth.
    ///
    /// The caller supplies the blob hash for those exact bytes. Parser and
    /// artifact support only select an enrichment facet; they never decide
    /// whether the file exists in the repository.
    pub fn index_any_content(
        &self,
        file_id: &FilePathId,
        content: &[u8],
        blob_hash: kin_blobs::Hash256,
    ) -> Result<IndexedAny> {
        self.index_any_content_in(file_id, content, blob_hash, &NoProjectFiles)
    }

    /// [`IndexPipeline::index_any_content`] for a unit of the tree whose
    /// project manifests `project` holds.
    pub fn index_any_content_in(
        &self,
        file_id: &FilePathId,
        content: &[u8],
        blob_hash: kin_blobs::Hash256,
        project: &dyn ProjectFiles,
    ) -> Result<IndexedAny> {
        let path = Path::new(&file_id.0);
        let classification = FileClassifier::classify_with_content(path, content);

        match classification {
            FileClassification::EntitySource => Ok(IndexedAny::EntitySource(
                self.index_file_content_in(file_id, content, blob_hash, project)?
                    .indexed_file,
            )),
            FileClassification::StructuredArtifact(kind) => {
                let artifact = artifacts::extract_artifact(kind, content, file_id)
                    .map_err(|e| IndexError::Graph(e.to_string()))?;

                debug!(
                    path = %file_id,
                    kind = ?kind,
                    hash = %blob_hash,
                    "enriched structured artifact"
                );

                Ok(IndexedAny::StructuredArtifact(artifact))
            }
            FileClassification::ShallowSyntax { language_hint } => {
                if let Some(shallow) = parse_shallow_file(content, file_id, &language_hint) {
                    debug!(
                        path = %file_id,
                        lang = %language_hint,
                        decls = shallow.declarations.len(),
                        imports = shallow.imports.len(),
                        "enriched shallow syntax file (C2)"
                    );
                    return Ok(IndexedAny::ShallowSyntax(shallow));
                }

                debug!(
                    path = %file_id,
                    lang = %language_hint,
                    "C2 grammar unavailable; retaining opaque enrichment"
                );
                Ok(IndexedAny::OpaqueArtifact(OpaqueArtifact {
                    file_id: file_id.clone(),
                    content_hash: Hash256::from_bytes(blob_hash.0),
                    mime_type: None,
                    text_preview: artifacts::opaque_text_preview(content, None),
                }))
            }
            FileClassification::OpaqueArtifact { mime_hint } => {
                debug!(
                    path = %file_id,
                    mime = ?mime_hint,
                    hash = %blob_hash,
                    "enriched opaque artifact"
                );

                let text_preview = artifacts::opaque_text_preview(content, mime_hint.as_deref());
                Ok(IndexedAny::OpaqueArtifact(OpaqueArtifact {
                    file_id: file_id.clone(),
                    content_hash: Hash256::from_bytes(blob_hash.0),
                    mime_type: mime_hint,
                    text_preview,
                }))
            }
        }
    }

    /// Index any file by storing its exact bytes once, then routing those same
    /// bytes through optional semantic enrichment.
    ///
    /// - EntitySource files go through the tree-sitter parser pipeline.
    /// - StructuredArtifact files go through the artifact extractor.
    /// - OpaqueArtifact files are stored as blobs with an optional MIME hint.
    pub fn index_any_file(&self, path: &Path, blob_store: &BlobStore) -> Result<IndexedAny> {
        let _span = tracing::info_span!(
            "kin.index.index_any_file",
            path = %path.display()
        )
        .entered();
        let content =
            std::fs::read(path).map_err(|e| IndexError::io(path.display().to_string(), e))?;
        let blob_hash = blob_store.write(&content)?;
        let file_id = FilePathId::new(path.display().to_string());
        self.index_any_content(&file_id, &content, blob_hash)
    }

    /// Index a file and upsert results into the graph store.
    pub fn index_and_store<G: GraphStore>(
        &self,
        path: &Path,
        blob_store: &BlobStore,
        graph: &G,
    ) -> Result<IndexedFile> {
        let _span = tracing::info_span!(
            "kin.index.index_and_store",
            path = %path.display()
        )
        .entered();
        let indexed = self.index_file(path, blob_store)?;

        for entity in &indexed.entities {
            graph
                .upsert_entity(entity)
                .map_err(|e| IndexError::Graph(e.to_string()))?;
        }
        for relation in &indexed.relations {
            graph
                .upsert_relation(relation)
                .map_err(|e| IndexError::Graph(e.to_string()))?;
        }

        debug!(
            path = %path.display(),
            "upserted {} entities and {} relations",
            indexed.entities.len(),
            indexed.relations.len()
        );

        Ok(indexed)
    }

    /// Resolve cross-file relations given parse data from all files.
    pub fn resolve_cross_file(
        &self,
        files: &[crate::linker::FileParseData],
        artifact_ids: &crate::linker::ArtifactIdentityMap,
    ) -> Result<Vec<Relation>> {
        let _span =
            tracing::info_span!("kin.index.resolve_cross_file", files = files.len()).entered();
        crate::linker::link_cross_file(files, artifact_ids)
    }

    /// Get the adapter registry for direct access.
    pub fn registry(&self) -> &AdapterRegistry {
        &self.registry
    }
}

impl Default for IndexPipeline {
    fn default() -> Self {
        Self::new()
    }
}

/// Normalize a file path into a stable `FilePathId` by stripping the working
/// directory prefix and converting backslashes to forward slashes.
///
/// If `path` does not start with `root`, it is used as-is (already relative).
pub fn normalize_file_path_id(path: &Path, root: &Path) -> FilePathId {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let normalized = relative.to_string_lossy().replace('\\', "/");
    FilePathId::new(normalized)
}

/// Where a container entity's own code is written: the text of its span with
/// the spans of the entities inside it taken out, whitespace collapsed. A
/// class's own code is its class-level statements (a `default_config` dict,
/// its attributes), a module's is its module-level code. What its members say
/// belongs to the members, which carry their own bodies.
pub const OWN_CODE_KEY: &str = "own_code";

/// The kinds whose span holds other entities, whose own code is recorded.
fn holds_members(kind: kin_model::EntityKind) -> bool {
    use kin_model::EntityKind;
    matches!(
        kind,
        EntityKind::Class
            | EntityKind::Interface
            | EntityKind::TraitDef
            | EntityKind::EnumDef
            | EntityKind::Module
            | EntityKind::Package
            | EntityKind::File
    )
}

/// Record each container's own code under [`OWN_CODE_KEY`]. Language
/// neutral: it reads spans alone.
fn attach_own_code(entities: &mut [Entity], source: &[u8]) {
    let spans: Vec<(usize, usize)> = entities
        .iter()
        .filter_map(|entity| entity.span.as_ref())
        .map(|span| (span.start_byte, span.end_byte))
        .filter(|(start, end)| start < end && *end <= source.len())
        .collect();
    for entity in entities.iter_mut() {
        if !holds_members(entity.kind) {
            continue;
        }
        let Some((start, end)) = entity
            .span
            .as_ref()
            .map(|span| (span.start_byte, span.end_byte))
            .filter(|(start, end)| start < end && *end <= source.len())
        else {
            continue;
        };
        let mut inner: Vec<(usize, usize)> = spans
            .iter()
            .copied()
            .filter(|&(s, e)| start <= s && e <= end && (s, e) != (start, end))
            .collect();
        inner.sort_unstable();
        let mut own = String::new();
        let mut at = start;
        for (s, e) in inner {
            if s > at {
                own.push_str(&String::from_utf8_lossy(&source[at..s]));
                own.push(' ');
            }
            at = at.max(e);
        }
        if at < end {
            own.push_str(&String::from_utf8_lossy(&source[at..end]));
        }
        let collapsed = own.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            continue;
        }
        entity
            .metadata
            .extra
            .insert(OWN_CODE_KEY.into(), serde_json::Value::String(collapsed));
    }
}

/// Bind parser-derived spans to the complete source blob, including trivia.
fn attach_span_source_digest(entities: &mut [Entity], blob_hash: kin_blobs::Hash256) {
    let digest = blob_hash.to_string();
    for entity in entities {
        entity
            .metadata
            .extra
            .insert("blob_hash".into(), digest.clone().into());
    }
}

/// Attach the behavior-equivalence class to each entity of a participating
/// language. Written graph-side onto `fingerprint.equivalence_hash` and consumed
/// at review time to tell behavior-preserving body edits (docstring / comment /
/// formatting) apart from real behavior changes.
///
/// Languages that do not participate keep the zero-hash sentinel; the review
/// layer never downgrades on the sentinel — the safe default. The digest keys
/// only on the entity's own AST node, so it is a pure, deterministic function of
/// that node's tokens.
fn attach_equivalence_class(
    entities: &mut [Entity],
    tree: &Tree,
    source: &[u8],
    language: LanguageId,
) {
    if !language_supports_equivalence(language) {
        return;
    }
    let root = tree.root_node();
    let file = FileEquivalenceContext::for_file(&root, source, language);
    for ent in entities.iter_mut() {
        let Some(span) = ent.span.as_ref() else {
            continue;
        };
        let end = span.end_byte.saturating_sub(1).max(span.start_byte);
        if let Some(node) = root.descendant_for_byte_range(span.start_byte, end) {
            ent.fingerprint.equivalence_hash =
                behavior_equivalence_hash_in_file(&node, source, language, &file);
        }
    }
}

/// Key under which language adapters attach command-effect contracts to
/// entity metadata.
pub const COMMAND_EFFECT_CONTRACT_KEY: &str = "command_effect_contract";

/// Classify a file path into an [`EntityRole`] based on directory and filename patterns.
///
/// Entities from the same file share the same role. The classifier checks path
/// components against well-known patterns (test dirs, vendor dirs, docs, generated
/// markers) and falls back to `Source` for everything else.
///
/// Test-directory detection matches whole path components (via
/// [`kin_model::test_paths::is_test_dir_component`]) rather than raw substrings, so a testing tool's own
/// product package — e.g. pytest's `_pytest/` — classifies as source and keeps its
/// signature/breaking changes on the contract surface. A repo's actual test tree
/// (`tests/`, `__tests__/`, or any `test_*`/`*_test` file) still resolves to the
/// test role.
pub fn classify_file_role(path: &str) -> EntityRole {
    if kin_model::test_paths::is_test_path(path) {
        return EntityRole::Test;
    }

    let lower = path.to_ascii_lowercase();

    // Vendored paths (copied dependencies)
    if lower.starts_with("vendor/")
        || lower.contains("/vendor/")
        || lower.starts_with("_vendor/")
        || lower.contains("/_vendor/")
        || lower.starts_with("node_modules/")
        || lower.contains("/node_modules/")
    {
        return EntityRole::Vendored;
    }

    // External paths (foreign code not vendored)
    if lower.starts_with("cextern/")
        || lower.contains("/cextern/")
        || lower.starts_with("extern/")
        || lower.contains("/extern/")
        || lower.starts_with("external/")
        || lower.contains("/external/")
        || lower.starts_with("third_party/")
        || lower.contains("/third_party/")
        || lower.starts_with("thirdparty/")
        || lower.contains("/thirdparty/")
    {
        return EntityRole::External;
    }

    // Generated paths. Amalgamated single-header bundles are byte-copies of
    // real sources regenerated out-of-band: their entities must never read as
    // independent consumers of the sources they were copied from.
    if lower.starts_with("generated/")
        || lower.contains("/generated/")
        || lower.starts_with("__generated__/")
        || lower.contains("/__generated__/")
        || lower.starts_with("single_include/")
        || lower.contains("/single_include/")
        || lower.contains("amalgamated")
        || lower.ends_with(".pb.go")
        || lower.ends_with("_pb2.py")
        || lower.ends_with(".generated.ts")
        || lower.ends_with(".generated.rs")
    {
        return EntityRole::Generated;
    }

    // Docs paths
    if lower.starts_with("docs/")
        || lower.contains("/docs/")
        || lower.starts_with("doc/")
        || lower.contains("/doc/")
        || lower.ends_with(".md")
        || lower.ends_with(".rst")
        || lower.ends_with(".txt")
    {
        return EntityRole::Docs;
    }

    EntityRole::Source
}

/// A generated member has identity and provenance, but no independent source span.
fn materialize_derived_members(
    entities: &mut Vec<Entity>,
    sites: Vec<kin_parser::ExtractedDerivedMember>,
    language: LanguageId,
    file: &FilePathId,
    role: EntityRole,
    blob_hash: kin_blobs::Hash256,
) {
    use kin_model::derivation::{
        DerivationSchema, EntityDerivation, ENTITY_DERIVATION_KEY, MEMBER_COVERAGE_KEY,
    };
    if sites.is_empty() {
        return;
    }
    let coverage = serde_json::json!({"schema":"kin.computed.members.v1", "scope":"javascript_bracket_writes", "observed_write_sites":sites.len(),
        "runtime_enumeration_complete":false, "sites":sites.iter().map(|site| serde_json::json!({
            "generator":site.generator,"assignment":site.assignment,"owner":site.owner,
            "candidate_keys":site.keys,"rule":site.rule,"conditions":site.conditions
        })).collect::<Vec<_>>()});
    if let Some(module) = entities
        .iter_mut()
        .find(|e| e.kind == kin_model::EntityKind::Module)
    {
        module
            .metadata
            .extra
            .insert(MEMBER_COVERAGE_KEY.into(), coverage);
    }
    for site in sites {
        // Noncallable writes remain typed coverage evidence until Field support is integrated.
        if !site.callable {
            continue;
        }
        let Some(owner) = site.owner else {
            continue;
        };
        for key in site.keys {
            if key.is_empty() {
                continue;
            }
            let name = format!("{owner}.{key}");
            let derivation = EntityDerivation {
                schema: DerivationSchema::V1,
                generator: site.generator.clone(),
                assignment: site.assignment.clone(),
                source_blob_hash: blob_hash.to_string(),
                owner: owner.clone(),
                member_key: key,
                rule: site.rule.clone(),
                conditions: site.conditions.clone(),
            };
            let mut metadata = kin_model::EntityMetadata::default();
            metadata.extra.insert(
                ENTITY_DERIVATION_KEY.into(),
                serde_json::to_value(&derivation).expect("derivation serializes"),
            );
            entities.push(Entity {
                id: kin_model::EntityId::from_content(&file.0, &name,
                    &format!("derived_member_candidate:{}", site.assignment.start_byte), site.assignment.start_line),
                kind: kin_model::EntityKind::Method,
                name, language, fingerprint: site.fingerprint.clone(), file_origin: Some(file.clone()),
                span: None, signature: site.signature.clone(), visibility: kin_model::Visibility::Public,
                role, doc_summary: Some("Candidate generated member; inspect the shared generator and unresolved conditions.".into()),
                metadata, lineage_parent: None, created_in: None, superseded_by: None,
            });
        }
    }
}

/// Resolve extracted name-based relations to entity-ID-based relations.
///
/// Returns both same-file resolved relations and cross-file unresolved ones.
/// Unresolved relations have the source entity ID but target name for deferred linking.
fn resolve_relations(
    extracted: &[kin_parser::ExtractedRelation],
    entities: &[Entity],
    file_id: &FilePathId,
    parse_completeness: &ParseCompleteness,
) -> (Vec<Relation>, Vec<UnresolvedRelation>) {
    let mut resolved = Vec::new();
    let mut relation_indices = HashMap::new();
    let mut unresolved = Vec::new();
    let source_index = crate::RelationSourceIndex::new(entities);
    let same_file = crate::linker::file_name_slots(entities);
    let call_extraction_complete = !extracted
        .iter()
        .any(kin_parser::is_call_extraction_incomplete_marker);

    for rel in extracted {
        if kin_parser::is_reserved_parser_record(rel) {
            continue;
        }
        let lexical_contains = crate::relation_source::is_lexical_contains(rel);
        let contains_pair = lexical_contains
            .then(|| source_index.resolve_lexical_contains(rel))
            .flatten();
        let src = if lexical_contains {
            contains_pair.map(|(parent, _)| parent)
        } else {
            source_index.resolve(rel)
        };
        let dst = if lexical_contains {
            contains_pair.map(|(_, child)| child)
        } else {
            same_file
                .get(rel.dst_name.as_str())
                .copied()
                .filter(|_| !crate::linker::requires_rust_import_authority(rel, &file_id.0))
                .filter(|slot| {
                    crate::linker::same_file_slot_binds(
                        rel.kind,
                        Some(&slot.kind),
                        Some(&slot.language),
                    )
                })
                // A Python name the file imports is bound twice, and which binding
                // a call reads depends on an order this resolver does not model, so
                // the linker settles it: through the import, with the file's own
                // declaration kept only as a candidate. Binding it here stamped the
                // same-file declaration parser-certain, including for a name a
                // function body imports from another module.
                .filter(|slot| {
                    !(slot.language == kin_model::LanguageId::Python
                        && crate::linker::carries_non_rust_import(rel, &file_id.0))
                })
                // A guard-narrowed call names a possible owner, never a proven
                // one, and a member value's destination depends on the class
                // hierarchy, so only the linker's own tiers may bind them.
                .filter(|_| !crate::linker::requires_linker_tier(rel))
                .filter(|_| {
                    // Receiver-shaped fields and unproven local method calls must
                    // reach the linker instead of capturing a bare free symbol.
                    //
                    // Python records the receiver of every attribute call whose
                    // owner the parser could not pin, so `settings.get(...)` and
                    // `@app.post(...)` arrive as the leaf `get` or `post` with a
                    // receiver. Such a call reaches a member of the receiver's
                    // type, never a module-level function that shares the leaf, and
                    // the linker already refuses that same-file match. Binding it
                    // here instead stamped it parser-certain.
                    !(crate::linker::is_go_selector_reference(
                        rel,
                        src.map(|entity| entity.language),
                    ) || (src.is_some_and(|s| {
                        matches!(
                            s.language,
                            kin_model::LanguageId::Go | kin_model::LanguageId::Python
                        )
                    }) && rel.kind == kin_model::RelationKind::Calls
                        && rel.receiver.is_some()
                        && !rel.dst_name.contains('.')))
                })
        };

        match (src, dst) {
            (Some(s), Some(d)) => {
                // Same-file relation. A daemon start re-derives this edge from
                // the extracted relation through the linker, then compares it
                // with the committed one, so both paths mint this id.
                let mut relation = Relation {
                    id: crate::linker::stable_relation_id(&s.id, &d.id, &rel.kind),
                    kind: rel.kind,
                    src: kin_model::GraphNodeId::Entity(s.id),
                    dst: kin_model::GraphNodeId::Entity(d.id),
                    confidence: 1.0,
                    origin: RelationOrigin::Parsed,
                    created_in: None,
                    import_source: rel.import_source.clone(),
                    evidence: crate::linker::relation_evidence(
                        rel,
                        file_id,
                        parse_completeness,
                        call_extraction_complete,
                    ),
                };
                crate::occurrence::stamp_fresh(&mut relation);
                crate::linker::accumulate_relation(&mut resolved, &mut relation_indices, relation);
            }
            (Some(s), None) => {
                // Partial resolution: src found, dst is cross-file
                debug!(
                    src = %rel.src_name,
                    dst = %rel.dst_name,
                    kind = ?rel.kind,
                    "unresolved cross-file relation, deferring to linker"
                );
                unresolved.push(UnresolvedRelation {
                    kind: rel.kind,
                    src_entity_id: s.id,
                    dst_name: rel.dst_name.clone(),
                });
            }
            _ => {
                // Both unresolved: skip entirely for now
                debug!(
                    src = %rel.src_name,
                    dst = %rel.dst_name,
                    kind = ?rel.kind,
                    "both src and dst unresolved, skipping"
                );
            }
        }
    }
    let derived = entities
        .iter()
        .filter(|entity| kin_model::is_derived_member(entity))
        .map(|entity| entity.id)
        .collect();
    crate::linker::limit_derived_relations(&mut resolved, &derived);
    (resolved, unresolved)
}

#[cfg(test)]
mod tests {

    /// A container's own code is its span with its members' spans taken out:
    /// Flask's class-level `default_config` belongs to its field, a method's body is
    /// the method's, and the module's own code is what no entity in it spans.
    #[test]
    fn a_container_records_its_own_code_and_not_its_members() {
        let source = b"import os\n\nclass Flask:\n    \"\"\"The app.\"\"\"\n\n    default_config = {\n        \"SESSION_REFRESH_EACH_REQUEST\": True,\n    }\n\n    def run(self):\n        return serve_forever()\n\nDEBUG_MODULE_FLAG = os.environ.get(\"X\")\n";
        let indexed = IndexPipeline::new()
            .index_file_content_with_tests(
                &FilePathId::new("src/flask/app.py"),
                source,
                kin_blobs::digest(source),
            )
            .unwrap()
            .indexed_file;
        let own = |name: &str| {
            indexed
                .entities
                .iter()
                .find(|entity| entity.name == name)
                .and_then(|entity| entity.metadata.extra.get(OWN_CODE_KEY))
                .and_then(|value| value.as_str())
                .map(str::to_string)
        };
        let class = own("Flask").expect("the class records its own code");
        assert!(!class.contains("SESSION_REFRESH_EACH_REQUEST"), "{class}");
        assert!(!class.contains("serve_forever"), "{class}");
        let key = indexed
            .entities
            .iter()
            .find(|entity| entity.name == "Flask.default_config.SESSION_REFRESH_EACH_REQUEST")
            .unwrap();
        let span = key.span.as_ref().unwrap();
        assert_eq!(
            &source[span.start_byte..span.end_byte],
            b"\"SESSION_REFRESH_EACH_REQUEST\": True"
        );
        assert_eq!(own("Flask.run"), None, "a method records no own code");
        let module = indexed
            .entities
            .iter()
            .find(|entity| entity.kind == kin_model::EntityKind::Module)
            .and_then(|entity| entity.metadata.extra.get(OWN_CODE_KEY))
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        assert!(module.contains("import os"), "{module}");
        assert!(!module.contains("default_config"), "{module}");
    }
    #[test]
    fn container_own_code_keeps_middle_evidence_beyond_the_old_window() {
        let padding = "padding café ".repeat(1_000);
        let source = format!(
            "register(\"{padding}module_center_beacon{padding}\")\n\n\
             class Settings:\n    register(\
             {{\"padding_head\": \"{padding}\", \
             \"class_center_beacon\": True, \
             \"padding_tail\": \"{padding}\"}})\n\n\
             \x20\x20\x20\x20def run(self):\n        return member_only_beacon()\n"
        );
        let indexed = IndexPipeline::new()
            .index_file_content_with_tests(
                &FilePathId::new("settings.py"),
                source.as_bytes(),
                kin_blobs::digest(source.as_bytes()),
            )
            .unwrap()
            .indexed_file;
        for (kind, present, absent) in [
            (
                kin_model::EntityKind::Class,
                "class_center_beacon",
                "module_center_beacon",
            ),
            (
                kin_model::EntityKind::Module,
                "module_center_beacon",
                "class_center_beacon",
            ),
        ] {
            let entity = indexed
                .entities
                .iter()
                .find(|entity| entity.kind == kind)
                .unwrap();
            let own = entity
                .metadata
                .extra
                .get(OWN_CODE_KEY)
                .and_then(|value| value.as_str())
                .unwrap();
            assert!(
                own.chars().count() > 16_000,
                "{kind:?} retained only a window"
            );
            assert!(own.contains(present), "{kind:?} lost its middle evidence");
            assert!(own.contains("café"), "{kind:?} lost Unicode source text");
            assert!(
                !own.contains(absent),
                "{kind:?} inherited another entity's evidence"
            );
            assert!(
                !own.contains("member_only_beacon"),
                "{kind:?} inherited a member body"
            );
        }
    }

    use super::*;

    #[test]
    fn source_ingestion_persists_scalar_binding_and_complete_python_write_census() {
        let pipeline = IndexPipeline::new();
        let file = FilePathId::new("app.py");
        for (source, writes) in [
            (
                "VALUE = 'value'\ndef render(): return 'prefix' + VALUE\n",
                1,
            ),
            ("VALUE = 'value'\nVALUE = lambda: 7\n", 2),
        ] {
            let digest = kin_blobs::digest(source.as_bytes());
            let indexed = pipeline
                .index_file_content_with_tests(&file, source.as_bytes(), digest)
                .unwrap()
                .indexed_file;
            let census_entity = indexed
                .entities
                .iter()
                .find(|entity| {
                    entity
                        .metadata
                        .extra
                        .contains_key("python_binding_census_v1")
                })
                .unwrap();
            assert_eq!(
                census_entity.metadata.extra["blob_hash"],
                digest.to_string()
            );
            assert_eq!(
                census_entity.metadata.extra["python_binding_census_v1"]["writes"]["VALUE"],
                writes
            );
            assert_eq!(
                indexed
                    .entities
                    .iter()
                    .filter(|entity| entity
                        .metadata
                        .extra
                        .contains_key("python_binding_census_v1"))
                    .count(),
                1
            );
            assert!(indexed.entities.iter().any(|entity| entity
                .metadata
                .extra
                .get("scalar_binding_v1")
                .is_some_and(|binding| binding["name"] == "VALUE")));
        }
    }

    #[test]
    fn index_pipeline_creates() {
        let pipeline = IndexPipeline::new();
        let langs = pipeline.registry().supported_languages();
        assert_eq!(langs.len(), 14);
    }

    #[test]
    fn index_typescript_file() {
        let dir = tempfile::tempdir().unwrap();
        let blob_store = BlobStore::new(dir.path().join("blobs")).unwrap();
        let pipeline = IndexPipeline::new();

        let ts_file = dir.path().join("test.ts");
        std::fs::write(
            &ts_file,
            b"export function hello(name: string): string { return `Hello ${name}`; }",
        )
        .unwrap();

        let result = pipeline.index_file(&ts_file, &blob_store).unwrap();
        assert_eq!(result.language, LanguageId::TypeScript);
        assert!(!result.entities.is_empty());
        // By kind and name, the way `index_python_file` below already does it.
        // Since FIR-2675 every TypeScript file carries a Module entity and it is
        // emitted first, so `entities[0]` is the module: this assertion read
        // "test" for the file stem rather than the function it names.
        assert!(
            result
                .entities
                .iter()
                .any(|entity| entity.kind == kin_model::EntityKind::Function
                    && entity.name == "hello"),
            "expected the exported function, got {:?}",
            result
                .entities
                .iter()
                .map(|e| (e.kind, e.name.as_str()))
                .collect::<Vec<_>>()
        );
        assert!(matches!(result.parse_state, ParseState::Valid));
    }

    #[test]
    fn index_python_file() {
        let dir = tempfile::tempdir().unwrap();
        let blob_store = BlobStore::new(dir.path().join("blobs")).unwrap();
        let pipeline = IndexPipeline::new();

        let py_file = dir.path().join("test.py");
        std::fs::write(&py_file, b"def greet(name):\n    return f'Hello {name}'").unwrap();

        let result = pipeline.index_file(&py_file, &blob_store).unwrap();
        assert_eq!(result.language, LanguageId::Python);
        assert!(
            result
                .entities
                .iter()
                .any(|entity| entity.kind == kin_model::EntityKind::Function
                    && entity.name == "greet")
        );
    }

    #[test]
    fn index_python_repeated_calls_aggregate_shape_evidence_order_independently() {
        let dir = tempfile::tempdir().unwrap();
        let blob_store = BlobStore::new(dir.path().join("blobs")).unwrap();
        let pipeline = IndexPipeline::new();
        let py_file = dir.path().join("test.py");
        let forward = b"def target(ext, args):\n    return ext, args\n\n\ndef caller():\n    target(1, 2)\n    return target(3, args=4)\n";
        let reversed = b"def target(ext, args):\n    return ext, args\n\n\ndef caller():\n    target(3, args=4)\n    return target(1, 2)\n";
        let evidence_for = |source: &[u8]| {
            std::fs::write(&py_file, source).unwrap();
            let indexed = pipeline.index_file(&py_file, &blob_store).unwrap();
            let caller = indexed
                .entities
                .iter()
                .find(|entity| entity.name == "caller")
                .expect("caller entity");
            let target = indexed
                .entities
                .iter()
                .find(|entity| entity.name == "target")
                .expect("target entity");
            let relation = indexed
                .relations
                .iter()
                .find(|relation| {
                    relation.kind == kin_model::RelationKind::Calls
                        && relation.src == kin_model::GraphNodeId::Entity(caller.id)
                        && relation.dst == kin_model::GraphNodeId::Entity(target.id)
                })
                .expect("one logical Calls edge");
            crate::occurrence::original_evidence(relation)
                .expect("valid occurrence metadata")
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()
        };

        let shapes = |evidence: &[kin_model::RelationEvidence]| {
            let mut shapes: Vec<String> = evidence
                .iter()
                .map(|record| format!("{:?}x{}", record.call_shape, record.occurrence_count))
                .collect();
            shapes.sort();
            shapes
        };

        let forward_evidence = evidence_for(forward);
        let reversed_evidence = evidence_for(reversed);
        // Evidence carries the call SITE now, and the two sources put their
        // calls at different offsets, so byte equality across the reversal is
        // not the invariant any more. What must hold is that the shapes reaching
        // the callee, which is what the merge gate reads, do not depend on which
        // call was written first.
        assert_eq!(shapes(&forward_evidence), shapes(&reversed_evidence));
        assert_eq!(forward_evidence.len(), 2);
        assert!(forward_evidence.iter().any(|evidence| {
            evidence
                .call_shape
                .as_ref()
                .is_some_and(|shape| shape.keywords == ["args"])
        }));
        assert!(
            forward_evidence
                .iter()
                .all(|record| record.source_span.is_some()),
            "the same-file arm must carry each call's site: {forward_evidence:?}"
        );
    }

    #[test]
    fn incomplete_python_parse_marks_same_file_call_shape_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let blob_store = BlobStore::new(dir.path().join("blobs")).unwrap();
        let pipeline = IndexPipeline::new();
        let py_file = dir.path().join("test.py");
        std::fs::write(
            &py_file,
            b"def target(ext, args):\n    return ext, args\n\n\ndef caller():\n    target(1, 2)\n    return target(3, args=4\n",
        )
        .unwrap();

        let indexed = pipeline.index_file(&py_file, &blob_store).unwrap();
        assert!(matches!(indexed.parse_state, ParseState::Incomplete { .. }));
        let caller = indexed
            .entities
            .iter()
            .find(|entity| entity.name == "caller")
            .expect("caller entity");
        let target = indexed
            .entities
            .iter()
            .find(|entity| entity.name == "target")
            .expect("target entity");
        let edge = indexed
            .relations
            .iter()
            .find(|relation| {
                relation.kind == kin_model::RelationKind::Calls
                    && relation.src == kin_model::GraphNodeId::Entity(caller.id)
                    && relation.dst == kin_model::GraphNodeId::Entity(target.id)
            })
            .expect("surviving positional call edge");
        let evidence =
            crate::occurrence::original_evidence(edge).expect("valid occurrence metadata");
        assert_eq!(evidence.len(), 1);
        assert_eq!(
            evidence[0].parser_rule.as_deref(),
            Some(crate::linker::CALL_SHAPE_EVIDENCE_INCOMPLETE_PARSE_V1)
        );
        assert!(evidence[0].call_shape.is_none());
    }

    #[test]
    fn incomplete_python_call_extraction_downgrades_same_file_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let blob_store = BlobStore::new(dir.path().join("blobs")).unwrap();
        let py_file = dir.path().join("test.py");
        std::fs::write(
            &py_file,
            b"def target(ext, args):\n    return ext, args\n\ndef other(ext, args):\n    return ext, args\n\ndef caller(flag):\n    target(1, 2)\n    return (target if flag else other)(ext=1, args=2)\n",
        )
        .unwrap();

        let indexed = IndexPipeline::new()
            .index_file(&py_file, &blob_store)
            .expect("index syntax-valid Python source");
        assert!(matches!(indexed.parse_state, ParseState::Valid));
        assert!(matches!(
            indexed.file_layout.parse_completeness,
            ParseCompleteness::Full
        ));
        assert!(
            indexed
                .extracted_relations
                .iter()
                .any(kin_parser::is_call_extraction_incomplete_marker),
            "raw parser marker must survive for later linking and history"
        );
        assert!(
            indexed.unresolved_relations.iter().all(|relation| {
                relation.dst_name != kin_parser::CALL_EXTRACTION_INCOMPLETE_MARKER_V1
            }),
            "the control record must never become an unresolved semantic relation"
        );
        let caller = indexed
            .entities
            .iter()
            .find(|entity| entity.name == "caller")
            .expect("caller entity");
        let target = indexed
            .entities
            .iter()
            .find(|entity| entity.name == "target")
            .expect("target entity");
        let edge = indexed
            .relations
            .iter()
            .find(|relation| {
                relation.kind == kin_model::RelationKind::Calls
                    && relation.src == kin_model::GraphNodeId::Entity(caller.id)
                    && relation.dst == kin_model::GraphNodeId::Entity(target.id)
            })
            .expect("surviving same-file target edge");
        let evidence =
            crate::occurrence::original_evidence(edge).expect("valid occurrence metadata");
        assert_eq!(evidence.len(), 1);
        assert_eq!(
            evidence[0].parser_rule.as_deref(),
            Some(crate::linker::CALL_SHAPE_EVIDENCE_INCOMPLETE_EXTRACTION_V1)
        );
        assert!(evidence[0].call_shape.is_none());
    }

    #[test]
    fn index_unsupported_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let blob_store = BlobStore::new(dir.path().join("blobs")).unwrap();
        let pipeline = IndexPipeline::new();

        let txt_file = dir.path().join("readme.txt");
        std::fs::write(&txt_file, b"just some text").unwrap();

        let result = pipeline.index_file(&txt_file, &blob_store);
        assert!(result.is_err());
    }

    #[test]
    fn index_c_file_entity_source() {
        let dir = tempfile::tempdir().unwrap();
        let blob_store = BlobStore::new(dir.path().join("blobs")).unwrap();
        let pipeline = IndexPipeline::new();

        let c_file = dir.path().join("hello.c");
        std::fs::write(
            &c_file,
            b"#include <stdio.h>\n\nvoid hello(void) {\n    printf(\"hi\\n\");\n}\n\nint add(int a, int b) {\n    return a + b;\n}\n",
        )
        .unwrap();

        let result = pipeline.index_file(&c_file, &blob_store).unwrap();
        let names: Vec<&str> = result.entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(result.language, LanguageId::C);
        assert!(names.contains(&"hello"), "Expected 'hello' in {:?}", names);
        assert!(names.contains(&"add"), "Expected 'add' in {:?}", names);
    }

    #[test]
    fn index_rust_file() {
        let dir = tempfile::tempdir().unwrap();
        let blob_store = BlobStore::new(dir.path().join("blobs")).unwrap();
        let pipeline = IndexPipeline::new();

        let rs_file = dir.path().join("lib.rs");
        std::fs::write(
            &rs_file,
            b"pub fn add(a: i32, b: i32) -> i32 { a + b }\npub struct Point { x: f64, y: f64 }",
        )
        .unwrap();

        let result = pipeline.index_file(&rs_file, &blob_store).unwrap();
        assert_eq!(result.language, LanguageId::Rust);
        assert!(result.entities.len() >= 2);
    }

    #[test]
    fn classify_source_files() {
        assert_eq!(classify_file_role("src/lib.rs"), EntityRole::Source);
        assert_eq!(classify_file_role("pkg/api/handler.go"), EntityRole::Source);
        assert_eq!(classify_file_role("internal/core.go"), EntityRole::Source);
    }

    #[test]
    fn classify_test_files() {
        assert_eq!(classify_file_role("tests/unit_test.py"), EntityRole::Test);
        assert_eq!(classify_file_role("src/foo_test.rs"), EntityRole::Test);
        assert_eq!(classify_file_role("src/foo_test.go"), EntityRole::Test);
        assert_eq!(classify_file_role("src/foo.test.ts"), EntityRole::Test);
        assert_eq!(classify_file_role("src/foo.spec.js"), EntityRole::Test);
        assert_eq!(classify_file_role("test/helpers.py"), EntityRole::Test);
        assert_eq!(
            classify_file_role("__tests__/App.test.ts"),
            EntityRole::Test
        );
    }

    /// Colocated framework tests, which the eight-literal rule read as product
    /// source. A React repository with `Foo.test.tsx` beside `Foo.tsx`
    /// classified 100% Source, and the two linker gates that filter on
    /// `role != Test` then removed nothing, because nothing was Test
    /// (FIR-1940).
    ///
    /// The controls are the point. Widening a suffix rule is only safe if it
    /// stays delimited, and every name in the second group ends with the letters
    /// of a marker without being a test.
    #[test]
    fn classify_colocated_framework_test_files() {
        for path in [
            // The dot-delimited family, across every extension the JS and TS
            // adapters claim rather than the four the old rule listed.
            "src/Foo.test.tsx",
            "src/Foo.spec.tsx",
            "src/foo.test.jsx",
            "src/foo.test.mjs",
            "src/foo.test.cjs",
            // The underscore-delimited family, beyond py/rs/go.
            "src/foo_test.ts",
            "src/foo_test.js",
            "spec/models/user_spec.rb",
            // The capital-delimited family. Java, C#, Kotlin and Swift name the
            // test after the type under test, with no separator but the case.
            "src/main/java/com/example/FooTest.java",
            "src/main/java/com/example/FooTests.java",
            "tests/FooTests.cs",
            "src/FooSpec.kt",
            "Tests/FooTests.swift",
        ] {
            assert_eq!(
                classify_file_role(path),
                EntityRole::Test,
                "{path} is a test by a convention this rule has to cover"
            );
        }

        // Controls. Each ends with a marker's letters and is not a test, which
        // is what a bare lowercased `ends_with` would get wrong.
        for path in [
            // `latest` ends with `test`. This is the name that makes a widened
            // suffix rule unsafe without a delimiter.
            "src/latest.java",
            "src/Latest.java",
            "latest/release.rs",
            "src/contest.py",
            "src/manifest.rs",
            // And the case the previous fix bought: pytest's own product
            // package must not be swept in by any widening.
            "src/_pytest/python.py",
        ] {
            assert_eq!(
                classify_file_role(path),
                EntityRole::Source,
                "{path} is product code and no marker is delimited in it"
            );
        }
    }

    /// A testing tool ships product code whose package name embeds "test"
    /// (pytest's `_pytest/`). That code is the framework's own public API — its
    /// signature/breaking changes are a contract surface — so it must classify
    /// as source, never test, even though a naive substring match on "test/"
    /// would sweep it into the test role and suppress those findings. The tool's
    /// actual test tree (`testing/`) still classifies as test.
    #[test]
    fn classify_test_named_product_package_as_source() {
        // pytest's shipped product package, in both `src/` layout and bare form.
        assert_eq!(
            classify_file_role("src/_pytest/python.py"),
            EntityRole::Source
        );
        assert_eq!(classify_file_role("_pytest/main.py"), EntityRole::Source);
        assert_eq!(classify_file_role("_pytest/nodes.py"), EntityRole::Source);
        assert_eq!(
            classify_file_role("src/_pytest/__init__.py"),
            EntityRole::Source
        );
        assert_eq!(
            classify_file_role("src/pytest/__init__.py"),
            EntityRole::Source
        );
        // Other product packages whose names merely embed a test keyword.
        assert_eq!(
            classify_file_role("src/contest/round.py"),
            EntityRole::Source
        );
        assert_eq!(classify_file_role("latest/release.rs"), EntityRole::Source);

        // pytest's own test tree stays test-role: a `test_*` file is a test
        // wherever it lives, and compound / underscore-prefixed test dirs still
        // resolve to a `test` word.
        assert_eq!(
            classify_file_role("testing/test_collection.py"),
            EntityRole::Test
        );
        assert_eq!(
            classify_file_role("integration_tests/api.py"),
            EntityRole::Test
        );
        assert_eq!(
            classify_file_role("acceptance_test/flow.py"),
            EntityRole::Test
        );
        assert_eq!(classify_file_role("_tests/helper.py"), EntityRole::Test);
        // A bare helper in a `testing/` tree is still test infrastructure even
        // without a `test_*` filename — `testing` is a test word, while
        // `_pytest`/`contest`/`latest` embed `test` only as a substring.
        assert_eq!(
            classify_file_role("testing/util_helper.py"),
            EntityRole::Test
        );
        assert_eq!(classify_file_role("testing/conftest.py"), EntityRole::Test);
    }

    /// A package a repository ships for other people's tests is product
    /// source: React's `react-test-renderer` host config, its `test-utils`,
    /// a `testing-library`. Only a directory whose last word names tests is
    /// a test tree, and a test file inside such a package is still a test by
    /// its own name or directory.
    #[test]
    fn classify_a_package_shipped_for_tests_as_source() {
        for path in [
            "packages/react-test-renderer/src/ReactFiberConfigTestHost.js",
            "packages/react-test-renderer/index.js",
            "packages/react-dom/test-utils.js",
            "packages/react-dom/src/test-utils/ReactTestUtils.js",
            "packages/internal-test-utils/consoleMock.js",
            "src/testing-library/render.ts",
        ] {
            assert_eq!(
                classify_file_role(path),
                EntityRole::Source,
                "{path} ships for tests and is not one"
            );
        }
        for path in [
            "packages/react-test-renderer/src/__tests__/ReactTestRenderer-test.js",
            "packages/react-test-renderer/src/ReactTestRenderer.test.js",
            "packages/react-test-renderer/tests/render.js",
            "packages/react-dom/src/test-utils/unit_tests/helper.js",
            "e2e-tests/login.ts",
            "src/test/java/com/example/Foo.java",
            "pkg/integration-test/flow.go",
        ] {
            assert_eq!(classify_file_role(path), EntityRole::Test, "{path}");
        }
    }

    #[test]
    fn classify_external_files() {
        assert_eq!(
            classify_file_role("cextern/wcslib/wcs.c"),
            EntityRole::External
        );
        assert_eq!(
            classify_file_role("third_party/zlib/zlib.h"),
            EntityRole::External
        );
    }

    #[test]
    fn classify_vendored_files() {
        assert_eq!(
            classify_file_role("vendor/github.com/pkg/errors/errors.go"),
            EntityRole::Vendored
        );
        assert_eq!(
            classify_file_role("node_modules/react/index.js"),
            EntityRole::Vendored
        );
    }

    #[test]
    fn classify_generated_files() {
        assert_eq!(classify_file_role("proto/api.pb.go"), EntityRole::Generated);
        assert_eq!(
            classify_file_role("gen/schema_pb2.py"),
            EntityRole::Generated
        );
        assert_eq!(
            classify_file_role("src/generated/types.ts"),
            EntityRole::Generated
        );
    }

    #[test]
    fn classify_docs_files() {
        assert_eq!(classify_file_role("docs/guide.md"), EntityRole::Docs);
        assert_eq!(classify_file_role("README.md"), EntityRole::Docs);
        assert_eq!(classify_file_role("doc/api.rst"), EntityRole::Docs);
    }

    // Relative indexing must derive entity/relation identity from the
    // root-relative path, never the machine-absolute path, so that the init and
    // reconcile paths agree and identical content produces identical IDs across
    // machines and checkout locations.

    // `add` carries a doc comment and an attribute on purpose: a Rust item's
    // span starts on those, and the id must still key on the `pub fn` line.
    const REL_RUST_SRC: &[u8] = b"pub struct Point { x: f64, y: f64 }\n\
impl Point { pub fn origin() -> Point { Point { x: 0.0, y: 0.0 } } }\n\
/// Adds two numbers.\n\
#[inline]\n\
pub fn add(a: i32, b: i32) -> i32 { a + b }\n";

    fn write_repo_file(root: &Path, rel: &str, content: &[u8]) -> std::path::PathBuf {
        let abs = root.join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&abs, content).unwrap();
        abs
    }

    fn entity_id_set(indexed: &IndexedFile) -> std::collections::HashSet<kin_model::EntityId> {
        indexed.entities.iter().map(|e| e.id).collect()
    }

    fn relation_id_set(indexed: &IndexedFile) -> std::collections::HashSet<kin_model::RelationId> {
        indexed.relations.iter().map(|r| r.id).collect()
    }

    #[test]
    fn relative_index_derives_entity_ids_from_relative_path_not_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let blob_store = BlobStore::new(root.join("blobs")).unwrap();
        let pipeline = IndexPipeline::new();

        let abs = write_repo_file(root, "src/geo/point.rs", REL_RUST_SRC);
        let indexed = pipeline
            .index_file_relative(&abs, &blob_store, root)
            .unwrap();

        assert_eq!(indexed.file_id.0, "src/geo/point.rs");

        let mut checked = 0;
        let mut widened = 0;
        for entity in &indexed.entities {
            let Some(span) = entity.span.as_ref() else {
                continue;
            };
            // The id keys on the declaration line, which the parser records
            // when the span starts above it on a doc comment or attribute.
            let id_line = entity
                .metadata
                .extra
                .get(kin_parser::DECLARATION_LINE_KEY)
                .and_then(|line| line.as_u64())
                .map_or(span.start_line, |line| {
                    widened += 1;
                    line as u32
                });
            let relative = kin_model::EntityId::from_content(
                "src/geo/point.rs",
                &entity.name,
                &format!("{:?}", entity.kind),
                id_line,
            );
            let absolute = kin_model::EntityId::from_content(
                &abs.display().to_string(),
                &entity.name,
                &format!("{:?}", entity.kind),
                id_line,
            );
            assert_eq!(
                entity.id, relative,
                "entity {} is not addressed by its relative path",
                entity.name
            );
            assert_ne!(
                entity.id, absolute,
                "entity {} still bakes the machine-absolute path",
                entity.name
            );
            assert_eq!(entity.file_origin.as_ref().unwrap().0, "src/geo/point.rs");
            checked += 1;
        }
        assert!(checked >= 2, "expected at least two spanned entities");
        assert_eq!(
            widened, 1,
            "the documented `add` must record its declaration line, or this test no longer exercises a widened span"
        );
    }

    #[test]
    fn relative_index_is_invariant_across_absolute_roots() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let blob_a = BlobStore::new(dir_a.path().join("blobs")).unwrap();
        let blob_b = BlobStore::new(dir_b.path().join("blobs")).unwrap();
        let pipeline = IndexPipeline::new();

        let abs_a = write_repo_file(dir_a.path(), "src/foo.rs", REL_RUST_SRC);
        let abs_b = write_repo_file(dir_b.path(), "src/foo.rs", REL_RUST_SRC);

        let a = pipeline
            .index_file_relative(&abs_a, &blob_a, dir_a.path())
            .unwrap();
        let b = pipeline
            .index_file_relative(&abs_b, &blob_b, dir_b.path())
            .unwrap();

        assert!(!a.entities.is_empty());
        assert_eq!(
            entity_id_set(&a),
            entity_id_set(&b),
            "entity IDs must not depend on the absolute checkout location"
        );
        assert_eq!(
            relation_id_set(&a),
            relation_id_set(&b),
            "relation IDs must not depend on the absolute checkout location"
        );
    }

    #[test]
    fn reconcile_relative_matches_init_content_ids() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let blob_store = BlobStore::new(root.join("blobs")).unwrap();
        let pipeline = IndexPipeline::new();

        let abs = write_repo_file(root, "src/foo.rs", REL_RUST_SRC);

        // reconcile path: absolute path on disk, normalized against the root.
        let reconcile = pipeline
            .index_file_relative(&abs, &blob_store, root)
            .unwrap();

        // init path: already-loaded content addressed by the relative file_id.
        let content = std::fs::read(&abs).unwrap();
        let blob_hash = blob_store.write(&content).unwrap();
        let init = pipeline
            .index_file_content_with_tests(&FilePathId::new("src/foo.rs"), &content, blob_hash)
            .unwrap()
            .indexed_file;

        assert_eq!(
            entity_id_set(&reconcile),
            entity_id_set(&init),
            "reconcile must produce the same entity IDs as init"
        );
        assert_eq!(
            relation_id_set(&reconcile),
            relation_id_set(&init),
            "reconcile must produce the same relation IDs as init"
        );
    }

    #[test]
    fn relative_index_relations_reference_local_entities() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let blob_store = BlobStore::new(root.join("blobs")).unwrap();
        let pipeline = IndexPipeline::new();

        let abs = write_repo_file(root, "src/foo.rs", REL_RUST_SRC);
        let indexed = pipeline
            .index_file_relative(&abs, &blob_store, root)
            .unwrap();

        let ids = entity_id_set(&indexed);
        for rel in &indexed.relations {
            if let kin_model::GraphNodeId::Entity(src) = &rel.src {
                assert!(
                    ids.contains(src),
                    "relation src {src:?} is not a local entity"
                );
            }
            if let kin_model::GraphNodeId::Entity(dst) = &rel.dst {
                assert!(
                    ids.contains(dst),
                    "relation dst {dst:?} is not a local entity"
                );
            }
        }
        for unresolved in &indexed.unresolved_relations {
            assert!(
                ids.contains(&unresolved.src_entity_id),
                "unresolved relation src {:?} is not a local entity",
                unresolved.src_entity_id
            );
        }
    }

    #[test]
    fn relative_hint_index_matches_non_hint_relative_ids() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let blob_store = BlobStore::new(root.join("blobs")).unwrap();
        let pipeline = IndexPipeline::new();

        let abs = write_repo_file(root, "src/foo.rs", REL_RUST_SRC);

        let full = pipeline
            .index_file_relative(&abs, &blob_store, root)
            .unwrap();
        let (hinted, _tree) = pipeline
            .index_file_relative_with_hint(&abs, &blob_store, root, None, None)
            .unwrap();

        assert_eq!(hinted.file_id.0, "src/foo.rs");
        assert_eq!(
            entity_id_set(&full),
            entity_id_set(&hinted),
            "incremental and full relative indexing must agree on entity IDs"
        );
    }

    #[test]
    fn admitted_binary_with_source_extension_gets_only_opaque_enrichment() {
        let bytes = b"\0\xff\x10not-rust";
        let hash = kin_blobs::digest(bytes);
        let indexed = IndexPipeline::new()
            .index_any_content(&FilePathId::new("src/payload.rs"), bytes, hash)
            .unwrap();

        let IndexedAny::OpaqueArtifact(artifact) = indexed else {
            panic!("binary bytes must not be forced through the Rust parser");
        };
        assert_eq!(artifact.file_id, FilePathId::new("src/payload.rs"));
        assert_eq!(artifact.content_hash, Hash256::from_bytes(hash.0));
        assert_eq!(
            artifact.mime_type.as_deref(),
            Some("application/octet-stream")
        );
        assert!(
            artifact.text_preview.is_none(),
            "binary bytes must not enter the text index"
        );
    }

    /// The FIR-2183 mechanism: real ingest stored every opaque artifact with no
    /// text at all, so the text index and the artifact embedding saw only a
    /// path and a MIME type, and a docs store could not answer for its own
    /// content. The deep marker sits past any head-sized preview, so this test
    /// fails against head-only retention too, not just against `None`.
    #[test]
    fn admitted_markdown_keeps_deep_text_in_its_retrieval_preview() {
        let mut body =
            String::from("# Field Notes\n\nHead marker: zebrafish lantern protocol.\n\n");
        for index in 0..40 {
            body.push_str(&format!(
                "Routine paragraph {index} about ordinary upkeep of the greenhouse ledger and \
                 irrigation schedule, nothing remarkable here.\n\n"
            ));
        }
        body.push_str(
            "## Deep Doctrine\n\nThe quokka semaphore doctrine states that a docs store must \
             answer for its own content.\n",
        );
        assert!(
            body.find("quokka").unwrap() > 2_000,
            "the deep marker must sit beyond any head-sized preview"
        );

        let bytes = body.as_bytes();
        let hash = kin_blobs::digest(bytes);
        let indexed = IndexPipeline::new()
            .index_any_content(&FilePathId::new("AGENTS.md"), bytes, hash)
            .unwrap();

        let IndexedAny::OpaqueArtifact(artifact) = indexed else {
            panic!("markdown must take the opaque enrichment facet");
        };
        let preview = artifact
            .text_preview
            .expect("a textual artifact must retain its text for retrieval");
        assert!(preview.contains("zebrafish lantern protocol"));
        assert!(preview.contains("quokka semaphore doctrine"));
    }

    /// The retention bound holds, and an extensionless text file qualifies by
    /// content rather than by MIME hint.
    #[test]
    fn artifact_text_retention_is_bounded_and_content_qualified() {
        let body = "word ".repeat(60_000);
        let bytes = body.as_bytes();
        let hash = kin_blobs::digest(bytes);
        let indexed = IndexPipeline::new()
            .index_any_content(&FilePathId::new("NOTICE"), bytes, hash)
            .unwrap();

        let IndexedAny::OpaqueArtifact(artifact) = indexed else {
            panic!("extensionless text must take the opaque enrichment facet");
        };
        assert_eq!(artifact.mime_type, None);
        let preview = artifact
            .text_preview
            .expect("printable extensionless bytes must qualify as text");
        assert_eq!(
            preview.chars().count(),
            crate::artifacts::ARTIFACT_TEXT_RETENTION_CHARS
        );
    }

    #[test]
    fn admitted_compose_bytes_get_structured_enrichment_without_changing_blob_identity() {
        let bytes = b"services:\n  api:\n    image: example/api\n";
        let hash = kin_blobs::digest(bytes);
        let indexed = IndexPipeline::new()
            .index_any_content(&FilePathId::new("compose.yaml"), bytes, hash)
            .unwrap();

        let IndexedAny::StructuredArtifact(artifact) = indexed else {
            panic!("Compose content must receive its structured facet");
        };
        assert_eq!(artifact.file_id, FilePathId::new("compose.yaml"));
        assert_eq!(artifact.kind, kin_model::ArtifactKind::ComposeFile);
        assert_ne!(
            artifact.content_hash,
            Hash256::from_bytes(hash.0),
            "structured hash is an enrichment fingerprint, not exact tree identity"
        );
    }

    #[test]
    fn test_functions_in_a_production_file_carry_the_test_role() {
        let dir = tempfile::tempdir().unwrap();
        let blob_store = BlobStore::new(dir.path().join("blobs")).unwrap();
        let pipeline = IndexPipeline::new();
        let source = br#"
pub fn shipped() -> u32 { 1 }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_returns_one() {
        assert_eq!(shipped(), 1);
    }
}
"#;
        let blob_hash = blob_store.write(source).unwrap();
        let indexed = pipeline
            .index_file_content_with_tests(&FilePathId::new("src/shipped.rs"), source, blob_hash)
            .unwrap();

        let role_of = |name: &str| {
            indexed
                .indexed_file
                .entities
                .iter()
                .find(|entity| entity.name == name)
                .unwrap_or_else(|| {
                    panic!(
                        "{name} missing from {:?}",
                        indexed
                            .indexed_file
                            .entities
                            .iter()
                            .map(|e| e.name.as_str())
                            .collect::<Vec<_>>()
                    )
                })
                .role
        };

        assert_eq!(
            role_of("shipped_returns_one"),
            EntityRole::Test,
            "a test in a production path is still a test"
        );
        assert_eq!(
            role_of("shipped"),
            EntityRole::Source,
            "the production function beside it keeps its role"
        );
    }

    fn assert_span_blob_provenance(indexed: &IndexedFile, source: &[u8]) {
        let expected = kin_blobs::digest(source).to_string();
        assert_eq!(indexed.blob_hash.to_string(), expected);
        assert!(
            !indexed.entities.is_empty(),
            "source fixture must produce entities"
        );
        for entity in &indexed.entities {
            let span = entity
                .span
                .as_ref()
                .expect("parsed entity must have a span");
            assert!(source
                .get(span.start_byte as usize..span.end_byte as usize)
                .is_some());
            assert_eq!(
                entity
                    .metadata
                    .extra
                    .get("blob_hash")
                    .and_then(|value| value.as_str()),
                Some(expected.as_str()),
                "{} span must name the exact parsed source blob",
                entity.name
            );
        }
    }

    #[test]
    fn span_provenance_current_content_and_file_ingestion_bind_exact_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let blob_store = BlobStore::new(dir.path().join("blobs")).unwrap();
        let pipeline = IndexPipeline::new();
        for (relative, source) in [
            (
                "src/lib.rs",
                &b"// exact file bytes\npub fn answer() -> u8 { 1 }\n"[..],
            ),
            (
                "src/sessions.py",
                &b"# exact file bytes\ndef merge_setting(value):\n    return value\n"[..],
            ),
        ] {
            let path = write_repo_file(dir.path(), relative, source);
            let blob_hash = blob_store.write(source).unwrap();
            let content = pipeline
                .index_file_content_with_tests(&FilePathId::new(relative), source, blob_hash)
                .unwrap()
                .indexed_file;
            assert_span_blob_provenance(&content, source);
            let file = pipeline
                .index_file_relative(&path, &blob_store, dir.path())
                .unwrap();
            assert_span_blob_provenance(&file, source);
            assert_eq!(file.entities, content.entities);
        }
    }

    #[test]
    fn span_provenance_incremental_edit_advances_digest_for_unchanged_entity() {
        let dir = tempfile::tempdir().unwrap();
        let blob_store = BlobStore::new(dir.path().join("blobs")).unwrap();
        let pipeline = IndexPipeline::new();
        let before = b"pub fn answer() -> u8 { 1 }\n";
        let after = b"\npub fn answer() -> u8 { 1 }\n";
        let path = write_repo_file(dir.path(), "src/lib.rs", before);
        let (old, tree) = pipeline
            .index_file_relative_with_hint(&path, &blob_store, dir.path(), None, None)
            .unwrap();
        assert_span_blob_provenance(&old, before);
        std::fs::write(&path, after).unwrap();
        let hint = kin_parser::EditHint {
            start_byte: 0,
            old_end_byte: 0,
            new_end_byte: 1,
        };
        let (new, _) = pipeline
            .index_file_relative_with_hint(&path, &blob_store, dir.path(), Some(&tree), Some(&hint))
            .unwrap();
        assert_span_blob_provenance(&new, after);
        assert_ne!(old.blob_hash, new.blob_hash);
        let old_answer = old
            .entities
            .iter()
            .find(|entity| entity.name == "answer")
            .unwrap();
        let new_answer = new
            .entities
            .iter()
            .find(|entity| entity.name == "answer")
            .unwrap();
        assert_eq!(old_answer.fingerprint, new_answer.fingerprint);
        assert_ne!(old_answer.span, new_answer.span);
        let full = pipeline
            .index_file_relative(&path, &blob_store, dir.path())
            .unwrap();
        assert_eq!(new.entities, full.entities);
    }

    #[test]
    fn span_provenance_rejects_digest_for_different_source_bytes() {
        let source = b"pub fn answer() -> u8 { 1 }\n";
        let unrelated = kin_blobs::digest(b"pub fn answer() -> u8 { 2 }\n");
        let error = IndexPipeline::new()
            .index_file_content_with_tests(&FilePathId::new("src/lib.rs"), source, unrelated)
            .unwrap_err();
        assert!(
            error.to_string().contains("supplied blob digest"),
            "{error}"
        );
        assert!(
            error.to_string().contains(&unrelated.to_string()),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .contains(&kin_blobs::digest(source).to_string()),
            "{error}"
        );
    }

    /// Retention validates every edge it would rebind before it writes any
    /// field. A certificate naming an endpoint its edge does not have refuses
    /// the whole retention and leaves the parsed file exactly as it was, so a
    /// caller never goes on with a half-remapped file.
    #[test]
    fn retaining_an_identity_over_a_corrupt_certificate_refuses_and_changes_nothing() {
        let source = b"def helper():\n    return 1\n\n\ndef renamed():\n    return helper()\n";
        let mut indexed = IndexPipeline::new()
            .index_file_content_with_tests(
                &FilePathId::new("mod.py"),
                source,
                kin_blobs::digest(source),
            )
            .unwrap()
            .indexed_file;
        let id_of = |indexed: &IndexedFile, name: &str| {
            indexed
                .entities
                .iter()
                .find(|entity| entity.name == name)
                .unwrap()
                .id
        };
        let parsed = id_of(&indexed, "renamed");
        let helper = id_of(&indexed, "helper");
        let call = indexed
            .relations
            .iter_mut()
            .find(|relation| {
                relation.kind == kin_model::RelationKind::Calls
                    && relation.src == kin_model::GraphNodeId::Entity(parsed)
                    && relation.dst == kin_model::GraphNodeId::Entity(helper)
            })
            .expect("the parser resolves the same-file call");
        let certificate = call
            .evidence
            .iter_mut()
            .find(|record| crate::occurrence::is_certificate(record))
            .expect("the fresh call carries an occurrence certificate");
        let mut proof: serde_json::Value =
            serde_json::from_str(certificate.token.as_deref().unwrap()).unwrap();
        proof["src"] =
            serde_json::to_value(kin_model::GraphNodeId::Entity(kin_model::EntityId::new()))
                .unwrap();
        certificate.token = Some(proof.to_string());
        let before = format!("{indexed:?}");

        let error = indexed
            .retain_entity_identity(parsed, kin_model::EntityId::new())
            .unwrap_err();

        assert!(
            error.contains("invalid parser occurrence evidence"),
            "the refusal names the evidence it rejected: {error}"
        );
        assert_eq!(
            format!("{indexed:?}"),
            before,
            "a refused retention changes nothing"
        );
    }

    /// A function whose `@overload` stubs sit above it, called by name from
    /// its own nested closure, binds that call to the implementation: the definition Python
    /// keeps under the name, and the one both linkers bind. Binding the first
    /// stub instead gave every live re-derivation of the file an edge a fresh
    /// import never made.
    #[test]
    fn a_self_call_under_overload_stubs_binds_the_implementation() {
        let dir = tempfile::tempdir().unwrap();
        let blob_store = BlobStore::new(dir.path().join("blobs")).unwrap();
        let pipeline = IndexPipeline::new();
        let source: &[u8] = br#"import typing as t


@t.overload
def stream(value: t.Iterator[str]) -> t.Iterator[str]: ...


@t.overload
def stream(value: t.Callable[..., t.Iterator[str]]) -> t.Callable[..., t.Iterator[str]]: ...


def stream(value):
    def wrapper(*args):
        return stream(value(*args))

    return wrapper


def caller(value):
    return stream(value)
"#;
        let blob_hash = blob_store.write(source).unwrap();
        let indexed = pipeline
            .index_file_content_with_tests(&FilePathId::new("pkg/stream.py"), source, blob_hash)
            .unwrap()
            .indexed_file;
        let mut group: Vec<&Entity> = indexed
            .entities
            .iter()
            .filter(|entity| {
                entity.name == "stream" && entity.kind == kin_model::EntityKind::Function
            })
            .collect();
        group.sort_by_key(|entity| entity.span.as_ref().map(|span| span.start_line));
        assert_eq!(group.len(), 3, "two stubs and the implementation");
        // The file's module carries the same name, `stream`, and must not take
        // the call either.
        let module = indexed
            .entities
            .iter()
            .find(|entity| entity.name == "stream" && entity.kind == kin_model::EntityKind::Module)
            .expect("the file's module is named for its stem")
            .id;
        let implementation = group[2].id;
        let caller = indexed
            .entities
            .iter()
            .find(|entity| entity.name == "caller")
            .unwrap()
            .id;
        // The recursive call is written in the implementation's nested
        // `wrapper`, which is an entity of its own and owns it.
        let wrapper = indexed
            .entities
            .iter()
            .find(|entity| entity.name == "stream.<locals>.wrapper")
            .expect("the nested closure is admitted")
            .id;
        let calls_to_group = |src: kin_model::EntityId| -> Vec<kin_model::EntityId> {
            indexed
                .relations
                .iter()
                .filter(|relation| {
                    relation.kind == kin_model::RelationKind::Calls
                        && relation.src.as_entity() == Some(src)
                })
                .filter_map(|relation| relation.dst.as_entity())
                .filter(|dst| *dst == module || group.iter().any(|entity| entity.id == *dst))
                .collect()
        };
        assert_eq!(calls_to_group(wrapper), vec![implementation]);
        assert!(calls_to_group(implementation).is_empty());
        assert_eq!(calls_to_group(caller), vec![implementation]);
    }

    /// Every answer the partial-refresh proofs give over a pair of C bodies,
    /// in a fixed order: each held declaration against each namesake in the
    /// current body, with each of its calls rebased and verified.
    type PartialProofAnswers = Vec<(
        kin_model::EntityId,
        kin_model::EntityId,
        bool,
        Vec<(Option<Relation>, bool)>,
    )>;

    fn partial_proof_answers(
        pipeline: &IndexPipeline,
        old: &IndexedFile,
        old_source: &[u8],
        new: &IndexedFile,
        source: &[u8],
    ) -> PartialProofAnswers {
        let mut answers = Vec::new();
        for held in &old.entities {
            for candidate in new
                .entities
                .iter()
                .filter(|entity| entity.name == held.name && entity.kind == held.kind)
            {
                let refresh =
                    pipeline.supports_partial_refresh(held, candidate, old_source, source);
                let calls = old
                    .relations
                    .iter()
                    .filter(|relation| relation.src.as_entity() == Some(held.id))
                    .map(|relation| {
                        (
                            pipeline.rebase_partial_relation(
                                held,
                                candidate,
                                old_source,
                                source,
                                relation,
                                &old.entities,
                            ),
                            pipeline.verifies_partial_relation(
                                held,
                                candidate,
                                old_source,
                                source,
                                relation,
                                &old.entities,
                            ),
                        )
                    })
                    .collect();
                answers.push((held.id, candidate.id, refresh, calls));
            }
        }
        answers
    }

    /// A small C file the partial-refresh proofs can read: a global, two
    /// complete functions and a torn declaration after them, so the parse is
    /// incomplete while both function bodies stay clean. `factor` is an edit
    /// inside the caller's body that leaves its call alone, which the proofs
    /// can follow; `limit` is the global's value, declaration context no proof
    /// may cross.
    fn partial_c_file(factor: u32, limit: u32) -> String {
        format!(
            "int limit = {limit};\n\nstatic int helper(int value) {{\n    return value + 1;\n}}\n\n\
             int caller(int value) {{\n    int total = helper(value);\n    \
             return total * {factor};\n}}\n\nint torn(int value {{\n"
        )
    }

    /// The partial-refresh proofs remember parsed C bodies by file and exact
    /// bytes. Remembered or not, every proof has to answer exactly what a
    /// fresh parse of the same two bodies answers: for a first reading, for a
    /// remembered one, for a different current body under the same path, and
    /// for a body read again after enough others pushed it out.
    #[test]
    fn remembered_partial_c_bodies_answer_every_proof_as_a_fresh_parse() {
        let pipeline = IndexPipeline::new();
        let file = FilePathId::new("partial.c");
        let index = |source: &str| {
            pipeline
                .index_file_content_with_tests(
                    &file,
                    source.as_bytes(),
                    kin_blobs::digest(source.as_bytes()),
                )
                .unwrap()
                .indexed_file
        };
        let before = partial_c_file(2, 1);
        let old = index(&before);
        assert!(
            matches!(old.parse_state, ParseState::Incomplete { .. }),
            "the fixture must parse incompletely, or no partial proof applies"
        );
        let forget = |on: bool| FORGET_PARTIAL_C_SOURCES.with(|forget| forget.set(on));
        let answers = |current: &str, new: &IndexedFile| {
            partial_proof_answers(&pipeline, &old, before.as_bytes(), new, current.as_bytes())
        };

        let mut refreshed = 0;
        let mut refused = 0;
        let mut rebased = 0;
        let mut first = None;
        for (factor, limit) in [(3, 1), (4, 2)] {
            let current = partial_c_file(factor, limit);
            let new = index(&current);
            forget(true);
            let fresh = answers(&current, &new);
            forget(false);
            assert_eq!(
                answers(&current, &new),
                fresh,
                "a first remembered reading answered differently from a fresh parse"
            );
            assert_eq!(
                answers(&current, &new),
                fresh,
                "a remembered body answered differently from a fresh parse"
            );
            refreshed += fresh.iter().filter(|answer| answer.2).count();
            refused += fresh.iter().filter(|answer| !answer.2).count();
            rebased += fresh
                .iter()
                .flat_map(|answer| &answer.3)
                .filter(|(relation, _)| relation.is_some())
                .count();
            first.get_or_insert((current, new, fresh));
        }
        assert!(
            refreshed > 0,
            "no declaration refreshed, so only refusals were compared"
        );
        assert!(
            refused > 0,
            "no declaration was refused, so only acceptances were compared"
        );
        assert!(
            rebased > 0,
            "no call was rebased, so the relation proofs were never compared"
        );

        // Push the first bodies out of what is remembered, then ask again.
        for n in 0..=REMEMBERED_PARTIAL_C_SOURCES {
            let other = format!("{before}/* {n} */\n");
            assert!(partial_c_source(&file, other.as_bytes()).is_some());
        }
        let (current, new, fresh) = first.unwrap();
        assert_eq!(
            answers(&current, &new),
            fresh,
            "a body parsed again after being forgotten answered differently"
        );
    }
}

#[cfg(test)]
mod unit_context_tests {
    use super::*;
    use kin_parser::project_manifest::NoProjectFiles;
    use std::sync::Arc;

    /// A tree's manifests, by path.
    struct Manifests(&'static [(&'static str, &'static str)]);

    impl ProjectFiles for Manifests {
        fn manifest(
            &self,
            path: &str,
        ) -> std::result::Result<Option<Arc<str>>, kin_parser::project_manifest::UnreadableManifest>
        {
            Ok(self
                .0
                .iter()
                .find(|(at, _)| *at == path)
                .map(|(_, text)| Arc::from(*text)))
        }
    }

    const OAUTH: Manifests = Manifests(&[(
        "examples/oauth/Cargo.toml",
        "[package]\nname = \"example-oauth\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )]);

    fn surface(indexed: &IndexedFile) -> Entity {
        indexed
            .entities
            .iter()
            .find(|entity| kin_model::is_file_module_surface(entity))
            .cloned()
            .expect("the crate root has a module of its own")
    }

    /// A crate root indexed in a tree whose manifest names its crate carries
    /// that crate's name, and the identity derived from it, and still owns
    /// its top-level declarations. With the manifests unknown it keeps the
    /// name its path gives.
    #[test]
    fn a_crate_root_takes_its_crate_name_from_the_tree_it_is_indexed_in() {
        let file = FilePathId::new("examples/oauth/src/main.rs");
        let source = b"use std::io;\n\nfn main() {\n    run();\n}\n\nfn run() {}\n";
        let digest = kin_blobs::digest(source);
        let pipeline = IndexPipeline::new();
        let named = pipeline
            .index_file_content_in(&file, source, digest, &OAUTH)
            .unwrap()
            .indexed_file;
        let unnamed = pipeline
            .index_file_content_with_tests(&file, source, digest)
            .unwrap()
            .indexed_file;
        assert_eq!(surface(&named).name, "example_oauth");
        assert_eq!(surface(&unnamed).name, "oauth");
        assert_ne!(surface(&named).id, surface(&unnamed).id);
        let held: Vec<&str> = named
            .relations
            .iter()
            .filter(|relation| {
                relation.kind == kin_model::RelationKind::Contains
                    && relation.src == kin_model::GraphNodeId::Entity(surface(&named).id)
            })
            .filter_map(|relation| relation.dst.as_entity())
            .filter_map(|id| named.entities.iter().find(|entity| entity.id == id))
            .map(|entity| entity.name.as_str())
            .collect();
        assert!(held.contains(&"main") && held.contains(&"run"), "{held:?}");
    }

    /// A kept parse answers only for the project declarations it was parsed
    /// under, so the same bytes at the same path in two trees whose
    /// manifests disagree are two parses.
    #[test]
    fn the_shared_index_keeps_a_parse_under_its_project_declarations() {
        let file = FilePathId::new("examples/oauth/src/main.rs");
        let source = b"fn main() {}\n\nfn kept_under_its_project() {}\n";
        let digest = kin_blobs::digest(source);
        let named = crate::shared_index::index_source(&file, source, digest, &OAUTH).unwrap();
        let unnamed =
            crate::shared_index::index_source(&file, source, digest, &NoProjectFiles).unwrap();
        let again = crate::shared_index::index_source(&file, source, digest, &OAUTH).unwrap();
        assert_eq!(surface(&named).name, "example_oauth");
        assert_eq!(surface(&unnamed).name, "oauth");
        assert_eq!(surface(&again).id, surface(&named).id);
    }
}
