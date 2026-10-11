// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Regression net for Python call-resolution.
//!
//! Python is the only call-extracting adapter that previously had no dedicated
//! call-resolution test, even though every other one (Go, JS/TS, Kotlin, Swift,
//! C++ qualified-name) does. This file closes that gap.
//!
//! Python narrows attribute callees to their **leaf name at extraction time**,
//! with one deliberate exception. `module.func()`, `obj.method()`, and
//! `self.member.save()` all emit a `Calls` edge whose `dst_name` is the trailing
//! identifier (`func`, `method`, `save`). Their receiver's type is unknowable at
//! parse time, so the file also carries negative call-coverage evidence; the
//! leaf edge is useful for recall but cannot certify a safe rename. But direct
//! `self.m()` / `cls.m()` dispatch through the *enclosing
//! class*, so they emit the class-qualified form (`Service.validate`): that
//! qualifier is what lets the linker resolve an inherited method through the
//! class's Extends chain instead of fanning out on the bare name. Because the
//! parser hands the linker either a simple name or a `Class.method` key that
//! matches how method entities are named, name-based cross-file resolution
//! stays aligned with the entity keyspace. These tests are the regression net
//! for both halves of that contract.
//!
//! Same-file extraction (entities + `Calls` `dst_name`s) is asserted here. The
//! cross-file dimension of the matrix — the same three call shapes resolving to
//! an entity in another file through the real linker — lives in
//! `kin-index/tests/python_call_resolution.rs`, since cross-file linking is a
//! kin-index responsibility.
//!
//! Matrix (same-file arm):
//!   * bare call (`target()`), with and without an import binding
//!   * module-attribute call (`module.func()`) — narrowed to `func`
//!   * method call on an instance (`obj.method()`) — narrowed to `method`
//!   * nesting: a `def` nested in a function or method is an entity of its
//!     own, named `outer.<locals>.inner`, and owns the calls in its body.

use kin_model::{EntityKind, FilePathId, RelationKind};
use kin_parser::{
    is_call_extraction_incomplete_marker, is_scoped_call_extraction_incomplete_marker,
    ExtractedRelation, LanguageAdapter, ParseOutput, PythonAdapter,
};

fn extract(source: &str) -> ParseOutput {
    extract_at("test/calls.py", source)
}

fn extract_at(path: &str, source: &str) -> ParseOutput {
    let adapter = PythonAdapter;
    let bytes = source.as_bytes();
    let tree = adapter.parse(bytes).expect("parse should succeed");
    let file_id = FilePathId::new(path);
    adapter
        .extract(&tree, bytes, &file_id)
        .expect("extract should succeed")
}

fn calls(output: &ParseOutput) -> Vec<&ExtractedRelation> {
    output
        .relations
        .iter()
        .filter(|r| r.kind == RelationKind::Calls)
        .collect()
}

fn calls_named<'a>(output: &'a ParseOutput, name: &str) -> Vec<&'a ExtractedRelation> {
    calls(output)
        .into_iter()
        .filter(|r| r.dst_name == name)
        .collect()
}

fn has_entity(output: &ParseOutput, kind: EntityKind, name: &str) -> bool {
    output
        .entities
        .iter()
        .any(|e| e.kind == kind && e.name == name)
}

const CALLS_FIXTURE: &[u8] = include_bytes!("../../../tests/adapter-fixtures/python/calls.py");

// ---- bare calls ----

#[test]
fn bare_call_emits_identifier_name() {
    let output = extract("def target():\n    pass\n\ndef caller():\n    target()\n");
    let hits = calls_named(&output, "target");
    assert_eq!(
        hits.len(),
        1,
        "expected exactly one Calls edge named 'target', got {:?}",
        calls(&output)
    );
    assert_eq!(hits[0].src_name, "caller");
    assert!(
        hits[0].import_source.is_none(),
        "a locally-defined callee carries no import source, got {:?}",
        hits[0].import_source
    );
    assert!(has_entity(&output, EntityKind::Function, "caller"));
    assert!(has_entity(&output, EntityKind::Function, "target"));
}

#[test]
fn bare_call_with_import_carries_import_source() {
    // `from helpers import compute` binds the simple name `compute`, so the
    // extractor annotates the `compute()` edge with its module of origin.
    let output = extract("from helpers import compute\n\ndef caller():\n    compute()\n");
    let hits = calls_named(&output, "compute");
    assert_eq!(
        hits.len(),
        1,
        "expected exactly one Calls edge named 'compute', got {:?}",
        calls(&output)
    );
    assert_eq!(hits[0].src_name, "caller");
    assert_eq!(
        hits[0].import_source.as_deref(),
        Some("helpers"),
        "an imported bare callee carries its import path as import_source"
    );
}

