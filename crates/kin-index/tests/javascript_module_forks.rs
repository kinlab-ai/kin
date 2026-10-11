// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! An ECMAScript import that names a module which does not declare the name
//! itself: re-export chains, and module fork families.
//!
//! A pinned import that misses in its module blocks every name fallback,
//! because binding it to a same-named symbol elsewhere mints a false consumer.
//! Two shapes still name their destination in source and are followed here:
//!
//! - `export * from './x'` and `export { a } from './x'` hand a name on to the
//!   module that declares it. Reaching exactly one declaration is an ordinary
//!   pinned resolution.
//! - A fork stub is a module that declares nothing its importers ask for while
//!   a sibling `forks/` directory holds `<stub>.<variant>.<ext>` files that a
//!   build aliases it to. The importer is linked to the declaration each
//!   variant reaches, at the module fork tier, with evidence naming the stub
//!   and the variant. The set is open, so it never reads as `type_resolved`.
//!
//! Every fixture runs through the real parser and through the batch linker,
//! the incremental linker, a restored checkpoint, and a restored checkpoint
//! relinking only the importer, which must read the re-exports the linker
//! retained for files it did not reparse.

use std::collections::HashMap;

use kin_index::resolution::{RelationResolution, MODULE_FORK_CONFIDENCE};
use kin_index::{FileParseData, IncrementalLinker, IndexPipeline, MODULE_FORK_EVIDENCE_RULE};
use kin_model::{ArtifactId, EntityId, FilePathId, Relation, RelationKind, RelationOrigin};

/// The tier a single declaration reached through the importer's own pin gets.
const IMPORT_PINNED_CONFIDENCE: f32 = 0.9;

fn parse(path: &str, source: &str) -> FileParseData {
    let indexed = IndexPipeline::new()
        .index_file_content_with_tests(
            &FilePathId::new(path),
            source.as_bytes(),
            kin_blobs::digest(source.as_bytes()),
        )
        .expect("fixture source indexes")
        .indexed_file;
    FileParseData {
        file_path: path.into(),
        entities: indexed.entities,
        relations: indexed.extracted_relations,
        imports: indexed.imports,
    }
}

fn parse_all(sources: &[(&str, &str)]) -> Vec<FileParseData> {
    sources
        .iter()
        .map(|(path, source)| parse(path, source))
        .collect()
}

fn entity(files: &[FileParseData], path: &str, name: &str) -> EntityId {
    files
        .iter()
        .find(|file| file.file_path == path)
        .unwrap_or_else(|| panic!("missing file {path}"))
        .entities
        .iter()
        .find(|entity| entity.name == name)
        .unwrap_or_else(|| panic!("missing entity {path}:{name}"))
        .id
}

/// Every linking path a store can take, each over the same admitted parses.
fn routes(files: &[FileParseData], importers: &[&str]) -> Vec<(&'static str, Vec<Relation>)> {
    let artifacts: HashMap<_, _> = files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect();
    let mut linker = IncrementalLinker::new();
    for file in files {
        linker.add_file(&file.file_path, artifacts[&file.file_path], &file.entities);
    }
    linker.record_class_bases(files);
    let checkpoint = serde_json::to_vec(&linker.to_checkpoint_v1()).unwrap();
    let restored =
        IncrementalLinker::from_checkpoint_v1(serde_json::from_slice(&checkpoint).unwrap())
            .unwrap();
    let importer_files: Vec<FileParseData> = files
        .iter()
        .filter(|file| importers.contains(&file.file_path.as_str()))
        .cloned()
        .collect();
    assert_eq!(
        importer_files.len(),
        importers.len(),
        "every importer is a fixture file"
    );
    vec![
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
        (
            "checkpoint, importer only",
            kin_index::link_cross_file_incremental(&importer_files, &restored).unwrap(),
        ),
    ]
}

fn calls_from(relations: &[Relation], src: EntityId) -> Vec<&Relation> {
    relations
        .iter()
        .filter(|edge| edge.kind == RelationKind::Calls && edge.src.as_entity() == Some(src))
        .collect()
}

