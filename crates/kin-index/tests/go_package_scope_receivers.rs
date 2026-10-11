// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Go names are scoped to a package, which spans every file of a directory
//! that shares a package clause. A receiver's type, the type's methods and a
//! constructor may each be declared in any file of the package, and an
//! unqualified type name never means another package's type.

use std::collections::HashMap;

use kin_index::receiver_origin::receiver_origin_chain;
use kin_index::resolution::RECEIVER_ORIGIN_CONFIDENCE;
use kin_index::{
    link_cross_file, link_cross_file_incremental, FileParseData, IncrementalLinker, IndexPipeline,
};
use kin_model::{ArtifactId, EntityId, FilePathId, Relation, RelationKind};

fn fixture(path: &str, source: &str) -> FileParseData {
    let indexed = IndexPipeline::new()
        .index_file_content_with_tests(
            &FilePathId::new(path),
            source.as_bytes(),
            kin_blobs::digest(source.as_bytes()),
        )
        .unwrap()
        .indexed_file;
    FileParseData {
        file_path: path.into(),
        entities: indexed.entities,
        relations: indexed.extracted_relations,
        imports: indexed.imports,
    }
}

fn links(files: &[FileParseData]) -> [(&'static str, Vec<Relation>); 2] {
    let ids: HashMap<String, ArtifactId> = files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect();
    let mut linker = IncrementalLinker::new();
    for file in files {
        linker.add_file(&file.file_path, ids[&file.file_path], &file.entities);
    }
    [
        ("batch", link_cross_file(files, &ids).unwrap()),
        (
            "incremental",
            link_cross_file_incremental(files, &linker).unwrap(),
        ),
    ]
}

/// Every call edge from the entity named `caller`, as (target file, target
/// name, confidence, whether the receiver-origin tier bound it).
fn calls_from(
    relations: &[Relation],
    files: &[FileParseData],
    caller: &str,
) -> Vec<(String, String, f32, bool)> {
    let targets: HashMap<EntityId, (String, String)> = files
        .iter()
        .flat_map(|file| {
            file.entities
                .iter()
                .map(|entity| (entity.id, (file.file_path.clone(), entity.name.clone())))
        })
        .collect();
    let caller = files
        .iter()
        .flat_map(|file| file.entities.iter())
        .find(|entity| entity.name == caller)
        .unwrap_or_else(|| panic!("no entity {caller}"))
        .id;
    let mut out: Vec<_> = relations
        .iter()
        .filter(|relation| {
            relation.kind == RelationKind::Calls && relation.src.as_entity() == Some(caller)
        })
        .filter_map(|relation| {
            let (file, name) = targets.get(&relation.dst.as_entity()?)?.clone();
            Some((
                file,
                name,
                relation.confidence,
                receiver_origin_chain(relation).is_some(),
            ))
        })
        .collect();
    out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    out
}

fn origin_bound(calls: &[(String, String, f32, bool)]) -> Vec<(&str, &str)> {
    calls
        .iter()
        .filter(|(_, _, confidence, origin)| *origin && *confidence == RECEIVER_ORIGIN_CONFIDENCE)
        .map(|(file, name, _, _)| (file.as_str(), name.as_str()))
        .collect()
}

#[test]
fn a_type_and_its_methods_resolve_across_the_files_of_its_package() {
    let files = vec![
        fixture(
            "store/store.go",
            "package store\n\ntype Store struct{}\n",
        ),
        fixture(
            "store/load.go",
            "package store\n\nfunc (s *Store) Load(key string) string { return key }\n",
        ),
        fixture(
            "store/use.go",
            "package store\n\nfunc use(s *Store) string {\n\treturn s.Load(\"k\")\n}\n",
        ),
        // Another package declares a type and a method of the same names, so
        // a repository-wide lookup by simple name finds two of each.
        fixture(
            "other/store.go",
            "package other\n\ntype Store struct{}\n\nfunc (s *Store) Load(key string) string { return \"\" }\n",
        ),
    ];
    for (label, relations) in links(&files) {
        let calls = calls_from(&relations, &files, "use");
        assert_eq!(
            origin_bound(&calls),
            vec![("store/load.go", "Store.Load")],
            "{label}: the package's own Store, whose Load another file of it declares: {calls:?}"
        );
        assert!(
            !calls.iter().any(|(file, _, _, _)| file == "other/store.go"),
            "{label}: {calls:?}"
        );
    }
}

#[test]
fn a_qualified_type_resolves_in_every_file_of_the_imported_package() {
    let files = vec![
        // The smallest file of the package is the one the import reaches.
        fixture("net/a.go", "package net\n\nfunc helper() {}\n"),
        fixture(
            "net/addr.go",
            "package net\n\ntype Addr interface {\n\tString() string\n}\n",
        ),
        fixture(
            "other/addr.go",
            "package other\n\ntype Addr interface {\n\tString() string\n}\n",
        ),
        fixture(
            "use/use.go",
            "package use\n\nimport \"example.com/x/net\"\n\nfunc show(a net.Addr) string {\n\treturn a.String()\n}\n",
        ),
    ];
    for (label, relations) in links(&files) {
        let calls = calls_from(&relations, &files, "show");
        assert_eq!(
            origin_bound(&calls),
            vec![("net/addr.go", "Addr.String")],
            "{label}: {calls:?}"
        );
    }
}

#[test]
fn an_unqualified_type_never_means_another_packages_type() {
    let files = vec![
        fixture(
            "b/store.go",
            "package b\n\ntype Store struct{}\n\nfunc (s *Store) Load() {}\n",
        ),
        fixture(
            "b/cache.go",
            "package b\n\ntype Cache struct{}\n\nfunc (c *Cache) Load() {}\n",
        ),
        // Package a declares no Store, so `Store` here does not compile
        // against b's; the only Store in the repository must not answer it.
        fixture(
            "a/use.go",
            "package a\n\nfunc use(s *Store) {\n\ts.Load()\n}\n",
        ),
    ];
    for (label, relations) in links(&files) {
        let calls = calls_from(&relations, &files, "use");
        assert!(
            origin_bound(&calls).is_empty(),
            "{label}: package b's Store is not package a's: {calls:?}"
        );
    }
}

#[test]
fn a_restored_incremental_linker_still_knows_the_files_of_each_package() {
    let files = vec![
        fixture("store/store.go", "package store\n\ntype Store struct{}\n"),
        fixture(
            "store/load.go",
            "package store\n\nfunc (s *Store) Load(key string) string { return key }\n",
        ),
        fixture(
            "store/use.go",
            "package store\n\nfunc use(s *Store) string {\n\treturn s.Load(\"k\")\n}\n",
        ),
    ];
    let mut linker = IncrementalLinker::new();
    for file in &files {
        linker.add_file(&file.file_path, ArtifactId::new(), &file.entities);
    }
    let restored = IncrementalLinker::from_checkpoint_v1(linker.to_checkpoint_v1()).unwrap();
    let relations = link_cross_file_incremental(&files, &restored).unwrap();
    let calls = calls_from(&relations, &files, "use");
    assert_eq!(
        origin_bound(&calls),
        vec![("store/load.go", "Store.Load")],
        "{calls:?}"
    );
    // A file removed from the package takes its declarations with it.
    let mut shrunk = restored;
    shrunk.remove_file("store/load.go");
    let relations = link_cross_file_incremental(&files[2..], &shrunk).unwrap();
    assert!(origin_bound(&calls_from(&relations, &files, "use")).is_empty());
}