#[test]
fn parenthesized_named_call_preserves_exact_argument_shape() {
    let output = extract(
        "def target(ext, args):\n    return ext, args\n\ndef caller():\n    return ((target))(args=2, ext=1)\n",
    );
    let hits = calls_named(&output, "target");
    assert_eq!(hits.len(), 1, "wrapped named call must be extracted once");
    let shape = hits[0].call_shape.as_ref().expect("wrapped call shape");
    assert_eq!(shape.positional, 0);
    assert_eq!(shape.keywords, vec!["args".to_string(), "ext".to_string()]);
    assert!(!shape.has_var_positional);
    assert!(!shape.has_var_keyword);
    assert!(
        !output
            .relations
            .iter()
            .any(is_call_extraction_incomplete_marker),
        "transparent parentheses must not degrade file-level call coverage"
    );
}

#[test]
fn parenthesized_self_attribute_keeps_class_qualification() {
    let output = extract(
        "class Service:\n    def target(self, value):\n        return value\n\n    def caller(self):\n        return (self.target)(value=1)\n",
    );
    let hits = calls_named(&output, "Service.target");
    assert_eq!(hits.len(), 1, "wrapped self call must stay class-qualified");
    assert_eq!(
        hits[0]
            .call_shape
            .as_ref()
            .expect("wrapped self shape")
            .keywords,
        vec!["value".to_string()]
    );
    assert!(
        !output
            .relations
            .iter()
            .any(is_call_extraction_incomplete_marker),
        "a resolvable wrapped attribute must retain complete coverage"
    );
}

#[test]
fn dynamic_callee_emits_one_incomplete_extraction_marker() {
    let output = extract(
        "def target(ext, args):\n    return ext, args\n\ndef other(ext, args):\n    return ext, args\n\ndef caller(flag):\n    target(1, 2)\n    return (target if flag else other)(ext=1, args=2)\n",
    );
    assert_eq!(
        output
            .relations
            .iter()
            .filter(|relation| is_call_extraction_incomplete_marker(relation))
            .count(),
        1,
        "one file-level marker covers every unsupported callee"
    );
}

/// A module-scope call is the module's call, the way a module-scope read is
/// the module's reference. It used to reach no relation, which is why it had
/// to withhold a full coverage claim; now it is represented, so the file's
/// coverage stands on it like any other named call.
#[test]
fn module_scope_call_is_the_modules_call() {
    let output = extract("def target(ext, args):\n    return ext, args\n\ntarget(ext=1, args=2)\n");
    let hits = calls_named(&output, "target");
    assert_eq!(hits.len(), 1, "got {:?}", calls(&output));
    assert_eq!(hits[0].src_name, "calls", "the module entity is the caller");
    assert_eq!(hits[0].site.as_ref().map(|site| site.start_line), Some(3));
    assert!(
        !output
            .relations
            .iter()
            .any(is_call_extraction_incomplete_marker),
        "a module-scope call the graph carries leaves nothing unrepresented"
    );
}

// ---- module-attribute calls ----

#[test]
fn module_attribute_call_narrows_to_leaf_name() {
    // `mathlib.compute()` narrows to the leaf `compute`. The dotted form must
    // never survive. Its receiver, rather than the bare leaf, carries the
    // namespace import claim whose exact source witness the linker checks.
    let output = extract("import mathlib\n\ndef caller():\n    mathlib.compute()\n");
    let hits = calls_named(&output, "compute");
    assert_eq!(
        hits.len(),
        1,
        "expected leaf-name edge 'compute' (not 'mathlib.compute'), got {:?}",
        calls(&output)
    );
    assert_eq!(hits[0].src_name, "caller");
    assert_eq!(hits[0].receiver.as_deref(), Some("mathlib"));
    assert_eq!(hits[0].import_source.as_deref(), Some("mathlib"));
    assert!(
        !calls(&output).iter().any(|r| r.dst_name.contains('.')),
        "dotted form must not appear as a Calls dst_name, got {:?}",
        calls(&output)
    );
    assert!(
        output
            .relations
            .iter()
            .any(is_call_extraction_incomplete_marker),
        "leaf-only module receiver evidence must not certify complete call resolution"
    );
}

// ---- method calls on an instance ----

