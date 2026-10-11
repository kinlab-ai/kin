// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Declaration coverage for answers read from the entity index.
//!
//! A name search answers from the entities extraction admitted, so a
//! declaration the extractor skipped cannot be among its rows, and an empty
//! answer could not separate a name the repository lacks from one the extractor
//! never admitted. Every adapter that takes a declaration census records, per
//! file, the declarations its grammar shows against the entities it admitted,
//! and lists each one it did not admit in the file's coverage certificate. This
//! module reads those censuses for every file the search covers and publishes
//! the `declarations` coverage class under `edge_coverage`:
//!
//! - `complete`: every file in scope carries a current census and no
//!   unadmitted declaration could match the query, so the rows are the whole
//!   set the index can hold for it;
//! - `complete_except`: the same, except the listed unadmitted declarations
//!   that could match and the listed files held as bytes and never parsed;
//! - `unmeasured`: some file in scope carries no current census (its adapter
//!   takes none, its certificate predates the census or no longer matches the
//!   file), so a skipped declaration there could match and is not listed.
//!
//! A census is the declarations a file's grammar shows. A file whose parse
//! recovered from an error region still carries one, and a declaration inside
//! that region is in neither the entities nor the census. The state does not
//! change for such a file; the reading says under `parsed` whether every file
//! it read was parsed whole and counts the ones that were not, which is what
//! an answer needs before it claims that no other declaration of a name
//! exists.
//!
//! The comparison is language agnostic. Which declarations count is each
//! adapter's contract (see `kin_parser::declaration_census`); this module only
//! reads what the certificate recorded, from the graph, and never from files.
//!
//! Whether an unadmitted declaration "could match" is decided conservatively,
//! as a superset of the store's own name predicate (the lowercased name
//! contains the pattern): it matches when its own name or `owner.name`
//! contains the pattern, when it sits inside an admitted entity whose name the
//! query matched, or inside another unadmitted declaration that matches. A
//! dotted pattern also matches a declaration whose name holds the pattern's
//! last segment beside an owner or enclosing entity holding the segment before
//! it. Listing a declaration that would not have matched costs a reader one
//! look; leaving out one that would have is a false certification.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use kin_index::coverage_certificate::{
    DECLARATION_COVERAGE_V1, PARSE_COMPLETENESS_V1, PARSE_ERROR_SPAN_V1, UNADMITTED_DECLARATION_V1,
};
use kin_model::entity::{Entity, EntityKind};
use kin_model::graph::{EntityStore, RelationLookup};
use kin_model::ids::{FilePathId, LanguageId};
use kin_model::relation::{GraphNodeId, Relation, RelationKind};
use kin_parser::declaration_census::DeclarationKind;

/// The class name under `edge_coverage.classes`.
pub const DECLARATIONS_CLASS: &str = "declarations";

/// The key under `edge_coverage` carrying the census reading.
pub const DECLARATIONS_KEY: &str = "declarations";

/// Every file in scope carries a current census and nothing unadmitted matches.
pub const STATE_COMPLETE: &str = "complete";
/// Complete except the listed declarations and files.
pub const STATE_COMPLETE_EXCEPT: &str = "complete_except";
/// Some file in scope carries no current census.
pub const STATE_UNMEASURED: &str = "unmeasured";

/// The clause naming unadmitted declarations that could match, each listed.
pub const UNADMITTED_LISTED_CODE: &str = "declarations_unadmitted_listed";
/// The clause naming files in scope held as bytes and never parsed, listed.
pub const UNPARSED_FILES_LISTED_CODE: &str = "declarations_unparsed_files_listed";
/// The clause naming files in scope whose declarations nothing measured.
pub const CENSUS_UNMEASURED_CODE: &str = "declaration_census_unmeasured";

/// The residue codes: an answer refused only by these is complete except what
/// they list.
pub const RESIDUE_CODES: [&str; 2] = [UNADMITTED_LISTED_CODE, UNPARSED_FILES_LISTED_CODE];

/// How many matching unadmitted declarations a reading lists. Past this it
/// counts them in `matching_unadmitted_total`.
pub const LISTED_MAX: usize = 24;

/// How many files a reading lists per group. Past this it counts them.
pub const FILES_LISTED_MAX: usize = 12;

/// Where a reading says how the files it read a census from were parsed:
/// [`PARSED_WHOLE`] when every one was read whole, [`PARSED_IN_PART`] when the
/// parse of any recovered from an error region. Such a file carries a current
/// census of the declarations its grammar shows, and a declaration inside a
/// region the parse could not read is not one of them. The reading's state
/// says what the census holds, so it does not change with this; an answer
/// that claims no other declaration of a name exists has to read
/// [`PARSED_WHOLE`] here.
pub const PARSED_KEY: &str = "parsed";
/// Every file a census was read from was parsed whole.
pub const PARSED_WHOLE: &str = "whole";
/// The parse of some file a census was read from recovered from an error.
pub const PARSED_IN_PART: &str = "in_part";
/// How many source units a census was read from whose parse recovered from
/// an error. Present when any did.
pub const PARSED_IN_PART_COUNT_KEY: &str = "parsed_in_part";
/// Where a reading lists the first [`FILES_LISTED_MAX`] of those files.
pub const PARTLY_PARSED_FILES_KEY: &str = "partly_parsed_files";

/// Where a reading says what the regions no parse could read hold of the name
/// asked for, once [`read_unread_regions`] has read them from held content:
/// [`UNREAD_CLEAR`], [`UNREAD_SPELL_NAME`] or [`UNREAD_NOT_READ`]. Absent when
/// they were not read, which leaves a declaration of the name possible there.
pub const UNREAD_REGIONS_KEY: &str = "unread_regions";
/// No unread region of any source unit spells the name, so none declares it.
pub const UNREAD_CLEAR: &str = "clear";
/// Some unread region spells the name; the units are listed.
pub const UNREAD_SPELL_NAME: &str = "spell_name";
/// The held content of some partly parsed unit could not be read back.
pub const UNREAD_NOT_READ: &str = "not_read";
/// The source units whose unread regions spell the name: how many, and the
/// first [`FILES_LISTED_MAX`] by module.
pub const UNREAD_SPELLING_COUNT_KEY: &str = "unread_regions_spelling";
pub const UNREAD_SPELLED_IN_KEY: &str = "unread_regions_spelled_in";
/// How many partly parsed units could not be read back.
pub const UNREAD_NOT_READ_COUNT_KEY: &str = "unread_regions_not_read";

/// Why a file in scope carries no census a search can read.
const NO_CERTIFICATE: &str = "no_certificate";
const NO_CENSUS: &str = "adapter_takes_no_census";
const STALE: &str = "certificate_not_current";
const LOOKUP_UNAVAILABLE: &str = "certificate_lookup_unavailable";
const NO_INVENTORY: &str = "no_tree_inventory";
const NO_FILES: &str = "no_source_files_in_scope";
const MODULE_QUALIFICATION_UNRECORDED: &str = "module_qualification_unrecorded";
const ENTITY_OUTSIDE_TREE: &str = "entity_file_not_in_tree";

/// What a certificate's parse completeness entry says of a parse that read the
/// whole file.
const CERTIFICATE_PARSED_WHOLE: &str = "full";

/// The name question a search asked, as the census is read against it.
#[derive(Debug, Clone)]
pub struct NameQuery {
    /// Lowercased patterns with their wildcards removed; a declaration matching
    /// any of them could have been a row.
    patterns: Vec<String>,
    /// The kinds a search narrowed to, when it did.
    kinds: Option<Vec<EntityKind>>,
    /// Files of recorded modules matching a two-part qualifier, independent
    /// of whether any matching declaration was admitted.
    module_files: Option<Vec<String>>,
    /// Files of same-named modules the qualifier's dotted path excluded.
    ruled_out_files: Vec<String>,
}

impl NameQuery {
    /// A query for `pattern`, narrowed to `kinds` when the search was.
    pub fn new(pattern: &str, kinds: Option<&[EntityKind]>) -> Self {
        Self {
            patterns: vec![normalize(pattern)],
            kinds: kinds.map(<[EntityKind]>::to_vec),
            module_files: None,
            ruled_out_files: Vec::new(),
        }
    }

