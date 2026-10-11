// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Python namespace calls reuse source-bound exact import authority. A carrier
//! refused by source, binding or package evidence cannot fall back to names.

use kin_index::{FileParseData, IncrementalLinker, IndexPipeline};
use kin_model::{ArtifactId, EntityId, EntityKind, FilePathId, Relation, RelationKind};
use std::collections::HashMap;

fn parse(path: &str, source: &str) -> (FileParseData, Vec<Relation>) {
    let indexed = IndexPipeline::new()
        .index_file_content_with_tests(
            &FilePathId::new(path),
            source.as_bytes(),
            kin_blobs::digest(source.as_bytes()),
        )
        .unwrap()
        .indexed_file;
    let local = indexed.relations;
    (
        FileParseData {
            file_path: path.into(),
            entities: indexed.entities,
            relations: indexed.extracted_relations,
            imports: indexed.imports,
        },
        local,
    )
}

fn fixture(sources: &[(&str, &str)]) -> (Vec<FileParseData>, Vec<Relation>) {
    let (files, local): (Vec<_>, Vec<_>) = sources
        .iter()
        .map(|(path, source)| parse(path, source))
        .unzip();
    (files, local.into_iter().flatten().collect())
}

fn id(files: &[FileParseData], path: &str, name: &str, kind: EntityKind) -> EntityId {
    files
        .iter()
        .find(|file| file.file_path == path)
        .unwrap()
        .entities
        .iter()
        .find(|entity| entity.name == name && entity.kind == kind)
        .unwrap()
        .id
}

fn routes(files: &[FileParseData], local: &[Relation]) -> Vec<(&'static str, Vec<Relation>)> {
    let artifacts: HashMap<_, _> = files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect();
    let mut linker = IncrementalLinker::new();
    for file in files {
        linker.add_file(&file.file_path, artifacts[&file.file_path], &file.entities);
    }
    let checkpoint = serde_json::to_vec(&linker.to_checkpoint_v1()).unwrap();
    let restored =
        IncrementalLinker::from_checkpoint_v1(serde_json::from_slice(&checkpoint).unwrap())
            .unwrap();
    [
        (
            "batch",
            kin_index::link_cross_file(files, &artifacts).unwrap(),
        ),
        (
            "incremental",
            kin_index::link_cross_file_incremental(files, &linker).unwrap(),
        ),
        (
            "checkpoint",
            kin_index::link_cross_file_incremental(files, &restored).unwrap(),
        ),
    ]
    .into_iter()
    .map(|(route, mut relations)| {
        relations.extend_from_slice(local);
        (route, relations)
    })
    .collect()
}

fn assert_target(files: &[FileParseData], local: &[Relation], caller: EntityId, target: EntityId) {
    for (route, relations) in routes(files, local) {
        let calls: Vec<_> = relations
            .iter()
            .filter(|edge| edge.src.as_entity() == Some(caller) && edge.kind == RelationKind::Calls)
            .collect();
        assert_eq!(calls.len(), 1, "{route}: {calls:?}");
        assert_eq!(calls[0].dst.as_entity(), Some(target), "{route}: {calls:?}");
        assert!(
            calls[0].confidence >= 0.95,
            "{route}: exact binding must be proven"
        );
        assert!(
            !kin_index::occurrence::static_proof_sites(calls[0]).is_empty(),
            "{route}: exact occurrence backing must qualify"
        );
    }
}

fn assert_gap(files: &[FileParseData], local: &[Relation], caller: EntityId) {
    for (route, relations) in routes(files, local) {
        let calls: Vec<_> = relations
            .iter()
            .filter(|edge| edge.src.as_entity() == Some(caller) && edge.kind == RelationKind::Calls)
            .collect();
        assert!(
            calls.is_empty(),
            "{route}: refused source authority must remain a gap: {calls:?}"
        );
    }
}

const IMPLEMENTATION: &str = "def get():\n    return 'selected'\n";

