// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Rust calls through a receiver the adapter's value-origin facts type, linked
//! batch and incremental.
//!
//! `Store` and `Cache` both declare `load`, and `Summary` and `Tally` both
//! implement `Sink::matched`. A call binds through its receiver's declared or
//! constructed type to that type's own method, a call through a trait object
//! binds the trait's method and no implementation, and a receiver no fact
//! types gains no receiver-origin edge at all.

use std::collections::HashMap;

use kin_index::receiver_origin::receiver_origin_chain;
use kin_index::resolution::{RECEIVER_ORIGIN_CONFIDENCE, RECEIVER_ORIGIN_NAME_CONFIDENCE};
use kin_index::{
    link_cross_file, link_cross_file_incremental, FileParseData, IncrementalLinker, IndexPipeline,
};
use kin_model::{ArtifactId, Entity, EntityId, FilePathId, GraphNodeId, Relation, RelationKind};

const CACHE: &str = r#"pub struct Cache;

impl Cache {
    pub fn new() -> Self {
        Cache
    }

    pub fn load(&self) -> u8 {
        1
    }
}
"#;

const STORE: &str = r#"use crate::cache::Cache;

pub trait Sink {
    fn matched(&mut self) -> bool;
}

pub struct Store {
    cache: Cache,
}

impl Store {
    pub fn new() -> Self {
        Store { cache: Cache::new() }
    }

    pub fn load(&self) -> u8 {
        0
    }

    pub fn reader(&self) -> &Cache {
        &self.cache
    }

    pub fn refresh(&self) -> u8 {
        let reader = self.reader();
        reader.load()
    }
}

pub struct Summary;

impl Sink for Summary {
    fn matched(&mut self) -> bool {
        true
    }
}

pub struct Tally;

impl Sink for Tally {
    fn matched(&mut self) -> bool {
        false
    }
}
"#;

const APP: &str = r#"use crate::cache::Cache;
use crate::store::{Sink, Store};

pub fn run(sink: &mut dyn Sink) -> u8 {
    let store = Store::new();
    let cache = Cache::new();
    sink.matched();
    store.load() + cache.load()
}

pub fn only_store() -> u8 {
    let store: Store = Store::new();
    store.load()
}

pub fn generic<S: Sink>(sink: &mut S) -> bool {
    sink.matched()
}

pub fn untyped(thing: Thing) -> u8 {
    thing.load()
}
"#;

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

fn files() -> Vec<FileParseData> {
    vec![
        fixture("src/cache.rs", CACHE),
        fixture("src/store.rs", STORE),
        fixture("src/app.rs", APP),
    ]
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

fn entity<'a>(files: &'a [FileParseData], name: &str) -> &'a Entity {
    files
        .iter()
        .flat_map(|file| file.entities.iter())
        .find(|entity| entity.name == name)
        .unwrap_or_else(|| panic!("no entity {name}"))
}

/// Every call edge from `caller`: (target name, confidence, chain).
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
            relation.kind == RelationKind::Calls && relation.src == GraphNodeId::Entity(caller)
        })
        .filter_map(|relation| {
            Some((
                names.get(&relation.dst.as_entity()?)?.to_string(),
                relation.confidence,
                receiver_origin_chain(relation).map(str::to_string),
            ))
        })
        .collect();
    out.sort_by(|left, right| left.0.cmp(&right.0));
    out
}

fn linked() -> Vec<(&'static str, Vec<FileParseData>, Vec<Relation>)> {
    let batch_files = files();
    let batch = link_batch(&batch_files);
    let incremental_files = files();
    let incremental = link_incremental(&incremental_files);
    vec![
        ("batch", batch_files, batch),
        ("incremental", incremental_files, incremental),
    ]
}

#[test]
fn a_constructed_receiver_binds_its_own_type_and_never_the_same_named_method() {
    for (label, files, relations) in linked() {
        let calls = calls_from(&relations, &files, "run");
        let loads: Vec<(&str, f32)> = calls
            .iter()
            .filter(|(name, _, _)| name.ends_with("::load"))
            .map(|(name, confidence, _)| (name.as_str(), *confidence))
            .collect();
        assert_eq!(
            loads,
            vec![
                ("Cache::load", RECEIVER_ORIGIN_CONFIDENCE),
                ("Store::load", RECEIVER_ORIGIN_CONFIDENCE),
            ],
            "{label}: {calls:?}"
        );
        let store_chain = calls
            .iter()
            .find(|(name, _, _)| name == "Store::load")
            .and_then(|(_, _, chain)| chain.clone())
            .expect("the edge names its chain");
        assert!(
            store_chain.starts_with("store:") && store_chain.contains("Store.new() -> Store"),
            "{label}: {store_chain}"
        );

        let only_store = calls_from(&relations, &files, "only_store");
        assert!(
            only_store
                .iter()
                .any(|(name, confidence, _)| name == "Store::load"
                    && *confidence == RECEIVER_ORIGIN_CONFIDENCE),
            "{label}: {only_store:?}"
        );
        assert!(
            !only_store.iter().any(|(name, _, _)| name == "Cache::load"),
            "{label}: Cache::load must gain no caller from a Store receiver: {only_store:?}"
        );
    }
}

#[test]
fn a_chain_through_a_declared_return_reaches_the_returned_type() {
    for (label, files, relations) in linked() {
        let calls = calls_from(&relations, &files, "Store::refresh");
        let load = calls
            .iter()
            .find(|(name, _, _)| name == "Cache::load")
            .unwrap_or_else(|| panic!("{label}: {calls:?}"));
        assert_eq!(load.1, RECEIVER_ORIGIN_CONFIDENCE, "{label}");
        assert!(
            load.2
                .as_deref()
                .is_some_and(|chain| chain.contains("Store.reader() -> Cache")),
            "{label}: {load:?}"
        );
        assert!(
            !calls.iter().any(|(name, _, _)| name == "Store::load"),
            "{label}: {calls:?}"
        );
    }
}

#[test]
fn a_trait_object_or_bounded_generic_binds_the_trait_method_and_no_implementation() {
    for (label, files, relations) in linked() {
        for caller in ["run", "generic"] {
            let calls = calls_from(&relations, &files, caller);
            let matched: Vec<(&str, f32)> = calls
                .iter()
                .filter(|(name, _, _)| name.ends_with("::matched"))
                .map(|(name, confidence, _)| (name.as_str(), *confidence))
                .collect();
            assert_eq!(
                matched,
                vec![("Sink::matched", RECEIVER_ORIGIN_CONFIDENCE)],
                "{label} {caller}: {calls:?}"
            );
        }
    }
}

#[test]
fn a_receiver_no_fact_types_gains_no_receiver_origin_edge() {
    for (label, files, relations) in linked() {
        let calls = calls_from(&relations, &files, "untyped");
        assert!(
            calls.iter().all(|(_, confidence, chain)| chain.is_none()
                && *confidence != RECEIVER_ORIGIN_CONFIDENCE
                && *confidence != RECEIVER_ORIGIN_NAME_CONFIDENCE),
            "{label}: {calls:?}"
        );
    }
}
