// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Which other names a census follows for a focal, over source the pipeline
//! parsed and linked.
//!
//! React's legacy Fizz config wraps the DOM config's `pushSegmentFinale`
//! under an alias:
//! `import {pushSegmentFinale as pushSegmentFinaleImpl} from './ReactFizzConfigDOM'`.
//! Three modules declare a function of that name. A census follows every
//! name an import could give its focal, since a call through such a name is
//! a call to it, and it read that import by its name alone: asked about the
//! legacy module's own `pushSegmentFinale`, or the markup renderer's, it
//! followed `pushSegmentFinaleImpl` as well, and an answer about either then
//! said some binding gives it another name and listed none.
//!
//! The import statement is resolved in the graph. Where its edge lands on
//! one declaration's own definition, the statement binds that declaration,
//! and the alias is a name of no other.

use std::collections::HashMap;

use kin_db::call_site_escape::WrittenImport;
use kin_db::{focal_escape_evidence_in, CensusFile, EscapeCensusGraph};
use kin_index::{FileParseData, IndexPipeline};
use kin_model::{
    focal_call_names, ArtifactId, Entity, EntityId, FilePathId, FocalEscape, Hash256, LanguageId,
    ParseCompleteness, Relation, RelationEvidence, RelationKind,
};

const DOM_JS: &str = "packages/react-dom-bindings/src/server/ReactFizzConfigDOM.js";
const LEGACY_JS: &str = "packages/react-dom-bindings/src/server/ReactFizzConfigDOMLegacy.js";
const MARKUP_JS: &str = "packages/react-markup/src/ReactFizzConfigMarkup.js";

const DOM: &str = r#"export function pushSegmentFinale(
  target,
  renderState,
  lastPushedText,
  textEmbedded,
) {
  if (lastPushedText && textEmbedded) {
    target.push(textSeparator);
  }
}
"#;

const LEGACY: &str = r#"import {
  pushSegmentFinale as pushSegmentFinaleImpl,
} from './ReactFizzConfigDOM';

export function pushSegmentFinale(
  target,
  renderState,
  lastPushedText,
  textEmbedded,
) {
  if (renderState.generateStaticMarkup) {
    return;
  } else {
    return pushSegmentFinaleImpl(
      target,
      renderState,
      lastPushedText,
      textEmbedded,
    );
  }
}
"#;

const MARKUP: &str = r#"export function pushSegmentFinale(
  target,
  renderState,
  lastPushedText,
  textEmbedded,
) {
  return;
}
"#;

const SOURCES: [(&str, &str); 3] = [(DOM_JS, DOM), (LEGACY_JS, LEGACY), (MARKUP_JS, MARKUP)];

struct Parsed {
    files: Vec<CensusFile>,
    entities: HashMap<String, Vec<Entity>>,
    /// The linked relations, when the graph a census reads holds any.
    relations: Vec<Relation>,
}

impl Parsed {
    fn of(sources: &[(&str, &str)], linked: bool) -> Self {
        let pipeline = IndexPipeline::new();
        let mut files = Vec::new();
        let mut entities = HashMap::new();
        let mut parsed = Vec::new();
        for (path, source) in sources {
            let blob = kin_blobs::digest(source.as_bytes());
            let indexed = pipeline
                .index_file_content_with_tests(&FilePathId::new(*path), source.as_bytes(), blob)
                .unwrap()
                .indexed_file;
            files.push(CensusFile {
                path: FilePathId::new(*path),
                language: LanguageId::JavaScript,
                blob: Hash256::from_hex(&blob.to_string()).unwrap(),
                parsed: Some(true),
            });
            entities.insert(path.to_string(), indexed.entities.clone());
            parsed.push(FileParseData {
                file_path: path.to_string(),
                entities: indexed.entities,
                relations: indexed.extracted_relations,
                imports: indexed.imports,
            });
        }
        let relations = if linked {
            let ids: HashMap<String, ArtifactId> = parsed
                .iter()
                .map(|file| (file.file_path.clone(), ArtifactId::new()))
                .collect();
            let completeness = parsed
                .iter()
                .map(|file| (file.file_path.clone(), ParseCompleteness::Full))
                .collect();
            kin_index::link_cross_file_with_completeness(&parsed, &ids, &completeness).unwrap()
        } else {
            Vec::new()
        };
        Self {
            files,
            entities,
            relations,
        }
    }