fn fork_records(edge: &Relation) -> Vec<(Option<&str>, Option<&str>)> {
    edge.evidence
        .iter()
        .filter(|record| record.parser_rule.as_deref() == Some(MODULE_FORK_EVIDENCE_RULE))
        .map(|record| {
            (
                record.source_path.as_deref(),
                record.resolved_path.as_deref(),
            )
        })
        .collect()
}

fn spanned_records(edge: &Relation) -> usize {
    edge.evidence
        .iter()
        .filter(|record| record.source_span.is_some())
        .count()
}

const THROWING_STUB: &str =
    "throw new Error('This module must be shimmed by a specific renderer.');\n";

const WORK: &str = "import {f} from './Config';\n\nexport function g() {\n  f();\n}\n";

/// A host-config fork family in the shape a renderer build aliases: a stub
/// that throws, two variants that hand a renderer's declarations on through
/// `export *`, a third that reaches the same declaration as the first, and a
/// variant whose export is a value read off a configuration object the build
/// passes in.
fn fork_family() -> Vec<FileParseData> {
    parse_all(&[
        ("src/Config.js", THROWING_STUB),
        (
            "src/forks/Config.dom.js",
            "export * from '../dom/ConfigDOM';\nexport * from '../shared/Console';\n",
        ),
        (
            "src/forks/Config.markup.js",
            "export * from '../dom/ConfigDOM';\nthrow new Error('Not used in markup');\n",
        ),
        (
            "src/forks/Config.art.js",
            "export * from '../art/ConfigART';\n",
        ),
        (
            "src/forks/Config.custom.js",
            "export const f = $$$config.f;\nexport const other = $$$config.other;\n",
        ),
        ("src/dom/ConfigDOM.js", "export function f() {}\n"),
        ("src/art/ConfigART.js", "export function f() {}\n"),
        ("src/shared/Console.js", "export function log() {}\n"),
        ("src/Work.js", WORK),
    ])
}

#[test]
fn an_import_from_a_fork_stub_calls_every_member_the_variants_reach() {
    let files = fork_family();
    let caller = entity(&files, "src/Work.js", "g");
    let dom = entity(&files, "src/dom/ConfigDOM.js", "f");
    let art = entity(&files, "src/art/ConfigART.js", "f");
    assert!(
        files
            .iter()
            .find(|file| file.file_path == "src/forks/Config.custom.js")
            .unwrap()
            .entities
            .iter()
            .all(|entity| entity.name != "f"),
        "the configuration-object variant declares no `f` of its own, so it contributes no member"
    );
    assert!(
        files
            .iter()
            .find(|file| file.file_path == "src/Work.js")
            .unwrap()
            .relations
            .iter()
            .any(|relation| relation.src_name == "g"
                && relation.dst_name == "f"
                && relation.import_source.as_deref() == Some("./Config")),
        "the call carries the import pin that forbids blind name fallback"
    );

    for (route, relations) in routes(&files, &["src/Work.js"]) {
        let calls = calls_from(&relations, caller);
        let mut destinations: Vec<_> = calls.iter().map(|edge| edge.dst.as_entity()).collect();
        destinations.sort();
        let mut expected = vec![Some(dom), Some(art)];
        expected.sort();
        assert_eq!(destinations, expected, "{route}: {calls:#?}");

        for edge in &calls {
            assert_eq!(
                edge.confidence.to_bits(),
                MODULE_FORK_CONFIDENCE.to_bits(),
                "{route}: {edge:#?}"
            );
            assert_eq!(edge.origin, RelationOrigin::Inferred, "{route}");
            let resolution = RelationResolution::of(edge);
            assert_eq!(resolution, RelationResolution::ImportScoped, "{route}");
            assert_ne!(resolution, RelationResolution::TypeResolved, "{route}");
            assert_eq!(
                spanned_records(edge),
                1,
                "{route}: one call site, one occurrence, however many variants reach it: {edge:#?}"
            );
            let proofs = kin_model::parser_occurrence::validated_proofs(edge)
                .unwrap_or_else(|()| panic!("{route}: occurrence certificate is valid: {edge:#?}"));
            assert_eq!(
                proofs.len(),
                1,
                "{route}: the site is certified at its own tier"
            );
            assert!(
                proofs
                    .values()
                    .all(|proof| proof.confidence.to_bits() == MODULE_FORK_CONFIDENCE.to_bits()),
                "{route}: {proofs:?}"
            );
        }

        let dom_edge = calls
            .iter()
            .find(|edge| edge.dst.as_entity() == Some(dom))
            .unwrap();
        assert_eq!(
            fork_records(dom_edge),
            vec![
                (Some("src/Config.js"), Some("src/forks/Config.dom.js")),
                (Some("src/Config.js"), Some("src/forks/Config.markup.js")),
            ],
            "{route}: the evidence names the stub and every variant that reaches the member"
        );
        let art_edge = calls
            .iter()
            .find(|edge| edge.dst.as_entity() == Some(art))
            .unwrap();
        assert_eq!(
            fork_records(art_edge),
            vec![(Some("src/Config.js"), Some("src/forks/Config.art.js"))],
            "{route}"
        );
    }
}

