// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Overrides declared by classes nested in function bodies.
//!
//! flask's own tests override `SessionInterface.open_session` with classes
//! they declare inside the test function. Before the parser admitted nested
//! definitions those classes had no entity, so an overrides question about
//! `open_session` found only the library's own subclass. The parser now names
//! them `test.<locals>.Class` and emits the same `Contains` and `Extends`
//! edges a module-level class gets, and the linker's override derivation reads
//! those edges unchanged. These fixtures drive the real parser and both real
//! linkers over that shape and its negatives.

use kin_index::linker::ArtifactIdentityMap;
use kin_index::{link_cross_file_incremental, FileParseData, IncrementalLinker};
use kin_model::{ArtifactId, Entity, EntityId, FilePathId, Relation, RelationKind};
use kin_parser::{LanguageAdapter, PythonAdapter};

const SESSIONS: &str = "\
class SessionInterface:
    def open_session(self, app, request):
        raise NotImplementedError()

    def save_session(self, app, session, response):
        raise NotImplementedError()


class SecureCookieSessionInterface(SessionInterface):
    def open_session(self, app, request):
        return None
";

const TEST_REQCTX: &str = "\
import flask
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
";

const TEST_SESSION_INTERFACE: &str = "\
import flask
from flask.sessions import SessionInterface


def test_open_session_with_endpoint():
    class MySessionInterface(SessionInterface):
        def save_session(self, app, session, response):
            pass

        def open_session(self, app, request):
            return None

    app = flask.Flask(__name__)
    app.session_interface = MySessionInterface()
";

/// A base passed in as a parameter is a value, not a class the file declares
/// or imports, so no hierarchy can be walked through it. A module-level class
/// of the same name as the parameter must not be picked up either.
const TEST_DYNAMIC_BASE: &str = "\
class base:
    def open_session(self, app, request):
        return 'decoy'


def t(base):
    class C(base):
        def open_session(self, app, request):
            return None
    return C
";

/// Two local classes in one function: `B(A)` overrides the local `A`, not a
/// module-level class that shares the name.
const TEST_LOCAL_BASE: &str = "\
class A:
    def m(self):
        return 'module'


def outer():
    class A:
        def m(self):
            return 'local'

    class B(A):
        def m(self):
            return 'override'

    return B
";

fn parse_py(file_path: &str, source: &str) -> FileParseData {
    let adapter = PythonAdapter;
    let file_id = FilePathId::new(file_path);
    let bytes = source.as_bytes();
    let tree = adapter.parse(bytes).expect("parse");
    let output = adapter.extract(&tree, bytes, &file_id).expect("extract");
    let entities: Vec<Entity> = output
        .entities
        .into_iter()
        .map(|e| e.into_entity_with_source(adapter.language_id(), &file_id, Some(bytes)))
        .collect();
    FileParseData {
        file_path: file_path.to_string(),
        entities,
        relations: output.relations,
        imports: output.imports,
    }
}

fn package() -> Vec<FileParseData> {
    vec![
        parse_py("flask/sessions.py", SESSIONS),
        parse_py("tests/test_reqctx.py", TEST_REQCTX),
        parse_py("tests/test_session_interface.py", TEST_SESSION_INTERFACE),
        parse_py("tests/test_dynamic_base.py", TEST_DYNAMIC_BASE),
        parse_py("tests/test_local_base.py", TEST_LOCAL_BASE),
    ]
}

fn artifact_ids(files: &[FileParseData]) -> ArtifactIdentityMap {
    files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect()
}

fn entity_id(files: &[FileParseData], file: &str, name: &str) -> EntityId {
    files
        .iter()
        .flat_map(|f| f.entities.iter())
        .find(|e| e.name == name && e.file_origin.as_ref().map(|p| p.0.as_str()) == Some(file))
        .unwrap_or_else(|| panic!("entity `{name}` in `{file}` not found"))
        .id
}

