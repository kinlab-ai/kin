// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! C++ receivers typed by declarations, batch and incremental.
//!
//! The C++ adapter settles a receiver its own file declares (`Store& s`,
//! `auto s = std::make_unique<Store>()`, `auto s = Store{}`, a field of the
//! enclosing class, a same-file function's or method's declared return) and
//! emits `Store::save`, reading declarations by block scope. Two classes
//! declare `save`; only the one the receiver's declarations name may gain the
//! caller. A receiver only another file settles keeps its bare call and the
//! name candidates the linker offers for it, rather than losing them.

use std::collections::HashMap;

use kin_index::receiver_origin::receiver_origin_chain;
use kin_index::resolution::{RelationResolution, RECEIVER_ORIGIN_CONFIDENCE};
use kin_index::{
    link_cross_file, link_cross_file_incremental, FileParseData, IncrementalLinker, IndexPipeline,
};
use kin_model::{ArtifactId, Entity, EntityId, FilePathId, GraphNodeId, Relation, RelationKind};

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
    link_cross_file(files, &ids).unwrap()
}

fn link_incremental(files: &[FileParseData]) -> Vec<Relation> {
    let mut linker = IncrementalLinker::new();
    for file in files {
        linker.add_file(&file.file_path, ArtifactId::new(), &file.entities);
    }
    link_cross_file_incremental(files, &linker).unwrap()
}

fn entity<'a>(files: &'a [FileParseData], file: &str, name: &str) -> &'a Entity {
    files
        .iter()
        .find(|parsed| parsed.file_path == file)
        .and_then(|parsed| parsed.entities.iter().find(|entity| entity.name == name))
        .unwrap_or_else(|| panic!("no entity {name} in {file}"))
}

/// Every call edge from `caller`, as (target file, target name, confidence,
/// chain).
fn calls_from(
    relations: &[Relation],
    files: &[FileParseData],
    caller: &Entity,
) -> Vec<(String, String, f32, Option<String>)> {
    let placed: HashMap<EntityId, (&str, &str)> = files
        .iter()
        .flat_map(|file| {
            file.entities
                .iter()
                .map(move |entity| (entity.id, (file.file_path.as_str(), entity.name.as_str())))
        })
        .collect();
    let mut out: Vec<_> = relations
        .iter()
        .filter(|relation| {
            relation.kind == RelationKind::Calls && relation.src == GraphNodeId::Entity(caller.id)
        })
        .filter_map(|relation| {
            let (file, name) = placed.get(&relation.dst.as_entity()?)?;
            Some((
                file.to_string(),
                name.to_string(),
                relation.confidence,
                receiver_origin_chain(relation).map(str::to_string),
            ))
        })
        .collect();
    out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    out
}

const STORAGE: &str = "#pragma once\n\
namespace storage {\n\
class Store {\n public:\n  void save() {}\n};\n\
class Cache {\n public:\n  void save() {}\n};\n\
class Registry {\n public:\n  Store& lookup(const char* name);\n};\n\
}  // namespace storage\n";

const JOBS: &str = "#include \"storage.hpp\"\n\
#include <memory>\n\
using namespace storage;\n\n\
Store make_local();\n\n\
class Shelf {\n public:\n  Store& current();\n};\n\n\
void by_reference(Store& store, Cache* cache) {\n  store.save();\n  cache->save();\n}\n\n\
void by_construction() {\n  auto owned = std::make_unique<Store>();\n  owned->save();\n  auto braced = Store{};\n  braced.save();\n  auto made = make_local();\n  made.save();\n}\n\n\
void by_method(Shelf& shelf) {\n  auto top = shelf.current();\n  top.save();\n}\n\n\
class Worker {\n public:\n  void run() {\n    store_->save();\n    this->stop();\n  }\n  void stop() {}\n private:\n  std::unique_ptr<Store> store_;\n};\n\n\
void shadowed(bool fast) {\n  if (fast) {\n    Store held;\n    held.save();\n  } else {\n    Cache held;\n    held.save();\n  }\n}\n\n\
void across_files(Registry& registry) {\n  auto found = registry.lookup(\"main\");\n  found.save();\n}\n";

fn files() -> Vec<FileParseData> {
    vec![
        fixture("include/storage.hpp", STORAGE),
        fixture("src/jobs.cpp", JOBS),
    ]
}

fn targets(calls: &[(String, String, f32, Option<String>)]) -> Vec<&str> {
    calls.iter().map(|(_, name, _, _)| name.as_str()).collect()
}

#[test]
fn a_receiver_the_file_declares_binds_only_its_own_class() {
    let files = files();
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        let by_reference = entity(&files, "src/jobs.cpp", "by_reference");
        let calls = calls_from(&relations, &files, by_reference);
        assert_eq!(
            targets(&calls),
            vec!["Cache::save", "Store::save"],
            "{label}"
        );

        let constructed = entity(&files, "src/jobs.cpp", "by_construction");
        let calls = calls_from(&relations, &files, constructed);
        assert_eq!(
            targets(&calls),
            vec!["Store::save", "make_local"],
            "{label}: make_unique, a braced construction and a same-file \
             factory's declared return all name Store: {calls:?}"
        );

        let by_method = entity(&files, "src/jobs.cpp", "by_method");
        let calls = calls_from(&relations, &files, by_method);
        assert_eq!(targets(&calls), vec!["Store::save"], "{label}: {calls:?}");

        let run = entity(&files, "src/jobs.cpp", "Worker::run");
        let calls = calls_from(&relations, &files, run);
        assert_eq!(
            targets(&calls),
            vec!["Store::save", "Worker::stop"],
            "{label}: a unique_ptr field names its pointee and `this` its class: {calls:?}"
        );
    }
}

#[test]
fn a_name_two_blocks_bind_to_different_classes_is_not_typed_by_the_first() {
    let files = files();
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        let shadowed = entity(&files, "src/jobs.cpp", "shadowed");
        let calls = calls_from(&relations, &files, shadowed);
        // Before the census the first declaration typed both reads, so the
        // `Cache` read bound to `Store::save` alone. Neither read has one
        // declared type, so both stay bare and each class is a candidate.
        assert_eq!(
            targets(&calls),
            vec!["Cache::save", "Store::save"],
            "{label}"
        );
        for (_, _, confidence, _) in &calls {
            assert_eq!(
                RelationResolution::from_confidence(*confidence),
                RelationResolution::NameOnly,
                "{label}: {calls:?}"
            );
        }
    }
}

#[test]
fn a_receiver_only_another_file_settles_keeps_its_candidates() {
    let files = files();
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        let across = entity(&files, "src/jobs.cpp", "across_files");
        let calls = calls_from(&relations, &files, across);
        assert!(
            calls.iter().any(|(_, name, _, _)| name == "Store::save"),
            "{label}: the real target stays among the candidates: {calls:?}"
        );
        assert!(
            calls.iter().all(|(_, _, confidence, chain)| chain.is_none()
                && *confidence < RECEIVER_ORIGIN_CONFIDENCE),
            "{label}: {calls:?}"
        );
    }
}
