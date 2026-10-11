// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! PHP receivers typed by declarations, batch and incremental.
//!
//! The PHP adapter records what each variable a callable binds once holds (a
//! declared parameter type, `new Store`, a method called on one of those, a
//! same-file function's declared return) and what each typed property holds,
//! and hands a call through such a receiver (`$store`, `$this->store`) to the
//! linker with its receiver and the bare method. The value-origin tier
//! follows the declarations to one class and asks it for the method. Two
//! classes declare `save`; only the one the receiver's declarations name may
//! gain the caller.

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

const STORE: &str = r#"<?php
namespace App\Models;

class Store
{
    public function save(): void {}
    public function child(): ?Store { return null; }
}
"#;

const CACHE: &str = r#"<?php
namespace App\Models;

class Cache
{
    public function save(): void {}
}
"#;

const REGISTRY: &str = r#"<?php
namespace App\Models;

class Registry
{
    public function lookup(string $name): Store { return new Store(); }
}
"#;

const CHECKOUT: &str = r#"<?php
namespace App\Services;

use App\Models\Store;
use App\Models\Registry;

class Checkout
{
    private Store $store;

    public function __construct(Store $store)
    {
        $this->store = $store;
    }

    public function place(Store $primary, Registry $registry): void
    {
        $primary->save();
        $this->store->save();
        $found = $registry->lookup('main');
        $found->save();
        $fresh = new Store();
        $fresh->save();
        $child = $found->child();
        $child->save();
        $this->audit();
    }

    public function untyped($anything): void
    {
        $anything->save();
    }

    public function twice(Store $s): void
    {
        $s->save();
        $s = make_cache();
        $s->save();
    }

    public function audit(): void {}
}
"#;

fn files() -> Vec<FileParseData> {
    vec![
        fixture("app/Models/Store.php", STORE),
        fixture("app/Models/Cache.php", CACHE),
        fixture("app/Models/Registry.php", REGISTRY),
        fixture("app/Services/Checkout.php", CHECKOUT),
    ]
}

/// Every chain the receiver-origin records on the edges from `caller` to
/// `target` spell.
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

#[test]
fn typed_parameters_properties_constructions_and_returns_bind_only_their_class() {
    let files = files();
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        let place = entity(&files, "app/Services/Checkout.php", "Checkout.place");
        let calls = calls_from(&relations, &files, place);
        let store_save = calls
            .iter()
            .find(|(_, name, _, _)| name == "Store.save")
            .unwrap_or_else(|| panic!("{label}: no Store.save edge in {calls:?}"));
        assert_eq!(
            store_save.2, RECEIVER_ORIGIN_CONFIDENCE,
            "{label}: {calls:?}"
        );
        assert!(
            !calls.iter().any(|(_, name, _, _)| name == "Cache.save"),
            "{label}: another class's save must gain no caller: {calls:?}"
        );
        assert!(
            calls.iter().any(|(_, name, _, _)| name == "Checkout.audit"),
            "{label}: {calls:?}"
        );
        let chains = edge_chains(&relations, &files, place, "Store.save");
        for expected in [
            "$primary: declared Store",
            "$this->store: declared Store",
            "$found: declared Registry -> Registry.lookup() -> Store",
            "$fresh: Store(...)",
            "$child: declared Registry -> Registry.lookup() -> Store -> Store.child() -> Store",
        ] {
            assert!(
                chains.iter().any(|chain| chain.starts_with(expected)),
                "{label}: {expected} in {chains:?}"
            );
        }
    }
}

#[test]
fn an_untyped_receiver_keeps_its_name_candidates() {
    let files = files();
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        let untyped = entity(&files, "app/Services/Checkout.php", "Checkout.untyped");
        let calls = calls_from(&relations, &files, untyped);
        for target in ["Store.save", "Cache.save"] {
            assert!(
                calls
                    .iter()
                    .any(|(_, name, confidence, chain)| name == target
                        && chain.is_none()
                        && *confidence < RECEIVER_ORIGIN_CONFIDENCE),
                "{label}: {target} in {calls:?}"
            );
        }
    }
}

#[test]
fn a_variable_written_twice_has_no_fact() {
    let files = files();
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        let twice = entity(&files, "app/Services/Checkout.php", "Checkout.twice");
        let calls = calls_from(&relations, &files, twice);
        assert!(
            calls.iter().all(|(_, _, _, chain)| chain.is_none()),
            "{label}: {calls:?}"
        );
    }
}
