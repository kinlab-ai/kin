// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Name-only candidates a call keeps when nothing proves them wrong, batch and
//! incremental, over parsed fixtures.
//!
//! Two shapes lost a call's true target from its candidates. An import written
//! from an alias root (`~/sqlite-core/foreign-keys.ts`) resolves to no file the
//! linker can place, and the import-named owner check read that as an import
//! from outside the repository. And a module whose file stem carries a dot
//! (`script.output.ts`) was keyed under the stem's last part, so it became an
//! `output` candidate and pushed the name's candidates past the fan-out cap.

use super::*;
use crate::resolution::{RECEIVER_NAME_FANOUT_CONFIDENCE, RECEIVER_ORIGIN_CONFIDENCE};

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

fn artifact_ids(files: &[FileParseData]) -> ArtifactIdentityMap {
    files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect()
}

fn incremental_linker(files: &[FileParseData], ids: &ArtifactIdentityMap) -> IncrementalLinker {
    let mut linker = IncrementalLinker::new();
    for file in files {
        linker.add_file(&file.file_path, ids[&file.file_path], &file.entities);
    }
    linker
}

/// The batch and the incremental linker's relations for `files`.
fn links(files: &[FileParseData]) -> [(&'static str, Vec<Relation>); 2] {
    let ids = artifact_ids(files);
    let linker = incremental_linker(files, &ids);
    [
        ("batch", super::link_cross_file(files, &ids).unwrap()),
        (
            "incremental",
            super::link_cross_file_incremental(files, &linker).unwrap(),
        ),
    ]
}

fn entity<'a>(files: &'a [FileParseData], file: &str, name: &str) -> &'a Entity {
    files
        .iter()
        .find(|parsed| parsed.file_path == file)
        .and_then(|parsed| parsed.entities.iter().find(|entity| entity.name == name))
        .unwrap_or_else(|| panic!("no entity {name} in {file}"))
}

/// Every call edge from `caller`, as (target file, target name, confidence),
/// sorted.
fn calls_from(
    relations: &[Relation],
    files: &[FileParseData],
    caller: &Entity,
) -> Vec<(String, String, f32)> {
    let targets: HashMap<EntityId, (&str, &str)> = files
        .iter()
        .flat_map(|file| {
            file.entities
                .iter()
                .map(move |entity| (entity.id, (file.file_path.as_str(), entity.name.as_str())))
        })
        .collect();
    let mut out: Vec<_> = relations
        .iter()
        .filter(|relation| {
            relation.kind == RelationKind::Calls && relation.src == GraphNodeId::Entity(caller.id)
        })
        .filter_map(|relation| {
            let (file, name) = targets.get(&relation.dst.as_entity()?)?;
            Some((file.to_string(), name.to_string(), relation.confidence))
        })
        .collect();
    out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    out
}

const FOREIGN_KEYS: &str = "export class ForeignKeyBuilder {\n  \
     onUpdate(action: string): this { return this; }\n  \
     build(table: string): number { return 1; }\n}\n";

const TABLE: &str = "export class SQLiteTable {\n  getName(): string { return 't'; }\n}\n";

/// A column builder in the shape of drizzle-orm's, importing its collaborators
/// through `import_root` (an alias root, or a package that is not in the
/// repository), next to a second dialect that declares a builder of the same
/// name.
fn column_files(import_root: &str) -> Vec<FileParseData> {
    vec![
        fixture("drizzle-orm/src/sqlite-core/foreign-keys.ts", FOREIGN_KEYS),
        fixture("drizzle-orm/src/pg-core/foreign-keys.ts", FOREIGN_KEYS),
        fixture("drizzle-orm/src/sqlite-core/table.ts", TABLE),
        fixture(
            "drizzle-orm/src/sqlite-core/columns/common.ts",
            &format!(
                "import {{ ForeignKeyBuilder }} from '{import_root}/sqlite-core/foreign-keys.ts';\n\
                 import type {{ SQLiteTable }} from '{import_root}/sqlite-core/table.ts';\n\n\
                 export class SQLiteColumnBuilder {{\n  \
                 buildForeignKeys(table: SQLiteTable): number {{\n    \
                 const builder = new ForeignKeyBuilder();\n    \
                 builder.onUpdate('cascade');\n    \
                 table.getName();\n    \
                 return builder.build('t');\n  \
                 }}\n}}\n"
            ),
        ),
    ]
}

