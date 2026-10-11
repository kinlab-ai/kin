// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Python definitions nested in function bodies.
//!
//! A class a test declares to override a framework hook, and a view function a
//! test registers with a decorator, are declarations like any other: code
//! overrides through them and calls them. These fixtures are the two shapes
//! from flask's own test suite that an override question about
//! `SessionInterface.open_session` has to find, plus the naming, ownership and
//! binding rules that keep them from colliding with anything else in a file.

use kin_model::{EntityKind, FilePathId, RelationKind};
use kin_parser::{
    is_scoped_call_extraction_incomplete_marker, ExtractedRelation, LanguageAdapter, ParseOutput,
    PythonAdapter,
};

fn extract(source: &str) -> ParseOutput {
    let adapter = PythonAdapter;
    let tree = adapter.parse(source.as_bytes()).expect("parse");
    adapter
        .extract(
            &tree,
            source.as_bytes(),
            &FilePathId::new("tests/test_nested.py"),
        )
        .expect("extract")
}

fn kind_of(output: &ParseOutput, name: &str) -> Option<EntityKind> {
    output
        .entities
        .iter()
        .find(|entity| entity.name == name)
        .map(|entity| entity.kind)
}

fn names(output: &ParseOutput) -> Vec<&str> {
    output.entities.iter().map(|e| e.name.as_str()).collect()
}

fn has_relation(output: &ParseOutput, kind: RelationKind, src: &str, dst: &str) -> bool {
    output
        .relations
        .iter()
        .any(|r| r.kind == kind && r.src_name == src && r.dst_name == dst)
}

fn calls_to<'a>(output: &'a ParseOutput, dst: &str) -> Vec<&'a ExtractedRelation> {
    output
        .relations
        .iter()
        .filter(|r| r.kind == RelationKind::Calls && r.dst_name == dst)
        .collect()
}

/// `tests/test_reqctx.py::test_session_error_pops_context`, verbatim.
const REQCTX: &str = r#"import flask
from flask.sessions import SessionInterface


def test_session_error_pops_context():
    class SessionError(Exception):
        pass

    class FailingSessionInterface(SessionInterface):
        def open_session(self, app, request):
            raise SessionError()

    class CustomFlask(flask.Flask):
        session_interface = FailingSessionInterface()

    app = CustomFlask(__name__)

    @app.route("/")
    def index():
        # shouldn't get here
        AssertionError()

    response = app.test_client().get("/")
    assert response.status_code == 500
    assert not flask.request
    assert not flask.current_app
"#;

/// `tests/test_session_interface.py`, verbatim.
const SESSION_INTERFACE: &str = r#"import flask
from flask.globals import app_ctx
from flask.sessions import SessionInterface


def test_open_session_with_endpoint():
    """If request.endpoint (or other URL matching behavior) is needed
    while loading the session, RequestContext.match_request() can be
    called manually.
    """

    class MySessionInterface(SessionInterface):
        def save_session(self, app, session, response):
            pass

        def open_session(self, app, request):
            app_ctx.match_request()
            assert request.endpoint is not None

    app = flask.Flask(__name__)
    app.session_interface = MySessionInterface()

    @app.get("/")
    def index():
        return "Hello, World!"

    response = app.test_client().get("/")
    assert response.status_code == 200
"#;

const TEST: &str = "test_session_error_pops_context";
const FAILING: &str = "test_session_error_pops_context.<locals>.FailingSessionInterface";
const FAILING_OPEN: &str =
    "test_session_error_pops_context.<locals>.FailingSessionInterface.open_session";

#[test]
fn a_class_nested_in_a_test_is_an_entity_with_its_methods() {
    let output = extract(REQCTX);
    assert_eq!(kind_of(&output, TEST), Some(EntityKind::Function));
    assert_eq!(kind_of(&output, FAILING), Some(EntityKind::Class));
    assert_eq!(kind_of(&output, FAILING_OPEN), Some(EntityKind::Method));
    assert_eq!(
        kind_of(
            &output,
            "test_session_error_pops_context.<locals>.SessionError"
        ),
        Some(EntityKind::Class)
    );
    assert_eq!(
        kind_of(
            &output,
            "test_session_error_pops_context.<locals>.CustomFlask"
        ),
        Some(EntityKind::Class)
    );
    assert_eq!(
        kind_of(&output, "test_session_error_pops_context.<locals>.index"),
        Some(EntityKind::Function)
    );
    for bare in [
        "FailingSessionInterface",
        "open_session",
        "index",
        "CustomFlask",
    ] {
        assert!(
            kind_of(&output, bare).is_none(),
            "a nested definition never takes a module-level name: {bare} in {:?}",
            names(&output)
        );
    }
}

