// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Every declaration a unit holds keeps an identity of its own, so the graph,
//! which holds entities by identity, holds every declaration the parse of the
//! same bytes holds.
//!
//! The fixture is the `.d.ts` declarations of resend/react-email, whose
//! shorthand `declare module 'postcss-import';` in `postcss-import.d.ts` gave
//! the unit's own module and the declared module one identity. The daemon then
//! found that unit's layout disagreeing with its bytes on every start and
//! refused to serve the repository at all.

use std::collections::HashSet;

use kin_index::IndexPipeline;
use kin_model::{EntityKind, FilePathId, ParseState, SourceRegion};

const REACT_EMAIL_DECLARATIONS: &[(&str, &str)] = &[
    (
        "packages/editor/postcss-import.d.ts",
        "declare module 'postcss-import';\n",
    ),
    (
        "packages/ui/module-punycode.d.ts",
        "declare module 'module-punycode' {\n  export * from 'node:punycode';\n}\n",
    ),
    (
        "packages/ui/inline-css.d.ts",
        "// Ambient declaration for the Vite-style ?inline / ?raw CSS import suffix.\n\
         declare module '*.css?inline' {\n  const content: string;\n  export default content;\n}\n\
         declare module '*.css?raw' {\n  const content: string;\n  export default content;\n}\n",
    ),
    (
        "packages/render/src/react-internals.d.ts",
        "declare module \"react-dom/server.browser\" {\n  export * from \"react-dom/server\";\n}\n\
         declare module \"react-dom/server.edge\" {\n  export * from \"react-dom/server\";\n}\n",
    ),
    (
        "apps/web/src/types/three.d.ts",
        "import * as THREE from 'three';\n\n\
         declare module '@react-three/fiber' {\n  interface ThreeElements {\n    \
         meshBannerMaterial: Partial<THREE.MeshBasicMaterialParameters> & {\n      \
         backfaceRepeatX?: number;\n      map?: THREE.Texture;\n      'map-anisotropy'?: number;\n      \
         'map-repeat'?: [number, number];\n    };\n  }\n}\n",
    ),
    (
        "examples/resend/next-env.d.ts",
        "/// <reference types=\"next\" />\n/// <reference types=\"next/image-types/global\" />\n\n\
         // NOTE: This file should not be edited\n",
    ),
    (
        "packages/render/e2e/vite/src/vite-env.d.ts",
        "/// <reference types=\"vite/client\" />\n",
    ),
    // The same collision without an ambient module: a namespace named like
    // its unit, on the unit's first line.
    (
        "src/utils.ts",
        "export namespace utils { export const limit = 3; }\n",
    ),
    // And a shim under the directory its module is named after.
    (
        "types/postcss-import/index.d.ts",
        "declare module 'postcss-import' {\n  const plugin: () => void;\n  export default plugin;\n}\n",
    ),
];

#[test]
fn every_declaration_in_a_unit_keeps_an_identity_of_its_own() {
    for (path, source) in REACT_EMAIL_DECLARATIONS {
        let bytes = source.as_bytes();
        let indexed = IndexPipeline::new()
            .index_file_content_with_tests(&FilePathId::new(*path), bytes, kin_blobs::digest(bytes))
            .unwrap_or_else(|error| panic!("{path}: {error}"))
            .indexed_file;
        assert!(
            matches!(indexed.parse_state, ParseState::Valid),
            "{path}: {:?}",
            indexed.parse_state
        );
        let ids: HashSet<_> = indexed.entities.iter().map(|entity| entity.id).collect();
        assert_eq!(
            ids.len(),
            indexed.entities.len(),
            "{path}: declarations share an identity: {:?}",
            indexed
                .entities
                .iter()
                .map(|entity| (entity.id, entity.kind, entity.name.as_str()))
                .collect::<Vec<_>>()
        );
        let referenced: Vec<_> = indexed
            .file_layout
            .regions
            .iter()
            .filter_map(|region| match region {
                SourceRegion::EntityRef { entity_id, .. } => Some(*entity_id),
                SourceRegion::Trivia { .. } => None,
            })
            .collect();
        assert_eq!(
            referenced.iter().copied().collect::<HashSet<_>>(),
            ids,
            "{path}: the layout names a declaration set other than the parse's"
        );
        assert_eq!(
            referenced.len(),
            ids.len(),
            "{path}: the layout names one declaration twice"
        );
    }
}