    pub(crate) fn with_module_files<'a>(mut self, files: impl Iterator<Item = &'a str>) -> Self {
        self.module_files = Some(files.map(str::to_string).collect());
        self
    }

    pub(crate) fn with_ruled_out_files<'a>(mut self, files: impl Iterator<Item = &'a str>) -> Self {
        self.ruled_out_files = files.map(str::to_string).collect();
        self
    }

    fn ruled_out(&self, path: &str) -> bool {
        self.ruled_out_files.iter().any(|file| file == path)
    }

    fn file_module_clue_matches(path: &str, module: &str) -> bool {
        let (directory, filename) = path.rsplit_once('/').unwrap_or(("", path));
        let stem = filename.rsplit_once('.').map_or(filename, |(stem, _)| stem);
        let clue = if matches!(stem, "__init__" | "index") {
            directory.rsplit('/').next().unwrap_or(directory)
        } else {
            stem
        };
        clue.to_lowercase() == module
    }

    fn module_context_missing(&self, path: &str) -> bool {
        !self.ruled_out(path)
            && self.module_files.as_ref().is_some_and(|files| {
                !files.iter().any(|file| file == path)
                    && self.patterns.iter().any(|pattern| {
                        crate::module_name_query::parts(pattern)
                            .is_some_and(|(module, _)| Self::file_module_clue_matches(path, module))
                    })
            })
    }

    fn in_file(&self, path: &str) -> Self {
        let mut scoped = self.clone();
        for pattern in &self.patterns {
            let Some((module, leaf)) = crate::module_name_query::parts(pattern) else {
                continue;
            };
            // The file stem is a conservative census clue ONLY. It never
            // manufactures a positive entity or proves module containment.
            // It keeps a withheld module/leaf from becoming a false absence
            // when no admitted module entity is available at all.
            if self.ruled_out(path) {
                continue;
            }
            if self
                .module_files
                .as_ref()
                .is_some_and(|files| files.iter().any(|file| file == path))
                || Self::file_module_clue_matches(path, module)
            {
                scoped.patterns.push(leaf.to_string());
            }
        }
        scoped
    }

    fn admits_kind(&self, kind: DeclarationKind) -> bool {
        self.kinds.as_ref().is_none_or(|kinds| {
            kinds
                .iter()
                .any(|admitted| kind.may_be_admitted_as(*admitted))
        })
    }

    fn matches_text(&self, text: &str) -> bool {
        let text = text.to_lowercase();
        self.patterns
            .iter()
            .any(|pattern| pattern.is_empty() || text.contains(pattern.as_str()))
    }
}

fn normalize(pattern: &str) -> String {
    pattern.trim().trim_matches('*').to_lowercase()
}

fn segments(pattern: &str) -> Vec<&str> {
    pattern
        .split(['.', ':', '#', '/'])
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// One unadmitted declaration as the census recorded it.
#[derive(Debug, Clone)]
struct Unadmitted {
    name: String,
    kind: DeclarationKind,
    reason: String,
    owner: Option<String>,
    start_byte: usize,
    end_byte: usize,
    line: u32,
}

/// One file's census as its certificate records it.
struct FileCensus {
    seen: usize,
    admitted: usize,
    unadmitted: Vec<Unadmitted>,
    /// The parse read the whole file. False when it recovered from an error
    /// region, inside which the grammar shows no declaration to count.
    parsed_whole: bool,
    /// How many error regions the parse recovered from, and the byte ranges
    /// of the ones the certificate lists.
    error_regions: usize,
    error_spans: Vec<(usize, usize)>,
}

/// A source unit in scope whose parse recovered from an error: its content
/// digest, the error regions its certificate lists and how many there were.
#[derive(Debug, Clone)]
pub(crate) struct UnreadUnit {
    pub(crate) path: String,
    pub(crate) digest: kin_model::Hash256,
    regions: usize,
    spans: Vec<(usize, usize)>,
    /// Where each declaration this answer already holds or lists begins in
    /// this unit, when its own name holds the pattern: the spelling of that
    /// name at its declaration is that declaration, not another one.
    accounted: Vec<usize>,
}

/// Read one file's census from its certificate, when the certificate is this
/// file's, current for `blob`, and carries a census.
fn read_census(
    relation: &Relation,
    path: &str,
    node: GraphNodeId,
    blob: kin_model::Hash256,
) -> Result<FileCensus, &'static str> {
    if relation.src != node || relation.dst != node || relation.kind != RelationKind::DependsOn {
        return Err(NO_CERTIFICATE);
    }
    if kin_index::parse_coverage_source_digest(relation) != Some(blob) {
        return Err(STALE);
    }
    let mut summary: Option<(usize, usize)> = None;
    let mut unadmitted = Vec::new();
    let mut parsed_whole = false;
    let mut error_regions = 0usize;
    let mut error_spans = Vec::new();
    for entry in &relation.evidence {
        match entry.parser_rule.as_deref() {
            Some(PARSE_COMPLETENESS_V1) => {
                parsed_whole = entry.token.as_deref() == Some(CERTIFICATE_PARSED_WHOLE);
                error_regions = entry.occurrence_count as usize;
            }
            Some(PARSE_ERROR_SPAN_V1) => {
                if let Some(span) = &entry.source_span {
                    error_spans.push((span.start_byte, span.end_byte));
                }
            }
            Some(DECLARATION_COVERAGE_V1) => {
                if entry.source_path.as_deref() != Some(path) || summary.is_some() {
                    return Err(STALE);
                }
                let admitted = entry
                    .token
                    .as_deref()
                    .and_then(|token| token.parse::<usize>().ok())
                    .ok_or(STALE)?;
                summary = Some((entry.occurrence_count as usize, admitted));
            }
            Some(UNADMITTED_DECLARATION_V1) => {
                let (Some(token), Some(span)) = (entry.token.as_deref(), &entry.source_span) else {
                    return Err(STALE);
                };
                let fields: Vec<&str> = token.split('\u{1f}').collect();
                let [name, kind, reason, owner] = fields.as_slice() else {
                    return Err(STALE);
                };
                unadmitted.push(Unadmitted {
                    name: (*name).to_string(),
                    kind: DeclarationKind::from_label(kind).ok_or(STALE)?,
                    reason: (*reason).to_string(),
                    owner: (!owner.is_empty()).then(|| (*owner).to_string()),
                    start_byte: span.start_byte,
                    end_byte: span.end_byte,
                    line: span.start_line,
                });
            }
            _ => {}
        }
    }
    let (seen, admitted) = summary.ok_or(NO_CENSUS)?;
    Ok(FileCensus {
        seen,
        admitted,
        unadmitted,
        parsed_whole,
        error_regions,
        error_spans,
    })
}

/// An admitted entity whose own name the query matched, by the byte range it
/// covers in its file.
struct Enclosing {
    start: usize,
    end: usize,
}

/// Which of one file's unadmitted declarations could match `query`.
fn matching<'a>(
    query: &NameQuery,
    unadmitted: &'a [Unadmitted],
    enclosing: &[Enclosing],
) -> Vec<&'a Unadmitted> {
    let within = |start: usize, end: usize, outer_start: usize, outer_end: usize| {
        outer_start <= start && end <= outer_end && (outer_start, outer_end) != (start, end)
    };
    // Outermost first, so a match by position can see its enclosing matches.
    let mut order: Vec<usize> = (0..unadmitted.len()).collect();
    order.sort_by_key(|&index| {
        (
            unadmitted[index].start_byte,
            std::cmp::Reverse(unadmitted[index].end_byte),
        )
    });
    let mut matched = vec![false; unadmitted.len()];
    for &index in &order {
        let declaration = &unadmitted[index];
        let qualified = match &declaration.owner {
            Some(owner) => format!("{owner}.{}", declaration.name),
            None => declaration.name.clone(),
        };
        let by_name = query.matches_text(&qualified);
        let inside_match = enclosing.iter().any(|outer| {
            within(
                declaration.start_byte,
                declaration.end_byte,
                outer.start,
                outer.end,
            )
        }) || order.iter().any(|&other| {
            matched[other]
                && within(
                    declaration.start_byte,
                    declaration.end_byte,
                    unadmitted[other].start_byte,
                    unadmitted[other].end_byte,
                )
        });
        let across_boundary = query.patterns.iter().any(|pattern| {
            let parts = segments(pattern);
            let [.., before, last] = parts.as_slice() else {
                return false;
            };
            let name = declaration.name.to_lowercase();
            if !name.contains(last) {
                return false;
            }
            declaration
                .owner
                .as_deref()
                .is_some_and(|owner| owner.to_lowercase().contains(before))
        });
        matched[index] = by_name || inside_match || across_boundary;
    }
    unadmitted
        .iter()
        .zip(matched)
        .filter(|(declaration, matched)| *matched && query.admits_kind(declaration.kind))
        .map(|(declaration, _)| declaration)
        .collect()
}

/// Observe the declaration census for every file a name search covers.
///
/// `languages` narrows the files to the languages the search filtered on;
/// `None` covers every source file the graph's tree holds. `enclosing` is the
/// set of admitted entities whose own names the query matched, of any kind,
/// which is what places a nested unadmitted declaration inside a matching
/// owner. The reading comes from the graph alone: the tree inventory, each
/// file's certificate by id, and nothing read from disk.
pub fn observe<S: EntityStore>(
    store: &S,
    languages: Option<&[LanguageId]>,
    query: &NameQuery,
    enclosing: &[Entity],
) -> Value {
    observe_units(store, languages, query, enclosing).0
}

