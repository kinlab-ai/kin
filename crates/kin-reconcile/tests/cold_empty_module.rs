// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! An admitted local module remains local after reopening, even without declarations.

use kin_blobs::BlobStore;
use kin_db::{InMemoryGraph, SnapshotManager};
use kin_index::{FileParseData, IndexPipeline, IndexedAny};
use kin_model::graph::ResolvedGraphState;
use kin_model::{
    ArtifactId, EntityDelta, EntityStore, FilePathId, Hash256, LocatedEntry, Relation,
    RelationDelta, RepoPath, TransactionDelta, TreeDelta, TreeEntry,
};
use kin_reconcile::Reconciler;

const RESPONSE: &str = "var http = require('http');\nvar res = Object.create(http.ServerResponse.prototype);\nmodule.exports = res;\nres.redirect = function redirect(url) { return 1; };\n";
const APPLICATION: &str = "var res = require('./response');\nexports = module.exports = createApplication;\nfunction createApplication() { return res; }\nexports.response = res;\n";
const INDEX: &str = "module.exports = require('./lib/express');\n";
const CONSUMER: &str = "var express = require('..');\ndescribe('res', function() { it('redirects', function() { var app = express(); app.use(function(req,res) { res.redirect('/'); }); }); });\n";

struct Repo {
    root: tempfile::TempDir,
    graph: InMemoryGraph,
    blobs: BlobStore,
}

impl Repo {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let blobs = BlobStore::new_ephemeral(root.path().join("objects")).unwrap();
        let graph = InMemoryGraph::new();
        let mut files = Vec::new();
        let mut artifacts = kin_index::linker::ArtifactIdentityMap::default();
        let mut completeness = kin_index::FileParseCompletenessMap::default();
        let mut delta = TransactionDelta::default();
        for (path, content) in [
            ("lib/response.js", RESPONSE),
            ("lib/express.js", APPLICATION),
            ("index.js", INDEX),
            ("test/res.redirect.js", CONSUMER),
        ] {
            let digest = blobs.write(content.as_bytes()).unwrap();
            let artifact = ArtifactId::new();
            delta.tree_deltas.push(TreeDelta::Added {
                artifact_id: artifact,
                new: LocatedEntry::new(
                    RepoPath::from_utf8(path).unwrap(),
                    TreeEntry::blob(Hash256::from_bytes(digest.0), false),
                ),
            });
            let IndexedAny::EntitySource(indexed) = IndexPipeline::new()
                .index_any_content(&FilePathId::new(path), content.as_bytes(), digest)
                .unwrap()
            else {
                panic!("fixture must be entity source");
            };
            if path == "index.js" {
                assert!(indexed.entities.is_empty());
            }
            artifacts.insert(path.to_string(), artifact);
            completeness.insert(
                path.to_string(),
                indexed.file_layout.parse_completeness.clone(),
            );
            delta.entity_deltas.extend(
                indexed
                    .entities
                    .iter()
                    .cloned()
                    .map(|new| EntityDelta::Added { new }),
            );
            files.push(indexed);
        }
        graph.apply_transaction_delta(&delta).unwrap();
        for indexed in &files {
            graph.upsert_file_layout(&indexed.file_layout).unwrap();
        }
        let parse: Vec<_> = files
            .iter()
            .map(|indexed| FileParseData {
                file_path: indexed.file_id.0.clone(),
                entities: indexed.entities.clone(),
                relations: indexed.extracted_relations.clone(),
                imports: indexed.imports.clone(),
            })
            .collect();
        let linked =
            kin_index::link_cross_file_with_completeness(&parse, &artifacts, &completeness)
                .unwrap();
        assert!(!linked.iter().any(is_external_local_import));
        for relation in &linked {
            if kin_index::is_external_import_placeholder(relation) {
                let source = graph
                    .get_entity(&relation.src.as_entity().unwrap())
                    .unwrap()
                    .unwrap();
                let target =
                    kin_index::placeholder_target_entity(relation, source.language).unwrap();
                graph.upsert_entity(&target).unwrap();
            }
        }
        graph.upsert_relations_batch(&linked).unwrap();
        Self { root, graph, blobs }
    }

    fn admit(&self, name: &str, bytes: &[u8]) -> kin_blobs::Hash256 {
        let file = FilePathId::new(name);
        let path = RepoPath::from_utf8(&file.0).unwrap();
        let digest = self.blobs.write(bytes).unwrap();
        let new = LocatedEntry::new(
            path.clone(),
            TreeEntry::blob(Hash256::from_bytes(digest.0), false),
        );
        let change = match self.graph.get_tree_entry(&file).unwrap() {
            Some(old) => TreeDelta::Updated {
                artifact_id: self.graph.artifact_id_at_path(&path).unwrap(),
                old: LocatedEntry::new(path, old),
                new,
            },
            None => TreeDelta::Added {
                artifact_id: ArtifactId::new(),
                new,
            },
        };
        self.graph
            .apply_transaction_delta(&TransactionDelta {
                tree_deltas: vec![change],
                ..Default::default()
            })
            .unwrap();
        digest
    }

    fn cold(&self) -> Reconciler {
        let mut reconciler = Reconciler::new(self.root.path().to_owned());
        reconciler.seed_cross_file_linker_from_graph(&self.graph);
        reconciler
    }

    fn reopen(&mut self) {
        let path = self.root.path().join("state.kindb");
        SnapshotManager::save_graph(&path, &self.graph).unwrap();
        let cold = SnapshotManager::open_without_text_index(&path).unwrap();
        self.graph = InMemoryGraph::from_snapshot(cold.graph().to_snapshot()).unwrap();
    }

    fn edit(&self, source: &str) -> (Reconciler, TransactionDelta) {
        let file = FilePathId::new("lib/response.js");
        let digest = self.admit(&file.0, source.as_bytes());
        let IndexedAny::EntitySource(indexed) = IndexPipeline::new()
            .index_any_content(&file, source.as_bytes(), digest)
            .unwrap()
        else {
            panic!("edited source remains supported");
        };
        let mut reconciler = Reconciler::new(self.root.path().to_owned());
        let result = reconciler
            .reconcile_indexed_content(&indexed, &self.blobs, &self.graph)
            .unwrap();
        (reconciler, result.delta)
    }
}