#[test]
fn a_shorthand_ambient_module_and_its_units_module_are_two_entities() {
    let path = "packages/editor/postcss-import.d.ts";
    let bytes = b"declare module 'postcss-import';\n";
    let indexed = IndexPipeline::new()
        .index_file_content_with_tests(&FilePathId::new(path), bytes, kin_blobs::digest(bytes))
        .unwrap()
        .indexed_file;
    let mut modules: Vec<_> = indexed
        .entities
        .iter()
        .filter(|entity| entity.kind == EntityKind::Module)
        .map(|entity| entity.name.as_str())
        .collect();
    modules.sort_unstable();
    assert_eq!(modules, ["\"postcss-import\"", "postcss-import"]);
}

/// An import in a unit that also declares a module or a namespace is the
/// unit's import, so its entity-level edge starts at the unit's own module.
///
/// The TypeScript adapter emits declared modules as it walks and the unit's
/// own module last, and the linker took the first module it met, so the edge
/// was sourced from `"@react-three/fiber"` or from the namespace instead.
#[test]
fn an_import_edge_starts_at_the_units_own_module_beside_declared_modules() {
    let index = |path: &str, source: &str| {
        let bytes = source.as_bytes();
        let indexed = IndexPipeline::new()
            .index_file_content_with_tests(&FilePathId::new(path), bytes, kin_blobs::digest(bytes))
            .unwrap()
            .indexed_file;
        kin_index::FileParseData {
            file_path: path.to_string(),
            entities: indexed.entities,
            relations: indexed.extracted_relations,
            imports: indexed.imports,
        }
    };
    let files = [
        index(
            "src/materials.ts",
            "export interface BannerMaterial { repeat: number }\n",
        ),
        index(
            "src/types/three.d.ts",
            "import type { BannerMaterial } from '../materials';\n\n\
             declare module '@react-three/fiber' {\n  interface ThreeElements {\n    \
             banner: BannerMaterial;\n  }\n}\n",
        ),
        index(
            "src/scene.ts",
            "import { BannerMaterial } from './materials';\n\
             export namespace Scene { export const limit = 3; }\n\
             export function banner(material: BannerMaterial): number { return material.repeat; }\n",
        ),
    ];
    let identities = files
        .iter()
        .map(|file| (file.file_path.clone(), kin_model::ArtifactId::new()))
        .collect();
    let edges = kin_index::link_cross_file(&files, &identities).unwrap();
    let target = files[0]
        .entities
        .iter()
        .find(|entity| entity.name == "BannerMaterial")
        .unwrap()
        .id;
    for importer in &files[1..] {
        let own = importer
            .entities
            .iter()
            .find(|entity| kin_model::is_file_module_surface(entity))
            .unwrap_or_else(|| panic!("{}: no module of its own", importer.file_path));
        let sources: Vec<_> = edges
            .iter()
            .filter(|edge| {
                edge.kind == kin_model::RelationKind::Imports
                    && edge.dst == kin_model::GraphNodeId::Entity(target)
                    && importer
                        .entities
                        .iter()
                        .any(|entity| kin_model::GraphNodeId::Entity(entity.id) == edge.src)
            })
            .map(|edge| edge.src)
            .collect();
        assert_eq!(
            sources,
            vec![kin_model::GraphNodeId::Entity(own.id)],
            "{}: the import edge must start at the unit's own module `{}`",
            importer.file_path,
            own.name
        );
    }
}
