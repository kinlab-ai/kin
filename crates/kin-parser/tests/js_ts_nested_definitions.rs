// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! JavaScript and TypeScript definitions nested in function bodies.
//!
//! React's no-op renderer builds its host config as an object literal inside
//! the `createReactNoop` factory, and a Flow file like that one is read by the
//! TypeScript walk, so each shape here runs through the JavaScript adapter,
//! the Flow path, and the TypeScript adapter.

use kin_model::{EntityKind, FilePathId, RelationKind};
use kin_parser::{JavaScriptAdapter, LanguageAdapter, ParseOutput, TypeScriptAdapter};

fn extract(adapter: &dyn LanguageAdapter, path: &str, source: &str) -> ParseOutput {
    let tree = adapter.parse(source.as_bytes()).expect("parse");
    adapter
        .extract(&tree, source.as_bytes(), &FilePathId::new(path))
        .expect("extract")
}

fn js(source: &str) -> ParseOutput {
    extract(&JavaScriptAdapter, "src/createReactNoop.js", source)
}

fn ts(source: &str) -> ParseOutput {
    extract(&TypeScriptAdapter, "src/createReactNoop.ts", source)
}

fn kind_of(output: &ParseOutput, name: &str) -> Option<EntityKind> {
    output
        .entities
        .iter()
        .find(|entity| entity.name == name)
        .map(|entity| entity.kind)
}

fn has_relation(output: &ParseOutput, kind: RelationKind, src: &str, dst: &str) -> bool {
    output
        .relations
        .iter()
        .any(|r| r.kind == kind && r.src_name == src && r.dst_name == dst)
}

fn callers_of<'a>(output: &'a ParseOutput, dst: &str) -> Vec<&'a str> {
    output
        .relations
        .iter()
        .filter(|r| r.kind == RelationKind::Calls && r.dst_name == dst)
        .map(|r| r.src_name.as_str())
        .collect()
}

const FACTORY: &str = "createReactNoop";
const CONFIG: &str = "createReactNoop.<locals>.mutationHostConfig";
const METHOD: &str = "createReactNoop.<locals>.mutationHostConfig.commitTextUpdate";

const PLAIN_JS: &str = "function createReactNoop(reconciler) {\n\
  const mutationHostConfig = {\n\
    commitTextUpdate(textInstance, oldText, newText) {\n\
      computeText(newText);\n\
    },\n\
  };\n\
  return reconciler(mutationHostConfig);\n\
}\n\
export default createReactNoop;\n";

const FLOW: &str = "/**\n * @flow\n */\n\
import type {HostConfig} from './HostConfig';\n\
function createReactNoop(reconciler: Function) {\n\
  const mutationHostConfig: Pick<HostConfig, 'commitTextUpdate'> = {\n\
    commitTextUpdate(textInstance: TextInstance, oldText: string, newText: string): void {\n\
      computeText(newText);\n\
    },\n\
  };\n\
  return reconciler(mutationHostConfig);\n\
}\n\
export default createReactNoop;\n";

fn assert_host_config_shape(output: &ParseOutput, dialect: &str) {
    assert_eq!(
        kind_of(output, METHOD),
        Some(EntityKind::Method),
        "{dialect}: {:?}",
        output.entities.iter().map(|e| &e.name).collect::<Vec<_>>()
    );
    assert_eq!(
        kind_of(output, CONFIG),
        Some(EntityKind::Constant),
        "{dialect}"
    );
    assert!(
        has_relation(output, RelationKind::Contains, FACTORY, CONFIG),
        "{dialect}: the factory contains its local config"
    );
    assert!(
        has_relation(output, RelationKind::Contains, CONFIG, METHOD),
        "{dialect}: the config contains its method"
    );
    assert_eq!(
        callers_of(output, "computeText"),
        vec![METHOD],
        "{dialect}: the call in the method body is the method's, not the factory's"
    );
    assert_eq!(
        callers_of(output, "reconciler"),
        vec![FACTORY],
        "{dialect}: the factory keeps its own calls"
    );
    assert!(
        kind_of(output, "mutationHostConfig").is_none()
            && kind_of(output, "mutationHostConfig.commitTextUpdate").is_none(),
        "{dialect}: a local object never takes a module-level name"
    );
}

