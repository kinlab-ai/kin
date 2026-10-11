// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A call of a Python function or method that `typing.overload` stubs
//! declare binds the implementation, the definition Python keeps under the
//! name and the code the call runs, never a stub whose body is `...`.
//!
//! Flask declares `locate_app` with two `@t.overload` stubs above its
//! implementation, and `App.template_filter` the same way. The stubs are
//! entities of their own, and the Python adapter now says they only declare
//! (`kin_parser::declaration_only`), which every fan-out tier reads to pass
//! over them. These calls bind through the `(file, name)` slot, which already
//! held the implementation; they stay bound to it.

use std::collections::HashMap;

use kin_index::{FileParseData, IndexPipeline};
use kin_model::{ArtifactId, Entity, EntityId, FilePathId, ParseCompleteness, RelationKind};

/// `src/flask/cli.py`, cut down.
const CLI: &str = r#"from __future__ import annotations

import typing as t


@t.overload
def locate_app(module_name: str, app_name: str | None, raise_if_not_found: t.Literal[True] = True) -> str: ...


@t.overload
def locate_app(module_name: str, app_name: str | None, raise_if_not_found: t.Literal[False] = ...) -> str | None: ...


def locate_app(module_name, app_name, raise_if_not_found=True):
    return module_name


class ScriptInfo:
    def load_app(self):
        return locate_app("app", None)
"#;

/// `src/flask/sansio/app.py`, cut down.
const APP: &str = r#"from __future__ import annotations

import typing as t


class App:
    @t.overload
    def template_filter(self, name: t.Callable) -> t.Callable: ...

    @t.overload
    def template_filter(self, name: str | None = None) -> t.Callable: ...

    def template_filter(self, name=None):
        return name
"#;

/// A module that imports `locate_app` and calls a method of a subclass.
const USE: &str = r#"from .cli import locate_app
from .sansio.app import App


class Flask(App):
    def setup(self):
        self.template_filter("upper")


def find(name):
    return locate_app(name, None)
"#;

struct Linked {
    entities: Vec<Entity>,
    calls: Vec<(EntityId, EntityId)>,
}

fn linked() -> Linked {
    let pipeline = IndexPipeline::new();
    let mut parsed = Vec::new();
    let mut entities = Vec::new();
    let mut calls = Vec::new();
    for (path, source) in [
        ("src/flask/cli.py", CLI),
        ("src/flask/sansio/app.py", APP),
        ("src/flask/use.py", USE),
    ] {
        let blob = kin_blobs::digest(source.as_bytes());
        let indexed = pipeline
            .index_file_content_with_tests(&FilePathId::new(path), source.as_bytes(), blob)
            .unwrap()
            .indexed_file;
        entities.extend(indexed.entities.iter().cloned());
        calls.extend(
            indexed
                .relations
                .iter()
                .filter(|relation| relation.kind == RelationKind::Calls)
                .filter_map(|relation| {
                    Some((relation.src.as_entity()?, relation.dst.as_entity()?))
                }),
        );
        parsed.push(FileParseData {
            file_path: path.to_string(),
            entities: indexed.entities,
            relations: indexed.extracted_relations,
            imports: indexed.imports,
        });
    }
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
    calls.extend(
        relations
            .iter()
            .filter(|relation| relation.kind == RelationKind::Calls)
            .filter_map(|relation| Some((relation.src.as_entity()?, relation.dst.as_entity()?))),
    );
    Linked { entities, calls }
}

impl Linked {
    fn one(&self, name: &str) -> EntityId {
        let found: Vec<&Entity> = self.entities.iter().filter(|e| e.name == name).collect();
        assert_eq!(found.len(), 1, "one entity named {name}: {found:?}");
        found[0].id
    }

    /// The same-named declarations in source order: the stubs, then the
    /// implementation.
    fn group(&self, name: &str) -> Vec<EntityId> {
        let mut group: Vec<&Entity> = self.entities.iter().filter(|e| e.name == name).collect();
        group.sort_by_key(|entity| entity.span.as_ref().map(|span| span.start_line));
        assert_eq!(group.len(), 3, "two stubs and the implementation of {name}");
        group.into_iter().map(|entity| entity.id).collect()
    }

    fn callees_of(&self, caller: EntityId, among: &[EntityId]) -> Vec<EntityId> {
        let mut callees: Vec<EntityId> = self
            .calls
            .iter()
            .filter(|(src, dst)| *src == caller && among.contains(dst))
            .map(|(_, dst)| *dst)
            .collect();
        callees.sort();
        callees.dedup();
        callees
    }
}

#[test]
fn a_call_of_an_overloaded_function_binds_its_implementation() {
    let linked = linked();
    let group = linked.group("locate_app");
    let implementation = group[2];
    for caller in ["ScriptInfo.load_app", "find"] {
        assert_eq!(
            linked.callees_of(linked.one(caller), &group),
            [implementation],
            "{caller} calls the implementation of locate_app and no stub"
        );
    }
}

#[test]
fn a_call_of_an_overloaded_method_binds_its_implementation() {
    let linked = linked();
    let group = linked.group("App.template_filter");
    assert_eq!(
        linked.callees_of(linked.one("Flask.setup"), &group),
        [group[2]],
        "Flask.setup calls the implementation of App.template_filter and no stub"
    );
}