#[test]
fn a_namespace_call_follows_an_ordinary_package_export_from_a_method() {
    let (files, local) = fixture(&[
        ("app.py", "import pkg\nclass Test:\n    def run(self):\n        return pkg.get()\n"),
        ("pkg/__init__.py", "VERSION = '1'\nif TYPE_CHECKING:\n    from foreign import Type\nfrom .api import get\n"),
        ("pkg/api.py", "if TYPE_CHECKING:\n    from foreign import Type\ndef get():\n    return 'selected'\n"),
        ("elsewhere.py", "def get():\n    return 'decoy'\n"),
    ]);
    assert_target(
        &files,
        &local,
        id(&files, "app.py", "Test.run", EntityKind::Method),
        id(&files, "pkg/api.py", "get", EntityKind::Function),
    );
}

#[test]
fn a_dotted_namespace_alias_and_reexport_aliases_keep_the_nested_caller() {
    let (files, local) = fixture(&[
        ("app.py", "import pkg.bridge as api\ndef outer():\n    def run():\n        return api.public()\n    return run()\n"),
        ("pkg/__init__.py", "VERSION = 1\n"),
        ("pkg/bridge.py", "from .api import get as public\ndef unrelated():\n    return 1\n"),
        ("pkg/api.py", IMPLEMENTATION),
    ]);
    assert_target(
        &files,
        &local,
        id(&files, "app.py", "outer.<locals>.run", EntityKind::Function),
        id(&files, "pkg/api.py", "get", EntityKind::Function),
    );
}

#[test]
fn an_unaliased_dotted_namespace_uses_the_same_exact_chain() {
    let (files, local) = fixture(&[
        (
            "app.py",
            "import pkg.bridge\ndef run():\n    return pkg.bridge.get()\n",
        ),
        ("pkg/__init__.py", "VERSION = 1\n"),
        ("pkg/bridge.py", IMPLEMENTATION),
    ]);
    assert_target(
        &files,
        &local,
        id(&files, "app.py", "run", EntityKind::Function),
        id(&files, "pkg/bridge.py", "get", EntityKind::Function),
    );
}

#[test]
fn an_escaped_dotted_namespace_keeps_its_member_calls_unproved() {
    let (files, local) = fixture(&[
        ("app.py", "import pkg.bridge\nalias = pkg.bridge\nalias.get = replacement\ndef run():\n    return pkg.bridge.get()\n"),
        ("pkg/__init__.py", "VERSION = 1\n"),
        ("pkg/bridge.py", IMPLEMENTATION),
    ]);
    assert_gap(
        &files,
        &local,
        id(&files, "app.py", "run", EntityKind::Function),
    );
}

#[test]
fn a_shadowed_parent_child_is_not_the_dotted_imported_module() {
    for package in [
        "bridge = replacement\n",
        "from other import bridge\n",
        "if enabled:\n    bridge = replacement\n",
        "import pkg\npkg.bridge = replacement\n",
    ] {
        for import in ["import pkg.bridge as api", "import pkg.bridge"] {
            let receiver = if import.ends_with("api") {
                "api"
            } else {
                "pkg.bridge"
            };
            let source = format!("{import}\ndef run():\n    return {receiver}.get()\n");
            let (files, local) = fixture(&[
                ("app.py", &source),
                ("pkg/__init__.py", package),
                ("pkg/bridge.py", IMPLEMENTATION),
            ]);
            assert_gap(
                &files,
                &local,
                id(&files, "app.py", "run", EntityKind::Function),
            );
        }
    }
}

#[test]
fn an_unrelated_namespace_member_mutation_does_not_erase_the_exact_binding() {
    let (files, local) = fixture(&[
        ("app.py", "import pkg\ndef replace_other():\n    pkg.helpers.value = replacement\ndef run():\n    return pkg.get()\n"),
        ("pkg/__init__.py", "from .api import get\n"),
        ("pkg/api.py", IMPLEMENTATION),
    ]);
    assert_target(
        &files,
        &local,
        id(&files, "app.py", "run", EntityKind::Function),
        id(&files, "pkg/api.py", "get", EntityKind::Function),
    );
}

