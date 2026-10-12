// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Source-bound Python lexical Contains, through every local linking path.

use kin_index::{FileParseData, IncrementalLinker, IndexPipeline, RelationSourceIndex};
use kin_model::{ArtifactId, EntityId, EntityKind, FilePathId, Relation, RelationKind};

fn indexed(path: &str, source: &str) -> (FileParseData, Vec<Relation>) {
    let bytes = source.as_bytes();
    let indexed = IndexPipeline::new()
        .index_file_content_with_tests(&FilePathId::new(path), bytes, kin_blobs::digest(bytes))
        .expect("source-bound indexing")
        .indexed_file;
    (
        FileParseData {
            file_path: path.to_string(),
            entities: indexed.entities,
            relations: indexed.extracted_relations,
            imports: indexed.imports,
        },
        indexed.relations,
    )
}

fn id(file: &FileParseData, name: &str, kind: EntityKind) -> EntityId {
    file.entities
        .iter()
        .find(|entity| entity.name == name && entity.kind == kind)
        .unwrap()
        .id
}

fn edges(files: &[FileParseData]) -> [Vec<Relation>; 2] {
    let identities = files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect();
    let batch = kin_index::link_cross_file(files, &identities).expect("batch");
    let mut incremental = IncrementalLinker::new();
    for file in files {
        incremental.add_file(&file.file_path, ArtifactId::new(), &file.entities);
    }
    incremental.record_class_bases(files);
    let incremental =
        kin_index::link_cross_file_incremental(files, &incremental).expect("incremental");
    [batch, incremental]
}

fn has(relations: &[Relation], kind: RelationKind, parent: EntityId, child: EntityId) -> bool {
    relations.iter().any(|relation| {
        relation.kind == kind
            && relation.src.as_entity() == Some(parent)
            && relation.dst.as_entity() == Some(child)
    })
}

#[test]
fn same_named_equal_span_module_and_function_have_distinct_contains_identities() {
    let (file, local) = indexed("pkg/same.py", "def same(): pass");
    let module = id(&file, "same", EntityKind::Module);
    let function = id(&file, "same", EntityKind::Function);
    assert_ne!(module, function);
    for relations in std::iter::once(local).chain(edges(std::slice::from_ref(&file))) {
        assert!(has(&relations, RelationKind::Contains, module, function));
        assert!(!has(&relations, RelationKind::Contains, function, function));
    }
}

#[test]
fn a_same_named_module_and_class_keep_module_members_and_method_members_separate() {
    let (file, local) = indexed(
        "pkg/Box.py",
        "class Box:\n    if enabled:\n        def wide(self): pass",
    );
    let module = id(&file, "Box", EntityKind::Module);
    let class = id(&file, "Box", EntityKind::Class);
    let method = id(&file, "Box.wide", EntityKind::Method);
    for relations in std::iter::once(local).chain(edges(std::slice::from_ref(&file))) {
        assert!(has(&relations, RelationKind::Contains, module, class));
        assert!(has(&relations, RelationKind::Contains, class, method));
        assert!(!has(&relations, RelationKind::Contains, module, method));
    }
}

#[test]
fn decorated_and_local_block_members_keep_their_exact_parent_and_provenance() {
    let (file, local) = indexed(
        "pkg/blocks.py",
        "\
if enabled:
    @decorate
    def outer():
        try:
            class Local:
                def run(self): pass
        finally:
            pass
",
    );
    let module = id(&file, "blocks", EntityKind::Module);
    let outer = id(&file, "outer", EntityKind::Function);
    let class = id(&file, "outer.<locals>.Local", EntityKind::Class);
    let method = id(&file, "outer.<locals>.Local.run", EntityKind::Method);
    for relations in std::iter::once(local).chain(edges(std::slice::from_ref(&file))) {
        for (parent, child, rule) in [
            (module, outer, "module_contains"),
            (outer, class, "lexical_contains"),
            (class, method, "lexical_contains"),
        ] {
            let relation = relations
                .iter()
                .find(|relation| {
                    relation.kind == RelationKind::Contains
                        && relation.src.as_entity() == Some(parent)
                        && relation.dst.as_entity() == Some(child)
                })
                .expect("exact lexical edge");
            assert!(relation
                .evidence
                .iter()
                .any(|evidence| evidence.parser_rule.as_deref() == Some(rule)
                    && evidence.source_span.is_some()));
        }
    }
}