fn is_external_local_import(relation: &Relation) -> bool {
    relation.import_source.as_deref() == Some("..")
        && kin_index::is_external_import_placeholder(relation)
}

#[test]
fn cold_signature_edit_does_not_misclassify_an_empty_local_module_as_external() {
    let mut repo = Repo::new();
    assert!(repo
        .graph
        .get_tree_entry(&FilePathId::new("index.js"))
        .unwrap()
        .is_some());
    let consumer_entities = repo
        .graph
        .query_entities(&kin_model::EntityFilter {
            file_path: Some(FilePathId::new("test/res.redirect.js")),
            ..Default::default()
        })
        .unwrap();
    for signature in ["redirect(url, status)", "redirect(url, status, extra)"] {
        let (reconciler, delta) = repo.edit(&RESPONSE.replace("redirect(url)", signature));
        assert!(
            reconciler.cross_file_linker().last_files_resolved() > 1,
            "the signature edit must rederive the unchanged consumer"
        );
        assert!(
            !delta
                .relation_deltas
                .iter()
                .filter_map(RelationDelta::new_state)
                .any(is_external_local_import),
            "an admitted local root index must not become external"
        );
        assert!(reconciler.cross_file_linker().knows_file("index.js"));
        repo.graph.apply_transaction_delta(&delta).unwrap();
        for entity in &consumer_entities {
            assert_eq!(
                repo.graph.get_entity(&entity.id).unwrap().as_ref(),
                Some(entity)
            );
        }
        repo.reopen();
    }
}

#[test]
fn cold_body_edit_preserves_consumer_declarations_and_local_module() {
    let repo = Repo::new();
    let before = repo
        .graph
        .query_entities(&kin_model::EntityFilter {
            file_path: Some(FilePathId::new("test/res.redirect.js")),
            ..Default::default()
        })
        .unwrap();
    let (reconciler, delta) = repo.edit(&RESPONSE.replace("return 1", "return 2"));
    assert!(reconciler.cross_file_linker().knows_file("index.js"));
    assert!(!delta
        .relation_deltas
        .iter()
        .filter_map(RelationDelta::new_state)
        .any(is_external_local_import));
    repo.graph.apply_transaction_delta(&delta).unwrap();
    for entity in before {
        assert_eq!(repo.graph.get_entity(&entity.id).unwrap(), Some(entity));
    }
}