    fn census(&self, path: &str, name: &str) -> FocalEscape {
        let focal = self.entities[path]
            .iter()
            .find(|entity| entity.name == name)
            .unwrap_or_else(|| panic!("no entity {name} in {path}"));
        focal_escape_evidence_in(self, focal, &focal_call_names(focal))
    }
}

impl EscapeCensusGraph for Parsed {
    fn census_files(&self, languages: &[LanguageId]) -> Option<Vec<CensusFile>> {
        Some(
            self.files
                .iter()
                .filter(|file| languages.contains(&file.language))
                .cloned()
                .collect(),
        )
    }

    fn entities_in(&self, path: &FilePathId) -> Option<Vec<Entity>> {
        Some(self.entities.get(&path.0).cloned().unwrap_or_default())
    }

    fn import_evidence_into(&self, target: EntityId) -> Option<Vec<RelationEvidence>> {
        Some(
            self.relations
                .iter()
                .filter(|relation| {
                    relation.kind == RelationKind::Imports
                        && relation.dst.as_entity() == Some(target)
                })
                .flat_map(|relation| relation.evidence.iter().cloned())
                .collect(),
        )
    }

    fn imports_written_in(&self, file: &CensusFile) -> Vec<WrittenImport> {
        kin_index::written_imports(self.relations.iter(), &file.path.0, |id| {
            self.entities
                .values()
                .flatten()
                .find(|entity| entity.id == id)
                .map(|entity| entity.kind)
        })
    }
}

/// The other names a census follows for a focal, and the places it says
/// give the focal one of them, as (file, line).
fn other_names(escape: &FocalEscape) -> (Vec<String>, Vec<(String, u32)>) {
    let residue = escape.residue().expect("the census read every file");
    (
        residue.alias_names.clone(),
        residue
            .alias_sites
            .iter()
            .map(|site| (site.file.clone(), site.line))
            .collect(),
    )
}

#[test]
fn an_alias_is_a_name_of_the_declaration_its_import_binds_and_of_no_other() {
    let linked = Parsed::of(&SOURCES, true);
    let alias = vec!["pushSegmentFinaleImpl".to_string()];

    // The DOM config's function is the one the import binds.
    assert_eq!(
        other_names(&linked.census(DOM_JS, "pushSegmentFinale")),
        (alias, vec![(LEGACY_JS.to_string(), 2)])
    );
    // The legacy module's own function and the markup renderer's are other
    // declarations of the name: the alias is not theirs.
    for path in [LEGACY_JS, MARKUP_JS] {
        assert_eq!(
            other_names(&linked.census(path, "pushSegmentFinale")),
            (Vec::new(), Vec::new()),
            "{path}"
        );
    }
}

/// A graph that resolved no import proves nothing about what the statement
/// binds, so the census follows the alias for every declaration of the name,
/// as it did.
#[test]
fn an_import_the_graph_did_not_resolve_keeps_the_alias_followed() {
    let unlinked = Parsed::of(&SOURCES, false);
    for path in [DOM_JS, LEGACY_JS, MARKUP_JS] {
        let (names, sites) = other_names(&unlinked.census(path, "pushSegmentFinale"));
        assert_eq!(names, vec!["pushSegmentFinaleImpl".to_string()], "{path}");
        assert_eq!(
            sites,
            Vec::new(),
            "{path}: no edge proves whose alias it is"
        );
    }
}