/// React's Fizz host config at the benchmark tree: the DOM variant re-exports
/// its renderer by a package path, the markup variant by the same kind of path
/// with the extension written. Both are implementations a call through the
/// stub reaches, so a caller of one is a caller of the other.
#[test]
fn a_variant_that_reexports_a_package_path_written_with_its_extension_is_a_member() {
    let files = parse_all(&[
        (
            "packages/react-server/src/ReactFizzConfig.js",
            THROWING_STUB,
        ),
        (
            "packages/react-server/src/forks/ReactFizzConfig.dom.js",
            "export * from 'react-dom-bindings/src/server/ReactFizzConfigDOM';\n",
        ),
        (
            "packages/react-server/src/forks/ReactFizzConfig.markup.js",
            "export * from 'react-markup/src/ReactFizzConfigMarkup.js';\n",
        ),
        (
            "packages/react-dom-bindings/src/server/ReactFizzConfigDOM.js",
            "export function pushSegmentFinale(target) {}\n",
        ),
        (
            "packages/react-markup/src/ReactFizzConfigMarkup.js",
            "export function pushSegmentFinale(target) {}\n",
        ),
        (
            "packages/react-server/src/ReactFizzServer.js",
            "import {pushSegmentFinale} from './ReactFizzConfig';\n\n\
             export function renderPreamble(segment) {\n  pushSegmentFinale(segment.chunks);\n}\n",
        ),
    ]);
    let server = "packages/react-server/src/ReactFizzServer.js";
    let caller = entity(&files, server, "renderPreamble");
    let dom = entity(
        &files,
        "packages/react-dom-bindings/src/server/ReactFizzConfigDOM.js",
        "pushSegmentFinale",
    );
    let markup = entity(
        &files,
        "packages/react-markup/src/ReactFizzConfigMarkup.js",
        "pushSegmentFinale",
    );

    for (route, relations) in routes(&files, &[server]) {
        let calls = calls_from(&relations, caller);
        let mut destinations: Vec<_> = calls.iter().map(|edge| edge.dst.as_entity()).collect();
        destinations.sort();
        let mut expected = vec![Some(dom), Some(markup)];
        expected.sort();
        assert_eq!(destinations, expected, "{route}: {calls:#?}");
        let markup_edge = calls
            .iter()
            .find(|edge| edge.dst.as_entity() == Some(markup))
            .unwrap();
        assert_eq!(
            fork_records(markup_edge),
            vec![(
                Some("packages/react-server/src/ReactFizzConfig.js"),
                Some("packages/react-server/src/forks/ReactFizzConfig.markup.js"),
            )],
            "{route}"
        );
    }
}

