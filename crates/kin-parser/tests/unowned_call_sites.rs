// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Calls written where no declaration's own walk reads them.
//!
//! Each adapter charges a call to the declaration whose body it is walking.
//! A call in file-scope code, in class-body code outside any member, or in the
//! body of a nested definition the adapter records rather than admits has to
//! land on the innermost admitted entity holding it, the file's module for
//! file-scope code, instead of reaching no relation at all. Each family below
//! asserts the call site exists once and names that caller, and that a call a
//! declaration walk already owns keeps its caller and is not repeated.

use kin_model::{EntityKind, FilePathId, RelationKind};
use kin_parser::{
    CAdapter, CppAdapter, GoAdapter, JavaAdapter, JavaScriptAdapter, KotlinAdapter,
    LanguageAdapter, ParseOutput, PhpAdapter, PythonAdapter, RustAdapter, SwiftAdapter,
    TypeScriptAdapter,
};

fn extract(adapter: &dyn LanguageAdapter, path: &str, source: &str) -> ParseOutput {
    let tree = adapter.parse(source.as_bytes()).expect("parse");
    assert!(
        !tree.root_node().has_error(),
        "fixture must parse cleanly: {}",
        tree.root_node().to_sexp()
    );
    adapter
        .extract(&tree, source.as_bytes(), &FilePathId::new(path))
        .expect("extract")
}

/// The 0-based line of the first line of `source` containing `needle`.
fn line_of(source: &str, needle: &str) -> u32 {
    source
        .lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("fixture has no line containing {needle:?}")) as u32
}

/// The caller of every `Calls` relation to `callee` written on the line that
/// holds `needle`.
fn callers_at(output: &ParseOutput, source: &str, callee: &str, needle: &str) -> Vec<String> {
    let line = line_of(source, needle);
    output
        .relations
        .iter()
        .filter(|r| {
            r.kind == RelationKind::Calls
                && r.dst_name == callee
                && r.site.as_ref().is_some_and(|site| site.start_line == line)
        })
        .map(|r| r.src_name.clone())
        .collect()
}

/// Assert the call to `callee` on the line holding `needle` exists exactly
/// once and names `caller`.
#[track_caller]
fn assert_caller(output: &ParseOutput, source: &str, callee: &str, needle: &str, caller: &str) {
    let callers = callers_at(output, source, callee, needle);
    assert_eq!(
        callers,
        vec![caller.to_string()],
        "call to {callee} on `{needle}` should be charged once to {caller}; calls: {:#?}",
        output
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Calls)
            .map(|r| (
                &r.src_name,
                &r.dst_name,
                r.site.as_ref().map(|s| s.start_line)
            ))
            .collect::<Vec<_>>()
    );
}

fn has_entity(output: &ParseOutput, name: &str) -> bool {
    output.entities.iter().any(|entity| entity.name == name)
}

// ── JavaScript and TypeScript ───────────────────────────────────────────────

/// The shape a test file is written in, taken from TanStack Query's
/// `useMutation.test.tsx`: every call sits in a callback passed to a
/// file-scope `describe`/`it`, and `Page` is a nested function the adapter
/// records rather than admits.
const TSX_TEST: &str = "import { fireEvent } from '@testing-library/react'
import { useMutation } from '..'
import { renderWithClient } from './utils'

describe('useMutation', () => {
  it('should be able to reset `data`', async () => {
    function Page() {
      const { mutate, data = 'empty', reset } = useMutation({ mutationFn: () => Promise.resolve('mutation') })
      return (
        <div>
          <h1>{data}</h1>
          <button onClick={() => reset()}>reset</button>
          <button onClick={() => mutate()}>mutate</button>
        </div>
      )
    }
    const { getByRole } = renderWithClient(queryClient, <Page />)
    fireEvent.click(getByRole('button', { name: /mutate/i }))
  })
})
";