#[test]
fn a_missing_layout_is_a_gap_and_installs_no_empty_module() {
    let repo = Repo::new();
    repo.graph
        .delete_file_layout(&FilePathId::new("index.js"))
        .unwrap();
    let mut reconciler = repo.cold();
    let error = reconciler
        .restore_cross_file_dependencies(&repo.graph, &repo.blobs)
        .unwrap_err();
    assert!(error.to_string().contains("no derived source layout"));
    assert!(!reconciler.cross_file_linker().knows_file("index.js"));
    assert_eq!(reconciler.cross_file_linker().pending_file_count(), 0);
}

#[test]
fn a_stale_layout_is_a_gap_and_installs_no_empty_module() {
    let repo = Repo::new();
    let mut layout = repo
        .graph
        .get_file_layout(&FilePathId::new("index.js"))
        .unwrap()
        .unwrap();
    layout.imports.byte_range = 0..9999;
    repo.graph.upsert_file_layout(&layout).unwrap();
    let mut reconciler = repo.cold();
    let error = reconciler
        .restore_cross_file_dependencies(&repo.graph, &repo.blobs)
        .unwrap_err();
    assert!(error.to_string().contains("layout differs from its source"));
    assert!(!reconciler.cross_file_linker().knows_file("index.js"));
    assert_eq!(reconciler.cross_file_linker().pending_file_count(), 0);
}

#[test]
fn non_source_bytes_and_incomplete_source_do_not_gain_module_authority() {
    for bytes in [&b"\0\0\0binary"[..], &b"module.exports = ;"[..]] {
        let repo = Repo::new();
        repo.admit("index.js", bytes);
        repo.admit("notes.txt", b"module.exports = require('./lib/express');");
        let mut reconciler = repo.cold();
        reconciler
            .restore_cross_file_dependencies(&repo.graph, &repo.blobs)
            .unwrap();
        assert!(!reconciler.cross_file_linker().knows_file("index.js"));
        assert!(!reconciler.cross_file_linker().knows_file("notes.txt"));
    }
}

/// Every admission prepares its batch in a semantic observation of the live
/// graph, which carries no derived layouts. An admitted module that declares
/// nothing stays local there on its checked parse instead of refusing the
/// admission, and with it every commit and stash that admits an edit.
#[test]
fn an_admission_batch_keeps_an_empty_local_module_without_held_layouts() {
    let repo = Repo::new();
    let predecessor = repo.graph.semantic_observation();
    let file = FilePathId::new("lib/response.js");
    let edited = RESPONSE.replace("redirect(url)", "redirect(url, status)");
    repo.admit(&file.0, edited.as_bytes());
    let candidate = repo.graph.semantic_observation();
    assert!(
        candidate.file_layouts.is_empty(),
        "the premise: an admission's candidate carries no derived layouts"
    );
    let prepared = Reconciler::prepare_admitted_source_batch(
        candidate,
        &[file],
        &repo.blobs,
        &[ResolvedGraphState {
            entities: predecessor.entities.clone(),
            relations: predecessor.relations.clone(),
            external_references: predecessor.external_references.clone(),
            tree: predecessor.resolved_tree.clone(),
            ..Default::default()
        }],
    )
    .expect("an admitted module that declares nothing must not refuse the admission");
    assert!(
        !prepared
            .snapshot()
            .relations
            .values()
            .any(is_external_local_import),
        "an admitted local root index must not become external"
    );
}

/// A layout the graph does hold still has to match the module's bytes when an
/// edit restores dependencies, so a stale one refuses the edit.
#[test]
fn a_held_stale_layout_still_refuses_an_edit() {
    let repo = Repo::new();
    let mut layout = repo
        .graph
        .get_file_layout(&FilePathId::new("index.js"))
        .unwrap()
        .unwrap();
    layout.imports.byte_range = 0..9999;
    repo.graph.upsert_file_layout(&layout).unwrap();
    let file = FilePathId::new("lib/response.js");
    let source = RESPONSE.replace("return 1", "return 2");
    let digest = repo.admit(&file.0, source.as_bytes());
    let IndexedAny::EntitySource(indexed) = IndexPipeline::new()
        .index_any_content(&file, source.as_bytes(), digest)
        .unwrap()
    else {
        panic!("edited source remains supported");
    };
    let mut reconciler = repo.cold();
    let error = reconciler
        .reconcile_indexed_content(&indexed, &repo.blobs, &repo.graph)
        .unwrap_err();
    assert!(
        error.to_string().contains("layout differs from its source"),
        "{error}"
    );
    assert!(!reconciler.cross_file_linker().knows_file("index.js"));
}
