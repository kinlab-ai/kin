// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Which assignments to a focal's name a census lists, over source the
//! pipeline parsed.
//!
//! A module-scope or class-scope assignment whose target spells the focal's
//! name is another declaration of that name, not a use of the focal. It is a
//! site of the focal only where it binds the focal's own name again: in the
//! focal's module, or in a class that stands in the focal's hierarchy. The
//! census used to list every such assignment in the repository, and an answer
//! then asked its reader to check each one.
//!
//! The sources below are Flask's and React's own lines for the cases that
//! were listed.

use std::collections::HashMap;

use kin_db::{focal_escape_evidence_in, CensusFile, EscapeCensusGraph};
use kin_index::IndexPipeline;
use kin_model::{
    focal_call_names, Entity, EntityId, FilePathId, FocalEscape, Hash256, LanguageId,
    RelationEvidence,
};

/// `src/flask/ctx.py`: the class whose `pop` a callers answer asked about.
const CTX: &str = r#"class AppContext:
    def push(self) -> None:
        pass

    def pop(self, exc: BaseException | None = None) -> None:
        pass
"#;

/// `src/flask/sessions.py`: `NullSession` binds seven names to `_fail` in
/// one assignment, `pop` among them.
const SESSIONS: &str = r#"import typing as t
from collections.abc import MutableMapping

from werkzeug.datastructures import CallbackDict


class SessionMixin(MutableMapping[str, t.Any]):
    @property
    def permanent(self) -> bool:
        return self.get("_permanent", False)


class SecureCookieSession(CallbackDict[str, t.Any], SessionMixin):
    modified = False


class NullSession(SecureCookieSession):
    def _fail(self, *args: t.Any, **kwargs: t.Any) -> t.NoReturn:
        raise RuntimeError("The session is unavailable.")

    __setitem__ = __delitem__ = clear = pop = popitem = update = setdefault = _fail
    del _fail
"#;

/// A subclass that replaces the member it inherits, by assignment.
const QUIET: &str = r#"from flask.ctx import AppContext


def _quiet(self, exc=None):
    return None


class QuietContext(AppContext):
    pop = _quiet
"#;

/// `react-dom-bindings`: the declaration a change request named.
const CONFIG_DOM: &str = r#"export function measureInstance(instance) {
  return instance.getBoundingClientRect();
}
"#;

/// `react-reconciler/src/ReactFiberConfigWithNoViewTransition.js`.
const CONFIG_SHIM: &str = r#"function shim(...args) {
  throw new Error('This renderer does not support view transitions.');
}

export const measureInstance = shim;
export const applyViewTransitionName = shim;
"#;

/// `react-reconciler/src/forks/ReactFiberConfig.custom.js`.
const CONFIG_CUSTOM: &str = r#"export const measureInstance = $$$config.measureInstance;
"#;

/// A module that binds its own function's name again, on one branch.
const TRACED: &str = r#"import os


def fast_load(path):
    return path


def load(path):
    return path


if os.environ.get("FAST"):
    load = fast_load
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
                language: if path.ends_with(".py") {
                    LanguageId::Python
                } else {
                    LanguageId::JavaScript
                },
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

    /// The same-named assignments the census lists for a focal.
    fn bindings(&self, path: &str, name: &str) -> Vec<(String, u32)> {
        let escape = self.census(path, name);
        assert!(
            !matches!(escape, FocalEscape::Unknown { .. }),
            "the census read every file: {escape:?}"
        );
        escape
            .residue()
            .map(|residue| {
                residue
                    .bindings
                    .iter()
                    .map(|site| (site.file.clone(), site.line))
                    .collect()
            })
            .unwrap_or_default()
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

#[test]
fn a_class_that_shares_nothing_with_the_focals_binds_its_own_member() {
    let parsed = Parsed::of(&[
        ("src/flask/ctx.py", CTX),
        ("src/flask/sessions.py", SESSIONS),
    ]);
    assert_eq!(
        parsed.bindings("src/flask/ctx.py", "AppContext.pop"),
        Vec::<(String, u32)>::new(),
        "`pop = ... = _fail` in NullSession declares NullSession.pop"
    );
}

#[test]
fn a_subclass_that_assigns_the_focals_name_stays_listed() {
    let parsed = Parsed::of(&[
        ("src/flask/ctx.py", CTX),
        ("src/flask/sessions.py", SESSIONS),
        ("tests/quiet.py", QUIET),
    ]);
    assert_eq!(
        parsed.bindings("src/flask/ctx.py", "AppContext.pop"),
        vec![("tests/quiet.py".to_string(), line_of(QUIET, "pop = _quiet"))],
        "QuietContext derives from AppContext, so its `pop` replaces the focal on its instances"
    );
}

#[test]
fn another_modules_assignment_to_the_name_declares_that_modules_name() {
    let dom = "packages/react-dom-bindings/src/client/ReactFiberConfigDOM.js";
    let parsed = Parsed::of(&[
        (dom, CONFIG_DOM),
        (
            "packages/react-reconciler/src/ReactFiberConfigWithNoViewTransition.js",
            CONFIG_SHIM,
        ),
        (
            "packages/react-reconciler/src/forks/ReactFiberConfig.custom.js",
            CONFIG_CUSTOM,
        ),
    ]);
    assert_eq!(
        parsed.bindings(dom, "measureInstance"),
        Vec::<(String, u32)>::new(),
        "`export const measureInstance = shim;` declares another module's measureInstance"
    );
}

#[test]
fn an_assignment_in_the_focals_own_module_stays_listed() {
    let parsed = Parsed::of(&[("app/config.py", TRACED)]);
    assert_eq!(
        parsed.bindings("app/config.py", "load"),
        vec![(
            "app/config.py".to_string(),
            line_of(TRACED, "load = fast_load")
        )]
    );
}
