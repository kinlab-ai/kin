// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Member reads off a receiver with a declared type, over source the
//! pipeline parsed and linked: whether a census lists one for a focal depends
//! on whether a value of the receiver's class could be an instance of the
//! focal's owner.
//!
//! Flask's `flask.session` is declared `SessionMixinProxy`, a class that
//! exists only under `if t.TYPE_CHECKING:`. A test passes `flask.session.pop`
//! on as a value. Asked about `AppContext.pop`, that read is not a use: no
//! class is both a session and an application context. Asked about a `pop`
//! declared inside the session hierarchy it is one, and must stay listed,
//! although `NullSession` is neither `SessionMixinProxy`, one of its bases
//! nor derived from it: the declared class is a fiction for the type
//! checker, and the object is whatever session is current.

use std::collections::HashMap;

use kin_db::call_site_escape::TypedMemberRead;
use kin_db::{focal_escape_evidence_in, CensusFile, EscapeCensusGraph};
use kin_index::coverage_certificate::CertificateDetail;
use kin_index::{FileParseData, IndexPipeline};
use kin_model::{
    focal_call_names, ArtifactId, Entity, EntityId, FilePathId, FocalEscape, GraphNodeId, Hash256,
    LanguageId, ParseCompleteness, RelationEvidence, UnrelatedReceiver,
};

const INIT: &str = "from .globals import session as session\n";

/// `src/flask/sessions.py`.
const SESSIONS: &str = r#"from collections.abc import MutableMapping


class SessionMixin(MutableMapping):
    @property
    def permanent(self):
        return self.get("_permanent", False)


class SecureCookieSession(SessionMixin):
    pass


class NullSession(SecureCookieSession):
    def pop(self, key, default=None):
        raise RuntimeError("The session is unavailable.")
"#;

/// `src/flask/ctx.py`.
const CTX: &str = r#"class AppContext:
    def pop(self, exc=None):
        return exc
"#;

/// `src/flask/globals.py`: the proxies' declared types exist for the type
/// checker alone.
const GLOBALS: &str = r#"from __future__ import annotations

import typing as t

from werkzeug.local import LocalProxy

if t.TYPE_CHECKING:
    from .ctx import AppContext
    from .sessions import SessionMixin

    T = t.TypeVar("T", covariant=True)

    class ProxyMixin(t.Protocol[T]):
        def _get_current_object(self) -> T: ...

    class AppContextProxy(ProxyMixin[AppContext], AppContext): ...

    class SessionMixinProxy(ProxyMixin[SessionMixin], SessionMixin): ...


session: SessionMixinProxy = LocalProxy(  # type: ignore[assignment]
    _cv_app, "session"
)
"#;

/// [`GLOBALS`] with other bases on the session proxy.
fn globals_with(bases: &str) -> String {
    let flask = "ProxyMixin[SessionMixin], SessionMixin";
    assert!(GLOBALS.contains(flask));
    GLOBALS.replace(flask, bases)
}

/// `tests/test_basic.py`: `test_missing_session`.
const TEST_BASIC: &str = r#"import flask


def test_missing_session(app):
    def expect_exception(f, *args, **kwargs):
        f(*args, **kwargs)

    expect_exception(flask.session.pop, "foo")
"#;

struct Parsed {
    files: Vec<CensusFile>,
    entities: HashMap<String, Vec<Entity>>,
    /// What each file's coverage certificate says of its member reads, when
    /// the graph a census reads holds certificates at all.
    typed: Option<HashMap<String, Vec<TypedMemberRead>>>,
}

impl Parsed {
    fn of(sources: &[(&str, &str)], linked: bool) -> Self {
        let pipeline = IndexPipeline::new();
        let mut files = Vec::new();
        let mut entities = HashMap::new();
        let mut parsed = Vec::new();
        for (path, source) in sources {
            let blob = kin_blobs::digest(source.as_bytes());
            let indexed = pipeline
                .index_file_content_with_tests(&FilePathId::new(*path), source.as_bytes(), blob)
                .unwrap()
                .indexed_file;
            files.push(CensusFile {
                path: FilePathId::new(*path),
                language: LanguageId::Python,
                blob: Hash256::from_hex(&blob.to_string()).unwrap(),
                parsed: Some(true),
            });
            entities.insert(path.to_string(), indexed.entities.clone());
            parsed.push(FileParseData {
                file_path: path.to_string(),
                entities: indexed.entities,
                relations: indexed.extracted_relations,
                imports: indexed.imports,
            });
        }
        let typed = linked.then(|| {
            let ids: HashMap<String, ArtifactId> = parsed
                .iter()
                .map(|file| (file.file_path.clone(), ArtifactId::new()))
                .collect();
            let completeness = parsed
                .iter()
                .map(|file| (file.file_path.clone(), ParseCompleteness::Full))
                .collect();
            let relations =
                kin_index::link_cross_file_with_completeness(&parsed, &ids, &completeness).unwrap();
            files
                .iter()
                .map(|file| {
                    let artifact = ids[&file.path.0];
                    let node = GraphNodeId::Artifact(artifact);
                    let reads = relations
                        .iter()
                        .filter(|relation| relation.src == node && relation.dst == node)
                        .find(|relation| CertificateDetail::from_certificate(relation).is_some())
                        .map(|certificate| {
                            kin_index::receiver_typed_reads_of(
                                certificate,
                                &file.path.0,
                                artifact,
                                file.blob,
                            )
                        })
                        .unwrap_or_default()
                        .into_iter()
                        .map(|read| TypedMemberRead {
                            line: read.line,
                            member: read.member,
                            receiver: read.receiver,
                            class: read.class,
                        })
                        .collect();
                    (file.path.0.clone(), reads)
                })
                .collect()
        });
        Self {
            files,
            entities,
            typed,
        }
    }

