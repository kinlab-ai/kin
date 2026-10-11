// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Calls through a Java or Kotlin receiver the adapter's value-origin facts
//! type, linked by the batch and the incremental linker.
//!
//! Each fixture holds a same-named method on two unrelated types, and on an
//! implementation of an interface. A call through a receiver whose declared
//! type is one of them binds to that type's method, at the receiver-origin
//! confidence, and to nothing else: never the same-named method of another
//! type, and never an implementation guessed for an interface.

use std::collections::HashMap;

use kin_index::receiver_origin::receiver_origin_chain;
use kin_index::resolution::RECEIVER_ORIGIN_CONFIDENCE;
use kin_index::{link_cross_file as link_cross_file_with_identities, link_cross_file_incremental};
use kin_index::{FileParseData, IncrementalLinker, IndexPipeline};
use kin_model::{ArtifactId, Entity, EntityId, FilePathId, Relation, RelationKind};

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

fn link_batch(files: &[FileParseData]) -> Vec<Relation> {
    let ids = files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect();
    link_cross_file_with_identities(files, &ids).unwrap()
}

fn link_incremental(files: &[FileParseData]) -> Vec<Relation> {
    let mut linker = IncrementalLinker::new();
    for file in files {
        linker.add_file(&file.file_path, ArtifactId::new(), &file.entities);
    }
    link_cross_file_incremental(files, &linker).unwrap()
}

fn entity<'a>(files: &'a [FileParseData], name: &str) -> &'a Entity {
    files
        .iter()
        .flat_map(|file| file.entities.iter())
        .find(|entity| entity.name == name)
        .unwrap_or_else(|| panic!("no entity {name}"))
}

/// Every call edge from `caller`, as (target name, confidence, chain).
fn calls_from(
    relations: &[Relation],
    files: &[FileParseData],
    caller: &str,
) -> Vec<(String, f32, Option<String>)> {
    let caller = entity(files, caller).id;
    let names: HashMap<EntityId, &str> = files
        .iter()
        .flat_map(|file| file.entities.iter())
        .map(|entity| (entity.id, entity.name.as_str()))
        .collect();
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

fn assert_binds_only(
    label: &str,
    calls: &[(String, f32, Option<String>)],
    target: &str,
    decoys: &[&str],
) {
    let edge = calls
        .iter()
        .find(|(name, _, _)| name == target)
        .unwrap_or_else(|| panic!("{label}: no edge to {target} in {calls:?}"));
    assert_eq!(edge.1, RECEIVER_ORIGIN_CONFIDENCE, "{label}: {calls:?}");
    assert!(edge.2.is_some(), "{label}: the edge names its chain");
    for decoy in decoys {
        assert!(
            !calls.iter().any(|(name, _, _)| name == decoy),
            "{label}: {decoy} must gain no caller: {calls:?}"
        );
    }
}

fn java_files() -> Vec<FileParseData> {
    vec![
        fixture(
            "src/main/java/app/store/Store.java",
            "package app.store;\n\npublic interface Store {\n    Item load(String key);\n}\n",
        ),
        fixture(
            "src/main/java/app/store/SqlStore.java",
            "package app.store;\n\npublic class SqlStore implements Store {\n    public Item load(String key) { return null; }\n}\n",
        ),
        fixture(
            "src/main/java/app/cache/Cache.java",
            "package app.cache;\n\npublic class Cache {\n    public Item load(String key) { return null; }\n    public Cache copy() { return this; }\n}\n",
        ),
        fixture(
            "src/main/java/app/Service.java",
            r#"package app;

import app.store.Store;
import app.cache.Cache;
import javax.inject.Inject;

public class Service {
    @Inject private Store store;
    private final Cache cache;

    @Inject
    public Service(Cache cache) {
        this.cache = cache;
    }

    public Item fetch(String key) {
        return store.load(key);
    }

    public Item warm(String key) {
        var local = new Cache();
        return local.load(key);
    }

    public Item copied(String key) {
        var copy = cache.copy();
        return copy.load(key);
    }

    public Item injected(String key) {
        return cache.load(key);
    }
}
"#,
        ),
    ]
}

#[test]
fn a_java_call_through_a_typed_receiver_binds_to_that_types_method_only() {
    let files = java_files();
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        assert_binds_only(
            label,
            &calls_from(&relations, &files, "Service.fetch"),
            "Store.load",
            &["Cache.load", "SqlStore.load"],
        );
        assert_binds_only(
            label,
            &calls_from(&relations, &files, "Service.warm"),
            "Cache.load",
            &["Store.load", "SqlStore.load"],
        );
        assert_binds_only(
            label,
            &calls_from(&relations, &files, "Service.injected"),
            "Cache.load",
            &["Store.load", "SqlStore.load"],
        );
        let copied = calls_from(&relations, &files, "Service.copied");
        assert_binds_only(
            label,
            &copied,
            "Cache.load",
            &["Store.load", "SqlStore.load"],
        );
        let chain = copied
            .iter()
            .find(|(name, _, _)| name == "Cache.load")
            .and_then(|(_, _, chain)| chain.as_deref())
            .unwrap();
        assert!(
            chain.contains("Cache.copy() -> Cache"),
            "{label}: a declared return carries the chain: {chain}"
        );
    }
}

