// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Lexical declaration coverage through Python structural blocks.

use kin_model::{EntityKind, FilePathId, RelationKind};
use kin_parser::{
    is_scoped_call_extraction_incomplete_marker, LanguageAdapter, ParseOutput, PythonAdapter,
    RelationSyntacticRole,
};

fn extract(source: &str) -> ParseOutput {
    let adapter = PythonAdapter;
    let tree = adapter.parse(source.as_bytes()).expect("parse");
    adapter
        .extract(
            &tree,
            source.as_bytes(),
            &FilePathId::new("rich/logging.py"),
        )
        .expect("extract")
}

fn has(output: &ParseOutput, kind: RelationKind, src: &str, dst: &str) -> bool {
    output.relations.iter().any(|relation| {
        relation.kind == kind && relation.src_name == src && relation.dst_name == dst
    })
}

fn names(output: &ParseOutput) -> Vec<&str> {
    output
        .entities
        .iter()
        .map(|entity| entity.name.as_str())
        .collect()
}

#[test]
fn rich_logging_main_block_divide_is_a_module_member_and_owns_its_calls() {
    let source = "\
import logging
if __name__ == '__main__':
    log = logging.getLogger('rich')
    def divide() -> None:
        number = 1
        divisor = 0
        log.debug('in divide')
        try:
            number / divisor
        except:
            log.exception('divide failed')
    divide()
";
    let output = extract(source);
    assert!(names(&output).contains(&"divide"));
    assert!(has(&output, RelationKind::Contains, "logging", "divide"));
    for callee in ["debug", "exception"] {
        let calls: Vec<_> = output
            .relations
            .iter()
            .filter(|relation| relation.kind == RelationKind::Calls && relation.dst_name == callee)
            .collect();
        assert_eq!(calls.len(), 1, "{callee}: {calls:?}");
        assert_eq!(calls[0].src_name, "divide");
    }
    assert!(has(&output, RelationKind::Calls, "logging", "divide"));
    assert!(output.nested_definitions.unwrap().not_admitted.is_empty());
}

#[test]
fn module_try_except_else_finally_definitions_keep_module_ownership() {
    let output = extract(
        "\
try:
    import optional
    def fast(): return optional.run()
except ImportError:
    def slow(): return 1
else:
    def ready(): return fast()
finally:
    def cleanup(): return slow()
",
    );
    for name in ["fast", "slow", "ready", "cleanup"] {
        assert!(
            names(&output).contains(&name),
            "{name}: {:?}",
            names(&output)
        );
        assert!(has(&output, RelationKind::Contains, "logging", name));
    }
    assert!(
        output.imports.is_empty(),
        "guarded imports are not runtime imports"
    );
    assert!(has(&output, RelationKind::Calls, "ready", "fast"));
    assert!(has(&output, RelationKind::Calls, "cleanup", "slow"));
}

#[test]
fn class_block_method_body_is_not_read_again_as_class_code() {
    let output = extract(
        "\
def helper(): pass
class Box:
    if enabled:
        def wide(self):
            helper()
            return helper
",
    );
    let method = output
        .entities
        .iter()
        .find(|entity| entity.name == "Box.wide")
        .unwrap();
    assert_eq!(method.kind, EntityKind::Method);
    assert!(has(&output, RelationKind::Contains, "Box", "Box.wide"));
    assert!(has(&output, RelationKind::Calls, "Box.wide", "helper"));
    assert!(has(&output, RelationKind::References, "Box.wide", "helper"));
    assert!(!has(&output, RelationKind::Calls, "Box", "helper"));
    assert!(!has(&output, RelationKind::References, "Box", "helper"));
}

#[test]
fn loops_with_and_match_do_not_create_declaration_scopes() {
    let output = extract(
        "\
for item in items:
    def in_for(): pass
while waiting:
    def in_while(): pass
with manager():
    def in_with(): pass
match state:
    case 'ready':
        def in_match(): pass
",
    );
    for name in ["in_for", "in_while", "in_with", "in_match"] {
        assert!(
            has(&output, RelationKind::Contains, "logging", name),
            "{name}"
        );
    }
    assert!(output.nested_definitions.unwrap().not_admitted.is_empty());
}

#[test]
fn nested_names_and_containment_survive_structural_blocks() {
    let output = extract(
        "\
if enabled:
    def outer():
        try:
            class Local:
                if ready:
                    def run(self):
                        def inner(): return 1
                        return inner()
        finally:
            pass
        return Local
",
    );
    for (parent, child) in [
        ("logging", "outer"),
        ("outer", "outer.<locals>.Local"),
        ("outer.<locals>.Local", "outer.<locals>.Local.run"),
        (
            "outer.<locals>.Local.run",
            "outer.<locals>.Local.run.<locals>.inner",
        ),
    ] {
        assert!(
            has(&output, RelationKind::Contains, parent, child),
            "{parent}→{child}"
        );
    }
    assert!(has(
        &output,
        RelationKind::Calls,
        "outer.<locals>.Local.run",
        "outer.<locals>.Local.run.<locals>.inner"
    ));
}

