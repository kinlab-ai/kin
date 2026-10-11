// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Go calls through receivers the adapter's value-origin facts type, batch and
//! incremental: a call binds the method of the type its receiver is declared,
//! constructed or returned as, and never the same-named method of another
//! type or a same-named free function.

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

fn batch(files: &[FileParseData]) -> Vec<Relation> {
    let identities = files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect();
    link_cross_file(files, &identities).unwrap()
}

fn incremental(files: &[FileParseData]) -> Vec<Relation> {
    let mut linker = IncrementalLinker::new();
    for file in files {
        linker.add_file(&file.file_path, ArtifactId::new(), &file.entities);
    }
    link_cross_file_incremental(files, &linker).unwrap()
}

fn names(files: &[FileParseData]) -> HashMap<EntityId, String> {
    files
        .iter()
        .flat_map(|file| file.entities.iter())
        .map(|entity| (entity.id, entity.name.clone()))
        .collect()
}

fn id(files: &[FileParseData], name: &str) -> EntityId {
    files
        .iter()
        .flat_map(|file| file.entities.iter())
        .find(|entity| entity.name == name)
        .unwrap_or_else(|| panic!("no entity {name}"))
        .id
}

/// Every call edge from `caller`, as (target name, confidence, chain).
fn calls_from(
    relations: &[Relation],
    files: &[FileParseData],
    caller: &str,
) -> Vec<(String, f32, Option<String>)> {
    let names = names(files);
    let caller = id(files, caller);
    let mut out: Vec<_> = relations
        .iter()
        .filter(|relation| {
            relation.kind == RelationKind::Calls && relation.src.as_entity() == Some(caller)
        })
        .filter_map(|relation| {
            Some((
                names.get(&relation.dst.as_entity()?)?.clone(),
                relation.confidence,
                receiver_origin_chain(relation).map(str::to_string),
            ))
        })
        .collect();
    out.sort_by(|left, right| left.0.cmp(&right.0));
    out
}

fn store_package() -> Vec<FileParseData> {
    vec![
        fixture(
            "store/store.go",
            r#"package store

// Loader loads a value by key.
type Loader interface {
	Load(key string) string
}

type Store struct{}

func (s *Store) Load(key string) string { return key }

type Cache struct{}

func (c *Cache) Load(key string) string { return "" }

// NewStore builds an empty store.
func NewStore() *Store { return &Store{} }

type Server struct {
	store *Store
}

func (srv *Server) Get(key string) string { return srv.store.Load(key) }

func warm(keys []string) {
	s := NewStore()
	for _, key := range keys {
		s.Load(key)
	}
}
"#,
        ),
        fixture(
            "store/handler.go",
            r#"package store

func handle(c *Cache, l Loader) {
	c.Load("a")
	l.Load("b")
	built := &Store{}
	built.Load("c")
}

// Load is a free function sharing the method name, which a call through a
// receiver never reaches.
func Load() {}
"#,
        ),
    ]
}

#[test]
fn a_typed_receiver_binds_its_own_type_and_never_a_same_named_method() {
    let files = store_package();
    for (tier, relations) in [
        ("batch", batch(&files)),
        ("incremental", incremental(&files)),
    ] {
        let handle = calls_from(&relations, &files, "handle");
        let targets: Vec<&str> = handle.iter().map(|(name, _, _)| name.as_str()).collect();
        assert_eq!(
            targets,
            vec!["Cache.Load", "Loader.Load", "Store.Load"],
            "{tier}: each receiver binds its own type's method, the interface's for an interface, and neither the free Load nor the other type's Load: {handle:?}"
        );
        for (target, confidence, chain) in &handle {
            assert_eq!(*confidence, RECEIVER_ORIGIN_CONFIDENCE, "{tier} {target}");
            assert!(
                chain.is_some(),
                "{tier}: {target} names the chain that typed it"
            );
        }
        assert_eq!(
            handle
                .iter()
                .find(|(name, _, _)| name == "Loader.Load")
                .and_then(|(_, _, chain)| chain.as_deref()),
            Some("l: declared Loader -> Loader.Load"),
            "{tier}"
        );

        let warm: Vec<_> = calls_from(&relations, &files, "warm")
            .into_iter()
            .filter(|(name, _, _)| name.ends_with("Load"))
            .collect();
        assert_eq!(
            warm,
            vec![(
                "Store.Load".to_string(),
                RECEIVER_ORIGIN_CONFIDENCE,
                Some("s: NewStore() -> Store -> Store.Load".to_string())
            )],
            "{tier}: a constructor's declared result types the local it is bound to"
        );

        let get = calls_from(&relations, &files, "Server.Get");
        let targets: Vec<&str> = get.iter().map(|(name, _, _)| name.as_str()).collect();
        assert_eq!(
            targets,
            vec!["Store.Load"],
            "{tier}: a field's declared type types the call through it"
        );
    }
}

#[test]
fn an_untyped_receiver_keeps_the_name_tiers_it_always_had() {
    let files = vec![
        fixture(
            "store/store.go",
            "package store\n\ntype Store struct{}\n\nfunc (s *Store) Load() {}\n",
        ),
        fixture(
            "store/walk.go",
            "package store\n\nfunc walk(items []*Store) {\n\tfor _, item := range items {\n\t\titem.Load()\n\t}\n}\n",
        ),
    ];
    for relations in [batch(&files), incremental(&files)] {
        let walk = calls_from(&relations, &files, "walk");
        assert!(
            walk.iter().all(|(_, _, chain)| chain.is_none()),
            "a range variable carries no fact: {walk:?}"
        );
    }
}

/// A parameter typed with another package's interface binds the interface's
/// own method, and one typed with its concrete type binds the concrete
/// method: neither is guessed onto the other.
#[test]
fn a_qualified_interface_parameter_binds_the_interface_method() {
    let files = vec![
        fixture(
            "repo/repo.go",
            "package repo\n\n// Interface is satisfied by Concrete.\ntype Interface interface {\n\tRepoOwner() string\n}\n\n// Concrete implements Interface.\ntype Concrete struct {\n\towner string\n}\n\nfunc (c Concrete) RepoOwner() string { return c.owner }\n",
        ),
        fixture(
            "use/use.go",
            "package use\n\nimport \"example.com/dispatch/repo\"\n\nfunc ViaInterface(r repo.Interface) string {\n\treturn r.RepoOwner()\n}\n\nfunc Direct(c repo.Concrete) string {\n\treturn c.RepoOwner()\n}\n",
        ),
    ];
    for relations in [batch(&files), incremental(&files)] {
        let via: Vec<String> = calls_from(&relations, &files, "ViaInterface")
            .into_iter()
            .map(|(name, _, _)| name)
            .collect();
        assert_eq!(via, vec!["Interface.RepoOwner"]);
        let direct = calls_from(&relations, &files, "Direct");
        assert_eq!(
            direct,
            vec![(
                "Concrete.RepoOwner".to_string(),
                RECEIVER_ORIGIN_CONFIDENCE,
                Some("c: declared Concrete -> Concrete.RepoOwner".to_string())
            )]
        );
    }
}