#[test]
fn nested_definitions_are_contained_by_what_encloses_them() {
    let output = extract(REQCTX);
    assert!(has_relation(&output, RelationKind::Contains, TEST, FAILING));
    assert!(has_relation(
        &output,
        RelationKind::Contains,
        FAILING,
        FAILING_OPEN
    ));
    assert!(has_relation(
        &output,
        RelationKind::Contains,
        TEST,
        "test_session_error_pops_context.<locals>.index"
    ));
    // The method is contained by its class, not directly by the test.
    assert!(!has_relation(
        &output,
        RelationKind::Contains,
        TEST,
        FAILING_OPEN
    ));
}

#[test]
fn a_nested_class_extends_its_imported_base_by_the_name_the_file_imports() {
    let output = extract(REQCTX);
    assert!(
        has_relation(&output, RelationKind::Extends, FAILING, "SessionInterface"),
        "{:?}",
        output.relations
    );
    assert!(has_relation(
        &output,
        RelationKind::Extends,
        "test_session_error_pops_context.<locals>.CustomFlask",
        "flask.Flask"
    ));
}

#[test]
fn calls_in_a_nested_method_belong_to_that_method() {
    let output = extract(REQCTX);
    let raised = calls_to(
        &output,
        "test_session_error_pops_context.<locals>.SessionError",
    );
    assert_eq!(
        raised
            .iter()
            .map(|r| r.src_name.as_str())
            .collect::<Vec<_>>(),
        vec![FAILING_OPEN],
        "`raise SessionError()` is the nested method's call, and names the class \
         the test defines: {:?}",
        output.relations
    );
    assert!(
        !output
            .relations
            .iter()
            .any(|r| r.kind == RelationKind::Calls
                && r.src_name == TEST
                && r.dst_name.ends_with("SessionError")),
        "the enclosing test must not keep a call written in the nested body"
    );
    let assertion = calls_to(&output, "AssertionError");
    assert_eq!(
        assertion
            .iter()
            .map(|r| r.src_name.as_str())
            .collect::<Vec<_>>(),
        vec!["test_session_error_pops_context.<locals>.index"]
    );

    let output = extract(SESSION_INTERFACE);
    let matched = calls_to(&output, "match_request");
    assert_eq!(
        matched
            .iter()
            .map(|r| r.src_name.as_str())
            .collect::<Vec<_>>(),
        vec!["test_open_session_with_endpoint.<locals>.MySessionInterface.open_session"],
        "{:?}",
        output.relations
    );
    assert_eq!(matched[0].receiver.as_deref(), Some("app_ctx"));
}

#[test]
fn a_call_to_a_local_class_names_the_local_entity() {
    let output = extract(REQCTX);
    let constructed = calls_to(
        &output,
        "test_session_error_pops_context.<locals>.CustomFlask",
    );
    assert_eq!(
        constructed
            .iter()
            .map(|r| r.src_name.as_str())
            .collect::<Vec<_>>(),
        vec![TEST],
        "`CustomFlask(__name__)` in the test body names the local class: {:?}",
        output.relations
    );

    let output = extract(SESSION_INTERFACE);
    let constructed = calls_to(
        &output,
        "test_open_session_with_endpoint.<locals>.MySessionInterface",
    );
    assert_eq!(
        constructed
            .iter()
            .map(|r| r.src_name.as_str())
            .collect::<Vec<_>>(),
        vec!["test_open_session_with_endpoint"]
    );
}

#[test]
fn a_call_in_a_nested_class_field_is_that_fields_call() {
    // A class initializer is walked in its lexical scope and charged to its
    // field. It sees the scopes of the function around it, so the call
    // names the test's local class rather than a module-level namesake.
    let output = extract(REQCTX);
    let at_site = |r: &&ExtractedRelation| {
        r.site
            .as_ref()
            .is_some_and(|site| REQCTX[site.start_byte..site.end_byte].starts_with("Failing"))
    };
    let calls: Vec<(&str, &str)> = output
        .relations
        .iter()
        .filter(|r| r.kind == RelationKind::Calls)
        .filter(at_site)
        .map(|r| (r.src_name.as_str(), r.dst_name.as_str()))
        .collect();
    assert_eq!(
        calls,
        vec![(
            "test_session_error_pops_context.<locals>.CustomFlask.session_interface",
            "test_session_error_pops_context.<locals>.FailingSessionInterface",
        )]
    );
    assert!(
        !output
            .relations
            .iter()
            .filter(|r| is_scoped_call_extraction_incomplete_marker(r))
            .any(|r| at_site(&r)),
        "a named, owned call is not a coverage gap"
    );
}