/// [`observe`], with the source units in scope whose parse recovered from an
/// error, for [`read_unread_regions`] to read.
pub(crate) fn observe_units<S: EntityStore>(
    store: &S,
    languages: Option<&[LanguageId]>,
    query: &NameQuery,
    enclosing: &[Entity],
) -> (Value, Vec<UnreadUnit>) {
    let mut unread: Vec<UnreadUnit> = Vec::new();
    let tree = match store.resolved_tree_snapshot() {
        Ok(Some(tree)) => tree,
        _ => {
            return (
                json!({
                    "state": STATE_UNMEASURED,
                    "reason": NO_INVENTORY,
                }),
                unread,
            );
        }
    };

    // Where a held declaration whose own name holds a pattern begins, by file.
    let mut held_starts: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for entity in enclosing {
        let Some(span) = entity.span.as_ref() else {
            continue;
        };
        let own = crate::handlers::entities::own_name(&entity.name).to_lowercase();
        if query
            .patterns
            .iter()
            .any(|pattern| !pattern.is_empty() && own.contains(pattern.as_str()))
        {
            held_starts
                .entry(span.file.0.as_str())
                .or_default()
                .push(span.start_byte);
        }
    }
    let mut enclosing_by_file: BTreeMap<&str, Vec<Enclosing>> = BTreeMap::new();
    for entity in enclosing {
        // A module or file entity spans its whole file without qualifying the
        // names declared in it, so it places nothing inside a match.
        if matches!(
            entity.kind,
            EntityKind::Module | EntityKind::Package | EntityKind::File | EntityKind::DocumentNode
        ) {
            continue;
        }
        let Some(span) = entity.span.as_ref() else {
            continue;
        };
        enclosing_by_file
            .entry(span.file.0.as_str())
            .or_default()
            .push(Enclosing {
                start: span.start_byte,
                end: span.end_byte,
            });
    }

    let mut files = 0usize;
    let mut seen_total = 0usize;
    let mut admitted_total = 0usize;
    let mut scope_languages: Vec<LanguageId> = Vec::new();
    let mut measured_languages: Vec<LanguageId> = Vec::new();
    let mut residue: Vec<Value> = Vec::new();
    let mut residue_total = 0usize;
    let mut unparsed: Vec<String> = Vec::new();
    let mut unparsed_total = 0usize;
    let mut unmeasured: Vec<Value> = Vec::new();
    let mut unmeasured_total = 0usize;
    let mut partly_parsed: Vec<String> = Vec::new();
    let mut partly_parsed_total = 0usize;
    let mut module_qualification_missing = false;
    let mut unmeasured_by_language: BTreeMap<String, usize> = BTreeMap::new();
    let mut files_by_language: BTreeMap<String, usize> = BTreeMap::new();

    for artifact in tree.artifacts_by_path() {
        let Some(path) = artifact.path.as_utf8() else {
            continue;
        };
        let Some(language) = kin_model::call_site_reading::language_of_path(path) else {
            continue;
        };
        if languages.is_some_and(|wanted| !wanted.contains(&language)) {
            continue;
        }
        if !matches!(
            kin_index::FileClassifier::classify(std::path::Path::new(path)),
            kin_index::FileClassification::EntitySource
        ) {
            continue;
        }
        let kin_model::TreeEntry::Blob { hash, .. } = artifact.entry else {
            continue;
        };
        files += 1;
        if !scope_languages.contains(&language) {
            scope_languages.push(language);
        }
        *files_by_language
            .entry(format!("{language:?}"))
            .or_default() += 1;

        let node = GraphNodeId::Artifact(artifact.artifact_id);
        let certificate = store
            .lookup_relation_by_id(&kin_index::parse_coverage_relation_id(artifact.artifact_id));
        let read = match certificate {
            Ok(RelationLookup::Present(relation)) => read_census(&relation, path, node, hash),
            Ok(RelationLookup::Absent) => Err(NO_CERTIFICATE),
            Ok(RelationLookup::Unavailable) | Err(_) => Err(LOOKUP_UNAVAILABLE),
        };
        match read {
            Ok(FileCensus {
                seen,
                admitted,
                unadmitted,
                parsed_whole,
                error_regions,
                error_spans,
            }) => {
                if !parsed_whole {
                    partly_parsed_total += 1;
                    if partly_parsed.len() < FILES_LISTED_MAX {
                        partly_parsed.push(path.to_string());
                    }
                    let scoped = query.in_file(path);
                    let mut accounted = held_starts.get(path).cloned().unwrap_or_default();
                    accounted.extend(
                        unadmitted
                            .iter()
                            .filter(|declaration| scoped.matches_text(&declaration.name))
                            .map(|declaration| declaration.start_byte),
                    );
                    unread.push(UnreadUnit {
                        path: path.to_string(),
                        digest: hash,
                        regions: error_regions,
                        spans: error_spans,
                        accounted,
                    });
                }
                if query.module_context_missing(path) {
                    module_qualification_missing = true;
                    unmeasured_total += 1;
                    if unmeasured.len() < FILES_LISTED_MAX {
                        unmeasured
                            .push(json!({"file": path, "reason": MODULE_QUALIFICATION_UNRECORDED}));
                    }
                }
                seen_total += seen;
                admitted_total += admitted;
                if !measured_languages.contains(&language) {
                    measured_languages.push(language);
                }
                let outer = enclosing_by_file
                    .get(path)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                for declaration in matching(&query.in_file(path), &unadmitted, outer) {
                    residue_total += 1;
                    if residue.len() < LISTED_MAX {
                        let mut row = json!({
                            "name": declaration.name,
                            "kind": declaration.kind.label(),
                            "reason": declaration.reason,
                            "file": path,
                            "line": crate::handlers::common::presentation_line(declaration.line),
                        });
                        if let Some(owner) = &declaration.owner {
                            row["owner"] = json!(owner);
                        }
                        residue.push(row);
                    }
                }
            }
            Err(NO_CERTIFICATE) if held_opaque(store, path, hash) => {
                unparsed_total += 1;
                if unparsed.len() < FILES_LISTED_MAX {
                    unparsed.push(path.to_string());
                }
            }
            Err(reason) => {
                unmeasured_total += 1;
                *unmeasured_by_language
                    .entry(format!("{language:?}"))
                    .or_default() += 1;
                if unmeasured.len() < FILES_LISTED_MAX {
                    unmeasured.push(json!({"file": path, "reason": reason}));
                }
            }
        }
    }

    // Every admitted entity the name matched must sit in a file the census
    // covered. One that does not came from a file the tree does not hold, and a
    // census over the tree says nothing about what else that file declares.
    let mut outside: Vec<&str> = enclosing
        .iter()
        .filter(|entity| languages.is_none_or(|wanted| wanted.contains(&entity.language)))
        .filter_map(|entity| {
            entity
                .span
                .as_ref()
                .map(|span| span.file.0.as_str())
                .or(entity.file_origin.as_ref().map(|file| file.0.as_str()))
        })
        .filter(|file| {
            kin_model::RepoPath::from_utf8(*file)
                .ok()
                .and_then(|path| tree.artifact_id_at_path(&path))
                .is_none()
        })
        .collect();
    outside.sort_unstable();
    outside.dedup();
    for file in outside {
        unmeasured_total += 1;
        if unmeasured.len() < FILES_LISTED_MAX {
            unmeasured.push(json!({"file": file, "reason": ENTITY_OUTSIDE_TREE}));
        }
    }
    if files == 0 && unmeasured_total == 0 {
        return (
            json!({
                "state": STATE_UNMEASURED,
                "reason": NO_FILES,
                "files": 0,
            }),
            unread,
        );
    }

    let state = if unmeasured_total > 0 {
        STATE_UNMEASURED
    } else if residue_total > 0 || unparsed_total > 0 {
        STATE_COMPLETE_EXCEPT
    } else {
        STATE_COMPLETE
    };
    let mut reading = json!({
        "state": state,
        "files": files,
        "declarations_seen": seen_total,
        "declarations_admitted": admitted_total,
        PARSED_KEY: if partly_parsed_total > 0 { PARSED_IN_PART } else { PARSED_WHOLE },
    });
    if residue_total > 0 {
        reading["matching_unadmitted"] = Value::Array(residue);
        reading["matching_unadmitted_total"] = json!(residue_total);
    }
    if unparsed_total > 0 {
        reading["unparsed_files"] = json!(unparsed);
        reading["unparsed_files_total"] = json!(unparsed_total);
    }
    if partly_parsed_total > 0 {
        reading[PARSED_IN_PART_COUNT_KEY] = json!(partly_parsed_total);
        reading[PARTLY_PARSED_FILES_KEY] = json!(partly_parsed);
    }
    if unmeasured_total > 0 {
        if module_qualification_missing {
            reading["reason"] = json!(MODULE_QUALIFICATION_UNRECORDED);
        }
        reading["unmeasured_files"] = Value::Array(unmeasured);
        reading["unmeasured_files_total"] = json!(unmeasured_total);
        // A language every file of which lacks a census is one whose adapter
        // takes none, which a reader fixes differently from one stale file.
        let whole: Vec<&String> = unmeasured_by_language
            .iter()
            .filter(|(language, count)| files_by_language.get(*language) == Some(count))
            .map(|(language, _)| language)
            .collect();
        if !whole.is_empty() {
            reading["unmeasured_languages"] = json!(whole);
        }
    }
    (reading, unread)
}