#[test]
fn method_call_on_instance_narrows_to_leaf_name() {
    let output = extract(
        "class Service:\n    def run(self):\n        pass\n\ndef driver():\n    svc = Service()\n    svc.run()\n",
    );
    let run_hits = calls_named(&output, "run");
    assert!(
        run_hits.iter().any(|r| r.src_name == "driver"),
        "expected driver -> run (from `svc.run()`), got {:?}",
        calls(&output)
    );
    // Constructor call `Service()` is a plain identifier callee.
    assert!(
        calls_named(&output, "Service")
            .iter()
            .any(|r| r.src_name == "driver"),
        "expected driver -> Service (constructor), got {:?}",
        calls(&output)
    );
    assert!(
        !calls(&output).iter().any(|r| r.dst_name.contains('.')),
        "no Calls dst_name may be dotted (e.g. 'svc.run'), got {:?}",
        calls(&output)
    );
    assert!(has_entity(&output, EntityKind::Method, "Service.run"));
    assert!(
        output
            .relations
            .iter()
            .any(is_call_extraction_incomplete_marker),
        "an untyped instance receiver must preserve negative call-coverage evidence"
    );
}

// ---- nesting recursion ----

#[test]
fn nested_def_owns_its_calls_and_the_outer_call_names_it() {
    // `inner` is a definition nested in `outer`'s body. It is an entity of its
    // own named the way CPython's `__qualname__` names it, the call written in
    // its body is its call, and `outer`'s call to it names that entity.
    let output = extract("def outer():\n    def inner():\n        leaf()\n    inner()\n");

    assert!(has_entity(&output, EntityKind::Function, "outer"));
    assert!(
        has_entity(&output, EntityKind::Function, "outer.<locals>.inner"),
        "a nested def is admitted under its qualified name, got {:?}",
        output.entities.iter().map(|e| &e.name).collect::<Vec<_>>()
    );
    assert!(
        !has_entity(&output, EntityKind::Function, "inner"),
        "a nested def must not take a module-level name"
    );

    let leaf = calls_named(&output, "leaf");
    assert!(
        !leaf.is_empty() && leaf.iter().all(|r| r.src_name == "outer.<locals>.inner"),
        "the call inside the nested def belongs to the nested def, got {:?}",
        calls(&output)
    );
    assert!(
        calls_named(&output, "outer.<locals>.inner")
            .iter()
            .any(|r| r.src_name == "outer"),
        "the call to the nested def names its entity, got {:?}",
        calls(&output)
    );
}

#[test]
fn class_method_nested_def_owns_its_calls() {
    // A method that wraps a nested def: the nested def is `Holder.method`'s
    // local, its body's call is its own, and `self` inside it still names
    // `Holder`, because a closure in a method sees the method's `self`.
    let output = extract(
        "class Holder:\n    def method(self):\n        def nested():\n            deep()\n            self.helper()\n        nested()\n",
    );

    assert!(has_entity(&output, EntityKind::Method, "Holder.method"));
    assert!(
        has_entity(
            &output,
            EntityKind::Function,
            "Holder.method.<locals>.nested"
        ),
        "a def nested in a method is `Class.method.<locals>.name`, got {:?}",
        output.entities.iter().map(|e| &e.name).collect::<Vec<_>>()
    );

    let deep = calls_named(&output, "deep");
    assert!(
        !deep.is_empty()
            && deep
                .iter()
                .all(|r| r.src_name == "Holder.method.<locals>.nested"),
        "the call inside the nested def belongs to it, got {:?}",
        calls(&output)
    );
    assert!(
        calls_named(&output, "Holder.helper")
            .iter()
            .any(|r| r.src_name == "Holder.method.<locals>.nested"),
        "`self.helper()` in a closure still names the method's class, got {:?}",
        calls(&output)
    );
    assert!(
        calls_named(&output, "Holder.method.<locals>.nested")
            .iter()
            .any(|r| r.src_name == "Holder.method"),
        "the method's call to its nested def names the nested entity, got {:?}",
        calls(&output)
    );
}

// ---- self/cls receivers: class-qualified dst_name ----

#[test]
fn self_call_emits_class_qualified_dst() {
    let output = extract(
        "class Command:\n    def handle(self):\n        self.validate()\n        cls_free()\n",
    );
    assert!(
        calls_named(&output, "Command.validate")
            .iter()
            .any(|r| r.src_name == "Command.handle"),
        "self.validate() inside Command must emit the class-qualified dst \
         'Command.validate' so the linker can walk the Extends chain, got {:?}",
        calls(&output)
    );
    assert!(
        calls_named(&output, "validate").is_empty(),
        "the bare form must not be emitted alongside the qualified one, got {:?}",
        calls(&output)
    );
}

// ---- fixture: dst_names are leaf identifiers, except self/cls dispatch ----

