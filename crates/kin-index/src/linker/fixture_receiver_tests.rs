// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Calls through receivers pytest fixtures supply, over a Flask-shaped
//! repository, batch and incremental: the callers the fixture chain proves,
//! and the decoys and shadows it must not bind.
//!
//! The shapes come from Flask's own tree. `examples/tutorial` is a project
//! nested in the repository whose `app` fixture yields what `create_app()`
//! returns and whose `runner` fixture returns `app.test_cli_runner()`;
//! `tests/test_cli.py` has a module `runner` fixture that is click's own
//! `CliRunner`; `tests/test_json.py` calls `app.json.dumps(...)`, where
//! `App.__init__` declares `self.json: JSONProvider`.

use super::*;
use crate::coverage_certificate::{CertificateDetail, ReceiverOriginGapEntry};
use crate::receiver_origin::{receiver_origin_steps, OriginStepKind};
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

fn completeness(files: &[FileParseData]) -> FileParseCompletenessMap {
    files
        .iter()
        .map(|file| (file.file_path.clone(), ParseCompleteness::Full))
        .collect()
}

struct Linked {
    label: &'static str,
    ids: ArtifactIdentityMap,
    relations: Vec<Relation>,
}

fn both(files: &[FileParseData]) -> [Linked; 2] {
    let ids: ArtifactIdentityMap = files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect();
    let batch = link_cross_file_with_completeness(files, &ids, &completeness(files)).unwrap();
    let mut linker = IncrementalLinker::new();
    for file in files {
        linker.add_file(&file.file_path, ids[&file.file_path], &file.entities);
    }
    let incremental =
        link_cross_file_incremental_with_completeness(files, &linker, &completeness(files))
            .unwrap();
    [
        Linked {
            label: "batch",
            ids: ids.clone(),
            relations: batch,
        },
        Linked {
            label: "incremental",
            ids,
            relations: incremental,
        },
    ]
}

fn entity<'a>(files: &'a [FileParseData], file: &str, name: &str) -> &'a Entity {
    files
        .iter()
        .find(|parsed| parsed.file_path == file)
        .and_then(|parsed| parsed.entities.iter().find(|entity| entity.name == name))
        .unwrap_or_else(|| panic!("no entity {name} in {file}"))
}

/// Every call edge from `caller`, as (target name, confidence, whether a
/// receiver chain bound it).
fn calls_from(
    linked: &Linked,
    files: &[FileParseData],
    caller: &Entity,
) -> Vec<(String, f32, bool)> {
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
            Some((
                target.to_string(),
                relation.confidence,
                receiver_origin_steps(relation).is_some(),
            ))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Whether `caller` has a proven receiver-chain edge to `target`.
fn proves(linked: &Linked, files: &[FileParseData], caller: &Entity, target: &str) -> bool {
    calls_from(linked, files, caller)
        .iter()
        .any(|(name, confidence, chained)| {
            name == target && *confidence == RECEIVER_ORIGIN_CONFIDENCE && *chained
        })
}

/// Whether `caller` has any edge to `target` a count of proven callers
/// would include. A name-only candidate a bare-name tier leaves is not one.
fn reaches(linked: &Linked, files: &[FileParseData], caller: &Entity, target: &str) -> bool {
    calls_from(linked, files, caller)
        .iter()
        .any(|(name, confidence, _)| {
            name == target
                && crate::resolution::RelationResolution::from_confidence(*confidence).is_proven()
        })
}

/// The certificate's receiver-origin gap entries for `file`.
fn gaps(linked: &Linked, file: &str) -> Vec<ReceiverOriginGapEntry> {
    let artifact = GraphNodeId::Artifact(linked.ids[file]);
    linked
        .relations
        .iter()
        .filter(|relation| relation.src == artifact && relation.dst == artifact)
        .find_map(CertificateDetail::from_certificate)
        .map(|detail| detail.receiver_origin_gaps)
        .unwrap_or_default()
}

/// The certificate's receiver-outside entries for `file`, as (method, chain).
fn outside(linked: &Linked, file: &str) -> Vec<(String, String)> {
    let artifact = GraphNodeId::Artifact(linked.ids[file]);
    linked
        .relations
        .iter()
        .filter(|relation| relation.src == artifact && relation.dst == artifact)
        .find_map(CertificateDetail::from_certificate)
        .map(|detail| {
            detail
                .receiver_outside
                .into_iter()
                .map(|entry| (entry.method, entry.chain))
                .collect()
        })
        .unwrap_or_default()
}

/// The gap a call of `method` through `receiver` in `caller` recorded.
fn gap_for(
    linked: &Linked,
    file: &str,
    caller: &Entity,
    receiver: &str,
    method: &str,
) -> ReceiverOriginGapEntry {
    let gaps = gaps(linked, file);
    gaps.iter()
        .find(|gap| {
            gap.receiver == receiver
                && gap.method == method
                && gap.steps.first().and_then(|step| step.entity) == Some(caller.id)
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "{}: no gap for {receiver}.{method} in {}: {gaps:?}",
                linked.label, caller.name
            )
        })
}

const FLASK_INIT: &str = "from .app import Flask as Flask\n";

const FLASK_TESTING: &str = r#"from click.testing import CliRunner


class FlaskCliRunner(CliRunner):
    def __init__(self, app, **kwargs):
        self.app = app
        super().__init__(**kwargs)

    def invoke(self, cli=None, args=None, **kwargs):
        return super().invoke(cli, args, **kwargs)
"#;

const FLASK_JSON_PROVIDER: &str = r#"class JSONProvider:
    def dumps(self, obj, **kwargs) -> str:
        raise NotImplementedError


class DefaultJSONProvider(JSONProvider):
    def dumps(self, obj, **kwargs) -> str:
        return ""
"#;

const FLASK_SANSIO_APP: &str = r#"from ..json.provider import DefaultJSONProvider
from ..json.provider import JSONProvider


class App:
    json_provider_class: type[JSONProvider] = DefaultJSONProvider

    def __init__(self, import_name):
        self.import_name = import_name
        self.json: JSONProvider = self.json_provider_class(self)
"#;

const FLASK_APP: &str = r#"import typing as t

from .sansio.app import App

