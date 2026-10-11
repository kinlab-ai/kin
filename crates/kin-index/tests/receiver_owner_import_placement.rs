// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A receiver-method call keeps the owner its import names as a candidate
//! unless the linker can place the import somewhere else.
//!
//! A `::` crate path is placed only by the Cargo authority that owns the
//! importing file, so a file no target owns keeps the builder it imports as a
//! name-only candidate while a file the target owns refuses a crate it does
//! not know. A Go import binds a package rather than a type, so a Go call
//! through a receiver the value-origin facts cannot type keeps the bare-name
//! candidates it had before the adapter recorded its receiver.

use std::collections::{BTreeMap, HashMap};

use kin_index::linker::{link_cross_file_with_rust_project, ArtifactIdentityMap};
use kin_index::resolution::RECEIVER_NAME_FANOUT_CONFIDENCE;
use kin_index::rust_project::{RustProjectAuthority, RustProjectLimits};
use kin_index::{
    link_cross_file, link_cross_file_incremental, link_cross_file_incremental_with_completeness,
    FileParseCompletenessMap, FileParseData, IncrementalLinker, IndexPipeline,
};
use kin_model::{
    ArtifactId, Entity, EntityId, FilePathId, Hash256, Relation, RelationKind, RepoPath,
    ResolvedArtifact, ResolvedTree, TreeEntry,
};

fn index(path: &str, source: &str) -> FileParseData {
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

/// Every call edge from the entity named `caller` in `file`, as (target name,
/// confidence), sorted.
fn calls_from(
    relations: &[Relation],
    files: &[FileParseData],
    file: &str,
    caller: &str,
) -> Vec<(String, f32)> {
    let names: HashMap<EntityId, &str> = files
        .iter()
        .flat_map(|parsed| parsed.entities.iter())
        .map(|entity| (entity.id, entity.name.as_str()))
        .collect();
    let caller = files
        .iter()
        .find(|parsed| parsed.file_path == file)
        .and_then(|parsed| parsed.entities.iter().find(|entity| entity.name == caller))
        .unwrap_or_else(|| panic!("no {caller} in {file}"))
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
            ))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

const BUILDER: &str = "pub struct SearcherBuilder { multi_line: bool }\n\n\
    impl SearcherBuilder {\n    \
    pub fn new() -> SearcherBuilder { SearcherBuilder { multi_line: false } }\n    \
    pub fn multi_line(&mut self, yes: bool) -> &mut SearcherBuilder { self.multi_line = yes; self }\n\
    }\n";

const CALLER: &str = "use grep::searcher::SearcherBuilder;\n\n\
    pub fn run() {\n    \
    let mut builder = SearcherBuilder::new();\n    \
    builder.multi_line(true);\n\
    }\n";

/// A Cargo package whose library owns `src/user.rs`, a source file no target
/// owns, and a builder in a directory no target owns either. Both callers
/// import the builder through a crate `grep` the package does not declare.
fn rust_workspace() -> (
    Vec<FileParseData>,
    Vec<Entity>,
    ResolvedTree,
    BTreeMap<Hash256, Vec<u8>>,
) {
    let sources = [
        ("Cargo.toml", "[package]\nname='fixture'\nedition='2021'\n"),
        ("src/lib.rs", "pub mod user;\n"),
        ("src/user.rs", CALLER),
        ("tools/probe.rs", CALLER),
        ("crates/searcher/src/searcher/mod.rs", BUILDER),
    ];
    let mut artifacts = Vec::new();
    let mut blobs = BTreeMap::new();
    let mut files = Vec::new();
    for (path, source) in sources {
        let hash = kin_blobs::digest(source.as_bytes());
        blobs.insert(hash, source.as_bytes().to_vec());
        artifacts.push(ResolvedArtifact::new(
            ArtifactId::new(),
            RepoPath::from_utf8(path).unwrap(),
            TreeEntry::Blob {
                hash,
                executable: false,
            },
        ));
        if path.ends_with(".rs") {
            files.push(index(path, source));
        }
    }
    let entities = files
        .iter()
        .flat_map(|file| file.entities.iter().cloned())
        .collect();
    (
        files,
        entities,
        ResolvedTree::from_artifacts(artifacts).unwrap(),
        blobs,
    )
}