#[test]
fn a_namespace_shadow_is_not_the_imported_package() {
    for body in [
        "def run(pkg):\n    return pkg.get()\n",
        "def run():\n    pkg = replacement\n    return pkg.get()\n",
        "def run():\n    for pkg in values:\n        return pkg.get()\n",
        "def run():\n    return [pkg.get() for pkg in values]\n",
        "def outer(pkg):\n    def run():\n        return pkg.get()\n    return run()\n",
    ] {
        let source = format!("import pkg\n{body}");
        let (files, local) = fixture(&[
            ("app.py", &source),
            ("pkg/__init__.py", "from .api import get\n"),
            ("pkg/api.py", IMPLEMENTATION),
        ]);
        let caller = if body.starts_with("def outer") {
            "outer.<locals>.run"
        } else {
            "run"
        };
        assert_gap(
            &files,
            &local,
            id(&files, "app.py", caller, EntityKind::Function),
        );
    }
}

#[test]
fn namespace_rebinding_or_mutation_refuses_the_exact_call() {
    for change in [
        "pkg = replacement\n",
        "if enabled:\n    pkg = replacement\n",
        "pkg.get = replacement\n",
        "pkg.get.attribute = replacement\n",
        "del pkg.get\n",
        "setattr(pkg, 'get', replacement)\n",
        "monkeypatch.setattr(pkg, 'get', replacement)\n",
        "pkg.__setattr__('get', replacement)\n",
        "pkg.__delattr__('get')\n",
        "setter = pkg.__setattr__\nsetter('get', replacement)\n",
        "setter = setattr\nsetter(pkg, 'get', replacement)\n",
        "alias = pkg\nalias.get = replacement\n",
        "mutate(pkg)\n",
        "type pkg = Replacement\n",
        "import sys\nsys.modules[__name__].pkg = replacement\n",
        "from sys import modules as registry\nregistry[__name__].pkg = replacement\n",
        "from importlib import import_module as load\nload(__name__).pkg = replacement\n",
        "globals()['pkg'] = replacement\n",
        "from other import *\n",
    ] {
        let source = format!("import pkg\n{change}def run():\n    return pkg.get()\n");
        let (files, local) = fixture(&[
            ("app.py", &source),
            ("pkg/__init__.py", "from .api import get\n"),
            ("pkg/api.py", IMPLEMENTATION),
        ]);
        assert_gap(
            &files,
            &local,
            id(&files, "app.py", "run", EntityKind::Function),
        );
    }
}

fn assert_binding_target_mutation_gap(change: &str) {
    let source = format!("import pkg\n{change}def run():\n    return pkg.get()\n");
    assert_valid_python(&source);
    let (files, local) = fixture(&[
        ("app.py", &source),
        ("pkg/__init__.py", "from .api import get\n"),
        ("pkg/api.py", IMPLEMENTATION),
    ]);
    assert_gap(
        &files,
        &local,
        id(&files, "app.py", "run", EntityKind::Function),
    );
}

fn assert_valid_python(source: &str) {
    use kin_parser::LanguageAdapter;
    let tree = kin_parser::PythonAdapter.parse(source.as_bytes()).unwrap();
    assert!(!tree.root_node().has_error(), "invalid fixture: {source}");
}

#[test]
fn binding_target_for_member_mutation_refuses_the_exact_call() {
    for change in [
        "for pkg.get in handlers:\n    pass\n",
        "for other, pkg.get in handlers:\n    pass\n",
        "for *pkg.get, other in handlers:\n    pass\n",
        "for pkg.get.attribute in handlers:\n    pass\n",
        "for pkg['get'] in handlers:\n    pass\n",
        "[None for pkg.get in handlers]\n",
        "{None for pkg.get in handlers}\n",
        "{1: None for pkg.get in handlers}\n",
        "(None for pkg.get in handlers)\n",
        "async def mutate():\n    async for pkg.get in handlers:\n        pass\n",
    ] {
        assert_binding_target_mutation_gap(change);
    }
}

#[test]
fn binding_target_with_member_mutation_refuses_the_exact_call() {
    for change in [
        "with manager as pkg.get:\n    pass\n",
        "with first as other, second as pkg.get:\n    pass\n",
        "with manager as (other, pkg.get):\n    pass\n",
        "with manager as pkg.get.attribute:\n    pass\n",
        "with manager as pkg['get']:\n    pass\n",
        "async def mutate():\n    async with manager as pkg.get:\n        pass\n",
    ] {
        assert_binding_target_mutation_gap(change);
    }
}

