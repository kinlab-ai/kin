// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Calls the receiver-origin facts prove reach outside the repository, batch
//! and incremental, over parsed fixtures.

use super::*;
use crate::coverage_certificate::CertificateDetail;
use crate::resolution::RECEIVER_ORIGIN_CONFIDENCE;

fn fixture(path: &str, source: &str) -> FileParseData {
    let indexed = crate::IndexPipeline::new()
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

fn artifact_ids(files: &[FileParseData]) -> ArtifactIdentityMap {
    files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect()
}

fn completeness(files: &[FileParseData]) -> FileParseCompletenessMap {
    files
        .iter()
        .map(|file| (file.file_path.clone(), ParseCompleteness::Full))
        .collect()
}

/// A link's relations with the artifact identity each file got.
struct Linked {
    label: &'static str,
    ids: ArtifactIdentityMap,
    relations: Vec<Relation>,
}

fn link_batch(files: &[FileParseData]) -> Linked {
    let ids = artifact_ids(files);
    let relations = link_cross_file_with_completeness(files, &ids, &completeness(files)).unwrap();
    Linked {
        label: "batch",
        ids,
        relations,
    }
}

fn incremental_linker(files: &[FileParseData], ids: &ArtifactIdentityMap) -> IncrementalLinker {
    let mut linker = IncrementalLinker::new();
    for file in files {
        linker.add_file(&file.file_path, ids[&file.file_path], &file.entities);
    }
    linker
}

fn link_incremental(files: &[FileParseData]) -> Linked {
    let ids = artifact_ids(files);
    let linker = incremental_linker(files, &ids);
    let relations =
        link_cross_file_incremental_with_completeness(files, &linker, &completeness(files))
            .unwrap();
    Linked {
        label: "incremental",
        ids,
        relations,
    }
}

fn both(files: &[FileParseData]) -> [Linked; 2] {
    [link_batch(files), link_incremental(files)]
}

fn entity<'a>(files: &'a [FileParseData], file: &str, name: &str) -> &'a Entity {
    files
        .iter()
        .find(|parsed| parsed.file_path == file)
        .and_then(|parsed| parsed.entities.iter().find(|entity| entity.name == name))
        .unwrap_or_else(|| panic!("no entity {name} in {file}"))
}

/// The certificate's receiver-outside entries for `file`, as (method, chain,
/// the source text of the call).
fn outside(linked: &Linked, file: &str) -> Vec<(String, String, String)> {
    let artifact = GraphNodeId::Artifact(linked.ids[file]);
    let certificate = linked
        .relations
        .iter()
        .find(|relation| {
            relation.kind == RelationKind::DependsOn
                && relation.src == artifact
                && relation.dst == artifact
                && coverage_evidence(relation, IMPORT_RESOLUTION_COVERAGE_V1).is_some()
        })
        .unwrap_or_else(|| panic!("{}: no certificate for {file}", linked.label));
    assert!(
        is_parse_coverage_relation(certificate, file, linked.ids[file]),
        "{}: the certificate must still be this factory's",
        linked.label
    );
    let detail = CertificateDetail::from_certificate(certificate).expect("detail reads back");
    let source = source_of(file);
    detail
        .receiver_outside
        .into_iter()
        .map(|entry| {
            (
                entry.method,
                entry.chain,
                source[entry.span.start_byte..entry.span.end_byte].to_string(),
            )
        })
        .collect()
}

/// Where each call the certificate of `file` records as settled outside
/// starts.
fn outside_starts(linked: &Linked, file: &str) -> Vec<usize> {
    let artifact = GraphNodeId::Artifact(linked.ids[file]);
    linked
        .relations
        .iter()
        .filter(|relation| relation.src == artifact && relation.dst == artifact)
        .find_map(CertificateDetail::from_certificate)
        .map(|detail| {
            detail
                .receiver_outside
                .iter()
                .map(|entry| entry.span.start_byte)
                .collect()
        })
        .unwrap_or_default()
}

fn source_of(file: &str) -> &'static str {
    SOURCES
        .with(|sources| {
            sources
                .borrow()
                .iter()
                .find(|(path, _)| path == file)
                .map(|(_, text)| *text)
        })
        .unwrap_or_else(|| panic!("no source recorded for {file}"))
}