fn overrides_from(relations: &[Relation], src: EntityId) -> Vec<EntityId> {
    let mut targets: Vec<EntityId> = relations
        .iter()
        .filter(|r| r.kind == RelationKind::Overrides && r.src.as_entity() == Some(src))
        .filter_map(|r| r.dst.as_entity())
        .collect();
    targets.sort();
    targets.dedup();
    targets
}

fn linked_batch(files: &[FileParseData]) -> Vec<Relation> {
    kin_index::link_cross_file(files, &artifact_ids(files))
        .expect("every fixture file has an explicitly assigned artifact identity")
}

fn linked_incremental(files: &[FileParseData]) -> Vec<Relation> {
    let mut linker = IncrementalLinker::new();
    for file in files {
        linker.add_file(&file.file_path, ArtifactId::new(), &file.entities);
    }
    linker.record_class_bases(files);
    link_cross_file_incremental(files, &linker)
        .expect("every fixture file has an explicitly assigned artifact identity")
}

#[test]
fn a_class_nested_in_a_test_overrides_its_imported_base_method() {
    let files = package();
    let base = entity_id(&files, "flask/sessions.py", "SessionInterface.open_session");
    for (linker, relations) in [
        ("batch", linked_batch(&files)),
        ("incremental", linked_incremental(&files)),
    ] {
        for (file, method) in [
            (
                "tests/test_reqctx.py",
                "test_session_error_pops_context.<locals>.FailingSessionInterface.open_session",
            ),
            (
                "tests/test_session_interface.py",
                "test_open_session_with_endpoint.<locals>.MySessionInterface.open_session",
            ),
        ] {
            let overrider = entity_id(&files, file, method);
            assert_eq!(
                overrides_from(&relations, overrider),
                vec![base],
                "{linker}: {method} overrides SessionInterface.open_session"
            );
        }
        let save = entity_id(
            &files,
            "tests/test_session_interface.py",
            "test_open_session_with_endpoint.<locals>.MySessionInterface.save_session",
        );
        assert_eq!(
            overrides_from(&relations, save),
            vec![entity_id(
                &files,
                "flask/sessions.py",
                "SessionInterface.save_session"
            )],
            "{linker}: every overridden member is found, not only the one asked about"
        );
    }
}

#[test]
fn the_library_subclass_still_overrides_beside_the_nested_ones() {
    let files = package();
    let base = entity_id(&files, "flask/sessions.py", "SessionInterface.open_session");
    let relations = linked_batch(&files);
    let overriders: Vec<EntityId> = relations
        .iter()
        .filter(|r| r.kind == RelationKind::Overrides && r.dst.as_entity() == Some(base))
        .filter_map(|r| r.src.as_entity())
        .collect();
    assert_eq!(overriders.len(), 3, "{overriders:?}");
}

#[test]
fn a_base_bound_to_a_parameter_is_not_a_hierarchy() {
    let files = package();
    let method = entity_id(
        &files,
        "tests/test_dynamic_base.py",
        "t.<locals>.C.open_session",
    );
    for (linker, relations) in [
        ("batch", linked_batch(&files)),
        ("incremental", linked_incremental(&files)),
    ] {
        assert!(
            relations
                .iter()
                .all(|r| !(r.kind == RelationKind::Overrides && r.src.as_entity() == Some(method))),
            "{linker}: `class C(base)` with `base` a parameter overrides nothing"
        );
    }
}

#[test]
fn a_local_base_resolves_to_the_local_class_and_not_a_module_namesake() {
    let files = package();
    let overrider = entity_id(&files, "tests/test_local_base.py", "outer.<locals>.B.m");
    let local = entity_id(&files, "tests/test_local_base.py", "outer.<locals>.A.m");
    for (linker, relations) in [
        ("batch", linked_batch(&files)),
        ("incremental", linked_incremental(&files)),
    ] {
        assert_eq!(
            overrides_from(&relations, overrider),
            vec![local],
            "{linker}: B(A) inside `outer` extends the `A` beside it"
        );
    }
}
