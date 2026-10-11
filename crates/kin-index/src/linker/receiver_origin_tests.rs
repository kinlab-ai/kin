// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The receiver-origin tier, batch and incremental, over parsed fixtures.

use super::*;
use crate::resolution::{
    RelationResolution, RECEIVER_ORIGIN_CONFIDENCE, RECEIVER_ORIGIN_NAME_CONFIDENCE,
};

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

fn link_batch(files: &[FileParseData]) -> Vec<Relation> {
    super::link_cross_file(files, &artifact_ids(files)).unwrap()
}

fn link_incremental(files: &[FileParseData]) -> Vec<Relation> {
    let ids = artifact_ids(files);
    let mut linker = IncrementalLinker::new();
    for file in files {
        linker.add_file(&file.file_path, ids[&file.file_path], &file.entities);
    }
    super::link_cross_file_incremental(files, &linker).unwrap()
}

fn entity<'a>(files: &'a [FileParseData], file: &str, name: &str) -> &'a Entity {
    files
        .iter()
        .find(|parsed| parsed.file_path == file)
        .and_then(|parsed| parsed.entities.iter().find(|entity| entity.name == name))
        .unwrap_or_else(|| panic!("no entity {name} in {file}"))
}

/// Every call edge from `caller`, as (target name, confidence, chain).
fn calls_from(
    relations: &[Relation],
    files: &[FileParseData],
    caller: &Entity,
) -> Vec<(String, f32, Option<String>)> {
    let names: HashMap<EntityId, &str> = files
        .iter()
        .flat_map(|file| file.entities.iter())
        .map(|entity| (entity.id, entity.name.as_str()))
        .collect();
    let mut out: Vec<_> = relations
        .iter()
        .filter(|relation| {
            relation.kind == RelationKind::Calls && relation.src == GraphNodeId::Entity(caller.id)
        })
        .filter_map(|relation| {
            let target = names.get(&relation.dst.as_entity()?)?;
            Some((
                target.to_string(),
                relation.confidence,
                crate::receiver_origin::receiver_origin_chain(relation).map(str::to_string),
            ))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn flask_files() -> Vec<FileParseData> {
    vec![
        fixture(
            "src/flask/ctx.py",
            "import typing_extensions as te\n\n\
             class _AppCtxGlobals:\n    def pop(self, name, default=None):\n        return None\n\n\
             class AppContext:\n    def push(self) -> None:\n        pass\n\n    \
             def pop(self, exc=None) -> None:\n        pass\n\n    \
             def __enter__(self) -> te.Self:\n        self.push()\n        return self\n",
        ),
        fixture(
            "src/flask/app.py",
            "from .ctx import AppContext\n\n\
             class Flask:\n    def test_request_context(self, *args, **kwargs) -> AppContext:\n        return AppContext()\n\n    \
             def app_context(self) -> AppContext:\n        return AppContext()\n",
        ),
        fixture("src/flask/__init__.py", "from .app import Flask as Flask\n"),
        fixture(
            "tests/conftest.py",
            "import pytest\nfrom flask import Flask\n\n\
             @pytest.fixture\ndef app():\n    app = Flask('flask_test')\n    return app\n\n\
             @pytest.fixture\ndef req_ctx(app):\n    with app.test_request_context() as ctx:\n        yield ctx\n",
        ),
        fixture(
            "tests/test_reqctx.py",
            "import flask\n\n\
             def test_teardown_on_pop(app):\n    ctx = app.test_request_context()\n    ctx.push()\n    ctx.pop()\n\n\
             def test_uses_req_ctx(req_ctx):\n    req_ctx.pop()\n\n\
             def helper(app):\n    ctx = app.test_request_context()\n    ctx.pop()\n\n\
             def test_dict_pop(app):\n    values = {}\n    values.pop('x')\n\n\
             class TestContext:\n    def test_method(self, app):\n        ctx = app.app_context()\n        ctx.pop()\n",
        ),
    ]
}

#[test]
fn a_pytest_fixture_chain_binds_the_test_to_the_declared_class_method() {
    let files = flask_files();
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        let test = entity(&files, "tests/test_reqctx.py", "test_teardown_on_pop");
        let calls = calls_from(&relations, &files, test);
        let pop = calls
            .iter()
            .find(|(name, _, _)| name == "AppContext.pop")
            .unwrap_or_else(|| panic!("{label}: no AppContext.pop edge in {calls:?}"));
        assert_eq!(pop.1, RECEIVER_ORIGIN_CONFIDENCE, "{label}");
        let chain = pop.2.as_deref().expect("the edge names its chain");
        assert!(
            chain.contains("pytest_fixture app@tests/conftest.py")
                && chain.contains("Flask.test_request_context() -> AppContext"),
            "{label}: {chain}"
        );
        assert!(
            calls
                .iter()
                .any(|(name, confidence, _)| name == "AppContext.push"
                    && *confidence == RECEIVER_ORIGIN_CONFIDENCE),
            "{label}: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|(name, _, _)| name == "_AppCtxGlobals.pop"),
            "{label}: another class's pop must gain no caller: {calls:?}"
        );
        assert_eq!(
            RelationResolution::from_confidence(pop.1),
            RelationResolution::TypeResolved
        );
    }
}

#[test]
fn a_generator_fixture_entered_through_a_context_manager_types_its_yield() {
    let files = flask_files();
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        let test = entity(&files, "tests/test_reqctx.py", "test_uses_req_ctx");
        let calls = calls_from(&relations, &files, test);
        assert!(
            calls
                .iter()
                .any(|(name, confidence, _)| name == "AppContext.pop"
                    && *confidence == RECEIVER_ORIGIN_CONFIDENCE),
            "{label}: {calls:?}"
        );
        let method = entity(&files, "tests/test_reqctx.py", "TestContext.test_method");
        let calls = calls_from(&relations, &files, method);
        assert!(
            calls
                .iter()
                .any(|(name, confidence, _)| name == "AppContext.pop"
                    && *confidence == RECEIVER_ORIGIN_CONFIDENCE),
            "{label}: a class-based test is injected too: {calls:?}"
        );
    }
}

#[test]
fn an_untyped_receiver_settled_by_a_unique_method_name_is_only_a_candidate() {
    let files = flask_files();
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        let helper = entity(&files, "tests/test_reqctx.py", "helper");
        let calls = calls_from(&relations, &files, helper);
        let pop = calls
            .iter()
            .find(|(name, _, _)| name == "AppContext.pop")
            .unwrap_or_else(|| panic!("{label}: {calls:?}"));
        assert_eq!(pop.1, RECEIVER_ORIGIN_NAME_CONFIDENCE, "{label}");
        assert_eq!(
            RelationResolution::from_confidence(pop.1),
            RelationResolution::NameOnly
        );
        let dict = entity(&files, "tests/test_reqctx.py", "test_dict_pop");
        let calls = calls_from(&relations, &files, dict);
        assert!(
            !calls.iter().any(|(_, _, chain)| chain.is_some()),
            "{label}: a dict receiver has no origin: {calls:?}"
        );
    }
}

