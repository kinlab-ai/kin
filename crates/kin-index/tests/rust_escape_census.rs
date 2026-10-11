// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The per-focal escape census over Rust files the pipeline parsed.
//!
//! Before the Rust adapter wrote value-escape records, every Rust focal's
//! census answered unknown, and every answer that leaned on it stayed
//! inconclusive. Each Rust file now carries its record, so a focal is contained
//! or escapes, with the sites that could still reach it listed: a method
//! passed by path, a call a macro body hides, and a trait implementation the
//! standard library invokes.

use std::collections::HashMap;

use kin_db::{focal_escape_evidence_in, CensusFile, EscapeCensusGraph};
use kin_index::IndexPipeline;
use kin_model::{
    focal_call_names, Entity, EntityId, EscapeResidue, FilePathId, FocalEscape, Hash256,
    LanguageId, RelationEvidence,
};

const STORE: &str = r#"use std::fmt;

pub struct Store;

impl Store {
    pub fn new() -> Self {
        Store
    }

    pub fn load(raw: &str) -> Store {
        let _ = raw;
        Store
    }

    pub fn save(&self) {}
}

impl fmt::Display for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("store")
    }
}
"#;

const APP: &str = r#"use crate::store::Store;

pub fn run(items: &[&str]) -> Vec<Store> {
    let store = Store::new();
    store.save();
    items.iter().copied().map(Store::load).collect()
}

fn hidden_call() {}

macro_rules! refresh {
    () => {
        hidden_call()
    };
}

pub fn tick() {
    refresh!();
}
"#;

struct Parsed {
    files: Vec<CensusFile>,
    entities: HashMap<String, Vec<Entity>>,
}

impl Parsed {
    fn of(sources: &[(&str, &str)]) -> Self {
        let pipeline = IndexPipeline::new();
        let mut files = Vec::new();
        let mut entities = HashMap::new();
        for (path, source) in sources {
            let blob = kin_blobs::digest(source.as_bytes());
            let indexed = pipeline
                .index_file_content_with_tests(&FilePathId::new(*path), source.as_bytes(), blob)
                .unwrap()
                .indexed_file;
            files.push(CensusFile {
                path: FilePathId::new(*path),
                language: LanguageId::Rust,
                blob: Hash256::from_hex(&blob.to_string()).unwrap(),
                parsed: Some(true),
            });
            entities.insert(path.to_string(), indexed.entities);
        }
        Self { files, entities }
    }

    fn census(&self, name: &str) -> FocalEscape {
        let focal = self
            .entities
            .values()
            .flatten()
            .find(|entity| entity.name == name)
            .unwrap_or_else(|| panic!("no entity {name}"));
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

    fn import_evidence_into(&self, _target: EntityId) -> Option<Vec<RelationEvidence>> {
        Some(Vec::new())
    }
}

fn residue(escape: &FocalEscape) -> &EscapeResidue {
    escape
        .residue()
        .unwrap_or_else(|| panic!("a census with a residue, not {escape:?}"))
}

fn parsed() -> Parsed {
    Parsed::of(&[("src/store.rs", STORE), ("src/app.rs", APP)])
}

#[test]
fn a_method_passed_by_path_escapes_and_lists_the_site() {
    let escape = parsed().census("Store::load");
    assert!(matches!(escape, FocalEscape::Escapes { .. }), "{escape:?}");
    let values: Vec<(&str, u32)> = residue(&escape)
        .values
        .iter()
        .map(|site| (site.file.as_str(), site.line))
        .collect();
    assert_eq!(values, vec![("src/app.rs", 6)]);
}

#[test]
fn a_method_only_ever_called_is_contained_with_nothing_listed() {
    let escape = parsed().census("Store::save");
    assert!(
        matches!(&escape, FocalEscape::Contained { residue, .. } if residue.is_empty()),
        "{escape:?}"
    );
}

#[test]
fn a_call_a_macro_body_hides_is_listed_as_an_implicit_call() {
    let escape = parsed().census("hidden_call");
    assert!(
        matches!(escape, FocalEscape::Contained { .. }),
        "{escape:?}"
    );
    let implicit: Vec<(&str, u32)> = residue(&escape)
        .implicit_calls
        .iter()
        .map(|site| (site.file.as_str(), site.line))
        .collect();
    assert_eq!(implicit, vec![("src/app.rs", 13)]);
}

#[test]
fn a_trait_implementation_names_the_syntax_that_invokes_it() {
    let escape = parsed().census("Store::fmt");
    let protocol = residue(&escape)
        .protocol
        .as_ref()
        .expect("a trait implementation is a protocol member");
    assert_eq!(protocol.dispatch, "implicit");
    assert_eq!(protocol.through.as_deref(), Some("formatting"));
}