#[test]
fn a_local_host_config_object_in_javascript_owns_its_methods() {
    assert_host_config_shape(&js(PLAIN_JS), "javascript");
}

#[test]
fn a_flow_annotated_host_config_reaches_the_typescript_walk_and_is_admitted() {
    let tree = JavaScriptAdapter.parse(FLOW.as_bytes()).expect("parse");
    assert!(
        kin_parser::languages::javascript::tree_is_typescript_family(&tree),
        "a Flow file is read by the TypeScript grammar, so this exercises the TypeScript walk"
    );
    let output = js(FLOW);
    assert_host_config_shape(&output, "flow");
}

#[test]
fn a_local_host_config_object_in_typescript_owns_its_methods() {
    let source = FLOW.replace("/**\n * @flow\n */\n", "");
    assert_host_config_shape(&ts(&source), "typescript");
}

#[test]
fn a_class_nested_in_a_named_function_is_admitted_with_its_methods() {
    for (dialect, output) in [
        (
            "javascript",
            js("function defineTests() {\n  it('renders', () => {\n    class Child extends React.Component {\n      render() { helper(); }\n    }\n  });\n}\n"),
        ),
        (
            "typescript",
            ts("function defineTests() {\n  it('renders', () => {\n    class Child extends React.Component<Props> {\n      render(): void { helper(); }\n    }\n  });\n}\n"),
        ),
    ] {
        let class = "defineTests.<locals>.Child";
        let render = "defineTests.<locals>.Child.render";
        assert_eq!(kind_of(&output, class), Some(EntityKind::Class), "{dialect}");
        assert_eq!(kind_of(&output, render), Some(EntityKind::Method), "{dialect}");
        assert!(has_relation(&output, RelationKind::Contains, "defineTests", class), "{dialect}");
        assert!(has_relation(&output, RelationKind::Contains, class, render), "{dialect}");
        assert!(has_relation(&output, RelationKind::Extends, class, "Component"), "{dialect}");
        assert_eq!(callers_of(&output, "helper"), vec![render], "{dialect}");
    }
}

#[test]
fn a_class_in_a_callback_no_entity_owns_is_recorded_with_its_bases() {
    let output = js(
        "describe('ReactComponent', () => {\n  it('renders', () => {\n    class Child extends React.Component {\n      render() { return null; }\n    }\n  });\n});\n",
    );
    assert!(
        output.entities.iter().all(|e| !e.name.contains("Child")),
        "no entity owns the callback, so there is no name to admit it under"
    );
    let census = output
        .nested_definitions
        .expect("javascript takes a census");
    assert_eq!(census.not_admitted.len(), 1, "{:?}", census.not_admitted);
    let child = &census.not_admitted[0];
    assert_eq!(child.name, "Child");
    assert_eq!(child.kind, EntityKind::Class);
    assert_eq!(child.start_line, 2);
    assert_eq!(child.bases, vec!["Component".to_string()]);
    assert_eq!(child.enclosing.as_deref(), Some("createReactNoop"));
}

