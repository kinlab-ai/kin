// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A Python bare name resolves the way Python binds it: the function scopes
//! around the call, then the names the module itself binds, then the builtins.
//! The file's own Module entity is never one of those bindings, and no call
//! reaches a module.
//!
//! `rich/repr.py` calls `repr(arg[0])`. The file's Module entity is named
//! `repr`, and it held the file's `(file, "repr")` slot because nothing the
//! file declares is called `repr`, so the same-file tier proved every one of
//! those calls to the module at parser certainty, and the builtin gate, which
//! only runs for a name the file does not define, never saw them. At run time
//! each call reaches `builtins.repr`.
//!
//! Every fixture goes through the real parser and ingestion pipeline and is
//! checked on all three paths that bind a call: the per-file resolver that
//! settles same-file relations during ingestion, the batch linker and the
//! incremental linker.

use std::collections::HashMap;

use kin_index::{
    link_cross_file, link_cross_file_incremental, FileParseData, IncrementalLinker, IndexPipeline,
    RelationResolution,
};
use kin_model::{ArtifactId, EntityId, EntityKind, FilePathId, Relation, RelationKind};

struct Indexed {
    file: FileParseData,
    /// What the per-file resolver bound during ingestion.
    same_file: Vec<Relation>,
}

fn index(path: &str, source: &str) -> Indexed {
    let indexed = IndexPipeline::new()
        .index_file_content_with_tests(
            &FilePathId::new(path),
            source.as_bytes(),
            kin_blobs::digest(source.as_bytes()),
        )
        .unwrap()
        .indexed_file;
    Indexed {
        same_file: indexed.relations,
        file: FileParseData {
            file_path: path.into(),
            entities: indexed.entities,
            relations: indexed.extracted_relations,
            imports: indexed.imports,
        },
    }
}

/// Every path's relations: the per-file resolver's, then each linker's.
fn paths(indexed: &[Indexed]) -> Vec<(&'static str, Vec<Relation>)> {
    let files: Vec<FileParseData> = indexed.iter().map(|one| one.file.clone()).collect();
    let ids: HashMap<String, ArtifactId> = files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect();
    let mut linker = IncrementalLinker::new();
    for file in &files {
        linker.add_file(&file.file_path, ids[&file.file_path], &file.entities);
    }
    linker.record_class_bases(&files);
    vec![
        (
            "per-file",
            indexed
                .iter()
                .flat_map(|one| one.same_file.iter().cloned())
                .collect(),
        ),
        ("batch", link_cross_file(&files, &ids).unwrap()),
        (
            "incremental",
            link_cross_file_incremental(&files, &linker).unwrap(),
        ),
    ]
}

struct Names {
    by_id: HashMap<EntityId, (String, String, EntityKind)>,
}

impl Names {
    fn of(indexed: &[Indexed]) -> Self {
        Names {
            by_id: indexed
                .iter()
                .flat_map(|one| {
                    one.file.entities.iter().map(|entity| {
                        (
                            entity.id,
                            (one.file.file_path.clone(), entity.name.clone(), entity.kind),
                        )
                    })
                })
                .collect(),
        }
    }

    fn id(&self, file: &str, name: &str) -> EntityId {
        self.by_id
            .iter()
            .find(|(_, (path, entity, kind))| {
                path == file && entity == name && *kind != EntityKind::Module
            })
            .map(|(id, _)| *id)
            .unwrap_or_else(|| panic!("no declaration {name} in {file}"))
    }