#[test]
fn both_flask_session_doubles_have_both_methods() {
    let output = extract(SESSION_INTERFACE);
    let class = "test_open_session_with_endpoint.<locals>.MySessionInterface";
    assert_eq!(kind_of(&output, class), Some(EntityKind::Class));
    for method in ["save_session", "open_session"] {
        let name = format!("{class}.{method}");
        assert_eq!(kind_of(&output, &name), Some(EntityKind::Method), "{name}");
        assert!(has_relation(&output, RelationKind::Contains, class, &name));
    }
    assert!(has_relation(
        &output,
        RelationKind::Contains,
        "test_open_session_with_endpoint",
        class
    ));
    assert!(has_relation(
        &output,
        RelationKind::Extends,
        class,
        "SessionInterface"
    ));
}

#[test]
fn two_tests_defining_the_same_nested_name_get_two_entities() {
    let output = extract(
        "def test_a(app):\n    @app.route(\"/\")\n    def index():\n        return 1\n\n\
         def test_b(app):\n    @app.route(\"/\")\n    def index():\n        return 2\n",
    );
    let indexes: Vec<&str> = output
        .entities
        .iter()
        .filter(|e| e.name.ends_with(".index"))
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(
        indexes,
        vec!["test_a.<locals>.index", "test_b.<locals>.index"]
    );
}

#[test]
fn a_nested_test_named_closure_is_not_a_test() {
    let output = extract(
        "def test_outer():\n    def test_inner():\n        pass\n    def helper():\n        pass\n    test_inner()\n",
    );
    let tests: Vec<&str> = output.tests.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(tests, vec!["test_outer"]);
    assert_eq!(
        kind_of(&output, "test_outer.<locals>.test_inner"),
        Some(EntityKind::Function)
    );
}

#[test]
fn a_nested_decorated_view_owns_its_decorator_call() {
    let output = extract(REQCTX);
    let routes = calls_to(&output, "route");
    assert_eq!(routes.len(), 1, "{:?}", output.relations);
    assert_eq!(
        routes[0].src_name,
        "test_session_error_pops_context.<locals>.index"
    );
    assert_eq!(routes[0].receiver.as_deref(), Some("app"));
    let index = output
        .entities
        .iter()
        .find(|e| e.name == "test_session_error_pops_context.<locals>.index")
        .expect("nested view");
    assert!(
        index.signature.starts_with("@route "),
        "the decorator is part of the nested declaration: {}",
        index.signature
    );
}

#[test]
fn a_nested_view_called_by_its_test_names_the_nested_entity() {
    // flask's `test_manual_context_binding` calls the view it registered. The
    // call is to the nested `index`, not to the test itself or to some
    // module-level `index`.
    let output = extract(
        "import flask\n\ndef index():\n    pass\n\n\
         def test_manual_context_binding(app):\n    @app.route(\"/\")\n    def index():\n        return flask.request.args['name']\n\n    assert index() == 'x'\n",
    );
    let calls = calls_to(&output, "test_manual_context_binding.<locals>.index");
    assert_eq!(
        calls
            .iter()
            .map(|r| r.src_name.as_str())
            .collect::<Vec<_>>(),
        vec!["test_manual_context_binding"]
    );
    assert!(
        calls_to(&output, "index").is_empty(),
        "{:?}",
        output.relations
    );
}

#[test]
fn a_local_base_class_is_extended_by_its_qualified_name() {
    let output =
        extract("def outer():\n    class A:\n        pass\n    class B(A):\n        pass\n");
    assert!(has_relation(
        &output,
        RelationKind::Extends,
        "outer.<locals>.B",
        "outer.<locals>.A"
    ));
}

#[test]
fn a_base_read_from_a_local_variable_names_that_variable() {
    // `base` is a parameter, a runtime value. Spelled bare it would read as a
    // module-level class `base`, which this file happens to declare.
    let output = extract(
        "class base:\n    pass\n\ndef t(base):\n    class C(base):\n        pass\n    return C\n",
    );
    assert!(
        has_relation(
            &output,
            RelationKind::Extends,
            "t.<locals>.C",
            "t.<locals>.base"
        ),
        "{:?}",
        output.relations
    );
    assert!(!has_relation(
        &output,
        RelationKind::Extends,
        "t.<locals>.C",
        "base"
    ));
}