#[test]
fn a_stub_that_declares_the_name_resolves_normally_and_never_forks() {
    let mut files = fork_family();
    files[0] = parse("src/Config.js", "export function f() {}\n");
    let caller = entity(&files, "src/Work.js", "g");
    let own = entity(&files, "src/Config.js", "f");

    for (route, relations) in routes(&files, &["src/Work.js"]) {
        let calls = calls_from(&relations, caller);
        assert_eq!(calls.len(), 1, "{route}: {calls:#?}");
        assert_eq!(calls[0].dst.as_entity(), Some(own), "{route}");
        assert!(
            fork_records(calls[0]).is_empty(),
            "{route}: {:#?}",
            calls[0]
        );
        assert_ne!(
            calls[0].confidence.to_bits(),
            MODULE_FORK_CONFIDENCE.to_bits(),
            "{route}"
        );
    }
}

#[test]
fn a_stub_without_a_forks_directory_keeps_its_pinned_miss() {
    // The same stub and the same importer, with the renderers' declarations
    // still in the repository but no `forks/` family naming them. The pin must
    // keep blocking the name fallback that would bind `f` to either one.
    let files = parse_all(&[
        ("src/Config.js", THROWING_STUB),
        ("src/dom/ConfigDOM.js", "export function f() {}\n"),
        ("src/art/ConfigART.js", "export function f() {}\n"),
        ("src/Work.js", WORK),
    ]);
    let caller = entity(&files, "src/Work.js", "g");

    for (route, relations) in routes(&files, &["src/Work.js"]) {
        let calls = calls_from(&relations, caller);
        assert!(calls.is_empty(), "{route}: {calls:#?}");
    }
}

#[test]
fn a_forks_directory_beside_a_different_module_is_not_this_stubs_family() {
    let files = parse_all(&[
        ("src/Config.js", THROWING_STUB),
        (
            "src/forks/Other.dom.js",
            "export * from '../dom/ConfigDOM';\n",
        ),
        ("src/forks/Config.js", "export * from '../dom/ConfigDOM';\n"),
        ("src/dom/ConfigDOM.js", "export function f() {}\n"),
        ("src/Work.js", WORK),
    ]);
    let caller = entity(&files, "src/Work.js", "g");

    for (route, relations) in routes(&files, &["src/Work.js"]) {
        let calls = calls_from(&relations, caller);
        assert!(calls.is_empty(), "{route}: {calls:#?}");
    }
}

#[test]
fn a_star_reexport_chain_resolves_an_import_to_its_one_declaration() {
    let files = parse_all(&[
        ("lib/impl.js", "export function h() {}\n"),
        ("lib/mid.js", "export * from './impl';\n"),
        (
            "lib/index.js",
            "export * from './mid';\nexport * from './other';\n",
        ),
        ("lib/other.js", "export function unrelated() {}\n"),
        (
            "app.js",
            "import {h} from './lib';\n\nexport function run() {\n  h();\n}\n",
        ),
    ]);
    let caller = entity(&files, "app.js", "run");
    let target = entity(&files, "lib/impl.js", "h");

    for (route, relations) in routes(&files, &["app.js"]) {
        let calls = calls_from(&relations, caller);
        assert_eq!(calls.len(), 1, "{route}: {calls:#?}");
        assert_eq!(calls[0].dst.as_entity(), Some(target), "{route}");
        assert_eq!(
            calls[0].confidence.to_bits(),
            IMPORT_PINNED_CONFIDENCE.to_bits(),
            "{route}: an ordinary pinned import resolution: {:#?}",
            calls[0]
        );
        assert_eq!(
            RelationResolution::of(calls[0]),
            RelationResolution::ImportScoped,
            "{route}"
        );
        assert!(fork_records(calls[0]).is_empty(), "{route}");
    }
}