#[test]
fn fixture_calls_are_all_leaf_names() {
    let adapter = PythonAdapter;
    let tree = adapter.parse(CALLS_FIXTURE).expect("parse fixture");
    let file_id = FilePathId::new("adapter-fixtures/python/calls.py");
    let output = adapter
        .extract(&tree, CALLS_FIXTURE, &file_id)
        .expect("extract fixture");
    let call_edges = calls(&output);

    // Regression invariant: attribute/method callees are narrowed unless the
    // receiver is `self`/`cls`, whose dispatch class is known — those emit the
    // `Class.method` form. Any other dotted dst_name means the narrowing broke.
    let dotted: Vec<&str> = call_edges
        .iter()
        .map(|r| r.dst_name.as_str())
        .filter(|n| n.contains('.') && *n != "Service.validate")
        .collect();
    assert!(
        dotted.is_empty(),
        "Calls dst_names must be simple identifiers (or self/cls-qualified \
         Class.method), but got dotted: {:?}",
        dotted
    );

    // Every call shape in the matrix is represented.
    for expected in &[
        "handler",          // bare call in plain()
        "add_url_rule",     // attribute call on a local (app.add_url_rule)
        "query",            // chained attribute call (db.session.query())
        "save",             // attribute call on an instance member (self.store.save)
        "Service.validate", // self-call (self.validate) — class-qualified
        "Store",            // constructor call
        "route",            // decorator call @app.route(...)
        "requires_auth",    // bare decorator @requires_auth
    ] {
        assert!(
            call_edges.iter().any(|r| r.dst_name == *expected),
            "fixture should produce a Calls edge to '{}', got {:?}",
            expected,
            call_edges.iter().map(|r| &r.dst_name).collect::<Vec<_>>()
        );
    }

    // Spot-check source attribution for representative shapes.
    assert!(
        call_edges
            .iter()
            .any(|r| r.src_name == "plain" && r.dst_name == "handler"),
        "expected plain -> handler"
    );
    assert!(
        call_edges
            .iter()
            .any(|r| r.src_name == "Service.process" && r.dst_name == "save"),
        "expected Service.process -> save (from self.store.save)"
    );
    assert!(
        call_edges
            .iter()
            .any(|r| r.src_name == "home" && r.dst_name == "route"),
        "expected home -> route (from the @app.route decorator)"
    );
}

// ---- scoping: a bare name resolves the way Python binds it ----

/// The callers in `output` that record a resolution gap for a call whose
/// callee they could name, with that name.
fn named_gaps(output: &ParseOutput) -> Vec<(String, String)> {
    output
        .relations
        .iter()
        .filter(|relation| is_scoped_call_extraction_incomplete_marker(relation))
        .filter_map(|relation| Some((relation.src_name.clone(), relation.receiver.clone()?)))
        .collect()
}

fn callers_of<'a>(output: &'a ParseOutput, name: &str) -> Vec<&'a str> {
    calls_named(output, name)
        .into_iter()
        .map(|relation| relation.src_name.as_str())
        .collect()
}

const SCOPING_FIXTURE: &str = "\
def helper():
    return 1


def with_param(helper):
    return helper()


def with_local():
    helper = make()
    return helper()


def with_loop(fns):
    for helper in fns:
        helper()


def with_context(manager):
    with manager as helper:
        return helper()


def with_except(run):
    try:
        run()
    except Exception as helper:
        return helper()


def enclosing():
    helper = make()

    def inner():
        return helper()

    return inner()


def with_nonlocal():
    helper = make()

    def inner():
        nonlocal helper
        return helper()

    return inner()


def with_global():
    global helper
    return helper()


def plain():
    return helper()


def make():
    return len
";

/// A parameter, a local assignment, a loop, `with` or `except` target, and an
/// enclosing function's local all hide the module's `def helper` from a call
/// written in that scope. Each such call reached the module-level function at
/// parser certainty, so a function its callers only shadow read as called
/// from every one of them. The call is kept as a named gap instead.
#[test]
fn a_call_through_a_local_binding_names_no_module_level_declaration() {
    let output = extract(SCOPING_FIXTURE);
    let mut callers = callers_of(&output, "helper");
    callers.sort_unstable();
    assert_eq!(
        callers,
        vec!["plain", "with_global"],
        "only a call the module scope binds reaches the module's helper"
    );
    let gaps = named_gaps(&output);
    for caller in [
        "with_param",
        "with_local",
        "with_loop",
        "with_context",
        "with_except",
        "enclosing.<locals>.inner",
        "with_nonlocal.<locals>.inner",
    ] {
        assert!(
            gaps.contains(&(caller.to_string(), "helper".to_string())),
            "{caller} calls a local value and records it as a named gap, got {gaps:?}"
        );
    }
}

