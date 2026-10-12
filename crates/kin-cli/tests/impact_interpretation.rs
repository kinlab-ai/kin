// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Structural impact keeps valid paths without presenting branch execution as proven.

use std::collections::{BTreeSet, HashMap};

use kin_cli::commands::impact::{build_impact_response, ImpactRequest, ImpactResponse};
use kin_db::InMemoryGraph;
use kin_index::{FileParseData, IndexPipeline};
use kin_model::{
    ArtifactId, EntityId, EntityKind, EntityStore, FilePathId, LocatedEntry, RepoPath,
    TransactionDelta, TreeDelta, TreeEntry,
};

const SETTINGS: &str = r#"export function parseTime(value: string) { return value; }
export function genericGetter(mode: string) {
  switch (mode) {
    case 'time-limit': return parseTime(mode);
    default: return mode;
  }
}
export function ordinarySettings() { return genericGetter('text'); }
"#;

fn project() -> (InMemoryGraph, EntityId) {
    let graph = InMemoryGraph::new();
    let mut files = Vec::new();
    let mut artifacts = HashMap::new();
    for (path, source) in [
        ("src/settings.ts", SETTINGS),
        (
            "src/settings.test.ts",
            "import { parseTime } from './settings';\nexport const fixture = 0;\n",
        ),
    ] {
        let hash = kin_blobs::digest(source.as_bytes());
        let file = IndexPipeline::new()
            .index_file_content_with_tests(&FilePathId::new(path), source.as_bytes(), hash)
            .unwrap()
            .indexed_file;
        let artifact = ArtifactId::new();
        graph
            .apply_transaction_delta(&TransactionDelta {
                tree_deltas: vec![TreeDelta::Added {
                    artifact_id: artifact,
                    new: LocatedEntry::new(
                        RepoPath::from_utf8(path).unwrap(),
                        TreeEntry::blob(hash, false),
                    ),
                }],
                ..TransactionDelta::default()
            })
            .unwrap();
        for entity in &file.entities {
            graph.upsert_entity(entity).unwrap();
        }
        artifacts.insert(path.to_string(), artifact);
        files.push(FileParseData {
            file_path: path.into(),
            entities: file.entities,
            relations: file.extracted_relations,
            imports: file.imports,
        });
    }
    let target = files[0]
        .entities
        .iter()
        .find(|entity| entity.name == "parseTime" && entity.kind == EntityKind::Function)
        .unwrap()
        .id;
    for relation in kin_index::link_cross_file(&files, &artifacts).unwrap() {
        graph.upsert_relation(&relation).unwrap();
    }
    (graph, target)
}

async fn response(graph: &InMemoryGraph, unique: bool) -> ImpactResponse {
    let temp = tempfile::tempdir().unwrap();
    build_impact_response(
        &kin_core::KinLayout::new(temp.path().join(".kin")),
        graph,
        &ImpactRequest {
            entity: "parseTime".into(), depth: 3, file: None, kind: None, signature: None,
            require_unique: unique, dispatch_candidates: false,
        },
        &kin_mcp::Envelope::daemon().with_health(&serde_json::json!({
            "initialized": true, "graph_loaded": true, "graph_entity_count": 6, "graph_generation": 1,
        })),
    ).await.unwrap()
}

#[tokio::test]
async fn structural_qualification_preserves_branch_path_and_import_dependents() {
    let (graph, target) = project();
    let before = kin_review::rank_impact(&graph, &target, 3).unwrap();
    let expected: BTreeSet<_> = before
        .candidates
        .iter()
        .map(|candidate| {
            (
                candidate.identity.name.clone(),
                format!("{:?}", candidate.relation),
                candidate.hop,
            )
        })
        .collect();
    assert!(expected.contains(&("genericGetter".into(), "Calls".into(), 1)));
    assert!(expected.contains(&("ordinarySettings".into(), "Calls".into(), 2)));
    assert!(expected.contains(&("settings.test".into(), "Imports".into(), 1)));
    let result = response(&graph, true).await;
    let after = result.ranked.as_ref().unwrap();
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        serde_json::to_value(after).unwrap(),
        "interpretation must not prune or change any ranked path"
    );
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["interpretation"]["reach"], "structural_graph");
    assert_eq!(
        json["interpretation"]["argument_and_branch_feasibility"],
        "not_established"
    );
    for unique in [false, true] {
        let result = response(&graph, unique).await;
        let text = result.lines.join("\n");
        assert!(text.contains("Structural graph reach"), "{text}");
        assert!(text.contains("argument and branch feasibility"), "{text}");
        assert!(text.contains("1 hop (direct graph dependents):"), "{text}");
        assert!(
            !text.contains("direct callers"),
            "an import is not a runtime call: {text}"
        );
        assert!(text.contains("settings.test (Module)"), "{text}");
        assert!(text.contains("ordinarySettings (Function)"), "{text}");
        assert!(
            text.contains(&format!("{} local graph dependents", expected.len())),
            "{text}"
        );
    }
}

#[test]
fn older_daemon_response_does_not_invent_a_feasibility_claim() {
    let legacy = serde_json::json!({
        "lines": [], "schema_version": "kin-impact-response-v1", "resolution": "resolved",
        "query": {}, "ranked": null,
    });
    // Query fields predate the interpretation and remain explicit on the wire.
    let mut legacy = legacy;
    legacy["query"] = serde_json::json!({
        "entity": "parseTime", "file": null, "kind": null, "signature": null, "match_count": 1,
    });
    let response: ImpactResponse = serde_json::from_value(legacy).unwrap();
    assert!(serde_json::to_value(response)
        .unwrap()
        .get("interpretation")
        .is_none());
}