#[test]
fn a_nearer_conftest_shadows_the_farther_fixture() {
    let mut files = flask_files();
    files.push(fixture(
        "src/other.py",
        "class Other:\n    def pop(self) -> None:\n        pass\n",
    ));
    files.push(fixture(
        "tests/sub/conftest.py",
        "import pytest\nfrom other import Other\n\n@pytest.fixture\ndef app():\n    return Other()\n",
    ));
    files.push(fixture(
        "tests/sub/test_sub.py",
        "def test_shadowed(app):\n    app.pop()\n",
    ));
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        let test = entity(&files, "tests/sub/test_sub.py", "test_shadowed");
        let calls = calls_from(&relations, &files, test);
        assert!(
            calls.iter().any(|(name, confidence, _)| name == "Other.pop"
                && *confidence == RECEIVER_ORIGIN_CONFIDENCE),
            "{label}: {calls:?}"
        );
        assert!(
            !calls.iter().any(|(name, _, _)| name == "AppContext.pop"),
            "{label}: {calls:?}"
        );
    }
}

#[test]
fn a_parametrize_hook_on_the_lookup_path_withdraws_the_fixture_link() {
    let mut files = flask_files();
    files.push(fixture(
        "tests/hooked/conftest.py",
        "def pytest_generate_tests(metafunc):\n    metafunc.parametrize('app', [1])\n",
    ));
    files.push(fixture(
        "tests/hooked/test_hooked.py",
        "def test_hooked(app):\n    ctx = app.app_context()\n    ctx.push()\n",
    ));
    for (label, relations) in [
        ("batch", link_batch(&files)),
        ("incremental", link_incremental(&files)),
    ] {
        let test = entity(&files, "tests/hooked/test_hooked.py", "test_hooked");
        let calls = calls_from(&relations, &files, test);
        assert!(
            !calls
                .iter()
                .any(|(_, confidence, _)| *confidence == RECEIVER_ORIGIN_CONFIDENCE),
            "{label}: a hook that can replace the value proves nothing: {calls:?}"
        );
    }
}

#[test]
fn a_step_without_the_providers_file_binds_nothing_it_cannot_read() {
    // The incremental linker without a graph sees only the step's files, so
    // a fixture declared in a file outside the step is unreadable and the
    // walk stops rather than skipping to a farther provider.
    let files = flask_files();
    let ids = artifact_ids(&files);
    let mut linker = IncrementalLinker::new();
    for file in &files {
        linker.add_file(&file.file_path, ids[&file.file_path], &file.entities);
    }
    let step: Vec<FileParseData> = files
        .iter()
        .filter(|file| file.file_path == "tests/test_reqctx.py")
        .cloned()
        .collect();
    let relations = super::link_cross_file_incremental(&step, &linker).unwrap();
    let test = entity(&files, "tests/test_reqctx.py", "test_teardown_on_pop");
    let calls = calls_from(&relations, &files, test);
    assert!(
        !calls
            .iter()
            .any(|(_, confidence, _)| *confidence == RECEIVER_ORIGIN_CONFIDENCE),
        "{calls:?}"
    );
}

#[test]
fn an_aliased_import_names_the_alias_as_a_call_name_of_the_focal() {
    let files = vec![
        fixture(
            "src/events.js",
            "export function setEnabled(enabled) {\n  return enabled;\n}\n",
        ),
        fixture(
            "src/config.js",
            "import {setEnabled as ReactBrowserEventEmitterSetEnabled} from './events';\n\n\
             export function prepareForCommit() {\n  ReactBrowserEventEmitterSetEnabled(false);\n}\n",
        ),
    ];
    let relations = link_batch(&files);
    let focal = entity(&files, "src/events.js", "setEnabled");
    assert_eq!(
        super::import_alias_call_names(focal, &relations),
        vec!["ReactBrowserEventEmitterSetEnabled".to_string()]
    );
    // The aliased call itself binds to the focal through the import.
    let caller = entity(&files, "src/config.js", "prepareForCommit");
    let calls = calls_from(&relations, &files, caller);
    assert!(
        calls
            .iter()
            .any(|(name, confidence, _)| name == "setEnabled"
                && RelationResolution::from_confidence(*confidence)
                    == RelationResolution::TypeResolved),
        "{calls:?}"
    );
}