thread_local! {
    static SOURCES: std::cell::RefCell<Vec<(String, &'static str)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn parsed(path: &str, source: &'static str) -> FileParseData {
    SOURCES.with(|sources| sources.borrow_mut().push((path.to_string(), source)));
    fixture(path, source)
}

/// Every call edge from `caller`, as (target name, confidence).
fn calls_from(linked: &Linked, files: &[FileParseData], caller: &Entity) -> Vec<(String, f32)> {
    let names: HashMap<EntityId, &str> = files
        .iter()
        .flat_map(|file| file.entities.iter())
        .map(|entity| (entity.id, entity.name.as_str()))
        .collect();
    let mut out: Vec<_> = linked
        .relations
        .iter()
        .filter(|relation| {
            relation.kind == RelationKind::Calls && relation.src == GraphNodeId::Entity(caller.id)
        })
        .filter_map(|relation| {
            let target = names.get(&relation.dst.as_entity()?)?;
            Some((target.to_string(), relation.confidence))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

const CTX: &str = "import typing_extensions as te\n\n\
class _AppCtxGlobals:\n    def pop(self, name, default=None):\n        return None\n\n\
class AppContext:\n    def push(self) -> None:\n        pass\n\n    \
def pop(self, exc=None) -> None:\n        pass\n\n    \
def __enter__(self) -> te.Self:\n        self.push()\n        return self\n";

const APP: &str = "from .ctx import AppContext\n\n\
class Flask:\n    def test_request_context(self, *args, **kwargs) -> AppContext:\n        return AppContext()\n";

const CONFTEST: &str = "import pytest\nfrom flask import Flask\n\n\
@pytest.fixture\ndef app():\n    app = Flask('flask_test')\n    return app\n\n\
@pytest.fixture\ndef client(app):\n    return app.test_client()\n\n\
@pytest.fixture\ndef leak_detector():\n    _app_ctx = AppContext()\n    yield\n    _app_ctx.pop()\n";

const TEST_BASIC: &str = "import typing as t\n\nimport flask\nimport pytest\n\n\
def pull_lang_code(endpoint: str | None, values: dict[str, t.Any] | None) -> None:\n    \
flask.g.lang_code = values.pop('lang_code', None)\n\n\
def untyped_pull(endpoint, values):\n    values.pop('lang_code', None)\n\n\
def test_max_cookie_size(app, client, recwarn):\n    w = recwarn.pop()\n    assert w\n\n\
def test_literals(app):\n    seen = {'a': 1}\n    seen.pop('a')\n    stack = [1, 2]\n    \
stack.pop()\n    ctx = app.test_request_context()\n    ctx.pop()\n\n\
def test_protocol(app):\n    seen = {'a': 1}\n    seen.__getitem__('a')\n";

fn flask_files() -> Vec<FileParseData> {
    vec![
        parsed("src/flask/ctx.py", CTX),
        parsed("src/flask/app.py", APP),
        parsed("src/flask/__init__.py", "from .app import Flask as Flask\n"),
        parsed("tests/conftest.py", CONFTEST),
        parsed("tests/test_basic.py", TEST_BASIC),
    ]
}

fn receivers(entries: &[(String, String, String)]) -> Vec<String> {
    let mut receivers: Vec<String> = entries
        .iter()
        .map(|(_, _, call)| {
            call.split('.')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string()
        })
        .collect();
    receivers.sort();
    receivers
}

#[test]
fn builtin_values_and_a_builtin_fixture_settle_elsewhere_while_the_fixture_chain_still_binds() {
    let files = flask_files();
    for linked in both(&files) {
        let label = linked.label;
        let entries = outside(&linked, "tests/test_basic.py");
        assert_eq!(
            receivers(&entries),
            vec!["recwarn", "seen", "stack", "values"],
            "{label}: {entries:#?}"
        );
        assert!(
            entries.iter().all(|(method, _, _)| method == "pop"),
            "{label}: {entries:#?}"
        );
        let chain = |receiver: &str| {
            entries
                .iter()
                .find(|(_, _, call)| call.starts_with(receiver))
                .map(|(_, chain, _)| chain.clone())
                .unwrap()
        };
        assert!(
            chain("recwarn").contains("pytest_fixture recwarn has no provider in the repository")
                && chain("recwarn").contains("_pytest.recwarn.WarningsRecorder")
                && chain("recwarn").ends_with("WarningsRecorder.pop is outside the repository"),
            "{label}: {}",
            chain("recwarn")
        );
        assert!(
            chain("values").starts_with("values: dict"),
            "{label}: {}",
            chain("values")
        );

        // None of the settled callers gains a same-named repository member.
        for caller in ["pull_lang_code", "test_max_cookie_size"] {
            let calls = calls_from(
                &linked,
                &files,
                entity(&files, "tests/test_basic.py", caller),
            );
            assert!(
                !calls.iter().any(|(name, _)| name.ends_with(".pop")),
                "{label}: {caller}: {calls:?}"
            );
        }
        // The fixture chain still binds `ctx.pop()` beside the literals.
        let calls = calls_from(
            &linked,
            &files,
            entity(&files, "tests/test_basic.py", "test_literals"),
        );
        assert!(
            calls
                .iter()
                .any(|(name, confidence)| name == "AppContext.pop"
                    && *confidence == RECEIVER_ORIGIN_CONFIDENCE),
            "{label}: {calls:?}"
        );
        assert!(
            !calls.iter().any(|(name, _)| name == "_AppCtxGlobals.pop"),
            "{label}: {calls:?}"
        );
    }
}

#[test]
fn an_untyped_parameter_and_a_protocol_member_stay_unsettled() {
    let files = flask_files();
    for linked in both(&files) {
        let entries = outside(&linked, "tests/test_basic.py");
        assert!(
            !entries.iter().any(|(method, _, _)| method == "__getitem__"),
            "{}: a protocol member is never settled by a negative fact: {entries:#?}",
            linked.label
        );
        // `untyped_pull`'s `values` could hold anything, an AppContext
        // included, so only the annotated one is settled.
        let values: Vec<&String> = entries
            .iter()
            .filter(|(_, _, call)| call.starts_with("values"))
            .map(|(_, chain, _)| chain)
            .collect();
        assert_eq!(values.len(), 1, "{}: {entries:#?}", linked.label);
        assert!(
            values[0].starts_with("values: dict"),
            "{}: {values:?}",
            linked.label
        );
    }
}

#[test]
fn a_nearer_user_fixture_shadows_the_builtin_one() {
    let mut files = flask_files();
    files.push(parsed(
        "tests/shadow/conftest.py",
        "import pytest\nfrom flask.ctx import AppContext\n\n\
         @pytest.fixture\ndef recwarn():\n    return AppContext()\n",
    ));
    files.push(parsed(
        "tests/shadow/test_shadow.py",
        "def test_shadowed(recwarn):\n    recwarn.pop()\n",
    ));
    for linked in both(&files) {
        let entries = outside(&linked, "tests/shadow/test_shadow.py");
        assert!(entries.is_empty(), "{}: {entries:#?}", linked.label);
        let calls = calls_from(
            &linked,
            &files,
            entity(&files, "tests/shadow/test_shadow.py", "test_shadowed"),
        );
        assert!(
            calls
                .iter()
                .any(|(name, confidence)| name == "AppContext.pop"
                    && *confidence == RECEIVER_ORIGIN_CONFIDENCE),
            "{}: the user's fixture binds: {calls:?}",
            linked.label
        );
    }
}

#[test]
fn a_fixture_the_walk_cannot_see_withdraws_the_builtin_fallback() {
    let mut files = flask_files();
    files.push(parsed(
        "tests/plugged/conftest.py",
        "pytest_plugins = ['tests.plugged.helpers']\n",
    ));
    files.push(parsed(
        "tests/plugged/test_plugged.py",
        "def test_plugged(recwarn):\n    recwarn.pop()\n",
    ));
    files.push(parsed(
        "tests/starred/conftest.py",
        "from tests.starred.helpers import *\n",
    ));
    files.push(parsed(
        "tests/starred/test_starred.py",
        "def test_starred(recwarn):\n    recwarn.pop()\n",
    ));
    files.push(parsed(
        "tests/imported/test_imported.py",
        "from tests.imported.helpers import recwarn\n\n\
         def test_imported(recwarn):\n    recwarn.pop()\n",
    ));
    for linked in both(&files) {
        for file in [
            "tests/plugged/test_plugged.py",
            "tests/starred/test_starred.py",
            "tests/imported/test_imported.py",
        ] {
            let entries = outside(&linked, file);
            assert!(entries.is_empty(), "{}: {file}: {entries:#?}", linked.label);
        }
    }
}

#[test]
fn a_repository_subclass_that_holds_the_method_keeps_a_declared_container_unsettled() {
    let mut files = flask_files();
    files.push(parsed(
        "src/flask/structures.py",
        "class TrackedDict(dict):\n    def pop(self, key, default=None):\n        return None\n",
    ));
    files.push(parsed(
        "tests/test_structures.py",
        "def takes(values: dict) -> None:\n    values.pop('x')\n\n\
         def builds() -> None:\n    values = dict(a=1)\n    values.pop('a')\n",
    ));
    for linked in both(&files) {
        let entries = outside(&linked, "tests/test_structures.py");
        // `dict(...)` constructs exactly a dict; an annotation admits a
        // TrackedDict, whose own `pop` the call may reach.
        let settled: Vec<&str> = entries.iter().map(|(_, chain, _)| chain.as_str()).collect();
        assert_eq!(settled.len(), 1, "{}: {entries:#?}", linked.label);
        assert!(
            settled[0].starts_with("values: dict"),
            "{}: {settled:?}",
            linked.label
        );
        let takes = entity(&files, "tests/test_structures.py", "takes");
        let builds = entity(&files, "tests/test_structures.py", "builds");
        let text = source_of("tests/test_structures.py");
        let in_builds = entries.iter().all(|(_, _, call)| {
            let at = text.find(call.as_str()).unwrap();
            at > text.find("def builds").unwrap()
        });
        assert!(in_builds, "{}: {entries:#?}", linked.label);
        assert_ne!(takes.id, builds.id);
    }
}

#[test]
fn a_shadowed_builtin_constructor_is_not_the_builtin() {
    let mut files = flask_files();
    files.push(parsed(
        "tests/test_shadowed_builtin.py",
        "from flask.ctx import AppContext as dict\n\n\
         def test_rebound(app):\n    values = dict()\n    values.pop()\n",
    ));
    for linked in both(&files) {
        let entries = outside(&linked, "tests/test_shadowed_builtin.py");
        assert!(entries.is_empty(), "{}: {entries:#?}", linked.label);
    }
}

#[test]
fn the_incremental_certificate_reads_the_conftest_through_the_graph() {
    let files = flask_files();
    let ids = artifact_ids(&files);
    let linker = incremental_linker(&files, &ids);
    let graph = kin_db::InMemoryGraph::new();
    for file in &files {
        for entity in &file.entities {
            kin_model::EntityStore::upsert_entity(&graph, entity).unwrap();
        }
    }
    let test_file = files
        .iter()
        .find(|file| file.file_path == "tests/test_basic.py")
        .unwrap();
    let with_graph = build_incremental_parse_coverage_relation_with_graph(
        test_file,
        ids["tests/test_basic.py"],
        &ParseCompleteness::Full,
        &linker,
        &graph,
    );
    let detail = CertificateDetail::from_certificate(&with_graph).unwrap();
    assert_eq!(detail.receiver_outside.len(), 4, "{detail:#?}");
    // Without the graph the conftest's facts are unreadable, so the fixture
    // walk stops before the built-in fallback.
    let without = build_incremental_parse_coverage_relation(
        test_file,
        ids["tests/test_basic.py"],
        &ParseCompleteness::Full,
        &linker,
    );
    let detail = CertificateDetail::from_certificate(&without).unwrap();
    assert_eq!(detail.receiver_outside.len(), 3, "{detail:#?}");
}

const STORE_TS: &str = "export class Store {\n  pop(): number {\n    return 1;\n  }\n  \
get(key: string): number {\n    return key.length;\n  }\n}\n";

const QUEUE_TS: &str = "export class Queue {\n  pop(): number {\n    return 2;\n  }\n}\n";

const USE_TS: &str = "import { Store } from './store';\n\n\
export function drain(store: Store) {\n  const items = [1, 2];\n  items.pop();\n  \
const cache = new Map<string, number>();\n  cache.get('a');\n  store.pop();\n}\n\n\
export function take(items: number[]) {\n  items.pop();\n}\n\n\
export class Panel {\n  constructor(private store: Store) {}\n  run() {\n    this.store.pop();\n  }\n}\n";

fn js_files() -> Vec<FileParseData> {
    vec![
        parsed("src/store.ts", STORE_TS),
        parsed("src/queue.ts", QUEUE_TS),
        parsed("src/use.ts", USE_TS),
    ]
}

#[test]
fn an_array_and_a_map_settle_elsewhere_beside_a_typed_store_receiver() {
    let files = js_files();
    for linked in both(&files) {
        let label = linked.label;
        let entries = outside(&linked, "src/use.ts");
        let mut calls: Vec<&str> = entries.iter().map(|(_, _, call)| call.as_str()).collect();
        calls.sort();
        assert_eq!(
            calls,
            vec!["cache.get('a')", "items.pop()"],
            "{label}: {entries:#?}"
        );
        assert!(
            entries
                .iter()
                .any(|(_, chain, _)| chain == "items: Array -> Array.pop is outside the repository"),
            "{label}: {entries:#?}"
        );
        let drain = entity(&files, "src/use.ts", "drain");
        let calls = calls_from(&linked, &files, drain);
        assert!(
            calls.iter().any(|(name, confidence)| name == "Store.pop"
                && *confidence == RECEIVER_ORIGIN_CONFIDENCE),
            "{label}: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|(name, _)| name == "Queue.pop" || name == "Store.get"),
            "{label}: the array and the map reach neither repository member: {calls:?}"
        );
        // A declared `number[]` fits any object shaped like an array, and the
        // repository declares a `pop`, so that call stays unsettled.
        let take = entity(&files, "src/use.ts", "take").span.clone().unwrap();
        let drain_span = drain.span.clone().unwrap();
        let starts = outside_starts(&linked, "src/use.ts");
        assert_eq!(starts.len(), 2, "{label}");
        for start in starts {
            assert!(
                start >= drain_span.start_byte && start < drain_span.end_byte,
                "{label}: only drain's literal and map settle"
            );
            assert!(start < take.start_byte || start >= take.end_byte, "{label}");
        }
        let run = entity(&files, "src/use.ts", "Panel.run");
        let calls = calls_from(&linked, &files, run);
        assert!(
            calls.iter().any(|(name, confidence)| name == "Store.pop"
                && *confidence == RECEIVER_ORIGIN_CONFIDENCE),
            "{label}: a constructor parameter property types this.store: {calls:?}"
        );
    }
}

#[test]
fn a_unittest_setup_attribute_binds_the_test_methods_calls() {
    let mut files = vec![
        parsed(
            "app/client.py",
            "class Client:\n    def get(self, path):\n        return path\n\n\
             class Other:\n    def get(self, path):\n        return None\n",
        ),
        parsed(
            "tests/test_views.py",
            "import unittest\nfrom app.client import Client\n\n\
             class TestViews(unittest.TestCase):\n    def setUp(self):\n        self.client = Client()\n\n    \
             def test_get(self):\n        self.client.get('/')\n",
        ),
    ];
    for linked in both(&files) {
        let test = entity(&files, "tests/test_views.py", "TestViews.test_get");
        let calls = calls_from(&linked, &files, test);
        assert!(
            calls.iter().any(|(name, confidence)| name == "Client.get"
                && *confidence == RECEIVER_ORIGIN_CONFIDENCE),
            "{}: {calls:?}",
            linked.label
        );
        assert!(
            !calls.iter().any(|(name, _)| name == "Other.get"),
            "{calls:?}"
        );
    }
    // A subclass could bind the attribute to something else in its own setUp
    // and run the inherited test method with it.
    files.push(parsed(
        "tests/test_more.py",
        "from app.client import Other\nfrom tests.test_views import TestViews\n\n\
         class TestMore(TestViews):\n    def setUp(self):\n        self.client = Other()\n",
    ));
    for linked in both(&files) {
        let test = entity(&files, "tests/test_views.py", "TestViews.test_get");
        let calls = calls_from(&linked, &files, test);
        assert!(
            !calls
                .iter()
                .any(|(_, confidence)| *confidence == RECEIVER_ORIGIN_CONFIDENCE),
            "{}: {calls:?}",
            linked.label
        );
    }
}

#[test]
fn a_fastapi_dependency_binds_through_what_its_provider_yields() {
    let files = vec![
        parsed(
            "app/services.py",
            "class Service:\n    def load(self):\n        return 1\n\n\
             class Loader:\n    def load(self):\n        return 2\n",
        ),
        parsed(
            "app/deps.py",
            "from app.services import Service\n\n\
             def get_service():\n    svc = Service()\n    try:\n        yield svc\n    finally:\n        pass\n",
        ),
        parsed(
            "app/main.py",
            "from fastapi import Depends\nfrom app.deps import get_service\n\n\
             def read(svc = Depends(get_service)):\n    return svc.load()\n",
        ),
    ];
    for linked in both(&files) {
        let read = entity(&files, "app/main.py", "read");
        let calls = calls_from(&linked, &files, read);
        assert!(
            calls.iter().any(|(name, confidence)| name == "Service.load"
                && *confidence == RECEIVER_ORIGIN_CONFIDENCE),
            "{}: {calls:?}",
            linked.label
        );
        assert!(
            !calls.iter().any(|(name, _)| name == "Loader.load"),
            "{calls:?}"
        );
    }
}