#[test]
fn both_conditional_alternatives_are_censused_without_a_unique_target() {
    for source in [
        "if flag:\n    def pick(): a()\nelse:\n    def pick(): b()\npick()\n",
        "def pick(): a()\nif flag:\n    def pick(): b()\npick()\n",
    ] {
        let output = extract(source);
        assert!(!names(&output).contains(&"pick"));
        let census = output.nested_definitions.as_ref().unwrap();
        assert_eq!(
            census
                .not_admitted
                .iter()
                .filter(|definition| definition.name == "pick")
                .count(),
            2
        );
        assert!(
            !output.relations.iter().any(|relation| {
                relation.kind == RelationKind::Calls
                    && ["pick", "a", "b"].contains(&relation.dst_name.as_str())
            }),
            "withheld bodies cannot invent module callers: {:?}",
            output.relations
        );
        assert!(output.relations.iter().any(|relation| {
            is_scoped_call_extraction_incomplete_marker(relation)
                && relation.receiver.as_deref() == Some("pick")
        }));
    }
}

#[test]
fn class_and_function_conditional_collisions_do_not_choose_a_first_definition() {
    let output = extract(
        "\
class Box:
    if flag:
        def pick(self): return 1
    else:
        def pick(self): return 2
    def run(self): return self.pick()
def outer():
    if flag:
        def local(): return 1
    else:
        def local(): return 2
    return local()
",
    );
    for name in ["Box.pick", "outer.<locals>.local"] {
        assert!(!names(&output).contains(&name));
        assert_eq!(
            output
                .nested_definitions
                .as_ref()
                .unwrap()
                .not_admitted
                .iter()
                .filter(|definition| definition.name == name)
                .count(),
            2
        );
        assert!(!output
            .relations
            .iter()
            .any(|relation| relation.kind == RelationKind::Calls && relation.dst_name == name));
        assert!(output.relations.iter().any(
            |relation| is_scoped_call_extraction_incomplete_marker(relation)
                && relation.receiver.as_deref() == Some(name)
        ));
    }
}

#[test]
fn imported_binding_collision_is_unknown_but_independent_local_import_stays_bound() {
    let output = extract(
        "\
from helpers import pick
if flag:
    def pick(): return 1
def run(): return pick()
def independent():
    from other import pick
    return pick()
",
    );
    assert!(!names(&output).contains(&"pick"));
    assert!(!output
        .relations
        .iter()
        .any(|relation| relation.kind == RelationKind::Calls && relation.src_name == "run"));
    let independent = output
        .relations
        .iter()
        .find(|relation| relation.kind == RelationKind::Calls && relation.src_name == "independent")
        .unwrap();
    assert_eq!(independent.dst_name, "pick");
    assert_eq!(independent.import_source.as_deref(), Some("other"));
}

#[test]
fn aliased_guarded_import_collision_cannot_become_a_runtime_import_proof() {
    let output = extract(
        "\
try:
    from fast import run as speed
except ImportError:
    def speed(): return 1
speed()
",
    );
    assert!(output.imports.is_empty());
    assert!(!names(&output).contains(&"speed"));
    assert!(!output
        .relations
        .iter()
        .any(|relation| relation.kind == RelationKind::Calls && relation.dst_name == "speed"));
    assert_eq!(
        output.nested_definitions.unwrap().not_admitted[0].name,
        "speed"
    );
}

#[test]
fn ambiguous_classes_and_dependent_bases_retain_the_written_census() {
    let output = extract(
        "\
class Derived(Twin): pass
if flag:
    class Twin(Left): pass
else:
    class Twin(Right): pass
class Further(Derived): pass
",
    );
    for name in ["Twin", "Derived", "Further"] {
        assert!(!names(&output).contains(&name), "{name}");
    }
    let census = output.nested_definitions.unwrap();
    assert_eq!(census.not_admitted.len(), 4);
    assert!(census
        .not_admitted
        .iter()
        .any(|definition| definition.name == "Twin" && definition.bases == ["Left"]));
    assert!(census
        .not_admitted
        .iter()
        .any(|definition| definition.name == "Twin" && definition.bases == ["Right"]));
    assert!(census
        .not_admitted
        .iter()
        .any(|definition| definition.name == "Derived" && definition.bases == ["Twin"]));
    assert!(!output
        .relations
        .iter()
        .any(|relation| relation.kind == RelationKind::Extends));
}