#[test]
fn a_test_file_callback_call_is_charged_to_the_module() {
    let output = extract(
        &TypeScriptAdapter,
        "packages/react-query/src/__tests__/useMutation.test.tsx",
        TSX_TEST,
    );
    let module = "useMutation.test";
    assert!(output
        .entities
        .iter()
        .any(|entity| entity.name == module && entity.kind == EntityKind::Module));
    assert_caller(
        &output,
        TSX_TEST,
        "describe",
        "describe('useMutation'",
        module,
    );
    assert_caller(&output, TSX_TEST, "it", "it('should be able", module);
    assert_caller(&output, TSX_TEST, "useMutation", "= useMutation(", module);
    assert_caller(&output, TSX_TEST, "reset", "=> reset()", module);
    assert_caller(&output, TSX_TEST, "mutate", "=> mutate()", module);
    assert_caller(
        &output,
        TSX_TEST,
        "renderWithClient",
        "renderWithClient(",
        module,
    );
    assert_caller(&output, TSX_TEST, "click", "fireEvent.click(", module);

    // The admission rule is unchanged: `Page` is still recorded, not admitted,
    // and the census still names the module as the entity its calls are on.
    assert!(!has_entity(&output, "Page"));
    let census = output.nested_definitions.as_ref().expect("census taken");
    let page = census
        .not_admitted
        .iter()
        .find(|definition| definition.name == "Page")
        .expect("Page is recorded");
    assert_eq!(page.enclosing.as_deref(), Some(module));
}

const JS_MODULE: &str = "setup();

export class Store {
  cache = createCache();
  get(key) {
    return this.cache.read(key);
  }
}

export function load(items) {
  items.forEach((item) => {
    process(item);
  });
}
";

#[test]
fn a_javascript_class_field_call_goes_to_the_field_and_a_walked_call_stays() {
    let output = extract(&JavaScriptAdapter, "src/store.js", JS_MODULE);
    assert_caller(&output, JS_MODULE, "setup", "setup();", "store");
    // A field's initializer belongs to the field entity, the innermost holding it.
    assert_caller(
        &output,
        JS_MODULE,
        "createCache",
        "createCache()",
        "Store.cache",
    );
    // Calls the declaration walks own keep their caller and are not repeated.
    assert_caller(&output, JS_MODULE, "read", "this.cache.read", "Store.get");
    assert_caller(&output, JS_MODULE, "process", "process(item)", "load");
}

// ── Python ──────────────────────────────────────────────────────────────────

const PYTHON_MODULE: &str = "from django.db import models
import app

settings = configure()

class Article(models.Model):
    title = models.CharField(max_length=100)

    def publish(self):
        notify(self)

@app.route('/articles')
def index():
    return render()

def test_nested_model():
    class Draft(models.Model):
        body = models.TextField()
    return Draft

if __name__ == '__main__':
    main()
";

#[test]
fn python_module_and_class_body_calls_have_a_caller() {
    let output = extract(&PythonAdapter, "blog/models.py", PYTHON_MODULE);
    assert_caller(
        &output,
        PYTHON_MODULE,
        "configure",
        "configure()",
        "settings",
    );
    assert_caller(
        &output,
        PYTHON_MODULE,
        "CharField",
        "models.CharField(",
        "Article.title",
    );
    assert_caller(&output, PYTHON_MODULE, "main", "    main()", "models");
    // Class-body code in a class declared inside a test function.
    assert_caller(
        &output,
        PYTHON_MODULE,
        "TextField",
        "models.TextField(",
        "test_nested_model.<locals>.Draft.body",
    );
    // Owned calls stay put, and the decorator is not charged twice.
    assert_caller(
        &output,
        PYTHON_MODULE,
        "notify",
        "notify(self)",
        "Article.publish",
    );
    assert_eq!(
        callers_at(&output, PYTHON_MODULE, "route", "@app.route"),
        vec!["index".to_string()]
    );
    // A call that now has a caller is no longer a coverage gap.
    let gaps = kin_parser::file_evidence::call_extraction_gaps_of(&output.relations);
    assert!(
        gaps.is_empty(),
        "every call here is named and owned: {gaps:?}"
    );
}