#[test]
fn repeated_unconditional_declarations_get_distinct_module_members_without_a_name_slot() {
    let (file, local) = indexed(
        "pkg/redefine.py",
        "def pick(): return 1\ndef pick(): return 2\n",
    );
    let module = id(&file, "redefine", EntityKind::Module);
    let members: Vec<_> = file
        .entities
        .iter()
        .filter(|entity| entity.kind == EntityKind::Function && entity.name == "pick")
        .map(|entity| entity.id)
        .collect();
    assert_eq!(members.len(), 2);
    for relations in std::iter::once(local).chain(edges(std::slice::from_ref(&file))) {
        for member in &members {
            assert!(has(&relations, RelationKind::Contains, module, *member));
        }
    }
}

#[test]
fn malformed_or_missing_child_sites_do_not_fall_back_to_names_in_either_linker() {
    let (file, _) = indexed("pkg/blocks.py", "if enabled:\n    def child(): pass\n");
    let module = id(&file, "blocks", EntityKind::Module);
    let child = id(&file, "child", EntityKind::Function);
    for mutation in [0, 1, 2] {
        let mut broken = file.clone();
        let relation = broken
            .relations
            .iter_mut()
            .find(|relation| relation.kind == RelationKind::Contains)
            .unwrap();
        match mutation {
            0 => relation.site.as_mut().unwrap().start_byte += 1,
            1 => relation.dst_name = "missing".into(),
            _ => relation.src_name = "missing".into(),
        }
        assert!(RelationSourceIndex::new(&broken.entities)
            .resolve_lexical_contains(relation)
            .is_none());
        for relations in edges(&[broken]) {
            assert!(!has(&relations, RelationKind::Contains, module, child));
        }
    }
}

#[test]
fn duplicate_exact_child_identity_or_wrong_source_file_refuses_containment() {
    let (mut file, _) = indexed("pkg/blocks.py", "def child(): pass\n");
    let relation = file
        .relations
        .iter()
        .find(|relation| relation.kind == RelationKind::Contains)
        .unwrap()
        .clone();
    let mut duplicate = file
        .entities
        .iter()
        .find(|entity| entity.kind == EntityKind::Function)
        .unwrap()
        .clone();
    duplicate.id = EntityId::new();
    file.entities.push(duplicate);
    assert!(RelationSourceIndex::new(&file.entities)
        .resolve_lexical_contains(&relation)
        .is_none());
    file.entities.pop();
    let module = file
        .entities
        .iter_mut()
        .find(|entity| entity.kind == EntityKind::Module)
        .unwrap();
    module.file_origin = Some(FilePathId::new("other.py"));
    assert!(RelationSourceIndex::new(&file.entities)
        .resolve_lexical_contains(&relation)
        .is_none());
}

#[test]
fn ambiguous_conditional_calls_and_bases_cannot_resolve_to_unrelated_namesakes() {
    let (file, _) = indexed(
        "pkg/ambiguous.py",
        "\
from decoy import pick
if flag:
    def pick(): return 1
def run(): return pick()
if flag:
    class Twin(Left): pass
else:
    class Twin(Right): pass
class Derived(Twin):
    def process(self): pass
",
    );
    let (decoy, _) = indexed(
        "decoy.py",
        "def pick(): return 2\nclass Twin:\n    def process(self): pass\n",
    );
    let run = id(&file, "run", EntityKind::Function);
    let pick = id(&decoy, "pick", EntityKind::Function);
    assert!(!file
        .entities
        .iter()
        .any(|entity| ["pick", "Twin", "Derived"].contains(&entity.name.as_str())));
    for relations in edges(&[file, decoy]) {
        assert!(!has(&relations, RelationKind::Calls, run, pick));
        assert!(!relations
            .iter()
            .any(|relation| relation.kind == RelationKind::Overrides));
    }
}

#[test]
fn conditional_global_and_nonlocal_writes_do_not_prove_calls_to_the_old_binding() {
    for source in [
        "def pick(): return 1\ndef caller():\n    global pick\n    if flag:\n        def pick(): return 2\n    return pick()\n",
        "def outer():\n    def pick(): return 1\n    def caller():\n        nonlocal pick\n        if flag:\n            def pick(): return 2\n        return pick()\n    return pick()\n",
    ] {
        let (file, local) = indexed("pkg/outward.py", source);
        let target = file.entities.iter().find(|entity| entity.kind == EntityKind::Function && (entity.name == "pick" || entity.name == "outer.<locals>.pick")).unwrap().id;
        for relations in std::iter::once(local).chain(edges(std::slice::from_ref(&file))) {
            assert!(!relations.iter().any(|relation| relation.kind == RelationKind::Calls && relation.dst.as_entity() == Some(target)));
        }
    }
}