#[test]
fn module_and_decorated_members_carry_exact_typed_child_spans() {
    let output = extract(
        "MAX_RETRIES = 3\n@decorate\ndef wrapped(): pass\nclass Box:\n    class Inner: pass\n",
    );
    for relation in output
        .relations
        .iter()
        .filter(|relation| relation.kind == RelationKind::Contains)
    {
        let site = relation.site.as_ref().expect("child site");
        let child = output
            .entities
            .iter()
            .find(|entity| entity.name == relation.dst_name)
            .unwrap();
        assert_eq!(
            (site.start_byte, site.end_byte),
            (child.span.start_byte, child.span.end_byte)
        );
        assert_eq!(
            site.syntactic_role,
            Some(if relation.src_name == "logging" {
                RelationSyntacticRole::ModuleContains
            } else {
                RelationSyntacticRole::LexicalContains
            })
        );
    }
    assert!(has(
        &output,
        RelationKind::Contains,
        "logging",
        "MAX_RETRIES"
    ));
    assert!(has(&output, RelationKind::Contains, "Box", "Inner"));
}

#[test]
fn a_new_block_class_cannot_take_another_lexical_owners_legacy_name() {
    let output = extract(
        "\
class Inner:
    def run(self): return 1
class Outer:
    if enabled:
        class Inner:
            def run(self): return 2
    make = Inner()
def independent(): return Inner()
",
    );
    assert_eq!(
        output
            .entities
            .iter()
            .filter(|entity| entity.name == "Inner")
            .count(),
        1
    );
    assert_eq!(
        output
            .entities
            .iter()
            .filter(|entity| entity.name == "Inner.run")
            .count(),
        1
    );
    assert!(output
        .nested_definitions
        .as_ref()
        .unwrap()
        .not_admitted
        .iter()
        .any(|definition| definition.name == "Inner"
            && definition.enclosing.as_deref() == Some("Outer")));
    assert!(!has(&output, RelationKind::Calls, "Outer", "Inner"));
    assert!(has(&output, RelationKind::Calls, "independent", "Inner"));
}

#[test]
fn a_conditional_definition_colliding_with_an_assignment_or_parameter_is_unknown() {
    for source in [
        "pick = factory()\nif flag:\n    def pick(): return 1\npick()\n",
        "def outer(pick):\n    if flag:\n        def pick(): return 1\n    return pick()\n",
    ] {
        let output = extract(source);
        assert!(!output
            .entities
            .iter()
            .any(|entity| entity.kind == EntityKind::Function
                && (entity.name == "pick" || entity.name == "outer.<locals>.pick")));
        assert!(!output
            .relations
            .iter()
            .any(|relation| relation.kind == RelationKind::Calls
                && (relation.dst_name == "pick" || relation.dst_name == "outer.<locals>.pick")));
        assert_eq!(output.nested_definitions.unwrap().not_admitted.len(), 1);
    }
}

#[test]
fn a_conditional_global_definition_cannot_certify_the_original_module_binding() {
    let output = extract(
        "\
def pick(): return 1
def mutate():
    global pick
    if flag:
        def pick(): return 2
    return pick()
def caller(): return pick()
def independent():
    from other import pick
    return pick()
",
    );
    assert!(names(&output).contains(&"pick"));
    assert!(!names(&output).contains(&"mutate.<locals>.pick"));
    for caller in ["mutate", "caller"] {
        assert!(!has(&output, RelationKind::Calls, caller, "pick"));
        assert!(output
            .relations
            .iter()
            .any(|relation| relation.src_name == caller
                && is_scoped_call_extraction_incomplete_marker(relation)
                && relation.receiver.as_deref() == Some("pick")));
    }
    assert!(has(&output, RelationKind::Calls, "independent", "pick"));
}

#[test]
fn a_conditional_nonlocal_definition_cannot_certify_either_lexical_binding() {
    let output = extract(
        "\
def outer():
    def pick(): return 1
    def mutate():
        nonlocal pick
        if flag:
            def pick(): return 2
        return pick()
    return pick()
",
    );
    assert!(names(&output).contains(&"outer.<locals>.pick"));
    assert!(!names(&output).contains(&"outer.<locals>.mutate.<locals>.pick"));
    assert!(!output.relations.iter().any(
        |relation| relation.kind == RelationKind::Calls && relation.dst_name.ends_with(".pick")
    ));
    let census = output.nested_definitions.unwrap();
    assert_eq!(
        census.not_admitted[0].name,
        "outer.<locals>.mutate.<locals>.pick"
    );
}