    /// Every `kind` edge out of `caller`, as `file:name` of its destination.
    fn targets(&self, relations: &[Relation], kind: RelationKind, caller: EntityId) -> Vec<String> {
        let mut out: Vec<String> = relations
            .iter()
            .filter(|relation| relation.kind == kind && relation.src.as_entity() == Some(caller))
            .filter_map(|relation| self.by_id.get(&relation.dst.as_entity()?))
            .map(|(file, name, _)| format!("{file}:{name}"))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Every `Calls` edge, on any path, whose destination is a Module.
    fn module_calls(&self, relations: &[Relation]) -> Vec<String> {
        relations
            .iter()
            .filter(|relation| relation.kind == RelationKind::Calls)
            .filter_map(|relation| {
                let (file, name, kind) = self.by_id.get(&relation.dst.as_entity()?)?;
                let (src_file, src, _) = self.by_id.get(&relation.src.as_entity()?)?;
                (*kind == EntityKind::Module).then(|| format!("{src_file}:{src} -> {file}:{name}"))
            })
            .collect()
    }
}

/// The shape of `rich/repr.py` at the commit the runtime trace ran against:
/// the builtin is called three times from a closure two functions deep, and a
/// value read of the builtin sits beside it.
const RICH_REPR: &str = "\
from typing import Any, Iterable, List


def auto(cls):
    def do_replace(cls):
        def auto_repr(self):
            repr_str: List[str] = []
            append = repr_str.append
            for arg in self.__rich_repr__():
                if isinstance(arg, tuple):
                    if len(arg) == 1:
                        append(repr(arg[0]))
                    else:
                        key, value, *default = arg
                        if key is None:
                            append(repr(value))
                else:
                    append(repr(arg))
            return ', '.join(repr_str)

        cls.__repr__ = auto_repr
        return cls

    return do_replace(cls)


def reprs(items: Iterable[Any]) -> List[str]:
    return list(map(repr, items))
";

#[test]
fn a_builtin_called_inside_a_module_named_for_it_reaches_no_module() {
    let indexed = vec![
        index("rich/repr.py", RICH_REPR),
        // A declaration elsewhere that shares the builtin's name, which the
        // name tiers must not settle an unimported builtin call on either.
        index("rich/pretty.py", "def repr(obj):\n    return str(obj)\n"),
    ];
    let names = Names::of(&indexed);
    let auto_repr = names.id(
        "rich/repr.py",
        "auto.<locals>.do_replace.<locals>.auto_repr",
    );
    let reprs = names.id("rich/repr.py", "reprs");
    for (path, relations) in paths(&indexed) {
        assert_eq!(
            names.targets(&relations, RelationKind::Calls, auto_repr),
            Vec::<String>::new(),
            "{path}: `repr(...)` is the builtin and `append(...)` a local value, so \
             neither has a destination the repository declares"
        );
        assert_eq!(
            names.targets(&relations, RelationKind::References, reprs),
            Vec::<String>::new(),
            "{path}: a value read of the builtin references nothing here"
        );
        assert_eq!(
            names.module_calls(&relations),
            Vec::<String>::new(),
            "{path}"
        );
    }
}

/// A module that declares the name it is called by is the negative case: the
/// declaration takes the slot, and a call written in the module reaches it.
#[test]
fn a_module_that_rebinds_the_builtin_keeps_the_call_on_its_declaration() {
    let indexed = vec![index(
        "pkg/format.py",
        "def format(value, spec=\"\"):\n    return f\"<{value}>\"\n\n\ndef render(x):\n    return format(x)\n",
    )];
    let names = Names::of(&indexed);
    let render = names.id("pkg/format.py", "render");
    for (path, relations) in paths(&indexed) {
        let edges: Vec<&Relation> = relations
            .iter()
            .filter(|relation| {
                relation.kind == RelationKind::Calls && relation.src.as_entity() == Some(render)
            })
            .collect();
        assert_eq!(edges.len(), 1, "{path}: {edges:?}");
        assert_eq!(
            edges[0].dst.as_entity(),
            Some(names.id("pkg/format.py", "format")),
            "{path}"
        );
        assert_eq!(
            RelationResolution::of(edges[0]),
            RelationResolution::TypeResolved,
            "{path}: the module's own declaration is proven"
        );
    }
}

/// An import of a name shaped like the importing module's own name reaches
/// the import, with no candidate edge left on the module.
#[test]
fn an_import_named_like_its_module_reaches_the_import() {
    let indexed = vec![
        index("pkg/other.py", "def helper():\n    return 1\n"),
        index(
            "pkg/helper.py",
            "from .other import helper\n\n\ndef run():\n    return helper()\n",
        ),
    ];
    let names = Names::of(&indexed);
    let run = names.id("pkg/helper.py", "run");
    for (path, relations) in paths(&indexed).into_iter().skip(1) {
        assert_eq!(
            names.targets(&relations, RelationKind::Calls, run),
            vec!["pkg/other.py:helper".to_string()],
            "{path}"
        );
    }
}

/// A module that binds the imported name by a plain assignment admits no
/// entity for it, so the lookup inside it meets only the module the file is
/// named for. The call reaches a value the graph does not hold and stays
/// unresolved, rather than being proven to the module.
#[test]
fn a_name_its_module_binds_by_assignment_is_left_unresolved() {
    let indexed = vec![
        index("pkg/shim.py", "def _impl():\n    return 1\n\n\nshim = _impl\n"),
        index(
            "pkg/caller.py",
            "import pkg.shim\nfrom .shim import shim\n\n\ndef call_imported():\n    return shim()\n\n\ndef call_through_module():\n    return pkg.shim.shim()\n",
        ),
    ];
    let names = Names::of(&indexed);
    for (path, relations) in paths(&indexed) {
        assert_eq!(
            names.module_calls(&relations),
            Vec::<String>::new(),
            "{path}"
        );
        for caller in ["call_imported", "call_through_module"] {
            let id = names.id("pkg/caller.py", caller);
            assert_eq!(
                names.targets(&relations, RelationKind::Calls, id),
                Vec::<String>::new(),
                "{path}: {caller}"
            );
        }
    }
}

const SCOPING: &str = "\
def helper():
    return 1


def with_param(helper):
    return helper()


def with_local():
    helper = make()
    return helper()


def with_loop(fns):
    for helper in fns:
        helper()


def enclosing():
    helper = make()

