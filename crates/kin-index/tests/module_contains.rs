// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A unit's own module contains the declarations written at its top, in every
//! language whose adapter gives a unit a module, through every local linking
//! path: the pipeline's same-unit pass, the batch linker and the incremental
//! linker.

use kin_index::{FileParseData, IncrementalLinker, IndexPipeline};
use kin_model::{ArtifactId, EntityId, EntityKind, FilePathId, Relation, RelationKind};

fn indexed(path: &str, source: &str) -> (FileParseData, Vec<Relation>) {
    let bytes = source.as_bytes();
    let indexed = IndexPipeline::new()
        .index_file_content_with_tests(&FilePathId::new(path), bytes, kin_blobs::digest(bytes))
        .expect("source-bound indexing")
        .indexed_file;
    (
        FileParseData {
            file_path: path.to_string(),
            entities: indexed.entities,
            relations: indexed.extracted_relations,
            imports: indexed.imports,
        },
        indexed.relations,
    )
}

fn edges(files: &[FileParseData]) -> [Vec<Relation>; 2] {
    let identities = files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect();
    let batch = kin_index::link_cross_file(files, &identities).expect("batch");
    let mut incremental = IncrementalLinker::new();
    for file in files {
        incremental.add_file(&file.file_path, ArtifactId::new(), &file.entities);
    }
    incremental.record_class_bases(files);
    let incremental =
        kin_index::link_cross_file_incremental(files, &incremental).expect("incremental");
    [batch, incremental]
}

fn contains(relations: &[Relation], parent: EntityId, child: EntityId) -> Option<&Relation> {
    relations.iter().find(|relation| {
        relation.kind == RelationKind::Contains
            && relation.src.as_entity() == Some(parent)
            && relation.dst.as_entity() == Some(child)
    })
}

fn find(file: &FileParseData, name: &str, kinds: &[EntityKind]) -> EntityId {
    file.entities
        .iter()
        .find(|entity| entity.name == name && kinds.contains(&entity.kind))
        .unwrap_or_else(|| {
            panic!(
                "{}: no {kinds:?} named {name} in {:?}",
                file.file_path,
                file.entities
                    .iter()
                    .map(|entity| (&entity.name, entity.kind))
                    .collect::<Vec<_>>()
            )
        })
        .id
}

fn unit_module(file: &FileParseData) -> EntityId {
    let modules: Vec<_> = file
        .entities
        .iter()
        .filter(|entity| kin_model::is_file_module_surface(entity))
        .collect();
    assert_eq!(modules.len(), 1, "{}", file.file_path);
    modules[0].id
}

/// Each language's unit: a function named like the unit's module where the
/// language allows it, a type, and a method of that type.
const UNITS: &[(&str, &str, &str, &str, &str)] = &[
    (
        "pkg/button.py",
        "def button():\n    return 1\n\n\nclass Store:\n    def get(self):\n        return 2\n",
        "button",
        "Store",
        "Store.get",
    ),
    (
        "src/button.ts",
        "export function button(): number {\n  return 1;\n}\n\nexport class Store {\n  get(): number {\n    return 2;\n  }\n}\n",
        "button",
        "Store",
        "Store.get",
    ),
    (
        "src/button.js",
        "export function button() {\n  return 1;\n}\n\nexport class Store {\n  get() {\n    return 2;\n  }\n}\n",
        "button",
        "Store",
        "Store.get",
    ),
    (
        "button/button.go",
        "package button\n\nfunc button() int { return 1 }\n\ntype Store struct{}\n\nfunc (s Store) Get() int { return 2 }\n",
        "button",
        "Store",
        "Store.Get",
    ),
    (
        "src/button.rs",
        "pub fn button() -> u8 {\n    1\n}\n\npub struct Store;\n\nimpl Store {\n    pub fn get(&self) -> u8 {\n        2\n    }\n}\n",
        "button",
        "Store",
        "Store::get",
    ),
    (
        "src/app/Store.java",
        "package app;\n\npublic class Store {\n    int get() { return 2; }\n}\n",
        "",
        "Store",
        "Store.get",
    ),
    (
        "src/app/button.kt",
        "package app\n\nfun button(): Int = 1\n\nclass Store {\n    fun get(): Int = 2\n}\n",
        "button",
        "Store",
        "Store.get",
    ),
    (
        "src/button.php",
        "<?php\nfunction button() { return 1; }\nclass Store { function get() { return 2; } }\n",
        "button",
        "Store",
        "Store.get",
    ),
    (
        "src/button.swift",
        "func button() -> Int { 1 }\nclass Store { func get() -> Int { 2 } }\n",
        "button",
        "Store",
        "Store.get",
    ),
];

#[test]
fn a_units_module_contains_its_top_level_declarations_in_every_language_with_one() {
    for (path, source, function, class, method) in UNITS {
        let (file, local) = indexed(path, source);
        let module = unit_module(&file);
        let class = find(&file, class, &[EntityKind::Class]);
        let method = find(&file, method, &[EntityKind::Method]);
        let function =
            (!function.is_empty()).then(|| find(&file, function, &[EntityKind::Function]));
        for (pass, relations) in ["pipeline", "batch", "incremental"]
            .into_iter()
            .zip(std::iter::once(local).chain(edges(std::slice::from_ref(&file))))
        {
            let said = format!("{path} ({pass})");
            for child in function.into_iter().chain([class]) {
                let relation = contains(&relations, module, child)
                    .unwrap_or_else(|| panic!("{said}: the module holds no top-level child"));
                assert!(
                    relation
                        .evidence
                        .iter()
                        .any(
                            |evidence| evidence.parser_rule.as_deref() == Some("module_contains")
                                && evidence.source_span.is_some()
                        ),
                    "{said}"
                );
            }
            // A method keeps its type as its owner, wherever it is written.
            assert!(contains(&relations, class, method).is_some(), "{said}");
            assert!(contains(&relations, module, method).is_none(), "{said}");
            // A declaration named like its module is never its own owner.
            if let Some(function) = function {
                assert!(contains(&relations, function, function).is_none(), "{said}");
            }
        }
    }
}

#[test]
fn a_unit_with_no_module_of_its_own_contains_nothing_from_one() {
    for (path, source) in [
        ("src/button.c", "int button(void) { return 1; }\n"),
        ("src/button.cpp", "int button() { return 1; }\n"),
        ("lib/button.rb", "def button\n  1\nend\n"),
        ("main.tf", "variable \"button\" {}\n"),
    ] {
        let (file, local) = indexed(path, source);
        assert!(
            !file.entities.iter().any(kin_model::is_file_module_surface),
            "{path}"
        );
        assert!(
            !local.iter().any(|relation| relation
                .evidence
                .iter()
                .any(|evidence| evidence.parser_rule.as_deref() == Some("module_contains"))),
            "{path}"
        );
    }
}