if t.TYPE_CHECKING:
    from .testing import FlaskCliRunner


class Flask(App):
    def test_cli_runner(self, **kwargs) -> FlaskCliRunner:
        from .testing import FlaskCliRunner as cls

        return cls(self, **kwargs)
"#;

const TESTS_CONFTEST: &str = r#"import pytest

from flask import Flask

pytest_plugins = ["tests.plugin_fixtures"]


@pytest.fixture
def app():
    app = Flask("flask_test")
    return app
"#;

const TESTS_PLUGIN_FIXTURES: &str = r#"import pytest


@pytest.fixture
def plugged_runner(app):
    return app.test_cli_runner()
"#;

const TESTS_JSON: &str = r#"import pytest


@pytest.mark.parametrize("test_value,expected", [(True, "a"), (False, "b")])
def test_json_as_unicode(test_value, expected, app):
    app.json.ensure_ascii = test_value
    rv = app.json.dumps("x")
    assert rv == expected


def test_json_dump_dataclass(app):
    value = app.json.dumps({})
    assert value
"#;

const TESTS_CLI: &str = r#"import pytest
from click.testing import CliRunner

from flask import Flask


@pytest.fixture
def runner():
    return CliRunner()


class TestRoutes:
    def test_host(self, runner):
        app = Flask(__name__)
        result = runner.invoke(app.cli, ["routes"])
        assert result.exit_code == 0
"#;

const TESTS_CLI_EMPTY: &str = r#"def test_cli_empty(app):
    result = app.test_cli_runner().invoke(args=["blue", "--help"])
    assert result.exit_code == 2


def test_cli_missing_method(app):
    app.missing_runner().invoke(args=[])
"#;

const TESTS_PLUGIN: &str = r#"def test_plugged(plugged_runner):
    plugged_runner.invoke(args=["hello"])
"#;

const TESTS_VIEWS: &str = r#"def test_init_once(app):
    n = 0

    class CountInit:
        def __init__(self):
            nonlocal n
            n += 1

    app.test_cli_runner()
"#;

const TESTS_CLASSES: &str = r#"import pytest


class Other:
    def invoke(self, args=None):
        return None


class TestBase:
    @pytest.fixture
    def app(self):
        return Other()


class TestChild(TestBase):
    def test_child(self, app):
        app.test_cli_runner()


class TestOverride:
    @pytest.fixture
    def app(self, app):
        return app

    def test_override(self, app):
        app.test_cli_runner()


@pytest.fixture(params=[1, 2])
def param_app(request):
    return request.param


def test_param_app(param_app):
    param_app.test_cli_runner()
"#;

const TUTORIAL_FLASKR: &str = r#"from flask import Flask


def create_app(test_config=None):
    app = Flask(__name__, instance_relative_config=True)
    if test_config is None:
        app.config.from_pyfile("config.py", silent=True)
    else:
        app.config.update(test_config)

    @app.route("/hello")
    def hello():
        return "Hello, World!"

    return app
"#;

const TUTORIAL_CONFTEST: &str = r#"import pytest

from flaskr import create_app

from .shared import cli_runner


@pytest.fixture
def app():
    app = create_app({"TESTING": True})
    yield app


@pytest.fixture
def runner(app):
    return app.test_cli_runner()


@pytest.fixture
def wrapped(app):
    return app
"#;

const TUTORIAL_SHARED: &str = r#"import pytest


@pytest.fixture
def cli_runner(app):
    return app.test_cli_runner()
"#;

const TUTORIAL_TEST_DB: &str = r#"def test_init_db_command(runner, monkeypatch):
    result = runner.invoke(args=["init-db"])
    assert "Initialized" in result.output


def test_imported_runner(cli_runner):
    cli_runner.invoke(args=["init-db"])
"#;

const TUTORIAL_SUB_CONFTEST: &str = r#"import pytest


class Other:
    def invoke(self, args=None):
        return None


@pytest.fixture
def app():
    return Other()
"#;

const TUTORIAL_SUB_TEST: &str = r#"def test_shadowed_runner(runner):
    runner.invoke(args=["init-db"])
"#;

const TUTORIAL_TEST_MODULE_APP: &str = r#"import pytest


class Other:
    def invoke(self, args=None):
        return None


@pytest.fixture
def app():
    return Other()


def test_module_app_runner(runner):
    runner.invoke(args=["init-db"])
"#;

const TUTORIAL_TEST_PARAM: &str = r#"import pytest


@pytest.mark.parametrize("app", [object()])
def test_parametrized_app(runner):
    runner.invoke(args=["init-db"])


@pytest.mark.parametrize("runner", [object()])
def test_parametrized_runner(runner):
    runner.invoke(args=["init-db"])


@pytest.mark.parametrize("app", [object()])
def test_parametrized_wrapped(wrapped):
    wrapped.test_cli_runner()


def test_wrapped(wrapped):
    wrapped.test_cli_runner()
"#;

const EXT_CONFTEST: &str = "pytest_plugins = [\"pytest_mock\"]\n";

const EXT_TEST: &str = r#"def test_mocker(mocker):
    mocker.patch("x")


def test_app_beside_an_external_plugin(app):
    app.test_cli_runner()
"#;