const COMMON: &str = "drizzle-orm/src/sqlite-core/columns/common.ts";
const SQLITE_FOREIGN_KEYS: &str = "drizzle-orm/src/sqlite-core/foreign-keys.ts";

#[test]
fn an_alias_import_keeps_the_owner_its_path_names_as_a_candidate() {
    let files = column_files("~");
    let caller = entity(&files, COMMON, "SQLiteColumnBuilder.buildForeignKeys");
    for (label, relations) in links(&files) {
        let calls = calls_from(&relations, &files, caller);
        for method in ["ForeignKeyBuilder.build", "ForeignKeyBuilder.onUpdate"] {
            assert!(
                calls.contains(&(
                    SQLITE_FOREIGN_KEYS.to_string(),
                    method.to_string(),
                    RECEIVER_NAME_FANOUT_CONFIDENCE
                )),
                "{label}: the alias import names {method} of the sqlite dialect: {calls:?}"
            );
        }
        // The alias spells the sqlite dialect's file, so the other dialect's
        // builder of the same name is no candidate.
        assert!(
            !calls.iter().any(|(file, _, _)| file.contains("pg-core")),
            "{label}: {calls:?}"
        );
        // A declared type imported through the alias is located, so the
        // receiver it types binds through the value-origin tier.
        assert!(
            calls.contains(&(
                "drizzle-orm/src/sqlite-core/table.ts".to_string(),
                "SQLiteTable.getName".to_string(),
                RECEIVER_ORIGIN_CONFIDENCE
            )),
            "{label}: {calls:?}"
        );
    }
}

#[test]
fn an_import_from_a_package_outside_the_repository_still_names_no_repository_owner() {
    for root in ["orm-kit", "@orm/kit"] {
        let files = column_files(root);
        let caller = entity(&files, COMMON, "SQLiteColumnBuilder.buildForeignKeys");
        for (label, relations) in links(&files) {
            let calls = calls_from(&relations, &files, caller);
            assert!(
                calls.is_empty(),
                "{label}: `{root}` can name a package, so no repository class of the \
                 imported name is its owner: {calls:?}"
            );
        }
    }
}

#[test]
fn an_alias_path_names_the_file_its_segments_spell() {
    let file = "drizzle-orm/src/sqlite-core/foreign-keys.ts";
    for written in [
        "~/sqlite-core/foreign-keys.ts",
        "~/sqlite-core/foreign-keys",
        "~/sqlite-core/foreign-keys.js",
        "@/sqlite-core/foreign-keys",
        "#orm/sqlite-core/foreign-keys",
        "$lib/sqlite-core/foreign-keys",
        "~/foreign-keys.ts",
    ] {
        assert!(aliased_module_path_names_file(written, file), "{written}");
    }
    assert!(aliased_module_path_names_file(
        "~/sqlite-core",
        "drizzle-orm/src/sqlite-core/index.ts"
    ));
    assert!(aliased_module_path_names_file(
        "~/filters/http-exception.filter",
        "src/filters/http-exception.filter.ts"
    ));
    for written in [
        "~/pg-core/foreign-keys.ts",
        "~/sqlite-core/foreign-keys/index.ts",
        "~/sqlite-core",
        "~",
        "~/../sqlite-core/foreign-keys.ts",
        "./sqlite-core/foreign-keys.ts",
        "orm-kit/sqlite-core/foreign-keys",
        "@orm/sqlite-core/foreign-keys",
        "sqlite-core/foreign-keys",
    ] {
        assert!(!aliased_module_path_names_file(written, file), "{written}");
    }
}

