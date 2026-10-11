// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! An owner an import names through an alias root
//! (`~/sqlite-core/foreign-keys.ts`) stays a call candidate on the live
//! reconcile path, and a declared type imported the same way still types its
//! receiver.

use std::sync::Arc;

use kin_blobs::BlobStore;
use kin_db::InMemoryGraph;
use kin_index::resolution::{RECEIVER_NAME_FANOUT_CONFIDENCE, RECEIVER_ORIGIN_CONFIDENCE};
use kin_index::IndexPipeline;
use kin_model::{
    ArtifactId, EntityStore, FilePathId, Hash256, LocatedEntry, RelationKind, RepoPath,
    TransactionDelta, TreeDelta, TreeEntry,
};
use kin_reconcile::Reconciler;

struct Live {
    _root: tempfile::TempDir,
    blobs: BlobStore,
    graph: Arc<InMemoryGraph>,
    reconciler: Reconciler,
}

impl Live {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let blobs = BlobStore::new(root.path().join("blobs")).unwrap();
        let reconciler = Reconciler::new(root.path().to_owned());
        Self {
            _root: root,
            blobs,
            graph: Arc::new(InMemoryGraph::new()),
            reconciler,
        }
    }

    fn add(&mut self, file: &str, source: &str) {
        let blob = self.blobs.write(source.as_bytes()).unwrap();
        let path = RepoPath::from_utf8(file.to_string()).unwrap();
        self.graph
            .apply_transaction_delta(&TransactionDelta {
                tree_deltas: vec![TreeDelta::Added {
                    artifact_id: ArtifactId::new(),
                    new: LocatedEntry::new(
                        path,
                        TreeEntry::blob(Hash256::from_bytes(blob.0), false),
                    ),
                }],
                ..Default::default()
            })
            .unwrap();
        let indexed = IndexPipeline::new()
            .index_file_content_with_tests(&FilePathId::new(file), source.as_bytes(), blob)
            .unwrap()
            .indexed_file;
        let result = self
            .reconciler
            .reconcile_indexed_observation(&indexed, &self.blobs, self.graph.as_ref())
            .unwrap();
        self.graph.apply_transaction_delta(&result.delta).unwrap();
    }

    /// Every call edge from the entity `caller` of `file`, as (target file,
    /// target name, confidence), sorted.
    fn calls_from(&self, file: &str, caller: &str) -> Vec<(String, String, f32)> {
        let entities = self.graph.list_all_entities().unwrap();
        let source = entities
            .iter()
            .find(|entity| {
                entity.name == caller && entity.file_origin == Some(FilePathId::new(file))
            })
            .unwrap_or_else(|| panic!("no {caller} in {file}"));
        let mut out: Vec<_> = self
            .graph
            .get_all_relations_for_entity(&source.id)
            .unwrap()
            .into_iter()
            .filter(|relation| {
                relation.kind == RelationKind::Calls && relation.src.as_entity() == Some(source.id)
            })
            .filter_map(|relation| {
                let target = relation.dst.as_entity()?;
                let target = entities.iter().find(|entity| entity.id == target)?;
                Some((
                    target.file_origin.as_ref()?.0.clone(),
                    target.name.clone(),
                    relation.confidence,
                ))
            })
            .collect();
        out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        out
    }
}

#[test]
fn an_alias_import_keeps_its_owner_through_live_reconcile() {
    let mut live = Live::new();
    let foreign_keys = "export class ForeignKeyBuilder {\n  \
         onUpdate(action: string): this { return this; }\n  \
         build(table: string): number { return 1; }\n}\n";
    live.add("drizzle-orm/src/sqlite-core/foreign-keys.ts", foreign_keys);
    live.add("drizzle-orm/src/pg-core/foreign-keys.ts", foreign_keys);
    live.add(
        "drizzle-orm/src/sqlite-core/table.ts",
        "export class SQLiteTable {\n  getName(): string { return 't'; }\n}\n",
    );
    let common = "drizzle-orm/src/sqlite-core/columns/common.ts";
    live.add(
        common,
        "import { ForeignKeyBuilder } from '~/sqlite-core/foreign-keys.ts';\n\
         import type { SQLiteTable } from '~/sqlite-core/table.ts';\n\n\
         export class SQLiteColumnBuilder {\n  \
         buildForeignKeys(table: SQLiteTable): number {\n    \
         const builder = new ForeignKeyBuilder();\n    \
         table.getName();\n    \
         return builder.build('t');\n  \
         }\n}\n",
    );
    let calls = live.calls_from(common, "SQLiteColumnBuilder.buildForeignKeys");
    assert!(
        calls.contains(&(
            "drizzle-orm/src/sqlite-core/foreign-keys.ts".to_string(),
            "ForeignKeyBuilder.build".to_string(),
            RECEIVER_NAME_FANOUT_CONFIDENCE
        )),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|(file, _, _)| file.contains("pg-core")),
        "{calls:?}"
    );
    assert!(
        calls.contains(&(
            "drizzle-orm/src/sqlite-core/table.ts".to_string(),
            "SQLiteTable.getName".to_string(),
            RECEIVER_ORIGIN_CONFIDENCE
        )),
        "{calls:?}"
    );
}