fn flask_repository() -> Vec<FileParseData> {
    vec![
        fixture("src/flask/__init__.py", FLASK_INIT),
        fixture("src/flask/testing.py", FLASK_TESTING),
        fixture("src/flask/json/__init__.py", ""),
        fixture("src/flask/json/provider.py", FLASK_JSON_PROVIDER),
        fixture("src/flask/sansio/app.py", FLASK_SANSIO_APP),
        fixture("src/flask/app.py", FLASK_APP),
        fixture("tests/conftest.py", TESTS_CONFTEST),
        fixture("tests/plugin_fixtures.py", TESTS_PLUGIN_FIXTURES),
        fixture("tests/test_json.py", TESTS_JSON),
        fixture("tests/test_cli.py", TESTS_CLI),
        fixture("tests/test_plugin.py", TESTS_PLUGIN),
        fixture("tests/test_cli_empty.py", TESTS_CLI_EMPTY),
        fixture("tests/test_views.py", TESTS_VIEWS),
        fixture("tests/test_classes.py", TESTS_CLASSES),
        fixture("tests/ext/conftest.py", EXT_CONFTEST),
        fixture("tests/ext/test_ext.py", EXT_TEST),
        fixture("examples/tutorial/flaskr/__init__.py", TUTORIAL_FLASKR),
        fixture("examples/tutorial/tests/conftest.py", TUTORIAL_CONFTEST),
        fixture("examples/tutorial/tests/shared.py", TUTORIAL_SHARED),
        fixture("examples/tutorial/tests/test_db.py", TUTORIAL_TEST_DB),
        fixture(
            "examples/tutorial/tests/test_module_app.py",
            TUTORIAL_TEST_MODULE_APP,
        ),
        fixture("examples/tutorial/tests/test_param.py", TUTORIAL_TEST_PARAM),
        fixture(
            "examples/tutorial/tests/sub/conftest.py",
            TUTORIAL_SUB_CONFTEST,
        ),
        fixture("examples/tutorial/tests/sub/test_sub.py", TUTORIAL_SUB_TEST),
    ]
}

/// `runner` in the tutorial's `test_init_db_command` is the conftest
/// fixture returning `app.test_cli_runner()`, where `app` yields what the
/// nested project's `create_app()` returns: a `Flask`. The call is a proven
/// caller of `FlaskCliRunner.invoke`, and so is the same call through a
/// fixture the conftest imports from another module.
#[test]
fn a_tutorial_runner_fixture_proves_its_test_calls_the_flask_cli_runner() {
    let files = flask_repository();
    let test = entity(
        &files,
        "examples/tutorial/tests/test_db.py",
        "test_init_db_command",
    );
    let imported = entity(
        &files,
        "examples/tutorial/tests/test_db.py",
        "test_imported_runner",
    );
    for linked in both(&files) {
        let label = linked.label;
        assert!(
            proves(&linked, &files, test, "FlaskCliRunner.invoke"),
            "{label}: {:?}",
            calls_from(&linked, &files, test)
        );
        assert!(
            proves(&linked, &files, imported, "FlaskCliRunner.invoke"),
            "{label}: {:?}",
            calls_from(&linked, &files, imported)
        );
    }
}

/// The chain the proven edge carries names every link: the binding, both
/// fixtures, the nested project's function, the class it builds, the method
/// whose declared return types the runner, and the method reached.
#[test]
fn the_tutorial_runner_edge_names_its_chain() {
    let files = flask_repository();
    let test = entity(
        &files,
        "examples/tutorial/tests/test_db.py",
        "test_init_db_command",
    );
    for linked in both(&files) {
        let label = linked.label;
        let relation = linked
            .relations
            .iter()
            .find(|relation| {
                relation.kind == RelationKind::Calls
                    && relation.src == GraphNodeId::Entity(test.id)
                    && crate::receiver_origin::is_receiver_origin_edge(relation)
            })
            .unwrap_or_else(|| panic!("{label}: no receiver-origin edge"));
        let chain = crate::receiver_origin::receiver_origin_chain(relation).unwrap();
        for link in [
            "pytest_fixture runner@examples/tutorial/tests/conftest.py",
            "pytest_fixture app@examples/tutorial/tests/conftest.py",
            "create_app() returns",
            "Flask(...)",
            "Flask.test_cli_runner() -> FlaskCliRunner",
            "FlaskCliRunner.invoke",
        ] {
            assert!(chain.contains(link), "{label}: {link} missing in {chain}");
        }
        let steps: Vec<OriginStepKind> = receiver_origin_steps(relation)
            .unwrap()
            .iter()
            .map(|step| step.step)
            .collect();
        assert_eq!(steps.first(), Some(&OriginStepKind::Binding), "{label}");
        assert_eq!(steps.last(), Some(&OriginStepKind::Method), "{label}");
    }
}

/// `app.json.dumps(...)` reads `json` off a `Flask` whose base declares
/// `self.json: JSONProvider`, so the call is a proven caller of
/// `JSONProvider.dumps`, the method the declaration types. It names no
/// override directly: which provider runs is the instance's business.
#[test]
fn an_attribute_a_base_declares_types_the_json_provider_call() {
    let files = flask_repository();
    for name in ["test_json_as_unicode", "test_json_dump_dataclass"] {
        let test = entity(&files, "tests/test_json.py", name);
        for linked in both(&files) {
            let label = linked.label;
            assert!(
                proves(&linked, &files, test, "JSONProvider.dumps"),
                "{label}: {name}: {:?}",
                calls_from(&linked, &files, test)
            );
            assert!(
                !reaches(&linked, &files, test, "DefaultJSONProvider.dumps"),
                "{label}: {name}: {:?}",
                calls_from(&linked, &files, test)
            );
        }
    }
}

/// The decoy: `TestRoutes.test_host` takes the module's own `runner`
/// fixture, click's `CliRunner`. No file of the repository can supply
/// `click`, and `FlaskCliRunner` derives from the same import, so the name
/// is a class and the fixture's value is an instance of exactly it. The call
/// is proved to reach no declaration of the repository, `FlaskCliRunner.invoke`
/// included, though that class derives from `CliRunner` and declares `invoke`.
#[test]
fn a_click_cli_runner_fixture_is_proved_outside_the_repository() {
    let files = flask_repository();
    let test = entity(&files, "tests/test_cli.py", "TestRoutes.test_host");
    for linked in both(&files) {
        let label = linked.label;
        assert!(
            calls_from(&linked, &files, test)
                .iter()
                .all(|(target, _, _)| target != "FlaskCliRunner.invoke"),
            "{label}: not even a name-only candidate: {:?}",
            calls_from(&linked, &files, test)
        );
        assert_eq!(
            outside(&linked, "tests/test_cli.py"),
            vec![(
                "invoke".to_string(),
                "runner: pytest_fixture runner@tests/test_cli.py -> CliRunner(...) -> \
                 CliRunner.invoke is outside the repository"
                    .to_string()
            )],
            "{label}"
        );
        assert!(
            gaps(&linked, "tests/test_cli.py")
                .iter()
                .all(|gap| gap.method != "invoke"),
            "{label}: a call proved outside is not also a gap"
        );
    }
}

