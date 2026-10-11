// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A typed receiver's method is found wherever the repository declares a
//! member of its type, not only in the file that declares the type: a Swift
//! extension in another file, a C++ out-of-line definition, a PHP trait the
//! class uses, and a Swift superclass the class names as a conformance. Batch
//! and incremental links agree, and a member whose own file reads the owner
//! name as another type never answers.

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
    linker.record_class_bases(files);
    [
        ("batch", link_cross_file(files, &ids).unwrap()),
        (
            "incremental",
            link_cross_file_incremental(files, &linker).unwrap(),
        ),
    ]
}

/// Every receiver-origin edge from the entity named `caller`, as (target
/// file, target name).
fn origin_calls(
    relations: &[Relation],
    files: &[FileParseData],
    caller: &str,
) -> Vec<(String, String)> {
    let placed: HashMap<EntityId, (String, String)> = files
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
            relation.kind == RelationKind::Calls
                && relation.src.as_entity() == Some(caller)
                && relation.confidence == RECEIVER_ORIGIN_CONFIDENCE
                && receiver_origin_chain(relation).is_some()
        })
        .filter_map(|relation| placed.get(&relation.dst.as_entity()?).cloned())
        .collect();
    out.sort();
    out
}

fn pair(file: &str, name: &str) -> (String, String) {
    (file.to_string(), name.to_string())
}

#[test]
fn a_swift_method_an_extension_in_another_file_adds_binds() {
    let files = vec![
        fixture("Sources/App/Cache.swift", "class Cache {}\n"),
        fixture(
            "Sources/App/Cache+Load.swift",
            "extension Cache {\n    func load() -> Int { 1 }\n}\n",
        ),
        fixture(
            "Sources/App/Store.swift",
            "class Store {\n    func load() -> Int { 2 }\n}\n",
        ),
        fixture(
            "Sources/App/Use.swift",
            "func run(c: Cache) -> Int {\n    return c.load()\n}\n",
        ),
    ];
    for (label, relations) in links(&files) {
        assert_eq!(
            origin_calls(&relations, &files, "run"),
            vec![pair("Sources/App/Cache+Load.swift", "Cache.load")],
            "{label}"
        );
    }
}

#[test]
fn a_swift_method_a_superclass_declares_binds_through_the_conformance_edge() {
    let files = vec![
        fixture(
            "Sources/App/Base.swift",
            "class Base {\n    func save() {}\n}\n\nclass Sub: Base {}\n",
        ),
        fixture(
            "Sources/App/Other.swift",
            "class Other {\n    func save() {}\n}\n",
        ),
        fixture(
            "Sources/App/Use.swift",
            "func go(s: Sub) {\n    s.save()\n}\n",
        ),
    ];
    for (label, relations) in links(&files) {
        assert_eq!(
            origin_calls(&relations, &files, "go"),
            vec![pair("Sources/App/Base.swift", "Base.save")],
            "{label}"
        );
    }
}

#[test]
fn two_ancestors_that_each_declare_the_method_bind_nothing() {
    let files = vec![
        fixture(
            "Sources/App/Base.swift",
            "class Base {\n    func save() {}\n}\n\nprotocol Saving {}\n\nextension Saving {\n    func save() {}\n}\n\n\
             class Sub: Base, Saving {}\n",
        ),
        fixture(
            "Sources/App/Use.swift",
            "func go(s: Sub) {\n    s.save()\n}\n",
        ),
    ];
    for (label, relations) in links(&files) {
        assert!(
            origin_calls(&relations, &files, "go").is_empty(),
            "{label}: which one runs depends on an order the index does not keep"
        );
    }
}

#[test]
fn a_php_method_a_used_trait_declares_binds() {
    let files = vec![
        fixture(
            "src/Loads.php",
            "<?php\nnamespace App;\n\ntrait Loads {\n    public function load() { return 1; }\n}\n",
        ),
        fixture(
            "src/Cache.php",
            "<?php\nnamespace App;\n\nclass Cache {\n    use Loads;\n}\n",
        ),
        fixture(
            "src/Store.php",
            "<?php\nnamespace App;\n\nclass Store {\n    public function load() { return 2; }\n}\n",
        ),
        fixture(
            "src/Use.php",
            "<?php\nnamespace App;\n\nfunction run(Cache $c) {\n    return $c->load();\n}\n",
        ),
    ];
    for (label, relations) in links(&files) {
        assert_eq!(
            origin_calls(&relations, &files, "run"),
            vec![pair("src/Loads.php", "Loads.load")],
            "{label}"
        );
    }
}

/// The C++ adapter emits a call through a receiver another file settles with
/// its bare name and no receiver until the linker can follow it; this fixture
/// hands the linker that receiver, as the adapter will.
fn with_receiver(mut file: FileParseData, callee: &str, receiver: &str) -> FileParseData {
    for relation in &mut file.relations {
        if relation.kind == RelationKind::Calls && relation.dst_name == callee {
            relation.receiver = Some(receiver.to_string());
        }
    }
    file
}

#[test]
fn a_cpp_member_defined_out_of_line_in_another_file_binds() {
    let files = vec![
        fixture(
            "src/widget.h",
            "class Widget {\npublic:\n    int draw() { return 1; }\n};\n",
        ),
        fixture(
            "src/cache.h",
            "#include \"widget.h\"\n\nclass Cache {\npublic:\n    Widget* widget();\n};\n",
        ),
        fixture(
            "src/cache.cpp",
            "#include \"cache.h\"\n\nWidget* Cache::widget() { return nullptr; }\n",
        ),
        fixture(
            "src/canvas.h",
            "class Canvas {\npublic:\n    int draw() { return 2; }\n};\n",
        ),
        with_receiver(
            fixture(
                "src/use.cpp",
                "#include \"cache.h\"\n\nint run() {\n    Cache c;\n    auto w = c.widget();\n    return w->draw();\n}\n",
            ),
            "draw",
            "w",
        ),
    ];
    for (label, relations) in links(&files) {
        assert_eq!(
            origin_calls(&relations, &files, "run"),
            vec![pair("src/widget.h", "Widget::draw")],
            "{label}: the out-of-line Cache::widget declares what w holds"
        );
    }
}

#[test]
fn a_member_whose_file_reads_the_owner_as_another_type_never_answers() {
    // Package a's Cache declares no Size; package b's Cache does, in a file
    // whose own reading of `Cache` is b's type, so it is not a's member.
    let files = vec![
        fixture("a/cache.go", "package a\n\ntype Cache struct{}\n"),
        fixture(
            "a/use.go",
            "package a\n\nfunc run(c *Cache) int {\n\treturn c.Size()\n}\n",
        ),
        fixture("b/cache.go", "package b\n\ntype Cache struct{}\n"),
        fixture(
            "b/size.go",
            "package b\n\nfunc (c *Cache) Size() int { return 0 }\n",
        ),
        fixture(
            "c/other.go",
            "package c\n\ntype Other struct{}\n\nfunc (o *Other) Size() int { return 1 }\n",
        ),
    ];
    for (label, relations) in links(&files) {
        assert!(
            origin_calls(&relations, &files, "run").is_empty(),
            "{label}: b's Cache.Size is not package a's Cache's member"
        );
    }
}