#[test]
fn a_crate_path_no_authority_places_keeps_the_owner_it_names() {
    let (files, entities, tree, blobs) = rust_workspace();
    let authority =
        RustProjectAuthority::from_admitted_tree(&tree, RustProjectLimits::default(), |hash| {
            blobs
                .get(&hash)
                .cloned()
                .ok_or("missing fixture body".into())
        })
        .unwrap();
    assert!(authority.contains_source("src/user.rs"));
    assert!(!authority.contains_source("tools/probe.rs"));

    let identities: ArtifactIdentityMap = files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect();
    let completeness: FileParseCompletenessMap = files
        .iter()
        .map(|file| (file.file_path.clone(), kin_model::ParseCompleteness::Full))
        .collect();
    let batch = link_cross_file_with_rust_project(
        &files.iter().collect::<Vec<_>>(),
        &entities.iter().collect::<Vec<_>>(),
        &identities,
        &completeness,
        &authority,
    )
    .unwrap();
    let mut linker = IncrementalLinker::new();
    for file in &files {
        linker.add_file(&file.file_path, identities[&file.file_path], &file.entities);
    }
    linker.install_rust_project(authority, &entities).unwrap();
    let incremental =
        link_cross_file_incremental_with_completeness(&files, &linker, &completeness).unwrap();

    for (label, relations) in [("batch", batch), ("incremental", incremental)] {
        // No target owns the probe, so nothing places `grep::searcher`: the
        // builder its import names stays a candidate, and only a name-only one.
        let probe = calls_from(&relations, &files, "tools/probe.rs", "run");
        assert!(
            probe.contains(&(
                "SearcherBuilder::multi_line".to_string(),
                RECEIVER_NAME_FANOUT_CONFIDENCE
            )),
            "{label}: {probe:?}"
        );
        // The library owns the user file and declares no crate `grep`, so its
        // import names something outside the repository.
        let user = calls_from(&relations, &files, "src/user.rs", "run");
        assert!(
            !user
                .iter()
                .any(|(name, _)| name == "SearcherBuilder::multi_line"),
            "{label}: {user:?}"
        );
    }
}

#[test]
fn an_untyped_go_receiver_keeps_its_bare_name_candidates() {
    let files = vec![
        index(
            "internal/buf/buffer.go",
            "package buf\n\ntype Buffer struct{ data []byte }\n\n\
             func (b *Buffer) Write(p []byte) (int, error) { return len(p), nil }\n",
        ),
        index(
            "internal/count/counter.go",
            "package count\n\ntype Counter struct{ n int }\n\n\
             func (c *Counter) Write(p []byte) (int, error) { return len(p), nil }\n",
        ),
        // `Writer` is no type package app declares, so the facts that record
        // the parameter's type cannot type the call through it.
        index(
            "cmd/app/emit.go",
            "package app\n\nfunc emit(sink Writer, payload []byte) {\n\tsink.Write(payload)\n}\n",
        ),
    ];
    let ids: HashMap<String, ArtifactId> = files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect();
    let mut linker = IncrementalLinker::new();
    for file in &files {
        linker.add_file(&file.file_path, ids[&file.file_path], &file.entities);
    }
    for (label, relations) in [
        ("batch", link_cross_file(&files, &ids).unwrap()),
        (
            "incremental",
            link_cross_file_incremental(&files, &linker).unwrap(),
        ),
    ] {
        let emit = calls_from(&relations, &files, "cmd/app/emit.go", "emit");
        assert_eq!(
            emit,
            [
                ("Buffer.Write".to_string(), RECEIVER_NAME_FANOUT_CONFIDENCE),
                ("Counter.Write".to_string(), RECEIVER_NAME_FANOUT_CONFIDENCE),
            ],
            "{label}"
        );
    }
}