/// Bytes of unit text the process keeps, lowercased, by content digest, so
/// the units a repository holds partly parsed are read from held content once
/// and not on every lookup. Past the bound a unit is read each time.
const UNREAD_TEXT_CACHE_BYTES: usize = 32 * 1024 * 1024;

type UnreadTextCache = std::sync::Mutex<(
    std::collections::HashMap<kin_model::Hash256, std::sync::Arc<Vec<u8>>>,
    usize,
)>;

fn unread_text_cache() -> &'static UnreadTextCache {
    static CACHE: std::sync::OnceLock<UnreadTextCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The byte ranges of `len` bytes no parse read: the listed error regions,
/// each widened to whole lines, or all of it when the certificate lists fewer
/// regions than the parse recovered from.
///
/// A region is read from the start of its first line to the end of its last
/// because a parse that recovers by supplying a token it expected leaves a
/// region of no width, and the declaration it then read as something else
/// begins on that line.
fn unread_ranges(text: &[u8], unit: &UnreadUnit) -> Vec<(usize, usize)> {
    if unit.spans.is_empty() || unit.spans.len() < unit.regions {
        return vec![(0, text.len())];
    }
    unit.spans
        .iter()
        .map(|&(start, end)| {
            let start = start.min(text.len());
            let end = end.clamp(start, text.len());
            let line_start = text[..start]
                .iter()
                .rposition(|&byte| byte == b'\n')
                .map_or(0, |at| at + 1);
            let line_end = text[end..]
                .iter()
                .position(|&byte| byte == b'\n')
                .map_or(text.len(), |at| end + at);
            (line_start, line_end)
        })
        .collect()
}

/// Whether `pattern` is spelled in what no parse read of `text`, other than
/// as the name of a declaration this answer already holds or lists.
///
/// A spelling is that declaration's own when it follows the declaration's
/// start with nothing between that ends a declaration's head: no `(`, `{`,
/// `;` or `=`. `text` is lowercased ASCII; a pattern outside ASCII is matched
/// against the text lowercased as a whole, with nothing accounted, since
/// offsets do not survive that.
fn spelled_unaccounted(text: &[u8], unit: &UnreadUnit, pattern: &str) -> bool {
    if !pattern.is_ascii() {
        return unread_ranges(text, unit).iter().any(|&(start, end)| {
            String::from_utf8_lossy(&text[start..end])
                .to_lowercase()
                .contains(pattern)
        });
    }
    let needle = pattern.as_bytes();
    let own = |at: usize| {
        unit.accounted.iter().any(|&start| {
            start <= at
                && !text[start..at]
                    .iter()
                    .any(|byte| matches!(byte, b'(' | b'{' | b';' | b'='))
        })
    };
    unread_ranges(text, unit).iter().any(|&(start, end)| {
        text[start..end]
            .windows(needle.len())
            .enumerate()
            .any(|(offset, window)| window == needle && !own(start + offset))
    })
}

/// Read what no parse could read of the partly parsed units in scope, from
/// held content, and say whether any of it spells the name asked for.
///
/// A declaration the grammar could not show has to spell its name inside the
/// region the parse did not read, so a name spelled in no such region is
/// declared in none, and an answer may then say its rows and listed
/// declarations are all there are. `text_of` gives a unit's held bytes by
/// content digest, never a file read; a unit it cannot give is counted as not
/// read. Matching is the census's own: the lowercased text holds the
/// lowercased pattern.
pub(crate) fn read_unread_regions<G: kin_model::GraphStore + ?Sized>(
    reading: &mut Value,
    store: &G,
    pattern: &str,
    units: &[UnreadUnit],
    text_of: &dyn Fn(kin_model::Hash256) -> Option<std::sync::Arc<Vec<u8>>>,
) {
    let pattern = normalize(pattern);
    if units.is_empty() || pattern.is_empty() {
        return;
    }
    let mut spelled: Vec<&UnreadUnit> = Vec::new();
    let mut not_read = 0usize;
    for unit in units {
        let cached = unread_text_cache()
            .lock()
            .ok()
            .and_then(|cache| cache.0.get(&unit.digest).cloned());
        let text = match cached {
            Some(text) => text,
            None => {
                let Some(bytes) = text_of(unit.digest) else {
                    not_read += 1;
                    continue;
                };
                let text = std::sync::Arc::new(bytes.to_ascii_lowercase());
                if let Ok(mut cache) = unread_text_cache().lock() {
                    let (held, held_bytes) = &mut *cache;
                    if *held_bytes + text.len() <= UNREAD_TEXT_CACHE_BYTES
                        && held.insert(unit.digest, text.clone()).is_none()
                    {
                        *held_bytes += text.len();
                    }
                }
                text
            }
        };
        if spelled_unaccounted(&text, unit, &pattern) {
            spelled.push(unit);
        }
    }
    reading[UNREAD_REGIONS_KEY] = json!(if !spelled.is_empty() {
        UNREAD_SPELL_NAME
    } else if not_read > 0 {
        UNREAD_NOT_READ
    } else {
        UNREAD_CLEAR
    });
    if !spelled.is_empty() {
        reading[UNREAD_SPELLING_COUNT_KEY] = json!(spelled.len());
        reading[UNREAD_SPELLED_IN_KEY] = Value::Array(
            spelled
                .iter()
                .take(FILES_LISTED_MAX)
                .map(|unit| {
                    let mut row = json!({"file": unit.path});
                    if let Some(module) = crate::semantic_owners::file_module(store, &unit.path) {
                        row["module"] = json!(module);
                    }
                    row
                })
                .collect(),
        );
    }
    if not_read > 0 {
        reading[UNREAD_NOT_READ_COUNT_KEY] = json!(not_read);
    }
}

/// Name where each listed unadmitted declaration sits, from graph records
/// alone: the dotted module of its file, and the entity that encloses it
/// (`inside_id`) when exactly one entity of its owner's name is declared in
/// that file around its line. Never a path: a declaration whose module the
/// records cannot name carries none.
pub(crate) fn place_residue<G: kin_model::GraphStore + ?Sized>(reading: &mut Value, store: &G) {
    let Some(rows) = reading
        .get_mut("matching_unadmitted")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    let mut modules: BTreeMap<String, Option<String>> = BTreeMap::new();
    for row in rows.iter_mut() {
        let Some(file) = row.get("file").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        let module = modules
            .entry(file.clone())
            .or_insert_with(|| crate::semantic_owners::file_module(store, &file))
            .clone();
        if let Some(module) = module {
            row["module"] = json!(module);
        }
        let Some(owner) = row.get("owner").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        let line = row.get("line").and_then(Value::as_u64);
        if let Some(id) = enclosing_entity(store, &file, &owner, line) {
            row["inside_id"] = json!(id.to_string());
        }
    }
}

/// The census reason for a top-level assignment Kin keeps no entity for.
const TOP_LEVEL_ASSIGNMENT: &str = "module_binding_filtered";

/// Name each listed top-level assignment that only re-exports another
/// module's member of the same name, such as `export const X = config.X;`,
/// as `reexport_of: config.X`. Its own line is read through `text_of`, the
/// recorded text of the module surface it sits in; a line that cannot be read
/// or that does anything else is left as it is.
pub(crate) fn name_reexports<G: kin_model::GraphStore + ?Sized>(
    reading: &mut Value,
    store: &G,
    text_of: &dyn Fn(&kin_model::Entity) -> Option<std::sync::Arc<Vec<u8>>>,
) {
    let Some(rows) = reading
        .get_mut("matching_unadmitted")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    let mut texts: BTreeMap<String, Option<std::sync::Arc<Vec<u8>>>> = BTreeMap::new();
    for row in rows.iter_mut() {
        if row.get("reason").and_then(Value::as_str) != Some(TOP_LEVEL_ASSIGNMENT)
            || row.get("owner").is_some_and(|owner| !owner.is_null())
        {
            continue;
        }
        let (Some(file), Some(line), Some(name)) = (
            row.get("file").and_then(Value::as_str).map(str::to_string),
            row.get("line").and_then(Value::as_u64),
            row.get("name").and_then(Value::as_str).map(str::to_string),
        ) else {
            continue;
        };
        let text = texts
            .entry(file.clone())
            .or_insert_with(|| {
                crate::semantic_owners::file_surface_module(store, &file)
                    .and_then(|module| text_of(&module))
            })
            .clone();
        let Some(text) = text else {
            continue;
        };
        let Some(source) = String::from_utf8_lossy(&text)
            .lines()
            .nth(line.saturating_sub(1) as usize)
            .and_then(|line| reexport_source(line, &name))
        else {
            continue;
        };
        row["reexport_of"] = json!(source);
    }
}