#[test]
fn binding_target_multi_delete_mutation_refuses_the_exact_call() {
    assert_binding_target_mutation_gap("del spare, pkg.get\n");
}

#[test]
fn binding_target_unrelated_member_writes_keep_the_exact_call() {
    for change in [
        "for pkg.other in handlers:\n    pass\n",
        "for pkg.getter in handlers:\n    pass\n",
        "for other, pkg.other.attribute in handlers:\n    pass\n",
        "[None for pkg.other in handlers]\n",
        "with manager as pkg.other:\n    pass\n",
        "with first as other, second as pkg.other.attribute:\n    pass\n",
    ] {
        let source = format!("import pkg\n{change}def run():\n    return pkg.get()\n");
        assert_valid_python(&source);
        let (files, local) = fixture(&[
            ("app.py", &source),
            ("pkg/__init__.py", "from .api import get\n"),
            ("pkg/api.py", IMPLEMENTATION),
        ]);
        assert_target(
            &files,
            &local,
            id(&files, "app.py", "run", EntityKind::Function),
            id(&files, "pkg/api.py", "get", EntityKind::Function),
        );
    }
}

#[test]
fn a_refused_source_budget_cannot_revive_the_legacy_module_tier() {
    let source = format!(
        "import pkg\npkg = replacement\n#{}\ndef run():\n    return pkg.get()\n",
        "x".repeat(256 * 1024)
    );
    let (files, local) = fixture(&[("app.py", &source), ("pkg/__init__.py", IMPLEMENTATION)]);
    assert_gap(
        &files,
        &local,
        id(&files, "app.py", "run", EntityKind::Function),
    );
}

#[test]
fn a_rebound_or_guarded_export_cannot_capture_a_module_twin_or_sibling() {
    for package in [
        "if enabled:\n    from .api import get\n",
        "from .api import get\nget = None\n",
        "from .api import get\nif enabled:\n    get = replacement\n",
        "from .api import get\nfrom .decoy import get\n",
        "from .bridge import get\n",
    ] {
        let (files, local) = fixture(&[
            ("app.py", "import pkg\ndef run():\n    return pkg.get()\n"),
            ("pkg/__init__.py", package),
            ("pkg/api.py", IMPLEMENTATION),
            ("pkg/bridge.py", "from pkg import get\n"),
            ("pkg/get.py", IMPLEMENTATION),
            ("pkg/decoy.py", IMPLEMENTATION),
        ]);
        assert_gap(
            &files,
            &local,
            id(&files, "app.py", "run", EntityKind::Function),
        );
    }
}

#[test]
fn a_rebound_terminal_declaration_does_not_keep_its_old_proof() {
    let (files, local) = fixture(&[
        ("app.py", "import pkg\ndef run():\n    return pkg.get()\n"),
        ("pkg/__init__.py", "from .api import get\n"),
        (
            "pkg/api.py",
            "def get():\n    return 1\nget = replacement\n",
        ),
    ]);
    assert_gap(
        &files,
        &local,
        id(&files, "app.py", "run", EntityKind::Function),
    );
}

#[test]
fn a_missing_or_unbound_carrier_cannot_revive_the_legacy_receiver_tier() {
    for tamper_source in [false, true] {
        let (mut files, local) = fixture(&[
            ("app.py", "import pkg\ndef run():\n    return pkg.get()\n"),
            ("pkg/__init__.py", IMPLEMENTATION),
        ]);
        if tamper_source {
            for entity in &mut files[0].entities {
                entity.metadata.extra.remove("blob_hash");
            }
        } else {
            files[0]
                .relations
                .retain(|raw| !kin_parser::import_witness::claims_import_witness(raw));
        }
        assert_gap(
            &files,
            &local,
            id(&files, "app.py", "run", EntityKind::Function),
        );
    }
}

#[test]
fn an_unimported_namespace_attribute_is_not_a_package_function() {
    let (files, local) = fixture(&[
        (
            "app.py",
            "import pkg\ndef run():\n    return pkg.unknown.get()\n",
        ),
        ("pkg/__init__.py", IMPLEMENTATION),
    ]);
    assert_gap(
        &files,
        &local,
        id(&files, "app.py", "run", EntityKind::Function),
    );
}