#[test]
fn a_java_typed_field_call_binds_its_class_and_never_the_callers_same_named_member() {
    // The object call `same_owner_bare_call_resolution.rs` pins as untyped,
    // with the field declared: the receiver now names its class.
    let files = vec![fixture(
        "src/main/java/app/Sample.java",
        "package app;\n\nclass Helper {\n    void b() { }\n}\n\nclass Foo {\n    Helper h;\n    void a() { h.b(); }\n    void b() { }\n}\n",
    )];
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        assert_binds_only(
            label,
            &calls_from(&relations, &files, "Foo.a"),
            "Helper.b",
            &["Foo.b"],
        );
    }
}

fn kotlin_files() -> Vec<FileParseData> {
    vec![
        fixture(
            "src/main/kotlin/app/store/Store.kt",
            "package app.store\n\ninterface Store {\n    fun load(key: String): Item\n}\n",
        ),
        fixture(
            "src/main/kotlin/app/store/SqlStore.kt",
            "package app.store\n\nclass SqlStore : Store {\n    override fun load(key: String): Item = Item(key)\n}\n",
        ),
        fixture(
            "src/main/kotlin/app/cache/Cache.kt",
            "package app.cache\n\nclass Cache {\n    fun load(key: String): Item = Item(key)\n\n    fun copy(): Cache = this\n}\n",
        ),
        fixture(
            "src/main/kotlin/app/Service.kt",
            r#"package app

import app.cache.Cache
import app.store.Store
import javax.inject.Inject

class Service @Inject constructor(
    private val store: Store,
    private val cache: Cache,
) {
    fun fetch(key: String): Item = store.load(key)

    fun warm(key: String): Item {
        val local = Cache()
        return local.load(key)
    }

    fun copied(key: String): Item {
        val copy = cache.copy()
        return copy.load(key)
    }

    fun injected(key: String): Item? = cache?.load(key)
}
"#,
        ),
    ]
}

#[test]
fn a_kotlin_call_through_a_typed_receiver_binds_to_that_types_method_only() {
    let files = kotlin_files();
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        assert_binds_only(
            label,
            &calls_from(&relations, &files, "Service.fetch"),
            "Store.load",
            &["Cache.load", "SqlStore.load"],
        );
        assert_binds_only(
            label,
            &calls_from(&relations, &files, "Service.warm"),
            "Cache.load",
            &["Store.load", "SqlStore.load"],
        );
        assert_binds_only(
            label,
            &calls_from(&relations, &files, "Service.copied"),
            "Cache.load",
            &["Store.load", "SqlStore.load"],
        );
        assert_binds_only(
            label,
            &calls_from(&relations, &files, "Service.injected"),
            "Cache.load",
            &["Store.load", "SqlStore.load"],
        );
    }
}

#[test]
fn a_kotlin_constructor_property_call_never_binds_the_callers_same_named_member() {
    let files = vec![fixture(
        "src/main/kotlin/app/Sample.kt",
        "package app\n\nclass Helper {\n    fun b() {}\n}\n\nclass Foo(private val h: Helper) {\n    fun a() {\n        h.b()\n    }\n\n    fun b() {}\n}\n",
    )];
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        assert_binds_only(
            label,
            &calls_from(&relations, &files, "Foo.a"),
            "Helper.b",
            &["Foo.b"],
        );
    }
}