// ── Go ──────────────────────────────────────────────────────────────────────

const GO_PACKAGE: &str = "package store

import \"errors\"

var ErrMissing = errors.New(\"missing\")

var client = newClient()

var handler = func() {
	serve()
}

func Run(items []int) {
	each(items, func(item int) {
		process(item)
	})
}
";

#[test]
fn a_go_package_level_initializer_call_goes_to_its_variable() {
    let output = extract(&GoAdapter, "store/store.go", GO_PACKAGE);
    assert_caller(&output, GO_PACKAGE, "newClient", "newClient()", "client");
    assert_caller(&output, GO_PACKAGE, "serve", "serve()", "handler");
    let missing = output
        .relations
        .iter()
        .find(|r| r.kind == RelationKind::Calls && r.dst_name == "New")
        .expect("errors.New is a call");
    assert_eq!(missing.src_name, "ErrMissing");
    assert_eq!(
        missing.import_source.as_deref(),
        Some("errors"),
        "the package qualifier travels with an adopted call"
    );
    assert_caller(&output, GO_PACKAGE, "process", "process(item)", "Run");
}

// ── Rust ────────────────────────────────────────────────────────────────────

const RUST_MODULE: &str = "thread_local! {
    static CACHE: RefCell<u32> = RefCell::new(seed());
}

define_rejection! {
    #[status = UNPROCESSABLE_ENTITY]
    #[cfg_attr(docsrs, doc(cfg(feature = \"json\")))]
    pub struct JsonDataError(Error);
}

fn outer() {
    { fn helper() { first_call(); } }
    { fn helper() { second_call(); } }
}
";

#[test]
fn a_rust_item_macro_call_goes_to_the_module_and_its_attributes_are_not_calls() {
    let output = extract(&RustAdapter, "src/cache.rs", RUST_MODULE);
    assert_caller(&output, RUST_MODULE, "seed", "seed()", "cache");
    for not_a_call in ["cfg", "cfg_attr", "doc", "JsonDataError"] {
        assert!(
            !output
                .relations
                .iter()
                .any(|r| r.kind == RelationKind::Calls && r.dst_name == not_a_call),
            "{not_a_call} is an attribute or a declaration, not a call"
        );
    }
    // The second `helper` is recorded, not admitted, and its call stays with
    // the function around it.
    assert_caller(
        &output,
        RUST_MODULE,
        "second_call",
        "second_call()",
        "outer",
    );
    assert_caller(
        &output,
        RUST_MODULE,
        "first_call",
        "first_call()",
        "outer.<locals>.helper",
    );
}

// ── Java and Kotlin ─────────────────────────────────────────────────────────

const JAVA_CLASS: &str = "class Registry {
    static int SIZE = computeSize();
    private final Map<String, Integer> index = buildIndex();
    static {
        registerDefaults();
    }
    void refresh() {
        Runnable task = () -> reload();
    }
}
";

#[test]
fn java_field_initializer_calls_go_to_the_field_and_block_calls_to_the_class() {
    let output = extract(&JavaAdapter, "src/Registry.java", JAVA_CLASS);
    assert_caller(
        &output,
        JAVA_CLASS,
        "computeSize",
        "computeSize()",
        "Registry.SIZE",
    );
    assert_caller(
        &output,
        JAVA_CLASS,
        "buildIndex",
        "buildIndex()",
        "Registry.index",
    );
    assert_caller(
        &output,
        JAVA_CLASS,
        "registerDefaults",
        "registerDefaults()",
        "Registry",
    );
    assert_caller(
        &output,
        JAVA_CLASS,
        "reload",
        "reload()",
        "Registry.refresh",
    );
}

const KOTLIN_FILE: &str = "val registry = buildRegistry()

class Cache {
    val store = openStore()
    init {
        warmUp()
    }
}

fun outer() {
    fun inner() { innerCall() }
    inner()
}
";