#[test]
fn a_named_reexport_resolves_to_the_declaration_it_renames() {
    let files = parse_all(&[
        ("lib/impl.js", "export function h() {}\n"),
        ("lib/named.js", "export {h as renamed} from './impl';\n"),
        (
            "app.js",
            "import {renamed} from './lib/named';\n\nexport function go() {\n  renamed();\n}\n",
        ),
    ]);
    let caller = entity(&files, "app.js", "go");
    let target = entity(&files, "lib/impl.js", "h");

    for (route, relations) in routes(&files, &["app.js"]) {
        let calls = calls_from(&relations, caller);
        assert_eq!(calls.len(), 1, "{route}: {calls:#?}");
        assert_eq!(calls[0].dst.as_entity(), Some(target), "{route}");
        assert_eq!(
            calls[0].confidence.to_bits(),
            IMPORT_PINNED_CONFIDENCE.to_bits(),
            "{route}"
        );
    }
}

#[test]
fn two_star_reexports_of_different_declarations_bind_neither() {
    // ECMAScript exports neither of two conflicting star bindings, so the
    // import names nothing and the pin keeps blocking the name fallback.
    let files = parse_all(&[
        ("lib/a.js", "export function h() {}\n"),
        ("lib/b.js", "export function h() {}\n"),
        (
            "lib/index.js",
            "export * from './a';\nexport * from './b';\n",
        ),
        (
            "app.js",
            "import {h} from './lib';\n\nexport function run() {\n  h();\n}\n",
        ),
    ]);
    let caller = entity(&files, "app.js", "run");

    for (route, relations) in routes(&files, &["app.js"]) {
        let calls = calls_from(&relations, caller);
        assert!(calls.is_empty(), "{route}: {calls:#?}");
    }
}

#[test]
fn a_namespace_reexport_hands_on_one_name_not_every_export() {
    let files = parse_all(&[
        ("lib/impl.js", "export function h() {}\n"),
        ("lib/ns.js", "export * as tools from './impl';\n"),
        (
            "app.js",
            "import {h} from './lib/ns';\n\nexport function run() {\n  h();\n}\n",
        ),
    ]);
    let caller = entity(&files, "app.js", "run");

    for (route, relations) in routes(&files, &["app.js"]) {
        let calls = calls_from(&relations, caller);
        assert!(calls.is_empty(), "{route}: {calls:#?}");
    }
}

#[test]
fn a_reexport_cycle_ends_without_an_edge() {
    let files = parse_all(&[
        ("lib/a.js", "export * from './b';\n"),
        ("lib/b.js", "export * from './a';\n"),
        (
            "app.js",
            "import {h} from './lib/a';\n\nexport function run() {\n  h();\n}\n",
        ),
    ]);
    let caller = entity(&files, "app.js", "run");

    for (route, relations) in routes(&files, &["app.js"]) {
        let calls = calls_from(&relations, caller);
        assert!(calls.is_empty(), "{route}: {calls:#?}");
    }
}

#[test]
fn a_reexport_chain_longer_than_the_hop_bound_is_not_followed() {
    let files = parse_all(&[
        ("lib/impl.js", "export function h() {}\n"),
        ("lib/l1.js", "export * from './impl';\n"),
        ("lib/l2.js", "export * from './l1';\n"),
        ("lib/l3.js", "export * from './l2';\n"),
        ("lib/l4.js", "export * from './l3';\n"),
        ("lib/l5.js", "export * from './l4';\n"),
        (
            "near.js",
            "import {h} from './lib/l4';\n\nexport function near() {\n  h();\n}\n",
        ),
        (
            "far.js",
            "import {h} from './lib/l5';\n\nexport function far() {\n  h();\n}\n",
        ),
    ]);
    let near = entity(&files, "near.js", "near");
    let far = entity(&files, "far.js", "far");
    let target = entity(&files, "lib/impl.js", "h");

    for (route, relations) in routes(&files, &["near.js", "far.js"]) {
        let near_calls = calls_from(&relations, near);
        assert_eq!(near_calls.len(), 1, "{route}: {near_calls:#?}");
        assert_eq!(near_calls[0].dst.as_entity(), Some(target), "{route}");
        assert!(calls_from(&relations, far).is_empty(), "{route}");
    }
}
