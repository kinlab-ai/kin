// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A call through one field of the method's own instance, `self.cache` in
//! Rust or `this.cache` in Java, binds the field's declared type's method at
//! the receiver-origin tier, batch and incremental, and never a same-named
//! method of another type.

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

/// Every call edge from the entity named `caller`, as (target name,
/// confidence, chain).
fn calls_from(
    relations: &[Relation],
    files: &[FileParseData],
    caller: &str,
) -> Vec<(String, f32, Option<String>)> {
    let names: HashMap<EntityId, &str> = files
        .iter()
        .flat_map(|file| file.entities.iter())
        .map(|entity| (entity.id, entity.name.as_str()))
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
            Some((
                names.get(&relation.dst.as_entity()?)?.to_string(),
                relation.confidence,
                receiver_origin_chain(relation).map(str::to_string),
            ))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn assert_binds(label: &str, calls: &[(String, f32, Option<String>)], target: &str, decoy: &str) {
    assert!(
        calls.iter().any(|(name, confidence, chain)| name == target
            && *confidence == RECEIVER_ORIGIN_CONFIDENCE
            && chain.is_some()),
        "{label}: {target} at the receiver-origin tier: {calls:?}"
    );
    assert!(
        !calls.iter().any(|(name, _, _)| name == decoy),
        "{label}: {decoy} gains no caller: {calls:?}"
    );
}

#[test]
fn a_rust_call_through_a_field_of_self_binds_the_fields_type() {
    let files = vec![
        fixture(
            "src/cache.rs",
            "pub struct Cache;\n\nimpl Cache {\n    pub fn load(&self) -> u8 {\n        1\n    }\n}\n",
        ),
        fixture(
            "src/store.rs",
            "pub struct Store;\n\nimpl Store {\n    pub fn load(&self) -> u8 {\n        2\n    }\n}\n",
        ),
        fixture(
            "src/app.rs",
            "use crate::cache::Cache;\n\npub struct App {\n    cache: Cache,\n}\n\n\
             impl App {\n    pub fn run(&self) -> u8 {\n        self.cache.load()\n    }\n}\n",
        ),
    ];
    for (label, relations) in links(&files) {
        let calls = calls_from(&relations, &files, "App::run");
        assert_binds(label, &calls, "Cache::load", "Store::load");
    }
}

#[test]
fn a_java_call_through_a_field_of_this_binds_the_fields_type() {
    let files = vec![
        fixture(
            "src/main/java/app/cache/Cache.java",
            "package app.cache;\n\npublic class Cache {\n    public int load() { return 1; }\n}\n",
        ),
        fixture(
            "src/main/java/app/store/Store.java",
            "package app.store;\n\npublic class Store {\n    public int load() { return 2; }\n}\n",
        ),
        fixture(
            "src/main/java/app/Service.java",
            "package app;\n\nimport app.cache.Cache;\nimport app.store.Store;\n\n\
             public class Service {\n    private Cache cache;\n\n    \
             public int run(Store cache) {\n        return this.cache.load();\n    }\n}\n",
        ),
    ];
    for (label, relations) in links(&files) {
        // The parameter shadows the field; `this.cache` still reads the field.
        let calls = calls_from(&relations, &files, "Service.run");
        assert_binds(label, &calls, "Cache.load", "Store.load");
    }
}