#[test]
fn kotlin_property_and_init_calls_have_a_caller() {
    let output = extract(&KotlinAdapter, "src/Cache.kt", KOTLIN_FILE);
    assert_caller(
        &output,
        KOTLIN_FILE,
        "buildRegistry",
        "buildRegistry()",
        "registry",
    );
    assert_caller(
        &output,
        KOTLIN_FILE,
        "openStore",
        "openStore()",
        "Cache.store",
    );
    assert_caller(&output, KOTLIN_FILE, "warmUp", "warmUp()", "Cache");
    assert_caller(&output, KOTLIN_FILE, "innerCall", "innerCall()", "outer");
}

// ── C and C++ ───────────────────────────────────────────────────────────────

const CPP_FILE: &str = "struct Config {
    int limit = compute_limit();
};

void outer() {
    { struct Local { void m() { first_call(); } }; }
    { struct Local { void m() { second_call(); } }; }
}
";

#[test]
fn a_cpp_member_initializer_call_goes_to_the_member() {
    let output = extract(&CppAdapter, "src/config.cpp", CPP_FILE);
    assert_caller(
        &output,
        CPP_FILE,
        "compute_limit",
        "compute_limit()",
        "Config::limit",
    );
    assert_caller(&output, CPP_FILE, "second_call", "second_call()", "outer");
    assert_caller(
        &output,
        CPP_FILE,
        "first_call",
        "first_call()",
        "outer.<locals>.Local::m",
    );
}

const C_FILE: &str = "static int opener(void) { return helper(); }
EXPORT_SYMBOL(opener);
";

#[test]
fn a_c_file_scope_macro_stays_out_and_leaves_the_file_measured() {
    let output = extract(&CAdapter, "drivers/opener.c", C_FILE);
    assert_caller(&output, C_FILE, "helper", "helper()", "opener");
    assert!(
        !output
            .relations
            .iter()
            .any(|r| r.kind == RelationKind::Calls && r.dst_name == "EXPORT_SYMBOL"),
        "C has no file module to own a file-scope macro"
    );
    assert!(
        !output
            .relations
            .iter()
            .any(kin_parser::is_call_extraction_incomplete_marker),
        "a macro that declares something does not make the file unmeasured"
    );
}

// ── Swift ───────────────────────────────────────────────────────────────────

const SWIFT_MAIN: &str = "let config = loadConfig()
setUp()

class Cache {
    let store = openStore()
}

func outer() {
    items.forEach { item in
        process(item)
    }
}
";

#[test]
fn swift_top_level_and_stored_property_calls_have_a_caller() {
    let output = extract(&SwiftAdapter, "Sources/App/main.swift", SWIFT_MAIN);
    assert_caller(&output, SWIFT_MAIN, "loadConfig", "loadConfig()", "config");
    assert_caller(&output, SWIFT_MAIN, "setUp", "setUp()", "main");
    assert_caller(
        &output,
        SWIFT_MAIN,
        "openStore",
        "openStore()",
        "Cache.store",
    );
    assert_caller(&output, SWIFT_MAIN, "process", "process(item)", "outer");
}

// ── PHP ─────────────────────────────────────────────────────────────────────

const PHP_TEST: &str = "<?php
bootstrap();

it('adds numbers', function () {
    expect(add(1, 2))->toBe(3);
});

function outer() {
    $format = function ($value) { return render($value); };
}
";

#[test]
fn php_top_level_and_test_closure_calls_go_to_the_module() {
    let output = extract(&PhpAdapter, "tests/AddTest.php", PHP_TEST);
    assert_caller(&output, PHP_TEST, "bootstrap", "bootstrap()", "AddTest");
    assert_caller(&output, PHP_TEST, "it", "it('adds numbers'", "AddTest");
    assert_caller(&output, PHP_TEST, "add", "expect(add(1, 2))", "AddTest");
    assert_caller(
        &output,
        PHP_TEST,
        "render",
        "render($value)",
        "outer.<locals>.format",
    );
}
