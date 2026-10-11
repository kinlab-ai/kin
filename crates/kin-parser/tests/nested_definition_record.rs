// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The per-entity record of nested definitions an adapter did not admit.

use kin_model::{Entity, EntityKind, FilePathId, LanguageId};
use kin_parser::{
    attach_file_nested_definition_record, LanguageAdapter, NestedDefinitionCensus, PythonAdapter,
    UnadmittedNestedDefinition, FILE_NESTED_DEFINITIONS_NOT_ADMITTED_KEY,
    NESTED_DEFINITIONS_NOT_ADMITTED_KEY, NESTED_DEFINITIONS_RECORD_CAP,
};

const SOURCE: &str = "def factory():\n    pass\n\n\ndef other():\n    pass\n";

fn entities() -> Vec<Entity> {
    let adapter = PythonAdapter;
    let file = FilePathId::new("pkg/record.py");
    let tree = adapter.parse(SOURCE.as_bytes()).expect("parse");
    adapter
        .extract(&tree, SOURCE.as_bytes(), &file)
        .expect("extract")
        .entities
        .into_iter()
        .map(|entity| entity.into_entity(LanguageId::Python, &file))
        .collect()
}

fn unadmitted(name: &str, enclosing: &str, line: u32) -> UnadmittedNestedDefinition {
    UnadmittedNestedDefinition {
        name: name.to_string(),
        kind: EntityKind::Function,
        start_line: line,
        enclosing: Some(enclosing.to_string()),
        bases: Vec::new(),
    }
}

fn named<'a>(entities: &'a [Entity], name: &str) -> &'a Entity {
    entities
        .iter()
        .find(|entity| entity.name == name)
        .expect(name)
}

#[test]
fn the_count_goes_on_every_entity_and_the_list_on_the_enclosing_one() {
    let mut entities = entities();
    let mut census = NestedDefinitionCensus::default();
    census.record(unadmitted("factory.<locals>.helper", "factory", 1));
    census.record(UnadmittedNestedDefinition {
        name: "factory.<locals>.Double".into(),
        kind: EntityKind::Class,
        start_line: 1,
        enclosing: Some("factory".into()),
        bases: vec!["Base".into()],
    });
    attach_file_nested_definition_record(&mut entities, Some(&census));

    for entity in &entities {
        assert_eq!(
            entity
                .metadata
                .extra
                .get(FILE_NESTED_DEFINITIONS_NOT_ADMITTED_KEY),
            Some(&serde_json::Value::from(2u64)),
            "{}",
            entity.name
        );
    }
    let list = named(&entities, "factory")
        .metadata
        .extra
        .get(NESTED_DEFINITIONS_NOT_ADMITTED_KEY)
        .expect("listed on the enclosing entity");
    assert_eq!(
        list,
        &serde_json::json!([
            {"name": "factory.<locals>.helper", "kind": "function", "start_line": 1},
            {"name": "factory.<locals>.Double", "kind": "class", "start_line": 1, "bases": ["Base"]},
        ])
    );
    assert!(!named(&entities, "other")
        .metadata
        .extra
        .contains_key(NESTED_DEFINITIONS_NOT_ADMITTED_KEY));
}

#[test]
fn the_list_is_capped_and_the_count_carries_the_total() {
    let mut entities = entities();
    let mut census = NestedDefinitionCensus::default();
    for index in 0..40 {
        census.record(unadmitted(
            &format!("factory.<locals>.f{index}"),
            "factory",
            1,
        ));
    }
    attach_file_nested_definition_record(&mut entities, Some(&census));
    let factory = named(&entities, "factory");
    assert_eq!(
        factory
            .metadata
            .extra
            .get(NESTED_DEFINITIONS_NOT_ADMITTED_KEY)
            .and_then(|list| list.as_array())
            .map(Vec::len),
        Some(NESTED_DEFINITIONS_RECORD_CAP)
    );
    assert_eq!(
        factory
            .metadata
            .extra
            .get(FILE_NESTED_DEFINITIONS_NOT_ADMITTED_KEY),
        Some(&serde_json::Value::from(40u64))
    );
}

#[test]
fn an_empty_census_is_a_measured_zero() {
    let mut entities = entities();
    attach_file_nested_definition_record(&mut entities, Some(&NestedDefinitionCensus::default()));
    for entity in &entities {
        assert_eq!(
            entity
                .metadata
                .extra
                .get(FILE_NESTED_DEFINITIONS_NOT_ADMITTED_KEY),
            Some(&serde_json::Value::from(0u64))
        );
        assert!(!entity
            .metadata
            .extra
            .contains_key(NESTED_DEFINITIONS_NOT_ADMITTED_KEY));
    }
}

#[test]
fn no_census_leaves_both_keys_absent_even_over_a_stale_record() {
    let mut entities = entities();
    let mut census = NestedDefinitionCensus::default();
    census.record(unadmitted("factory.<locals>.helper", "factory", 1));
    attach_file_nested_definition_record(&mut entities, Some(&census));
    attach_file_nested_definition_record(&mut entities, None);
    for entity in &entities {
        assert!(!entity
            .metadata
            .extra
            .contains_key(FILE_NESTED_DEFINITIONS_NOT_ADMITTED_KEY));
        assert!(!entity
            .metadata
            .extra
            .contains_key(NESTED_DEFINITIONS_NOT_ADMITTED_KEY));
    }
}