    def inner():
        return helper()

    return inner()


def with_import():
    from .other import helper

    return helper()


def with_global():
    global helper
    return helper()


def plain():
    return helper()


def make():
    return len
";

/// Python binds a name in the innermost function scope that assigns it. A
/// parameter, a local, a loop variable and an enclosing function's local all
/// hide the module's `def helper`, and a function-local import names another
/// module's. Each of these calls was proven to the module-level function.
#[test]
fn a_call_through_a_function_scope_binding_reaches_no_module_level_declaration() {
    let indexed = vec![
        index("pkg/scoping.py", SCOPING),
        index("pkg/other.py", "def helper():\n    return 2\n"),
    ];
    let names = Names::of(&indexed);
    let local_helper = "pkg/scoping.py:helper".to_string();
    for (path, relations) in paths(&indexed) {
        for caller in [
            "with_param",
            "with_local",
            "with_loop",
            "enclosing.<locals>.inner",
        ] {
            let id = names.id("pkg/scoping.py", caller);
            let targets = names.targets(&relations, RelationKind::Calls, id);
            assert!(
                !targets.contains(&local_helper),
                "{path}: {caller} calls a local value, got {targets:?}"
            );
        }
        for caller in ["with_global", "plain"] {
            let id = names.id("pkg/scoping.py", caller);
            assert_eq!(
                names.targets(&relations, RelationKind::Calls, id),
                vec![local_helper.clone()],
                "{path}: {caller} reads the module's own binding"
            );
        }
        let with_import = names.id("pkg/scoping.py", "with_import");
        let targets = names.targets(&relations, RelationKind::Calls, with_import);
        if path != "per-file" {
            assert!(
                targets.contains(&"pkg/other.py:helper".to_string()),
                "{path}: the local import names other.helper, got {targets:?}"
            );
        }
        let proven_local = relations.iter().any(|relation| {
            relation.kind == RelationKind::Calls
                && relation.src.as_entity() == Some(with_import)
                && relation.dst.as_entity() == Some(names.id("pkg/scoping.py", "helper"))
                && RelationResolution::of(relation).is_proven()
        });
        assert!(
            !proven_local,
            "{path}: a function-local import is never proven to the module-level declaration"
        );
    }
}