#[test]
fn nested_plain_functions_are_recorded_and_keep_their_calls_on_the_holder() {
    for output in [
        js("function F() {\n  function helper() { a(); }\n  const cb = () => b();\n  function* gen() {}\n  helper();\n}\n"),
        ts("function F() {\n  function helper(): void { a(); }\n  const cb = (): void => b();\n  function* gen() {}\n  helper();\n}\n"),
    ] {
        let census = output.nested_definitions.as_ref().expect("census");
        let recorded: Vec<(&str, EntityKind, u32, Option<&str>)> = census
            .not_admitted
            .iter()
            .map(|d| (d.name.as_str(), d.kind, d.start_line, d.enclosing.as_deref()))
            .collect();
        assert_eq!(
            recorded,
            vec![
                ("F.<locals>.helper", EntityKind::Function, 1, Some("F")),
                ("F.<locals>.cb", EntityKind::Function, 2, Some("F")),
                ("F.<locals>.gen", EntityKind::Function, 3, Some("F")),
            ]
        );
        assert!(output.entities.iter().all(|e| !e.name.contains("<locals>")));
        assert_eq!(callers_of(&output, "a"), vec!["F"]);
        assert_eq!(callers_of(&output, "b"), vec!["F"]);
    }
}

#[test]
fn an_object_inside_a_nested_function_is_named_through_that_function() {
    let output = js(
        "function F() {\n  function g() {\n    const X = { m() { bar(); } };\n    return X;\n  }\n  return g;\n}\n",
    );
    let object = "F.<locals>.g.<locals>.X";
    let method = "F.<locals>.g.<locals>.X.m";
    assert_eq!(kind_of(&output, method), Some(EntityKind::Method));
    assert!(
        has_relation(&output, RelationKind::Contains, "F", object),
        "the innermost admitted entity holds it: {:?}",
        output.relations
    );
    assert_eq!(callers_of(&output, "bar"), vec![method]);
}

#[test]
fn a_call_moves_to_the_innermost_admitted_member_only() {
    let output = js(
        "function F() {\n  const A = {\n    m() {\n      const B = { n() { baz(); } };\n      this.other();\n      return B;\n    },\n    other() {},\n  };\n  return A;\n}\n",
    );
    let n = "F.<locals>.A.m.<locals>.B.n";
    assert_eq!(kind_of(&output, n), Some(EntityKind::Method));
    assert!(has_relation(
        &output,
        RelationKind::Contains,
        "F.<locals>.A.m",
        "F.<locals>.A.m.<locals>.B"
    ));
    assert_eq!(callers_of(&output, "baz"), vec![n]);
    assert_eq!(
        callers_of(&output, "F.<locals>.A.other"),
        vec!["F.<locals>.A.m"],
        "`this.other()` in a local object's method names its sibling: {:?}",
        output.relations
    );
}

#[test]
fn a_class_nested_in_a_method_is_named_under_the_method() {
    let output = ts(
        "class Svc {\n  run(): void {\n    class Local extends Base {\n      go(): void { z(); }\n    }\n    new Local().go();\n  }\n}\n",
    );
    assert_eq!(
        kind_of(&output, "Svc.run.<locals>.Local.go"),
        Some(EntityKind::Method)
    );
    assert!(has_relation(
        &output,
        RelationKind::Contains,
        "Svc.run",
        "Svc.run.<locals>.Local"
    ));
    assert_eq!(callers_of(&output, "z"), vec!["Svc.run.<locals>.Local.go"]);
    assert_eq!(
        callers_of(&output, "go"),
        vec!["Svc.run"],
        "the method keeps the call it writes itself"
    );
}

#[test]
fn a_module_level_object_keeps_its_existing_names() {
    let output = js("const utils = {\n  parse() { inner(); },\n};\n");
    assert_eq!(kind_of(&output, "utils.parse"), Some(EntityKind::Method));
    assert_eq!(callers_of(&output, "inner"), vec!["utils.parse"]);
    assert!(output
        .nested_definitions
        .expect("census")
        .not_admitted
        .is_empty());
}

#[test]
fn two_helpers_declaring_the_same_local_object_get_two_entities() {
    let output = js(
        "function a() { const cfg = { run() {} }; return cfg; }\nfunction b() { const cfg = { run() {} }; return cfg; }\n",
    );
    assert_eq!(
        kind_of(&output, "a.<locals>.cfg.run"),
        Some(EntityKind::Method)
    );
    assert_eq!(
        kind_of(&output, "b.<locals>.cfg.run"),
        Some(EntityKind::Method)
    );
}