/// The dotted member a line re-exports under `name`, when the line is
/// exactly an assignment of `name` to another module's member of that name.
fn reexport_source(line: &str, name: &str) -> Option<String> {
    let mut rest = line.trim();
    rest = rest.strip_prefix("export ").unwrap_or(rest).trim_start();
    for keyword in ["const ", "let ", "var "] {
        if let Some(after) = rest.strip_prefix(keyword) {
            rest = after.trim_start();
            break;
        }
    }
    let rest = rest.strip_prefix(name)?.trim_start();
    let rhs = rest.strip_prefix('=')?.trim();
    let rhs = rhs.strip_suffix(';').unwrap_or(rhs).trim_end();
    let (head, last) = rhs.rsplit_once('.')?;
    (last == name
        && !head.is_empty()
        && rhs
            .chars()
            .all(|c| c == '.' || c == '_' || c == '$' || c.is_alphanumeric()))
    .then(|| rhs.to_string())
}

/// The one entity named `owner` (or ending in `.owner`) declared in `file`
/// whose span holds `line`, when there is exactly one.
fn enclosing_entity<G: kin_model::GraphStore + ?Sized>(
    store: &G,
    file: &str,
    owner: &str,
    line: Option<u64>,
) -> Option<kin_model::EntityId> {
    let leaf = owner.rsplit('.').next().unwrap_or(owner);
    let page = store
        .query_entities_page(
            &kin_model::graph::EntityFilter {
                name_pattern: Some(leaf.to_string()),
                file_path: Some(kin_model::FilePathId::new(file)),
                ..Default::default()
            },
            &kin_model::graph::EntityPage::first(64),
        )
        .ok()?;
    let holds = |entity: &kin_model::Entity| {
        let Some(span) = entity.span.as_ref() else {
            return line.is_none();
        };
        // The census line is 1-based; spans count from 0.
        line.is_none_or(|line| {
            let line = line.saturating_sub(1) as u32;
            span.start_line <= line && line <= span.end_line
        })
    };
    let mut found = page.entities.into_iter().filter(|entity| {
        let name = entity.name.replace(".<locals>.", ".");
        (name == owner || name.ends_with(&format!(".{owner}")) || entity.name == leaf)
            && !matches!(
                entity.kind,
                kin_model::EntityKind::Module
                    | kin_model::EntityKind::Package
                    | kin_model::EntityKind::File
            )
            && holds(entity)
    });
    let first = found.next()?;
    found.next().is_none().then_some(first.id)
}

/// Whether the graph holds `path` as bytes it never parsed, at this content.
fn held_opaque<S: EntityStore>(store: &S, path: &str, blob: kin_model::Hash256) -> bool {
    store
        .get_opaque_artifact(&FilePathId::new(path))
        .ok()
        .flatten()
        .is_some_and(|opaque| opaque.content_hash == blob)
}

/// Publish a census reading on an `edge_coverage` observation: the class state
/// under `classes` and the reading beside it.
pub fn attach(observation: &mut Value, reading: Value) {
    let state = reading
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or(STATE_UNMEASURED)
        .to_string();
    if let Some(classes) = observation
        .get_mut("classes")
        .and_then(Value::as_object_mut)
    {
        classes.insert(DECLARATIONS_CLASS.to_string(), json!(state));
    } else {
        observation["classes"] = json!({ DECLARATIONS_CLASS: state });
    }
    observation[DECLARATIONS_KEY] = reading;
}

/// The declarations class state an observation carries, when it carries one.
pub fn class_state(coverage: &serde_json::Map<String, Value>) -> Option<&str> {
    coverage
        .get("classes")
        .and_then(Value::as_object)
        .and_then(|classes| classes.get(DECLARATIONS_CLASS))
        .and_then(Value::as_str)
}