/// Six classes that each declare `output`, a builder whose methods return
/// itself, and three generated modules whose file stems end in `.output`.
fn output_files() -> Vec<FileParseData> {
    let mut files: Vec<FileParseData> = (0..5)
        .map(|index| {
            fixture(
                &format!("packages/server/src/defs{index}.ts"),
                &format!("export class Def{index} {{\n  output(): void {{}}\n}}\n"),
            )
        })
        .collect();
    files.push(fixture(
        "packages/server/src/procedureBuilder.ts",
        "export class ProcedureBuilder {\n  \
         input(schema: unknown): ProcedureBuilder { return this; }\n  \
         output(schema: unknown): ProcedureBuilder { return this; }\n}\n",
    ));
    for page in ["sponsors", "wall", "footer"] {
        files.push(fixture(
            &format!("www/src/{page}/script.output.ts"),
            "export const rows = [1, 2, 3];\n",
        ));
    }
    files.push(fixture(
        "packages/server/src/router.ts",
        "export function route(base: unknown) {\n  \
         return make(base).input(1).output(2);\n}\n\n\
         function make(base: unknown): any { return base; }\n",
    ));
    files
}

#[test]
fn a_module_whose_stem_ends_like_a_member_is_never_that_members_candidate() {
    let files = output_files();
    let modules: Vec<EntityId> = files
        .iter()
        .flat_map(|file| file.entities.iter())
        .filter(|entity| entity.kind == EntityKind::Module && entity.name == "script.output")
        .map(|entity| entity.id)
        .collect();
    assert_eq!(
        modules.len(),
        3,
        "the fixture mints the dotted module stems"
    );
    let caller = entity(&files, "packages/server/src/router.ts", "route");
    for (label, relations) in links(&files) {
        let calls = calls_from(&relations, &files, caller);
        let outputs: Vec<&str> = calls
            .iter()
            .filter(|(_, name, _)| name.ends_with("output"))
            .map(|(_, name, _)| name.as_str())
            .collect();
        // Six declarations carry `output`, within the fan-out cap; the three
        // modules pushed the count past it and dropped all six.
        assert_eq!(
            outputs,
            [
                "Def0.output",
                "Def1.output",
                "Def2.output",
                "Def3.output",
                "Def4.output",
                "ProcedureBuilder.output"
            ],
            "{label}: {calls:?}"
        );
        assert!(
            !relations
                .iter()
                .filter_map(|relation| relation.dst.as_entity())
                .any(|target| modules.contains(&target)),
            "{label}: no call reaches a module by the leaf of its stem"
        );
    }
}

#[test]
fn a_checkpoint_that_keys_a_module_by_its_leaf_restores_without_it() {
    let files = output_files();
    let ids = artifact_ids(&files);
    let linker = incremental_linker(&files, &ids);
    assert!(linker
        .entity_by_bare_name
        .get("output")
        .is_some_and(|holders| holders
            .iter()
            .all(|(_, id)| linker.entity_kind_by_id.get(id) != Some(&EntityKind::Module))));

    // An earlier build of this checkpoint version keyed each module under the
    // leaf of its stem.
    let mut checkpoint = linker.to_checkpoint_v1();
    let modules: Vec<(String, EntityId)> = files
        .iter()
        .flat_map(|file| {
            file.entities
                .iter()
                .filter(|entity| entity.kind == EntityKind::Module)
                .filter(|entity| entity.name == "script.output")
                .map(move |entity| (file.file_path.clone(), entity.id))
        })
        .collect();
    let slot = checkpoint
        .entity_by_bare_name
        .iter_mut()
        .find(|(bare, _)| bare == "output")
        .expect("the checkpoint keys the members named output");
    slot.1.extend(modules.iter().cloned());

    let restored = IncrementalLinker::from_checkpoint_v1(checkpoint).unwrap();
    let holders = &restored.entity_by_bare_name["output"];
    assert_eq!(holders.len(), 6, "{holders:?}");
    assert!(!holders.iter().any(|holder| modules.contains(holder)));
    let caller = entity(&files, "packages/server/src/router.ts", "route");
    let relations = super::link_cross_file_incremental(&files, &restored).unwrap();
    assert!(calls_from(&relations, &files, caller)
        .iter()
        .any(|(_, name, _)| name == "ProcedureBuilder.output"));
}