#[test]
fn a_local_binding_hides_an_enclosing_nested_definition() {
    // `h` binds its own `g`, so its `g()` is neither the enclosing function's
    // nested `def g` nor the module's: it calls the parameter, and is recorded
    // as a gap on that name. `k` declares `global g`, which reaches the module.
    let output = extract(
        "def g():\n    pass\n\ndef outer():\n    def g():\n        pass\n    def h(g):\n        g()\n    def k():\n        global g\n        g()\n    def m():\n        g()\n",
    );
    let from = |src: &str| -> Vec<&str> {
        output
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Calls && r.src_name == src)
            .map(|r| r.dst_name.as_str())
            .collect()
    };
    assert_eq!(from("outer.<locals>.h"), Vec::<&str>::new());
    assert!(output
        .relations
        .iter()
        .any(|r| r.src_name == "outer.<locals>.h"
            && is_scoped_call_extraction_incomplete_marker(r)
            && r.receiver.as_deref() == Some("g")));
    assert_eq!(from("outer.<locals>.k"), vec!["g"]);
    assert_eq!(from("outer.<locals>.m"), vec!["outer.<locals>.g"]);
}

#[test]
fn a_nested_definition_read_as_a_value_is_referenced_by_its_entity() {
    let output = extract(
        "def test_view(app):\n    def index():\n        return 1\n    app.add_url_rule(\"/\", view_func=index)\n",
    );
    assert!(
        has_relation(
            &output,
            RelationKind::References,
            "test_view",
            "test_view.<locals>.index"
        ),
        "{:?}",
        output.relations
    );
}

#[test]
fn a_closure_reads_its_enclosing_functions_declared_receiver_types() {
    let output = extract(
        "from adapters import HTTPAdapter\n\n\
         def send(adapter: HTTPAdapter):\n    def retry():\n        adapter.send(1)\n    retry()\n",
    );
    let sends = calls_to(&output, "HTTPAdapter.send");
    assert_eq!(
        sends
            .iter()
            .map(|r| r.src_name.as_str())
            .collect::<Vec<_>>(),
        vec!["send.<locals>.retry"],
        "{:?}",
        output.relations
    );
}

#[test]
fn unique_block_definitions_are_admitted_and_conditional_collisions_are_censused() {
    let output = extract(
        "try:\n    from fast import speed\nexcept ImportError:\n    def speed():\n        pass\n\n\
         class Box:\n    if True:\n        def wide(self):\n            pass\n\n\
         def f():\n    if x:\n        def g():\n            a()\n    else:\n        def g():\n            b()\n",
    );
    let census = output
        .nested_definitions
        .as_ref()
        .expect("python takes a census");
    let recorded: Vec<(&str, EntityKind, Option<&str>)> = census
        .not_admitted
        .iter()
        .map(|d| (d.name.as_str(), d.kind, d.enclosing.as_deref()))
        .collect();
    assert_eq!(
        recorded,
        vec![
            ("speed", EntityKind::Function, Some("test_nested")),
            ("f.<locals>.g", EntityKind::Function, Some("f")),
            ("f.<locals>.g", EntityKind::Function, Some("f")),
        ]
    );
    assert_eq!(kind_of(&output, "Box.wide"), Some(EntityKind::Method));
    // Neither alternative gets a unique entity or invents an enclosing caller.
    assert_eq!(kind_of(&output, "f.<locals>.g"), None);
    assert_eq!(
        calls_to(&output, "a")
            .iter()
            .map(|r| r.src_name.as_str())
            .collect::<Vec<_>>(),
        Vec::<&str>::new()
    );
    assert_eq!(
        calls_to(&output, "b")
            .iter()
            .map(|r| r.src_name.as_str())
            .collect::<Vec<_>>(),
        Vec::<&str>::new()
    );
}

#[test]
fn a_file_with_every_nested_definition_admitted_records_an_empty_census() {
    let census = extract(REQCTX).nested_definitions.expect("census");
    assert!(census.not_admitted.is_empty(), "{:?}", census.not_admitted);
}

#[test]
fn a_class_nested_in_a_nested_class_keeps_the_qualified_owner() {
    let output = extract(
        "def outer():\n    class C:\n        class D:\n            def m(self):\n                self.n()\n",
    );
    assert_eq!(
        kind_of(&output, "outer.<locals>.C.D"),
        Some(EntityKind::Class)
    );
    assert_eq!(
        kind_of(&output, "outer.<locals>.C.D.m"),
        Some(EntityKind::Method)
    );
    assert!(has_relation(
        &output,
        RelationKind::Contains,
        "outer.<locals>.C",
        "outer.<locals>.C.D"
    ));
    assert_eq!(
        calls_to(&output, "outer.<locals>.C.D.n")
            .iter()
            .map(|r| r.src_name.as_str())
            .collect::<Vec<_>>(),
        vec!["outer.<locals>.C.D.m"]
    );
}

#[test]
fn a_module_level_class_in_a_class_keeps_its_plain_name() {
    let output = extract("class A:\n    class B:\n        def m(self):\n            pass\n");
    assert_eq!(kind_of(&output, "B"), Some(EntityKind::Class));
    assert_eq!(kind_of(&output, "B.m"), Some(EntityKind::Method));
}
