// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use std::path::Path;

use kin_lsp::enrichment::{EntityIndex, EntityRef};
use kin_model::{EntityId, EntityKind, FilePathId, RelationKind};
use kin_parser::{LanguageAdapter, PythonAdapter};

fn parse(source: &str) -> kin_parser::ParseOutput {
    let tree = PythonAdapter.parse(source.as_bytes()).unwrap();
    assert!(!tree.root_node().has_error());
    PythonAdapter
        .extract(&tree, source.as_bytes(), &FilePathId::new("app.py"))
        .unwrap()
}

#[test]
fn class_attributes_and_literal_keys_have_the_same_parser_and_lsp_owner() {
    let source = "def build(): pass\nclass App:\n    response_class = build\n    defaults = {\n        'DEBUG': build(),\n        'NESTED': {\n            'CACHE': build(),\n        },\n    }\n    timeout = build()\n";
    let output = parse(source);
    let index = EntityIndex::new(
        output
            .entities
            .iter()
            .map(|entity| EntityRef {
                id: EntityId::new(),
                name: entity.name.clone(),
                file_path: "app.py".into(),
                start_line: entity.span.start_line,
                start_col: entity.span.start_col,
                end_line: entity.span.end_line,
                name_line: entity.span.start_line,
                name_col: entity.span.start_col,
                declares_name: entity.kind != EntityKind::Module,
                declares_only: false,
                kind: entity.kind,
            })
            .collect(),
        Path::new("/repo"),
    );
    for (line, owner) in [
        (2, "App.response_class"),
        (4, "App.defaults.DEBUG"),
        (6, "App.defaults.NESTED.CACHE"),
        (9, "App.timeout"),
    ] {
        let entity = output
            .entities
            .iter()
            .find(|entity| entity.name == owner)
            .unwrap();
        assert_eq!(entity.kind, EntityKind::Field);
        assert_eq!(
            index.find_at("file:///repo/app.py", line).unwrap().name,
            owner
        );
        let occurrences: Vec<_> = output
            .relations
            .iter()
            .filter(|relation| {
                matches!(
                    relation.kind,
                    RelationKind::Calls | RelationKind::References
                ) && relation.dst_name == "build"
                    && relation
                        .site
                        .as_ref()
                        .is_some_and(|site| site.start_line == line)
            })
            .collect();
        assert!(!occurrences.is_empty(), "missing use on line {line}");
        assert!(
            occurrences
                .iter()
                .all(|relation| relation.src_name == owner),
            "{occurrences:?}"
        );
        assert_eq!(
            occurrences
                .iter()
                .filter(|relation| relation.kind == RelationKind::Calls)
                .count(),
            usize::from(line != 2)
        );
        let body = &source[entity.span.start_byte..entity.span.end_byte];
        assert!(
            body.contains("build"),
            "field source is the full initializer"
        );
    }
    assert!(output
        .relations
        .iter()
        .any(|relation| relation.kind == RelationKind::Contains
            && relation.src_name == "App.defaults"
            && relation.dst_name == "App.defaults.DEBUG"));
}

#[test]
fn compact_dynamic_and_duplicate_keys_do_not_fabricate_an_entity() {
    let output = parse("class App:\n    first = 1; second = 2\n    compact = {'KEY': 1}\n    values = {\n        'same': 1,\n        'same': 2,\n        'two-words': 3,\n        f'{name}': 4,\n        name: 5,\n        'left': 6, 'right': 7,\n        'OWN': 8,\n    }\n    repeated = 1\n    repeated = 2\n");
    let fields: Vec<_> = output
        .entities
        .iter()
        .filter(|e| e.kind == EntityKind::Field)
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(fields, ["App.compact", "App.values", "App.values.OWN"]);
}

#[test]
fn module_literal_keys_are_contained_by_the_existing_constant() {
    let output = parse("def build(): pass\nDEFAULTS = {\n    'DEBUG': build(),\n}\n");
    assert!(output
        .relations
        .iter()
        .any(|r| r.kind == RelationKind::Contains
            && r.src_name == "DEFAULTS"
            && r.dst_name == "DEFAULTS.DEBUG"));
    let calls: Vec<_> = output
        .relations
        .iter()
        .filter(|r| r.kind == RelationKind::Calls && r.dst_name == "build")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].src_name, "DEFAULTS.DEBUG");
}
