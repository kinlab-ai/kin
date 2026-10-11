// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Member reads off `self`, in a class that shares no instance with the
//! focal's owner, over source the pipeline parsed.
//!
//! `MethodView.dispatch_request` in Flask reads `getattr(self, "get", None)`.
//! A census asked about `Scaffold.get`, the route decorator, listed that read
//! as a use that could hand `Scaffold.get` on, because it spells `get` on
//! some object. `self` there is a `MethodView`, and no class is, or derives
//! from, both `MethodView` and `Scaffold`, so the read gets a member of the
//! view's own hierarchy.
//!
//! The reading is relative to the focal. It never says where the member that
//! is read is declared: for a focal inside the receiver's own hierarchy the
//! same read stays listed.

use std::collections::HashMap;

use kin_db::{focal_escape_evidence_in, CensusFile, EscapeCensusGraph};
use kin_index::IndexPipeline;
use kin_model::{
    focal_call_names, Entity, EntityId, FilePathId, FocalEscape, Hash256, LanguageId,
    RelationEvidence, UnrelatedReceiver,
};

/// `src/flask/sansio/scaffold.py`: the route decorator a reach question
/// followed.
const SCAFFOLD: &str = r#"class Scaffold:
    def get(self, rule, **options):
        return rule
"#;

/// `src/flask/views.py`: the view that looks its handler up by method name.
const VIEWS: &str = r#"class View:
    def dispatch_request(self):
        raise NotImplementedError()


class MethodView(View):
    def dispatch_request(self, **kwargs):
        meth = getattr(self, "get", None)
        return meth(**kwargs)
"#;

/// A view that is also a scaffold: an instance of `MethodView` may be one.
const ROUTED: &str = r#"from flask.sansio.scaffold import Scaffold
from flask.views import MethodView


class RoutedView(MethodView, Scaffold):
    pass
"#;

/// A view whose base answers attribute reads by name.
const DYNAMIC_VIEWS: &str = r#"class View:
    def __getattr__(self, name):
        return self.handlers[name]


class MethodView(View):
    def dispatch_request(self, **kwargs):
        meth = getattr(self, "get", None)
        return meth(**kwargs)
"#;

/// A view whose base no declared name settles.
const OPEN_VIEWS: &str = r#"def make_base():
    return object


class MethodView(make_base()):
    def dispatch_request(self, **kwargs):
        meth = getattr(self, "get", None)
        return meth(**kwargs)
"#;

/// `src/flask/sessions.py`: a base reads `self.pop`, and a class derived from
/// it declares `pop`.
const SESSIONS: &str = r#"class SessionMixin:
    def discard(self, key):
        return call_later(self.pop, key)


class NullSession(SessionMixin):
    def pop(self, key, default=None):
        raise RuntimeError("The session is unavailable.")
"#;

struct Parsed {
    files: Vec<CensusFile>,
    entities: HashMap<String, Vec<Entity>>,
}

impl Parsed {
    fn of(sources: &[(&str, &str)]) -> Self {
        let pipeline = IndexPipeline::new();
        let mut files = Vec::new();
        let mut entities = HashMap::new();
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
            entities.insert(path.to_string(), indexed.entities);
        }
        Self { files, entities }
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

const SCAFFOLD_PY: &str = "src/flask/sansio/scaffold.py";
const VIEWS_PY: &str = "src/flask/views.py";

#[test]
fn a_read_off_self_in_an_unrelated_class_is_not_a_use_of_the_focal() {
    let parsed = Parsed::of(&[(SCAFFOLD_PY, SCAFFOLD), (VIEWS_PY, VIEWS)]);
    let escape = parsed.census(SCAFFOLD_PY, "Scaffold.get");
    assert!(
        matches!(escape, FocalEscape::Contained { .. }),
        "{escape:?}"
    );
    assert_eq!(values(&escape), Vec::<(String, u32)>::new());
    assert_eq!(
        unrelated(&escape),
        vec![UnrelatedReceiver {
            receiver: "self".into(),
            class: "MethodView".into(),
            name: "get".into(),
            reads: 1,
        }]
    );
}

#[test]
fn a_class_derived_from_both_keeps_the_read_listed() {
    let parsed = Parsed::of(&[
        (SCAFFOLD_PY, SCAFFOLD),
        (VIEWS_PY, VIEWS),
        ("tests/routed.py", ROUTED),
    ]);
    let escape = parsed.census(SCAFFOLD_PY, "Scaffold.get");
    assert_eq!(
        values(&escape),
        vec![(VIEWS_PY.to_string(), line_of(VIEWS, "getattr(self"))],
        "a MethodView may be a RoutedView, whose `get` is Scaffold's"
    );
    assert_eq!(unrelated(&escape), Vec::new());
}

#[test]
fn a_hierarchy_the_records_do_not_settle_keeps_the_read_listed() {
    for (views, why) in [
        (DYNAMIC_VIEWS, "a base answers attribute reads by name"),
        (OPEN_VIEWS, "a base no declared name settles"),
    ] {
        let parsed = Parsed::of(&[(SCAFFOLD_PY, SCAFFOLD), (VIEWS_PY, views)]);
        let escape = parsed.census(SCAFFOLD_PY, "Scaffold.get");
        assert_eq!(
            values(&escape),
            vec![(VIEWS_PY.to_string(), line_of(views, "getattr(self"))],
            "{why}"
        );
        assert_eq!(unrelated(&escape), Vec::new(), "{why}");
    }
}

/// The guard on what the rule says. `self.pop` in `SessionMixin` may be read
/// off a `NullSession`, whose `pop` is the repository's own: asked about
/// that member the read stays listed. Asked about `Scaffold.get`'s neighbour
/// in another hierarchy it is settled, as a read of another hierarchy's
/// member and nothing more.
#[test]
fn a_read_in_the_focals_own_hierarchy_stays_listed() {
    let sessions = "src/flask/sessions.py";
    let context = "src/flask/ctx.py";
    let ctx = "class AppContext:\n    def pop(self, exc=None):\n        return exc\n";
    let parsed = Parsed::of(&[(sessions, SESSIONS), (context, ctx)]);

    let own = parsed.census(sessions, "NullSession.pop");
    assert_eq!(
        values(&own),
        vec![(sessions.to_string(), line_of(SESSIONS, "self.pop"))]
    );
    assert_eq!(unrelated(&own), Vec::new());

    let other = parsed.census(context, "AppContext.pop");
    assert_eq!(values(&other), Vec::<(String, u32)>::new());
    assert_eq!(
        unrelated(&other),
        vec![UnrelatedReceiver {
            receiver: "self".into(),
            class: "SessionMixin".into(),
            name: "pop".into(),
            reads: 1,
        }]
    );
}

/// A record older than version 3 lists its classes without saying which
/// answer attribute reads by name, so nothing is settled over it.
#[test]
fn a_record_that_states_no_class_marks_keeps_the_read_listed() {
    let mut parsed = Parsed::of(&[(SCAFFOLD_PY, SCAFFOLD), (VIEWS_PY, VIEWS)]);
    for entities in parsed.entities.values_mut() {
        for entity in entities.iter_mut() {
            if let Some(record) = entity
                .metadata
                .extra
                .get_mut(kin_model::VALUE_ESCAPE_RECORD_KEY)
            {
                record["v"] = serde_json::json!(2);
            }
        }
    }
    let escape = parsed.census(SCAFFOLD_PY, "Scaffold.get");
    assert_eq!(
        values(&escape),
        vec![(VIEWS_PY.to_string(), line_of(VIEWS, "getattr(self"))]
    );
    assert_eq!(unrelated(&escape), Vec::new());
}