/// `tests/test_cli.py` also has a fixture that hands the click runner's
/// `invoke` on as a value, `partial(runner.invoke, cli)`. That read is no
/// call, so no relation carries it; the file's value-escape record does, by
/// its line, and the certificate records the proof for that line.
const TESTS_CLI_PARTIAL: &str = r#"from functools import partial

import pytest
from click.testing import CliRunner


@pytest.fixture
def runner():
    return CliRunner()


class TestRoutes:
    @pytest.fixture
    def invoke(self, app, runner):
        cli = app.cli
        return partial(runner.invoke, cli)

    def test_simple(self, invoke):
        assert invoke(["routes"]).exit_code == 0
"#;

/// The certificate's entries for member reads proved outside, as (line
/// counted from zero, member, receiver, owner).
fn outside_reads(linked: &Linked, file: &str) -> Vec<(u32, String, String, String)> {
    let artifact = GraphNodeId::Artifact(linked.ids[file]);
    linked
        .relations
        .iter()
        .filter(|relation| relation.src == artifact && relation.dst == artifact)
        .find_map(CertificateDetail::from_certificate)
        .map(|detail| {
            detail
                .receiver_outside_reads
                .into_iter()
                .map(|read| (read.line, read.member, read.receiver, read.owner))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_member_of_the_click_runner_passed_as_a_value_is_proved_outside() {
    let mut files = flask_repository();
    let cli = files
        .iter_mut()
        .find(|file| file.file_path == "tests/test_cli.py")
        .unwrap();
    *cli = fixture("tests/test_cli.py", TESTS_CLI_PARTIAL);
    let line = TESTS_CLI_PARTIAL
        .lines()
        .position(|line| line.contains("partial(runner.invoke"))
        .unwrap() as u32;
    for linked in both(&files) {
        assert_eq!(
            outside_reads(&linked, "tests/test_cli.py"),
            vec![(
                line,
                "invoke".to_string(),
                "runner".to_string(),
                "click.testing.CliRunner".to_string()
            )],
            "{}",
            linked.label
        );
    }
    // Handed to other code, the repository's subclass could be what the
    // library constructs, and the read stays unproven.
    let handed = format!("{FLASK_TESTING}\n\nimport plugins\n\nplugins.register(FlaskCliRunner)\n");
    let testing = files
        .iter_mut()
        .find(|file| file.file_path == "src/flask/testing.py")
        .unwrap();
    *testing = fixture("src/flask/testing.py", &handed);
    for linked in both(&files) {
        assert_eq!(
            outside_reads(&linked, "tests/test_cli.py"),
            Vec::new(),
            "{}",
            linked.label
        );
    }
}

/// The same repository, where the module that declares `FlaskCliRunner` also
/// hands the class to other code. A library that was handed a subclass can
/// make instances of it where the source constructs the base, so the call
/// stays unproven and its gap names the constructor.
#[test]
fn a_subclass_handed_to_other_code_keeps_the_library_constructor_unproven() {
    let handed = format!("{FLASK_TESTING}\n\nimport plugins\n\nplugins.register(FlaskCliRunner)\n");
    let mut files = flask_repository();
    let testing = files
        .iter_mut()
        .find(|file| file.file_path == "src/flask/testing.py")
        .unwrap();
    *testing = fixture("src/flask/testing.py", &handed);
    let test = entity(&files, "tests/test_cli.py", "TestRoutes.test_host");
    for linked in both(&files) {
        let label = linked.label;
        assert_eq!(
            outside(&linked, "tests/test_cli.py"),
            Vec::<(String, String)>::new(),
            "{label}"
        );
        let gap = gap_for(&linked, "tests/test_cli.py", test, "runner", "invoke");
        assert_eq!(gap.missing.step, OriginStepKind::Call, "{label}");
        assert_eq!(gap.missing.name, "CliRunner", "{label}");
        assert_eq!(gap.missing.reason, "callee_not_located", "{label}");
    }
}

/// `tests/test_testing.py`: a test derives its own runner class where it
/// uses it, and gives it to the application, never to click.
const TESTS_TESTING: &str = r#"from flask.testing import FlaskCliRunner


def test_cli_runner_class(app):
    runner = app.test_cli_runner()
    assert isinstance(runner, FlaskCliRunner)

    class SubRunner(FlaskCliRunner):
        pass

    app.test_cli_runner_class = SubRunner
    runner = app.test_cli_runner()
    assert isinstance(runner, SubRunner)
"#;

/// A class a test declares in its own body is one more class of the
/// hierarchy the proof has to hear from. Read, and handed to no call, it
/// leaves the proof standing; passed to a call, it withdraws it.
#[test]
fn a_runner_class_a_test_declares_is_read_like_any_other() {
    let mut files = flask_repository();
    files.push(fixture("tests/test_testing.py", TESTS_TESTING));
    for linked in both(&files) {
        let label = linked.label;
        assert_eq!(
            outside(&linked, "tests/test_cli.py").len(),
            1,
            "{label}: {:?}",
            gaps(&linked, "tests/test_cli.py")
        );
    }
    let registered = TESTS_TESTING.replace(
        "    app.test_cli_runner_class = SubRunner\n",
        "    plugins.register(SubRunner)\n",
    );
    assert_ne!(registered, TESTS_TESTING);
    let mut files = flask_repository();
    files.push(fixture("tests/test_testing.py", &registered));
    for linked in both(&files) {
        assert_eq!(
            outside(&linked, "tests/test_cli.py"),
            Vec::<(String, String)>::new(),
            "{}",
            linked.label
        );
    }
}

const CLIENT_INIT: &str = "from .app import Flask as Flask\n";

/// `src/flask/sansio/scaffold.py`: the route decorator that shares its name
/// with the test client's request method.
const CLIENT_SCAFFOLD: &str = r#"class Scaffold:
    def get(self, rule):
        return rule
"#;

/// `src/flask/testing.py`: `FlaskClient` declares `open` and takes `get`
/// from werkzeug's `Client`.
const CLIENT_TESTING: &str = r#"from werkzeug.test import Client


class FlaskClient(Client):
    preserve_context = False

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.environ_base = {}

    def open(self, *args, **kwargs):
        return super().open(*args, **kwargs)
"#;

const CLIENT_APP: &str = r#"from .scaffold import Scaffold
from .testing import FlaskClient


class Flask(Scaffold):
    def test_client(self) -> FlaskClient:
        return FlaskClient(self)
"#;

const CLIENT_CONFTEST: &str = r#"import pytest

from flask import Flask


@pytest.fixture
def app():
    app = Flask("flask_test")
    return app


@pytest.fixture
def client(app):
    return app.test_client()
"#;

const CLIENT_TEST: &str = r#"def test_index(client):
    rv = client.get("/")
    again = client.open("/")
    assert rv


def test_missing(app):
    app.missing()
"#;

/// A client class of the repository that binds `get` by assignment.
const CLIENT_RECORDING: &str = r#"from .testing import FlaskClient


class RecordingClient(FlaskClient):
    get = FlaskClient.open
"#;

fn client_repository(extra: &[(&str, &str)]) -> Vec<FileParseData> {
    let mut files = vec![
        fixture("src/flask/__init__.py", CLIENT_INIT),
        fixture("src/flask/scaffold.py", CLIENT_SCAFFOLD),
        fixture("src/flask/testing.py", CLIENT_TESTING),
        fixture("src/flask/app.py", CLIENT_APP),
        fixture("tests/conftest.py", CLIENT_CONFTEST),
        fixture("tests/test_basic.py", CLIENT_TEST),
    ];
    files.extend(extra.iter().map(|(path, source)| fixture(path, source)));
    files
}

/// `client.get("/")` in a Flask test: the fixture chain types `client` as
/// `FlaskClient`, no class of the repository under it binds `get`, and its
/// one base outside the repository is werkzeug's `Client`. The call reaches
/// no declaration of the repository, and never `Scaffold.get`, the route
/// decorator of the same name. `client.open` beside it is the repository's.
#[test]
fn a_method_a_client_inherits_from_a_library_is_proved_outside() {
    let files = client_repository(&[]);
    let test = entity(&files, "tests/test_basic.py", "test_index");
    for linked in both(&files) {
        let label = linked.label;
        assert!(
            proves(&linked, &files, test, "FlaskClient.open"),
            "{label}: {:?}",
            calls_from(&linked, &files, test)
        );
        assert!(
            calls_from(&linked, &files, test)
                .iter()
                .all(|(target, _, _)| target != "Scaffold.get"),
            "{label}: not even a name-only candidate: {:?}",
            calls_from(&linked, &files, test)
        );
        assert_eq!(
            outside(&linked, "tests/test_basic.py"),
            vec![(
                "get".to_string(),
                "client: pytest_fixture client@tests/conftest.py -> \
                 pytest_fixture app@tests/conftest.py -> Flask(...) -> \
                 Flask.test_client() -> FlaskClient -> \
                 FlaskClient.get is outside the repository, inherited from Client"
                    .to_string()
            )],
            "{label}"
        );
    }
}

/// The facts the proof reads are on the class entity the pipeline indexed,
/// not only in what the adapter computes.
#[test]
fn an_indexed_class_entity_carries_what_the_class_says_about_itself() {
    let files = client_repository(&[]);
    let class = entity(&files, "src/flask/testing.py", "FlaskClient");
    let facts = kin_parser::value_origins::read_value_origins(class).expect("the class has facts");
    let bound = facts.bound_names.expect("the class says what it binds");
    assert!(bound.contains("open") && bound.contains("environ_base"));
    assert!(!bound.contains("get"));
    assert_eq!(
        facts.base_origins.get("Client"),
        Some(&Some("werkzeug.test.Client".to_string()))
    );
    assert!(!facts.handed_over);
}

/// A name no class of a hierarchy holds, on a class the repository declares
/// whole, is not a library's: `app.missing()` stays a gap.
#[test]
fn a_method_missing_from_a_class_with_no_library_base_stays_a_gap() {
    let files = client_repository(&[]);
    let test = entity(&files, "tests/test_basic.py", "test_missing");
    for linked in both(&files) {
        let label = linked.label;
        assert!(
            outside(&linked, "tests/test_basic.py")
                .iter()
                .all(|(method, _)| method != "missing"),
            "{label}"
        );
        let gap = gap_for(&linked, "tests/test_basic.py", test, "app", "missing");
        assert_eq!(gap.missing.reason, "method_not_found", "{label}");
    }
}

/// The value typed `FlaskClient` may be an instance of any class the
/// repository derives from it. One that binds `get`, by an assignment no
/// entity stands for, keeps the call unproven.
#[test]
fn a_derived_class_that_binds_the_name_keeps_the_call_unproven() {
    let files = client_repository(&[("src/flask/recording.py", CLIENT_RECORDING)]);
    let test = entity(&files, "tests/test_basic.py", "test_index");
    for linked in both(&files) {
        let label = linked.label;
        assert_eq!(
            outside(&linked, "tests/test_basic.py"),
            Vec::<(String, String)>::new(),
            "{label}"
        );
        let gap = gap_for(&linked, "tests/test_basic.py", test, "client", "get");
        assert_eq!(gap.missing.reason, "method_not_found", "{label}");
    }
}

/// A conftest nearer the test, or the test's own module, that provides
/// `app` decides the `app` the tutorial's `runner` fixture receives, since
/// pytest resolves a fixture's parameters for the test that requested it.
/// Here that `app` is no `Flask`, so neither call reaches the Flask runner.
#[test]
fn a_nearer_app_fixture_decides_what_the_runner_fixture_receives() {
    let files = flask_repository();
    for (file, name) in [
        (
            "examples/tutorial/tests/sub/test_sub.py",
            "test_shadowed_runner",
        ),
        (
            "examples/tutorial/tests/test_module_app.py",
            "test_module_app_runner",
        ),
    ] {
        let test = entity(&files, file, name);
        for linked in both(&files) {
            let label = linked.label;
            assert!(
                !reaches(&linked, &files, test, "FlaskCliRunner.invoke"),
                "{label}: {name}: {:?}",
                calls_from(&linked, &files, test)
            );
            let gap = gap_for(&linked, file, test, "runner", "invoke");
            assert_eq!(gap.missing.step, OriginStepKind::Method, "{label}");
            assert_eq!(gap.missing.name, "Other.test_cli_runner", "{label}");
            assert_eq!(gap.missing.reason, "method_not_found", "{label}");
        }
    }
}

/// A name a test parametrizes directly is the parameter's value for every
/// fixture of its request: `runner`'s own `app` when the test parametrizes
/// `app`, and `runner` itself when it parametrizes `runner`. Neither call is
/// a proven caller. Where no method name settles the call on its own, the
/// gap names the parametrized link; the same fixture unparametrized binds.
#[test]
fn a_parametrized_name_is_never_answered_by_a_fixture() {
    let files = flask_repository();
    let file = "examples/tutorial/tests/test_param.py";
    let app_param = entity(&files, file, "test_parametrized_app");
    let runner_param = entity(&files, file, "test_parametrized_runner");
    let wrapped_param = entity(&files, file, "test_parametrized_wrapped");
    let wrapped = entity(&files, file, "test_wrapped");
    for linked in both(&files) {
        let label = linked.label;
        for test in [app_param, runner_param] {
            assert!(
                !reaches(&linked, &files, test, "FlaskCliRunner.invoke"),
                "{label}: {}: {:?}",
                test.name,
                calls_from(&linked, &files, test)
            );
        }
        assert!(
            !reaches(&linked, &files, wrapped_param, "Flask.test_cli_runner"),
            "{label}: {:?}",
            calls_from(&linked, &files, wrapped_param)
        );
        let gap = gap_for(&linked, file, wrapped_param, "wrapped", "test_cli_runner");
        assert_eq!(gap.missing.step, OriginStepKind::Provider, "{label}");
        assert_eq!(gap.missing.name, "app", "{label}");
        assert_eq!(gap.missing.reason, "provider_withheld", "{label}");
        assert!(
            proves(&linked, &files, wrapped, "Flask.test_cli_runner"),
            "{label}: {:?}",
            calls_from(&linked, &files, wrapped)
        );
        assert!(
            gaps(&linked, file)
                .iter()
                .all(|gap| gap.steps.first().and_then(|step| step.entity) != Some(runner_param.id)),
            "{label}: a parametrized parameter carries no binding to walk"
        );
    }
}

/// A test class sees the fixtures its bases declare before the module's and
/// the conftest's, so `TestChild`'s `app` is `TestBase`'s `Other`. A fixture
/// that requests its own name receives the next one out, so `TestOverride`'s
/// `app` is the conftest's `Flask`. A fixture whose value is `request.param`
/// stays unbound and says so.
#[test]
fn class_fixtures_overrides_and_request_params() {
    let files = flask_repository();
    let file = "tests/test_classes.py";
    let child = entity(&files, file, "TestChild.test_child");
    let overriding = entity(&files, file, "TestOverride.test_override");
    let param = entity(&files, file, "test_param_app");
    for linked in both(&files) {
        let label = linked.label;
        assert!(
            !reaches(&linked, &files, child, "Flask.test_cli_runner"),
            "{label}: {:?}",
            calls_from(&linked, &files, child)
        );
        let gap = gap_for(&linked, file, child, "app", "test_cli_runner");
        assert_eq!(gap.missing.name, "Other.test_cli_runner", "{label}");
        assert!(
            proves(&linked, &files, overriding, "Flask.test_cli_runner"),
            "{label}: {:?}",
            calls_from(&linked, &files, overriding)
        );
        assert!(
            !reaches(&linked, &files, param, "Flask.test_cli_runner"),
            "{label}: {:?}",
            calls_from(&linked, &files, param)
        );
        let gap = gap_for(&linked, file, param, "param_app", "test_cli_runner");
        assert_eq!(gap.missing.step, OriginStepKind::Provider, "{label}");
        assert_eq!(gap.missing.reason, "provider_value_unknown", "{label}");
    }
}

/// A fixture a `pytest_plugins` module of the repository declares answers
/// below every conftest, and its own `app` is the test's. A plugin the
/// repository does not hold leaves a name it may provide open, without
/// hiding a farther conftest's fixture of another name.
#[test]
fn plugin_modules_provide_fixtures_below_the_conftest_walk() {
    let files = flask_repository();
    let plugged = entity(&files, "tests/test_plugin.py", "test_plugged");
    let mocker = entity(&files, "tests/ext/test_ext.py", "test_mocker");
    let beside = entity(
        &files,
        "tests/ext/test_ext.py",
        "test_app_beside_an_external_plugin",
    );
    for linked in both(&files) {
        let label = linked.label;
        assert!(
            proves(&linked, &files, plugged, "FlaskCliRunner.invoke"),
            "{label}: {:?}",
            calls_from(&linked, &files, plugged)
        );
        let gap = gap_for(&linked, "tests/ext/test_ext.py", mocker, "mocker", "patch");
        assert_eq!(gap.missing.reason, "provider_lookup_open", "{label}");
        assert!(
            proves(&linked, &files, beside, "Flask.test_cli_runner"),
            "{label}: {:?}",
            calls_from(&linked, &files, beside)
        );
    }
}

/// A `nonlocal` in a class nested in the test withdraws only the name it
/// declares, so the `app` fixture still binds.
#[test]
fn a_nonlocal_name_withdraws_only_itself() {
    let files = flask_repository();
    let test = entity(&files, "tests/test_views.py", "test_init_once");
    for linked in both(&files) {
        assert!(
            proves(&linked, &files, test, "Flask.test_cli_runner"),
            "{}: {:?}",
            linked.label,
            calls_from(&linked, &files, test)
        );
    }
}

/// `app.test_cli_runner().invoke(...)` calls a method on what a method of
/// the fixture's value returns, without binding it to a name: the call's
/// receiver is the expression, and it is a proven caller all the same. A
/// method the value's class does not hold says so.
#[test]
fn a_method_called_on_the_fixture_value_types_the_receiver_it_returns() {
    let files = flask_repository();
    let file = "tests/test_cli_empty.py";
    let empty = entity(&files, file, "test_cli_empty");
    let missing = entity(&files, file, "test_cli_missing_method");
    for linked in both(&files) {
        let label = linked.label;
        assert!(
            proves(&linked, &files, empty, "FlaskCliRunner.invoke"),
            "{label}: {:?}",
            calls_from(&linked, &files, empty)
        );
        let gap = gap_for(&linked, file, missing, "app.missing_runner()", "invoke");
        assert_eq!(gap.missing.step, OriginStepKind::Method, "{label}");
        assert_eq!(gap.missing.name, "Flask.missing_runner", "{label}");
        assert_eq!(gap.missing.reason, "method_not_found", "{label}");
    }
}

/// `src/flask/sessions.py`, with `SessionMixin`'s base left to each case.
fn proxy_sessions(base: &str) -> String {
    format!(
        r#"import typing as t
from collections.abc import MutableMapping


class SessionMixin({base}):
    @property
    def permanent(self):
        return self.get("_permanent", False)


class SecureCookieSession(SessionMixin):
    pass


class NullSession(SecureCookieSession):
    def _fail(self, *args, **kwargs):
        raise RuntimeError("The session is unavailable.")

    __setitem__ = __delitem__ = clear = pop = popitem = update = setdefault = _fail
"#
    )
}

/// `SessionMixin`'s base written bare, and subscripted the way Flask writes
/// it: either way it is the library's `MutableMapping`.
const SESSION_BASES: [&str; 2] = ["MutableMapping", "MutableMapping[str, t.Any]"];

/// `src/flask/globals.py`, with the proxy's bases left to each test, and a
/// function whose parameter is declared as the proxy.
fn proxy_globals(bases: &str) -> String {
    format!(
        r#"from __future__ import annotations

import typing as t

from werkzeug.local import LocalProxy

if t.TYPE_CHECKING:
    from .sessions import SessionMixin

    T = t.TypeVar("T", covariant=True)

    class ProxyMixin(t.Protocol[T]):
        def _get_current_object(self) -> T: ...

    class SessionMixinProxy({bases}): ...


session: SessionMixinProxy = LocalProxy(  # type: ignore[assignment]
    _cv_app, "session"
)


def forget(current: SessionMixinProxy) -> None:
    current.pop("foo")
    current.get("missing_key")
"#
    )
}

const PROXY_TEST: &str = r#"import flask


def test_missing_session(app):
    flask.session.pop("foo")
    flask.session.get("missing_key")
"#;

const GLOBALS_PY: &str = "src/flask/globals.py";

/// The proxy repository with the proxy's `bases`, once for each spelling of
/// `SessionMixin`'s base in [`SESSION_BASES`], each with its spelling.
fn proxy_repositories(bases: &str) -> Vec<(&'static str, Vec<FileParseData>)> {
    let globals = proxy_globals(bases);
    SESSION_BASES
        .into_iter()
        .map(|session_base| {
            let sessions = proxy_sessions(session_base);
            let files = [
                (
                    "src/flask/__init__.py",
                    "from .globals import session as session\n",
                ),
                ("src/flask/sessions.py", sessions.as_str()),
                (GLOBALS_PY, globals.as_str()),
                ("tests/test_basic.py", PROXY_TEST),
            ]
            .into_iter()
            .map(|(path, source)| fixture(path, source))
            .collect();
            (session_base, files)
        })
        .collect()
}

/// A value declared `SessionMixinProxy`, a class that exists for the type
/// checker alone, is of any class of `SessionMixin`'s hierarchy.
/// `NullSession` binds `pop`, by an assignment: the call may reach the
/// repository and is not proved outside, although no class the proxy itself
/// derives from holds the name. `get`, which no class of that hierarchy
/// binds, is the library's.
#[test]
fn a_proxy_for_the_type_checker_stands_for_the_classes_its_base_promises() {
    for (session_base, files) in proxy_repositories("SessionMixin") {
        for linked in both(&files) {
            let label = linked.label;
            assert_eq!(
                outside(&linked, GLOBALS_PY),
                vec![(
                    "get".to_string(),
                    "current: declared SessionMixinProxy -> \
                     SessionMixinProxy.get is outside the repository, inherited from MutableMapping"
                        .to_string()
                )],
                "{label}, SessionMixin({session_base})"
            );
        }
    }
}

/// A protocol promises nothing about a value's class, and neither does a
/// proxy whose only base is one: nothing is proved of either call.
#[test]
fn a_proxy_with_only_a_structural_base_proves_nothing() {
    for (session_base, files) in proxy_repositories("ProxyMixin[SessionMixin]") {
        for linked in both(&files) {
            assert_eq!(
                outside(&linked, GLOBALS_PY),
                Vec::<(String, String)>::new(),
                "{}, SessionMixin({session_base})",
                linked.label
            );
        }
    }
}

/// Flask's own shape, a protocol beside the nominal base: the nominal base
/// alone bounds the value's class.
#[test]
fn a_structural_base_beside_a_nominal_one_does_not_widen_the_family() {
    for (session_base, files) in proxy_repositories("ProxyMixin[SessionMixin], SessionMixin") {
        for linked in both(&files) {
            let label = linked.label;
            assert!(
                outside(&linked, GLOBALS_PY)
                    .iter()
                    .all(|(method, _)| method != "pop"),
                "{label}, SessionMixin({session_base})"
            );
        }
    }
}

/// `flask.session` is read off the package, which re-exports the name
/// `globals.py` declares as `SessionMixinProxy`. A call through it is
/// followed there: `get`, which no class of `SessionMixin`'s hierarchy binds,
/// is the library's, and `pop`, which `NullSession` binds, stays a gap at the
/// proxy. The module itself is no receiver of a call.
#[test]
fn a_call_through_a_name_a_module_binds_is_followed_to_its_declared_type() {
    for (session_base, files) in proxy_repositories("ProxyMixin[SessionMixin], SessionMixin") {
        let test = entity(&files, "tests/test_basic.py", "test_missing_session");
        for linked in both(&files) {
            let label = format!("{}, SessionMixin({session_base})", linked.label);
            assert_eq!(
                outside(&linked, "tests/test_basic.py"),
                vec![(
                    "get".to_string(),
                    "flask.session: declared SessionMixinProxy -> \
                     SessionMixinProxy.get is outside the repository, inherited from MutableMapping"
                        .to_string()
                )],
                "{label}"
            );
            let gap = gap_for(&linked, "tests/test_basic.py", test, "flask.session", "pop");
            assert_eq!(gap.missing.reason, "method_not_found", "{label}");
            assert_eq!(gap.missing.name, "SessionMixinProxy.pop", "{label}");
            assert_eq!(
                calls_from(&linked, &files, test),
                Vec::<(String, f32, bool)>::new(),
                "{label}"
            );
        }
    }
}

const APP_PROXY_APP: &str = r#"class Flask:
    def ensure_sync(self, func):
        return func
"#;

/// `src/flask/globals.py` with the proxy's bases left to each case, and a
/// function that calls through the proxy.
fn app_proxy_globals(bases: &str) -> String {
    format!(
        r#"import typing as t

from werkzeug.local import LocalProxy

if t.TYPE_CHECKING:
    from .app import Flask

    T = t.TypeVar("T", covariant=True)

    class ProxyMixin(t.Protocol[T]):
        def _get_current_object(self) -> T: ...

    class FlaskProxy({bases}): ...


current_app: FlaskProxy = LocalProxy(_cv_app, "app")


def dispatch(meth):
    app = current_app._get_current_object()
    return current_app.ensure_sync(meth)
"#
    )
}

/// `current_app` is declared `FlaskProxy`, a class for the type checker that
/// stands for a `Flask` and names `ProxyMixin`, a protocol, beside it. A call
/// of a method `Flask` declares is bound to it. `_get_current_object` is
/// declared only by the protocol, whose method describes the value and is
/// never the code that runs (werkzeug's `LocalProxy` runs its own), so that
/// call is no edge and its chain stops at the protocol's method, whether the
/// base is written bare or subscripted the way Flask writes it.
#[test]
fn a_method_only_a_protocol_declares_is_no_target_of_a_call() {
    for (bases, reason) in [
        ("ProxyMixin, Flask", "declared_for_type_checker"),
        ("ProxyMixin[Flask], Flask", "declared_for_type_checker"),
    ] {
        let globals = app_proxy_globals(bases);
        let files: Vec<FileParseData> = [
            ("src/flask/app.py", APP_PROXY_APP),
            ("src/flask/globals.py", globals.as_str()),
        ]
        .into_iter()
        .map(|(path, source)| fixture(path, source))
        .collect();
        let dispatch = entity(&files, "src/flask/globals.py", "dispatch");
        for linked in both(&files) {
            let label = format!("{} {bases}", linked.label);
            assert!(
                proves(&linked, &files, dispatch, "Flask.ensure_sync"),
                "{label}: {:?}",
                calls_from(&linked, &files, dispatch)
            );
            assert!(
                calls_from(&linked, &files, dispatch)
                    .iter()
                    .all(|(target, _, _)| !target.ends_with("_get_current_object")),
                "{label}: {:?}",
                calls_from(&linked, &files, dispatch)
            );
            let gap = gap_for(
                &linked,
                "src/flask/globals.py",
                dispatch,
                "current_app",
                "_get_current_object",
            );
            assert_eq!(gap.missing.reason, reason, "{label}");
        }
    }
}

/// `flask.request._get_current_object()`: `RequestProxy` stands for a
/// `Request`, whose hierarchy reaches a library base, and the method is
/// declared by none of it. The protocol beside it declares the method, and
/// what runs is werkzeug's proxy method, which no base names. So nothing is
/// proved of the call, rather than a proof that names `Request` as where
/// the method comes from.
#[test]
fn a_method_a_proxys_protocol_declares_is_not_proved_inherited() {
    let globals = r#"import typing as t

from werkzeug.local import LocalProxy

if t.TYPE_CHECKING:
    from .wrappers import Request

    T = t.TypeVar("T", covariant=True)

    class ProxyMixin(t.Protocol[T]):
        def _get_current_object(self) -> T: ...

    class RequestProxy(ProxyMixin[Request], Request): ...


request: RequestProxy = LocalProxy(_cv_app, "request")


def view():
    real = request._get_current_object()
    return request.get_json()
"#;
    let wrappers = r#"from werkzeug.wrappers import Request as RequestBase


class Request(RequestBase):
    url_rule = None
"#;
    let files: Vec<FileParseData> = [
        ("src/flask/wrappers.py", wrappers),
        ("src/flask/globals.py", globals),
    ]
    .into_iter()
    .map(|(path, source)| fixture(path, source))
    .collect();
    for linked in both(&files) {
        let methods: Vec<String> = outside(&linked, "src/flask/globals.py")
            .into_iter()
            .map(|(method, _)| method)
            .collect();
        assert_eq!(methods, vec!["get_json".to_string()], "{}", linked.label);
    }
}

/// Flask's tests declare views inside the test: `@app.get("/")` on a nested
/// function reads the test's `app`, the fixture, in the scope the definition
/// sits in, and so does a call in a nested helper's body. Both are bound
/// through the fixture chain. Where a nested function binds the name itself,
/// the test holds two bindings of it, and no call reads either as one value.
#[test]
fn a_nested_function_reads_the_receiver_its_enclosing_test_binds() {
    let test = r#"def test_route(app):
    @app.get("/")
    def index():
        return "x"

    def helper():
        return app.get("/again")

    return index


def test_shadowed(app):
    @app.get("/")
    def index():
        return "x"

    def shadowed(app):
        return app.get("/mine")

    return index
"#;
    let files = client_repository(&[("tests/test_route.py", test)]);
    let named = |name: &str| entity(&files, "tests/test_route.py", name);
    for linked in both(&files) {
        let label = linked.label;
        for name in ["test_route.<locals>.index", "test_route.<locals>.helper"] {
            assert!(
                proves(&linked, &files, named(name), "Scaffold.get"),
                "{label} {name}: {:?}",
                calls_from(&linked, &files, named(name))
            );
        }
        // A nested function that binds `app` makes the test's `app` two
        // bindings, so nothing in that test reads it as one value.
        for name in [
            "test_shadowed.<locals>.index",
            "test_shadowed.<locals>.shadowed",
        ] {
            assert!(
                !proves(&linked, &files, named(name), "Scaffold.get"),
                "{label} {name}: {:?}",
                calls_from(&linked, &files, named(name))
            );
        }
    }
}