/// A plain `from module import name` in a function body binds the name there,
/// so the call carries that module rather than reading as a call to the
/// module-level declaration of the same name, and the file's own import of
/// the name from elsewhere does not overwrite it.
#[test]
fn a_function_local_import_carries_its_module() {
    let output = extract(
        "from .elsewhere import helper\n\ndef helper_user():\n    from .other import helper\n\n    return helper()\n\ndef top_level_user():\n    return helper()\n",
    );
    let local = calls_named(&output, "helper")
        .into_iter()
        .find(|relation| relation.src_name == "helper_user")
        .expect("the locally imported call is kept");
    assert_eq!(local.import_source.as_deref(), Some(".other"));
    let top = calls_named(&output, "helper")
        .into_iter()
        .find(|relation| relation.src_name == "top_level_user")
        .expect("the module-scope import still binds its call");
    assert_eq!(top.import_source.as_deref(), Some(".elsewhere"));
}

/// An aliased local import and `import module` bind a value this parse does
/// not follow, so their calls are gaps too.
#[test]
fn an_aliased_local_import_is_a_local_value() {
    let output = extract(
        "def helper():\n    return 1\n\ndef user():\n    from .other import tool as helper\n\n    return helper()\n",
    );
    assert!(
        callers_of(&output, "helper").is_empty(),
        "{:?}",
        calls(&output)
    );
    assert!(named_gaps(&output).contains(&("user".to_string(), "helper".to_string())));
}

/// A decorator is evaluated in the scope around its definition, so a bare
/// decorator an enclosing function receives as a parameter is that value, not
/// a module-level function sharing its name.
#[test]
fn a_decorator_bound_by_an_enclosing_parameter_is_a_gap() {
    let output = extract(
        "def deco(fn):\n    return fn\n\ndef outer(deco):\n    @deco\n    def inner():\n        return 1\n\n    return inner\n\n@deco\ndef top():\n    return 2\n",
    );
    assert_eq!(
        callers_of(&output, "deco"),
        vec!["top"],
        "{:?}",
        calls(&output)
    );
    assert!(
        named_gaps(&output).contains(&("outer.<locals>.inner".to_string(), "deco".to_string())),
        "{:?}",
        named_gaps(&output)
    );
}

/// `rich/repr.py` calls `repr(arg[0])`. The file's Module entity is named
/// `repr`, but a module binds no name for itself, so the name is the builtin:
/// the parser keeps the call bare for the linker's builtin gate, and a value
/// read of `repr` is no reference to anything the file declares.
#[test]
fn a_builtin_named_like_its_module_is_not_a_reference_to_the_module() {
    let output = extract_at(
        "rich/repr.py",
        "def auto(cls):\n    def auto_repr(self):\n        out = []\n        append = out.append\n        for arg in self.__rich_repr__():\n            append(repr(arg[0]))\n        return out\n    return auto_repr\n\ndef reprs(items):\n    return list(map(repr, items))\n",
    );
    assert!(
        output
            .entities
            .iter()
            .any(|entity| entity.kind == EntityKind::Module && entity.name == "repr"),
        "the file's module entity is named for the file"
    );
    let repr_calls = calls_named(&output, "repr");
    assert_eq!(repr_calls.len(), 1, "{:?}", calls(&output));
    assert_eq!(repr_calls[0].src_name, "auto.<locals>.auto_repr");
    assert_eq!(repr_calls[0].import_source, None);
    assert!(
        !output.relations.iter().any(
            |relation| relation.kind == RelationKind::References && relation.dst_name == "repr"
        ),
        "a read of the builtin is not a reference to the module: {:?}",
        output.relations
    );
    assert!(
        named_gaps(&output)
            .contains(&("auto.<locals>.auto_repr".to_string(), "append".to_string())),
        "the locally bound `append` is a named gap, got {:?}",
        named_gaps(&output)
    );
}

#[test]
fn a_module_constructor_call_is_already_owned_by_its_constant() {
    let output = extract("from fastapi import FastAPI\napp = FastAPI()\n");
    let hits = calls_named(&output, "FastAPI");
    assert_eq!(
        hits.len(),
        1,
        "the shared census must not lose or duplicate the RHS call"
    );
    assert_eq!(hits[0].src_name, "app");
    assert_eq!(hits[0].import_source.as_deref(), Some("fastapi"));
    assert!(has_entity(&output, EntityKind::Constant, "app"));
}