    fn census(&self, path: &str, name: &str) -> FocalEscape {
        let focal = self.entities[path]
            .iter()
            .find(|entity| entity.name == name)
            .unwrap_or_else(|| panic!("no entity {name} in {path}"));
        focal_escape_evidence_in(self, focal, &focal_call_names(focal))
    }
}

impl EscapeCensusGraph for Parsed {
    fn census_files(&self, languages: &[LanguageId]) -> Option<Vec<CensusFile>> {
        Some(
            self.files
                .iter()
                .filter(|file| languages.contains(&file.language))
                .cloned()
                .collect(),
        )
    }

    fn entities_in(&self, path: &FilePathId) -> Option<Vec<Entity>> {
        Some(self.entities.get(&path.0).cloned().unwrap_or_default())
    }

    fn import_evidence_into(&self, _target: EntityId) -> Option<Vec<RelationEvidence>> {
        Some(Vec::new())
    }

    fn typed_member_reads(&self, file: &CensusFile) -> Vec<TypedMemberRead> {
        self.typed
            .as_ref()
            .and_then(|typed| typed.get(&file.path.0).cloned())
            .unwrap_or_default()
    }
}

fn line_of(source: &str, text: &str) -> u32 {
    source
        .lines()
        .position(|line| line.contains(text))
        .unwrap_or_else(|| panic!("`{text}` is in the source")) as u32
        + 1
}

/// The value uses a census lists for a focal, as (file, line).
fn values(escape: &FocalEscape) -> Vec<(String, u32)> {
    escape
        .residue()
        .map(|residue| {
            residue
                .values
                .iter()
                .map(|site| (site.file.clone(), site.line))
                .collect()
        })
        .unwrap_or_default()
}

fn unrelated(escape: &FocalEscape) -> Vec<UnrelatedReceiver> {
    escape
        .residue()
        .map(|residue| residue.unrelated_receivers.clone())
        .unwrap_or_default()
}

const SOURCES: [(&str, &str); 5] = [
    ("src/flask/__init__.py", INIT),
    ("src/flask/sessions.py", SESSIONS),
    ("src/flask/ctx.py", CTX),
    ("src/flask/globals.py", GLOBALS),
    ("tests/test_basic.py", TEST_BASIC),
];

fn the_read() -> Vec<(String, u32)> {
    vec![(
        "tests/test_basic.py".to_string(),
        line_of(TEST_BASIC, "flask.session.pop"),
    )]
}

#[test]
fn a_read_off_the_session_proxy_is_not_a_use_of_another_hierarchys_member() {
    let focal = ("src/flask/ctx.py", "AppContext.pop");

    // No certificate: nothing says what `flask.session` is, so the read is
    // listed.
    let unlinked = Parsed::of(&SOURCES, false).census(focal.0, focal.1);
    assert_eq!(values(&unlinked), the_read());

    // Linked, the receiver is the declared `SessionMixinProxy`, which stands
    // for anything in `SessionMixin`'s hierarchy, and no class is both that
    // and an `AppContext`.
    let linked = Parsed::of(&SOURCES, true).census(focal.0, focal.1);
    assert!(
        matches!(linked, FocalEscape::Contained { .. }),
        "{linked:?}"
    );
    assert_eq!(values(&linked), Vec::<(String, u32)>::new());
    assert_eq!(
        unrelated(&linked),
        vec![UnrelatedReceiver {
            receiver: "flask.session".into(),
            class: "SessionMixinProxy".into(),
            name: "pop".into(),
            reads: 1,
        }]
    );
}

/// A protocol promises nothing about a value's class, and a base outside
/// the repository names no class the records hold. A proxy with only such
/// bases says nothing of what `flask.session` is, so the read stays listed.
#[test]
fn a_proxy_whose_bases_promise_no_class_settles_nothing() {
    for bases in [
        "ProxyMixin[SessionMixin]",
        "ProxyMixin[SessionMixin], LocalProxy",
    ] {
        let globals = globals_with(bases);
        let mut sources = SOURCES;
        sources[3].1 = globals.as_str();
        let linked = Parsed::of(&sources, true).census("src/flask/ctx.py", "AppContext.pop");
        assert_eq!(values(&linked), the_read(), "{bases}");
        assert_eq!(unrelated(&linked), Vec::new(), "{bases}");
    }
}

/// The guard. In the test this line comes from, the session is a
/// `NullSession`, whose `pop` is the repository's own. `NullSession` is
/// neither `SessionMixinProxy`, an ancestor nor a descendant of it, so a rule
/// that read the declared class as the value's class would settle the read
/// and lose a true use.
#[test]
fn the_same_read_stays_listed_for_a_member_of_the_session_hierarchy() {
    let linked = Parsed::of(&SOURCES, true).census("src/flask/sessions.py", "NullSession.pop");
    assert_eq!(values(&linked), the_read());
    assert_eq!(unrelated(&linked), Vec::new());
}
