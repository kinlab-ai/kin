// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Swift receivers typed by declarations, batch and incremental.
//!
//! The Swift adapter records what each name a callable binds holds (a
//! declared type, an initializer, a same-file function's declared return, a
//! method called on one of those, the enclosing type's property) and hands a
//! call through such a receiver to the linker with its receiver and the bare
//! method. The value-origin tier follows the declarations to one class and
//! asks it for the method. Two classes declare `save`; only the one the
//! receiver's declarations name may gain the caller.

use std::collections::HashMap;

use kin_index::receiver_origin::receiver_origin_chain;
use kin_index::resolution::RECEIVER_ORIGIN_CONFIDENCE;
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

const STORAGE: &str = r#"import Foundation

final class Store {
    func save() {}
    func child() -> Store? { return nil }
}

final class Cache {
    func save() {}
}

final class Registry {
    func lookup(name: String) throws -> Store { return Store() }
}
"#;

const ARCHIVE: &str = r#"extension Store {
    func archive() {}
}
"#;

const SYNC: &str = r#"import Foundation

final class SyncService {
    let store: Store

    init(store: Store) {
        self.store = store
    }

    func flush(primary: Store, backup: Cache?) {
        primary.save()
        backup?.save()
        store.save()
        self.reset()
    }

    func rebuild(registry: Registry) throws {
        let fresh = Store()
        fresh.save()
        let found = try registry.lookup(name: "main")
        found.save()
        let made = makeStore()
        made.save()
        if let child = found.child() {
            child.save()
        }
        fresh.archive()
    }

    func reset() {}
}

func makeStore() -> Store {
    return Store()
}
"#;

fn files() -> Vec<FileParseData> {
    vec![
        fixture("Sources/Storage/Store.swift", STORAGE),
        fixture("Sources/Storage/Store+Archive.swift", ARCHIVE),
        fixture("Sources/Sync/SyncService.swift", SYNC),
    ]
}

/// Every chain the receiver-origin records on the one edge from `caller` to
/// `target` spell: the edge carries each site's evidence.
fn edge_chains(
    relations: &[Relation],
    files: &[FileParseData],
    caller: &Entity,
    target: &str,
) -> Vec<String> {
    let target_ids: Vec<EntityId> = files
        .iter()
        .flat_map(|file| file.entities.iter())
        .filter(|entity| entity.name == target)
        .map(|entity| entity.id)
        .collect();
    relations
        .iter()
        .filter(|relation| {
            relation.kind == RelationKind::Calls
                && relation.src == GraphNodeId::Entity(caller.id)
                && relation
                    .dst
                    .as_entity()
                    .is_some_and(|dst| target_ids.contains(&dst))
        })
        .flat_map(|relation| relation.evidence.iter())
        .filter(|record| {
            record.parser_rule.as_deref() == Some(kin_index::receiver_origin::RECEIVER_ORIGIN_RULE)
        })
        .filter_map(|record| record.token.clone())
        .collect()
}

fn chains_to<'a>(calls: &'a [(String, String, f32, Option<String>)], target: &str) -> Vec<&'a str> {
    calls
        .iter()
        .filter(|(_, name, confidence, _)| {
            name == target && *confidence == RECEIVER_ORIGIN_CONFIDENCE
        })
        .filter_map(|(_, _, _, chain)| chain.as_deref())
        .collect()
}

#[test]
fn a_typed_parameter_optional_and_property_bind_only_their_class() {
    let files = files();
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        let flush = entity(
            &files,
            "Sources/Sync/SyncService.swift",
            "SyncService.flush",
        );
        let calls = calls_from(&relations, &files, flush);
        assert!(
            !chains_to(&calls, "Store.save").is_empty(),
            "{label}: {calls:?}"
        );
        let store_chains = edge_chains(&relations, &files, flush, "Store.save");
        for expected in ["primary: declared Store", "store: declared Store"] {
            assert!(
                store_chains.iter().any(|chain| chain.starts_with(expected)),
                "{label}: {expected} in {store_chains:?}"
            );
        }
        assert!(
            chains_to(&calls, "Cache.save")
                .iter()
                .any(|chain| chain.starts_with("backup: declared Cache")),
            "{label}: an optional of one type is that type: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|(_, name, _, _)| name == "SyncService.reset"),
            "{label}: {calls:?}"
        );
    }
}

#[test]
fn initializers_declared_returns_and_unwrapped_optionals_bind_the_class() {
    let files = files();
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        let rebuild = entity(
            &files,
            "Sources/Sync/SyncService.swift",
            "SyncService.rebuild",
        );
        let calls = calls_from(&relations, &files, rebuild);
        let chains = chains_to(&calls, "Store.save");
        assert!(
            !chains.is_empty(),
            "{label}: the rebuild calls bind Store.save: {calls:?}"
        );
        assert!(
            !calls.iter().any(|(_, name, _, _)| name == "Cache.save"),
            "{label}: another class's save must gain no caller: {calls:?}"
        );
        for (_, name, confidence, chain) in &calls {
            if name == "Store.save" {
                assert_eq!(
                    *confidence, RECEIVER_ORIGIN_CONFIDENCE,
                    "{label}: {calls:?}"
                );
                assert!(chain.is_some(), "{label}: {calls:?}");
            }
        }
    }
}

#[test]
fn every_rebuild_receiver_reaches_store_through_its_own_chain() {
    // One edge per (caller, target) carries every site's evidence; the
    // chains name each receiver's declarations.
    let files = files();
    let relations = link_batch(&files);
    let rebuild = entity(
        &files,
        "Sources/Sync/SyncService.swift",
        "SyncService.rebuild",
    );
    let chains = edge_chains(&relations, &files, rebuild, "Store.save");
    for expected in [
        "fresh: Store(...)",
        "found: declared Registry -> Registry.lookup() -> Store",
        "made: makeStore() -> Store",
        "child: declared Registry -> Registry.lookup() -> Store -> Store.child() -> Store",
    ] {
        assert!(
            chains.iter().any(|chain| chain.starts_with(expected)),
            "{expected} in {chains:?}"
        );
    }
}

#[test]
fn a_method_in_another_files_extension_gains_no_wrong_edge() {
    // The tier asks the class for a method in the file that declares the
    // class; an extension in another file is outside what it reads, so the
    // call binds nothing rather than a guess.
    let files = files();
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        let rebuild = entity(
            &files,
            "Sources/Sync/SyncService.swift",
            "SyncService.rebuild",
        );
        let calls = calls_from(&relations, &files, rebuild);
        assert!(
            calls
                .iter()
                .filter(|(_, name, _, _)| name.ends_with("archive"))
                .all(
                    |(file, name, _, _)| file == "Sources/Storage/Store+Archive.swift"
                        && name == "Store.archive"
                ),
            "{label}: {calls:?}"
        );
    }
}