/// The verdict clauses a census reading carries: none when complete, the
/// residue clauses when complete except a listed residue, and the unmeasured
/// clause otherwise. `None` when the observation carries no census reading.
pub fn clauses(coverage: &serde_json::Map<String, Value>) -> Option<Vec<String>> {
    let state = class_state(coverage)?;
    let reading = coverage.get(DECLARATIONS_KEY).and_then(Value::as_object);
    let count = |key: &str| {
        reading
            .and_then(|reading| reading.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let files = count("files");
    let mut clauses = Vec::new();
    match state {
        STATE_COMPLETE => {}
        STATE_COMPLETE_EXCEPT => {
            let unadmitted = count("matching_unadmitted_total");
            if unadmitted > 0 {
                clauses.push(format!(
                    "{UNADMITTED_LISTED_CODE}: {unadmitted} declaration{} the grammar shows in the \
                     {files} source units this search covers {} not admitted as entities and could match \
                     it, and each is listed under edge_coverage.declarations.matching_unadmitted, \
                     so the rows are complete except those",
                    plural(unadmitted),
                    if unadmitted == 1 { "was" } else { "were" },
                ));
            }
            let unparsed = count("unparsed_files_total");
            if unparsed > 0 {
                clauses.push(format!(
                    "{UNPARSED_FILES_LISTED_CODE}: {unparsed} source unit{} this search covers {} \
                     held as bytes and never parsed, listed under \
                     edge_coverage.declarations.unparsed_files, so the rows are complete except \
                     what those units declare",
                    plural(unparsed),
                    if unparsed == 1 { "is" } else { "are" },
                ));
            }
        }
        _ => {
            let unmeasured = count("unmeasured_files_total");
            let languages = reading
                .and_then(|reading| reading.get("unmeasured_languages"))
                .and_then(Value::as_array)
                .map(|languages| {
                    languages
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|languages| !languages.is_empty());
            let reason = reading
                .and_then(|reading| reading.get("reason"))
                .and_then(Value::as_str);
            clauses.push(match (reason, languages) {
                (Some(MODULE_QUALIFICATION_UNRECORDED), _) => format!(
                    "{CENSUS_UNMEASURED_CODE}: recorded module identity is missing for source units \
                     matching the qualified query, so declaration counts alone cannot certify \
                     the matching rows or absence. Qualification and any additional census \
                     limitations are listed under edge_coverage.declarations.unmeasured_files"
                ),
                (Some(reason), _) => format!(
                    "{CENSUS_UNMEASURED_CODE}: the declaration census could not be read \
                     ({reason}), so a declaration the extractor did not admit could match this \
                     search and is not listed"
                ),
                (None, Some(languages)) => format!(
                    "{CENSUS_UNMEASURED_CODE}: {unmeasured} of the {files} source units this search \
                     covers carry no declaration census, including every {languages} unit, whose \
                     adapter takes none, so a declaration the extractor did not admit there could \
                     match and is not listed"
                ),
                (None, None) => format!(
                    "{CENSUS_UNMEASURED_CODE}: {unmeasured} of the {files} source units this search \
                     covers carry no current declaration census, listed under \
                     edge_coverage.declarations.unmeasured_files, so a declaration the extractor \
                     did not admit there could match and is not listed"
                ),
            });
        }
    }
    Some(clauses)
}

fn plural(count: u64) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unadmitted(name: &str, owner: Option<&str>, start: usize, end: usize) -> Unadmitted {
        Unadmitted {
            name: name.into(),
            kind: DeclarationKind::Function,
            reason: "nested_in_callable".into(),
            owner: owner.map(str::to_string),
            start_byte: start,
            end_byte: end,
            line: 0,
        }
    }

    fn names(found: Vec<&Unadmitted>) -> Vec<&str> {
        found
            .into_iter()
            .map(|declaration| declaration.name.as_str())
            .collect()
    }

    #[test]
    fn a_declaration_matches_by_name_owner_or_a_matching_enclosure() {
        let file = [
            unadmitted("pop_helper", Some("outer"), 10, 20),
            unadmitted("inner", Some("Store"), 30, 40),
            unadmitted("deep", Some("inner"), 32, 38),
            unadmitted("other", None, 50, 60),
            unadmitted("nested", Some("method"), 72, 78),
        ];
        let query = NameQuery::new("pop", None);
        assert_eq!(names(matching(&query, &file, &[])), vec!["pop_helper"]);

        // `Store.inner` holds the pattern, and `deep` sits inside it.
        let query = NameQuery::new("store", None);
        assert_eq!(names(matching(&query, &file, &[])), vec!["inner", "deep"]);

        // Inside an admitted `Store.method` the query matched.
        let enclosing = [Enclosing { start: 70, end: 80 }];
        assert_eq!(
            names(matching(&query, &file, &enclosing)),
            vec!["inner", "deep", "nested"]
        );

        // A dotted pattern crossing into the declaration's own name, with the
        // owner's own owners unknown to the census.
        let query = NameQuery::new("method.nest", None);
        assert_eq!(names(matching(&query, &file, &[])), vec!["nested"]);
        let query = NameQuery::new("Store.method.nested", None);
        assert_eq!(names(matching(&query, &file, &[])), vec!["nested"]);
        let query = NameQuery::new("Other.nested", None);
        assert!(matching(&query, &file, &[]).is_empty());
    }

    #[test]
    fn a_kind_filter_keeps_only_declarations_that_could_be_admitted_as_it() {
        let file = [
            Unadmitted {
                kind: DeclarationKind::Attribute,
                ..unadmitted("pop", Some("Box"), 0, 5)
            },
            unadmitted("pop", Some("f"), 10, 15),
        ];
        let query = NameQuery::new("pop", Some(&[EntityKind::Class]));
        assert!(matching(&query, &file, &[]).is_empty());
        let query = NameQuery::new("pop", Some(&[EntityKind::Field]));
        assert_eq!(matching(&query, &file, &[]).len(), 1);
        let query = NameQuery::new("*pop", Some(&[EntityKind::Function]));
        assert_eq!(matching(&query, &file, &[]).len(), 1);
    }

    fn unit(regions: usize, spans: &[(usize, usize)], accounted: &[usize]) -> UnreadUnit {
        UnreadUnit {
            path: "src/unit.js".to_string(),
            digest: kin_model::Hash256::from_bytes([7; 32]),
            regions,
            spans: spans.to_vec(),
            accounted: accounted.to_vec(),
        }
    }

    /// What no parse read is each listed error region, widened to whole
    /// lines: a region of no width, left where the parse supplied a token it
    /// expected, still gives the line the unread declaration begins on. A
    /// certificate that lists fewer regions than the parse recovered from
    /// gives the whole unit.
    #[test]
    fn unread_ranges_are_the_error_regions_by_whole_lines_or_the_whole_unit() {
        let text = b"function kept() {}\ncomponent hidden(props) {\n  return null;\n}\nfunction also() {}\n";
        let at = |needle: &str| {
            let start = text
                .windows(needle.len())
                .position(|window| window == needle.as_bytes())
                .unwrap();
            (start, start + needle.len())
        };
        let read = |unit: &UnreadUnit| -> Vec<String> {
            unread_ranges(text, unit)
                .into_iter()
                .map(|(start, end)| String::from_utf8_lossy(&text[start..end]).into_owned())
                .collect()
        };
        assert_eq!(
            read(&unit(1, &[at("hidden")], &[])),
            ["component hidden(props) {"]
        );
        let start = at("component").0;
        assert_eq!(
            read(&unit(1, &[(start, start)], &[])),
            ["component hidden(props) {"]
        );
        assert_eq!(
            read(&unit(1, &[(at("hidden").0, at("null").1)], &[])),
            ["component hidden(props) {\n  return null;"]
        );
        for whole in [unit(17, &[at("hidden")], &[]), unit(1, &[], &[])] {
            assert_eq!(read(&whole), [String::from_utf8_lossy(text)]);
        }
        assert_eq!(read(&unit(1, &[(4000, 4010)], &[])), [""]);
    }

    /// A spelling of the name in what no parse read counts, except the name
    /// of a declaration the answer holds or lists, at that declaration's head.
    #[test]
    fn a_held_declarations_own_spelling_is_accounted_and_any_other_is_not() {
        let text = b"export function measure(a) {\n  return measure(a);\n}\nconst measure2 = 1;\n";
        let whole = |accounted: &[usize]| unit(9, &[], accounted);
        // Nothing accounted: the declaration's own spelling counts.
        assert!(spelled_unaccounted(text, &whole(&[]), "measure"));
        // Its declaration accounted, the call inside it still counts.
        assert!(spelled_unaccounted(text, &whole(&[0]), "measure"));
        let call = text
            .windows(7)
            .position(|window| window == b"return ")
            .unwrap();
        let only_head = unit(1, &[(0, 10)], &[0]);
        assert!(!spelled_unaccounted(text, &only_head, "measure"));
        let call_line = unit(1, &[(call, call + 6)], &[0]);
        assert!(spelled_unaccounted(text, &call_line, "measure"));
        // A start past a head's `(` accounts for nothing after it.
        let opened = text.iter().position(|&byte| byte == b'(').unwrap();
        assert!(spelled_unaccounted(
            text,
            &unit(1, &[(opened, opened + 1)], &[0]),
            "a"
        ));
        // A pattern outside ASCII is read whole, and accounts for nothing.
        assert!(!spelled_unaccounted(text, &whole(&[0]), "mésure"));
    }

    #[test]
    fn clauses_follow_the_state() {
        let mut observation = json!({"classes": {}});
        attach(
            &mut observation,
            json!({"state": STATE_COMPLETE, "files": 3}),
        );
        let map = observation.as_object().unwrap();
        assert_eq!(class_state(map), Some(STATE_COMPLETE));
        assert_eq!(clauses(map), Some(vec![]));

        attach(
            &mut observation,
            json!({"state": STATE_COMPLETE_EXCEPT, "files": 3,
                   "matching_unadmitted_total": 2, "unparsed_files_total": 1}),
        );
        let found = clauses(observation.as_object().unwrap()).unwrap();
        assert_eq!(found.len(), 2);
        assert!(found[0].starts_with(UNADMITTED_LISTED_CODE));
        assert!(found[1].starts_with(UNPARSED_FILES_LISTED_CODE));

        attach(
            &mut observation,
            json!({"state": STATE_UNMEASURED, "files": 3, "unmeasured_files_total": 3,
                   "unmeasured_languages": ["Rust"]}),
        );
        let found = clauses(observation.as_object().unwrap()).unwrap();
        assert!(found[0].starts_with(CENSUS_UNMEASURED_CODE) && found[0].contains("Rust"));
        for clause in found {
            assert!(!clause.contains(crate::verdict::CLAUSE_SEPARATOR));
        }
    }
}

/// Fixtures that run real adapters over small files, admit them into a graph
/// with their certificates, and read the census back through the search
/// handler and the one verdict.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::collections::HashMap;

    use kin_db::InMemoryGraph;
    use kin_model::graph::EntityStore;
    use kin_model::{
        ArtifactId, EntityKind, FilePathId, LanguageId, LocatedEntry, RepoPath, TransactionDelta,
        TreeDelta, TreeEntry,
    };
    use serde_json::{json, Value};

    use crate::types::{ContentBlock, ToolCallResult};

    pub(crate) const PYTHON: &str = "\
class Store:
    limit = 3

    def pop(self):
        def pop_inner():
            return 1
        return pop_inner()


def helper():
    return Store().pop()


if True:
    def guarded_pop():
        pass


if CHOOSE_FIRST:
    def pop_unadmitted():
        return 1
else:
    def pop_unadmitted():
        return 2
";

    pub(crate) const JAVASCRIPT: &str = "\
export class Store {
  pop() { return 1; }
}
export function helper() {
  function popInner() { return 2; }
  return popInner();
}
export const api = { popItem() { return 3; } };
";

    pub(crate) const TYPESCRIPT: &str = "\
export interface Queue { popFront(): number }
export class Store {
  pop(): number { return 1; }
}
export function helper(): number {
  const popLocal = () => 2;
  return popLocal();
}
";

    /// Parse `source` with its real adapter, admit it at `path` and store its
    /// entities and coverage certificate, which records the parse as the
    /// adapter found it: whole, or recovered from an error region. `stale`
    /// binds the certificate to other bytes, as a file changed since its
    /// derivation would be.
    pub(crate) fn admit(graph: &InMemoryGraph, path: &str, source: &str, stale: bool) {
        admit_with_census(graph, path, source, stale, true);
    }

    /// `with_census = false` explicitly models a stored certificate from an
    /// adapter that did not measure declarations, independent of current adapters.
    fn admit_with_census(
        graph: &InMemoryGraph,
        path: &str,
        source: &str,
        stale: bool,
        with_census: bool,
    ) {
        let bytes = source.as_bytes();
        let blob = kin_blobs::digest(bytes);
        let indexed = kin_index::IndexPipeline::new()
            .index_file_content_with_tests(&FilePathId::new(path), bytes, blob)
            .unwrap()
            .indexed_file;
        let artifact = ArtifactId::new();
        graph
            .apply_transaction_delta(&TransactionDelta {
                tree_deltas: vec![TreeDelta::Added {
                    artifact_id: artifact,
                    new: LocatedEntry::new(
                        RepoPath::from_utf8(path).unwrap(),
                        TreeEntry::blob(kin_model::Hash256::from_bytes(blob.0), false),
                    ),
                }],
                ..Default::default()
            })
            .unwrap();
        for entity in &indexed.entities {
            graph.upsert_entity(entity).unwrap();
        }
        let mut certificate = kin_index::build_parse_coverage_relation(
            &kin_index::FileParseData {
                file_path: path.to_string(),
                entities: indexed.entities.clone(),
                relations: indexed.extracted_relations.clone(),
                imports: indexed.imports.clone(),
            },
            artifact,
            &indexed.file_layout.parse_completeness,
            &std::collections::HashSet::<String>::new(),
        );
        if stale {
            kin_index::bind_parse_coverage_source(
                &mut certificate,
                path,
                kin_model::Hash256::from_bytes([9; 32]),
            );
        }
        if !with_census {
            certificate.evidence.retain(|entry| {
                !matches!(
                    entry.parser_rule.as_deref(),
                    Some(super::DECLARATION_COVERAGE_V1 | super::UNADMITTED_DECLARATION_V1)
                )
            });
        }
        graph.upsert_relation(&certificate).unwrap();
    }

    pub(crate) fn search(graph: &InMemoryGraph, query: &str, kind: Option<&str>) -> Value {
        let mut args: HashMap<String, Value> = HashMap::new();
        args.insert("query".into(), json!(query));
        if let Some(kind) = kind {
            args.insert("kind".into(), json!(kind));
        }
        let result = crate::handlers::entities::handle_semantic_search(&args, graph).unwrap();
        finalized(graph, result)
    }

    /// [`search`] for a name, with the content of `held` sources readable by
    /// digest, as a request's source authority gives it. An in-memory graph
    /// keeps no content, so a test hands the search the bytes it admitted.
    pub(crate) fn search_holding(graph: &InMemoryGraph, query: &str, held: &[&str]) -> Value {
        let content: HashMap<kin_model::Hash256, std::sync::Arc<Vec<u8>>> = held
            .iter()
            .map(|source| {
                let bytes = source.as_bytes().to_vec();
                (
                    kin_model::Hash256::from_bytes(kin_blobs::digest(&bytes).0),
                    std::sync::Arc::new(bytes),
                )
            })
            .collect();
        let args = HashMap::from([("query".to_string(), json!(query))]);
        let result = crate::handlers::entities::search_names(
            &args,
            graph,
            None,
            crate::handlers::common::EntitySourceScope::WorkspaceHead,
            Some(&|digest| content.get(&digest).cloned()),
        )
        .unwrap();
        finalized(graph, result)
    }

    fn finalized(graph: &InMemoryGraph, result: ToolCallResult) -> Value {
        let envelope = crate::envelope::Envelope::daemon().with_health(&json!({
            "graph_loaded": true,
            "initialized": true,
            "graph_entity_count": graph.list_all_entities().unwrap().len(),
            "graph_generation": 1,
        }));
        let finalized = crate::envelope::finalize_bounded(
            result,
            envelope,
            "semantic_search",
            &crate::budget::ResponseBudget::default(),
        );
        text_value(&finalized)
    }

    fn text_value(result: &ToolCallResult) -> Value {
        let ContentBlock::Text { text } = result.content.first().expect("one block");
        serde_json::from_str(text).unwrap()
    }

    fn residue_names(reply: &Value) -> Vec<String> {
        reply["edge_coverage"]["declarations"]["matching_unadmitted"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|row| row["name"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn names(reply: &Value) -> Vec<String> {
        let mut names: Vec<String> = reply["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap().to_string())
            .collect();
        names.sort();
        names
    }

    fn state(reply: &Value) -> (&str, Option<&str>, bool) {
        let verdict = &reply["_kin"]["verdict"];
        (
            verdict["state"].as_str().unwrap(),
            verdict["limiting_factor"].as_str(),
            verdict["safe_to_conclude_absent"].as_bool().unwrap(),
        )
    }

    struct Case {
        language: LanguageId,
        path: &'static str,
        source: &'static str,
        certify: &'static str,
        certify_rows: &'static [&'static str],
        except_rows: &'static [&'static str],
        except_residue: &'static [&'static str],
        empty_except: &'static str,
        empty_except_residue: &'static [&'static str],
    }

    const CASES: [Case; 3] = [
        Case {
            language: LanguageId::Python,
            path: "app/store.py",
            source: PYTHON,
            certify: "helper",
            certify_rows: &["helper"],
            // Single guarded declarations are now admitted. Duplicate branch
            // declarations retain the ambiguity that this residue control needs.
            except_rows: &["Store.pop", "Store.pop.<locals>.pop_inner", "guarded_pop"],
            except_residue: &["pop_unadmitted", "pop_unadmitted"],
            empty_except: "pop_unadmitted",
            empty_except_residue: &["pop_unadmitted", "pop_unadmitted"],
        },
        Case {
            language: LanguageId::JavaScript,
            path: "src/store.js",
            source: JAVASCRIPT,
            certify: "Store",
            certify_rows: &["Store", "Store.pop", "store"],
            except_rows: &["Store.pop"],
            except_residue: &["popInner", "popItem"],
            empty_except: "popItem",
            empty_except_residue: &["popItem"],
        },
        Case {
            language: LanguageId::TypeScript,
            path: "src/store.ts",
            source: TYPESCRIPT,
            certify: "Queue",
            certify_rows: &["Queue", "Queue.popFront"],
            except_rows: &["Queue.popFront", "Store.pop"],
            except_residue: &["popLocal"],
            empty_except: "popLocal",
            empty_except_residue: &["popLocal"],
        },
    ];

    /// For each language with a census: a search whose name nothing unadmitted
    /// could carry certifies its rows, one an unadmitted declaration could
    /// carry is complete except that declaration, an empty search for a name
    /// only an unadmitted declaration carries is complete except it and never
    /// an absence, and an empty search for a name nothing carries certifies the
    /// absence. No language server is installed, which the census does not need.
    #[test]
    fn name_search_certifies_or_lists_the_unadmitted_residue_per_language() {
        let _host = crate::edge_coverage::test_support::scoped_language_servers(&[]);
        for case in CASES {
            let graph = InMemoryGraph::new();
            admit(&graph, case.path, case.source, false);
            let label = format!("{:?}", case.language);

            let reply = search(&graph, case.certify, None);
            assert_eq!(names(&reply), case.certify_rows, "{label}: {reply}");
            assert_eq!(
                reply["edge_coverage"]["classes"]["declarations"], "complete",
                "{label}: {reply}"
            );
            assert_eq!(
                state(&reply),
                ("certified", None, false),
                "{label}: {reply}"
            );

            let reply = search(&graph, "pop", None);
            assert_eq!(names(&reply), case.except_rows, "{label}: {reply}");
            assert_eq!(
                residue_names(&reply),
                case.except_residue,
                "{label}: {reply}"
            );
            assert_eq!(
                state(&reply),
                (
                    "certified_except",
                    Some(super::UNADMITTED_LISTED_CODE),
                    false
                ),
                "{label}: {reply}"
            );

            let reply = search(&graph, case.empty_except, None);
            assert!(names(&reply).is_empty(), "{label}: {reply}");
            assert_eq!(
                residue_names(&reply),
                case.empty_except_residue,
                "{label}: {reply}"
            );
            assert_eq!(
                state(&reply),
                (
                    "certified_except",
                    Some(super::UNADMITTED_LISTED_CODE),
                    false
                ),
                "{label}: an absence with a listed residue is never licensed: {reply}"
            );

            let reply = search(&graph, "nothingNamedThis", None);
            assert!(names(&reply).is_empty(), "{label}: {reply}");
            assert_eq!(
                reply["edge_coverage"]["declarations"]["state"], "complete",
                "{label}: {reply}"
            );
            assert_eq!(state(&reply), ("certified", None, true), "{label}: {reply}");
            assert_eq!(reply["negative"]["safe_to_conclude_absent"], true);
        }
    }

    /// A kind filter narrows residue and admitted rows alike: the independently
    /// held class attribute `limit` is a field, never a class.
    #[test]
    fn a_kind_filter_narrows_the_residue() {
        let _host = crate::edge_coverage::test_support::scoped_language_servers(&[]);
        let graph = InMemoryGraph::new();
        admit(&graph, "app/store.py", PYTHON, false);
        let reply = search(&graph, "pop", Some("method"));
        assert_eq!(names(&reply), ["Store.pop"]);
        assert_eq!(
            residue_names(&reply),
            ["pop_unadmitted", "pop_unadmitted"],
            "{reply}"
        );
        let reply = search(&graph, "limit", None);
        assert_eq!(names(&reply), ["Store.limit"], "{reply}");
        assert_eq!(reply["results"][0]["kind"], "field", "{reply}");
        assert!(residue_names(&reply).is_empty(), "{reply}");
        assert_eq!(state(&reply), ("certified", None, false), "{reply}");
        let reply = search(&graph, "limit", Some("class"));
        assert!(names(&reply).is_empty(), "{reply}");
        assert!(residue_names(&reply).is_empty(), "{reply}");
        assert_eq!(
            reply["edge_coverage"]["declarations"]["state"], "complete",
            "{reply}"
        );
        // Removing a known field with a class filter cannot certify that
        // the name is absent from the repository.
        assert_eq!(
            state(&reply),
            ("inconclusive", Some("name_filter_narrowed_to_zero"), false),
            "{reply}"
        );
    }

    #[test]
    fn module_qualified_unadmitted_declaration_prevents_false_absence() {
        let graph = InMemoryGraph::new();
        admit(&graph, "app/rate_engine.py", "if FLAG:\n    def normalize_rate(value):\n        return value\nelse:\n    def normalize_rate(value):\n        return value + 1\n", false);
        let reply = search(&graph, "rate_engine.normalize_rate", Some("function"));
        assert!(names(&reply).is_empty(), "{reply}");
        assert_eq!(
            residue_names(&reply),
            ["normalize_rate", "normalize_rate"],
            "{reply}"
        );
        assert_eq!(
            reply["edge_coverage"]["declarations"]["state"], "complete_except",
            "{reply}"
        );
        assert!(!state(&reply).2, "{reply}");
        let other = search(&graph, "other_engine.normalize_rate", Some("function"));
        assert!(residue_names(&other).is_empty(), "{other}");
    }

    #[test]
    fn module_qualified_missing_module_identity_cannot_certify_absence() {
        for path in [
            "app/rate_engine.py",
            "app/rate_engine/__init__.py",
            "app/rate_engine/index.js",
        ] {
            let graph = InMemoryGraph::new();
            let source = if path.ends_with(".js") {
                "export function normalize_rate(value) { return value; }"
            } else {
                "def normalize_rate(value):\n    return value\n"
            };
            admit(&graph, path, source, false);
            for module in graph
                .query_entities(&kin_model::graph::EntityFilter {
                    kinds: Some(vec![EntityKind::Module]),
                    ..Default::default()
                })
                .unwrap()
            {
                graph.remove_entity(&module.id).unwrap();
            }
            let reply = search(&graph, "rate_engine.normalize_rate", None);
            assert!(names(&reply).is_empty(), "{reply}");
            assert_eq!(
                reply["edge_coverage"]["declarations"]["state"], "unmeasured",
                "{reply}"
            );
            assert_eq!(
                reply["edge_coverage"]["declarations"]["unmeasured_files"][0]["reason"],
                "module_qualification_unrecorded",
                "{reply}"
            );
            assert!(!state(&reply).2, "{reply}");
            assert!(
                reply["edge_coverage"]["declarations"]
                    .get("unmeasured_languages")
                    .is_none(),
                "{reply}"
            );
            let factor = state(&reply).1.unwrap();
            assert!(factor.contains(super::CENSUS_UNMEASURED_CODE), "{factor}");
            assert!(!factor.contains("unlisted_clause"), "{factor}");
            let explanation = super::clauses(reply["edge_coverage"].as_object().unwrap())
                .unwrap()
                .join(" ");
            assert!(
                explanation.contains("recorded module identity is missing"),
                "{explanation}"
            );
            assert!(!explanation.contains("adapter takes none"), "{explanation}");
        }
    }

    #[test]
    fn module_qualified_census_lists_unadmitted_sibling_even_when_another_file_matches() {
        let graph = InMemoryGraph::new();
        admit(
            &graph,
            "one/rate_engine.py",
            "def normalize_rate(value):\n    return value\n",
            false,
        );
        admit(&graph, "two/rate_engine.py", "if FLAG:\n    def normalize_rate(value):\n        return value\nelse:\n    def normalize_rate(value):\n        return value + 1\n", false);
        let entities = graph
            .query_entities(&kin_model::graph::EntityFilter {
                file_path: Some(FilePathId::new("one/rate_engine.py")),
                ..Default::default()
            })
            .unwrap();
        let module = entities
            .iter()
            .find(|entity| entity.kind == EntityKind::Module)
            .unwrap();
        let function = entities
            .iter()
            .find(|entity| entity.name == "normalize_rate")
            .unwrap();
        graph
            .upsert_relation(&kin_model::Relation {
                id: kin_model::RelationId::new(),
                kind: kin_model::RelationKind::Contains,
                src: kin_model::GraphNodeId::Entity(module.id),
                dst: kin_model::GraphNodeId::Entity(function.id),
                confidence: 1.0,
                origin: kin_model::RelationOrigin::Parsed,
                created_in: None,
                import_source: None,
                evidence: Vec::new(),
            })
            .unwrap();
        let reply = search(&graph, "rate_engine.normalize_rate", Some("function"));
        assert_eq!(names(&reply), ["normalize_rate"], "{reply}");
        assert_eq!(
            residue_names(&reply),
            ["normalize_rate", "normalize_rate"],
            "{reply}"
        );
        assert_eq!(
            reply["edge_coverage"]["declarations"]["state"], "complete_except",
            "{reply}"
        );
        assert!(!state(&reply).2, "{reply}");
    }

    /// A file whose certificate no longer matches its bytes, and a stored
    /// certificate without a census, leave the search unmeasured and name why.
    #[test]
    fn a_stale_certificate_or_a_certificate_without_a_census_is_unmeasured() {
        let _host = crate::edge_coverage::test_support::scoped_language_servers(&[]);
        let graph = InMemoryGraph::new();
        admit(&graph, "app/store.py", PYTHON, true);
        let reply = search(&graph, "helper", None);
        let declarations = &reply["edge_coverage"]["declarations"];
        assert_eq!(declarations["state"], "unmeasured", "{reply}");
        assert_eq!(
            declarations["unmeasured_files"][0]["reason"],
            "certificate_not_current"
        );
        assert_eq!(state(&reply).0, "inconclusive");
        assert!(state(&reply)
            .1
            .unwrap()
            .contains(super::CENSUS_UNMEASURED_CODE));

        let graph = InMemoryGraph::new();
        admit(&graph, "app/store.py", PYTHON, false);
        // Ruby now takes a census. Deliberately omit only the fixture's
        // census entries to retain the missing-measurement refusal control.
        admit_with_census(
            &graph,
            "lib/helper.rb",
            "def helper\n  1\nend\n",
            false,
            false,
        );
        let reply = search(&graph, "helper", None);
        let declarations = &reply["edge_coverage"]["declarations"];
        assert_eq!(declarations["state"], "unmeasured", "{reply}");
        assert_eq!(
            declarations["unmeasured_languages"],
            json!(["Ruby"]),
            "{reply}"
        );
        assert!(state(&reply)
            .1
            .unwrap()
            .contains(super::CENSUS_UNMEASURED_CODE));

        // Narrowed to the language that carries a census, the same search
        // certifies.
        let mut args: HashMap<String, Value> = HashMap::new();
        args.insert("query".into(), json!("helper"));
        args.insert("language".into(), json!("python"));
        let result = crate::handlers::entities::handle_semantic_search(&args, &graph).unwrap();
        let reply = text_value(&result);
        assert_eq!(
            reply["edge_coverage"]["declarations"]["state"], "complete",
            "{reply}"
        );

        // Current Ruby and Go adapters both take a census, so a search
        // over Python, Ruby and Go certifies without any omitted evidence.
        let graph = InMemoryGraph::new();
        admit(&graph, "app/store.py", PYTHON, false);
        admit(&graph, "lib/helper.rb", "def helper\n  1\nend\n", false);
        admit(
            &graph,
            "cmd/main.go",
            "package main\n\nfunc helper() int { return 1 }\n",
            false,
        );
        let reply = search(&graph, "helper", None);
        assert_eq!(
            reply["edge_coverage"]["declarations"]["state"], "complete",
            "{reply}"
        );
    }
}

#[cfg(test)]
mod reexport_tests {
    use super::reexport_source;

    /// Only a line that assigns the name to another module's member of the
    /// same name is a re-export; anything else is left unnamed.
    #[test]
    fn a_reexport_is_named_only_for_an_exact_member_forward() {
        for (line, name, expected) in [
            (
                "export const measureInstance = $$$config.measureInstance;",
                "measureInstance",
                Some("$$$config.measureInstance"),
            ),
            ("  route = Scaffold.route", "route", Some("Scaffold.route")),
            (
                "export const measureInstance = $$$config.other;",
                "measureInstance",
                None,
            ),
            (
                "export const measureInstance = shim(measureInstance);",
                "measureInstance",
                None,
            ),
            (
                "export const measureInstance = measureInstance;",
                "measureInstance",
                None,
            ),
            (
                "export function measureInstance() {}",
                "measureInstance",
                None,
            ),
        ] {
            assert_eq!(reexport_source(line, name).as_deref(), expected, "{line}");
        }
    }
}
